//! WASAPI 渲染环回采集线程。
//!
//! 通路：默认输出端点 → `IAudioClient` → `GetMixFormat` → 以 SHARED + LOOPBACK 初始化
//! → `IAudioCaptureClient` 轮询取包 → 转成交织 s16 → 切成 10 ms 一块交出去。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use windows::core::GUID;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

use crate::chunk_len;
use crate::take_chunks;
use crate::to_s16;
use crate::Chunk;
use crate::Format;
use crate::SampleFormat;

/// 环回环形缓冲 200 ms（100 ns 单位）。给到块长的 20 倍，采集线程被调度晚了
/// 也还有富余，不会丢包。
const RING_HNS: i64 = 2_000_000;

/// 没有数据时每轮轮询的间隔。环回不走事件回调（那是独占模式的路子），
/// 3 ms 对 10 ms 的块足够密。
const POLL: Duration = Duration::from_millis(3);

/// 设备静音判定阈值：超过它就开始补静音块
const DRY: Duration = Duration::from_millis(100);

/// 块长，即补静音的节奏
const CHUNK: Duration = Duration::from_millis(10);

// 这几个 tag 与 SubFormat GUID 在 0.62 里分属 Win32_Media_Multimedia / Win32_Media_KernelStreaming
// 整块绑定，为几个常量把它们拉进来不划算；照 dxgi.rs 的做法直接写数值。
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// COM 分配的混音格式指针，用 Drop 兜住任何一条早退路径，不漏内存
struct MixPtr(*mut WAVEFORMATEX);

impl Drop for MixPtr {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const core::ffi::c_void)) }
    }
}

pub(crate) fn start(stop: Arc<AtomicBool>) -> Result<(Format, mpsc::Receiver<Chunk>), String> {
    let (tx, rx) = mpsc::channel();
    // 混音格式只有 COM 线程问得到，用 rendezvous 通道让 start 阻塞到线程把它报上来
    let (rep_tx, rep_rx) = mpsc::sync_channel::<Result<Format, String>>(0);
    std::thread::Builder::new()
        .name("ess-audio-loopback".into())
        .spawn(move || thread_main(stop, tx, rep_tx))
        .map_err(|e| format!("环回采集线程起不来: {e}"))?;
    // 线程要么报回格式、要么报回错误、要么整个没了（Drop 让 recv 断开）：三种都要转成结果
    let fmt = rep_rx
        .recv()
        .map_err(|_| "环回采集线程在报格式之前就退出了".to_string())??;
    Ok((fmt, rx))
}

fn thread_main(stop: Arc<AtomicBool>, tx: Sender<Chunk>, rep: SyncSender<Result<Format, String>>) {
    // start() 从非 COM 线程调过来，COM 的 init / uninit 都在这条采集线程上配对
    // （S_FALSE 表示这条线程已经 init 过，绑定按成功交回来，正好）
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = rep.send(Err(format!("CoInitializeEx: {e}")));
            return;
        }
        let opened = match open_loopback() {
            Ok(v) => v,
            Err(e) => {
                let _ = rep.send(Err(e));
                CoUninitialize();
                return;
            }
        };
        let (client, cap, fmt, block_align, encoding) = opened;
        let _ = rep.send(Ok(fmt));
        pump(&stop, &tx, &cap, fmt, block_align, encoding);
        let _ = client.Stop();
        CoUninitialize();
    }
}

/// 打开默认渲染端点上的环回流。返回 `(IAudioClient, 采集接口, 混音格式, 每帧字节数, 样本编码)`。
unsafe fn open_loopback() -> Result<(IAudioClient, IAudioCaptureClient, Format, usize, SampleFormat), String> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| format!("创建设备枚举器: {e}"))?;
    let device = enumerator
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .map_err(|e| format!("默认输出端点（没接扬声器/耳机就是这个错）: {e}"))?;
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("激活 IAudioClient: {e}"))?;
    let mix = MixPtr(client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?);
    if mix.0.is_null() {
        return Err("GetMixFormat 给出空指针".into());
    }
    let (fmt, encoding, block_align) = parse_mix(mix.0)?;
    // ESS_AUDIO_TRACE=1 时打出解析结果，用来确认真机走的到底是 float 还是 PCM 通路
    if std::env::var("ESS_AUDIO_TRACE").is_ok() {
        eprintln!("ess-audio: 混音格式 {fmt:?} 编码 {encoding:?} 每帧 {block_align} 字节");
    }
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            RING_HNS,
            0,
            mix.0,
            None,
        )
        .map_err(|e| format!("初始化环回流({fmt:?}): {e}"))?;
    let cap: IAudioCaptureClient = client
        .GetService()
        .map_err(|e| format!("取 IAudioCaptureClient: {e}"))?;
    // 0.62 的绑定里 StartStream 就叫 Start
    client.Start().map_err(|e| format!("启动环回流: {e}"))?;
    Ok((client, cap, fmt, block_align, encoding))
}

/// 一帧的字节数。nBlockAlign 为 0 的驱动不算罕见，按位宽自己算。
fn block_align_of(n_block_align: u16, fmt: &Format, encoding: SampleFormat) -> usize {
    if n_block_align > 0 {
        n_block_align as usize
    } else {
        fmt.channels as usize
            * match encoding {
                SampleFormat::F32 => 4,
                SampleFormat::I16 => 2,
            }
    }
}

/// 定出混音格式的采样率、声道数与样本编码。
///
/// 扩展格式（0xFFFE）的 wFormatTag 只说明「这是扩展头」，真正的样本编码在 SubFormat
/// GUID 里，不读它就会把 float 总线当 PCM 解，出来全是噪声。
unsafe fn parse_mix(mix: *const WAVEFORMATEX) -> Result<(Format, SampleFormat, usize), String> {
    // WAVEFORMATEX / WAVEFORMATEXTENSIBLE 都是 packed(1)：按非对齐读成局部值，
    // 之后只在局部值上判断，不在字段上留未对齐的引用
    let wf = core::ptr::read_unaligned(mix);
    let rate = wf.nSamplesPerSec;
    let channels = wf.nChannels;
    let n_block_align = wf.nBlockAlign;
    let cb = wf.cbSize;
    let (tag, bits, sub) = if wf.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
        // 扩展头要求 cbSize 至少装得下 WAVEFORMATEXTENSIBLE 比 WAVEFORMATEX 多出来的那截
        let tail = core::mem::size_of::<WAVEFORMATEXTENSIBLE>() - core::mem::size_of::<WAVEFORMATEX>();
        if (cb as usize) < tail {
            return Err(format!("扩展头不完整：cbSize={cb} 需要至少 {tail}"));
        }
        let e = core::ptr::read_unaligned(mix as *const WAVEFORMATEXTENSIBLE);
        (e.Format.wFormatTag, e.Format.wBitsPerSample, Some(e.SubFormat))
    } else {
        (wf.wFormatTag, wf.wBitsPerSample, None)
    };
    if rate == 0 || channels == 0 {
        return Err(format!("混音格式非法: {rate}Hz x {channels}ch"));
    }
    let encoding = match sub {
        // 扩展格式里 wFormatTag 自己就是 0xFFFE，只能靠 SubFormat 分辨
        Some(g) if g == SUBTYPE_FLOAT => SampleFormat::F32,
        Some(g) if g == SUBTYPE_PCM => SampleFormat::I16,
        Some(g) => return Err(format!("不支持的 SubFormat GUID {g:?}")),
        None if tag == WAVE_FORMAT_IEEE_FLOAT => SampleFormat::F32,
        None if tag == WAVE_FORMAT_PCM => SampleFormat::I16,
        None => return Err(format!("不支持的音频格式 wFormatTag={tag:#06x}")),
    };
    let want = match encoding {
        SampleFormat::F32 => 32,
        SampleFormat::I16 => 16,
    };
    if bits as usize != want {
        return Err(format!(
            "不支持 {} 位 {:?}（只收 32 位 IEEE float 和 16 位 PCM）",
            bits, encoding
        ));
    }
    let fmt = Format {
        sample_rate: rate,
        channels,
    };
    Ok((fmt, encoding, block_align_of(n_block_align, &fmt, encoding)))
}

fn silence(per_chunk: usize, channels: usize) -> Chunk {
    Chunk {
        pcm: vec![0; per_chunk],
        samples_per_channel: per_chunk / channels.max(1),
    }
}

/// 采集主循环。拿到数据就转码 + 切块交出去；设备静音就补静音块，保证时间轴不断。
unsafe fn pump(
    stop: &AtomicBool,
    tx: &Sender<Chunk>,
    cap: &IAudioCaptureClient,
    fmt: Format,
    block_align: usize,
    encoding: SampleFormat,
) {
    let per_chunk = chunk_len(&fmt);
    let channels = fmt.channels as usize;
    let mut staging: Vec<i16> = Vec::with_capacity(per_chunk * 4);
    let mut last_real = Instant::now();
    let mut last_emit = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let mut got = false;
        loop {
            // GetNextPacketSize 返回 AUDCLNT_S_BUFFER_EMPTY 这类 S_ 码时，绑定按 Ok(0) 交回来，
            // 所以下面 avail == 0 就等价于「这一刻设备没出声」
            let avail = match cap.GetNextPacketSize() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("ess-audio: GetNextPacketSize 失败，采集中止: {e}");
                    return;
                }
            };
            if avail == 0 {
                break;
            }
            let mut data: *mut u8 = core::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if let Err(e) = cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None) {
                eprintln!("ess-audio: GetBuffer 失败，采集中止: {e}");
                return;
            }
            if frames > 0 && !data.is_null() {
                let raw = std::slice::from_raw_parts(data, frames as usize * block_align);
                staging.extend_from_slice(&to_s16(raw, encoding));
                got = true;
                // 设备被重置（睡眠唤醒、切端点、格式变更）时这一段接不上时间轴，丢掉重来
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    staging.clear();
                }
                last_real = Instant::now();
            }
            if let Err(e) = cap.ReleaseBuffer(frames) {
                eprintln!("ess-audio: ReleaseBuffer 失败，采集中止: {e}");
                return;
            }
        }

        if got {
            for c in take_chunks(&mut staging, per_chunk, channels) {
                if tx.send(c).is_err() {
                    return; // 接收端已经不要了
                }
                last_emit = Instant::now();
            }
        } else {
            std::thread::sleep(POLL);
            // 关键行为：系统不出声时环回一个包都不交（不是给零数据），流会整个干涸。
            // 干超过 100 ms 就按 10 ms 的节奏补静音块，消费端队列一空就会把这段当成
            // 播放结束，时间轴也断了。
            if last_real.elapsed() >= DRY {
                while last_emit.elapsed() >= CHUNK {
                    if tx.send(silence(per_chunk, channels)).is_err() {
                        return;
                    }
                    last_emit = Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod live_tests {
    //! 真机验证：环回确实抓到了正在播放的声音。
    //! `cargo test -p ess-audio -- --ignored --nocapture`

    use super::*;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Child;
    use std::process::Command;
    use std::process::Stdio;

    /// 不弹控制台窗口
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn put(b: &mut Vec<u8>, s: &[u8]) {
        b.extend_from_slice(s);
    }
    fn u32le(b: &mut Vec<u8>, x: u32) {
        b.extend_from_slice(&x.to_le_bytes());
    }
    fn u16le(b: &mut Vec<u8>, x: u16) {
        b.extend_from_slice(&x.to_le_bytes());
    }

    /// 自己写 RIFF：44 字节头 + 16 位 PCM 立体声正弦，不依赖任何外部工具
    fn sine_wav(rate: u32, secs: f64, freq: f64, amp: f64) -> Vec<u8> {
        let frames = (rate as f64 * secs) as usize;
        let ch = 2usize;
        let data = frames * ch * 2;
        let mut b = Vec::with_capacity(44 + data);
        put(&mut b, b"RIFF");
        u32le(&mut b, (36 + data) as u32);
        put(&mut b, b"WAVE");
        put(&mut b, b"fmt ");
        u32le(&mut b, 16);
        u16le(&mut b, 1);
        u16le(&mut b, ch as u16);
        u32le(&mut b, rate);
        u32le(&mut b, rate * ch as u32 * 2);
        u16le(&mut b, (ch * 2) as u16);
        u16le(&mut b, 16);
        put(&mut b, b"data");
        u32le(&mut b, data as u32);
        // 头尾各 10 ms 淡入淡出：整段缓冲硬起硬止会带一记 click，那不该算进音窗能量
        let ramp = (rate / 100) as usize;
        for i in 0..frames {
            let env = (i.min(frames - 1 - i).min(ramp) as f64 / ramp as f64).clamp(0.0, 1.0);
            let s = (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() * amp * env;
            let v = (s * 32767.0) as i16;
            for _ in 0..ch {
                put(&mut b, &v.to_le_bytes());
            }
        }
        b
    }

    /// 播放进程 + 临时文件的生命周期收在这儿：断言失败也按 PID 收进程、删文件
    struct Player {
        child: Option<Child>,
        wav: PathBuf,
    }

    impl Player {
        fn spawn(wav: &Path) -> Self {
            let script = format!("(New-Object Media.SoundPlayer '{}').PlaySync()", wav.display());
            let child = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .expect("起不来 powershell，没法播参考音");
            println!("播放进程 pid={}", child.id());
            Player {
                child: Some(child),
                wav: wav.to_path_buf(),
            }
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            if let Some(mut c) = self.child.take() {
                let pid = c.id();
                let _ = c.kill();
                let _ = c.wait();
                println!("播放进程已按 PID 结束 pid={pid}");
            }
            let _ = std::fs::remove_file(&self.wav);
        }
    }

    fn collect(rx: &mpsc::Receiver<Chunk>, dur: Duration) -> Vec<Chunk> {
        let t0 = Instant::now();
        let mut v = Vec::new();
        while t0.elapsed() < dur {
            if let Ok(c) = rx.recv_timeout(Duration::from_millis(50)) {
                v.push(c);
            }
        }
        v
    }

    fn flat(cs: &[Chunk]) -> Vec<i16> {
        cs.iter().flat_map(|c| c.pcm.iter().copied()).collect()
    }

    /// 满幅归一后的 RMS
    fn rms(x: &[i16]) -> f64 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|&v| (v as f64 / 32768.0).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// 交织流里取一个声道，归一到 ±1
    fn channel(pcm: &[i16], channels: usize, idx: usize) -> Vec<f64> {
        pcm.chunks(channels.max(1))
            .filter_map(|f| f.get(idx).copied())
            .map(|v| v as f64 / 32768.0)
            .collect()
    }

    /// Goertzel 单频点幅度估计。bin 取最近整数做相干采样，否则谱泄漏会把邻近频点一起抬起来。
    fn goertzel(x: &[f64], rate: u32, freq: f64) -> f64 {
        let n = x.len() as f64;
        if n < 1.0 {
            return 0.0;
        }
        let k = (0.5 + freq * n / rate as f64).floor();
        let w = 2.0 * std::f64::consts::PI * k / n;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() * 2.0 / n
    }

    fn band(pcm: &[i16], channels: usize, rate: u32, freq: f64) -> f64 {
        let ch = channels.max(1);
        (0..ch)
            .map(|i| goertzel(&channel(pcm, ch, i), rate, freq))
            .sum::<f64>()
            / ch as f64
    }

    #[test]
    #[ignore]
    fn loopback_captures_playback() {
        let stop = Arc::new(AtomicBool::new(false));
        let (fmt, rx) = match crate::start(stop.clone()) {
            Ok(v) => v,
            // 本机没有音频端点就必须炸，跳过不能看着像过了
            Err(e) => panic!("打不开环回端点: {e}"),
        };
        let per = chunk_len(&fmt);
        let ch = fmt.channels as usize;
        println!(
            "混音格式 = {}Hz x {ch}，10 ms 一块 = {per} 个交织 i16",
            fmt.sample_rate
        );

        // 播放前先收 200 ms 当底噪；这段时间设备本来就没出声，顺带验证了补静音生效
        let base = collect(&rx, Duration::from_millis(200));
        assert!(!base.is_empty(), "启动后 200 ms 一块都没收到：设备静音时流干涸了");
        for c in base.iter() {
            assert_eq!(c.pcm.len(), per, "每块必须恰好 10 ms");
            assert_eq!(c.samples_per_channel, per / ch.max(1));
        }
        let base_rms = rms(&flat(&base));
        println!("底噪 {} 块，RMS={base_rms:.6}", base.len());

        let mut path = std::env::temp_dir();
        path.push(format!("ess-audio-tone-{}.wav", std::process::id()));
        std::fs::write(&path, sine_wav(fmt.sample_rate, 1.5, 440.0, 0.7)).expect("写测试 WAV 失败");
        let player = Player::spawn(&path);

        let tone = collect(&rx, Duration::from_millis(3000));
        stop.store(true, Ordering::Relaxed);
        // 给采集线程一点时间退出（顺带做 CoUninitialize）
        std::thread::sleep(Duration::from_millis(80));
        drop(player);

        assert!(!tone.is_empty(), "播放期间一块都没收到");
        for c in tone.iter() {
            assert_eq!(c.pcm.len(), per, "每块必须恰好 10 ms");
            assert_eq!(c.samples_per_channel, per / ch.max(1));
        }
        let all = flat(&tone);
        let win = 100usize; // 1 s
        let nwin = all.len() / per;
        assert!(nwin > win, "播放窗口只收到 {nwin} 块，不够 1 秒");
        // powershell 冷启动时间不定，取能量最高的那一秒做测量窗，比按固定偏移切稳
        let (bi, tone_rms) = (0..=nwin - win)
            .map(|i| (i, rms(&all[i * per..(i + win) * per])))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        let w = &all[bi * per..(bi + win) * per];
        let g440 = band(w, ch, fmt.sample_rate, 440.0);
        let g1000 = band(w, ch, fmt.sample_rate, 1000.0);
        println!(
            "收到 {} 块；音窗从第 {bi} 块（播放后 {} ms）起，RMS={tone_rms:.6}，底噪 RMS={base_rms:.6}",
            tone.len(),
            bi * 10
        );
        println!(
            "|Goertzel(440)|={g440:.6}  |Goertzel(1000)|={g1000:.6}  比值={:.1}",
            g440 / (g1000 + 1e-12)
        );

        assert!(
            tone_rms > base_rms * 4.0 + 0.003,
            "播放期间能量没明显起来: 音窗 {tone_rms:.6} vs 底噪 {base_rms:.6}"
        );
        assert!(
            g440 > g1000 * 5.0,
            "440Hz 没盖过 1000Hz（{g440:.6} vs {g1000:.6}）：抓到的不是我们放的那段音"
        );
        assert!(g440 > 0.01, "440Hz 幅度太弱: {g440:.6}");
    }
}
