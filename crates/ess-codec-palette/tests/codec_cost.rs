//! 选码判据的测量台：**不改行为**，只把三种内容形状下"每种编码到底花多少字节、多少时间"摊开。
//!
//! 为什么先跑这个：把"照片类块允许有损"落地之前，得知道
//! 1. 调色板那条免费的分界线（≤256 色）在真实形状上命中率如何；
//! 2. 有损相对 PNG 究竟省几倍——省得少的场合（照片背景上的字）必须留在无损；
//! 3. "满量程跳变计数"能不能把"含字"和"纯照片"分开（如果能，它就是第三层判据；不能就不用）。
//!
//! 跑法：`cargo test -p ess-codec-palette --test codec_cost -- --nocapture`
//!
//! 三种形状都取 64×64（= `TILE`），与真实图块同尺寸：
//! - `text`：代码页平涂 + 16 px 行纹 + 行噪点（调色板应当吃得下）；
//! - `photo`：渐变 + 轻微哈希噪声（`ESS_DEMO_STEP` 那半屏照片类的同款配方）；
//! - `text_on_photo`：同样的渐变背景上压一层硬边缘字块（**这格是判据的真正考题**）。

use ess_codec_palette::PaletteZstdCodec;
use ess_codec_png::PngCodec;
use ess_core::codec::TileCodec;
use ess_core::motion;
use ess_core::TILE;

const N: usize = TILE as usize;

/// 平涂代码页：颜色集合很小，调色板路径应该直接成功。
fn text_tile(base: usize) -> Vec<u8> {
    let colors = [
        0xff10_1010u32,
        0xffee_eeee,
        0xff20_2020,
        0xffe0_e0e0,
        0xff00_66cc,
    ];
    let mut px = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let sy = (y / 16) + base;
            let sx = x / 16;
            let line = (sy ^ (sx >> 2)) as usize;
            let c = colors[(line ^ (line >> 3)) % colors.len()];
            let v = if ((x / 2) ^ (sy * 16)) % 7 == 0 { colors[1] } else { c };
            let s = (y * N + x) * 4;
            px[s..s + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    px
}

/// 照片类：逐像素渐变 + 哈希噪声，与合成源 `step_at` 那次半屏变化同配方。
fn photo_tile(base: usize) -> Vec<u8> {
    let mut px = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let yy = y + base;
            let r = ((x / 3 + yy / 7) ^ (x * yy / 61)) as u8;
            let g = ((x * 2 + yy) / 5) as u8;
            let b = ((x + yy * 3) / 4) as u8;
            let s = (y * N + x) * 4;
            px[s] = b;
            px[s + 1] = g;
            px[s + 2] = r;
            px[s + 3] = 255;
        }
    }
    px
}

/// 照片背景上的字：渐变底 + 硬边白块（模拟笔画）。判据若只看"省几倍"，这类会被误降。
fn text_on_photo_tile(base: usize) -> Vec<u8> {
    let mut px = photo_tile(base);
    for y in 0..N {
        for x in 0..N {
            // 4 px 粗的两条横杠 + 一条竖杠：满量程跳变成对出现
            let glyph = (y % 21 < 4) || (x > N / 2 && x < N / 2 + 5);
            if glyph {
                let s = (y * N + x) * 4;
                px[s..s + 4].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
            }
        }
    }
    px
}

/// 满量程跳变计数：相邻像素亮度差 >128 的次数（横向 + 纵向）。
fn full_scale_edges(px: &[u8]) -> usize {
    let lum = |i: usize| -> u32 {
        (px[i] as u32 * 19595 + px[i + 1] as u32 * 38470 + px[i + 2] as u32 * 7471) >> 16
    };
    let mut hits = 0usize;
    for y in 0..N {
        for x in 0..N {
            let i = (y * N + x) * 4;
            if x + 1 < N {
                let j = (y * N + x + 1) * 4;
                if lum(i).abs_diff(lum(j)) > 128 {
                    hits += 1;
                }
            }
            if y + 1 < N {
                let j = ((y + 1) * N + x) * 4;
                if lum(i).abs_diff(lum(j)) > 128 {
                    hits += 1;
                }
            }
        }
    }
    hits
}

fn bgra_to_rgb(px: &[u8]) -> Vec<u8> {
    let mut rgb = vec![0u8; N * N * 3];
    for i in 0..N * N {
        rgb[i * 3] = px[i * 4 + 2];
        rgb[i * 3 + 1] = px[i * 4 + 1];
        rgb[i * 3 + 2] = px[i * 4];
    }
    rgb
}

/// 一格内容 × 三种编码的完整账单。
struct Cost {
    shape: &'static str,
    palette_ok: bool,
    palette_b: usize,
    png_b: usize,
    jpeg_b: usize,
    edges: usize,
    us_palette: u128,
    us_png: u128,
    us_jpeg: u128,
}

fn measure(shape: &'static str, idx: usize, px: &[u8], pal: &PaletteZstdCodec, png: &PngCodec) -> Cost {
    let t = std::time::Instant::now();
    let p = pal.encode(px, TILE, TILE);
    let us_palette = t.elapsed().as_micros();

    let t = std::time::Instant::now();
    let png_b = png.encode(px, TILE, TILE).unwrap().len();
    let us_png = t.elapsed().as_micros();

    let rgb = bgra_to_rgb(px);
    let t = std::time::Instant::now();
    let jpeg_b = motion::encode_rgb(&rgb, TILE, TILE, 80).map(|v| v.len()).unwrap_or(usize::MAX);
    let us_jpeg = t.elapsed().as_micros();

    Cost {
        shape,
        palette_ok: p.is_ok(),
        palette_b: p.unwrap_or_default().len(),
        png_b,
        jpeg_b,
        edges: full_scale_edges(px),
        us_palette,
        us_png,
        us_jpeg,
    }
}

fn med(xs: &[usize]) -> usize {
    let mut v = xs.to_vec();
    v.sort_unstable();
    v[v.len() / 2]
}

#[test]
fn codec_cost_table_is_printed() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let mut rows: Vec<Cost> = Vec::new();
    for i in 0..24usize {
        rows.push(measure("text", i, &text_tile(i), &pal, &png));
        rows.push(measure("photo", i, &photo_tile(i), &pal, &png));
        rows.push(measure("text_on_photo", i, &text_on_photo_tile(i), &pal, &png));
    }

    for shape in ["text", "photo", "text_on_photo"] {
        let sel: Vec<&Cost> = rows.iter().filter(|r| r.shape == shape).collect();
        let hit = sel.iter().filter(|r| r.palette_ok).count();
        let pngs: Vec<usize> = sel.iter().map(|r| r.png_b).collect();
        let jpegs: Vec<usize> = sel.iter().map(|r| r.jpeg_b).collect();
        let pals: Vec<usize> = sel.iter().filter(|r| r.palette_ok).map(|r| r.palette_b).collect();
        let edges: Vec<usize> = sel.iter().map(|r| r.edges).collect();
        let ratio = med(&jpegs).max(1) as f64;
        println!(
            "{shape:14} n={} 调色板命中={hit}/{} | 中位字节 palette={} png={} jpeg={} | jpeg/png={:.2}×省 | 满量程跳变 中位={} min={} max={}",
            sel.len(),
            sel.len(),
            if pals.is_empty() { 0 } else { med(&pals) },
            med(&pngs),
            med(&jpegs),
            med(&pngs) as f64 / ratio,
            med(&edges),
            edges.iter().min().unwrap(),
            edges.iter().max().unwrap(),
        );
        // 逐格账单（只有前 6 格，避免刷屏）
        for r in sel.iter().take(6) {
            println!(
                "    {}  palette={}({}B) png={}B jpeg={}B edges={} 用时 pal={}us png={}us jpeg={}us",
                r.shape,
                if r.palette_ok { "ok" } else { "reject" },
                r.palette_b,
                r.png_b,
                r.jpeg_b,
                r.edges,
                r.us_palette,
                r.us_png,
                r.us_jpeg,
            );
        }
    }

    // 这三条只是"测量台自己没坏"的守卫，不是产品判据。
    let text: Vec<&Cost> = rows.iter().filter(|r| r.shape == "text").collect();
    let photo: Vec<&Cost> = rows.iter().filter(|r| r.shape == "photo").collect();
    assert!(
        text.iter().all(|r| r.palette_ok),
        "平涂文本块应该全部走得调色板（否则分界线判据本身失效）"
    );
    assert!(
        photo.iter().all(|r| !r.palette_ok),
        "照片类块应该全部被调色板拒绝（否则 ≤256 色这条免费分界区分不开两类）"
    );
    let png_med = med(&photo.iter().map(|r| r.png_b).collect::<Vec<_>>());
    let jpeg_med = med(&photo.iter().map(|r| r.jpeg_b).collect::<Vec<_>>());
    assert!(
        jpeg_med * 2 < png_med,
        "照片块上有损连 2 倍都省不出来（jpeg={jpeg_med} png={png_med}），那这条路线根本不值得做"
    );
}

use ess_core::codec::JpegCodec;
use ess_core::codec::TileCodecId;
use ess_core::pipeline::CodecSet;

/// 选码规则的合同：只看"省几倍"，不看内容语义。
/// 三个形状分别代表三条分支——平涂块（调色板就够）、照片块（该降级）、照片上的字（不该降级）。
#[test]
fn the_policy_demotes_photo_and_keeps_text_lossless() {
    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    let jpeg = JpegCodec::default();
    let set = CodecSet { codecs: &[&pal, &png, &jpeg] };
    let pick = |px: &[u8]| set.encode_by_cost(px, TILE, TILE, 512, 2, 3, 64).unwrap();

    let (id, b) = pick(&text_tile(0));
    assert_eq!(id, TileCodecId::PaletteZstd, "平涂文本块必须走调色板（最便宜且无损）");
    assert!(b.len() < 300, "平涂块不该超过 300 B，实测 {}", b.len());

    let (id, b) = pick(&photo_tile(0));
    let png_b = png.encode(&photo_tile(0), TILE, TILE).unwrap().len();
    assert_eq!(id, TileCodecId::Jpeg, "照片块省 4.6×，该降级（png={png_b}B）");
    assert!(b.len() * 3 <= png_b, "降级后必须真省到 3 倍：jpeg={} png={png_b}", b.len());

    let (id, _) = pick(&text_on_photo_tile(0));
    assert_eq!(id, TileCodecId::Png, "照片上压字只省 2.5× 且含高对比结构，必须留无损（糊字不划算）");

    // 地板：一次 1080p 半屏照片类变化 = 255 块，按规则走完是多少字节
    let total: usize = (0..255).map(|i| pick(&photo_tile(i % 24)).1.len()).sum();
    println!("255 块照片类变化的图块总价 = {} B（无损 PNG 口径约 1.42 MB/255 块）", total);
    assert!(total < 600_000, "255 块照片类变化应落进 0.6 MB 以内，实测 {total} B");
}

/// 有损图块的线格式往返：解回来的画面允许有误差，但不能糊到不可读；
/// 窄块（1080p 底行只有 16 px 高）也要能编能解——那是现场红过的形状。
#[test]
fn jpeg_tiles_roundtrip_even_for_narrow_tiles() {
    let jpeg = JpegCodec::default();
    let src = photo_tile(0);
    for (w, h) in [(64u32, 64u32), (64, 40), (16, 64)] {
        let mut t = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = (y * 64 + x) * 4;
                let d = (y * w as usize + x) * 4;
                t[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
        let enc = jpeg.encode(&t, w, h).unwrap();
        let mut out = vec![0u8; (w * 4 * h) as usize];
        jpeg.decode_into(&enc, &mut out, w * 4).unwrap();
        let mut acc = 0u64;
        for i in 0..(w * h) as usize {
            acc += (out[i * 4] as i32 - t[i * 4] as i32).unsigned_abs() as u64;
        }
        let mae = acc / (w * h) as u64;
        assert!(mae < 8, "{w}x{h} 平均绝对误差 {mae} 太大 —— 那不是有损，是糊");
    }
}
