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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    // 静止期长度同源（默认 0）：采样时刻要跟着往后挪，否则期望帧落在动画期里
    let delay: u64 = std::env::var("ESS_DEMO_ANIM_DELAY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    src.animate_delay = delay as u32;
    // ts 取动画结束后的任意时刻：10 fps，anim 帧后即固定
    let f = src.frame((anim as u64 + delay + 10) * 100_000);
    match write_ppm(&out, &f) {
        Ok(()) => println!("wrote {out} ({}x{})", f.w, f.h),
        Err(e) => {
            eprintln!("write: {e}");
            std::process::exit(1);
        }
    }
}
