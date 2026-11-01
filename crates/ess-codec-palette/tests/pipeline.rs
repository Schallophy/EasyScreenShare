//! 端到端管线测试：真实像素 → 差分 → 编码 → 解码 → 画布重建。
//!
//! 覆盖：整帧图块往返逐字节相等、滚动检测 + 应用后的重建、差分范围、
//! §7.2 粒度选择、§9 守门员阶梯与迟滞、fps 下限。

use ess_codec_palette::PaletteZstdCodec;
use ess_core::codec::TileCodec;
use ess_core::frame::{extract_tile, tile_hash, Frame, SendState, TileGrid};
use ess_core::governor::{ContentClass, Governor, Obs};
use ess_core::model::TileId;
use ess_core::policy::{choose, Choice, PolicyConsts};
use ess_core::scroll::{apply_vscroll, detect};
use ess_core::{SUBTILE, TILE};

const W: u32 = 640;
const H: u32 = 480;

/// 文字页风格合成帧：16 子块为单位的二值图案，颜色数 ≤ 8。
/// `base` 控制内容分布，`seed` 让每行指纹互异。
fn text_page(base: u32, seed: u32) -> Frame {
    let mut f = Frame::new(W, H, 0);
    let colors = [0xff00_0000u32, 0xffff_ffff, 0xff20_2020, 0xffe0_e0e0, 0xff00_66cc];
    for y in 0..H {
        for x in 0..W {
            let sx = x / SUBTILE;
            let sy = y / SUBTILE;
            // 行纹：每 16 px 行一条"文字行"，用 (base, seed, sy) 决定颜色
            let line = sy.wrapping_add(base);
            let c = colors[((line ^ (line >> 3) ^ seed) as usize) % colors.len()];
            let px = if ((x / 2) ^ y) % 7 == 0 { colors[1] } else { c };
            let s = y as usize * f.stride + x as usize * 4;
            f.px[s..s + 4].copy_from_slice(&px.to_le_bytes());
        }
    }
    f
}

fn fill_tile(frame: &mut Frame, g: ess_core::frame::TileGeom, color: u32) {
    for y in g.y..g.y + g.h {
        for x in g.x..g.x + g.w {
            let s = y as usize * frame.stride + x as usize * 4;
            frame.px[s..s + 4].copy_from_slice(&color.to_le_bytes());
        }
    }
}

#[test]
fn full_frame_roundtrip_is_byte_identical() {
    let codec = PaletteZstdCodec::default();
    let f = text_page(0, 7);
    let grid = TileGrid::for_frame(W, H);
    let mut canvas = Frame::new(W, H, 0);
    for i in 0..grid.tile_count() {
        let id = TileId(i as u16);
        let g = grid.geom(id, W, H);
        let raw = extract_tile(&f, g);
        let enc = codec.encode(&raw, g.w, g.h).expect("encode");
        // 有损/无损语义：解码到画布对应位置（stride 对齐）
        let mut tmp = vec![0u8; g.w as usize * g.h as usize * 4];
        codec.decode_into(&enc, &mut tmp, g.w * 4).expect("decode");
        // 贴回画布
        for y in 0..g.h as usize {
            let ds = (g.y as usize + y) * canvas.stride + g.x as usize * 4;
            let ss = y * g.w as usize * 4;
            canvas.px[ds..ds + g.w as usize * 4]
                .copy_from_slice(&tmp[ss..ss + g.w as usize * 4]);
        }
    }
    assert_eq!(canvas.px, f.px, "canvas must equal source frame");
}

#[test]
fn scroll_detection_and_rebuild() {
    let codec = PaletteZstdCodec::default();
    let grid = TileGrid::for_frame(W, H);

    // A：页面 1；B：A 向上滚 37 px，底部露出新的 37 行
    let a = text_page(0, 3);
    let dy = 37u32;
    let mut b = Frame::new(W, H, 0);
    for y in 0..H - dy {
        let s_src = (y + dy) as usize * a.stride;
        let s_dst = y as usize * b.stride;
        b.px[s_dst..s_dst + W as usize * 4]
            .copy_from_slice(&a.px[s_src..s_src + W as usize * 4]);
    }
    let page2 = text_page(9, 11);
    for y in H - dy..H {
        let s_src = y as usize * page2.stride;
        let s_dst = y as usize * b.stride;
        b.px[s_dst..s_dst + W as usize * 4]
            .copy_from_slice(&page2.px[s_src..s_src + W as usize * 4]);
    }

    // 检测
    let det = detect(&a, &b).expect("scroll must be detected");
    assert_eq!(det.dy, dy as i32, "detected dy");
    assert!(det.match_ratio > 0.95);

    // 差分：接收端先应用滚动，故基准是"平移后的旧帧"；只有底部条带的块变化
    let shifted = a.vscroll(det.dy);
    let state = SendState::new(shifted.clone());
    let diff = state.diff(&b, grid);
    assert!(!diff.changed.is_empty());
    let tile_rows_changed: std::collections::BTreeSet<u32> = diff
        .changed
        .iter()
        .map(|(id, _)| id.0 as u32 / grid.cols as u32)
        .collect();
    let first_changed_row = (H - dy) / TILE as u32;
    for r in &tile_rows_changed {
        assert!(
            *r >= first_changed_row,
            "tile row {r} changed above scroll band; changed ids: {:?}",
            diff.changed.iter().map(|(id, _)| id.0).collect::<Vec<_>>()
        );
    }
    // 与其直接比原帧相比，平移后的差分应显著更小
    let naive = SendState::new(a.clone()).diff(&b, grid);
    assert!(
        diff.changed.len() < naive.changed.len(),
        "scroll diff ({}) should be smaller than naive diff ({})",
        diff.changed.len(),
        naive.changed.len()
    );

    // 接收端重建：画布 = A，先滚，再用差分块覆盖
    let mut canvas = shifted.clone();
    for (id, _) in &diff.changed {
        let g = grid.geom(*id, W, H);
        let raw = extract_tile(&b, g);
        let enc = codec.encode(&raw, g.w, g.h).unwrap();
        let mut tmp = vec![0u8; g.w as usize * g.h as usize * 4];
        codec.decode_into(&enc, &mut tmp, g.w * 4).unwrap();
        for y in 0..g.h as usize {
            let ds = (g.y as usize + y) * canvas.stride + g.x as usize * 4;
            let ss = y * g.w as usize * 4;
            canvas.px[ds..ds + g.w as usize * 4]
                .copy_from_slice(&tmp[ss..ss + g.w as usize * 4]);
        }
    }
    assert_eq!(canvas.px, b.px, "rebuilt canvas must equal frame B");
}

#[test]
fn small_scroll_band_not_misdetected() {
    // 只改一条 16 px 高的横带：不应判为滚动
    let a = text_page(0, 5);
    let mut b = a.clone();
    for y in 200..216 {
        for x in 0..W {
            let s = y as usize * b.stride + x as usize * 4;
            b.px[s..s + 4].copy_from_slice(&0xffff_0000u32.to_le_bytes());
        }
    }
    assert!(detect(&a, &b).is_none(), "single band edit is not a scroll");
}

#[test]
fn tile_hash_stable_and_sensitive() {
    let a = text_page(0, 1);
    let g = TileGrid::for_frame(W, H).geom(TileId(5), W, H);
    let h1 = tile_hash(&extract_tile(&a, g));
    let h2 = tile_hash(&extract_tile(&a, g));
    assert_eq!(h1, h2);
    let mut changed = a.clone();
    fill_tile(&mut changed, g, 0xff00_00ff);
    assert_ne!(h1, tile_hash(&extract_tile(&changed, g)));
}

#[test]
fn policy_two_level_choice() {
    let c = PolicyConsts::default();
    assert_eq!(choose(1, &c), Choice::Sub16); // 1×110 < 380
    assert_eq!(choose(3, &c), Choice::Sub16); // 3×110 = 330 < 380
    assert_eq!(choose(4, &c), Choice::Whole64); // 440 ≥ 380
}

#[test]
fn governor_steps_down_then_up_with_hysteresis() {
    let mut g = Governor::new(ContentClass::Text);
    assert_eq!(g.settings().fps, 15);

    let bad = Obs {
        encode_ms: 90.0,
        budget_ms: 33.0,
        queue_delay_ms: 0,
        qp: 10,
        qp_max: 40,
        bwe_bps: 1_000_000,
        needed_bps: 500_000,
    };
    // 第一次降：立即
    assert_eq!(g.tick(1000, &bad), Some(ess_core::governor::Step::Down));
    assert_eq!(g.settings().fps, 10);
    // 2 s 内不连降（迟滞）
    assert_eq!(g.tick(1500, &bad), None);
    assert_eq!(g.settings().fps, 10);
    // 2 s 后允许再次降
    assert_eq!(g.tick(3100, &bad), Some(ess_core::governor::Step::Down));
    assert_eq!(g.settings().fps, 5);

    // 带宽富余 30% 但不足 5 s：不升
    let good = Obs {
        encode_ms: 10.0,
        budget_ms: 33.0,
        queue_delay_ms: 5,
        qp: 10,
        qp_max: 40,
        bwe_bps: 2_000_000,
        needed_bps: 1_000_000,
    };
    assert_eq!(g.tick(4000, &good), None);
    assert_eq!(g.tick(6000, &good), None); // 富余已持续 2 s，不够 5 s
    assert_eq!(g.settings().fps, 5);
    // 持续到 5 s 后升一档
    assert_eq!(g.tick(9100, &good), Some(ess_core::governor::Step::Up));
    assert_eq!(g.settings().fps, 10);
}

#[test]
fn governor_motion_floor_keeps_fps_at_15() {
    let mut g = Governor::new(ContentClass::Text);
    g.motion = true; // 滚动中
    let bad = Obs {
        encode_ms: 90.0,
        budget_ms: 33.0,
        queue_delay_ms: 0,
        qp: 10,
        qp_max: 40,
        bwe_bps: 500_000,
        needed_bps: 900_000,
    };
    let _ = g.tick(1000, &bad);
    assert_eq!(g.settings().fps, 15, "motion content must not drop below 15 fps");
}

#[test]
fn governor_hard_drop_ignores_hysteresis() {
    let mut g = Governor::new(ContentClass::Game);
    let normal = Obs {
        encode_ms: 5.0,
        budget_ms: 16.0,
        queue_delay_ms: 0,
        qp: 10,
        qp_max: 40,
        bwe_bps: 9_000_000,
        needed_bps: 8_000_000,
    };
    assert_eq!(g.tick(1000, &normal), None);
    let hard = Obs { queue_delay_ms: 300, ..normal };
    assert_eq!(g.tick(1100, &hard), Some(ess_core::governor::Step::Down));
    assert_eq!(g.settings().fps, 30);
}
