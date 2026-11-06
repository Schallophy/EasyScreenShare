//! 视频平面决策器与「平面交接」契约。
//!
//! 产品级验收要求交接不出现可见的回退/冻结，因此有两条硬契约：
//! 1. **顺序**：退出运动的修复批次必须排在 `VideoMode{off}` 之前发出。可靠通道
//!    有序，观看端把"收到 off"当作"画布已经正确"的信号，顺序错了就会看到画面
//!    退回运动前的一瞬。
//! 2. **精确**：视频平面期间观看端画布不被有损帧污染（发送端因此不发 JPEG 运动
//!    帧），所以退出只需补真正的差集，且补完后画布与真实画面**逐字节一致**。

use ess_core::codec::{RawCodec, TileCodec};
use ess_core::frame::Frame;
use ess_core::pipeline::{CodecSet, RecvPipeline, SendPipeline};
use ess_core::wire::WireMsg;

/// 与上一帧完全不相关的噪声：既不满足滚动匹配，也必然判为大面积运动。
fn moving(k: u32, w: u32, h: u32) -> Frame {
    let mut f = Frame::new(w, h, 0);
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

fn codecs() -> CodecSet<'static> {
    CodecSet { codecs: Box::leak(vec![&RawCodec as &dyn TileCodec].into_boxed_slice()) }
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

fn sender(video_plane: bool) -> SendPipeline<'static> {
    let mut p = SendPipeline::new(moving(0, 128, 128), codecs());
    p.video_plane = video_plane;
    // 本文件其余用例锁的是**交接协议**（宣告顺序、修复精确性）。预热会把"决定要交"
    // 和"宣告交"隔开，那是另一件事，由文件末尾 `warm_up_*` 那组单独锁；这里把预热
    // 关掉，让协议用例继续只看交接本身。
    p.set_warm_max_us(0);
    p
}

/// 预热那组用例要按时间戳推进 deadline，而 [`moving`] 的 ts 恒为 0（永远推不动），
/// 所以给它一个带时间戳的版本。
fn moving_at(k: u32, ts_us: u64) -> Frame {
    let mut f = moving(k, 128, 128);
    f.ts_us = ts_us;
    f
}

fn canvas_equal(a: &Frame, b: &Frame) -> bool {
    if a.w != b.w || a.h != b.h {
        return false;
    }
    for y in 0..a.h as usize {
        for x in 0..(a.w as usize * 4) {
            if a.px[y * a.stride + x] != b.px[y * b.stride + x] {
                return false;
            }
        }
    }
    true
}

/// 把一批消息喂给接收端，返回其消息种类。
fn pump(rx: &mut RecvPipeline, msgs: &[WireMsg]) {
    for m in msgs {
        rx.on_wire(m).unwrap();
    }
}

#[test]
fn video_plane_entry_announces_once_and_parks_the_jpeg_path() {
    let mut p = sender(true);
    assert_eq!(kinds(&p.on_frame(&moving(0, 128, 128)).unwrap()), vec!["full"]);

    let mut on = 0;
    let mut jpeg = 0;
    let mut entry = 0;
    for k in 1..10 {
        let f = moving(k, 128, 128);
        let ks = kinds(&p.on_frame(&f).unwrap());
        on += ks.iter().filter(|s| **s == "video-on").count();
        jpeg += ks.iter().filter(|s| **s == "motion").count();
        if ks.contains(&"video-on") {
            entry = k;
            assert_eq!(ks, vec!["video-on"], "进入运动只宣告，不发有损帧");
        }
    }
    assert_eq!(entry, 2, "整帧后第一帧只用来填探针，第二帧才判为运动");
    assert_eq!(on, 1, "进入宣告只能有一次");
    assert_eq!(jpeg, 0, "视频平面期间画布不能被有损帧污染");
    assert!(p.video_mode());
}

#[test]
fn jpeg_path_still_works_without_a_video_plane() {
    let mut p = sender(false);
    p.on_frame(&moving(0, 128, 128)).unwrap();
    let mut jpeg = 0;
    for k in 1..8 {
        let ks = kinds(&p.on_frame(&moving(k, 128, 128)).unwrap());
        jpeg += ks.iter().filter(|s| **s == "motion").count();
    }
    assert!(jpeg >= 5, "没有视频轨时 JPEG 运动帧仍是主通路: {jpeg}");
    assert!(!p.video_mode(), "没有视频轨就不该宣告平面切换");
}

#[test]
fn exit_repairs_with_a_diff_and_then_announces_off() {
    let mut p = sender(true);
    p.on_frame(&moving(0, 128, 128)).unwrap();
    for k in 1..4 {
        p.on_frame(&moving(k, 128, 128)).unwrap();
    }
    assert!(p.video_mode());

    let last = moving(9, 128, 128);
    p.on_frame(&last).unwrap();
    let mut out = Vec::new();
    for _ in 0..20 {
        out = kinds(&p.on_frame(&last).unwrap());
        if !out.is_empty() {
            break;
        }
    }
    assert_eq!(out, vec!["repair", "video-off"], "必须先修复后关平面（顺序即契约）");
    assert!(!p.video_mode());
    assert_eq!(p.stats.video_off, 1);
    assert!(p.stats.repair_tiles > 0 && p.stats.repair_tiles <= 12 * 20);
}

#[test]
fn repair_restores_the_canvas_byte_exact() {
    // 端到端：运动一段 → 退出 → 观看端画布必须与真实画面逐字节相同
    let mut p = sender(true);
    let mut rx = RecvPipeline::new(codecs());
    rx.accept_video_plane = true;

    let f0 = moving(0, 128, 128);
    pump(&mut rx, &p.on_frame(&f0).unwrap());
    assert!(canvas_equal(&rx.canvas, &f0), "整帧后画布应精确");

    // 整帧后第一帧只用来填探针，仍走无损差分 → 观看端画布停在 f1，
    // 这一帧就是退出运动时的修复基线。
    let f1 = moving(1, 128, 128);
    pump(&mut rx, &p.on_frame(&f1).unwrap());
    assert!(canvas_equal(&rx.canvas, &f1));
    for k in 2..6 {
        pump(&mut rx, &p.on_frame(&moving(k, 128, 128)).unwrap());
    }
    assert!(rx.video_mode());
    assert_eq!(rx.stats.motion_frames, 0, "视频模式不该解码任何有损帧");
    assert!(canvas_equal(&rx.canvas, &f1), "视频平面期间画布停在最后一次无损画面");

    let last = moving(9, 128, 128);
    pump(&mut rx, &p.on_frame(&last).unwrap());
    for _ in 0..20 {
        let msgs = p.on_frame(&last).unwrap();
        let done = !msgs.is_empty();
        pump(&mut rx, &msgs);
        if done {
            break;
        }
    }
    assert!(!rx.video_mode(), "off 必须已到达");
    assert!(canvas_equal(&rx.canvas, &last), "修复后画布与真实画面必须逐字节一致");
}

#[test]
fn empty_diff_exits_without_sending_pixels() {
    // 画面绕一圈回到基线（滚动回去、窗口关掉）：退出只需宣告，零像素。
    // 基线 = 进入运动前最后一次无损送达的帧（这里是 f1）。
    let mut p = sender(true);
    let f0 = moving(0, 128, 128);
    p.on_frame(&f0).unwrap();
    let f1 = moving(1, 128, 128);
    p.on_frame(&f1).unwrap();
    for k in 2..6 {
        p.on_frame(&moving(k, 128, 128)).unwrap();
    }
    let mut out = Vec::new();
    for _ in 0..20 {
        out = kinds(&p.on_frame(&f1).unwrap());
        if !out.is_empty() {
            break;
        }
    }
    assert_eq!(out, vec!["video-off"], "回到基线时不该发任何像素");
}

#[test]
fn mid_burst_full_falls_back_to_a_full_exit() {
    // 运动中途给迟到者补过整帧 → 各观看端基线不再唯一 → 退出必须用整帧
    let mut p = sender(true);
    p.on_frame(&moving(0, 128, 128)).unwrap();
    for k in 1..4 {
        p.on_frame(&moving(k, 128, 128)).unwrap();
    }
    p.force_full();
    assert_eq!(kinds(&p.on_frame(&moving(4, 128, 128)).unwrap()), vec!["full", "video-on"]);

    let last = moving(4, 128, 128);
    let mut out = Vec::new();
    for _ in 0..20 {
        out = kinds(&p.on_frame(&last).unwrap());
        if !out.is_empty() {
            break;
        }
    }
    assert_eq!(out, vec!["full", "video-off"], "基线不唯一时退回整帧修复");
}

#[test]
fn forced_full_during_motion_does_not_strand_the_plane() {
    let mut p = sender(true);
    p.on_frame(&moving(0, 128, 128)).unwrap();
    for k in 1..4 {
        p.on_frame(&moving(k, 128, 128)).unwrap();
    }
    assert!(p.video_mode());
    p.force_full();
    let last = moving(5, 128, 128);
    assert_eq!(kinds(&p.on_frame(&last).unwrap()), vec!["full", "video-on"]);
    let mut off = false;
    for _ in 0..20 {
        if kinds(&p.on_frame(&last).unwrap()).contains(&"video-off") {
            off = true;
            break;
        }
    }
    assert!(off, "补帧后静止必须关掉视频平面");
    assert!(!p.video_mode());
}

#[test]
fn receiver_with_video_plane_ignores_stray_jpeg() {
    // 兜底：万一有 JPEG 到达（例如观看端中途才订阅上视频轨），有视频轨的一端
    // 不把它贴到画布上，否则会留下永久模糊的补丁。
    let f = moving(2, 128, 128);
    let (_w, _h, jpeg) = ess_core::motion::encode(&f, 64, 64, 50).unwrap();
    let motion = WireMsg::Motion { seq: 9, layout_id: 1, frame_id: 9, w: 64, h: 64, data: jpeg };
    let full = {
        let mut p = sender(false);
        p.on_frame(&moving(0, 128, 128)).unwrap().remove(0)
    };

    for accept in [true, false] {
        let mut rx = RecvPipeline::new(codecs());
        rx.accept_video_plane = accept;
        pump(&mut rx, std::slice::from_ref(&full));
        rx.on_wire(&WireMsg::VideoMode { seq: 8, layout_id: 1, on: true }).unwrap();
        let before = rx.stats.motion_frames;
        pump(&mut rx, std::slice::from_ref(&motion));
        if accept {
            assert_eq!(rx.stats.motion_frames, before, "有视频轨时跳过有损帧");
            assert!(rx.stats.motion_skipped >= 1);
        } else {
            assert_eq!(rx.stats.motion_frames, before + 1, "无视频轨时兜底帧要落画布");
        }
    }
}

#[test]
fn i420_planes_match_bt601_limits() {
    let mut black = Frame::new(64, 64, 0);
    for o in (0..black.px.len()).step_by(4) {
        black.px[o + 3] = 255;
    }
    let (w, h, y, u, v) = ess_core::motion::to_i420(&black, 32, 32);
    assert_eq!((w, h), (32, 32));
    assert_eq!(y.len(), (w * h) as usize);
    let nc = (((w + 1) / 2) * ((h + 1) / 2)) as usize;
    assert_eq!(u.len(), nc);
    assert_eq!(v.len(), nc);
    assert!(y.iter().all(|&p| p == 16), "黑场 Y 应为 16（限制范围）");
    assert!(u.iter().chain(v.iter()).all(|&p| p == 128));

    let mut white = Frame::new(64, 64, 0);
    for o in (0..white.px.len()).step_by(4) {
        white.px[o] = 255;
        white.px[o + 1] = 255;
        white.px[o + 2] = 255;
        white.px[o + 3] = 255;
    }
    let (_, _, wy, wu, _) = ess_core::motion::to_i420(&white, 64, 64);
    assert!(wy.iter().all(|&p| (p as i32 - 235).abs() <= 1));
    assert!(wu.iter().all(|&p| (p as i32 - 128).abs() <= 2));
}

#[test]
fn fused_i420_matches_reference() {
    let f = moving(3, 320, 200);
    let (w, h, y, u, v) = ess_core::motion::to_i420(&f, 160, 100);
    let (rw, rh, rgb) = ess_core::motion::to_rgb(&f, 160, 100);
    let (ry, ru, rv) = ess_core::motion::rgb_to_i420(&rgb, rw, rh);
    assert_eq!((w, h), (rw, rh));
    for (a, b) in y.iter().zip(&ry) {
        assert!(a.abs_diff(*b) <= 1, "Y 偏差 {a} vs {b}");
    }
    for (a, b) in u.iter().zip(&ru).chain(v.iter().zip(&rv)) {
        assert!(a.abs_diff(*b) <= 2, "色度偏差 {a} vs {b}");
    }
}

#[test]
fn video_dims_are_even_and_fit_the_cap() {
    let (w, h, _) = ess_core::motion::to_rgb(&moving(0, 1920, 1080), 1280, 720);
    assert_eq!((w, h), (1280, 720));
    assert_eq!(w % 2, 0);
    assert_eq!(h % 2, 0);
    let (w, h, _) = ess_core::motion::to_rgb(&moving(0, 1366, 768), 1280, 720);
    assert_eq!((w, h), (1280, 718), "等比缩放取两轴较小比例");
}

/// 换平面的判据是"无损装不下这条链路"，不是"画面大面积变了"。同一段全画面重绘，
/// 只改链路容量这一个变量：宽链路必须留在无损（30 fps + 逐像素精确），窄链路才许
/// 切到媒体通道。两条必须一起跑——少了反向对照，前一条可能只是碰巧绿。
#[test]
fn wide_link_keeps_large_motion_on_the_lossless_plane() {
    let mut p = sender(true);
    p.set_link_bps(100_000_000, 30);
    p.on_frame(&moving(0, 128, 128)).unwrap();
    for k in 1..5u32 {
        let mut f = moving(k, 128, 128);
        f.ts_us = k as u64 * 33_000;
        let ks = kinds(&p.on_frame(&f).unwrap());
        assert!(!ks.contains(&"video-on"), "100 Mbps 装得下无损更新，却还是切了平面: {ks:?}");
    }
    assert!(!p.video_mode());
    assert!(p.stats.tiles_sent > 0, "留在无损却一个块都没发，等于画面不更新");
}

#[test]
fn narrow_link_still_switches_on_large_motion() {
    let mut p = sender(true);
    p.set_link_bps(2_000_000, 30);
    p.on_frame(&moving(0, 128, 128)).unwrap();
    let mut switched = false;
    for k in 1..6u32 {
        let mut f = moving(k, 128, 128);
        f.ts_us = k as u64 * 33_000;
        switched |= kinds(&p.on_frame(&f).unwrap()).contains(&"video-on");
    }
    assert!(switched, "链路只有 2 Mbps，大面积重绘还不切平面 → 无损块只会堆成排队");
    assert!(p.video_mode());
}

/// 流畅优先档（§13.1）：帧率是合同，面积够大就切媒体通道，**不问**无损装不装得下。
/// 与 `wide_link_keeps_large_motion_on_the_lossless_plane` 成对——同一素材同一链路，
/// 差别只有 `smooth_motion`。少了这一条，档位就只是界面文案。
#[test]
fn smooth_mode_switches_even_on_a_wide_link() {
    let mut p = sender(true);
    p.smooth_motion = true;
    p.set_link_bps(100_000_000, 30);
    p.on_frame(&moving(0, 128, 128)).unwrap();
    let mut switched = false;
    for k in 1..5u32 {
        let mut f = moving(k, 128, 128);
        f.ts_us = k as u64 * 33_000;
        switched |= kinds(&p.on_frame(&f).unwrap()).contains(&"video-on");
    }
    assert!(switched, "流畅优先档在宽链路上还不切平面 → 大面积重绘仍由无损块扛，帧率合同没人守");
    assert!(p.video_mode());
}

// ---- 预热：把"决定要交"和"宣告交"隔开 ----
//
// 交接那一帧要付一个关键帧，1080p 平衡档实测那一次 pacing 吃掉 283–492 ms（观看端一个
// 看得见的定格），编码器从静止节拍爬到声明帧率还要一两秒（观看端第 1→31 帧实测
// 1428–1487 ms，直尺 990 ms）。这两笔账消不掉，但可以让它们发生在观众看不见的时候：
// 无损通路本来就在画这一帧，再顶它一两秒不改一个像素。

fn warm_sender() -> SendPipeline<'static> {
    let mut p = SendPipeline::new(moving_at(0, 0), codecs());
    p.video_plane = true;
    p.set_warm_max_us(3_000_000);
    p
}

/// 推进到"决策要交出去"的那一帧（k=1 只填探针，k=2 才判为大面积运动）。
fn drive_to_entry(p: &mut SendPipeline, ts_step: u64) -> Vec<WireMsg> {
    p.on_frame(&moving_at(0, 0)).unwrap();
    p.on_frame(&moving_at(1, ts_step)).unwrap();
    p.on_frame(&moving_at(2, ts_step * 2)).unwrap()
}

#[test]
fn warm_up_holds_the_picture_on_the_lossless_plane() {
    let mut p = warm_sender();
    let out = drive_to_entry(&mut p, 33_000);
    let ks = kinds(&out);
    assert!(!ks.contains(&"video-on"), "决定要交的那一刻不该马上交：{:?}", ks);
    assert!(ks.contains(&"update"), "预热期画面仍由无损差分出：{:?}", ks);
    assert!(!ks.contains(&"motion"), "预热期不发 JPEG 有损帧：{:?}", ks);
    assert!(p.media_pending() && !p.video_mode());
    assert!(p.media_feeding(), "预热期必须按满帧率喂编码器，否则等的东西永远不会到");

    // 一个达标窗还不够（WebRTC 的尺寸自适应约 3 s 才走一步，那一步也要付关键帧）：
    // 连续两个 1 s 窗达标才交。
    p.note_media_delivery(30.0, 30, 960, 540);
    let ks = kinds(&p.on_frame(&moving_at(3, 132_000)).unwrap());
    assert!(!ks.contains(&"video-on"), "只达标一次就交，会撞上后面那次尺寸调整：{:?}", ks);
    p.note_media_delivery(30.0, 30, 960, 540);
    let ks = kinds(&p.on_frame(&moving_at(4, 165_000)).unwrap());
    assert!(ks.contains(&"video-on"), "连续达标后还不交就是白顶无损：{:?}", ks);
    assert!(p.video_mode() && !p.media_pending());
}

#[test]
fn warm_up_deadline_hands_over_without_delivery_evidence() {
    let mut p = warm_sender();
    drive_to_entry(&mut p, 33_000);
    assert!(p.media_pending(), "前 3s 无交付证据时应该仍在预热");
    p.note_media_delivery(4.0, 30, 960, 540); // 一直不达标
    let ks = kinds(&p.on_frame(&moving_at(3, 2_100_000)).unwrap());
    assert!(!ks.contains(&"video-on"), "2.1s 还没到上限，不该提前交");
    let ks = kinds(&p.on_frame(&moving_at(4, 3_600_000)).unwrap());
    assert!(ks.contains(&"video-on"), "顶到上限就必须交：窄管子里无损通路撑不住（{:?}）", ks);

    // 交付没达标的中间时刻不该提前交
    let mut q = warm_sender();
    drive_to_entry(&mut q, 33_000);
    q.note_media_delivery(10.0, 30, 960, 540);
    q.note_media_delivery(10.0, 30, 960, 540);
    let ks = kinds(&q.on_frame(&moving_at(3, 1_000_000)).unwrap());
    assert!(!ks.contains(&"video-on"), "20fps 的交付（10/30）不该被当成达标");
}

#[test]
fn warm_up_cancels_only_after_the_same_hysteresis_as_a_plane_exit() {
    let mut p = warm_sender();
    drive_to_entry(&mut p, 33_000);
    assert!(p.media_pending());
    // 一两帧没到阈值不算"内容静止"：块状跳变动物的 ratio 本来就在阈值上下抖。上一版在
    // `!want_plane` 那一帧就静默取消，于是预热反复从头计时、上限永远到不了，切换沿验收臂
    // 整整 18 秒一次都没交出去（switches=0 / vmode=false）。取消与退出平面共用迟滞。
    let mut ks = Vec::new();
    // hold_frames 默认 10，多给 2 帧确保数完
    for i in 0..12u64 {
        let out = p.on_frame(&moving_at(2, 99_000 + i * 33_000)).unwrap();
        ks.extend(kinds(&out));
    }
    assert!(!p.media_pending(), "内容真静止了迟滞就该数完，预热该收");
    assert!(!p.motion_active());
    assert!(!ks.contains(&"video-off") && !ks.contains(&"full"), "取消预热不该在线上留东西：{:?}", ks);
    assert!(!ks.contains(&"video-on"), "没交付证据就不该交出去：{:?}", ks);
}

#[test]
fn budget_forced_entry_does_not_warm_up() {
    // 链路预算装不下无损块时，多顶一秒无损就是多堵一秒：这条路立刻交接。
    let mut p = warm_sender();
    p.set_budget_bps(1);
    p.on_frame(&moving_at(0, 0)).unwrap();
    let mut pending_seen = false;
    let mut announced = false;
    for k in 1..4u32 {
        let ks = kinds(&p.on_frame(&moving_at(k, k as u64 * 33_000)).unwrap());
        pending_seen |= p.media_pending();
        announced |= ks.contains(&"video-on");
    }
    assert!(announced, "预算逼出来的平面一次都没交接：预热把它挡在门外了");
    assert!(!pending_seen, "预算逼进来的平面不该预热");
}

/// 一块一色的 128x128（2x2 块），用来精确控制"变了几块"。
fn quad(ts_us: u64, colors: [u32; 4]) -> Frame {
    let mut f = Frame::new(128, 128, ts_us);
    for (i, &c) in colors.iter().enumerate() {
        let (cx, cy) = ((i % 2) as u32, (i / 2) as u32);
        for y in cy * 64..(cy + 1) * 64 {
            for x in cx * 64..(cx + 1) * 64 {
                let o = y as usize * f.stride + x as usize * 4;
                f.px[o..o + 4].copy_from_slice(&c.to_le_bytes());
            }
        }
    }
    f
}

/// 退出平面的修复基线必须取**交接那一刻**观看端手里的画面，而不是"决定要交"那一刻的。
///
/// 预热期（已决定交、还没真交）画面仍由无损差分出：那几帧的更新会照常发出去，观看端
/// 画布因此往前走，而 `repair_baseline` 还停在上限之前那次快照上。退出时按这份过期基线
/// 做差分，凡是"中途变过、又变回原样"的块都会被判成"没变"——那块就永远停在预热期的
/// 中途内容上，而且两端版本账一致（发送端只在真正发出时推进版本），报告看不出缺口。
///
/// 真机现场（`scripts/tile-idle-cut.sh`，1280x720）：底部一块停在动画第 5 帧的内容上，
/// 终帧 MAE 0.128，`missing=0 behind=0`、`REPAIR` 只剩退出那一次。这里用"一块一色"把
/// 那次巧合（周期性纹理在基线与终帧恰好同色）做成确定性用例。
#[test]
fn warm_up_keeps_sending_so_the_exit_baseline_is_taken_at_handover() {
    const BASE: u32 = 0xff10_1010;
    const MID: u32 = 0xff30_3030;

    let mut p = SendPipeline::new(quad(0, [BASE; 4]), codecs());
    p.video_plane = true;
    p.set_warm_max_us(6_000_000);
    let mut rx = RecvPipeline::new(codecs());
    pump(&mut rx, &p.on_frame(&quad(33_000, [BASE; 4])).unwrap());
    // 探针要有一帧作基准（`motion_ratio` 第一次调用只填探针，判 0）
    pump(&mut rx, &p.on_frame(&quad(66_000, [BASE; 4])).unwrap());

    // 决定要交：四块全变，预热开始；这一帧仍由无损差分出画。
    pump(&mut rx, &p.on_frame(&quad(99_000, [MID; 4])).unwrap());
    assert!(p.media_pending() && !p.video_mode(), "这一帧应该还在预热");

    // 预热期里再变一块：同样走无损差分，观看端手里这块因此是"中途内容"。
    let out = p.on_frame(&quad(132_000, [MID, 0xff40_4040, MID, MID])).unwrap();
    assert!(kinds(&out).contains(&"update"), "预热期画面仍由无损差分出：{:?}", kinds(&out));
    pump(&mut rx, &out);

    // 交付达标两窗之后，下一帧仍是大面积变化 → 真正交接（这一帧只记账，不发给观看端）。
    p.note_media_delivery(30.0, 30, 96, 96);
    p.note_media_delivery(30.0, 30, 96, 96);
    let out = p.on_frame(&quad(165_000, [MID, 0xff50_5050, MID, MID])).unwrap();
    assert!(kinds(&out).contains(&"video-on"), "连续达标后应当交接：{:?}", kinds(&out));
    assert!(p.video_mode());

    // 画面回到 BASE：与"决定要交"那一帧的快照逐块相同，与观看端手里的中途内容不同。
    let back = quad(198_000, [BASE; 4]);
    pump(&mut rx, &p.on_frame(&back).unwrap());
    for i in 1..14u64 {
        pump(&mut rx, &p.on_frame(&quad(198_000 + i * 33_000, [BASE; 4])).unwrap());
    }
    assert!(!p.video_mode(), "画面静止后应当退回图块平面");
    assert_eq!(p.stats.video_off, 1);

    assert!(
        canvas_equal(&rx.canvas, &back),
        "退出平面后画布还停在预热期的中途内容上（块 1 应当是 {:08x}）",
        BASE
    );
}
