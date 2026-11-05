//! 链路预算执行器：无损通路装不下时把画面交给媒体通道，预算松了再退回无损。
//!
//! 这条路径的存在理由：窄链路上继续推无损块只会让发包排队延迟涨到观看端卡死，
//! 而"有损但流畅"才是产品级验收要的画面。测试用 RawCodec + 确定性时间戳，
//! 不依赖网络。

use ess_core::codec::RawCodec;
use ess_core::frame::Frame;
use ess_core::model::TileId;
use ess_core::pipeline::{CodecSet, SendPipeline};
use ess_core::wire::WireMsg;

fn noise(k: u32, w: u32, h: u32) -> Frame {
    // 时间戳按 100 ms 一帧推进，预算窗口（1 s）才会真的滚动
    let mut f = Frame::new(w, h, k as u64 * 100_000);
    for y in 0..h {
        for x in 0..w {
            let o = y as usize * f.stride + x as usize * 4;
            let mut v = x.wrapping_mul(0x9e37_79b1)
                ^ y.wrapping_mul(0x85eb_ca6b)
                ^ k.wrapping_mul(0xc2b2_ae35);
            v ^= v >> 15;
            v = v.wrapping_mul(0x2545_f491);
            v ^= v >> 13;
            f.px[o] = v as u8;
            f.px[o + 1] = (v >> 8) as u8;
            f.px[o + 2] = (v >> 16) as u8;
            f.px[o + 3] = 255;
        }
    }
    f
}

/// 只改一小块：变化率远低于运动阈值，所以平面切换只可能来自预算这条判据。
fn edit(f: &mut Frame, x: u32, y: u32) {
    for dy in 0..4 {
        for dx in 0..4 {
            let o = ((y + dy) as usize) * f.stride + ((x + dx) as usize) * 4;
            f.px[o] = f.px[o].wrapping_add(37);
        }
    }
}

fn codecs() -> CodecSet<'static> {
    CodecSet { codecs: Box::leak(vec![&RawCodec as &dyn ess_core::codec::TileCodec].into_boxed_slice()) }
}

fn sender() -> SendPipeline<'static> {
    let mut p = SendPipeline::new(noise(0, 128, 128), codecs());
    p.video_plane = true;
    // 关掉运动通路，隔离出预算这一条判据
    p.motion_enabled = false;
    p.hold_frames = 2;
    p
}

fn kinds(msgs: &[WireMsg]) -> Vec<&'static str> {
    msgs.iter()
        .map(|m| match m {
            WireMsg::Full { .. } => "full",
            WireMsg::Update { repair: true, .. } => "repair",
            WireMsg::Update { .. } => "update",
            WireMsg::Motion { .. } => "motion",
            WireMsg::VideoMode { on: true, .. } => "video-on",
            WireMsg::VideoMode { on: false, .. } => "video-off",
            WireMsg::Scroll { .. } => "scroll",
            _ => "other",
        })
        .collect()
}

/// 观看端"我全都收到了"的报告：把整帧消息里携带的版本向量原样抄回来。
///
/// 发送端在**产出**那一刻就提交了向量，链路真丢掉块时向量不会退回去——于是这份报告恰恰
/// 看不出缺口（真机 #52：`resends=0` 而画布整片旧着）。这是脱落后唯一还能证明"必须补账"的
/// 证据；报告能指出缺口时不在此列，那是逐块修复路径的活（#53）。
fn blind(msgs: &[WireMsg]) -> (u32, Vec<u32>, Vec<u32>) {
    msgs.iter().find_map(|m| match m {
        WireMsg::Full { layout_id, versions, epochs, .. } => {
            Some((*layout_id, versions.clone(), epochs.clone()))
        }
        _ => None,
    }).expect("没有整帧消息")
}

/// 只动一两块（光标/时钟/闪烁光标）不该因为窄预算被换到有损平面；同样的窄预算下
/// 真正的成片变化仍然必须交出去。这条区分的是"绝对量很小"和"无损真的装不下"。
#[test]
fn cursor_sized_edit_stays_on_tiles_under_a_tight_budget() {
    let mut p = sender();
    // 512x512 = 8x8 网格：一块 = 1.6%，远低于平面门槛
    let f0 = noise(0, 512, 512);
    let m0 = p.on_frame(&f0).unwrap();
    assert!(kinds(&m0).contains(&"full"), "首帧应发整帧: {:?}", kinds(&m0));

    // 1 KB/s：连一次小编辑都塞不下两次
    p.set_budget_bps(8_000);
    let mut f1 = f0.clone();
    f1.ts_us = 100_000;
    edit(&mut f1, 5, 5); // 1 块 = 1.6%
    let k1 = kinds(&p.on_frame(&f1).unwrap());
    assert!(
        !k1.contains(&"video-on") && !p.video_mode(),
        "光标级别的变化不该被窄预算顶到有损平面: {k1:?}"
    );
    assert_eq!(p.stats.budget_forced, 0);

    // 同一预算下改到 10 块（15.6%）——这次是真装不下，必须交出去
    let mut f2 = f1.clone();
    f2.ts_us = 200_000;
    for i in 0..10u32 {
        edit(&mut f2, i * 48, i * 48);
    }
    let k2 = kinds(&p.on_frame(&f2).unwrap());
    assert!(
        k2.contains(&"video-on") && p.video_mode(),
        "成片变化在窄预算下仍该交给媒体平面: {k2:?}"
    );
    assert_eq!(p.stats.budget_forced, 1);
}

#[test]
fn tight_budget_forces_media_plane_and_recovers_with_repair_first() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let m0 = p.on_frame(&f0).unwrap();
    assert!(kinds(&m0).contains(&"full"), "首帧应发整帧: {:?}", kinds(&m0));

    // 1 KB/s：一次小编辑的量都塞不下两次，无损通路必须让路
    p.set_budget_bps(8_000);
    let mut f1 = f0.clone();
    edit(&mut f1, 5, 5);
    let m1 = p.on_frame(&f1).unwrap();
    let k1 = kinds(&m1);
    assert!(k1.contains(&"video-on"), "预算不足应把画面交给媒体通道: {k1:?}");
    assert!(p.video_mode(), "平面状态没跟上");
    assert_eq!(p.stats.budget_forced, 1);

    // 预算仍然紧：继续改画面也不许退回无损（每次退出都要补一次修复，那正是
    // 链路装不下的东西）
    for i in 2..6u32 {
        let mut f = f1.clone();
        f.ts_us = i as u64 * 100_000;
        edit(&mut f, i * 3, i * 3);
        let k = kinds(&p.on_frame(&f).unwrap());
        assert!(!k.contains(&"update"), "预算仍紧却退回无损: {k:?}");
        assert!(p.video_mode(), "预算仍紧却提前退出平面");
    }
    assert_eq!(p.stats.budget_forced, 1, "不该反复触发切换");

    // 预算解除：迟滞走完必须退回无损，且 off 之前先有修复批次
    p.set_budget_bps(0);
    let mut order = None;
    for i in 6..12u32 {
        let mut f = f1.clone();
        f.ts_us = i as u64 * 100_000;
        let k = kinds(&p.on_frame(&f).unwrap());
        if k.contains(&"video-off") {
            order = Some(k);
            break;
        }
    }
    let order = order.expect("预算解除后没有退回无损平面");
    let i_off = order.iter().position(|x| *x == "video-off").unwrap();
    assert!(
        order[..i_off].contains(&"repair") || order[..i_off].contains(&"full"),
        "video-off 之前必须先发修复批次，否则观看端会退回旧画面: {order:?}"
    );
    assert!(!p.video_mode());
}

/// 脱落**知道掉的是哪些块**时，补账只补那些块，不许兑现成整帧。
///
/// 实测（1080p 半屏单帧突变 + 原生观看端）：一次 `stale=1` 先正确逐块补了 121 块，随后仍被
/// 一份"全齐"的报告兑现成一次 4.2 MB 整帧重建——同一份变化的总价从 2.45 MB 涨到 6.65 MB。
/// 闸门与队列丢弃的都是 Update 批次，块 id 本来就知情，所以这里没有理由花整帧。
#[test]
fn known_lost_tiles_are_repaired_not_escalated_to_a_full_frame() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000_000); // 宽预算：整帧塞得下，旧行为一定会选整帧
    let (lay, ver, eph) = blind(&first);

    p.note_link_loss_tiles(&[TileId(0), TileId(1)]);
    let k = kinds(&p.on_report(lay, &ver, &eph, 200).unwrap());
    assert!(k.contains(&"update"), "已知块该按块补回来: {k:?}");
    assert!(!k.contains(&"full"), "知道掉了谁就不许花整帧: {k:?}");

    // 补过账之后，下一帧也不该再被那份案兑成整帧
    let mut f2 = f0.clone();
    f2.ts_us = 300_000;
    let k2 = kinds(&p.on_frame(&f2).unwrap());
    assert!(!k2.contains(&"full"), "案已按块清掉，却又补了一次整帧: {k2:?}");
}

/// 反向对照：脱落**不知情**（没带块 id）时，整帧兑现那条契约必须还在。
#[test]
fn unknown_loss_still_cashes_out_as_a_full_frame() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000_000);
    let (lay, ver, eph) = blind(&first);
    p.note_link_loss();
    p.on_report(lay, &ver, &eph, 200).unwrap();
    let mut f2 = f0.clone();
    f2.ts_us = 200_000;
    let k = kinds(&p.on_frame(&f2).unwrap());
    assert!(k.contains(&"full"), "不知掉了谁时仍要补一次整帧: {k:?}");
}

/// 预算扣下的那部分不许悄悄销案：留着下一次报告继续补。
#[test]
fn budget_clipped_repair_keeps_the_remaining_tiles_claimed() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    // §6.6 的修复预算读的是**带宽提示**（`repair_bw`），与"要不要换平面"的预算是两条账：
    // 这里把提示压到 800 B/窗口 ≈ 2 块（REPAIR_UNIT 400），链路预算本身仍宽，保证这条
    // 测试测的是"补账被预算截断"，不是"被预算逼进平面"。
    p.set_budget_bps(8_000_000);
    p.ctl.set_bandwidth(Some(32_000));
    let (lay, ver, eph) = blind(&first);
    let owed: Vec<TileId> = (0..p.ctl.version.len()).map(|i| TileId(i as u16)).collect();
    p.note_link_loss_tiles(&owed);

    let a = p.on_report(lay, &ver, &eph, 200).unwrap();
    let b = p.on_report(lay, &ver, &eph, 1300).unwrap(); // 新窗口，剩下的必须续上
    let count = |ms: &[WireMsg]| -> usize {
        ms.iter()
            .map(|m| match m {
                WireMsg::Update { tiles, .. } => tiles.len(),
                _ => 0,
            })
            .sum()
    };
    let (ka, kb) = (kinds(&a), kinds(&b));
    assert!(!ka.contains(&"full") && !kb.contains(&"full"), "知情脱落不许兑成整帧: {ka:?} {kb:?}");
    let na = count(&a);
    assert!(na > 0 && na < owed.len(), "一个预算窗口只该补得动一部分，实补 {na}/{}: {ka:?}", owed.len());
    assert!(count(&b) > 0, "被预算扣下的块必须留案到下一次报告，不许悄悄销案: {kb:?}");
}

/// 反向对照：同样的画面序列，脱落一条都不记时**不该**出整帧。
#[test]
fn no_loss_claim_means_no_rebuild() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000_000);
    let (lay, ver, eph) = blind(&first);
    p.on_report(lay, &ver, &eph, 200).unwrap();
    let mut f2 = f0.clone();
    f2.ts_us = 200_000;
    let k = kinds(&p.on_frame(&f2).unwrap());
    assert!(!k.contains(&"full"), "没记案却补了整帧: {k:?}");
}

/// 反向对照：没有预算限制时，同样的画面序列**不该**碰视频平面。
/// 少了这一条，上面的测试可能只是碰巧绿。
#[test]
fn unlimited_budget_never_leaves_the_lossless_plane() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    p.on_frame(&f0).unwrap();
    let mut f = f0.clone();
    for i in 1..8u32 {
        edit(&mut f, i * 4, i * 4);
        let k = kinds(&p.on_frame(&f).unwrap());
        assert!(!k.iter().any(|x| *x == "video-on"), "没有预算限制却切了平面: {k:?}");
        assert!(k.contains(&"update") || k.is_empty(), "无损通路应正常出差分块: {k:?}");
    }
    assert_eq!(p.stats.budget_forced, 0);
    assert!(!p.video_mode());
}

/// 静止即回退：预算**一直没有放松**，但画面停下来之后必须退回无损。
///
/// 这是真机 `scripts/forced-plane.sh` 抓出来的缺陷：旧退出判据拿"被逼进来那一次的
/// 每秒需求"当尺子，而那个数在平面期内再也不更新，于是动画停了、画面糊着，平面
/// 再也没下来过（观看端拿到的是 320x180 的小字，永远不再是无损）。主流远控的行为
/// 是"停下来就变清晰"，这条钉住它。
#[test]
fn settled_picture_returns_to_lossless_even_while_budget_stays_tight() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000);

    let mut moving = f0.clone();
    moving.ts_us = 100_000;
    edit(&mut moving, 5, 5);
    let k = kinds(&p.on_frame(&moving).unwrap());
    assert!(k.contains(&"video-on"), "预算不足应把画面交给媒体通道: {k:?}");

    // 从此画面不再变化，预算一个字也没放松
    let mut order = None;
    let mut at_frame = 0u32;
    for i in 2..20u32 {
        let mut f = moving.clone();
        f.ts_us = i as u64 * 100_000;
        let k = kinds(&p.on_frame(&f).unwrap());
        // 不许在预算窗口滚过一秒之前回退：那样会来回抖，每次抖动都要补一次修复
        assert!(!k.contains(&"video-off") || i >= 11, "窗口没滚过一秒就退回无损: i={i} {k:?}");
        if k.contains(&"video-off") {
            order = Some(k);
            at_frame = i;
            break;
        }
    }
    let order = order.expect("画面静止后仍赖在有损平面上（旧缺陷回来了）");
    assert!(
        at_frame <= 14,
        "静止后回退太慢（第 {} 帧才退，窗口 1 s = 第 11 帧）",
        at_frame
    );
    let i_off = order.iter().position(|x| *x == "video-off").unwrap();
    assert!(
        order[..i_off].contains(&"repair") || order[..i_off].contains(&"full"),
        "video-off 之前必须先发修复批次: {order:?}"
    );
    assert!(!p.video_mode());
    assert_eq!(p.budget_bps(), 8_000, "预算本来就没放松，回退只能是因为画面静止");
}

/// 退出平面后**再次**被预算逼进来，归因仍然是预算——即使观看端此刻也在落后。
///
/// 真机 forced-plane 的形状：窄预算 → 图块通路压住 → 观看端落后（strain 成立）→
/// 平面进出好几轮。第二轮进入时记的是 `strain_forced=6 / forced_plane=0`，那条
/// "执行器真的压过图块"的契约就没东西证明了。塌陷的原因在需求量本身：它原本取
/// 最近 1 s **实际编码**的字节，而无损通路整个平面期一个字都不编，窗口滚过一秒后
/// 读数归零 ⇒ room 成立 ⇒ 跨帧存活的 strain 抢先。修法是需求量按窗口内的**新变化
/// 块数 × 已观测平均块大小**折算（`predict_egress`），它跟实测同量级，但不会因为
/// "通路正被自己压着"而塌成 0。
#[test]
fn reentry_after_a_plane_period_is_still_credited_to_the_budget() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000);

    // 第一次进入：一次小编辑就超出这条窄管子
    let mut moving = f0.clone();
    moving.ts_us = 100_000;
    edit(&mut moving, 5, 5);
    let k = kinds(&p.on_frame(&moving).unwrap());
    assert!(k.contains(&"video-on"), "前置条件：第一次由预算逼进平面: {k:?}");
    assert_eq!(p.stats.budget_forced, 1);

    // 画面静止 → 迟滞走完 → 退回无损（修复批次可能为空，无损通路因此没有新的编码量）
    let mut i = 2u32;
    loop {
        let mut f = moving.clone();
        f.ts_us = i as u64 * 100_000;
        let k = kinds(&p.on_frame(&f).unwrap());
        i += 1;
        if k.contains(&"video-off") {
            break;
        }
        assert!(i < 30, "画面静止却退不出平面");
    }
    assert!(!p.video_mode());

    // 观看端是因为我们压住图块才落后的：strain 现在成立。窗口已经滚过一整秒，
    // 里面**实际编码**的字节是 0——旧写法会把这个需求读成 0。
    p.receiver_strain = true;
    let mut f = moving.clone();
    f.ts_us = i as u64 * 100_000;
    edit(&mut f, 40, 40);
    let k = kinds(&p.on_frame(&f).unwrap());
    assert!(
        k.contains(&"video-on"),
        "预算没放松、画面又变了，应该再次交给媒体通道: {k:?}"
    );
    assert_eq!(p.stats.budget_forced, 2, "第二次进入仍该记在预算头上");
    assert_eq!(p.stats.strain_forced, 0, "strain 不该从预算手里抢走归因");
}

/// 脱落补账的取舍：链路塞得下一次整帧重建 ⇒ 走整帧（可靠通路，不会被脱落丢掉）。
///
/// 存在的理由：版本向量与 `last_sent` 是在**产出消息**那一刻提交的，传输层丢掉一条无损更新
/// 不会自动退回去 —— 发送端与观看端向量一致而像素不一致，报告看不出缺口（真机 #52：
/// `resends=0` 而画布整片旧着）。所以脱落必须由管线补账，而不是指望报告驱动。
///
/// 补账的**前提**是那次报告真的看不出缺口（#53）：健康链路上的偶发脱落会体现在报告里，
/// 由逐块修复补回来，这里抢着重建整帧就是每条链路白花一个整帧。
#[test]
fn link_loss_rebuilds_the_baseline_when_the_budget_can_carry_it() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000_000); // 宽预算：一次整帧重建塞得下

    let (lay, ver, eph) = blind(&first);
    p.note_link_loss();
    p.on_report(lay, &ver, &eph, 200).unwrap();

    let mut f2 = f0.clone();
    f2.ts_us = 200_000;
    let k = kinds(&p.on_frame(&f2).unwrap());
    assert!(
        k.contains(&"full"),
        "预算装得下重建时就该补一次整帧，而不是把丢掉的块永远欠着: {k:?}"
    );
    assert!(!p.video_mode(), "宽链路上不该因为脱落去换平面");
    assert_eq!(p.stats.budget_forced, 0, "这条补的是整帧账，不是预算逼出来的平面账");
}

/// 补账不许每半秒要一次整帧。第一份"看不出缺口"的报告排上重建之后，紧跟着的几份报告
/// 不能再排第二次——重建那份走的是 baseline + 可靠通路，它要么在路上要么已经到了。
/// 实测（1080p 照片类半屏单帧突变）这个循环让一张静止画面为一次 2.3 MB 的变化累计发了
/// **10 个整帧 = 36 MB**（对照：打断循环只要 1 个整帧 / 8.6 MB），队列被顶到 3–5、
/// 后续 108 个批次被判过时丢掉。
///
/// 反过来也要钉住：冷却期满，补账的权力必须回来——冷却不是永久弃权。
#[test]
fn loss_rebuild_is_rate_limited_to_one_per_report_round_trip() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000_000);
    let (lay, ver, eph) = blind(&first);

    p.note_link_loss();
    p.on_report(lay, &ver, &eph, 200).unwrap();
    let mut f = f0.clone();
    f.ts_us = 200_000;
    let k = kinds(&p.on_frame(&f).unwrap());
    assert!(k.contains(&"full"), "第一次补账该排上整帧重建: {k:?}");

    // 画面从此不动，报告继续"看不出缺口"：1500 ms 墙钟冷却之内不许再排。
    // 报告故意排得比默认周期密（800 / 1300 ms）——早报是正常行为，它不该把冷却压短。
    let mut extra = 0;
    for (i, at) in [800u64, 1300].into_iter().enumerate() {
        p.note_link_loss();
        p.on_report(lay, &ver, &eph, at).unwrap();
        let mut g = f.clone();
        g.ts_us = f.ts_us + (i as u64 + 1) * 100_000;
        if kinds(&p.on_frame(&g).unwrap()).contains(&"full") {
            extra += 1;
        }
    }
    assert_eq!(extra, 0, "冷却期内又排了 {extra} 次整帧重建：报告往返被当成了重发许可证");

    // 墙钟满 1500 ms 之后，补账的权力必须回来
    p.note_link_loss();
    p.on_report(lay, &ver, &eph, 1800).unwrap();
    let mut h = f.clone();
    h.ts_us = 1_800_000;
    let k = kinds(&p.on_frame(&h).unwrap());
    assert!(k.contains(&"full"), "冷却期满后必须还能再补一次账: {k:?}");
}

/// 反过来：报告**承认**缺口 ⇒ 逐块修复路径接手，不许抢它的活去重建整帧（#53 的门禁）。
/// 修好前这条在健康链路上每次都多花一个整帧（实测 `full=2`）。
#[test]
fn link_loss_a_report_can_see_is_left_to_the_repair_path() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000_000);

    // 先改一块：发送端给那块记了新版本，于是"报告全 0"就是承认缺一块
    let mut f = f0.clone();
    f.ts_us = 100_000;
    edit(&mut f, 5, 5);
    p.on_frame(&f).unwrap();

    let (lay, ver, _) = blind(&first);
    let none = vec![0u32; ver.len()];
    p.note_link_loss();
    let repaired = p.on_report(lay, &none, &none, 200).unwrap();
    assert!(!repaired.is_empty(), "承认的缺口该由逐块修复补回来");

    let mut f2 = f.clone();
    f2.ts_us = 200_000;
    let k = kinds(&p.on_frame(&f2).unwrap());
    assert!(!k.contains(&"full"), "报告看得出缺口时不许白花整帧重建: {k:?}");
    assert!(!p.video_mode(), "也不该因为脱轮换平面: {k:?}");
}

/// 反过来：链路连一次整帧重建都塞不下 ⇒ 不该再往窄管子里灌整帧，把画面交给媒体平面，
/// 并且**记在预算那条账上**。第二点是关键：会话层原来直接 `force_full()`，于是 forced-plane
/// 那条"执行器真的压过图块"的证明被脱落抢走（实测 `forced_plane=0`）。
#[test]
fn link_loss_hands_over_to_the_media_plane_when_a_rebuild_wont_fit() {
    let mut p = sender();
    let f0 = noise(0, 128, 128);
    let first = p.on_frame(&f0).unwrap();
    p.set_budget_bps(8_000); // 窄预算：连一次整帧重建都塞不下

    let (lay, ver, eph) = blind(&first);
    // 画面**不再变化**：差分是空的，所以这一帧的交接只能由脱落引起
    p.note_link_loss();
    p.on_report(lay, &ver, &eph, 200).unwrap();
    let mut f = f0.clone();
    f.ts_us = 200_000;
    let k = kinds(&p.on_frame(&f).unwrap());
    assert!(
        k.contains(&"video-on"),
        "装不下重建就该把画面交给媒体平面（差分是空的，只有脱落能解释这次交接）: {k:?}"
    );
    assert!(p.video_mode(), "平面状态没跟上");
    assert_eq!(p.stats.budget_forced, 1, "归因记在预算头上，执行器的证明不许被脱落顶掉");
}

/// 平面期间观看端画布**根本没被有损像素碰过**时（H264 自己出画、没有 JPEG 兜底帧），
/// 退出必须用定点修复，不许补整帧。
///
/// 这条不是洁癖，是 #1 的形态：整帧在 1080p 照片类内容上是 510 块 / ~3.5 MB，观看端要
/// 逐块盖好几秒才盖满——用户看到的"很多个图块慢慢覆盖"就是这个。真机产品路径实测
/// （照片类半屏突变 + 允许媒体平面）：`switches=4`、观看端 `fulls` 2→4，而真正变化的
/// 255 块**从未**以定点修复落地。
///
/// 反过来，JPEG 兜底帧真的盖过画布时基线不可信，必须整帧——那是 #56 的教训，由
/// `motion-stays-lossless` 钉着，这条不许把它带翻。
#[test]
fn plane_exit_uses_targeted_repair_when_no_motion_jpeg_painted_the_canvas() {
    let mut p = sender();
    p.motion_enabled = false; // 媒体通道出画，无 JPEG 兜底帧落画布
    let f0 = noise(0, 128, 128);
    p.on_frame(&f0).unwrap();

    p.set_budget_bps(8_000);
    let mut f1 = f0.clone();
    f1.ts_us = 100_000;
    edit(&mut f1, 5, 5);
    let k1 = kinds(&p.on_frame(&f1).unwrap());
    assert!(k1.contains(&"video-on"), "预算不足应把画面交给媒体通道: {k1:?}");
    assert_eq!(p.stats.motion_frames, 0, "这一臂不该编出 JPEG 兜底帧");

    // 预算解除、迟滞走完 → 退回无损
    p.set_budget_bps(0);
    let mut exit = None;
    for i in 2..14u32 {
        let mut f = f1.clone();
        f.ts_us = i as u64 * 100_000;
        let k = kinds(&p.on_frame(&f).unwrap());
        if k.contains(&"video-off") {
            exit = Some(k);
            break;
        }
    }
    let k = exit.expect("静止后没退回无损平面");
    assert!(
        k.contains(&"repair"),
        "平面期间画布没被有损帧碰过，退出却补整帧: {k:?}"
    );
    assert!(!k.contains(&"full"), "定点修复已经完备，不该再来一次全量重建: {k:?}");
    assert!(!p.video_mode());
}

fn feed_recv(recv: &mut ess_core::pipeline::RecvPipeline, msgs: Vec<WireMsg>) {
    for m in &msgs {
        let bytes = ess_core::wire::encode(m).expect("wire encode");
        let decoded = ess_core::wire::decode(&bytes).expect("wire decode");
        recv.on_wire(&decoded).expect("recv");
    }
}

/// 判别 #11 的两条候选根因，用现场那种**最底下不满 64 像素的窄块**。
///
/// 现场证据（`target/motion-pass` vs `target/motion-fail`，同一份二进制、空载）：失败臂从第 3 秒起
/// `forced_plane=1` 并保持（`vmode=true` 5 秒 vs 成功臂 4 秒），两臂的退出修复块数同为 238、观看端
/// 都确实应用了 238 块，而全程 `stale`/`missing`/`behind` 为 0、一条"链路累计脱落"都没记——
/// 画布却与期望差 864 个像素，全部落在底行那个 64×16 窄块里：**版本对得上，像素差一步**。
///
/// 这条测试问的就是那个窄块：平面期内只改它，退出时必须被精确修复补回来。
/// 红 ⇒ 根因 A（平面期把版本号提交了、像素却没进"基线↔当前"的差集）；
/// 绿 ⇒ A 在这个形状上不成立，转根因 B（观看端应用顺序）去查。
#[test]
fn a_partial_row_tile_changed_during_the_plane_is_repaired_on_exit() {
    // 128x100 配 64 的块 ⇒ 底行只有 36 像素高，正是现场出问题的那一行
    let f0 = noise(0, 128, 100);
    let mut p = SendPipeline::new(f0.clone(), codecs());
    p.video_plane = true;
    p.motion_enabled = false;
    p.hold_frames = 2;
    let mut recv = ess_core::pipeline::RecvPipeline::new(codecs());

    let m0 = p.on_frame(&f0).unwrap();
    assert!(kinds(&m0).contains(&"full"), "首帧应发整帧: {:?}", kinds(&m0));
    feed_recv(&mut recv, m0);
    assert_eq!(recv.canvas.px, f0.px, "首帧落画");

    // 进平面：预算压到地板，图块通路塞不下
    p.set_budget_bps(1_000);
    let mut f1 = f0.clone();
    f1.ts_us = 100_000;
    edit(&mut f1, 8, 8);
    let k1 = kinds(&p.on_frame(&f1).unwrap());
    assert!(k1.contains(&"video-on"), "预算不足该把画面交给媒体通道: {k1:?}");

    // 平面期内：只改底行那块（y=70 落在 64..99 这一条 36 像素高的窄块里）
    let mut f2 = f1.clone();
    f2.ts_us = 200_000;
    edit(&mut f2, 8, 70);
    p.on_frame(&f2).unwrap();

    // 松开预算，等退回无损并收下退出修复
    p.set_budget_bps(0);
    let mut exit_msgs: Vec<WireMsg> = Vec::new();
    for i in 3..16u32 {
        let mut f = f2.clone();
        f.ts_us = i as u64 * 100_000;
        let msgs = p.on_frame(&f).unwrap();
        if kinds(&msgs).contains(&"video-off") {
            exit_msgs = msgs;
            break;
        }
    }
    assert!(!exit_msgs.is_empty(), "静止后没退回无损平面");
    feed_recv(&mut recv, exit_msgs);

    let bad = recv
        .canvas
        .px
        .iter()
        .zip(f2.px.iter())
        .position(|(a, b)| a != b);
    match bad {
        None => {}
        Some(o) => {
            let px = o / 4;
            panic!(
                "退出修复没把画面补全：首个不同像素在 (#x={}, #y={})，底行窄块从 y=64 开始",
                px % 128,
                px / 128
            );
        }
    }
}
