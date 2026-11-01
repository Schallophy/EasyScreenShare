//! 运动帧：整帧降采样 + JPEG（有损）。
//!
//! 数据通道实测只能推 ~3.7 MB/s（O-1/O-2 之外的新测量），1080p 全屏无损
//! 每帧 ~0.9 MB，30 fps 需要 27 MB/s——物理上不可能。所以大面积运动时走
//! 这条通路：整帧缩到 ≤ max_w×max_h，JPEG 编码，接收端缩放贴回画布。
//! 静态/文字仍走无损图块；两者用 `layout_id` + 整帧做边界，互不混用。
//!
//! 同一份降采样还能直接产出 I420（[`to_i420`]）交给 WebRTC 视频轨：视频轨
//! 有硬件编码器和自己的带宽估计，30 fps 比 JPEG 更省也更快。JPEG 通路保留
//! 为不支持视频轨的观看端兜底（见 `SendPipeline::video_plane`）。

use crate::frame::Frame;

/// 返回 (编码宽, 编码高, JPEG 字节)。
pub fn encode(
    frame: &Frame,
    max_w: u32,
    max_h: u32,
    quality: u8,
) -> Result<(u32, u32, Vec<u8>), String> {
    let (tw, th, rgb) = to_rgb(frame, max_w, max_h);
    Ok((tw, th, encode_rgb(&rgb, tw, th, quality)?))
}

/// 降采样为 RGB（宽高均为偶数），JPEG 兜底与视频轨共用这一次采样。
pub fn to_rgb(frame: &Frame, max_w: u32, max_h: u32) -> (u32, u32, Vec<u8>) {
    let (tw, th) = fit(frame.w, frame.h, max_w, max_h);
    (tw, th, downscale_rgb(frame, tw, th))
}

/// 已降采样的 RGB → JPEG（4:4:4：屏幕内容的色度不能被抽掉）。
pub fn encode_rgb(rgb: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(64 * 1024);
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality);
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    enc.encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// 降采样并直接产出 I420（Y + 半分辨率 U/V），给 WebRTC 视频轨用。
/// 返回 (宽, 高, Y, U, V)。
pub fn to_i420(frame: &Frame, max_w: u32, max_h: u32) -> (u32, u32, Vec<u8>, Vec<u8>, Vec<u8>) {
    let (w, h) = fit(frame.w, frame.h, max_w, max_h);
    let cw = ((w + 1) / 2) as usize;
    let ch = ((h + 1) / 2) as usize;
    let mut y = vec![0u8; (w * h) as usize];
    let mut u = vec![128u8; cw * ch];
    let mut v = vec![128u8; cw * ch];
    write_i420(frame, w, h, &mut y, &mut u, &mut v);
    (w, h, y, u, v)
}

/// 写进调用方给的连续平面（通常就是 `I420Buffer::data_mut()` 的三段）。
///
/// 源像素只读一遍：RGB 中间层按两输出行的滚动窗口放在栈上的小缓冲里，
/// 省掉整帧 2.7 MB 的分配和第二次遍历——1080p→720p 时这部分就是半帧预算。
pub fn write_i420(f: &Frame, tw: u32, th: u32, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    let cw = ((tw + 1) / 2) as usize;
    let ch = ((th + 1) / 2) as usize;
    let xs = scale_bounds(f.w, tw);
    let ys = scale_bounds(f.h, th);
    // 色度行两两成组（cy ↔ 源行 2cy、2cy+1），组间没有依赖 ⇒ 按行区间并行。这一段实测
    // 每帧 10.4 ms（视频平面臂、33 ms 预算的三分之一，§14.7.12），是量准了才动的。
    let threads = (ch / 8)
        .max(1)
        .min(std::thread::available_parallelism().map_or(1, |c| c.get()));
    if threads > 1 {
        let per = (ch as u32).div_ceil(threads as u32);
        let wstride = tw as usize;
        std::thread::scope(|s| {
            let (mut yrest, mut urest, mut vrest) = (y, u, v);
            let mut c0 = 0u32;
            while c0 < ch as u32 {
                let cn = per.min(ch as u32 - c0);
                // 这块负责的源 y 行数：th 为奇数时末块只剩一行
                let yn = (2 * cn).min(th.saturating_sub(2 * c0));
                let (yc, y_next) = yrest.split_at_mut(yn as usize * wstride);
                let (uc, u_next) = urest.split_at_mut(cn as usize * cw);
                let (vc, v_next) = vrest.split_at_mut(cn as usize * cw);
                let (f_, xs_, ys_) = (f, &xs, &ys);
                s.spawn(move || convert_cy(f_, tw, th, cw, xs_, ys_, c0, c0 + cn, yc, uc, vc));
                (yrest, urest, vrest) = (y_next, u_next, v_next);
                c0 += cn;
            }
        });
        return;
    }
    convert_cy(f, tw, th, cw, &xs, &ys, 0, ch as u32, y, u, v);
}

/// 转换色度行区间 `[cy_from, cy_to)`。三个输出平面是**该区间自己的窗口**，索引从这里的 0
/// 起算，所以并行时每个线程只看得见自己那一段。
fn convert_cy(
    f: &Frame,
    tw: u32,
    th: u32,
    cw: usize,
    xs: &[(u32, u32)],
    ys: &[(u32, u32)],
    cy_from: u32,
    cy_to: u32,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let wstride = tw as usize;
    let mut memo = [0u32; RECIP];
    let mut r0 = vec![0u8; wstride * 3];
    let mut r1 = vec![0u8; wstride * 3];
    let y_off = (2 * cy_from) as usize * wstride;
    let c_off = cy_from as usize * cw;
    for cy in cy_from..cy_to {
        let oy0 = cy * 2;
        let oy1 = oy0 + 1;
        let has1 = oy1 < th;
        avg_row(f, oy0, tw, &xs, &ys, &mut r0, &mut memo);
        if has1 {
            avg_row(f, oy1, tw, &xs, &ys, &mut r1, &mut memo);
        }
        let base = oy0 as usize * wstride - y_off;
        for x in 0..wstride {
            let o = x * 3;
            y[base + x] = luma(r0[o] as i32, r0[o + 1] as i32, r0[o + 2] as i32);
        }
        if has1 {
            let base1 = base + wstride;
            for x in 0..wstride {
                let o = x * 3;
                y[base1 + x] = luma(r1[o] as i32, r1[o + 1] as i32, r1[o + 2] as i32);
            }
        }
        for cx in 0..cw {
            let (mut sr, mut sg, mut sb) = (0u32, 0u32, 0u32);
            let mut n = 0u32;
            acc2(&r0, cx, tw as usize, &mut sr, &mut sg, &mut sb, &mut n);
            if has1 {
                acc2(&r1, cx, tw as usize, &mut sr, &mut sg, &mut sb, &mut n);
            }
            let mul = recip(&mut memo, n);
            let round = 1u32 << 19;
            let (r, g, b) = (
                ((sr * mul + round) >> 20) as i32,
                ((sg * mul + round) >> 20) as i32,
                ((sb * mul + round) >> 20) as i32,
            );
            let i = cy as usize * cw + cx - c_off;
            u[i] = chroma_u(r, g, b);
            v[i] = chroma_v(r, g, b);
        }
    }
}

/// 输出像素 → 源像素区间，**每帧算一次**。原来 `avg_row` 的内层每个输出像素要做两次
/// 64 位整数除法（1280x720 送入 ≈ 92 万×2 次），那才是 BGRA→I420 转换 12.8 ms 的
/// 主要来源（§14.7.12 的 `push_cvt` 读数）；边界单调，算成数组后内层只剩加法。
/// 取值与原来逐像素现算完全一致：`x0 = i*src/out`，`x1 = max(((i+1)*src/out), x0+1).min(src)`。
fn scale_bounds(n_src: u32, n_out: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(n_out as usize);
    if n_src == 0 || n_out == 0 {
        return out;
    }
    for i in 0..n_out as u64 {
        let a = (i * n_src as u64 / n_out as u64) as u32;
        let b = ((i + 1) * n_src as u64 / n_out as u64) as u32;
        out.push((a, b.max(a + 1).min(n_src)));
    }
    out
}

/// 一个输出像素（色度块的一半）累加进 RGB 和。
fn acc2(row: &[u8], cx: usize, tw: usize, sr: &mut u32, sg: &mut u32, sb: &mut u32, n: &mut u32) {
    for dx in 0..2 {
        let px = cx * 2 + dx;
        if px >= tw {
            continue;
        }
        let o = px * 3;
        *sr += row[o] as u32;
        *sg += row[o + 1] as u32;
        *sb += row[o + 2] as u32;
        *n += 1;
    }
}

/// `n` 的定点倒数（(1<<20)/n）：一帧里只有几种取值，避免每像素整数除法。
const RECIP: usize = 4096;

fn recip(memo: &mut [u32; RECIP], n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    if (n as usize) < RECIP {
        let slot = &mut memo[n as usize];
        if *slot == 0 {
            *slot = (1u32 << 20) / n;
        }
        *slot
    } else {
        ((1u64 << 20) / n as u64) as u32
    }
}

/// 输出一行：源帧对应行区间做区域平均，写成 RGB 连续行。行列的源区间由调用方按帧
/// 预计算（`scale_bounds`），这里不再做除法。
fn avg_row(f: &Frame, ty: u32, tw: u32, xs: &[(u32, u32)], ys: &[(u32, u32)], out: &mut [u8], memo: &mut [u32; RECIP]) {
    let (sy0, sy1) = ys[ty as usize];
    for tx in 0..tw as usize {
        let (x0, x1) = xs[tx];
        let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
        let mut n = 0u32;
        for sy in sy0..sy1 {
            let srow = sy as usize * f.stride;
            for sx in x0..x1 {
                let o = srow + sx as usize * 4;
                b += f.px[o] as u32;
                g += f.px[o + 1] as u32;
                r += f.px[o + 2] as u32;
                n += 1;
            }
        }
        let mul = recip(memo, n);
        let round = 1u32 << 19;
        let o = tx * 3;
        out[o] = ((r * mul + round) >> 20) as u8;
        out[o + 1] = ((g * mul + round) >> 20) as u8;
        out[o + 2] = ((b * mul + round) >> 20) as u8;
    }
}

/// RGB → I420，BT.601 限制范围（16–235 / 16–240）：VP8 无色彩元数据时
/// 解码端按这个范围解释，写成全范围会整体偏亮、对比偏低。
pub fn rgb_to_i420(rgb: &[u8], w: u32, h: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let cw = (w + 1) / 2;
    let ch = (h + 1) / 2;
    let mut y = vec![0u8; (w * h) as usize];
    let mut u = vec![128u8; (cw * ch) as usize];
    let mut v = vec![128u8; (cw * ch) as usize];
    for row in 0..h {
        let srow = row as usize * w as usize * 3;
        let dy = row as usize * w as usize;
        for col in 0..w {
            let o = srow + col as usize * 3;
            let (r, g, b) = (rgb[o] as i32, rgb[o + 1] as i32, rgb[o + 2] as i32);
            y[dy + col as usize] = luma(r, g, b);
        }
    }
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut sr, mut sg, mut sb) = (0i64, 0i64, 0i64);
            let mut n = 0i64;
            for dy in [0, 1] {
                let py = cy * 2 + dy;
                if py >= h {
                    continue;
                }
                for dx in [0, 1] {
                    let px = cx * 2 + dx;
                    if px >= w {
                        continue;
                    }
                    let o = (py as usize * w as usize + px as usize) * 3;
                    sr += rgb[o] as i64;
                    sg += rgb[o + 1] as i64;
                    sb += rgb[o + 2] as i64;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let (r, g, b) = ((sr / n) as i32, (sg / n) as i32, (sb / n) as i32);
            let i = cy as usize * cw as usize + cx as usize;
            u[i] = chroma_u(r, g, b);
            v[i] = chroma_v(r, g, b);
        }
    }
    (y, u, v)
}

fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

fn chroma_u(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

fn chroma_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

/// 等比缩到 `max_w×max_h` 内，返回偶数尺寸（I420/编码器要求）。
pub fn fit(w: u32, h: u32, mw: u32, mh: u32) -> (u32, u32) {    // 偶数尺寸：I420 的半分辨率色度要求宽高成对，编码器也拒绝奇数亮度平面。
    let even = |v: u32| (v & !1).max(2);
    if w <= mw && h <= mh {
        return (even(w), even(h));
    }
    let s = (mw as f64 / w as f64).min(mh as f64 / h as f64);
    (even((w as f64 * s) as u32), even((h as f64 * s) as u32))
}

/// 区域平均降采样（1920→960 正好是 2×2 平均），同时 BGRA → RGB。
fn downscale_rgb(f: &Frame, tw: u32, th: u32) -> Vec<u8> {
    let mut out = vec![0u8; (tw * th * 3) as usize];
    let mut memo = [0u32; RECIP];
    let xs = scale_bounds(f.w, tw);
    let ys = scale_bounds(f.h, th);
    for ty in 0..th {
        let row = ty as usize * tw as usize * 3;
        avg_row(
            f,
            ty,
            tw,
            &xs,
            &ys,
            &mut out[row..row + tw as usize * 3],
            &mut memo,
        );
    }
    out
}

/// 解码 JPEG 并最近邻缩放到画布（画布尺寸由布局决定，不随运动帧改变）。
pub fn decode_into_canvas(data: &[u8], canvas: &mut Frame) -> Result<(), String> {
    if canvas.w == 0 || canvas.h == 0 {
        return Err("motion before layout".into());
    }
    let mut dec = zune_jpeg::JpegDecoder::new(data);
    let rgb = dec.decode().map_err(|e| format!("jpeg decode: {e:?}"))?;
    let info = dec.info().ok_or("jpeg missing info")?;
    let iw = u32::from(info.width);
    let ih = u32::from(info.height);
    if iw == 0 || ih == 0 {
        return Err("jpeg empty".into());
    }
    let px = iw as usize * ih as usize;
    let ch = (rgb.len() / px).max(1);
    for y in 0..canvas.h {
        let sy = (y as u64 * ih as u64 / canvas.h as u64) as usize;
        let drow = y as usize * canvas.stride;
        for x in 0..canvas.w {
            let sx = (x as u64 * iw as u64 / canvas.w as u64) as usize;
            let si = (sy * iw as usize + sx) * ch;
            let di = drow + x as usize * 4;
            if ch >= 3 {
                canvas.px[di] = rgb[si + 2];
                canvas.px[di + 1] = rgb[si + 1];
                canvas.px[di + 2] = rgb[si];
            } else {
                canvas.px[di] = rgb[si];
                canvas.px[di + 1] = rgb[si];
                canvas.px[di + 2] = rgb[si];
            }
            canvas.px[di + 3] = 255;
        }
    }
    Ok(())
}
