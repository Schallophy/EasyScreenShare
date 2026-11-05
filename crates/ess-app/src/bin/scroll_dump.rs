//! ess-scroll-dump：把合成屏的滚动序列按裸 BGRA 写到 stdout，供 ffmpeg 直接编码。
//!
//! 为什么要有这个东西：滚动基线（IMPLEMENTATION §14.4）的"视频 vs 图块"对比必须用
//! **同一段素材**，而素材要能逐帧复现——否则两边的质量数字不同源，比较无意义。写成裸
//! 流而不是几百个 PPM 文件，是因为 300 帧 1280x720 的 PPM 有 800 MB，而管道给 ffmpeg
//! 是零落盘。
//!
//! 合成屏本身就是**代码页风格 + 逐帧纵向滚动**（行号随动画计数推进），所以它正是 §14.4
//! 要的那个场景。动画计数直接由时间戳决定（`idx = ts / 100ms`），所以这里按帧号给定
//! idx，滚动速度就是确定的：每帧 `16 × --step` 像素。
//!
//! ```text
//! ess-scroll-dump [--w 1280] [--h 720] [--frames 300] [--fps 30] [--step 1] > raw.bin
//! ```

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    use ess_capture::ScreenSource;
    let a: Vec<String> = std::env::args().skip(1).collect();
    let w: u32 = arg(&a, "--w", "1280").parse().unwrap_or(1280);
    let h: u32 = arg(&a, "--h", "720").parse().unwrap_or(720);
    let frames: u64 = arg(&a, "--frames", "300").parse().unwrap_or(300);
    let fps: u64 = arg(&a, "--fps", "30").parse().unwrap_or(30);
    // 每个输出帧推进几个动画格（一格 = 一行 16 px）
    let step: u64 = arg(&a, "--step", "1").parse().unwrap_or(1);

    eprintln!(
        "ess-scroll-dump: {w}x{h} {frames} 帧 @{fps} fps，每帧滚 {} px ⇒ {} px/s",
        16 * step,
        16 * step * fps
    );

    let mut screen = ess_capture::SyntheticScreen {
        w,
        h,
        // 动画期覆盖整段：静止期由 --frames 截断，不会中途停下
        animate_frames: u32::MAX,
        animate_delay: 0,
        flash: false,
        // 保持有红块：§14.4 的质量表是用这份素材测出来的，换素材就等于换基线。
        // 要测"纯滚动内容"请走 `ess-share --demo` + `ESS_DEMO_BLOCK=0`。
        block: true,
        video_like: false,
        step_at: 0,
    };
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::io::stdout());
    for i in 0..frames {
        // idx = i*step ⇒ 时间戳给到 idx*100ms 即可（合成屏按 ts/100_000 取动画帧）
        let ts = i * step * 100_000;
        let f = screen.frame(ts);
        // BGRA 逐行写出：Frame 的 stride 可能带填充，rawvideo 要紧密排列
        for y in 0..h as usize {
            let row = &f.px[y * f.stride..y * f.stride + (w as usize * 4)];
            std::io::Write::write_all(&mut out, row).unwrap();
        }
    }
    drop(out);
}
