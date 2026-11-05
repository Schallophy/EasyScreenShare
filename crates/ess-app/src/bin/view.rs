//! ess-view：LiveKit 数据通道 → 图块管线 → 窗口 / 落盘。
//!
//! 用法：
//! ```text
//! ess-view [--url ws://127.0.0.1:7880] [--room demo] [--identity view]
//!         [--no-window] [--dump out.ppm] [--seconds 0]
//!         [--audio-dump out.wav] [--audio-channels 2]
//!         [--video-dump out.ppm] [--video-seq DIR]
//! ```
//! 每 500 ms 回一条状态报告（§6.4 兜底）。系统声音轨订阅后只统计不播放（同机会成
//! 回声环），要验内容用 `--audio-dump`。
//!
//! `--video-dump`/`--video-seq` 解码的是**媒体通道那条视频轨**：平面切到视频时图块
//! 画布不再更新，观看端真正看到的画面就在这条轨里。产品视频平面的交付质量只能从
//! 这里量，从发送端码流解出来的数字都不算数。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ess_codec_palette::PaletteZstdCodec;
use ess_codec_png::PngCodec;
use ess_core::frame::Frame;
use ess_core::model::Message;
use ess_core::pipeline::{CodecSet, RecvPipeline};
use ess_core::wire::WireMsg;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_string())
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// 观看端收到的系统声音统计。计数证明链路通，RMS/峰值证明里面真的有声音而不是
/// 一串全零——"轨订阅成功"和"听得到"是两件事，必须分开验。
#[derive(Default, Clone, Copy)]
struct AudioStat {
    blocks: u64,
    samples: u64,
    sum_sq: f64,
    peak: f64,
    rate: u32,
    channels: u32,
}

impl AudioStat {
    fn rms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            (self.sum_sq / self.samples as f64).sqrt()
        }
    }
}

/// 收到的 PCM 落盘成 WAV——听一眼频谱才能证明 440 Hz 是穿过 Opus 回来的，
/// 而不是本地统计里的自说自话。
fn write_wav(path: &str, pcm: &[i16], sample_rate: u32, channels: u16) -> std::io::Result<()> {
    use std::io::Write;
    let data_len = (pcm.len() * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * 2;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&(36 + data_len).to_le_bytes());
    hdr.extend_from_slice(b"WAVEfmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes()); // PCM
    hdr.extend_from_slice(&channels.to_le_bytes());
    hdr.extend_from_slice(&sample_rate.to_le_bytes());
    hdr.extend_from_slice(&byte_rate.to_le_bytes());
    hdr.extend_from_slice(&(channels * 2).to_le_bytes());
    hdr.extend_from_slice(&16u16.to_le_bytes());
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_len.to_le_bytes());
    out.write_all(&hdr)?;
    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    out.write_all(&bytes)?;
    Ok(())
}

/// 媒体通道视频轨的解码产物统计。
///
/// 平面切到视频时图块画布不再更新，"观看端看到的"就是这条轨的最新解出帧；
/// 帧数用来算**交付帧率**（这是合同），落盘帧用来算交付画质。
#[derive(Default)]
struct VideoStat {
    frames: u64,
    w: u32,
    h: u32,
    latest: Option<Frame>,
    /**
     * 相邻解码帧的到达间隔（ms），按报告窗口取走。窗口平均帧率看不出错在哪：平面
     * 没开的那几秒本来就没有视频帧（画面由无损平面出），平均下来却像"交付掉到
     * 19 fps"。间隔只在真的有帧在流的时候统计，才回答"观看端看到的流畅度"。
     */
    gaps: Vec<u32>,
    /// 本窗口内**所有**连续流片段收下的间隔。`gaps` 只跟踪当前片段（断流/平面关闭就
    /// 清零，避免把跨平面的巨大间隔算进分位数），但如果窗口报告只看剩下那一段，一个
    /// "平面开 3 秒后关闭"的窗口会报成 n=0——那 3 秒的真实卡顿数据就丢了。
    win: Vec<u32>,
}

/// BGRA → PPM(P6)，供人眼查看/自动化比对。
fn write_ppm(path: &str, f: &Frame) -> std::io::Result<()> {
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

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = arg(&args, "--url", "ws://127.0.0.1:7880");
    let room_name = arg(&args, "--room", "demo");
    let identity = arg(&args, "--identity", "view");
    let key = arg(&args, "--key", "devkey");
    let secret = arg(&args, "--secret", "secret");
    let seconds: u64 = arg(&args, "--seconds", "0").parse().unwrap_or(0);
    let dump = args
        .iter()
        .position(|a| a == "--dump")
        .and_then(|i| args.get(i + 1).cloned());
    let no_window = flag(&args, "--no-window");
    // 解复用声道数要跟发布端的混音格式一致（本机 48 kHz 立体声）。对不上会错位成
    // 刺耳噪声，所以留个开关；日志里首帧会回读实际值供核对。
    let audio_channels: i32 = arg(&args, "--audio-channels", "2").parse().unwrap_or(2);
    let audio_dump = args
        .iter()
        .position(|a| a == "--audio-dump")
        .and_then(|i| args.get(i + 1).cloned());
    let video_dump = args
        .iter()
        .position(|a| a == "--video-dump")
        .and_then(|i| args.get(i + 1).cloned());
    let video_seq = args
        .iter()
        .position(|a| a == "--video-seq")
        .and_then(|i| args.get(i + 1).cloned());
    if let Some(dir) = &video_seq {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("video-seq: {e}");
            std::process::exit(1);
        }
    }

    let tok = ess_transport::token(&key, &secret, &room_name, &identity);
    let ess_transport::Conn { room, mut events } = match ess_transport::connect(&url, &tok).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    println!("connected to {url} room={room_name} as {identity}");
    let lp = room.local_participant();

    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    // JPEG 放最后：选码器先试无损链，只有"省够 3 倍"才降级（见 `CodecSet::encode_by_cost`）。
    let jpeg = ess_core::codec::JpegCodec::default();
    let codecs = CodecSet { codecs: &[&pal, &png, &jpeg] };
    let mut recv = RecvPipeline::new(codecs);
    let mut asm = ess_transport::Assembler::default();

    // 观看端积压闸门。事件队列是无界的：一旦**应用速率 < 到达速率**（手机 CPU 只有
    // 桌面的几分之一，1080p 大面积变化时成片图块要解要画），历史就一帧帧攒在队列里
    // 按序播放 —— 表现为"时间轴一直漂移、延迟越来越大"。处理一条消息前先看它后面还
    // 排着多少条：超过阈值说明这条已经过时，非基线更新直接丢。缺的块由周期报告按
    // **当前**画面要回来，与发送端新鲜度闸门是同一个收敛论证（forced-plane 已证）。
    let backlog_keep: usize = std::env::var("ESS_VIEW_BACKLOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    // 慢客户端模拟：每应用一个图块忙等 N µs，用来在桌面上复现手机端的排队行为。
    let slow_us: u64 = std::env::var("ESS_VIEW_SLOW_US").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    // 第二个条件：解码占用。队列 momentarily 深几帧（一次突发）是能在下一帧追平的，
    // 这时候丢包只会画出一个可见的空洞；只有**持续**解码不过来（占用高）时丢历史才划算。
    // 实测教训：只看队列深度，桌面 30 fps 档出现 145 ms 空洞（本可 0 丢包）。
    let duty_max: f64 = std::env::var("ESS_VIEW_DUTY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.35);
    let mut view_skip: u64 = 0;
    let mut skip_last: u64 = 0;
    let mut inbox_peak: usize = 0;
    let mut applied_last: u64 = 0;
    let mut secs_last = 0u64;
    let mut duty_win_at = Instant::now();
    let mut duty_busy_us = 0u64;
    let mut duty = 0f64;

    let shared: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicBool::new(true));

    if !no_window {
        let shared = shared.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            let mut window: Option<minifb::Window> = None;
            let mut buf: Vec<u32> = Vec::new();
            let mut dims = (0usize, 0usize);
            while alive.load(Ordering::Relaxed) {
                let snap = shared.lock().unwrap().clone();
                if let Some(f) = snap {
                    let (w, h) = (f.w as usize, f.h as usize);
                    let need_new = window.is_none() || dims != (w, h);
                    if need_new {
                        let opts = minifb::WindowOptions {
                            resize: true,
                            scale: minifb::Scale::FitScreen,
                            ..Default::default()
                        };
                        match minifb::Window::new("ess-view", w, h, opts) {
                            Ok(win) => {
                                dims = (w, h);
                                window = Some(win);
                            }
                            Err(e) => {
                                eprintln!("window: {e}");
                                break;
                            }
                        }
                    }
                    buf.resize(w * h, 0);
                    for y in 0..h {
                        for x in 0..w {
                            let s = y * f.stride + x * 4;
                            let b = f.px[s] as u32;
                            let g = f.px[s + 1] as u32;
                            let r = f.px[s + 2] as u32;
                            buf[y * w + x] = (r << 16) | (g << 8) | b;
                        }
                    }
                    if let Some(win) = window.as_mut() {
                        if win.is_open() {
                            let _ = win.update_with_buffer(&buf, w, h);
                        } else {
                            break;
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
        });
    }

    let t0 = Instant::now();
    let mut last_report = Instant::now();
    // 发送端序列空洞：lossy 通路丢掉的那批更新在这里才看得见（与周期报告一起构成
    // "发现缺口 → 立刻上报"的那条早路）
    let mut seqg = ess_core::wire::SeqGaps::default();
    let mut gaps_reported = 0u64;
    // 因"看见新空洞"而提前上报的次数（早路真的动作过没有，只有这个数能证明）
    let mut early_reports = 0u64;
    let mut last_secs = 0u64;
    let audio_stat: Arc<Mutex<AudioStat>> = Arc::new(Mutex::new(AudioStat::default()));
    let audio_pcm: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let mut audio_blocks_last = 0u64;
    let video_stat: Arc<Mutex<VideoStat>> = Arc::new(Mutex::new(VideoStat::default()));
    let video_pending: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let mut video_frames_last = 0u64;
    // 已经起过解码环的视频轨。LiveKit 会把**上一轮发布者的僵尸轨**也一起交给新加入
    // 的观看端（实测：同一服务端上第二次开会话，观看端收到两条不同 sid 的订阅事件，
    // 前一条正是上一轮那条），两条环各拿一份帧，30 fps 就被数成 59.6。所以只认最新
    // 一条：每次订阅推进一代，旧环看到代次变了就自己退出。
    let mut video_sids: std::collections::HashSet<String> = Default::default();
    let video_gen: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    // 画面此刻归哪个平面。逐帧卡顿只有发生在**平面开着**的那段才是用户看到的洞：
    // 平面关着时观看端不画视频帧（图块通路在画），而发送端仍按静止节拍喂编码器
    // （~2 fps），那 500 ms 的间隔是喂帧节拍，不是卡顿——把它算成卡顿会把正确行为
    // 判成缺陷（实测一次 17 秒的跑里这样误报了 23 个"空洞"）。
    let plane_on: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    loop {
        tokio::select! {
            Some(ev) = events.recv() => {
                match &ev {
                    livekit::prelude::RoomEvent::DataReceived { payload, .. } => {
                        if let Some(msg) = ess_transport::decode_incoming(payload.as_ref(), &mut asm) {
                            if let WireMsg::VideoMode { on, .. } = &msg {
                                plane_on.store(*on, Ordering::Relaxed);
                            }
                            inbox_peak = inbox_peak.max(events.len());
                            // 序列记账必须在**脱落判定之前**：本机主动丢掉的 seq 我们也看见了，
                            // 它不该被算成链路空洞再去要回来（那正好把新鲜度闸门想省的东西又要
                            // 一遍，等于自造补账循环）。
                            if let Some(s) = msg.seq() {
                                seqg.note(s);
                            }
                            if events.len() > backlog_keep && duty > duty_max && !is_baseline_msg(&msg)
                            {
                                view_skip += 1;
                            } else {
                                let t = Instant::now();
                                let r = recv.on_wire(&msg);
                                if slow_us > 0 {
                                    let until = t + Duration::from_micros(slow_us * tile_count_of(&msg) as u64);
                                    while Instant::now() < until {
                                        std::hint::spin_loop();
                                    }
                                }
                                duty_busy_us += t.elapsed().as_micros() as u64;
                                if duty_win_at.elapsed() >= Duration::from_millis(500) {
                                    duty = duty_busy_us as f64 / duty_win_at.elapsed().as_micros() as f64;
                                    duty_win_at = Instant::now();
                                    duty_busy_us = 0;
                                }
                                if let Err(e) = r {
                                    eprintln!("decode: {e}");
                                } else if let Some(f) = canvas_snapshot(&recv) {
                                    *shared.lock().unwrap() = Some(f);
                                }
                            }
                        }
                    }
                    // 系统声音轨：解出 PCM 只做统计、不接扬声器——分享端和观看端同机
                    // 时播放会立刻形成回声环，本机验收要的是"收到多少块、有没有能量"。
                    livekit::prelude::RoomEvent::TrackSubscribed { track, .. } => {
                        if let livekit::prelude::RemoteTrack::Audio(rat) = track {
                            println!(
                                "audio track 已订阅：name={} source={:?}",
                                rat.name(),
                                rat.source()
                            );
                            let stat = audio_stat.clone();
                            let rtc = rat.rtc_track();
                            let dump = audio_dump.clone().is_some();
                            let pcm_buf = audio_pcm.clone();
                            tokio::spawn(async move {
                                use tokio_stream::StreamExt;
                                // Opus 在 WebRTC 里固定按 48 kHz 解；声道数错了会
                                // 交错错位，所以从解出来的帧里回读真实值打印一次。
                                let mut stream =
                                    livekit::webrtc::audio_stream::native::NativeAudioStream::new(
                                        rtc,
                                        48000,
                                        audio_channels,
                                    );
                                let mut reported = false;
                                while let Some(frame) = stream.next().await {
                                    if dump {
                                        pcm_buf.lock().unwrap().extend_from_slice(frame.data.as_ref());
                                    }
                                    let mut st = stat.lock().unwrap();
                                    st.blocks += 1;
                                    st.rate = frame.sample_rate;
                                    st.channels = frame.num_channels;
                                    for &s in frame.data.as_ref() {
                                        let v = s as f64 / 32767.0;
                                        st.sum_sq += v * v;
                                        st.samples += 1;
                                        if v.abs() > st.peak {
                                            st.peak = v.abs();
                                        }
                                    }
                                        if !reported {
                                        reported = true;
                                        println!(
                                            "audio 首帧：{} Hz {} 声道 {} 样本/声道",
                                            frame.sample_rate,
                                            frame.num_channels,
                                            frame.samples_per_channel
                                        );
                                    }
                                }
                            });
                        } else if let livekit::prelude::RemoteTrack::Video(rvt) = track {
                            if !video_sids.insert(rvt.sid().to_string()) {
                                eprintln!(
                                    "video track 重复订阅（sid={}），忽略：再起一条解码环会把同一条轨的帧数两遍",
                                    rvt.sid()
                                );
                                continue;
                            }
                            let my_gen = video_gen.fetch_add(1, Ordering::SeqCst) + 1;
                            println!(
                                "video track 已订阅：name={} source={:?} sid={}（第 {my_gen} 代解码环）",
                                rvt.name(),
                                rvt.source(),
                                rvt.sid()
                            );
                            let rtc = rvt.rtc_track();
                            let keep = video_dump.clone().is_some();
                            let seq = video_seq.clone();
                            let stat = video_stat.clone();
                            let pending = video_pending.clone();
                            let gen = video_gen.clone();
                            // 逐次卡顿轨迹：窗口百分位数说得清"有没有超合同"，说不清
                            // "洞落在哪一帧"——起步斜坡归因要靠这个
                            let trace = std::env::var("ESS_GAP_TRACE").is_ok();
                            let plane = plane_on.clone();
                            tokio::spawn(async move {
                                use tokio_stream::StreamExt;
                                let mut stream =
                                    livekit::webrtc::video_stream::native::NativeVideoStream::new(
                                        rtc,
                                    );
                                let mut last_at: Option<Instant> = None;
                                // 上一根帧解到时平面是否开着（交接那一拍不算卡顿，见下面）
                                let mut prev_streaming = false;
                                // 起步爬坡尺子：第 1 帧到第 31 帧应当是 30 个帧周期。
                                // 平均交付率把它摊平了看不出来（"头 5 秒 15 帧/s"到底是
                                // 慢两秒再满 3 秒，还是全程 15 帧，用户体感完全不同）。
                                let mut first_at: Option<Instant> = None;
                                // 平面开着期间解到的帧数（爬坡尺子的分母）
                                let mut ramp_n: u64 = 0;
                                while let Some(vf) = stream.next().await {
                                    // 有更新的轨订阅上了（上一轮的僵尸轨 + 本轮新轨会一起
                                    // 交给观看端），旧环继续跑只会把同一份画面数两遍。
                                    if gen.load(Ordering::Relaxed) != my_gen {
                                        println!("视频解码环退出：已被更新一代取代");
                                        break;
                                    }
                                    let at = Instant::now();
                                    let streaming = plane.load(Ordering::Relaxed);
                                    // 只统计"平面开着、帧在连续流"时的间隔。断流超过 500 ms
                                    // 说明这条流自己停了；平面关着时解到的帧根本不上画面
                                    // （发送端只是按静止节拍喂编码器保温），两种都不算卡顿。
                                    // 交接的那一拍也不算：前一根帧是"没在画"的帧，它到第一根
                                    // 在画的帧之间隔多久，观众看的都是无损画布（实测把这一段
                                    // 数成卡顿，会在正确的预热实现上误报 296–299 ms 的洞）。
                                    let mut gap_ms = None;
                                    if let Some(p) = last_at {
                                        let g = at.duration_since(p).as_millis() as u32;
                                        if g > 500 || !streaming || !prev_streaming {
                                            let mut st = stat.lock().unwrap();
                                            let run = std::mem::take(&mut st.gaps);
                                            st.win.extend(run);
                                        } else {
                                            gap_ms = Some(g);
                                        }
                                    }
                                    prev_streaming = streaming;
                                    last_at = Some(at);
                                    let (w, h) = (vf.buffer.width(), vf.buffer.height());
                                    if w == 0 || h == 0 {
                                        continue;
                                    }
                                    // VideoFormatType::ARGB 落到 libyuv 的 I420ToARGB：小端
                                    // 32 位 ARGB 字，内存里就是 B,G,R,A —— 正好是本仓库 Frame
                                    // 的 BGRA 排布。选错会红蓝互换，所以测量脚本带反向对照。
                                    let mut f = Frame::new(w, h, vf.timestamp_us.max(0) as u64);
                                    vf.buffer.to_argb(
                                        livekit::webrtc::video_frame::VideoFormatType::ARGB,
                                        &mut f.px,
                                        f.stride as u32,
                                        w as i32,
                                        h as i32,
                                    );
                                    let idx = {
                                        let mut st = stat.lock().unwrap();
                                        st.frames += 1;
                                        if let Some(g) = gap_ms {
                                            if trace && g >= 120 {
                                                println!(
                                                    "卡顿 第 {} 帧 间隔 {}ms（当前 {}x{}，平面内第 {} 帧 / 交接后 {}ms）",
                                                    st.frames,
                                                    g,
                                                    w,
                                                    h,
                                                    ramp_n + 1,
                                                    first_at.map_or(0, |t| at
                                                        .duration_since(t)
                                                        .as_millis() as u64)
                                                );
                                            }
                                            if st.gaps.len() < 8192 {
                                                st.gaps.push(g);
                                            }
                                        }
                                        st.w = w;
                                        st.h = h;
                                        if keep {
                                            st.latest = Some(f.clone());
                                        }
                                        st.frames
                                    };
                                    if trace && streaming {
                                        first_at.get_or_insert(at);
                                        ramp_n += 1;
                                        if ramp_n == 16 || ramp_n == 31 {
                                            println!(
                                                "爬坡 前 {ramp_n} 帧用了 {}ms（{}x{}）",
                                                    at.duration_since(first_at.unwrap_or(at))
                                                    .as_millis(),
                                                w,
                                                h
                                            );
                                        }
                                    }
                                    // 落盘丢给 blocking 线程，且只限制在飞的写任务数：写盘慢
                                    // 只能丢**文件**，不能把解码帧堵在 sink 队列里——那会让
                                    // 交付帧率假摔，而帧率正是要对标远控软件的那一项。
                                    if let Some(dir) = &seq {
                                        if pending.load(Ordering::Relaxed) < 16 {
                                            pending.fetch_add(1, Ordering::Relaxed);
                                            let f = f.clone();
                                            let dir = dir.clone();
                                            let pending = pending.clone();
                                            tokio::task::spawn_blocking(move || {
                                                let path = format!("{dir}/{idx:05}.ppm");
                                                if let Err(e) = write_ppm(&path, &f) {
                                                    eprintln!("video seq: {e}");
                                                }
                                                pending.fetch_sub(1, Ordering::Relaxed);
                                            });
                                        }
                                    }
                                }
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }

        // 周期报告（§6.4 兜底）：让发送端重发丢失块。
        //
        // 新看见一个链路空洞就立刻上报，不等下一个 500 ms 刻度：实测半屏照片类突变（255 块 /
        // 2.3 MB）约四分之三当秒就到齐，剩下那一截纯粹在等这份报告的往返——报告是几 KB 的上行
        // 可靠消息，早发不占下行带宽，也不动新鲜度判定（要回来的仍是发送端按**当前**画面算出的差集）。
        let periodic = last_report.elapsed() >= Duration::from_millis(500);
        if periodic || seqg.gaps > gaps_reported {
            if !periodic {
                early_reports += 1;
            }
            last_report = Instant::now();
            gaps_reported = seqg.gaps;
            // 周期时钟：等不齐的滚动组必须由它判死。不调用 ⇒ 画布不平移、报告要回来的块被组缓冲
            // 重新吞掉、版本却记成最新，现场读起来就是"零丢包、版本对、整条块行是旧的"。
            // 见 `RecvPipeline::tick`（机制与实测）与 `crates/ess-codec-palette/tests/recovery.rs`。
            recv.tick(t0.elapsed().as_millis() as u64);
            if let Message::Report { layout_id, versions, epochs } = recv.report() {
                let state = WireMsg::State {
                    layout_id,
                    cols: recv.rx.grid.cols,
                    rows: recv.rx.grid.rows,
                    versions,
                    epochs,
                };
                let mut id = 0;
                if let Err(e) = ess_transport::send_wire(&lp, &state, &mut id).await {
                    eprintln!("report: {e}");
                }
            }
        }

        let secs = t0.elapsed().as_secs();
        if secs != last_secs && secs % 5 == 0 {
            last_secs = secs;
            let st = *audio_stat.lock().unwrap();
            // 10 ms 一块 → 满速率 100 块/秒；按 5 秒窗口算实际到达率
            let rate = (st.blocks - audio_blocks_last) as f64 / 5.0;
            audio_blocks_last = st.blocks;
            let mut vs = video_stat.lock().unwrap();
            // 视频轨的交付帧率是合同项：平面开着的时候画布不动，观看端看到的
            // 流畅度完全由这个数决定，所以它和音频到达率一样要出现在周期报告里。
            let vrate = (vs.frames - video_frames_last) as f64 / 5.0;
            video_frames_last = vs.frames;
            let (vw, vh) = (vs.w, vs.h);
            // 间隔分位数按窗口取走（不是累计），报告的是"这 5 秒里真的在放视频时
            // 帧与帧之间隔了多久"。窗口平均会把平面没开的时间算进来，那是假的缺口。
            // 取的是 `win`（本窗口所有连续流片段的间隔）：当前片段先并进去，否则一个
            // "平面开 3 秒后关闭"的窗口会因为关闭时的清零而报成 n=0，白丢那 3 秒的数据。
            let run = std::mem::take(&mut vs.gaps);
            vs.win.extend(run);
            let mut gaps = std::mem::take(&mut vs.win);
            drop(vs);
            gaps.sort_unstable();
            let (g50, g95, gmax, gn) = if gaps.is_empty() {
                (0u32, 0u32, 0u32, 0u32)
            } else {
                let pct = |q: f64| gaps[((gaps.len() as f64 - 1.0) * q).round() as usize];
                (pct(0.50), pct(0.95), *gaps.last().unwrap(), gaps.len() as u32)
            };
            println!(
                "t={secs}s canvas={}x{} applied={} buffered={} scrolls={} fulls={} motion={} audio={}块/s rms={:.4} peak={:.2} {}Hzx{} video={}帧/s {}x{} 间隔p50={}ms p95={}ms max={}ms n={}",
                recv.canvas.w,
                recv.canvas.h,
                recv.stats.applied_tiles,
                recv.stats.buffered_tiles,
                recv.stats.scrolls_applied,
                recv.stats.full_frames,
                recv.stats.motion_frames,
                rate,
                st.rms(),
                st.peak,
                st.rate,
                st.channels,
                vrate,
                vw,
                vh,
                g50,
                g95,
                gmax,
                gn
            );
        }

        // 每秒一行积压读数：inbox 是"这条消息后面还排着多少条"的峰值，skip 是这一秒
        // 丢掉的过时更新数。延迟漂移的判据在这里，不在 5 秒汇总里。
        let s1 = t0.elapsed().as_secs();
        if s1 != secs_last {
            secs_last = s1;
            let applied = recv.stats.applied_tiles;
            println!(
                "VIEW-STATS t={s1}s inbox_peak={} skip={} duty={:.2} applied={} tile_rate={} gaps={} early={}",
                inbox_peak,
                view_skip - skip_last,
                duty,
                applied,
                applied - applied_last,
                seqg.gaps,
                early_reports
            );
            inbox_peak = 0;
            skip_last = view_skip;
            applied_last = applied;
        }

        if seconds > 0 && t0.elapsed() >= Duration::from_secs(seconds) {
            if let Some(path) = &dump {
                if let Some(f) = canvas_snapshot(&recv) {
                    match write_ppm(path, &f) {
                        Ok(()) => println!("dumped {path} ({}x{})", f.w, f.h),
                        Err(e) => eprintln!("dump: {e}"),
                    }
                }
            }
            if let Some(path) = &audio_dump {
                let pcm = std::mem::take(&mut *audio_pcm.lock().unwrap());
                let st = *audio_stat.lock().unwrap();
                let (rate, ch) = (
                    if st.rate > 0 { st.rate } else { 48000 },
                    if st.channels > 0 { st.channels as u16 } else { 2 },
                );
                match write_wav(path, &pcm, rate, ch) {
                    Ok(()) => println!(
                        "audio dumped {path} ({} 样本 = {:.1} s, {rate} Hz x {ch})",
                        pcm.len(),
                        pcm.len() as f64 / (rate as f64 * ch as f64)
                    ),
                    Err(e) => eprintln!("audio dump: {e}"),
                }
            }
            if let Some(path) = &video_dump {
                let vs = video_stat.lock().unwrap();
                match &vs.latest {
                    Some(f) => match write_ppm(path, f) {
                        Ok(()) => println!(
                            "video dumped {path} ({}x{}, 解出 {} 帧 = {:.1} fps)",
                            f.w,
                            f.h,
                            vs.frames,
                            vs.frames as f64 / t0.elapsed().as_secs_f64().max(1.0)
                        ),
                        Err(e) => eprintln!("video dump: {e}"),
                    },
                    None => println!("video dump 跳过：一条视频帧都没解出（平面从未开过？）"),
                }
            }
            if let Some(dir) = &video_seq {
                // 收尾等在飞的写任务：否则最后十几帧的 PPM 还没落盘进程就退了，
                // 序列长度对不上帧数，画质曲线的横轴就不可信。
                for _ in 0..100 {
                    if video_pending.load(Ordering::Relaxed) == 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let n = video_stat.lock().unwrap().frames;
                let written = std::fs::read_dir(dir).map(|rd| rd.count()).unwrap_or(0);
                println!("video seq 解出 {n} 帧，落盘 {written} 张 → {dir}");
            }
            alive.store(false, Ordering::Relaxed);
            let _ = room.close().await;
            return;
        }
    }
}

/// 基线类消息：丢了画布就永远缺一块，所以积压再深也必须处理。其余（普通逐块更新、
/// 运动帧）过期就丢——周期报告会按当前画面把缺的部分要回来。
fn is_baseline_msg(m: &WireMsg) -> bool {
    matches!(
        m,
        WireMsg::Full { .. } | WireMsg::Scroll { .. } | WireMsg::VideoMode { .. }
    ) || matches!(m, WireMsg::Update { repair: true, .. })
}

/// 一条消息要画多少块——慢客户端模拟用它折算忙等时间。一帧整屏 JPEG（运动通路）
/// 的解码+上屏代价约等于几百个小图块，不能按 1 计，否则放大不出手机端的负载。
fn tile_count_of(m: &WireMsg) -> usize {
    match m {
        WireMsg::Update { tiles, .. } => tiles.len(),
        WireMsg::Motion { .. } => 200,
        WireMsg::Full { .. } => 510,
        _ => 0,
    }
}

fn canvas_snapshot(recv: &RecvPipeline) -> Option<Frame> {
    if recv.canvas.w == 0 {
        None
    } else {
        Some(recv.canvas.clone())
    }
}
