//! 链路预算执行器：无损通路装不下时把画面交给媒体通道，预算松了再退回无损。
//!
//! 这条路径的存在理由：窄链路上继续推无损块只会让发包排队延迟涨到观看端卡死，
//! 而"有损但流畅"才是产品级验收要的画面。测试用 RawCodec + 确定性时间戳，
//! 不依赖网络。

use ess_core::codec::RawCodec;
use ess_core::frame::Frame;
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
