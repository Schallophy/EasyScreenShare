//! 端到端：真实像素 → 管线 → protobuf 线格式 → 接收管线 → 画布。
//!
//! 断言逐帧逐字节相等，覆盖：初始整帧、局部编辑、整数滚动（宣告+条带）、
//! 分辨率变化（整帧重建）、线格式往返。

use ess_codec_palette::PaletteZstdCodec;
use ess_codec_png::PngCodec;
use ess_core::frame::Frame;
use ess_core::pipeline::{CodecSet, RecvPipeline, SendPipeline};
use ess_core::wire::{self, WireMsg};
use ess_core::SUBTILE;

fn text_page(w: u32, h: u32, base: u32, seed: u32) -> Frame {
    let mut f = Frame::new(w, h, 0);
    let colors = [0xff00_0000u32, 0xffff_ffff, 0xff20_2020, 0xffe0_e0e0, 0xff00_66cc];
    for y in 0..h {
        for x in 0..w {
            let sx = x / SUBTILE;
            let sy = y / SUBTILE;
            let line = sy.wrapping_add(base) ^ (sx >> 2);
            let c = colors[((line ^ (line >> 3) ^ seed) as usize) % colors.len()];
            let px = if ((x / 2) ^ y) % 7 == 0 { colors[1] } else { c };
            let s = y as usize * f.stride + x as usize * 4;
            f.px[s..s + 4].copy_from_slice(&px.to_le_bytes());
        }
    }
    f
}

fn paint_rect(f: &mut Frame, x: u32, y: u32, w: u32, h: u32, color: u32) {
    for yy in y..(y + h).min(f.h) {
        for xx in x..(x + w).min(f.w) {
            let s = yy as usize * f.stride + xx as usize * 4;
            f.px[s..s + 4].copy_from_slice(&color.to_le_bytes());
        }
    }
}

/// 内容上移 dy，底部换新页。
fn scroll_frame(f: &mut Frame, dy: u32, fresh: &Frame) {
    let row_bytes = f.w as usize * 4;
    for y in 0..(f.h - dy) as usize {
        let src = (y + dy as usize) * f.stride;
        let dst = y * f.stride;
        f.px.copy_within(src..src + row_bytes, dst);
    }
    for y in (f.h - dy)..f.h {
        let src = y as usize * fresh.stride;
        let dst = y as usize * f.stride;
        f.px[dst..dst + row_bytes].copy_from_slice(&fresh.px[src..src + row_bytes]);
    }
}

fn feed(recv: &mut RecvPipeline, msgs: Vec<WireMsg>) {
    for m in &msgs {
        let bytes = wire::encode(m).expect("wire encode");
        let decoded = wire::decode(&bytes).expect("wire decode");
        assert_eq!(&decoded, m, "wire roundtrip must preserve message");
        recv.on_wire(&decoded).expect("recv");
    }
}

fn assert_frame_eq(canvas: &Frame, frame: &Frame, label: &str) {
    assert_eq!(canvas.w, frame.w, "{label}: width");
    assert_eq!(canvas.h, frame.h, "{label}: height");
    if canvas.px == frame.px {
        return;
    }
    let off = canvas
        .px
        .iter()
        .zip(frame.px.iter())
        .position(|(a, b)| a != b)
        .unwrap();
    let row = off / canvas.stride;
    let col = (off % canvas.stride) / 4;
    let n_diff = canvas.px.iter().zip(frame.px.iter()).filter(|(a, b)| a != b).count();
    panic!(
        "{label}: first mismatch at px({col},{row}) byte {off} ({} differing bytes of {})",
        n_diff,
        canvas.px.len()
    );
}

#[test]
fn e2e_screen_session_is_byte_identical() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let codecs = CodecSet { codecs: &[&pal, &png] };
    let mut frame = text_page(640, 480, 0, 3);
    let mut send = SendPipeline::new(frame.clone(), codecs);
    // 这些用例断言逐字节一致，显式关闭有损运动通路
    send.motion_enabled = false;
    let mut recv = RecvPipeline::new(codecs);

    // 1) 会话开始：整帧
    let msgs = send.on_frame(&frame).unwrap();
    assert!(matches!(msgs.first(), Some(WireMsg::Full { .. })));
    feed(&mut recv, msgs);
    assert_eq!(recv.canvas.px, frame.px, "initial full frame");

    // 2) 局部编辑 ×5
    for i in 0..5u32 {
        paint_rect(&mut frame, 20 + i * 60, 30 + i * 40, 48, 24, 0xffff_0000);
        let msgs = send.on_frame(&frame).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], WireMsg::Update { .. }));
        feed(&mut recv, msgs);
        assert_frame_eq(&recv.canvas, &frame, &format!("edit {i}"));
    }

    // 3) 滚动两连（37 px、又 90 px），每帧伴随条带新内容
    for (k, dy) in [37u32, 90].into_iter().enumerate() {
        let fresh = text_page(640, 480, 10 + k as u32, 21 + k as u32);
        scroll_frame(&mut frame, dy, &fresh);
        let msgs = send.on_frame(&frame).unwrap();
        assert_eq!(msgs.len(), 2, "scroll emits announce + update");
        assert!(matches!(msgs[0], WireMsg::Scroll { .. }));
        assert!(matches!(msgs[1], WireMsg::Update { .. }));
        feed(&mut recv, msgs);
        assert_frame_eq(&recv.canvas, &frame, &format!("scroll {dy}"));
    }
    assert_eq!(recv.stats.scrolls_applied, 2);

    // 4) 滚动 + 同帧编辑（编辑在滚动条带之外）
    let fresh = text_page(640, 480, 30, 41);
    scroll_frame(&mut frame, 52, &fresh);
    paint_rect(&mut frame, 100, 100, 32, 32, 0xff00_ff00);
    let msgs = send.on_frame(&frame).unwrap();
    feed(&mut recv, msgs);
    assert_eq!(recv.canvas.px, frame.px, "scroll plus edit");

    // 5) 分辨率变化：整帧重建
    let mut frame2 = text_page(800, 600, 5, 55);
    paint_rect(&mut frame2, 400, 300, 64, 64, 0xff00_00ff);
    let msgs = send.on_frame(&frame2).unwrap();
    assert!(matches!(msgs.first(), Some(WireMsg::Full { .. })));
    feed(&mut recv, msgs);
    assert_eq!(recv.canvas.w, 800);
    assert_frame_eq(&recv.canvas, &frame2, "layout change rebuilt");

    // 6) 变化后再编辑
    paint_rect(&mut frame2, 700, 520, 40, 40, 0xffff_ff00);
    let msgs = send.on_frame(&frame2).unwrap();
    feed(&mut recv, msgs);
    assert_frame_eq(&recv.canvas, &frame2, "edit after layout change");

    // 统计自洽
    assert!(send.stats.full_frames >= 2);
    assert!(send.stats.scrolls >= 2);
    assert_eq!(recv.stats.full_frames, send.stats.full_frames);
    assert!(recv.stats.applied_tiles > 0);
}

#[test]
fn e2e_scroll_sends_only_band_tiles() {
    // 滚动帧只发条带块：整屏 640×480 = 10×8 = 80 块，
    // 滚 37 px 只应编码底部 2 个 tile 行（≤20 块），而非 80 块。
    let pal = PaletteZstdCodec::default();
    let codecs = CodecSet { codecs: &[&pal] };
    let mut frame = text_page(640, 480, 0, 3);
    let mut send = SendPipeline::new(frame.clone(), codecs);
    // 这些用例断言逐字节一致，显式关闭有损运动通路
    send.motion_enabled = false;
    let _ = send.on_frame(&frame).unwrap();

    let fresh = text_page(640, 480, 7, 9);
    scroll_frame(&mut frame, 37, &fresh);
    let msgs = send.on_frame(&frame).unwrap();
    let mut band_tiles = 0u64;
    for m in &msgs {
        if let WireMsg::Update { tiles, .. } = m {
            band_tiles += tiles.len() as u64;
        }
    }
    assert!(band_tiles >= 10, "bottom band must carry tiles, got {band_tiles}");
    assert!(band_tiles <= 20, "only the exposed band should be sent, got {band_tiles} of 80");
}

#[test]
fn e2e_no_scroll_when_disabled() {
    let pal = PaletteZstdCodec::default();
    let codecs = CodecSet { codecs: &[&pal] };
    let mut frame = text_page(640, 480, 0, 3);
    let mut send = SendPipeline::new(frame.clone(), codecs);
    // 这些用例断言逐字节一致，显式关闭有损运动通路
    send.motion_enabled = false;
    send.scroll_enabled = false;
    let _ = send.on_frame(&frame).unwrap();

    let fresh = text_page(640, 480, 7, 9);
    scroll_frame(&mut frame, 37, &fresh);
    let msgs = send.on_frame(&frame).unwrap();
    assert_eq!(msgs.len(), 1, "disabled scroll → single tile update");
    if let WireMsg::Update { tiles, .. } = &msgs[0] {
        assert_eq!(tiles.len(), 80, "naive diff sends every tile");
    } else {
        panic!("expected update");
    }
}

#[test]
fn e2e_truecolor_tile_falls_back_to_png() {
    // 照片类块（>256 色）调色板编码器拒绝 → 自动降级 PNG
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let codecs = CodecSet { codecs: &[&pal, &png] };
    let mut frame = text_page(320, 192, 0, 3);
    let mut send = SendPipeline::new(frame.clone(), codecs);
    // 这些用例断言逐字节一致，显式关闭有损运动通路
    send.motion_enabled = false;
    let mut recv = RecvPipeline::new(codecs);
    let msgs = send.on_frame(&frame).unwrap();
    feed(&mut recv, msgs);

    // 在 (0,0) 块里画 4096 种不同颜色
    for y in 0..64u32 {
        for x in 0..64u32 {
            let c = (y * 64 + x) | 0xff00_0000;
            let s = y as usize * frame.stride + x as usize * 4;
            frame.px[s..s + 4].copy_from_slice(&c.to_le_bytes());
        }
    }
    let msgs = send.on_frame(&frame).unwrap();
    let mut saw_png = false;
    for m in &msgs {
        if let WireMsg::Update { tiles, .. } = m {
            saw_png |= tiles.iter().any(|t| t.codec == ess_core::codec::TileCodecId::Png);
        }
    }
    assert!(saw_png, "truecolor tile should use PNG fallback");
    feed(&mut recv, msgs);
    assert_frame_eq(&recv.canvas, &frame, "truecolor tile");
}
