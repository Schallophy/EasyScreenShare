//! 观看端能力信号：报告里大面积落后 ⇒ 把大面积变化交给媒体平面。
//!
//! 动机（用户真实场景）：手机上跑的是浏览器观看端，图块通路要在 wasm 里逐块解码再画
//! canvas，能力只有桌面的几分之一。发送端原来只看**自己**的代价（链路预算 / 单帧编码
//! 耗时），完全不知道对面啃不动——实测同一份 1080p「视频+弹幕」内容，桌面 ess-view 会
//! 切媒体平面，浏览器观看端却停在图块平面硬扛。落后两版以上才算「跟不上」：落后一版是
//! 在途或一次丢包，下一次报告就补平了，所以这个判据天然不会因抖动反复切平面。
//!
//! 分工：`lagging_report_marks_the_pipeline_strained` 测「报告 → 标志」的传导，
//! `strain_alone_hands_the_picture...` 测「标志 → 平面决策」的接线，
//! `budget/encode_keeps...` 测**归因优先级**（strain 是跨帧存活的报告量，不许抢掉
//! 预算/编码那两条契约自己的计数），
//! `sender_strain_needs_both...` 测判据本身（广度 × 深度 × 网格下限）。
//! 不用大面积变化内容去测接线，是因为那种内容本来就该走运动通路，测不出归因。

use ess_core::codec::{RawCodec, TileCodec};
use ess_core::frame::Frame;
use ess_core::model::Grid;
use ess_core::model::Sender;
use ess_core::pipeline::{CodecSet, SendPipeline};
use ess_core::wire::WireMsg;

/// 全屏噪声：大面积、非平移，本来就该走运动/媒体通路（只用来验证标志传导）。
fn noise(k: u32, w: u32, h: u32) -> Frame {
    let mut f = Frame::new(w, h, k as u64 * 33_000);
    for y in 0..h {
        for x in 0..w {
            let o = y as usize * f.stride + x as usize * 4;
            let mut v = x
                .wrapping_mul(0x9e37_79b1)
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

/// 只有左上角一块在变的「轻载」内容（512x512 = 8x8 = 64 块，单块 ≈1.6% 画面）：
/// 运动检测与链路预算都不会触发平面，所以平面一旦出现只可能是 strain 造成的。
fn mild(k: u32) -> Frame {
    let mut f = Frame::new(512, 512, k as u64 * 33_000);
    for y in 0..64u32 {
        for x in 0..64u32 {
            let o = y as usize * f.stride + x as usize * 4;
            f.px[o..o + 4].copy_from_slice(&(0xff00_0000u32 | k).to_le_bytes());
        }
    }
    f
}

/// 比 `mild` 多改 11 块（共 12/64 ≈ 19%）：超过预算/编码平面的变化率门槛（10%），
/// 但仍低于运动阈值（25%），所以平面出现只可能来自被测试的那条判据。
fn moderate(k: u32) -> Frame {
    let mut f = mild(k);
    for t in 1..12u32 {
        let bx = (t % 8) * 64;
        let by = (t / 8) * 64;
        for y in by..by + 64 {
            for x in bx..bx + 64 {
                let o = y as usize * f.stride + x as usize * 4;
                f.px[o..o + 4].copy_from_slice(&(0xff00_0000u32 | (k + t)).to_le_bytes());
            }
        }
    }
    f
}

fn codecs() -> CodecSet<'static> {
    CodecSet { codecs: Box::leak(vec![&RawCodec as &dyn TileCodec].into_boxed_slice()) }
}

fn sender(video_plane: bool) -> SendPipeline<'static> {
    let mut p = SendPipeline::new(mild(0), codecs());
    p.video_plane = video_plane;
    p.set_budget_bps(0); // 不限预算：让「链路装不下」这条理由永远不成立
    p.set_encode_pressure(false);
    p
}

#[test]
fn strain_alone_hands_the_picture_to_the_media_plane() {
    // 接线完成：无损通路**此刻**装得下（预算不限、编码不超压），但观看端连续两次报告
    // 大面积落后 ≥2 版 ⇒ 是它啃不动，画面就该交给硬件 H264。手机上跑的就是这份账：
    // 发送端只看自己的代价，永远不知道对面有多快。
    let mut p = sender(true);
    for k in 1..4 {
        p.on_frame(&mild(k)).unwrap();
    }
    p.receiver_strain = true;
    let out = p.on_frame(&mild(4)).unwrap();
    assert!(p.video_mode(), "观看端跟不上时，装得下的无损通路也得让位");
    assert_eq!(p.stats.strain_forced, 1, "这一帧的归因是 strain");
    assert_eq!(p.stats.budget_forced, 0);
    assert!(
        out.iter()
            .any(|m| matches!(m, WireMsg::VideoMode { on: true, .. })),
        "交接必须宣告出去，否则观看端还在硬扛图块"
    );
}

#[test]
fn the_strain_gate_can_be_closed_for_controlled_experiments() {
    // 受控实验用：证明"预算执行器"时，必须让能力信号让开，否则慢机器上的观看端会先
    // 真的落后，strain 抢在预算前面翻平面，预算那条契约当帧就记不到账（§14.7.37）。
    // 关掉之后连"落后"本身都不该把画面交出去。
    let mut p = sender(true);
    for k in 1..4 {
        p.on_frame(&mild(k)).unwrap();
    }
    p.receiver_strain = true;
    p.set_strain_gate(false);
    p.on_frame(&mild(4)).unwrap();
    assert!(!p.video_mode(), "关掉能力信号后它不该再单独翻平面");
    assert_eq!(p.stats.strain_forced, 0);
}

#[test]
fn budget_keeps_the_attribution_when_strain_is_also_true() {
    // strain 是**跨帧存活**的报告量，可能恰好在"这一帧差分很小、装得下"的时刻先说话。
    // 归因按此刻真正成立的那条判据走：链路装不下就记预算——forced-plane 那条契约证明的是
    // 执行器真的压过图块，被 strain 抢掉计数它就再也证不出来了（实测 strain_forced=6 /
    // forced_plane=0）。
    let mut p = sender(true);
    p.set_budget_bps(1); // 什么都装不下
    p.receiver_strain = true;
    for k in 1..4 {
        p.on_frame(&moderate(k)).unwrap();
    }
    assert!(p.video_mode());
    assert!(p.stats.budget_forced >= 1, "归因给此刻真正装不下的那一条");
    assert_eq!(p.stats.strain_forced, 0);
}

#[test]
fn encode_pressure_outranks_strain_in_the_attribution() {
    // 同上，只不过这次是编码耗时成立：单帧编码已经超了帧预算，那条路自己会去交画面，
    // 账该记在它头上（`encode_forced`），strain 只是凑巧也真。
    let mut p = sender(true);
    p.set_encode_pressure(true);
    p.receiver_strain = true;
    for k in 1..4 {
        p.on_frame(&moderate(k)).unwrap();
    }
    assert!(p.video_mode());
    assert!(p.stats.encode_forced >= 1);
    assert_eq!(p.stats.strain_forced, 0);
}

#[test]
fn a_weak_client_short_circuits_the_warm_up() {
    // 预热的全部理由是"把编码器起步的代价藏在观众看不见的画面上"。但如果对面**已经**
    // 啃不动这份画面，多顶一秒就是多扛一秒的重负载——手机正是这种情况。所以能力证据
    // 必须能立刻打断预热，而不是等满上限。
    let mut p = sender(true);
    p.set_link_bps(100_000_000, 30);
    for k in 1..=3u32 {
        p.on_frame(&noise(k, 512, 512)).unwrap();
    }
    assert!(p.media_pending(), "大面积运动先预热，不该已经交出去");
    assert!(!p.video_mode());
    p.receiver_strain = true;
    let out = p.on_frame(&noise(4, 512, 512)).unwrap();
    assert!(p.video_mode(), "能力证据到位就必须马上交，预热不许再挡");
    assert!(
        out.iter()
            .any(|m| matches!(m, WireMsg::VideoMode { on: true, .. })),
        "交接要在线上宣告出来"
    );
    assert_eq!(p.stats.strain_forced, 1, "这一帧的归因仍是 strain");
    assert_eq!(p.stats.budget_forced, 0, "宽链路上不该记到预算头上");
}

#[test]
fn a_catching_up_client_is_left_on_the_lossless_plane() {
    // 负对照：同样的轻载内容，strain 不加，平面一次都不该出现。
    let mut p = sender(true);
    for k in 1..8 {
        p.on_frame(&mild(k)).unwrap();
    }
    assert!(!p.video_mode(), "跟得上的观看端不该被换到有损平面");
    assert_eq!(p.stats.strain_forced, 0);
}

#[test]
fn lagging_report_marks_the_pipeline_strained() {
    let mut p = sender(false);
    for k in 1..4 {
        p.on_frame(&noise(k, 512, 512)).unwrap();
    }
    let n = p.ctl.version.len();
    let (zero, id) = (vec![0u32; n], p.ctl.layout_id);
    // 先看一次"跟上"的报告：能力判据比的是本来跟得上、现在跟不上了（见
    // `a_client_that_never_caught_up_is_not_capability_evidence`）。
    let (v0, e0, id0) = (p.ctl.version.clone(), p.ctl.epoch.clone(), p.ctl.layout_id);
    p.on_report(id0, &v0, &e0, 900).unwrap();
    p.on_report(id, &zero, &zero, 1_000).unwrap();
    assert!(!p.receiver_strain, "单次落后报告不该就算能力不足（可能是整帧在途/预算压制）");
    p.on_report(id, &zero, &zero, 1_500).unwrap();
    assert!(p.receiver_strain, "连续两次大面积落后必须传导成 strain");

    // 第三次报告带上发送端自己的版本与 epoch（= 完全跟上），标志必须清掉。
    let (v, e, id2) = (p.ctl.version.clone(), p.ctl.epoch.clone(), p.ctl.layout_id);
    p.on_report(id2, &v, &e, 2_000).unwrap();
    assert!(!p.receiver_strain, "跟上的报告必须把 strain 清掉，否则平面再也退不出来");
}

#[test]
fn a_client_that_never_caught_up_is_not_capability_evidence() {
    // 真机 forced-plane 的形状：观看端刚加入，整帧还在路上，报告里全是 0。连着两次
    // "大面积落后"如果就当能力证据，会话第 1 秒就会把画面交给媒体轨道——那条轨道当时
    // 被压到 300 kbps，编码器自己降到 384x216，观看端看到的是糊掉的小字，而它下一秒
    // 就要把无损整帧落画了。落后必须相对于"本来跟得上"这个基线才说明问题。
    let mut p = sender(false);
    for k in 1..6 {
        p.on_frame(&noise(k, 512, 512)).unwrap();
    }
    let n = p.ctl.version.len();
    let (zero, id) = (vec![0u32; n], p.ctl.layout_id);
    for t in 0..6 {
        p.on_report(id, &zero, &zero, 1_000 + t * 500).unwrap();
    }
    assert!(!p.receiver_strain, "从未跟上过的观看端没有能力基线");
    assert!(!p.video_mode(), "更不能因为它将画面交给有损平面");

    // 补一次跟上的报告，之后再落后就成立了——基线一旦建立就不因一次抖动消失。
    let (v, e, id2) = (p.ctl.version.clone(), p.ctl.epoch.clone(), p.ctl.layout_id);
    p.on_report(id2, &v, &e, 4_000).unwrap();
    p.on_report(id, &zero, &zero, 4_500).unwrap();
    assert!(!p.receiver_strain, "仍然要连续两次");
    p.on_report(id, &zero, &zero, 5_000).unwrap();
    assert!(p.receiver_strain, "见过跟上之后的连续落后才是能力证据");
}

#[test]
fn a_report_that_lands_before_the_first_block_is_not_a_baseline() {
    // 真机顺序：观看端先连上、发送端隔一秒才起。这中间送到的那份报告里，观看端版本是 0，
    // 发送端的版本**也还是 0**——差集天然为空，而"空差集 = 跟上了"会把基线立成真。
    // 基线一假，整帧还在路上的那几份报告（`missing=240`）就被读成"啃不动"，冷启动第一秒
    // 把画面交给被压到 300 kbps 的轨道（编码器自己降到 384x216），而那正是 §14.7.4 想挡的。
    let mut p = sender(false);
    let (id, zero) = (p.ctl.layout_id, vec![0u32; p.ctl.version.len()]);
    p.on_report(id, &zero, &zero, 0).unwrap();
    assert!(!p.ctl.ever_current, "还没发过任何块时的空差集不是基线");

    // 发送端开始出帧，观看端一块都还没落——这几份报告不许变成能力证据。
    for k in 1..4 {
        p.on_frame(&noise(k, 512, 512)).unwrap();
    }
    for t in 0..4 {
        p.on_report(id, &zero, &zero, 500 + t * 500).unwrap();
    }
    assert!(!p.ctl.ever_current, "落后报告不会反向建立基线");
    assert!(!p.receiver_strain, "整帧还在路上时不许判成啃不动");

    // 真正跟上一次以后，基线才成立（后续落后才算证据）。
    let (v, e, id2) = (p.ctl.version.clone(), p.ctl.epoch.clone(), p.ctl.layout_id);
    p.on_report(id2, &v, &e, 3_000).unwrap();
    assert!(p.ctl.ever_current, "见过一次完全跟上才立基线");
}

#[test]
fn a_joining_viewer_does_not_inherit_the_previous_baseline() {
    // 会话只有一条管线，观看端重连（切网、刷新页面）走的是 `force_full`：整帧重建画布。
    // 基线如果留着，新观看端就从上一块画布那里继承了"曾经跟上"，重连后的第一秒——
    // 整帧还在路上、报告里全是 0——会被判成"啃不动"，画面立刻被交给窄媒体轨道。
    let mut p = sender(true);
    for k in 1..4 {
        p.on_frame(&noise(k, 512, 512)).unwrap();
    }
    let (v, e, id) = (p.ctl.version.clone(), p.ctl.epoch.clone(), p.ctl.layout_id);
    p.on_report(id, &v, &e, 1_000).unwrap();
    assert!(p.ctl.ever_current, "前置条件：老观看端曾经跟上过");

    p.force_full();
    assert!(!p.ctl.ever_current, "整帧重建后基线要重新挣");

    let (id2, zero) = (p.ctl.layout_id, vec![0u32; p.ctl.version.len()]);
    for t in 0..3 {
        p.on_report(id2, &zero, &zero, 2_000 + t * 500).unwrap();
    }
    assert!(!p.receiver_strain, "重连后第一秒的落后不是能力证据");
}

#[test]
fn reports_during_the_plane_do_not_latch_strain() {
    // 平面期间发送端本来就不发图块，观看端的版本当然停在原地。把它读成"跟不上"会让
    // 平面永远退不出来（实测 forced-plane：画面静止后仍留在有损平面，终帧逐字节不一致）。
    // 先跑几帧大面积变化，把发送端自己的版本推上去（这样"报 0"才真的是大面积落后两版）；
    // 否则 behind 只有 1 块，测出来的是判据的下限而不是平面门控。
    let mut p = sender(true);
    for k in 0..3 {
        p.on_frame(&noise(k, 512, 512)).unwrap();
    }
    assert!(p.ctl.version.iter().any(|&v| v >= 2), "前置条件：版本被推起来了");
    // 先建立"跟得上"的基线，否则测到的是 ever_current 门而不是平面门
    let (nv, ne, nid) = (p.ctl.version.clone(), p.ctl.epoch.clone(), p.ctl.layout_id);
    p.on_report(nid, &nv, &ne, 4_000).unwrap();
    assert!(p.ctl.ever_current, "前置条件：基线建立了");
    p.motion_enabled = false; // 平面切换改由编码超压驱动，strain 只作被测状态
    p.set_encode_pressure(true);
    p.on_frame(&noise(3, 512, 512)).unwrap();
    assert!(p.video_mode(), "前置条件：此时应已在媒体平面里");
    p.set_encode_pressure(false);
    let n = p.ctl.version.len();
    let (z, id) = (vec![0u32; n], p.ctl.layout_id);
    p.on_report(id, &z, &z, 5_000).unwrap();
    p.on_report(id, &z, &z, 5_500).unwrap();
    assert!(!p.receiver_strain, "平面期间的落后报告不能算能力证据");
}

#[test]
fn sender_strain_needs_both_breadth_and_depth() {
    let mut s = Sender::new(Grid { cols: 8, rows: 8 });
    // 小网格（<16 块）不参与判定：统计意义不够，一次突发就翻平面。
    let mut small = Sender::new(Grid { cols: 2, rows: 2 });
    for i in 0..s.version.len() {
        s.version[i] = 5;
    }
    for i in 0..small.version.len() {
        small.version[i] = 5;
    }
    let (ls, lsm) = (s.layout_id, small.layout_id);
    let (z64, z4) = (vec![0u32; 64], vec![0u32; 4]);
    // 建立"见过跟上"的基线（能力判据的比较起点，见 `Sender::ever_current`）
    let (sv, se) = (s.version.clone(), s.epoch.clone());
    s.on_report(ls, &sv, &se, 5);
    let (smv, sme) = (small.version.clone(), small.epoch.clone());
    small.on_report(lsm, &smv, &sme, 5);
    s.on_report(ls, &z64, &z64, 10);
    assert!(!s.receiver_strain(), "一次报告不算能力证据");
    s.on_report(ls, &z64, &z64, 15);
    s.on_report(ls, &z64, &z64, 20);
    assert!(s.receiver_strain(), "连续两次以上的大面积落后才是能力证据");
    small.on_report(lsm, &z4, &z4, 10);
    small.on_report(lsm, &z4, &z4, 15);
    assert!(!small.receiver_strain(), "4 块的网格不该被当成能力证据");

    // 广度够、深度不够（每块只落后 1 版）同样不算。
    let mut shallow = Sender::new(Grid { cols: 8, rows: 8 });
    for i in 0..shallow.version.len() {
        shallow.version[i] = 1;
    }
    let lsh = shallow.layout_id;
    shallow.on_report(lsh, &z64, &z64, 10);
    assert!(!shallow.receiver_strain(), "只落后一版是在途/丢包，不是跟不上");
}
