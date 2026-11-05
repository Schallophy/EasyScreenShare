//! 生成合成屏的"终帧"（动画停止后的固定画面）PPM，用于端到端验收比对。
//!
//! 用法：`ess-gen-expected <out.ppm> [--w 1280 --h 720 --anim 30]`

use ess_capture::{ScreenSource, SyntheticScreen};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_string())
}

fn write_ppm(path: &str, f: &ess_core::frame::Frame) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{} {}\n255\n", f.w, f.h)?;
    let mut row = Vec::with_capacity(f.w as usize * 3);
    for y in 0..f.h as usize {
        row.clear();
        for x in 0..f.w as usize {
            let s = y * f.stride + x * 4;
            row.push(f.px[s + 2]);
            row.push(f.px[s + 1]);
            row.push(f.px[s]);
        }
        out.write_all(&row)?;
    }
    Ok(())
}

/// 读 P6（本工具自己写的那种头：`P6\n<w> <h>\n255\n`）。返回 (w, h, rgb)。
fn read_ppm(path: &str) -> Result<(usize, usize, Vec<u8>), String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut it = b.splitn(4, |c| *c == b'\n');
    let magic = it.next().unwrap_or_default();
    if magic != b"P6" {
        return Err(format!("{path}: 不是 P6"));
    }
    let dims = String::from_utf8_lossy(it.next().unwrap_or_default()).to_string();
    let mut wh = dims.split_whitespace().map(|s| s.parse::<usize>());
    let (w, h) = match (wh.next(), wh.next()) {
        (Some(Ok(w)), Some(Ok(h))) => (w, h),
        _ => return Err(format!("{path}: 尺寸头坏了")),
    };
    let maxv = it.next().unwrap_or_default();
    if maxv != b"255" {
        return Err(format!("{path}: 只支持 8 bit"));
    }
    let px = it.next().unwrap_or_default().to_vec();
    if px.len() < w * h * 3 {
        return Err(format!("{path}: 像素不足（{} < {}）", px.len(), w * h * 3));
    }
    Ok((w, h, px))
}

/// `diff <a.ppm> <b.ppm> [--mae 8]`：逐通道平均绝对误差 + 最大差。
/// 存在的理由：优先级改成"流畅度第一"之后，验收臂不能再拿"逐字节相等"当判据，
/// 得有一个**可算的**替代（MAE 阈值）。尺寸不等直接判红。
fn diff_mode(args: &[String]) -> ! {
    let (Some(a), Some(b)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: ess-gen-expected diff <a.ppm> <b.ppm> [--mae 8]");
        std::process::exit(2);
    };
    let limit: f64 = arg(args, "--mae", "8").parse().unwrap_or(8.0);
    let (wa, ha, pa) = match read_ppm(a) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("FAIL {e}");
            std::process::exit(1);
        }
    };
    let (wb, hb, pb) = match read_ppm(b) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("FAIL {e}");
            std::process::exit(1);
        }
    };
    if (wa, ha) != (wb, hb) {
        eprintln!("FAIL 尺寸不同：{wa}x{ha} vs {wb}x{hb}");
        std::process::exit(1);
    }
    let mut acc = 0u64;
    let mut worst = 0u8;
    for (x, y) in pa.iter().zip(pb.iter()) {
        let d = x.abs_diff(*y);
        acc += d as u64;
        worst = worst.max(d);
    }
    let n = (wa * ha * 3) as f64;
    let mae = acc as f64 / n;
    println!(
        "MAE={mae:.3} max={worst} 尺寸={wa}x{ha} 像素={}",
        wa * ha
    );
    if args.iter().any(|a| a == "--tiles") {
        // 逐块报告：**差异的形状**比总量更能指路（整条块行 vs 散点），#11 就是靠这个读出来的。
        let t = ess_core::TILE as usize;
        let cols = wa.div_ceil(t);
        let rows = ha.div_ceil(t);
        let mut hit_rows = vec![0usize; rows];
        let mut listed = 0usize;
        for ry in 0..rows {
            for cx in 0..cols {
                let (x1, y1) = (((cx + 1) * t).min(wa), ((ry + 1) * t).min(ha));
                let mut acc_t = 0u64;
                let mut worst_t = 0u8;
                let mut n = 0u64;
                for y in ry * t..y1 {
                    for x in cx * t..x1 {
                        let i = (y * wa + x) * 3;
                        let d = pa[i].abs_diff(pb[i]);
                        acc_t += d as u64;
                        worst_t = worst_t.max(d);
                        // 只按 B 通道统计，够了：判"哪块坏了"不看色度
                        if x + 1 < x1 { /* 下一像素由循环覆盖 */ }
                        n += 1;
                    }
                }
                let m = acc_t as f64 / n.max(1) as f64;
                if m >= 2.0 {
                    hit_rows[ry] += 1;
                    if listed < 20 {
                        println!("  块({ry},{cx}) MAE={m:.1} max={worst_t}");
                        listed += 1;
                    }
                }
            }
        }
        let mut rows_hit: Vec<(usize, usize)> = hit_rows.iter().copied().enumerate().filter(|(_, n)| *n > 0).collect();
        rows_hit.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        println!("差异块按行统计（行,块数，前 6）：{:?}", &rows_hit[..rows_hit.len().min(6)]);
    }
    if mae >= limit {
        eprintln!("FAIL MAE {mae:.3} ≥ {limit}（判据：流畅度第一档允许有损，但要看得清）");
        std::process::exit(1);
    }
    println!("PASS MAE {mae:.3} < {limit}");
    std::process::exit(0);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s == "diff").unwrap_or(false) {
        diff_mode(&args);
    }
    let Some(out) = args.first().filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: ess-gen-expected <out.ppm> [--w 1280 --h 720 --anim 30]");
        std::process::exit(2);
    };
    let w: u32 = arg(&args, "--w", "1280").parse().unwrap();
    let h: u32 = arg(&args, "--h", "720").parse().unwrap();
    let anim: u32 = arg(&args, "--anim", "30").parse().unwrap();
    let mut src = SyntheticScreen::new(w, h, anim);
    // 与 --demo 同源：ESS_DEMO_BLOCK=0 时不画那块移动红块，否则期望帧与实帧不同源
    src.block = std::env::var("ESS_DEMO_BLOCK").map(|v| v != "0").unwrap_or(true);
    // 同理：ESS_DEMO_STEP 决定左半屏那次单帧突变画不画，不读同一个开关期望帧就对不上
    src.step_at = std::env::var("ESS_DEMO_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // 静止期长度同源（默认 0）：采样时刻要跟着往后挪，否则期望帧落在动画期里
    let delay: u64 = std::env::var("ESS_DEMO_ANIM_DELAY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    src.animate_delay = delay as u32;
    // 取样时刻必须落在**动画结束之后、且 step_at 那次单帧突变之后**：
    // `idx = ts_us/100_000`，所以 step_at 用的是 100 ms 格（90 ≈ t=9 s）。
    // 原来只按 anim 取样，带 ESS_DEMO_STEP 的夹具会拿"变化前"的画面当期望帧——
    // 实测表现为 MAE 恒定 52（半屏照片类的整块内容差），会被误读成有损糊了。
    let ts_idx = (anim as u64 + delay + 10).max(src.step_at as u64 + 2);
    let f = src.frame(ts_idx * 100_000);
    match write_ppm(&out, &f) {
        Ok(()) => println!("wrote {out} ({}x{})", f.w, f.h),
        Err(e) => {
            eprintln!("write: {e}");
            std::process::exit(1);
        }
    }
}
