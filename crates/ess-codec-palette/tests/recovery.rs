//! 报告重发的正确性：内容丢失之后，观看端的快照必须把画面要回**当前**内容，而不是要回发送端
//! 平移基准（`state.last_sent`）里的旧像素。
//!
//! 写这两条的动机是现场两臂的观测：纯滚动臂（`ESS_DEMO_BLOCK=0`、`switches=0`、滚动宣告 77 次）
//! 零丢包、版本向量对得上，画布却差**整条顶部块行**（y0..63、20 块、75 200 像素）。
//!
//! - [`total_content_loss_is_recovered_by_viewer_reports`]：内容消息一条不落地（宣告也扣），
//!   只靠周期报告收账。版本与 `last_sent` 在产出那一刻一起提交 ⇒ 这条应该绿。
//! - [`scrolled_band_recovered_by_report_is_not_the_shifted_baseline`]：**对照臂**，滚动宣告照常
//!   落地、只扣腾出来的那条带。接收端自己平移对了、缺的是发送端补给它的那一条 —— 现场那条差异
//!   正落在"平移腾出来的边"上，所以这条才是贴形状的判据。
//!
//! 两条都跑在 ess-codec-palette 而不是 ess-session：这里只用到管线与真编解码器，而 `ess-session`
//! 挂着 livekit→webrtc-sys，改一行测试要重下一份 webrtc 归档（实测 32 KB/s 起了 40 min 还没完）。

use ess_codec_palette::PaletteZstdCodec;
use ess_codec_png::PngCodec;
use ess_core::frame::Frame;
use ess_core::model::Message;
use ess_core::pipeline::{CodecSet, RecvPipeline, SendPipeline};
use ess_core::wire::{self, WireMsg};
use ess_core::{SUBTILE, TILE};

/// 代码页风格、每步纵向平移 `SUBTILE` 像素的画面。行纹与行噪点用**同一个**平移坐标，
/// 否则滚动检测认不出来（一次滚动被当成整屏重绘，代价见 O-23）。
fn scroll_page(w: u32, h: u32, base: u32) -> Frame {
    let mut f = Frame::new(w, h, 0);
    let colors = [0xff00_0000u32, 0xffff_ffff, 0xff20_2020, 0xffe0_e0e0, 0xff00_66cc];
    for y in 0..h {
        for x in 0..w {
            let sy = (y / SUBTILE).wrapping_add(base);
            let sx = x / SUBTILE;
            let line = sy ^ (sx >> 2);
            let c = colors[((line ^ (line >> 3)) as usize) % colors.len()];
            let px = if ((x / 2) ^ (sy * SUBTILE)) % 7 == 0 { colors[1] } else { c };
            let s = y as usize * f.stride + x as usize * 4;
            f.px[s..s + 4].copy_from_slice(&px.to_le_bytes());
        }
    }
    f
}

fn feed(recv: &mut RecvPipeline, msgs: Vec<WireMsg>) {
    for m in &msgs {
        let b = wire::encode(m).unwrap();
        recv.on_wire(&wire::decode(&b).unwrap()).unwrap();
    }
}

/// 画布里逐块比对：返回**至少有一个像素不一致**的块坐标 (行, 列)。
/// 只看首个差异像素不够——整幅都旧时首差也是行 0，会把"没补全"和"只缺一行"读成同一条。
fn diff_tiles(recv: &RecvPipeline, want: &Frame) -> Vec<(usize, usize)> {
    let t = TILE as usize;
    let cols = (want.w as usize).div_ceil(t);
    let rows = (want.h as usize).div_ceil(t);
    let mut out = Vec::new();
    for ry in 0..rows {
        for cx in 0..cols {
            let x1 = ((cx + 1) * t).min(want.w as usize);
            let y1 = ((ry + 1) * t).min(want.h as usize);
            let w4 = (x1 - cx * t) * 4;
            let differs = (ry * t..y1).any(|y| {
                let s = y * want.stride + cx * t * 4;
                recv.canvas.px[s..s + w4] != want.px[s..s + w4]
            });
            if differs {
                out.push((ry, cx));
            }
        }
    }
    out
}

fn fmt_diff(diff: &[(usize, usize)]) -> (usize, Vec<usize>, Vec<usize>) {
    let mut rows: Vec<usize> = diff.iter().map(|(r, _)| *r).collect();
    rows.sort_unstable();
    rows.dedup();
    let mut cols: Vec<usize> = diff.iter().map(|(_, c)| *c).collect();
    cols.sort_unstable();
    cols.dedup();
    (diff.len(), rows, cols)
}

/// 夹具尺寸：10 列 × 6 行，底行只有 40 px 高（从 y=320 起）。
const W: u32 = 640;
const H: u32 = 360;
/// 内容只走 8 步就彻底静止；之后的报告只能要回旧账，不能指望"新内容顺路带回来"。
const CONTENT_STEPS: u32 = 8;

/// 扣掉**所有**内容消息（`Update` 与 `Scroll` 都不落地），只让观看端的周期报告收账。
#[test]
fn total_content_loss_is_recovered_by_viewer_reports() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let codecs = CodecSet { codecs: &[&pal, &png] };
    let f0 = scroll_page(W, H, 0);
    let mut send = SendPipeline::new(f0.clone(), codecs);
    send.motion_enabled = false;
    let mut recv = RecvPipeline::new(codecs);
    feed(&mut recv, send.on_frame(&f0).unwrap());

    let mut recovered = 0u32;
    let mut last = f0;
    for i in 1..30u32 {
        let f = scroll_page(W, H, i.min(CONTENT_STEPS));
        let step = send.on_frame(&f).unwrap();
        let kept: Vec<WireMsg> = step
            .into_iter()
            .filter(|m| !matches!(m, WireMsg::Update { .. } | WireMsg::Scroll { .. }))
            .collect();
        feed(&mut recv, kept);
        if i % 3 == 0 {
            recv.tick(i as u64 * 500);
            let Message::Report { layout_id, versions, epochs } = recv.report() else {
                panic!("报告形状变了");
            };
            let reply = send.on_report(layout_id, &versions, &epochs, i as u64 * 500).unwrap_or_default();
            recovered += reply.iter().map(|m| match m {
                WireMsg::Update { tiles, .. } => tiles.len() as u32,
                _ => 0,
            }).sum::<u32>();
            feed(&mut recv, reply);
        }
        last = f;
    }
    // 落定：内容静止、消息一条不扣，最多 12 轮报告（时钟每轮 +500 ms，够修复预算窗口翻转）。
    let mut rounds = 0usize;
    for i in 30..42u32 {
        let f = scroll_page(W, H, CONTENT_STEPS);
        feed(&mut recv, send.on_frame(&f).unwrap_or_default());
        last = f;
        recv.tick(i as u64 * 500 + 15_000);
        let Message::Report { layout_id, versions, epochs } = recv.report() else {
            panic!("报告形状变了");
        };
        let reply = send
            .on_report(layout_id, &versions, &epochs, i as u64 * 500 + 15_000)
            .unwrap_or_default();
        recovered += reply.iter().map(|m| match m {
            WireMsg::Update { tiles, .. } => tiles.len() as u32,
            _ => 0,
        }).sum::<u32>();
        feed(&mut recv, reply);
        rounds += 1;
        if diff_tiles(&recv, &last).is_empty() {
            break;
        }
    }
    let diff = diff_tiles(&recv, &last);
    assert!(
        recovered > 0,
        "扣住全部内容步之后，报告至少要回一些块；一个都没回说明报告看不见缺口"
    );
    let (n, rows, cols) = fmt_diff(&diff);
    assert_eq!(
        diff,
        Vec::<(usize, usize)>::new(),
        "报告重发没能把画面补全：落定 {rounds} 轮后仍差 {n} 块，行={rows:?} 列={cols:?}（顶行=0，底行窄块从 y=320 起），\
         累计要回 {recovered} 块，throttled={} escalations={} report_resends={} resends={}",
        send.ctl.stats.repair_throttled,
        send.ctl.stats.escalations,
        send.ctl.stats.report_resends,
        send.ctl.stats.resends,
    );
}

/// 对照臂：滚动宣告**照常落地**，只扣腾出来的那条带（滚动一步只重发 dirty 块，
/// 所以"扣 Update"= "扣条带"）。这条的判据是报告要回来的必须是**新内容**，
/// 不是平移基准里的那一条。
#[test]
fn scrolled_band_recovered_by_report_is_not_the_shifted_baseline() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let codecs = CodecSet { codecs: &[&pal, &png] };
    let f0 = scroll_page(W, H, 0);
    let mut send = SendPipeline::new(f0.clone(), codecs);
    send.motion_enabled = false;
    let mut recv = RecvPipeline::new(codecs);
    feed(&mut recv, send.on_frame(&f0).unwrap());

    let mut recovered = 0u32;
    let mut scrolls = 0u32;
    let mut last = f0;
    for i in 1..30u32 {
        let f = scroll_page(W, H, i.min(CONTENT_STEPS));
        let step = send.on_frame(&f).unwrap();
        scrolls += step.iter().filter(|m| matches!(m, WireMsg::Scroll { .. })).count() as u32;
        let kept: Vec<WireMsg> = step
            .into_iter()
            .filter(|m| !matches!(m, WireMsg::Update { .. }))
            .collect();
        feed(&mut recv, kept);
        if i % 3 == 0 {
            recv.tick(i as u64 * 500);
            let Message::Report { layout_id, versions, epochs } = recv.report() else {
                panic!("报告形状变了");
            };
            let reply = send.on_report(layout_id, &versions, &epochs, i as u64 * 500).unwrap_or_default();
            recovered += reply.iter().map(|m| match m {
                WireMsg::Update { tiles, .. } => tiles.len() as u32,
                _ => 0,
            }).sum::<u32>();
            feed(&mut recv, reply);
        }
        last = f;
    }
    let mut rounds = 0usize;
    for i in 30..42u32 {
        let f = scroll_page(W, H, CONTENT_STEPS);
        feed(&mut recv, send.on_frame(&f).unwrap_or_default());
        last = f;
        recv.tick(i as u64 * 500 + 15_000);
        let Message::Report { layout_id, versions, epochs } = recv.report() else {
            panic!("报告形状变了");
        };
        let reply = send
            .on_report(layout_id, &versions, &epochs, i as u64 * 500 + 15_000)
            .unwrap_or_default();
        recovered += reply.iter().map(|m| match m {
            WireMsg::Update { tiles, .. } => tiles.len() as u32,
            _ => 0,
        }).sum::<u32>();
        feed(&mut recv, reply);
        rounds += 1;
        if diff_tiles(&recv, &last).is_empty() {
            break;
        }
    }
    let diff = diff_tiles(&recv, &last);
    assert!(
        scrolls > 0,
        "夹具没走出滚动通路（滚动宣告 0 次），这条对照臂就没在比它要比的形状"
    );
    assert!(
        recovered > 0,
        "滚动落地、条带被扣之后报告至少要回一些块；一个都没回说明滚动宣告把缺口掩盖了"
    );
    let (n, rows, cols) = fmt_diff(&diff);
    // 版本审计：差异块上"接收端记的版本/epoch == 发送端当前的"，就是现场那条读数的形状
    // （零丢包、版本对得上、内容却是旧的）。若这里版本并不相等，那问题在"报告没要这些块"，
    // 不在"要回来的内容不对" —— 两种红的修法完全不同。
    let ncols = (W as usize).div_ceil(TILE as usize);
    let stale_but_acknowledged = diff.iter().all(|&(r, c)| {
        let i = r * ncols + c;
        recv.rx.version[i] == send.ctl.version[i] && recv.rx.epoch[i] == send.ctl.epoch[i]
    });
    let audit: Vec<String> = diff
        .iter()
        .take(4)
        .map(|&(r, c)| {
            let i = r * ncols + c;
            format!(
                "块({r},{c}) recv v{} e{} / send v{} e{}",
                recv.rx.version[i], recv.rx.epoch[i], send.ctl.version[i], send.ctl.epoch[i]
            )
        })
        .collect();
    assert_eq!(
        diff,
        Vec::<(usize, usize)>::new(),
        "报告要回来的条带内容不对：落定 {rounds} 轮后仍差 {n} 块，行={rows:?} 列={cols:?}（顶行=0，底行窄块从 y=320 起），\
         累计要回 {recovered} 块、滚动宣告 {scrolls} 次，throttled={} escalations={} report_resends={} resends={} scrolls={}\n\
         差异块是否全部「版本已对齐」={stale_but_acknowledged}；样例：{audit:?}",
        send.ctl.stats.repair_throttled,
        send.ctl.stats.escalations,
        send.ctl.stats.report_resends,
        send.ctl.stats.resends,
        send.ctl.stats.scrolls,
    );
}

/// 反向护栏：这条修复唯一的代价面是"把健康的滚动组误判成烂掉"，那样每步都要白重取一批块。
/// 所以全量投递 + 每一步都叫时钟，逐步断言两件事：画布逐字节等于当时那一帧，且发送端
/// `report_resends` 一直是 0（一次块都没被报告要过 ⇒ 时钟没造出任何缺口）。
/// 为什么 `report_resends == 0` 就够了：组若被误判，缺的条带块一定留在版本后面，下一份报告
/// 必然要它们 ⇒ 计数必然非零。
#[test]
fn a_healthy_scroll_pays_nothing_for_the_clock() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let codecs = CodecSet { codecs: &[&pal, &png] };
    let f0 = scroll_page(W, H, 0);
    let mut send = SendPipeline::new(f0.clone(), codecs);
    send.motion_enabled = false;
    let mut recv = RecvPipeline::new(codecs);
    feed(&mut recv, send.on_frame(&f0).unwrap());

    for i in 1..30u32 {
        let f = scroll_page(W, H, i.min(CONTENT_STEPS));
        feed(&mut recv, send.on_frame(&f).unwrap_or_default());
        recv.tick(i as u64 * 500);
        assert_eq!(
            diff_tiles(&recv, &f),
            Vec::<(usize, usize)>::new(),
            "第 {i} 步画布与当时那一帧不一致（投递是全的，只有时钟在跑）"
        );
        if i % 3 == 0 {
            let Message::Report { layout_id, versions, epochs } = recv.report() else {
                panic!("报告形状变了");
            };
            let reply = send.on_report(layout_id, &versions, &epochs, i as u64 * 500).unwrap_or_default();
            assert_eq!(
                reply.iter().filter(|m| matches!(m, WireMsg::Update { .. })).count(),
                0,
                "第 {i} 步一份健康的滚动被时钟判出了缺口：报告要回了内容块"
            );
            feed(&mut recv, reply);
        }
    }
    assert_eq!(
        send.ctl.stats.report_resends, 0,
        "健康滚动 + 每步时钟，累计却要回了 {} 个块 —— 超时把没坏的组判死了",
        send.ctl.stats.report_resends
    );
}
