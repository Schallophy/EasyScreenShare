//! ess-tone：生成纯正弦 WAV 供声音链路验收（440 Hz 是基准音，1000 Hz 用来做对照）。
//!
//! 验收要的是「观看端解出来的 PCM 里确实有那个频率」，所以激励信号必须可生成、可重复、
//! 频谱干净——播放器现场合成会带自己的噪声，直接写 PCM 更可靠。
//!
//! ```text
//! ess-tone out.wav [--freq 440] [--seconds 20] [--rate 48000] [--channels 2] [--amp 0.5]
//! ```

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = a.first().cloned() else {
        eprintln!("用法: ess-tone out.wav [--freq 440] [--seconds 20]");
        std::process::exit(2);
    };
    let freq: f64 = arg(&a, "--freq", "440").parse().unwrap_or(440.0);
    let secs: f64 = arg(&a, "--seconds", "20").parse().unwrap_or(20.0);
    let rate: u32 = arg(&a, "--rate", "48000").parse().unwrap_or(48000);
    let ch: u16 = arg(&a, "--channels", "2").parse().unwrap_or(2);
    let amp: f64 = arg(&a, "--amp", "0.5").parse().unwrap_or(0.5);

    let n = (rate as f64 * secs) as usize;
    let mut pcm: Vec<i16> = Vec::with_capacity(n * ch as usize);
    for i in 0..n {
        let v = (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() * amp * 32767.0;
        let s = v.round().clamp(-32768.0, 32767.0) as i16;
        for _ in 0..ch {
            pcm.push(s);
        }
    }
    // 淡入淡出 10 ms，免得起始那一下阶跃在频谱上铺一层宽带能量，把对照频点也抬高
    let ramp = (rate as f64 * 0.01) as usize;
    for i in 0..ramp.min(n) {
        let g = i as f64 / ramp as f64;
        for c in 0..ch as usize {
            pcm[i * ch as usize + c] = (pcm[i * ch as usize + c] as f64 * g) as i16;
            let tail = (n - 1 - i) * ch as usize + c;
            pcm[tail] = (pcm[tail] as f64 * g) as i16;
        }
    }

    let data_len = (pcm.len() * 2) as u32;
    let mut out: Vec<u8> = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * ch as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(ch * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in &pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    if let Err(e) = std::fs::write(&path, &out) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    println!("{path}: {freq:.0} Hz {rate} Hz x {ch} · {:.1} s", secs);
}
