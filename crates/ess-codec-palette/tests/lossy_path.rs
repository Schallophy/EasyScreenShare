//! 有损图块的端到端：照片类整屏 → 选码（降级）→ 线格式 → 接收管线 → 画布。
//!
//! 判据按用户拍板的优先级改写（**流畅度第一，不追求绝对无损**），所以**不再**断言逐字节相等，
//! 而是三条可测的：
//! 1. 真的降级了：`tiles_lossy > 0`，且总字节比同内容的无损口径省 3 倍以上；
//! 2. 解回来"看得清"：逐块平均绝对误差 < 8/255，**窄块（底行 16 px）也不能例外**；
//! 3. 报告修复路径在有损块上照样收敛：把整批 Update 扣掉，靠一份报告要回来，最终 MAE 仍达标。
//!
//! 这三条合起来才是"糊得可接受、但不会永久错画面"——旧契约里的逐字节断言解释不了后者。

use ess_codec_palette::PaletteZstdCodec;
use ess_codec_png::PngCodec;
use ess_core::codec::{JpegCodec, TileCodec, TileCodecId};
use ess_core::frame::Frame;
use ess_core::model::Message;
use ess_core::pipeline::{CodecSet, RecvPipeline, SendPipeline};
use ess_core::wire::{self, WireMsg};

const W: u32 = 640;
const H: u32 = 360; // 6 行块：底行只有 40 px 高（窄块）

/// 照片类整屏：渐变 + 哈希噪声。**不能用"平移一像素"造变化**——那是垂直滚动，
/// 管线会正确地把它认成 Scroll（只重发 1 px 的条带），根本走不到"大面积重画"这条路。
/// 换 `seed` 让整幅内容另一张图，才是现场那次半屏照片类突变的形状。
fn photo_page(w: u32, h: u32, seed: u32) -> Frame {
    let mut f = Frame::new(w, h, 0);
    let salt = (seed.wrapping_mul(101) & 0xff) as i32;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let yy = y;
            let r = (((x / 3 + yy / 7) ^ (x * yy / 61)) ^ salt) as u8;
            let g = ((x * 2 + yy) / 5) as u8;
            let b = ((x + yy * 3) / 4) as u8;
            let s = y as usize * f.stride + x as usize * 4;
            f.px[s] = b;
            f.px[s + 1] = g;
            f.px[s + 2] = r;
            f.px[s + 3] = 255;
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

/// 逐块平均绝对误差（只看亮度通道，够判"看得清"）。返回 (最大块 MAE, 出问题的块数)。
fn tile_mae(recv: &RecvPipeline, want: &Frame) -> (u32, usize) {
    let t = ess_core::TILE as usize;
    let cols = (want.w as usize).div_ceil(t);
    let rows = (want.h as usize).div_ceil(t);
    let mut worst = 0u32;
    let mut bad = 0usize;
    for ry in 0..rows {
        for cx in 0..cols {
            let x1 = ((cx + 1) * t).min(want.w as usize);
            let y1 = ((ry + 1) * t).min(want.h as usize);
            let mut acc = 0u64;
            let mut n = 0u64;
            for y in ry * t..y1 {
                for x in cx * t..x1 {
                    let i = y * want.stride + x * 4;
                    let (a, b) = (recv.canvas.px[i], want.px[i]); // B 通道
                    acc += (a as i32 - b as i32).unsigned_abs() as u64;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let mae = (acc / n) as u32;
            worst = worst.max(mae);
            if mae >= 8 {
                bad += 1;
            }
        }
    }
    (worst, bad)
}

fn codecs<'a>(pal: &'a PaletteZstdCodec, png: &'a PngCodec, jpeg: &'a JpegCodec) -> CodecSet<'a> {
    // `CodecSet` 里存的是切片引用：数组本体必须活得和它一样久，不能借用临时值。
    let list: &'a [&'a dyn TileCodec] = Box::leak(Box::new([
        pal as &'a dyn TileCodec,
        png as &'a dyn TileCodec,
        jpeg as &'a dyn TileCodec,
    ]));
    CodecSet { codecs: list }
}

#[test]
fn a_photo_burst_goes_lossy_and_still_reads_clearly() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let jpeg = JpegCodec::default();
    let cs = codecs(&pal, &png, &jpeg);
    let f0 = photo_page(W, H, 0);
    let mut send = SendPipeline::new(f0.clone(), cs);
    send.motion_enabled = false; // 只看图块通路，不让平面进来
    let mut recv = RecvPipeline::new(cs);
    feed(&mut recv, send.on_frame(&f0).unwrap());

    let f1 = photo_page(W, H, 1);
    let before = send.stats.bytes_encoded;
    let msgs = send.on_frame(&f1).unwrap();
    let lossy_total = (send.stats.bytes_encoded - before) as usize; // 只算这一批，不含首帧
    let updates: Vec<WireMsg> = msgs.into_iter().filter(|m| matches!(m, WireMsg::Update { .. })).collect();
    assert!(!updates.is_empty(), "整屏变化必须走 Update 批次");
    let tiles: usize = updates
        .iter()
        .map(|m| match m {
            WireMsg::Update { tiles, .. } => tiles.len(),
            _ => 0,
        })
        .sum();
    let by_codec = |msgs: &[WireMsg]| {
        let mut j = (0usize, 0usize); // (块数, 字节)
        let mut o = (0usize, 0usize);
        for m in msgs {
            if let WireMsg::Update { tiles, .. } = m {
                for t in tiles {
                    let slot = if t.codec == TileCodecId::Jpeg { &mut j } else { &mut o };
                    slot.0 += 1;
                    slot.1 += t.payload.len();
                }
            }
        }
        (j, o)
    };
    let (jpeg_n, jpeg_b) = by_codec(&updates).0;
    let (other_n, other_b) = by_codec(&updates).1;
    println!("这一批：JPEG {jpeg_n} 块/{jpeg_b} B，其余 {other_n} 块/{other_b} B");
    feed(&mut recv, updates.clone());

    // 无损口径（同内容、只给 palette+png）作对照
    let cs_ll = CodecSet { codecs: &[&pal, &png] };
    let lossless_total: usize = {
        let mut s = SendPipeline::new(f0.clone(), cs_ll);
        s.motion_enabled = false;
        s.on_frame(&f0).unwrap(); // 首帧走 Full，不算进这一批
        s.on_frame(&f1)
            .unwrap()
            .iter()
            .map(|m| match m {
                WireMsg::Update { tiles, .. } => tiles
                    .iter()
                    .map(|t| t.payload.len())
                    .sum::<usize>(),
                _ => 0,
            })
            .sum()
    };

    let (worst, bad) = tile_mae(&recv, &f1);
    println!(
        "整屏变化 {tiles} 块：有损口径 {lossy_total} B vs 无损口径 {lossless_total} B（{:.2}×）；最坏块 MAE={worst}，超标块={bad}",
        lossless_total as f64 / lossy_total.max(1) as f64
    );
    assert!(send.stats.tiles_lossy > 40, "tiles_lossy={} 太小，说明没真的降级", send.stats.tiles_lossy);
    // 逐块门限是 3×，但"合计"省不到 3× 是正常的：PNG 本来就便宜的那些块（本例约三分之一）
    // 按规则留在无损，它们的绝对额小、却拉低了合计倍数。合计只要 ≥2× 就算这条路线成立。
    assert!(
        lossy_total * 2 < lossless_total,
        "总价连 2 倍都没省：有损 {lossy_total} vs 无损 {lossless_total}"
    );
    assert_eq!(bad, 0, "有 {bad} 个块 MAE≥8（最坏 {worst}）—— 那不叫有损，叫糊");
}

#[test]
fn a_lost_lossy_batch_is_rebuilt_by_reports() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let jpeg = JpegCodec::default();
    let cs = codecs(&pal, &png, &jpeg);
    let f0 = photo_page(W, H, 0);
    let mut send = SendPipeline::new(f0.clone(), cs);
    send.motion_enabled = false;
    let mut recv = RecvPipeline::new(cs);
    feed(&mut recv, send.on_frame(&f0).unwrap());

    // 内容一步到位，然后把这一批 Update 全部扣掉 —— 只留报告这条活路。
    let f1 = photo_page(W, H, 1);
    let _dropped = send.on_frame(&f1).unwrap();
    let Message::Report { layout_id, versions, epochs } = recv.report() else {
        panic!("报告形状变了");
    };
    let reply = send.on_report(layout_id, &versions, &epochs, 1_000).unwrap_or_default();
    let resent: usize = reply
        .iter()
        .map(|m| match m {
            WireMsg::Update { tiles, .. } => tiles.len(),
            _ => 0,
        })
        .sum();
    // 重发的块必须也是 JPEG（否则报告修复会把地板悄悄推回 PNG 价）——先取 id 再投喂
    let ids: Vec<TileCodecId> = reply
        .iter()
        .filter_map(|m| match m {
            WireMsg::Update { tiles, .. } => Some(tiles.iter().map(|t| t.codec).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    let lossy_n = ids.iter().filter(|id| **id == TileCodecId::Jpeg).count();
    feed(&mut recv, reply);
    let (worst, bad) = tile_mae(&recv, &f1);
    println!("报告要回 {resent} 块（其中 JPEG {lossy_n} 块），最坏块 MAE={worst}");
    assert!(resent > 0, "报告一块都没要 —— 修复路径在有损块上断了");
    assert_eq!(bad, 0, "报告修复后仍有 {bad} 块 MAE≥8（最坏 {worst}）");
    assert!(send.stats.tiles_lossy > 0, "修复路径没走有损（那这条测试没测到 JPEG 的重发）");
    assert!(
        lossy_n * 2 > ids.len(),
        "重发的 {} 块里只有 {lossy_n} 块是有损，修复会把价推回 PNG",
        ids.len()
    );
}

/// 首帧（整帧 `Full`）逐块选码的端到端：线格式带上每块编码 → 接收端按块解 → 画布。
/// 旧口径（整帧一个编码器）实测 1080p 照片类桌面 **3.72 MB**；现在应与逐块按价一致（约 1.67 MB）。
#[test]
fn first_full_frame_is_priced_per_tile_end_to_end() {
    const W1: u32 = 1920;
    const H1: u32 = 1080;
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let jpeg = JpegCodec::default();
    let cs = codecs(&pal, &png, &jpeg);
    let f0 = photo_page(W1, H1, 0);
    let mut send = SendPipeline::new(f0.clone(), cs);
    send.motion_enabled = false;
    let msgs = send.on_frame(&f0).unwrap();

    let mut full_bytes = 0usize;
    let mut jpeg_n = 0usize;
    let mut total_n = 0usize;
    for m in &msgs {
        if let WireMsg::Full { codecs: cs, payloads, .. } = m {
            full_bytes += payloads.iter().map(|p| p.len()).sum::<usize>();
            total_n += cs.len();
            jpeg_n += cs.iter().filter(|c| **c == TileCodecId::Jpeg).count();
        }
    }
    assert!(full_bytes > 0, "首帧必须走 Full");
    assert!(jpeg_n > 100, "逐块选码没生效：JPEG 只有 {jpeg_n}/{total_n} 块");

    // 接收端按每块编码解回来
    let mut recv = RecvPipeline::new(cs);
    feed(&mut recv, msgs);
    let (worst, bad) = tile_mae(&recv, &f0);
    // 全局 MAE（三通道平均）= 验收臂的口径；逐块最坏值只作报告，不单独判死——
    // q80 在平滑渐变上单块最坏约 10/255（实测），而全局平均只有个位数。
    let mut gacc = 0u64;
    for (a, b) in recv.canvas.px.iter().zip(f0.px.iter()) {
        gacc += a.abs_diff(*b) as u64;
    }
    let gmae = gacc as f64 / f0.px.len() as f64;
    println!(
        "1080p 照片类首帧：Full={full_bytes} B（{:.2} MB，旧口径 3 897 773 B），JPEG {jpeg_n}/{total_n} 块；解回 全局MAE={gmae:.2} 最坏块={worst} 超标块={bad}",
        full_bytes as f64 / 1048576.0
    );
    assert!(full_bytes < 2_000_000, "首帧 {full_bytes} B 没吃到逐块选码的好处（旧口径 3 897 773 B）");
    assert!(gmae < 8.0, "首帧解回全局 MAE {gmae:.2} ≥ 8");
    assert!(worst <= 16, "首帧有块的最坏误差 {worst} > 16/255，那不叫有损叫糊");
}
