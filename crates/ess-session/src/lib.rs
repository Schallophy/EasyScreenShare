//! 共享会话：采集 → 图块管线 → LiveKit 数据通道。
//!
//! CLI（`ess-share`）与 Tauri 外壳（`ess-share-ui`）共用这一条循环，避免两份
//! 实现漂移。调用方持有停止开关，通过回调拿到状态/统计/日志。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ess_capture::ScreenSource;
use ess_codec_palette::PaletteZstdCodec;
use ess_codec_png::PngCodec;
use ess_core::frame::Frame;
use ess_core::governor::{ContentClass, Governor, Obs, Step};
use ess_core::model::TileId;
use ess_core::pipeline::{CodecSet, SendPipeline};
use ess_core::wire::WireMsg;

mod video;
use livekit::options::{
    AudioEncoding, DegradationPreference, TrackPublishOptions, VideoCodec, VideoEncoding,
};
use livekit::prelude::{LocalTrack, LocalVideoTrack, TrackSource};
use livekit::webrtc::video_source::RtcVideoSource;
use video::VideoPlane;

/// 守门员阶梯 → 实际帧率上限。
///
/// 阶梯按内容类不同（Text 顶 15 / Video 顶 30），所以必须用**同一个比例**去缩放
/// 用户声明的 `fps_ceil`，否则一次降档后永远回不到用户要的那一档。入口与回升
/// 都必须走这里，不能各自算一套。
fn governed_fps(gov: &Governor, fps_ceil: u32) -> u32 {
    ((fps_ceil as u64 * gov.settings().fps as u64) / gov.top_fps().max(1) as u64)
        .clamp(1, fps_ceil.max(1) as u64) as u32
}

pub struct Config {
    pub url: String,
    pub room: String,
    pub identity: String,
    pub key: String,
    pub secret: String,
    pub fps: u32,
    pub demo: bool,
    /// 视频轨分辨率上限（None = 跟随采集分辨率，最高 4K）。画面糊时先降这里。
    pub video_max: Option<(u32, u32)>,
    /// 视频轨目标码率 bps（None = 按「像素 x 帧率 x 0.09 bpp」自动定档）。
    pub video_bitrate: Option<u64>,
    /// 是否共享系统声音（WASAPI 环回 → 独立音频轨）。
    pub audio: bool,
    /// §13.1「画质·流畅度」的产品档。取舍交给用户选，代码只定默认与每档的判据。
    pub mode: ProductMode,
}

/// 产品档位（§13.1）。三档对应三条**互相冲突**的合同，所以只能由用户选：
///
/// - [`ProductMode::Clear`] 清晰优先：宁可掉帧率也不糊字。无损平面优先，媒体通道
///   只在图块通路真装不下时进来，且进来后也不主动缩送入尺寸（编码器按
///   `MaintainResolution` 自己保分辨率）。
/// - [`ProductMode::Balanced`] 平衡（默认）：按链路需求决定要不要换平面，帧率是
///   合同，所以交付不足时主动缩送入尺寸。
/// - [`ProductMode::Smooth`] 流畅优先：帧率合同优先于一切——面积够大就走媒体通道
///   （不问无损装不装得下），码率按 0.15 bpp 给到"最差内容也够 30 帧"，交付不足
///   时**更快更深**地缩送入尺寸（分辨率就是这一档要花掉的代价）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ProductMode {
    Clear,
    #[default]
    Balanced,
    Smooth,
}

impl ProductMode {
    /// CLI/UI 传的字符串。中文名一并接受，因为表单里显示的就是中文。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "clear" | "清晰" | "清晰优先" => Some(Self::Clear),
            "balanced" | "平衡" => Some(Self::Balanced),
            "smooth" | "流畅" | "流畅优先" => Some(Self::Smooth),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Clear => "清晰优先",
            Self::Balanced => "平衡",
            Self::Smooth => "流畅优先",
        }
    }

    /// 自动码率档的 bpp 系数（×100）。实测（§14.4 结论 4，2026-10-31 重测）：
    /// 1280x720@30 给 2.49 Mbps（0.09 bpp）时编码器全程报带宽受限，靠把送入降到
    /// 960x540 才保住 30 帧；给 4.1 Mbps（0.15 bpp）时 720p 原尺寸、`ql_bw=0`、
    /// 交付 29.6–30.0 fps，重传为零。流畅优先档要的是后者。
    fn bpp_x100(&self) -> u64 {
        match self {
            Self::Smooth => 15,
            _ => 9,
        }
    }

    /// 自动档的码率上限：流畅优先允许更贵的自动档（4K/60 让编码器自己降尺寸）。
    fn bitrate_ceil(&self) -> u64 {
        match self {
            Self::Smooth => 16_000_000,
            _ => 12_000_000,
        }
    }

    /// 交付帧率低于目标多少算"缺"，以及连续缺多久才动手（步）。
    /// 平衡档实测稳态就在 29.6–30.4，80%/2 s 足够；流畅优先档要把"缺"当成必须
    /// 立刻消掉的东西，所以阈值抬到 95%、观察窗压到 1 s。清晰档不缩尺寸（返回 None）。
    fn fps_short(&self) -> Option<(f64, u32)> {
        match self {
            Self::Clear => None,
            Self::Balanced => Some((0.80, 2)),
            Self::Smooth => Some((0.95, 1)),
        }
    }

    /// 送入尺寸的下限：这一档愿意把画面缩到多小来换帧率。
    fn feed_floor(&self) -> f64 {
        match self {
            Self::Smooth => 0.25,
            _ => crate::video::MIN_FEED_SCALE,
        }
    }

    /// 视频平面这一段的送入尺寸起点（#41 的起步卡顿）。把请求码率折算成实际 bpp，低于
    /// 「运动内容保住帧率需要的那份」（= 流畅优先档的系数，实测那才是 720p30 不降
    /// 尺寸、`ql_bw=0` 的价）时，差额只能由像素付——那就别送原尺寸。
    /// 这个尺寸在会话开始和每次平面退出时预置（见帧循环的退出沿），不在进入瞬间改：
    /// 改尺寸本身要付一个关键帧 + 重配，那笔账必须花在平面关着的时候。
    ///
    /// 实测平衡档按 720p 原尺寸进平面：编码器头两秒只出 3/14 帧，发包排队 442 ms
    /// （一个原尺寸关键帧在当前码率下要 pacing 掉大半秒），之后它才自己降到 960x540
    /// 稳住 30 帧。观看端第 1 个窗口的帧间隔 p95=100 ms / max=281 ms 全在这两秒里，
    /// 而发送端每秒*平均*交付（15、30）抓不到它，所以控制器一次都没动手。
    ///
    /// 返回 `None` = 这一档不拿像素换帧率（清晰优先），进入就送声明尺寸。
    fn entry_scale(&self, bps: u64, w: u64, h: u64, fps: u64) -> Option<f64> {
        if self.fps_short().is_none() || w == 0 || h == 0 {
            return None;
        }
        // 用整数比而不是两次除法：自动码率本来就是「像素 × 帧率 × 系数」算出来的，
        // 流畅优先档传回的比例必须精确等于 1（差一个 ulp 就会被下面取整降成 0.95，
        // 那一档"原尺寸满帧率"的承诺当场破）。
        let need = Self::Smooth.bpp_x100().max(1);
        let num = bps as u128 * 100;
        let den = w as u128 * h as u128 * fps.max(1) as u128 * need as u128;
        if den == 0 {
            return None;
        }
        // 面积比 = 码率比开方（同帧率同内容）。向下取到 1/20 网格，之后由控制器的
        // ±0.25 步长按实测修正。网格用乘 20 取整再除回来，不写 `/ 0.05`：0.05 在
        // 二进制里不精确，比例正好是 1 时会算出 19.999…，把流畅优先档的"原尺寸满帧率"
        // 降成 0.95（1280x720 → 1216x684）。
        let est = (num as f64 / den as f64).sqrt();
        Some(((est * 20.0).floor() / 20.0).min(1.0))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: "ws://127.0.0.1:7880".into(),
            room: "demo".into(),
            identity: "share".into(),
            key: "devkey".into(),
            secret: "secret".into(),
            fps: 30,
            demo: false,
            video_max: None,
            video_bitrate: None,
            audio: true,
            mode: ProductMode::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub elapsed_s: u64,
    pub frames: u64,
    pub tiles: u64,
    pub msgs: u64,
    pub bytes: u64,
    pub updates: u64,
    pub fulls: u64,
    pub scrolls: u64,
    /// 视频平面已推送给编码器的帧数（0 = 从未进入视频模式）
    pub video_frames: u64,
    /// 当前是否由视频平面出画
    pub video_mode: bool,
    /// 数据通道可用（视频轨发布成功）：运动时走媒体通道，否则退回 JPEG
    pub video_track: bool,
    /// 系统声音轨发布成功
    pub audio_track: bool,
    /// 已喂进音频轨的 10 ms 块数（0 = 一路都没进去，用来识破"轨发了但没声音"）
    pub audio_frames: u64,
    /// 每秒一次的真实链路/编码统计（libwebrtc 的拥塞控制与编码器受限原因）
    pub link: Option<LinkStats>,
}

/// 从 libwebrtc 的 get_stats 里取出来的链路量——守门员的输入。
/// 这些数不是我们算的，是拥塞控制（GCC）和编码器自己报的，所以拿它做决策
/// 不需要再造一套估计器；我们负责的是**读到它并据此收/放图块通路的预算**。
#[derive(Clone, Copy, Debug, Default)]
pub struct LinkStats {
    /// 拥塞控制估计的可用上行带宽 bps（0 = 还没测出来）
    pub avail_bps: f64,
    /// 当前候选对 RTT，ms
    pub rtt_ms: f64,
    /// 观看端用 RTCP 回报告的丢包比例与往返时延——这才是"这一路观众的网络差"
    /// 的信号（候选对上的 rtt 在很多实现里一直是 0）。
    pub loss_fraction: f64,
    pub rx_rtt_ms: f64,
    /// 编码器当前目标码率 bps
    pub target_bps: f64,
    /// 编码器**实际**输出的分辨率与帧率——受限时会低于请求值
    pub enc_w: u32,
    pub enc_h: u32,
    pub enc_fps: f64,
    /// quality_limitation_reason == bandwidth
    pub bw_limited: bool,
    /// quality_limitation_reason == cpu
    pub cpu_limited: bool,
    /// 因受限而切分辨率的累计次数（单调递增）
    pub res_changes: u32,
    /// 编码器**每帧**平均耗时（区间增量），守门员的 over_time 判据
    pub enc_ms_frame: f64,
    /// 区间平均 QP（over_qp 判据）
    pub avg_qp: u8,
    /// 平均发包排队延迟 ms（hard 判据：>100 ms 立即降级）
    pub queue_ms: u32,
    /// 实测上行速率 bps（needed_bps）
    pub egress_bps: f64,
    /// 系统声音轨实际发出的速率 bps（DTX 是否真的在静音时收口，看这个数而不是看
    /// 接收端块数——接收端的音频 sink 空了也会持续交静音帧）
    pub audio_egress_bps: f64,
    /// 音频编码器自己报的目标码率（确认我们请求的档位真的进了编码器，而不是被忽略）
    pub audio_target_bps: f64,
    /// 音频实际发包速率 pkt/s（10 ms 一包 ≈ 100；算上包头才解释得了 kbps 为什么高于目标）
    pub audio_packets_ps: f64,
    /// 视频轨**真正发到线上**的帧率（frames_sent 区间增量）。和 `enc_fps`（编码器产出）
    /// 分开看才知道 30 帧的合同断在哪一层：编码器没产，还是产了没发出去。
    pub sent_fps: f64,
    /// 观看端 NACK 请求重传速率 pkt/s（有丢包时才有值；用它判断帧是不是被丢包吃掉）
    pub nack_ps: f64,
    /// quality_limitation_durations["bandwidth"] 的区间占比（1.0 = 这一秒全程被带宽压着）
    pub ql_bw: f64,
}

/// `read_link` 的上一轮累计量——stats 里的编码耗时、QP、发包延迟都是**单调累计**
/// 计数器，判据要的是区间增量，所以必须自己存上一轮。
#[derive(Default)]
struct LinkPrev {
    at: Option<Instant>,
    encode_s: f64,
    qp_sum: u64,
    frames: u64,
    delay_s: f64,
    packets: u64,
    bytes: u64,
    audio_bytes: u64,
    audio_packets: u64,
    sent_frames: u64,
    nack: u64,
    ql_bw_s: f64,
}

/// 读一次发布端的真实链路统计。`get_stats` 拿的是原始 WebRTC stats 列表，
/// 我们只挑视频轨那一条 outbound、transport 与候选对，其余（codec/来源/…）忽略。
async fn read_link(room: &livekit::Room, prev: &mut LinkPrev) -> LinkStats {
    use livekit::webrtc::stats::{QualityLimitationReason as Q, RtcStats};
    let mut out = LinkStats::default();
    let Ok(st) = room.get_stats().await else { return out };
    let dump = std::env::var("ESS_STATS_DUMP").is_ok();
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    // #57/#59：这个循环对**每一条** video OutboundRtp 都是"后者覆盖前者"，而观看端解出的
    // 尺寸与 `enc=` 报的在同一次跑里对不上（§14.7.20），怀疑同时存在不止一条 video 出站
    // 条目（重新发布留下的旧条目 / simulcast 层）。先把每条原样打出来，再决定取哪一条。
    let mut vid_dump = Vec::new();
    // 一次 get_stats 会交出十几条候选对，只有被提名且在用的那条带非零估计值，
    // 其余全是 0。按「提名 + 非零」打分取最优，不依赖 stats 列表的顺序。
    let mut pair_rank = -1i8;
    let (mut encode_s, mut qp_sum, mut frames, mut delay_s, mut packets, mut bytes) =
        (0f64, 0u64, 0u64, 0f64, 0u64, 0u64);
    let mut audio_bytes = 0u64;
    let mut audio_packets = 0u64;
    let mut sent_frames = 0u64;
    let mut nack = 0u64;
    let mut ql_bw = 0f64;
    for s in st.publisher_stats {
        if dump {
            *kinds.entry(stats_name(&s)).or_default() += 1;
        }
        match s {
            RtcStats::CandidatePair(cp) => {
                let p = &cp.candidate_pair;
                let rank = i8::from(p.nominated) * 2 + i8::from(p.available_outgoing_bitrate > 0.0);
                if rank > pair_rank
                    || (rank == pair_rank && p.available_outgoing_bitrate > out.avail_bps)
                {
                    pair_rank = rank;
                    out.avail_bps = p.available_outgoing_bitrate;
                    // W3C 里 currentRoundTripTime 的单位是秒
                    out.rtt_ms = p.current_round_trip_time * 1000.0;
                }
            }
            RtcStats::RemoteInboundRtp(ri) => {
                // 多观看端时取最坏的一条：自适应要按最差的那路观众来收。
                out.loss_fraction = out.loss_fraction.max(ri.remote_inbound.fraction_lost);
                out.rx_rtt_ms = out.rx_rtt_ms.max(ri.remote_inbound.round_trip_time * 1000.0);
            }
            RtcStats::Transport(tp) => {
                bytes = bytes.max(tp.transport.bytes_sent);
            }
            RtcStats::OutboundRtp(ob) if ob.stream.kind == "video" => {
                let o = &ob.outbound;
                out.target_bps = o.target_bitrate;
                out.enc_w = o.frame_width;
                out.enc_h = o.frame_height;
                out.enc_fps = o.frames_per_second;
                if dump {
                    vid_dump.push(format!(
                        "{}x{}@{:.1} tgt={:.2}Mbps qld_bw={:.2} reschg={} bytes={}",
                        o.frame_width,
                        o.frame_height,
                        o.frames_per_second,
                        o.target_bitrate / 1e6,
                        o.quality_limitation_durations
                            .get("bandwidth")
                            .copied()
                            .unwrap_or(0.0),
                        o.quality_limitation_resolution_changes,
                        ob.sent.bytes_sent
                    ));
                }
                out.res_changes = o.quality_limitation_resolution_changes;
                out.bw_limited = o.quality_limitation_reason == Q::Bandwidth;
                out.cpu_limited = o.quality_limitation_reason == Q::Cpu;
                encode_s += o.total_encode_time;
                qp_sum += o.qp_sum;
                frames += o.frames_encoded as u64;
                delay_s += o.total_packet_send_delay;
                packets += ob.sent.packets_sent;
                sent_frames += o.frames_sent as u64;
                nack += o.nack_count as u64;
                ql_bw += o.quality_limitation_durations.get("bandwidth").copied().unwrap_or(0.0);
            }
            RtcStats::OutboundRtp(ob) if ob.stream.kind == "audio" => {
                // 音频可能有多个 encoding 条目，按 kind 全部累加。DTX 到底有没有在静音
                // 时收口，只有这个字节数能说明——接收端的 sink 空了也照样交静音帧。
                audio_bytes += ob.sent.bytes_sent;
                audio_packets += ob.sent.packets_sent;
                out.audio_target_bps = out.audio_target_bps.max(ob.outbound.target_bitrate);
            }
            _ => {}
        }
    }
    if dump {
        eprintln!("STATS-DUMP publisher: {kinds:?}");
        if !vid_dump.is_empty() {
            eprintln!("STATS-DUMP video[{}]: {vid_dump:?}", vid_dump.len());
        }
    }
    let now = Instant::now();
    if let Some(t0) = prev.at {
        let dt = now.duration_since(t0).as_secs_f64();
        if dt > 0.05 {
            let d_frames = frames.saturating_sub(prev.frames);
            let d_packets = packets.saturating_sub(prev.packets);
            out.enc_ms_frame = (encode_s - prev.encode_s) * 1000.0 / d_frames.max(1) as f64;
            out.avg_qp = if d_frames > 0 {
                (qp_sum.saturating_sub(prev.qp_sum) / d_frames) as u8
            } else {
                0
            };
            out.queue_ms = if d_packets > 0 {
                ((delay_s - prev.delay_s) * 1000.0 / d_packets as f64) as u32
            } else {
                0
            };
            out.egress_bps = bytes.saturating_sub(prev.bytes) as f64 * 8.0 / dt;
            out.audio_egress_bps =
                audio_bytes.saturating_sub(prev.audio_bytes) as f64 * 8.0 / dt;
            out.audio_packets_ps = audio_packets.saturating_sub(prev.audio_packets) as f64 / dt;
            out.sent_fps = sent_frames.saturating_sub(prev.sent_frames) as f64 / dt;
            out.nack_ps = nack.saturating_sub(prev.nack) as f64 / dt;
            out.ql_bw = ((ql_bw - prev.ql_bw_s).max(0.0) / dt).min(1.0);
        }
    }
    *prev = LinkPrev {
        at: Some(now),
        encode_s,
        qp_sum,
        frames,
        delay_s,
        packets,
        bytes,
        audio_bytes,
        audio_packets,
        sent_frames,
        nack,
        ql_bw_s: ql_bw,
    };
    out
}

/// 统计条目类型名，只用于 ESS_STATS_DUMP 的诊断输出。
fn stats_name(s: &livekit::webrtc::stats::RtcStats) -> String {
    use livekit::webrtc::stats::RtcStats as R;
    let n = match s {
        R::Codec(_) => "codec",
        R::InboundRtp(_) => "inbound-rtp",
        R::OutboundRtp(_) => "outbound-rtp",
        R::RemoteInboundRtp(_) => "remote-inbound-rtp",
        R::RemoteOutboundRtp(_) => "remote-outbound-rtp",
        R::MediaSource(_) => "media-source",
        R::MediaPlayout(_) => "media-playout",
        R::PeerConnection(_) => "peer-connection",
        R::DataChannel(_) => "data-channel",
        R::Transport(_) => "transport",
        R::CandidatePair(_) => "candidate-pair",
        R::LocalCandidate(_) => "local-candidate",
        R::RemoteCandidate(_) => "remote-candidate",
        R::Certificate(_) => "certificate",
        R::Stream(_) => "stream",
        R::Track => "track",
    };
    match s {
        R::OutboundRtp(o) => format!("{n}({})", o.stream.kind),
        R::CandidatePair(c) => format!(
            "{n}(avail={:.0} rtt={:.4} nominated={} state={:?})",
            c.candidate_pair.available_outgoing_bitrate,
            c.candidate_pair.current_round_trip_time,
            c.candidate_pair.nominated,
            c.candidate_pair.state
        ),
        R::RemoteInboundRtp(r) => format!(
            "{n}(rtt={:.4} lost={:.3})",
            r.remote_inbound.round_trip_time, r.remote_inbound.fraction_lost
        ),
        R::DataChannel(d) => format!("{n}(label={} bytes={})", d.dc.label, d.dc.bytes_sent),
        _ => n.to_string(),
    }
}

pub enum Event {
    Connected { width: u32, height: u32 },
    Stats(Stats),
    Log(String),
    Error(String),
}

/// 合成屏动画时长（10 fps 节拍数）：默认 3 s 后静止，验证视频平面时要拉长。
fn demo_frames() -> u32 {
    std::env::var("ESS_DEMO_ANIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// 合成屏源（`ESS_DEMO_FLASH=1` 时带墙钟对齐的白闪，用于延迟测量）
/// 合成屏尺寸。默认 1280x720；`ESS_DEMO_W/H` 可以拉到 1920x1080。
/// 性能必须在**用户的真实工作点**上量：无损差分是逐块拷贝比对，代价随像素数线性涨
/// （720p 每帧 ~10 ms，1080p ~33 ms），拿 720p 的读数说"30 fps 达标"是系统性乐观。
fn demo_dims() -> (u32, u32) {
    let n = |k: &str, d: u32| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    (n("ESS_DEMO_W", 1280), n("ESS_DEMO_H", 720))
}

fn synthetic(w: u32, h: u32) -> ess_capture::SyntheticScreen {
    ess_capture::SyntheticScreen {
        w,
        h,
        animate_frames: demo_frames(),
        // 静止期长度：平面切换的验收要"先静止若干秒再动"，冷启动不是切换
        animate_delay: std::env::var("ESS_DEMO_ANIM_DELAY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        flash: std::env::var("ESS_DEMO_FLASH").is_ok(),
        // ESS_DEMO_BLOCK=0 → 纯文本滚动（走滚动宣告，无损够便宜就不切平面）；
        // 默认保留那块移动红块，它是"滚动 + 局部动画"的混合内容，该走媒体通道。
        block: std::env::var("ESS_DEMO_BLOCK").map(|v| v != "0").unwrap_or(true),
        // ESS_DEMO_VIDEO=1 → "视频窗口 + 弹幕"：大面积、非平移、每帧都变。滚动型内容
        // 对无损通路近乎免费（一条 Scroll 宣告），拿它做性能验收会系统性乐观。
        video_like: std::env::var("ESS_DEMO_VIDEO").map(|v| v != "0").unwrap_or(false),
        // ESS_DEMO_STEP=N → 第 N 帧把左半屏一次性改掉（配 ESS_DEMO_ANIM=0 用）。
        // 测「一次大面积变化到底丢多少」，别让渐进动画把丢包伪装成慢。
        step_at: std::env::var("ESS_DEMO_STEP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    }
}

/// 一批待发的外发消息 + 它的年龄起点。
struct Batch {
    at: Instant,
    /// true = 建基线/宣告类（整帧、滚动宣告、平面开关、运动退出的精确修复）：
    /// 过期也必须发，否则观看端画布永远缺一块。其余（普通逐块更新、JPEG 运动帧）
    /// 过期就丢——观看端的周期报告会把缺的块按**当前**画面要回去，那才是新内容。
    baseline: bool,
    msgs: Vec<WireMsg>,
}

fn is_baseline(m: &WireMsg) -> bool {
    matches!(
        m,
        WireMsg::Full { .. } | WireMsg::Scroll { .. } | WireMsg::VideoMode { .. }
    ) || matches!(m, WireMsg::Update { repair: true, .. })
}

/// 线消息的字节量，只给测量台的限速桶用（`ESS_EGRESS_CAP`）。估算不必精确到协议头：
/// 偏一点只会让"人为链路"略宽或略窄，不影响判定方向。
fn wire_bytes(m: &WireMsg) -> usize {
    match m {
        WireMsg::Update { tiles, .. } => tiles.iter().map(|t| t.payload.len() + 12).sum(),
        WireMsg::Full { payloads, .. } => payloads.iter().map(|p| p.len() + 8).sum(),
        WireMsg::Motion { data, .. } => data.len() + 16,
        WireMsg::Scroll { targets, dirty, .. } => targets.len() * 12 + dirty.len() * 4 + 16,
        _ => 64,
    }
}

/// 入队发送。基线类必须排队（等队列腾地方）；逐块更新满了就地丢，丢的是
/// "来不及发的旧画面"。真正的年龄闸门在发送任务里（见 `fresh`）。
/// 一个批次里涉及哪些块（只有无损更新带块 id）。脱落时把这份 id 交给管线，补账才有资格
/// 按块补而不是按整帧兑现。
fn batch_tile_ids(msgs: &[WireMsg]) -> Vec<TileId> {
    let mut out = Vec::new();
    for m in msgs {
        if let WireMsg::Update { tiles, .. } = m {
            out.extend(tiles.iter().map(|t| t.id));
        }
    }
    out
}

/// 脱落账本：掉的是哪些块（`ids`），以及**有没有掉过我们叫不出块 id 的内容**（`unknown`，
/// 例如 JPEG 运动帧）。`unknown` 一旦为真，补账必须回到整帧兑现——实测把这条判据放宽成
/// "只要有些 id 就按块补"，`motion-stays-lossless` 的"画布逐字节等于期望终帧"当场挂。
#[derive(Default)]
struct LossAccount {
    ids: Vec<TileId>,
    unknown: bool,
}

/// 把一次脱落并进账本。有块 id 的（Update 批次）按块记；没有 id 可记的内容直接把账标成
/// 不明，让整帧兜底那条路保留。
fn merge_lost(v: &mut LossAccount, ids: Vec<TileId>) {
    if ids.is_empty() {
        v.unknown = true;
        return;
    }
    let fresh: Vec<TileId> = ids.into_iter().filter(|id| !v.ids.contains(id)).collect();
    v.ids.extend(fresh);
}

async fn enqueue(
    tx: &tokio::sync::mpsc::Sender<Batch>,
    dropped: &std::sync::atomic::AtomicU64,
    lost_ids: &std::sync::Mutex<LossAccount>,
    msgs: Vec<WireMsg>,
) {
    // 空批次直接丢掉，不入队。静止画面每帧都产出一个空批次，它们在队列里 aging 时会被
    // 新鲜度闸门记成「脱落」，而脱落累计会去设 `loss_claimed` —— 于是观看端一份「我全对上
    // 了」的报告就被解读成「提交即记账把缺口掩盖了」，每 1.5 s 补一个整帧重建。实测
    // 1080p 半屏单帧突变 + 之后完全静止：`stale` 0→68、`full` 1→4、`tile_bytes`
    // 2.45→11.57 MB，其中 9 MB 是这么白花掉的。这里也让 `q`/`lag` 回到「只在真有东西
    // 要发时才读数」的本义。
    if msgs.is_empty() {
        return;
    }
    let baseline = msgs.iter().any(is_baseline);
    let ids = batch_tile_ids(&msgs);
    let b = Batch { at: Instant::now(), baseline, msgs };
    let ok = if baseline { tx.send(b).await.is_ok() } else { tx.try_send(b).is_ok() };
    if !ok {
        dropped.fetch_add(1, Ordering::Relaxed);
        if !ids.is_empty() {
            if let Ok(mut v) = lost_ids.lock() {
                merge_lost(&mut v, ids);
            }
        }
    }
}
/// 创建采集源。返回 (源, 通路说明)，说明直接进日志，免得线上出问题时无从下手。
///
/// 通路选择：`ESS_CAPTURE=gdi|dxgi` 显式指定；没指定时自动挑。
/// 原来的唯一自动理由是**帧率**（实测本机持续重绘负载下 GDI 在 40/50 fps 精确达标、设 60 只能
/// 给到 49–55，桌面复制稳定 60–61）。2026-11-01 加了第二条：**抖动**。
/// `scripts/latency-ab.sh` 一次开机内交替配对测（30 fps、同一支墙钟对齐白闪、同一观看窗，
/// 共 4 对 8 个样本）：**均值两臂测不出差别**（dxgi p50 94/106/108/95，gdi 97/97/110/87 ms，
/// 差值在 ±10 ms 的跑间噪声里；先前"dxgi 少 15 ms"是跨开机漂移造的假象），但**同一跑内
/// 边沿的散布** desktop duplication 四跑是 1/1/13/1 ms、GDI 四跑是 12/6/7/6 ms：GDI 的轮询
/// 与画面内容不相位，采样时刻在帧周期里随机落。共享里"每帧都差不多快"比"平均快一点"更接近
/// 用户说的不卡，所以单显示器默认走桌面复制；这条不是均值收益，别当成延迟优化引用。
/// 只在**单显示器**时为了抖动切：桌面复制只复制主输出，多屏会采错屏（`monitor_count()`，
/// 副屏见 #30）。指针不是障碍——桌面纹理里确实没有指针，但 `DupCapture` 已用
/// `GetFramePointerShape` 合成，活体测试 `dxgi::live_tests::duplication_paints_pointer` 是绿的。
/// 那条测试**必须 `--test-threads=1` 单独跑**：同一个进程里开两路复制会让第二路
/// `DupCapture::new()` 失败，2026-11-01 我就是把这条竞态误读成"指针没合成"而白回退了一次
/// 默认（§14.7.19）。
#[cfg(windows)]
fn make_source(demo: bool, fps: u32) -> Result<(Box<dyn ScreenSource>, String), String> {
    if demo {
        let (dw, dh) = demo_dims();
        return Ok((Box::new(synthetic(dw, dh)), "合成屏".into()));
    }
    let want = match std::env::var("ESS_CAPTURE").as_deref() {
        Ok("dxgi") | Ok("gdi") => Some(std::env::var("ESS_CAPTURE").unwrap()),
        _ => None,
    };
    let fast_reason = fps > 50;
    let auto = if fast_reason || ess_capture::monitor_count() <= 1 { "dxgi" } else { "gdi" };
    let mode = want.clone().unwrap_or_else(|| auto.to_string());
    if mode == "dxgi" {
        match ess_capture::create("dxgi") {
            Ok(src) => {
                let why = match &want {
                    Some(_) => "ESS_CAPTURE=dxgi".to_string(),
                    None if fast_reason => format!("自动：设定 {fps} fps 超过 GDI 的 49–55 fps 实际上限"),
                    None => format!(
                        "自动：单显示器，同跑内边沿散布桌面复制 1–2ms（GDI 6–12ms），均值无可测差别（{fps} fps）"
                    ),
                };
                return Ok((src, format!("dxgi 桌面复制（{why}）")));
            }
            Err(e) => {
                // 桌面复制会在输出被拔掉/改分辨率/无 GPU 会话里失效；回落 GDI 而不是让会话起不来
                let src = ess_capture::create("gdi")?;
                return Ok((src, format!("GDI BitBlt（dxgi 不可用: {e}）")));
            }
        }
    }
    Ok((ess_capture::create("gdi")?, "GDI BitBlt".into()))
}

#[cfg(not(windows))]
fn make_source(demo: bool, _fps: u32) -> Result<(Box<dyn ScreenSource>, String), String> {
    if demo {
        let (dw, dh) = demo_dims();
        Ok((Box::new(synthetic(dw, dh)), "合成屏".into()))
    } else {
        Err("capture not implemented for this platform".into())
    }
}

/// 事件循环 → 主循环的恢复请求。
#[derive(Clone, Copy, Debug)]
enum Recovery {
    /// 新观看方加入：下一帧发整帧，补齐它错过的历史
    Full,
    /// SDK 重连成功：数据通道是全新的，断线期间的更新（无论可靠与否）都回不来，
    /// 观看端画布停在断线前那一刻。只有整帧重建才能保证不出现旧画面。
    Reconnected,
}

/// 系统声音：WASAPI 环回 → LiveKit 音频轨（`ScreenshareAudio`，浏览器会把它和共享
/// 画面当成同一路内容）。返回一句可以直接进日志的格式描述。
///
/// 环回线程给的是 `std::sync::mpsc` 的 10 ms 块，喂给 WebRTC 要 await，所以中间垫一层
/// 有界 tokio 通道（50 块 = 500 ms）。满了**丢最新一块**而不是继续排队：实时音频宁可
/// 漏 10 ms，也不能让延迟一路往上堆到秒级。
async fn publish_audio(
    lp: &livekit::prelude::LocalParticipant,
    stop: Arc<AtomicBool>,
    count: Arc<std::sync::atomic::AtomicU64>,
) -> Result<String, String> {
    use livekit::prelude::{LocalAudioTrack, RtcAudioSource};
    use livekit::webrtc::audio_frame::AudioFrame;
    use livekit::webrtc::audio_source::native::NativeAudioSource;
    use livekit::webrtc::audio_source::AudioSourceOptions;
    use std::borrow::Cow;

    let (fmt, srx) = ess_audio::start(stop.clone())?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ess_audio::Chunk>(50);
    let stop_fwd = stop.clone();
    tokio::task::spawn_blocking(move || {
        while let Ok(chunk) = srx.recv() {
            if stop_fwd.load(Ordering::Relaxed) {
                break;
            }
            let _ = tx.try_send(chunk);
        }
    });

    // 环回拿到的已经是混音结果，再挂 AEC/降噪/AGC 只会把音乐和音效糊掉
    let src = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        },
        fmt.sample_rate,
        fmt.channels as u32,
        500,
    );
    // 立体声 64 kbps 是 Opus 音乐档的可用下限，也是主流远控软件"共享电脑声音"的量级；
    // 单声道 48 kbps。再高就要和视频轨抢带宽了。ESS_AUDIO_BITRATE 给测量和窄链路留口子。
    let bitrate: u64 = std::env::var("ESS_AUDIO_BITRATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(if fmt.channels > 1 { 64_000 } else { 48_000 });
    let track = LocalAudioTrack::create_audio_track("ess-audio", RtcAudioSource::Native(src.clone()));
    let opts = TrackPublishOptions {
        source: TrackSource::ScreenshareAudio,
        audio_encoding: Some(AudioEncoding { max_bitrate: bitrate }),
        // 不静音不发：环回抓的是默认输出端点，绝大多数时候它是安静的，而固定码率的
        // Opus 会把这段安静照样推出去。屏幕分享的画面带宽比这段静音值钱得多。
        // 实测（scripts/audio-dtx.sh，48 kHz 立体声）：静音 0.2 kbps vs 关 DTX 4.5 kbps，
        // 有声音时 131 kbps——正好是目标码率的两倍，因为默认 red:true 会在每个包里
        // 带上前一帧的冗余副本（丢一个包也听得懂）。这 131 kbps 只走一次上行，
        // 与观看人数无关（见扇出验收），所以留着冗余比省这一半带宽划算。
        // ESS_AUDIO_DTX=0 留着做 A/B 测量（只有关掉才看得出它到底省了多少）。
        dtx: std::env::var("ESS_AUDIO_DTX").map(|v| v != "0").unwrap_or(true),
        ..Default::default()
    };
    lp.publish_track(LocalTrack::Audio(track), opts)
        .await
        .map_err(|e| e.to_string())?;

    tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let frame = AudioFrame {
                data: Cow::Owned(chunk.pcm),
                sample_rate: fmt.sample_rate,
                num_channels: fmt.channels as u32,
                samples_per_channel: chunk.samples_per_channel as u32,
            };
            if src.capture_frame(&frame).await.is_err() {
                break;
            }
            count.fetch_add(1, Ordering::Relaxed);
        }
    });

    Ok(format!(
        "{} Hz {} 声道 · Opus {} kbps · DTX（WASAPI 环回）",
        fmt.sample_rate,
        fmt.channels,
        bitrate / 1000
    ))
}

/// 跑一次共享会话，直到 `stop` 置位或发生不可恢复错误。
pub async fn run(
    cfg: Config,
    stop: Arc<AtomicBool>,
    mut emit: impl FnMut(Event),
) -> Result<(), String> {
    let (mut source, path) = make_source(cfg.demo, cfg.fps)?;
    let (w, h) = source.dims();

    let pal = PaletteZstdCodec::default();
    let png = PngCodec;
    // 发送端选码：照片类块省够 3 倍才降级到 JPEG（实测 4.6×），文字块留无损。
    let jpeg = ess_core::codec::JpegCodec::default();
    let codecs = CodecSet { codecs: &[&pal, &png, &jpeg] };

    emit(Event::Log(format!(
        "采集源 {w}x{h}（{path}），{} fps",
        cfg.fps.max(1)
    )));
    let tok = ess_transport::token(&cfg.key, &cfg.secret, &cfg.room, &cfg.identity);
    let ess_transport::Conn { room: room_obj, mut events } = ess_transport::connect(&cfg.url, &tok)
        .await
        .map_err(|e| {
            emit(Event::Error(e.clone()));
            e
        })?;
    emit(Event::Connected { width: w, height: h });
    emit(Event::Log(format!("已连接 {} 房间={}", cfg.url, cfg.room)));

    // 帧率是承诺：GDI 通路先量一次自己的节拍。注意这个数随画面变化率浮动（本机静止
    // 约 31.5、持续重绘 82），只报数不报警——真正的判定要在画面正在变化却产不满帧
    // 时做。桌面复制不阻塞（没新帧就复用上一帧），拿它测只会得到循环频率（296），
    // 没有意义，所以只对 GDI 跑这个探针。
    if !cfg.demo && path.starts_with("GDI") {
        let probe_n = 15u32;
        let t0 = Instant::now();
        for i in 0..probe_n {
            let f = source.frame(i as u64);
            std::hint::black_box(&f.px);
        }
        let rate = probe_n as f64 / t0.elapsed().as_secs_f64();
        // 静止桌面上 BitBlt 会等合成，这个数是「空闲节奏」而不是上限：本机实测
        // 静止 31.5 fps、内容持续重绘时 82 fps。所以只报数，不据此报警——
        // 真正的判定要在画面正在变化却产不满帧时做。
        emit(Event::Log(format!(
            "GDI 抓屏节拍本次实测 {rate:.1} fps（随画面变化率浮动：静止约 31.5，持续重绘 82）"
        )));
    }
    let lp = room_obj.local_participant();

    // 视频平面：大面积运动时画面走媒体通道（VP8 屏幕共享），数据通道只管图块。
    // 发布失败不影响会话——决策器会退回 JPEG 运动帧通路。
    let video_on = std::env::var("ESS_VIDEO").map(|v| v != "0").unwrap_or(true);
    let mode = cfg.mode;
    let mut video: Option<VideoPlane> = None;
    // 下一次进入视频平面时的送入尺寸（见 `ProductMode::entry_scale`）。会话内会随
    // 实测修正：平面退出时没被带宽压着，就记住那个尺寸发得动。
    let mut entry_scale: Option<f64> = None;
    if video_on {
        // 默认不再硬压到 720p：视频轨按采集分辨率出画（上限 4K），码率按
        // 「像素 × 帧率 × 0.09 bpp」自动定档（推导见下面 auto_bps 处的实测记录）。
        // 优先级：调用方给的 Config 字段 > 环境变量（脚本/CLI 兼容）> 自动档
        let env_num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        let max_w = cfg
            .video_max
            .map(|v| v.0)
            .or_else(|| env_num("ESS_VIDEO_MAX_W").map(|n| n as u32))
            .unwrap_or_else(|| w.min(3840));
        let max_h = cfg
            .video_max
            .map(|v| v.1)
            .or_else(|| env_num("ESS_VIDEO_MAX_H").map(|n| n as u32))
            .unwrap_or_else(|| h.min(2160));
        let bitrate_env = cfg.video_bitrate.or_else(|| env_num("ESS_VIDEO_BITRATE"));
        // 编码器档位实测见 docs/APP.md：VP8 +screencast 只有 6 fps 呈现（屏幕共享
        // 码控在压帧率），H264 +screencast 才是现默认。
        let screencast = std::env::var("ESS_VIDEO_SCREENCAST").map(|v| v != "0").unwrap_or(true);
        let codec = match std::env::var("ESS_VIDEO_CODEC").as_deref() {
            Ok("vp8") => VideoCodec::VP8,
            Ok("vp9") => VideoCodec::VP9,
            Ok("av1") => VideoCodec::AV1,
            _ => VideoCodec::H264,
        };
        let (mut plane, src) = VideoPlane::new(w, h, max_w, max_h, screencast);
        // 这一档愿意把送入缩到多小来换帧率（清晰/平衡档仍是 0.4 的通用下限）
        plane.set_feed_floor(mode.feed_floor());
        let (vw, vh) = plane.dims();
        // 码率自动档的来历（最坏内容：全屏逐帧随机色重绘，1080p60，H264+screencast，
        // `scripts/bitrate-ladder.sh`）：
        //    2 Mbps → 编码器把分辨率降到 1280x720 来保帧率（MaintainFramerate 生效），
        //             发送端仍 61 fps，浏览器只呈现 38 fps
        //    4 Mbps → 1080p，呈现 53 fps
        //    6 Mbps → 1080p，呈现 60 fps（最坏内容下丢帧 12%）
        //   10 Mbps → 1080p，呈现 60 fps，丢帧回落 5%
        // 真实桌面内容比这好压一个量级，所以按「像素 × 帧率 × bpp」定档。bpp 系数
        // 由产品档决定（见 `ProductMode::bpp_x100`）：0.09 是"无损装不下才用媒体通道"
        // 时的够用值，实测会让编码器整程报带宽受限；0.15 是"帧率优先"的够用值，
        // 1280x720@30 实测 4.1 Mbps、原尺寸不降、交付 30、重传为零。
        let auto_bps = (vw as u64
            * vh as u64
            * cfg.fps.max(1) as u64
            * mode.bpp_x100()
            / 100)
            .clamp(1_500_000, mode.bitrate_ceil());
        let bitrate = bitrate_env.unwrap_or(auto_bps);
        entry_scale = mode.entry_scale(bitrate, vw as u64, vh as u64, cfg.fps.max(1) as u64);
        // 一开场就把送入尺寸预置好：尺寸变化会让编码器重配并补一个关键帧，这个开销
        // 必须花在平面关着的时候（观看端此时根本不收视频帧），见帧循环里的退出沿。
        if let Some(s) = entry_scale {
            plane.set_feed_scale(s);
        }
        let bitrate_how = if cfg.video_bitrate.is_some() {
            "调用方指定"
        } else if bitrate_env.is_some() {
            "ESS_VIDEO_BITRATE"
        } else {
            "自动"
        };
        video = Some(plane);
        let track = LocalVideoTrack::create_video_track("ess-screen", RtcVideoSource::Native(src));
        let opts = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: codec,
            simulcast: false,
            // 帧率是不是合同由档位说：清晰优先档带宽不足时保分辨率（宁可掉帧也不糊字），
            // 其余两档降分辨率保帧率。
            degradation_preference: Some(match mode {
                ProductMode::Clear => DegradationPreference::MaintainResolution,
                _ => DegradationPreference::MaintainFramerate,
            }),
            video_encoding: Some(VideoEncoding {
                max_bitrate: bitrate,
                max_framerate: cfg.fps.max(1) as f64,
            }),
            ..Default::default()
        };
        match lp.publish_track(LocalTrack::Video(track), opts).await {
            Ok(_) => emit(Event::Log(format!(
                "视频轨已发布：{vw}x{vh} {:?} {} bps（{bitrate_how}，{} 档 {} bpp）screencast={screencast}",
                codec,
                bitrate,
                mode.label(),
                mode.bpp_x100() as f64 / 100.0
            ))),
            Err(e) => {
                video = None;
                emit(Event::Log(format!("视频轨发布失败（退回 JPEG 运动帧）: {e}")));
            }
        }
    }
    let video_track = video.is_some();
    // 系统声音轨。失败不影响画面（比如输出端点被拔/无音频设备），只少一路内容。
    let audio_cnt = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let audio_track = if !cfg.audio {
        emit(Event::Log("系统声音：调用方关闭".into()));
        false
    } else {
        match publish_audio(&lp, stop.clone(), audio_cnt.clone()).await {
            Ok(desc) => {
                emit(Event::Log(format!("系统声音已共享：{desc}")));
                true
            }
            Err(e) => {
                emit(Event::Log(format!("系统声音不可用（只共享画面）: {e}")));
                false
            }
        }
    };
    // 静止期喂编码器的节拍（~2 fps）；必须在 cfg 被采集线程移走之前算好
    let idle_feed_every = (cfg.fps.max(1) / 2).max(1) as u64;
    // 测量台用：覆盖静止期喂帧节拍（1 = 平面关着也按声明帧率喂）。用来验
    // "起步斜坡是不是编码器的输入帧率估计被 2 fps 校准坏了"这个假设。
    let idle_feed_every = std::env::var("ESS_IDLE_FEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(idle_feed_every)
        .max(1);
    // 用户设定的帧率是**上限**；守门员只能在这个上限往下调，不能往上。
    let fps_ceil = cfg.fps.max(1);
    let fps_ctl = Arc::new(std::sync::atomic::AtomicU32::new(fps_ceil));
    // 视频平面交付帧率缺口的连续计数：连缺两步才降送入尺寸，连富四步才回升，
    // 免得在边界上一档一档地抖（每次改尺寸编码器都要补一个关键帧）。
    let mut fps_low_streak = 0u32;
    let mut fps_high_streak = 0u32;
    // 上一秒喂给编码器的累计帧数（用来区分"真的喂了满帧率但没交付"和"本来就没在喂"）
    let mut push_last = 0u64;
    // 帧率控制器按 1 s 窗评估：观测拍子是 250 ms，这里攒四个样本再判一次
    let mut ctl_at = Instant::now();
    let mut sent_acc = 0f64;
    let mut sent_n = 0u32;
    // 预热状态机自己的 1 s 证据窗。它跟上面的尺寸控制器分开，因为**清晰优先档不做尺寸
    // 控制**（`fps_short()=None`），但那一档同样要等"交付达标 + 编码器尺寸安顿下来"才把
    // 画面交出去——否则预热只能靠 6 s 上限兜底，白顶一段无损。
    let mut warm_at = Instant::now();
    let mut warm_acc = 0f64;
    let mut warm_n = 0u32;

    // 采集→会话：**单槽"最新赢"信箱**，不是队列。原来的 mpsc(cap 2) + `blocking_send`
    // 注释写着"满则丢旧帧"，代码却按序阻塞——会话循环一慢，采集就排队等着，观看端
    // 收到的就是按序播放的旧画面（延迟单调增长的来源之一）。信箱只留最新一帧：
    // 循环落后时旧帧直接被覆盖，无损差分本来就是对"观看端手里那一帧"做的
    // （`state.last_sent`），跳过中间的帧不丢任何内容。
    let mail: Arc<std::sync::Mutex<Option<(std::time::Instant, Frame)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let (tick_tx, mut tick_rx) = tokio::sync::watch::channel(0u64);
    let stop_cap = stop.clone();
    let fps_cap = fps_ctl.clone();
    let cap_mail = mail.clone();
    let capture = std::thread::spawn(move || {
        let t0 = Instant::now();
        // 排程用"下一次截止时刻"累加而不是 t0 + period*n：帧率被守门员改动时
        // 不需要重算基准，也不会因为改值瞬间跳一大段。绝对截止点保证采集自身
        // 耗时不会把节奏越拖越慢（30 fps 硬指标）。
        let mut next = t0;
        while !stop_cap.load(Ordering::Relaxed) {
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
            let ts = t0.elapsed().as_micros() as u64;
            let f = source.frame(ts);
            // 覆盖式投递：消费端跟不上时丢的是**更旧**的那一帧，不是最新的一帧
            *cap_mail.lock().unwrap() = Some((Instant::now(), f));
            if tick_tx.send(ts).is_err() {
                break;
            }
            let step = Duration::from_micros(1_000_000 / fps_cap.load(Ordering::Relaxed).max(1) as u64);
            next += step;
            if next < Instant::now() {
                // 落后太多（卡顿/改帧率）：丢掉错过的节拍，重新对齐而不是补偿式狂奔
                next = Instant::now() + step;
            }
        }
    });

    // 上行报告 & 新观看方加入 & 连接生命周期
    let (report_tx, mut report_rx) =
        tokio::sync::mpsc::channel::<(u32, Vec<u32>, Vec<u32>, u64)>(8);
    let (force_tx, mut force_rx) = tokio::sync::mpsc::channel::<Recovery>(8);
    let (log_tx, mut log_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (gone_tx, mut gone_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let trace = std::env::var("ESS_TRACE").is_ok();
    // 观看端报告的抵达计数（每秒一行进 STATS）。`missing` 只在报告抵达时才刷新，所以
    // 「观看端一块都不缺」和「观看端不再说话」在读数上长得一模一样——分不开就会把断流
    // 误判成健康（实测 1080p 半屏单帧突变后，观看端画布代数冻结而 `miss` 恒 0）。
    let report_cnt = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let rc = report_cnt.clone();
        tokio::spawn(async move {
            let mut asm = ess_transport::Assembler::default();
            let t0 = Instant::now();
            while let Some(ev) = events.recv().await {
                match ev {
                    livekit::prelude::RoomEvent::DataReceived { payload, .. } => {
                        if let Some(WireMsg::State {
                            layout_id,
                            versions,
                            epochs,
                            ..
                        }) = ess_transport::decode_incoming(&payload, &mut asm)
                        {
                            rc.fetch_add(1, Ordering::Relaxed);
                            if trace {
                                eprintln!("report layout={layout_id} tiles={}", versions.len());
                            }
                            let _ = report_tx.send((
                                layout_id,
                                versions,
                                epochs,
                                t0.elapsed().as_millis() as u64,
                            )).await;
                        }
                    }
                livekit::prelude::RoomEvent::ParticipantConnected(_) => {
                    let _ = force_tx.try_send(Recovery::Full);
                }
                livekit::prelude::RoomEvent::Reconnecting => {
                    let _ = log_tx.send("与服务器的连接中断，正在重连…".into());
                }
                livekit::prelude::RoomEvent::Reconnected => {
                    let _ = log_tx.send("重连成功：补发整帧重建观看端画面".into());
                    let _ = force_tx.try_send(Recovery::Reconnected);
                }
                // SDK 在全量重连里自动重新发布本地轨：底层 Track 与绑定的
                // NativeVideoSource 都保留，所以视频平面不需要重建。
                livekit::prelude::RoomEvent::LocalTrackRepublished { .. } => {
                    let _ = log_tx.send("视频轨已随重连自动恢复（编码器与源保持不变）".into());
                }
                livekit::prelude::RoomEvent::Disconnected { reason } => {
                    let _ = gone_tx.send(format!("与服务器的连接已断开: {reason:?}"));
                    return;
                }
                _ => {}
            }
        }
    });
    }

    let mut pipeline: Option<SendPipeline> = None;
    // 发送解耦：数据通道写满（实测 ~3.7 MB/s）不能反过来卡住采集节拍——
    // 视频轨在同一线程喂帧，一卡就掉帧。这里只排队，由专门的任务慢慢推。
    // 队列同时是**新鲜度闸门**：年龄超过 `fresh` 的逐块更新 / JPEG 运动帧不再发出，
    // 直接丢弃并计数。旧写法 128 深、没有年龄上限，等于在内存里替 lossy 通道攒下
    // 最多 8.5 s 的旧画面按序放出去——观看端"延迟越来越大、不追最新"就是这么来的。
    // 被丢的块由观看端的周期报告按**当前**画面要回去，所以收敛方向是"跳到最新"。
    let fresh = std::env::var("ESS_FRESH_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis((4000 / fps_ceil.max(1) as u64).clamp(100, 250)));
    let (wire_tx, mut wire_rx) = tokio::sync::mpsc::channel::<Batch>(32);
    // 真的没能发出去的**块 id**（新鲜度闸门丢的、限速桶丢的、队列满拒的）。管线拿到这份
    // id 才有资格只补那些块；留空 = 脱落来源没告诉我们掉了谁，那才走整帧兑现。
    let lost_ids: Arc<std::sync::Mutex<LossAccount>> =
        Arc::new(std::sync::Mutex::new(LossAccount::default()));
    let sent_cnt = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dropped_cnt = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stale_cnt = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // 测量台的人为链路上限（bps，0 = 不限，默认关）。本机环回上"排队"与"脱落"两件
    // 事都不会发生，所以 §6.6 预算执行器与平面交接的**后果**从来没被量过（#32）。
    // 令牌桶按 cap 放行：可靠消息桶空就地等（真链路就是排队），不可靠消息桶空就丢
    // （真链路就是丢包，观看端的周期报告按当前画面把它要回来）。
    let egress_cap = std::env::var("ESS_EGRESS_CAP")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let shed_cnt = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // 丢包注入的读数在 `ess_transport::DROPPED_LOSSY`（`ESS_DROP_EVERY`，测量台用）：注入点
    // 必须在 `send_wire` 里的**包**一层，不是会话侧的消息层——一条 255 块的 Update 在会话侧
    // 只是一条消息。上一版放在消息层，四臂 `inj` 全 0，量的根本不是丢包。
    // 花在 `publish_data` 上的累计时间（µs）。要回答的问题很具体：一次半屏变化要拆成
    // 140 条 16 KB 小包，逐条 await 串行推——如果观看到的画面是"一块块爬上来"，那要么
    // 是编码/链路，要么就是这个发送循环本身。没有读数之前这三种分不开。
    let send_us = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut send_prev = 0u64;
    {
        let lp = lp.clone();
        let sent = sent_cnt.clone();
        let stale = stale_cnt.clone();
        let shed = shed_cnt.clone();
        let lost = lost_ids.clone();
        let send = send_us.clone();
        tokio::spawn(async move {
            let mut msg_id = 0u32;
            let mut tokens = 0f64;
            let mut bucket_at = Instant::now();
            while let Some(b) = wire_rx.recv().await {
                if !b.baseline && b.at.elapsed() >= fresh {
                    stale.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut v) = lost.lock() {
                        merge_lost(&mut v, batch_tile_ids(&b.msgs));
                    }
                    continue;
                }
                for m in &b.msgs {
                    if egress_cap > 0 {
                        let now = Instant::now();
                        let rate = egress_cap as f64 / 8.0; // 字节/秒
                        // 突发只允许 250 ms 的量，否则开局可以整段免费冲出去
                        let fill = now.duration_since(bucket_at).as_secs_f64() * rate;
                        tokens = (tokens + fill).min(egress_cap as f64 * 0.25 / 8.0);
                        bucket_at = now;
                        // 允许**欠账**：一条整帧在 2 Mbps 上本来就要几秒。但每条消息最多
                        // 睡 200 ms，其余欠账由后续消息分摊。上一版按整条消息睡到还清，
                        // 结果一条 Full 把发送任务钉住两秒，可靠队列后面全饿死
                        // （实测 `sent=1` 冻结 12 秒）——那是在制造人工死锁，不是模拟窄链路。
                        tokens -= wire_bytes(m) as f64;
                        if tokens < 0.0 {
                            if b.baseline {
                                let slice = 0.2f64.min(-tokens / rate);
                                tokio::time::sleep(Duration::from_secs_f64(slice)).await;
                                tokens += slice * rate;
                                bucket_at = Instant::now();
                            } else {
                                shed.fetch_add(1, Ordering::Relaxed);
                                tokens = 0.0;
                            }
                        }
                    }
                    let t_send = Instant::now();
                    match ess_transport::send_wire(&lp, m, &mut msg_id).await {
                        Ok(()) => {
                            sent.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => eprintln!("发送失败: {e}"),
                    }
                    send.fetch_add(t_send.elapsed().as_micros() as u64, Ordering::Relaxed);
                }
            }
        });
    }
    let dropped = dropped_cnt.clone();
    let t_start = Instant::now();
    let mut last_stats = Instant::now();
    // 服务端彻底断开（SDK 放弃重连）时记录原因，循环退出后作为错误返回
    let mut dead: Option<String> = None;
    // 链路看门狗状态
    let mut link_up = true;
    let mut last_live = Instant::now();
    // §9 守门员：250 ms 观测一次，两次调整之间 ≥2 s 迟滞，发包排队 >100 ms 立即降级。
    let mut gov = Governor::new(ContentClass::Text);
    let mut gov_at = Instant::now();
    let mut link_now = LinkStats::default();
    let mut link_prev = LinkPrev::default();
    // 观测平滑：单帧整帧编码耗时、启动时补发整帧造成的突发，都不是拥塞，
    // 上一版直接拿单样本决策，开局就被这两样误降到 5 fps。
    let mut win_enc_us: u128 = 0;
    let mut win_frames: u64 = 0;
    let mut egress_ewma = 0f64;
    let mut last_fulls = 0u64;
    let mut quiet_until = Instant::now();
    let grace_s: u64 = std::env::var("ESS_RECONNECT_GRACE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .max(5);
    let perf = std::env::var("ESS_PERF").is_ok();
    let (mut enc_us, mut msg_n, mut frame_n) = (0u128, 0u64, 0u64);
    let mut feed_n = 0u64;
    // 采集→会话的最大积压年龄（ms），每秒随 STATS 报出后清零：这是"观看端在看
    // 多旧的画面"的第一段账，实时产品必须让它有界
    let mut lag_max_ms = 0u64;
    // 无损平面编码速度超预算的迟滞计数（超 2 s 置压、松 4 s 解除）
    // 无损平面单帧耗时（ms）的最近读数，随 STATS 报出
    let mut tile_enc_ms = 0f32;
    // #55：`tile_enc` 的窗口里其实混着两件事——`vp.push`（喂 libwebrtc 的 H264）与
    // `p.on_frame`（无损管线自己）。23 ms 的单帧尖峰必须先分清是谁的，否则又会像
    // §14.7.8 那样优化错一段。尾延迟看**每窗最大值**，均值只会把它抹平。
    let mut win_feed_us = 0u128;
    let mut win_pipe_us = 0u128;
    let mut win_feed_max = 0u128;
    let mut win_pipe_max = 0u128;
    let mut feed_ms = 0f32;
    let mut pipe_ms = 0f32;
    let mut pipe_max_ms = 0f32;
    let mut feed_max_ms = 0f32;
    // `push` 的三段（新建 I420 / 转换 / 交给 libwebrtc）按推送次数取均值
    let mut push_prev = 0u64;
    let mut tn_prev = 0u128;
    let mut tc_prev = 0u128;
    let mut tk_prev = 0u128;
    let mut push_new_ms = 0f32;
    let mut push_cvt_ms = 0f32;
    let mut push_cap_ms = 0f32;
    // `tile_enc` 量的是整个 `on_frame`（差分 + 策略 + 编码 + 打包），归因不出哪一段压掉了
    // 帧预算（§14.7.8 的并行化就是被这个标签带偏的）。管线内部分段计时按同一个窗口取增量，
    // 报成 `tile_diff=` / `tile_pack=`。
    let mut tile_diff_ms = 0f32;
    let mut tile_pack_ms = 0f32;
    let mut tile_detect_ms = 0f32;
    let mut detect_prev_us = 0u64;
    let mut diff_prev_us = 0u64;
    let mut pack_prev_us = 0u64;
    let mut frames_prev = 0u64;
    let mut enc_over_ticks = 0u32;
    let mut enc_under_ticks = 0u32;
    let mut enc_pressure = false;
    // 上一帧时的视频平面状态，用来抓切换沿（送入尺寸的进入/退出修正）
    let mut vmode_prev = false;
    // 预热开始前的图块通路帧率。预热会把采集抬到声明帧率（让编码器按满帧率预热），
    // 但预热**被取消**时（内容没动到值得进平面）必须原样还回去，否则刚抬到 30 的
    // 图块差分立刻超预算、守门员又降档，30↔20↔10 反复抖（用户报的果冻）。
    let mut fps_before_warm: Option<u32> = None;
    // 本段视频平面的开始时刻（爬坡期豁免用）
    let mut plane_on_at: Option<Instant> = None;

    // 脱落 → 基线重建的状态：三个计数器都是累计值，比较增量就知道"这一帧之后链路上少了几条"
    let mut lost_prev = 0u64;
    let mut loss_seen = false;
    let mut rebuild_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        tokio::select! {
            // 信箱里永远是**最新**那一帧：循环落后时更旧的帧已被覆盖，不会被排队重放
            Ok(()) = tick_rx.changed() => {
                let Some((at, frame)) = mail.lock().unwrap().take() else { continue };
                // 采集→会话的积压年龄：观看端看到旧画面的第一段账，随 STATS 报出去
                lag_max_ms = lag_max_ms.max(at.elapsed().as_millis() as u64);
                if pipeline.is_none() {
                    let mut p = SendPipeline::new(frame.clone(), codecs);
                    // 每帧打一行平面判定轨迹（detect / ratio / 需求 / 链路），路径争议就靠它
                    p.decide_log = std::env::var("ESS_DECIDE").is_ok();
                    // 运动帧参数（有损，用于保帧率）；默认开启
                    if let Ok(v) = std::env::var("ESS_MOTION") {
                        p.motion_enabled = v != "0";
                    }
                    if let Ok(v) = std::env::var("ESS_MOTION_MAX_W") {
                        if let Ok(n) = v.parse() { p.motion_max_w = n; }
                    }
                    if let Ok(v) = std::env::var("ESS_MOTION_MAX_H") {
                        if let Ok(n) = v.parse() { p.motion_max_h = n; }
                    }
                    if let Ok(v) = std::env::var("ESS_MOTION_QUALITY") {
                        if let Ok(n) = v.parse() { p.motion_quality = n; }
                    }
                    if let Ok(v) = std::env::var("ESS_MOTION_THRESHOLD") {
                        if let Ok(n) = v.parse() { p.motion_threshold = n; }
                    }
                    if let Ok(v) = std::env::var("ESS_MOTION_HOLD") {
                        if let Ok(n) = v.parse() { p.hold_frames = n; }
                    }
                    // 算力路径切平面的最低变化占比（0 = 关掉门槛，回到只看能不能编动）
                    if let Ok(v) = std::env::var("ESS_ENCODE_PLANE_MIN_RATIO") {
                        if let Ok(n) = v.parse() { p.encode_plane_min_ratio = n; }
                    }
                    // 图块选码：0 = 强制无损（验收臂的单变量对照，旧"逐字节相等"契约靠它复跑），
                    // 默认按"省几倍"降级到 JPEG（见 `CodecSet::encode_by_cost`）。
                    if let Ok(v) = std::env::var("ESS_LOSSY_SAVE_X") {
                        if let Ok(n) = v.parse::<u32>() {
                            if n == 0 {
                                p.lossy_trial_floor = usize::MAX; // 永不比价 ⇒ 永不降级
                            } else {
                                p.lossy_save_x = n;
                            }
                        }
                    }
                    if let Ok(v) = std::env::var("ESS_LOSSY_SAVE_X_EDGY") {
                        if let Ok(n) = v.parse() { p.lossy_save_x_edgy = n; }
                    }
                    // 预热上限（ms）：0 = 关掉预热，回到"决定即交接"（反向对照臂用）
                    if let Ok(v) = std::env::var("ESS_WARM_MS") {
                        if let Ok(n) = v.parse::<u64>() {
                            p.set_warm_max_us(n * 1000);
                        }
                    }
                    // 视频平面可用 → 运动时切媒体通道，JPEG 运动通路让位
                    p.video_plane = video.is_some();
                    // 流畅优先档：不问"无损装不装得下"，面积够大就切（帧率是这一档的合同）
                    p.smooth_motion = matches!(mode, ProductMode::Smooth);
                    pipeline = Some(p);
                }
                let p = pipeline.as_mut().unwrap();
                // 脱落 → 重建整帧基线。发送端的 `last_sent` 是在**产出消息**那一刻提交的，
                // 传输层真丢掉一条无损更新时没人把它退回去（`SendState::defer` /
                // `pending_dirty` 这条设计从来没接线），于是版本向量说"你有了"而像素没有：
                // 实测观看端报告一次缺口都没有（`resends=0`）、画布却整片是旧的（#52）。
                // 修复走已有的整帧重建路径：代价有界（≥1 s 一次），平面开着/预热时先欠着，
                // 退回无损后的第一帧再补。
                let lost = dropped_cnt.load(Ordering::Relaxed)
                    + stale_cnt.load(Ordering::Relaxed)
                    + shed_cnt.load(Ordering::Relaxed);
                if lost != lost_prev {
                    lost_prev = lost;
                    loss_seen = true;
                }
                if loss_seen
                    && !p.video_mode()
                    && !p.media_pending()
                    && rebuild_at.elapsed() >= Duration::from_millis(1000)
                {
                    // 只记案，不在这里决定补什么：管线会拿下一次观看端报告裁决——报告指出缺口就
                    // 交给逐块修复，报告说"全齐"才升级（#53）。直接在会话里 force_full() 会把
                    // 执行器自己的证明顶掉（实测 forced-plane 因此又变成 forced_plane=0）。
                    // 掉的是哪些块通常我们知道（闸门/队列丢的都是 Update 批次）——把 id 一起
                    // 记案，管线才可能只补那些块。id 不知情的脱落（例如外部只报了个计数）仍然
                    // 走整帧兑现，那是 #52/#53 钉住的行为。
                    let acct = lost_ids
                        .lock()
                        .map(|mut v| std::mem::take(&mut *v))
                        .unwrap_or_default();
                    if acct.unknown || acct.ids.is_empty() {
                        p.note_link_loss();
                    } else {
                        p.note_link_loss_tiles(&acct.ids);
                    }
                    loss_seen = false;
                    rebuild_at = Instant::now();
                    emit(Event::Log(format!(
                        "链路累计脱落 {lost} 条 → 记案，等下一次报告裁决（可知块 {}，{}）",
                        if acct.unknown { 0 } else { acct.ids.len() },
                        if acct.unknown {
                            "含说不清块 id 的内容，整帧兜底那条路保留"
                        } else {
                            "块 id 全可知，补账按块不兑整帧"
                        }
                    )));
                }
                let t0 = Instant::now();
                // 视频轨**先**拿帧（按上一帧的平面判定）。无损差分一帧要 10–30 ms，把
                // 喂帧排在它后面等于每帧都给编码器制造一次积压。静止期也按 ~2 fps 继续喂：
                // 消掉 libwebrtc 的黑帧占位，也让下一次进入视频平面时观看端接手的是
                // "几乎当前"的画面，而不是几十秒前的残留帧（那会看成画面回退）。
                let was_video = p.video_mode();
                let was_feeding = p.media_feeding();
                if let Some(vp) = video.as_mut() {
                    // 预热期也按满帧率喂：编码器需要在这段时间里把输入帧率从静止节拍
                    // 拉到声明帧率，不喂它就永远拉不上去（见 SendPipeline::plane_handover）
                    let every = if p.media_feeding() { 1 } else { idle_feed_every };
                    if feed_n % every == 0 {
                        let tf = Instant::now();
                        vp.push(&frame);
                        let d = tf.elapsed().as_micros();
                        win_feed_us += d;
                        win_feed_max = win_feed_max.max(d);
                    }
                }
                let tp = Instant::now();
                let encoded = p.on_frame(&frame);
                {
                    let d = tp.elapsed().as_micros();
                    win_pipe_us += d;
                    win_pipe_max = win_pipe_max.max(d);
                }
                if let Some(vp) = video.as_mut() {
                    // 平面切换沿。改送入尺寸这件事办在**退出**这一侧：尺寸一变编码器就
                    // 要重配 + 补一个关键帧，办在进入那一帧上，观看端正好在切换点看到一个
                    // 洞（实测 1280x720→960x540 之后 263 ms 才来第 5 帧）。平面关着时观看端
                    // 根本不收视频帧（`RecvPipeline` 要先见到 VideoMode{on}），所以这里的
                    // 开销是隐形的，下一次进入时编码器已经站在正确的尺寸上。
                    // 进入沿：记下时刻，让帧率控制器在前 2 s 的爬坡期内不动手（见观测支路）。
                    if p.video_mode() && !vmode_prev {
                        plane_on_at = Some(Instant::now());
                        // 平面期内不测图块耗时，进平面前那一刻的判定不能一直当真：
                        // 留着它会让平面退出的迟滞每帧续期，进来就再也退不出去。
                        enc_pressure = false;
                        enc_over_ticks = 0;
                        enc_under_ticks = 0;
                        emit(Event::Log("视频平面进入 → 画面由媒体通道出".into()));
                    }
                    if !p.video_mode() && vmode_prev {
                        plane_on_at = None;
                        emit(Event::Log("视频平面退出 → 回到无损图块".into()));
                        // 那一段没被带宽压着，说明当前尺寸真发得动：记下来给下次进入用，
                        // 便宜内容不该每次切换都白缩一遍
                        if link_now.ql_bw < 0.02 {
                            let s = vp.scale();
                            entry_scale = Some(entry_scale.map_or(s, |e| e.max(s)));
                        } else if link_now.enc_w > 0 && vp.dims().0 > 0 {
                            // 那一段**一直**被带宽压着：编码器已经自己缩到比我们的送入尺寸
                            // 更小的地方（实测 1080p 平衡档把 1440x810 再降到 960x536）。
                            // 线上真实尺寸才是下次进入的起点——只记我们自己的送入值，等于
                            // 每一次进入都从同一个错重新起跑（先爬坡、再被它降一档）。
                            let s =
                                (link_now.enc_w as f64 / vp.dims().0 as f64 * 20.0).floor() / 20.0;
                            if s > 0.0 {
                                entry_scale = Some(s.max(vp.floor()));
                            }
                        }
                        if let Some(s) = entry_scale {
                            if (vp.scale() - s).abs() > 0.01 {
                                let (fw, fh) = vp.set_feed_scale(s);
                                emit(Event::Log(format!(
                                    "视频平面退出：送入尺寸预置为 {fw}x{fh}（缩放 {s:.2}）"
                                )));
                            }
                        }
                    }
                    // 进入沿补喂一次：平面判定是这一帧刚变的，前面按"平面关着"的节奏
                    // 可能没喂到，切换那一帧不该在编码器里缺席。
                    // 预热过的帧**不补**：那一帧上面已经按满帧率喂过了，再补一次就是
                    // 同一时刻两根帧——实测交接后 202 ms 的那个洞就是这么来的（编码器
                    // 被两根同时刻的输入帧绊了一下）。
                    if p.video_mode() && !was_video && !was_feeding {
                        vp.push(&frame);
                    }
                    // 帧率上限是图块通路和媒体通路共用的一个数。图块阶段按 1080p 的
                    // 差分/编码耗时把它压到过 4，平面一开这个数不会自己回来——采集线程
                    // 仍按 4 fps 出帧，视频通路就永远只有 4 帧。预热起点和交接沿都把它
                    // 还给声明帧率：此后只有编码器真的算力不够（且像素已到底）才会再动它。
                    if (!was_feeding && p.media_feeding())
                        || (p.video_mode() && !was_video)
                    {
                        // 只有"从图块进入预热"这一沿才记旧值；交接沿/预算平面
                        // 不算预热，没有可还的图块帧率。
                        if !was_feeding && p.media_pending() {
                            fps_before_warm = Some(fps_ctl.load(Ordering::Relaxed));
                        }
                        if p.video_mode() && !was_video {
                            // 交接成功：平面接管，预热前的图块帧率作废
                            fps_before_warm = None;
                        }
                        let prev = fps_ctl.swap(fps_ceil, Ordering::Relaxed);
                        if prev < fps_ceil {
                            emit(Event::Log(format!(
                                "进入媒体通路（预热/交接）→ 帧率上限 {prev} → {fps_ceil} fps（图块阶段的降档不带入媒体通路）"
                            )));
                        }
                    }
                    // 预热被取消（内容没动到值得进平面）：把帧率原样还给图块通路。
                    // 不还的话，为预热抬到 30 的采集节奏拿去跑 1080p 图块差分，
                    // 每帧 30–45 ms > 33 ms 预算，守门员立刻再降一档；紧接着下一次
                    // 预热又抬回去——实测整场在 30/20/10/4 之间来回抖，观看端就是果冻。
                    if was_feeding && !p.media_feeding() && !p.video_mode() {
                        if let Some(prev_fps) = fps_before_warm.take() {
                            let prev = fps_ctl.swap(prev_fps, Ordering::Relaxed);
                            if prev != prev_fps {
                                emit(Event::Log(format!(
                                    "预热取消 → 帧率上限 {prev} → {prev_fps} fps（回到图块通路的守门员档位）"
                                )));
                            }
                        }
                    }
                    vmode_prev = p.video_mode();
                }
                feed_n += 1;
                let frame_enc_us = t0.elapsed().as_micros();
                win_enc_us += frame_enc_us;
                win_frames += 1;
                enc_us += frame_enc_us;
                match encoded {
                    Ok(msgs) => {
                        msg_n += msgs.len() as u64;
                        enqueue(&wire_tx, &dropped_cnt, &lost_ids, msgs).await;
                        frame_n += 1;
                    }
                    Err(e) => emit(Event::Error(format!("编码失败: {e}"))),
                }
            }
            Some((layout_id, versions, epochs, now)) = report_rx.recv() => {
                if let Some(p) = pipeline.as_mut() {
                    // 运动帧期间接收端是有损的，逐块补发只会抢带宽；退出运动时
                    // 会补整帧。但 layout=0（还没拿到布局）必须放行，否则运动
                    // 期间加入的观看方会一直黑屏。
                    if p.motion_active() && layout_id != 0 {
                        continue;
                    }
                    // layout 0 = 观看方还没收到整帧（刚加入或整帧在数据通道就绪前丢失）
                    if layout_id == 0 {
                        p.force_full();
                    }
                    match p.on_report(layout_id, &versions, &epochs, now) {
                        Ok(msgs) => enqueue(&wire_tx, &dropped_cnt, &lost_ids, msgs).await,
                        Err(e) => emit(Event::Error(format!("重发编码失败: {e}"))),
                    }
                }
            }
            Some(kind) = force_rx.recv() => {
                if let Some(p) = pipeline.as_mut() {
                    p.force_full();
                    // Reconnected 那一支的原因已经由事件循环说过了，这里只补新观看方
                    if matches!(kind, Recovery::Full) {
                        emit(Event::Log("新观看方加入：下一帧发送整帧".into()));
                    }
                }
            }
            Some(line) = log_rx.recv() => emit(Event::Log(line)),
            Some(reason) = gone_rx.recv() => {
                dead = Some(reason);
                stop.store(true, Ordering::Relaxed);
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }

        // 观测周期 250 ms（§9.2）：读真实链路统计 → 喂守门员 → 执行它的决定。
        if gov_at.elapsed() >= Duration::from_millis(250) {
            gov_at = Instant::now();
            link_now = read_link(&room_obj, &mut link_prev).await;
            let tile_ms = if win_frames > 0 {
                (win_enc_us / win_frames as u128) as f32 / 1000.0
            } else {
                0.0
            };
            tile_enc_ms = tile_ms;
            // 图块通路每秒吃掉的 CPU 份额（0–1）：`win_enc_us` 是这条管线在观测窗里
            // 花掉的时间，除以窗长就是"吃掉几成核"。
            //
            // 判"能不能升回更高帧率"必须用它，不能拿单帧耗时比目标预算：1080p 差分/
            // 编码是按**每秒变更面积**付钱的，30 fps 压到 20 fps 只是让每帧变更更多、
            // 单帧更贵。实测同一段内容 30 fps 单帧 30–37 ms（预算 33.3）、20 fps 单帧
            // 46–48 ms（预算 50）——占用都是 92–100 %。按单帧比，20→30 永远"装不下"，
            // 按占用比才是同一件事。
            let tile_duty = {
                let ms = last_stats.elapsed().as_millis() as f64;
                if ms >= 200.0 {
                    (win_enc_us as f64 / 1000.0) / ms
                } else {
                    f64::NAN
                }
            };
            // 管线内部的分段计时按窗口取增量：每帧平均花多少 µs 在差分上、多少在编码+打包上
            let st = pipeline.as_ref().map(|p| p.stats);
            if let Some(s) = st {
                let df = s.frames.saturating_sub(frames_prev).max(1) as f64;
                tile_diff_ms = (s.diff_us.saturating_sub(diff_prev_us) as f64 / df / 1000.0) as f32;
                tile_pack_ms = (s.encode_us.saturating_sub(pack_prev_us) as f64 / df / 1000.0) as f32;
                tile_detect_ms =
                    (s.detect_us.saturating_sub(detect_prev_us) as f64 / df / 1000.0) as f32;
                diff_prev_us = s.diff_us;
                pack_prev_us = s.encode_us;
                detect_prev_us = s.detect_us;
                frames_prev = s.frames;
            }
            let f_n = win_frames.max(1) as f64;
            feed_ms = (win_feed_us as f64 / f_n / 1000.0) as f32;
            pipe_ms = (win_pipe_us as f64 / f_n / 1000.0) as f32;
            feed_max_ms = (win_feed_max as f64 / 1000.0) as f32;
            pipe_max_ms = (win_pipe_max as f64 / 1000.0) as f32;
            if let Some(vp) = video.as_ref() {
                let n = (vp.pushed + vp.dropped).saturating_sub(push_prev).max(1) as f64;
                push_new_ms = (vp.t_new_us.saturating_sub(tn_prev) as f64 / n / 1000.0) as f32;
                push_cvt_ms = (vp.t_cvt_us.saturating_sub(tc_prev) as f64 / n / 1000.0) as f32;
                push_cap_ms = (vp.t_cap_us.saturating_sub(tk_prev) as f64 / n / 1000.0) as f32;
                push_prev = vp.pushed + vp.dropped;
                tn_prev = vp.t_new_us;
                tc_prev = vp.t_cvt_us;
                tk_prev = vp.t_cap_us;
            }
            win_feed_us = 0;
            win_pipe_us = 0;
            win_feed_max = 0;
            win_pipe_max = 0;
            win_enc_us = 0;
            win_frames = 0;
            // 实发速率取指数平均（α=0.3 → 约 1 s 尺度）：首帧整帧 + 补发造成的
            // 26 Mbps 突发在平滑后只剩几百 kbps 的鼓包，不会被当成持续拥塞。
            egress_ewma = if egress_ewma == 0.0 {
                link_now.egress_bps
            } else {
                egress_ewma * 0.7 + link_now.egress_bps * 0.3
            };
            // 刚发过整帧（首次/新观看端加入/改分辨率）之后的 2 s 里，队列里是自己塞进去
            // 的 1.2 MB，排队延迟与丢包抬头不代表链路差，所以整段不做决策（照常观测）。
            let fulls = pipeline.as_ref().map(|p| p.stats.full_frames).unwrap_or(0);
            if fulls != last_fulls {
                last_fulls = fulls;
                quiet_until = Instant::now() + Duration::from_secs(2);
            }
            // 预热 3 s：开局那几帧（整帧、首帧编码、订阅建立）不代表稳态，不做决策
            if t_start.elapsed() >= Duration::from_secs(3) && Instant::now() >= quiet_until {
                let vmode = pipeline.as_ref().map(|p| p.video_mode()).unwrap_or(false);
                // 预热期：画面还在无损通路上，但编码器已经在按满帧率收帧了。帧率控制器
                // 在这段也要动手（见下面 `vmode || pending`）——改尺寸的代价（重配 +
                // 关键帧，实测 260–490 ms 的画面空洞）此刻落在观众看不见的画面上。
                let pending = pipeline.as_ref().map(|p| p.media_pending()).unwrap_or(false);
                // 预热期也算视频类：这时喂给编码器的已经是满帧率，拿图块的耗时去判
                // "算力不够"会把声明帧率当成本钱砍掉（1080p 图块差分本来就贵）。
                gov.set_class(if vmode || pending {
                    ContentClass::Video
                } else {
                    ContentClass::Text
                });
                gov.motion = vmode || pending;
                let fps_now = fps_ctl.load(Ordering::Relaxed).max(1) as f32;
                // §9.2 的「bwe < 需求」判据要求估计值真的反映链路容量。实测本机
                // availableOutgoingBitrate 只是跟着视频目标码率爬（数据通道不走 pacer），
                // 图块通路一忙就 10 Mbps > 5 Mbps，被误判成拥塞连降两档。所以这条只在
                // 有真实拥塞证据（排队延迟抬头 / 观看端回包丢包 / 编码器自己报带宽受限）
                // 时才交给守门员；没有证据就当链路够用，其余判据（排队、QP、编码耗时）
                // 各自独立成立。
                let congestion =
                    link_now.queue_ms > 50 || link_now.loss_fraction > 0.01 || link_now.bw_limited;
                // 图块通路的发送预算：可用上行减去视频轨目标码率就是无损通路还能占的
                // 余量，取 60% 留抖动空间，下限 200 kbps（再小就连小编辑都发不动，
                // 那种情况下不如干脆留在媒体通道）。没有拥塞证据时不限——本机
                // availableOutgoingBitrate 只是跟着目标码率爬，拿它当容量会误伤。
                let tile_budget = if congestion && link_now.avail_bps > link_now.target_bps {
                    ((link_now.avail_bps - link_now.target_bps) * 0.6) as u64
                } else if congestion {
                    200_000
                } else {
                    0
                };
                // 无损平面的**速度**合同：单帧编码耗时逼近帧预算，就和"链路装不下"
                // 是同一种病——继续留在图块通路，观看端拿到的仍是按序播放的旧画面，
                // 降帧率只是把延迟摊平。连续 2 s 超 90% 预算就把画面交给媒体通道
                // （1080p H264 一帧 5–10 ms），连续 4 s 低于 60% 预算才允许收回来。
                // 平面开着时不测量：那时图块通路本来就没活干，读数没有意义。
                if !vmode {
                    let budget_ms = 1000.0 / fps_now.max(1.0);
                    if tile_ms > budget_ms * 0.9 {
                        enc_over_ticks += 1;
                        enc_under_ticks = 0;
                    } else if tile_ms < budget_ms * 0.6 {
                        enc_under_ticks += 1;
                        enc_over_ticks = 0;
                    } else {
                        enc_over_ticks = 0;
                        enc_under_ticks = 0;
                    }
                    if !enc_pressure && enc_over_ticks >= 8 {
                        enc_pressure = true;
                        enc_over_ticks = 0;
                        emit(Event::Log(format!(
                            "无损编码超预算（单帧 {:.1} ms / 预算 {:.1} ms，连续 2 s）→ 画面交给媒体通道",
                            tile_ms, budget_ms
                        )));
                    } else if enc_pressure && enc_under_ticks >= 16 {
                        enc_pressure = false;
                        enc_under_ticks = 0;
                        emit(Event::Log("无损编码回到预算内（连续 4 s）→ 允许退回图块通路".into()));
                    }
                }
                if let Some(p) = pipeline.as_mut() {
                    p.set_budget_bps(tile_budget);
                    p.set_encode_pressure(enc_pressure);
                    // 运动判定要的是**原始链路估计**，不是图块预算：预算已经把视频轨
                    // 的目标码率扣掉了，而换平面的目的正是把画面交给视频轨。
                    p.set_link_bps(link_now.avail_bps as u64, fps_ceil);
                }
                // 帧率合同由我们自己守：交付不上设定帧率时，主动把送入编码器的尺寸
                // 降一档，而不是等 WebRTC 的自适应（它要好几秒才动，而一次滚动总共
                // 才几秒）。实测同一素材同一条 2.5 Mbps：原样送 720p 交付 12.7 fps，
                // 送 640x360 交付 21.8 fps（编码器自报 29）。
                // 阈值/耐心/下限都由产品档决定——清晰优先档根本不缩尺寸（分辨率就是
                // 它要保的东西），流畅优先档 1 s 证据就动手、允许缩到 0.25。
                // 预热证据窗：只喂给状态机，尺寸控制器的判据在下面
                if pending {
                    warm_acc += link_now.sent_fps;
                    warm_n += 1;
                    if warm_at.elapsed() >= Duration::from_secs(1) {
                        let want = fps_ctl.load(Ordering::Relaxed).max(1) as u64;
                        let sent_ps = warm_acc / warm_n.max(1) as f64;
                        warm_acc = 0.0;
                        warm_n = 0;
                        warm_at = Instant::now();
                        if let Some(p) = pipeline.as_mut() {
                            p.note_media_delivery(sent_ps, want, link_now.enc_w, link_now.enc_h);
                        }
                    }
                }
                if let Some((short_at, short_streaks)) = mode.fps_short() {
                    if let Some(vp) = video.as_mut() {
                        // 判据按 **1 s 窗**看。观测拍子是 250 ms，上一版把"这一秒喂满了
                        // 几帧"问在 250 ms 的拍子上（fed_ps≈15 对 want=60），条件永不成立，
                        // 整个控制器一次也没跑过——2026-10-31 复看三档六轮日志，没有任何
                        // 一条尺寸调整记录，当时保帧率的其实是 WebRTC 自己的降分辨率。
                        sent_acc += link_now.sent_fps;
                        sent_n += 1;
                        if ctl_at.elapsed() >= Duration::from_secs(1) {
                            let want = fps_ctl.load(Ordering::Relaxed).max(1) as f64;
                            let sent_ps = sent_acc / sent_n.max(1) as f64;
                            let fed_ps = vp.pushed.saturating_sub(push_last);
                            push_last = vp.pushed;
                            sent_acc = 0.0;
                            sent_n = 0;
                            ctl_at = Instant::now();
                            // 预热期把尺寸谈拢：
                            //   (a) 编码器已经自己降过档 → 把送入尺寸**对齐**到线上真实
                            //       尺寸。继续送大帧一是白做一次缩放，二是让上面那条
                            //       "尺寸稳定"判据永远数不到稳定（实测交接就是被这条
                            //       上限拖到 3.5 s 硬交出去，交付还在 4–8 fps 的半坡上）；
                            //   (b) 它还在喊带宽不够 → **我们**先降一档，不等它。这一步
                            //       迟早要付（实测它自己动是在交接后 0.6–0.8 s，观看端于是
                            //       吃下一次 346–400 ms 的定格），而预热期的画面还由无损
                            //       图块出着，付在这儿是免费的。
                            if pending {
                                let (dw, _dh) = vp.dims();
                                let target = if link_now.enc_w > 0 && dw > 0 {
                                    Some(
                                        (link_now.enc_w as f64 / dw as f64 * 20.0).floor() / 20.0,
                                    )
                                } else if link_now.ql_bw >= 0.02 {
                                    Some(vp.scale() - 0.25)
                                } else {
                                    None
                                };
                                // 预热期只降不升：把画面变大从来不是急事，交给退出沿预置
                                if let Some(s) = target.filter(|s| *s < vp.scale() - 0.01) {
                                    if vp.scale() > vp.floor() + 0.01 {
                                        let (fw, fh) = vp.set_feed_scale(s.max(vp.floor()));
                                        emit(Event::Log(format!(
                                            "预热期尺寸对齐 → 送入 {fw}x{fh}（编码器 {w}x{h}，交付 {:.1}/{} fps）",
                                            sent_ps,
                                            want as u32,
                                            w = link_now.enc_w,
                                            h = link_now.enc_h,
                                        )));
                                    }
                                }
                            }
                            // 只有真的按满帧率喂给编码器时，交付不足才算帧率缺口。静止期
                            // 只喂 ~2 fps（消 libwebrtc 的黑帧占位用），交付自然掉到 0.x——
                            // 上一版把平面退出前那几秒当成"不达标"，白白又把送入缩了一档。
                            // 爬坡豁免：平面进入后的头 2 s 编码器只出 12–16 帧/s（每一跑都
                            // 如此，与档位、码率无关），那是起步斜坡不是稳态缺口。把它当
                            // 证据去改尺寸，等于用一次 283 ms 的画面空洞 + 永久更小的画面
                            // 去换"慢半拍一秒"（2026-10-31 的 C-smooth 就是这么坏的）。
                            let ramping = plane_on_at
                                .map(|t| t.elapsed() < Duration::from_secs(2))
                                .unwrap_or(false);
                            if (vmode || pending) && !ramping && fed_ps + 2 >= want as u64 {
                                if sent_ps + 0.5 < want * short_at {
                                    fps_low_streak += 1;
                                    fps_high_streak = 0;
                                } else if sent_ps + 0.5 >= want * 0.95 {
                                    fps_high_streak += 1;
                                    fps_low_streak = 0;
                                } else {
                                    fps_low_streak = 0;
                                    fps_high_streak = 0;
                                }
                                if fps_low_streak >= short_streaks && vp.scale() > vp.floor() + 0.01
                                {
                                    let (fw, fh) = vp.set_feed_scale(vp.scale() - 0.25);
                                    fps_low_streak = 0;
                                    emit(Event::Log(format!(
                                        "视频平面送入尺寸 → {fw}x{fh}（交付 {:.1}/{} fps，{}档，编码器报带宽受限 {:.0}%）",
                                        sent_ps,
                                        want as u32,
                                        mode.label(),
                                        link_now.ql_bw * 100.0
                                    )));
                                } else if fps_high_streak >= 4
                                    && vp.scale() < 0.99
                                    && (link_now.ql_bw < 0.02
                                        // 编码器报带宽受限，但线上明明还有两成余量：它的
                                        // `quality_limitation_reason` 一旦落到 Bandwidth
                                        // 就不再自己清（实测 60 fps 档 59 个采样里 57 个
                                        // `bw_limited=true`，同期 `egress 4.5 < target 6.4`、
                                        // qp 26、交付 60.0/60）。只认 `ql_bw` 的话回升闸门
                                        // 在那一档永久关着——画面被一次进入爬坡的抖动钉死。
                                        // 这一条不放宽"升多少/什么时候升"，只补一个反证。
                                        || (link_now.target_bps > 0.0
                                            && link_now.egress_bps
                                                <= 0.8 * link_now.target_bps))
                                {
                                    // 回升只**预置**，不在平面开着的时候动：改尺寸要编码器重配 +
                                    // 补关键帧，那是一次 260 ms 量级的画面空洞（#41 实测），而
                                    // "把画面变大"从来不是急事。降级反过来——帧率已经在掉了，
                                    // 立刻执行。预置的值由退出沿在平面关闭期间落账。
                                    fps_high_streak = 0;
                                    let next = (vp.scale() + 0.25).min(1.0);
                                    entry_scale = Some(next);
                                    emit(Event::Log(format!(
                                        "送入尺寸回升预置 → {:.2}（交付 {:.1}/{} fps 已连续 4 s，{}）",
                                        next,
                                        sent_ps,
                                        want as u32,
                                        if link_now.ql_bw < 0.02 {
                                            "编码器不再报带宽受限"
                                        } else {
                                            "实发已低于目标的 80%"
                                        }
                                    )));
                                }
                            } else {
                                fps_low_streak = 0;
                                fps_high_streak = 0;
                            }
                        }
                    }
                }
                let obs = Obs {
                    // 视频平面出画时看编码器的每帧耗时；图块平面看我们自己管线的耗时。
                    // 预热期同理——那一段图块还在画，但帧率预算已经归编码器了。
                    encode_ms: if vmode || pending {
                        link_now.enc_ms_frame as f32
                    } else {
                        tile_ms
                    },
                    budget_ms: 1000.0 / fps_now,
                    queue_delay_ms: link_now.queue_ms,
                    qp: link_now.avg_qp,
                    // H264 的 QP 上限是 51，超过就是码率不够用
                    qp_max: 51,
                    bwe_bps: if congestion && link_now.avail_bps > 0.0 {
                        link_now.avail_bps as u64
                    } else {
                        u64::MAX
                    },
                    needed_bps: egress_ewma as u64,
                };
                let why = if obs.queue_delay_ms > 100 {
                    format!("发包排队 {} ms", obs.queue_delay_ms)
                } else if obs.bwe_bps < obs.needed_bps {
                    format!(
                        "上行估计 {:.1} Mbps < 实发 {:.1} Mbps",
                        obs.bwe_bps as f64 / 1e6,
                        obs.needed_bps as f64 / 1e6
                    )
                } else if obs.qp > obs.qp_max {
                    format!("平均 QP {} 超上限", obs.qp)
                } else if obs.encode_ms > obs.budget_ms * 0.9 {
                    format!("单帧 {:.1} ms 逼近预算 {:.1} ms", obs.encode_ms, obs.budget_ms)
                } else {
                    "链路持续富余".to_string()
                };
                if let Some(step) = gov.tick(t_start.elapsed().as_millis() as u64, &obs) {
                    // 执行器按判据分流：帧率是对用户的承诺，只有"编码器算力不够"才动它；
                    // 带宽/排队受限要压的是图块通路自己塞进可靠通道的那坨量（E2E 里
                    // 实测 248 ms 排队就是 1.2 MB 整帧把通道堵出来的，降采集节奏治不了）。
                    let cpu_case = obs.encode_ms > obs.budget_ms * 0.9 || link_now.cpu_limited;
                    // 算力不够、又是视频平面在出画：先让**像素**付这笔账。帧率是对用户的
                    // 承诺（"设定 60 就要跑到 60"），而少送一圈像素直接减小编码器每帧的
                    // 工作量。旧代码在这儿把承诺当场砍半——实测 1080p60 / 1248x702 单帧
                    // 18.5 ms 对 16.7 ms 预算，于是降到 30 fps，之后 40 s 都是 30 帧、观看端
                    // 帧间隔 p50 从 16 ms 变 30 ms，而砍之前交付还稳稳是 60.0。
                    // 只有像素已经缩到档位下限（帧率真的换不出东西了）才动帧率。
                    // 只有愿意拿尺寸换帧率的档才走这条路（清晰优先档里分辨率就是它要保的
                    // 东西，那一档该动的是帧率）。
                    // 而视频平面这条账要"**一帧真的塞不进周期**"才动手：本机实测 1080p60
                    // 单帧 15.0 ms 对 16.7 ms 预算——过了 0.9× 预警线，可帧时间还塞得进，
                    // 交付那时是 60.0/60、观看端间隔 p50 16 / p95 18 ms。为这点余量花掉
                    // 44% 的像素不值。图块通路仍按 0.9× 预警走（那条成本是我们自己的管线，
                    // 预算吃紧就是真吃紧）。
                    let over_budget = obs.encode_ms > obs.budget_ms || link_now.cpu_limited;
                    let preemptive = cpu_case && step == Step::Down && vmode && !over_budget;
                    let paid = if cpu_case
                        && step == Step::Down
                        && vmode
                        && over_budget
                        && mode.fps_short().is_some()
                    {
                        video.as_mut().and_then(|vp| {
                            (vp.scale() > vp.floor() + 0.01)
                                .then(|| vp.set_feed_scale(vp.scale() - 0.25))
                        })
                    } else {
                        None
                    };
                    if preemptive {
                        emit(Event::Log(format!(
                            "守门员：算力预警未到临界（单帧 {:.1} ms / 周期 {:.1} ms）→ 尺寸与帧率都不动",
                            obs.encode_ms, obs.budget_ms
                        )));
                    } else if let Some((fw, fh)) = paid {
                        emit(Event::Log(format!(
                            "守门员：编码算力不够（单帧 {:.1} ms / 预算 {:.1} ms{}）→ 送入尺寸 {fw}x{fh}，帧率保持 {} fps",
                            obs.encode_ms,
                            obs.budget_ms,
                            if link_now.cpu_limited {
                                "，编码器自报算力受限"
                            } else {
                                ""
                            },
                            fps_ctl.load(Ordering::Relaxed)
                        )));
                    } else if !cpu_case {
                        // 帧率是对用户的承诺，带宽/排队受限不动它；真正被收放的是上面
                        // set_budget_bps 那次调用里的图块通路预算。日志要说"做了什么"，
                        // 不能说"应压制"——那副模样好像执行器还没接上。
                        //
                        // 回升不在这里做：能不能把上限还给用户，取决于这条管线每秒
                        // 还剩多少算力（下面的占用闸门）。这里只报告阶梯位置。
                        emit(Event::Log(format!(
                            "守门员：{}（{why}）→ 图块预算 {}，帧率保持 {} fps",
                            if step == Step::Down { "链路收紧" } else { "链路回升" },
                            if tile_budget == 0 {
                                "不限".to_string()
                            } else {
                                format!("{:.0} kbps", tile_budget as f64 / 1000.0)
                            },
                            fps_ctl.load(Ordering::Relaxed)
                        )));
                    } else if step == Step::Down {
                        // 阶梯按用户上限等比缩放：Text 阶梯 15/10/5/2 配 30 fps 上限
                        // 就是 30/20/10/4，降完还能回到用户要的那一档
                        let want = governed_fps(&gov, fps_ceil);
                        fps_ctl.store(want, Ordering::Relaxed);
                        emit(Event::Log(format!(
                            "守门员：降级（{why} → {want} fps）"
                        )));
                    }
                }
                // 回升不在阶梯事件里做：图块通路的算力是**每秒**账（见 tile_duty），
                // 只有当下真的有 20 % 余量才把帧率还回去。每个观测拍子都评估，
                // 而不是只在 Step::Up 那一刻——阶梯走到顶以后就不会再有 Up 事件，
                // 那时内容变轻了也要能把帧率还回来。
                if !vmode && !pending && tile_duty.is_finite() && tile_duty <= 0.8 {
                    let want = governed_fps(&gov, fps_ceil);
                    let cur = fps_ctl.load(Ordering::Relaxed);
                    if want > cur {
                        fps_ctl.store(want, Ordering::Relaxed);
                        emit(Event::Log(format!(
                            "帧率上限回升 {cur} → {want} fps（图块占用 {:.0}% ≤ 80%）",
                            tile_duty * 100.0
                        )));
                    }
                }
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(1) {
            last_stats = Instant::now();
            // 连接看门狗。实测：服务端进程消失后 SDK 会报 Reconnecting（约 4–9 s 内，
            // 参与者列表同时清空），但 60 s 内都没有报 Disconnected——会话就一直挂在
            // "重连中"，观看端画面永久停在旧帧，分享端 UI 还显示正在共享。这里给它一个
            // 上限：超过宽限期没恢复就判定会话已死，返回错误让 UI 落到 error 态。
            let up = matches!(
                room_obj.connection_state(),
                livekit::prelude::ConnectionState::Connected
            );
            if up {
                if !link_up {
                    emit(Event::Log("链路已恢复".into()));
                }
                link_up = true;
                last_live = Instant::now();
            } else {
                if link_up {
                    emit(Event::Log(format!(
                        "检测到链路中断，最多等待 {grace_s} s 让 SDK 重连"
                    )));
                }
                link_up = false;
                if dead.is_none() && last_live.elapsed() >= Duration::from_secs(grace_s) {
                    dead = Some(format!("与服务器的连接中断超过 {grace_s} s，会话结束"));
                    stop.store(true, Ordering::Relaxed);
                }
            }
            let (frames, tiles, bytes, updates, fulls, scrolls, motion, motion_bytes, mdrop, video_frames, vmode, switches) =
                match &pipeline {
                    Some(p) => (
                        p.stats.frames,
                        p.stats.tiles_sent,
                        p.stats.bytes_encoded,
                        p.stats.updates,
                        p.stats.full_frames,
                        p.stats.scrolls,
                        p.stats.motion_frames,
                        p.stats.motion_bytes,
                        p.stats.motion_dropped,
                        video.as_ref().map(|v| v.pushed).unwrap_or(0),
                        p.video_mode(),
                        p.stats.video_on + p.stats.video_off,
                    ),
                    None => (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, false, 0),
                };
            let sent = sent_cnt.load(Ordering::Relaxed);
            let link = link_now;
            if perf {
                let (bud, forced) = pipeline
                    .as_ref()
                    .map(|p| (p.budget_bps(), p.stats.budget_forced))
                    .unwrap_or((0, 0));
                eprintln!(
                    "STATS fps_cap={} avail={:.2}Mbps egress={:.2}Mbps audio={:.1}kbps(audiotgt={:.0} pkt={:.0}/s) rtt={:.1}ms loss={:.3} rx_rtt={:.1}ms target={:.2}Mbps enc={}x{}@{:.1} vsent={:.1} ql_bw={:.2} nack={:.0} qdelay={}ms qp={} bw_limited={} cpu_limited={} res_changes={} tile_budget={} forced_plane={} lag={}ms q={} stale={} enc_press={} strain={} warm={} strain_forced={} tile_enc={:.1}ms tile_diff={:.1}ms tile_pack={:.1}ms tile_detect={:.1}ms feed={:.1}ms feed_max={:.1}ms pipe={:.1}ms pipe_max={:.1}ms push_new={:.1}ms push_cvt={:.1}ms push_cap={:.1}ms tile_bytes={} tiles={} updates={} egress_cap={} shed={} missing={} behind={} reports={} send={:.0}ms inj={} lossy={}",
                    fps_ctl.load(Ordering::Relaxed),
                    link.avail_bps / 1e6,
                    link.egress_bps / 1e6,
                    link.audio_egress_bps / 1e3,
                    link.audio_target_bps,
                    link.audio_packets_ps,
                    link.rtt_ms,
                    link.loss_fraction,
                    link.rx_rtt_ms,
                    link.target_bps / 1e6,
                    link.enc_w,
                    link.enc_h,
                    link.enc_fps,
                    link.sent_fps,
                    link.ql_bw,
                    link.nack_ps,
                    link.queue_ms,
                    link.avg_qp,
                    link.bw_limited,
                    link.cpu_limited,
                    link.res_changes,
                    bud,
                    forced,
                    lag_max_ms,
                    32usize.saturating_sub(wire_tx.capacity()),
                    stale_cnt.load(Ordering::Relaxed),
                    enc_pressure,
                    pipeline.as_ref().map(|p| p.receiver_strain).unwrap_or(false),
                    pipeline.as_ref().map(|p| p.media_pending()).unwrap_or(false),
                    pipeline.as_ref().map(|p| p.stats.strain_forced).unwrap_or(0),
                    tile_enc_ms,
                    tile_diff_ms,
                    tile_pack_ms,
                    tile_detect_ms,
                    feed_ms,
                    feed_max_ms,
                    pipe_ms,
                    pipe_max_ms,
                    push_new_ms,
                    push_cvt_ms,
                    push_cap_ms,
                    bytes,
                    tiles,
                    updates,
                    egress_cap,
                    shed_cnt.load(Ordering::Relaxed),
                    // 上一份观看端报告里「发送端版本比它新」的块数与「落后 ≥2 版」的块数。
                    // 前者是**收敛曲线**的读数：一次大面积变化后它应当在几百毫秒内回到 0，
                    // 回不去就是用户说的"很多图块慢慢覆盖"；后者用来区分"我们没发"和"它啃不动"。
                    pipeline.as_ref().map(|p| p.ctl.last_gap).unwrap_or(0),
                    pipeline.as_ref().map(|p| p.ctl.last_behind).unwrap_or(0),
                    report_cnt.load(Ordering::Relaxed),
                    (send_us.load(Ordering::Relaxed) - send_prev) as f64 / 1000.0,
                    ess_transport::DROPPED_LOSSY.load(Ordering::Relaxed),
                    pipeline.as_ref().map(|p| p.stats.tiles_lossy).unwrap_or(0)
                );
            }
            // 每秒的最大积压年龄报完就清零（要的是"这一秒最旧的帧有多旧"）
            lag_max_ms = 0;
            send_prev = send_us.load(Ordering::Relaxed);
            // 两条账必须分开印。`claims` = 观看端报告**认领**的块数（在途抑制之前计数，
            // 所以它含被摘掉的那些）；`resends` = 真正重新编码并交给人的块数。
            // 以前这里把 `report_resends` 印成 `resends=`，于是"重发 137 块"与"同一秒
            // `tile_bytes` 一分没花"摆在同一行，读起来像仪表坏了（实测 z60c 就是这么起的疑）。
            let (repairs, repair_tiles, resends, claims, throttled, escalations) = match &pipeline {
                Some(p) => (
                    p.stats.repairs,
                    p.stats.repair_tiles,
                    p.ctl.stats.resends,
                    p.ctl.stats.report_resends,
                    p.ctl.stats.repair_throttled,
                    p.ctl.stats.escalations,
                ),
                None => (0, 0, 0, 0, 0, 0),
            };
            if perf {
                eprintln!(
                    "LINK {:?} participants={}",
                    room_obj.connection_state(),
                    room_obj.remote_participants().len()
                );
                eprintln!(
                    "MOTION frames={motion} bytes={motion_bytes} dropped={mdrop} full={fulls} video={video_frames} vmode={vmode} switches={switches}"
                );
                // 报告驱动的修复链有没有真的动起来：`claims` = 报告认领的块数，
                // `resends` = 其中真正重发的块数，`throttled` = 被 §6.6 修复预算扣下的，
                // `escalations` = 升级为整帧的次数。窄链路脱落能不能收敛看后三条（#51）。
                eprintln!(
                    "REPAIR exits={repairs} tiles={repair_tiles} resends={resends} claims={claims} throttled={throttled} escalations={escalations}"
                );
                eprintln!(
                    "PERF 1s frames={frame_n} msgs={msg_n} enc={:.0}ms sent={sent} qdrop={} audio_total={}",
                    enc_us as f64 / 1000.0,
                    dropped.load(Ordering::Relaxed),
                    audio_cnt.load(Ordering::Relaxed)
                );            }
            enc_us = 0; msg_n = 0; frame_n = 0;
            emit(Event::Stats(Stats {
                elapsed_s: t_start.elapsed().as_secs(),
                frames,
                tiles,
                msgs: sent,
                bytes,
                updates,
                fulls,
                scrolls,
                video_frames,
                video_mode: vmode,
                video_track,
                audio_track,
                audio_frames: audio_cnt.load(Ordering::Relaxed),
                link: Some(link),
            }));
        }
    }

    stop.store(true, Ordering::Relaxed);
    drop(tick_rx);
    let _ = capture.join();
    if dead.is_none() {
        let _ = room_obj.close().await;
    }
    match dead {
        // 只返回错误：CLI 与 UI 都已经在 Err 分支上把原因显示出来，
        // 这里再 emit 一次会在日志里出现两条同样的"会话结束"。
        Some(reason) => Err(reason),
        None => {
            emit(Event::Log(format!(
                "会话结束：共发送 {} 条消息",
                sent_cnt.load(Ordering::Relaxed)
            )));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{enqueue, merge_lost, Batch, LossAccount, ProductMode};
    use ess_core::wire::WireMsg;

    /// 滚动中让观看端在**这一步内容落地之前**取快照交报告（现场那 ~0.5 s 报告往返的形状），
    /// 每一步都断言画布逐字节等于当时那一帧。为 #11 唯一幸存的假设而写：报告重发的像素取自
    /// `state.last_sent`（`crates/ess-core/src/pipeline.rs:1067`），而版本向量在**产出那一刻**提交
    /// （同文件 295 行注释）——两者不是同一帧就会"盖错内容、记对版本"，画布一整条边停在旧的。
    /// **这个形状的结果：30 步逐帧逐字节一致，中途快照没造出缺口** ⇒ 现场那条"整条顶边差一步"
    /// 还没被这个机制解释。
    ///
    /// 滚动 **加上观看端的周期报告**：快照取在这一步的内容落地之前，内容落地之后才把报告的
    /// 答复发出去——这正是现场那 ~0.5 s 报告往返里发生的事，也是上一条逐帧精确的测试唯一没覆盖的一维。
    ///
    /// 为什么写这条：现场纯滚动臂（`ESS_DEMO_BLOCK=0`，`switches=0`、平面一秒没开、滚动宣告 77 次）
    /// 仍然会红，差异形状是**整条顶部块行**（y0..63、20 块、75 200 像素）——滚动平移腾出来的那条边。
    /// 唯一还没被排除的机制（#11）：报告重发的像素取自 `state.last_sent`，而版本向量在产出那一刻
    /// 就提交了；两者不是同一帧时，就会"盖错内容、记对版本"。这条测试就是判它的。
    #[test]
    fn a_scroll_with_mid_flight_viewer_reports_stays_pixel_exact() {
        use ess_capture::{ScreenSource, SyntheticScreen};
        use ess_core::model::Message;
        let mut screen = SyntheticScreen {
            w: 640,
            h: 360,
            animate_frames: 40,
            animate_delay: 0,
            flash: false,
            block: false,
            step_at: 0,
            video_like: false,
        };
        let pal = ess_codec_palette::PaletteZstdCodec::default();
        let png = ess_codec_png::PngCodec;
        let codecs = ess_core::pipeline::CodecSet { codecs: &[&pal, &png] };
        let f0 = screen.frame(0);
        let mut send = ess_core::pipeline::SendPipeline::new(f0.clone(), codecs);
        send.motion_enabled = false;
        let mut recv = ess_core::pipeline::RecvPipeline::new(codecs);

        let feed = |recv: &mut ess_core::pipeline::RecvPipeline, msgs: Vec<ess_core::wire::WireMsg>| {
            for m in &msgs {
                let b = ess_core::wire::encode(m).unwrap();
                recv.on_wire(&ess_core::wire::decode(&b).unwrap()).unwrap();
            }
        };
        feed(&mut recv, send.on_frame(&f0).unwrap());

        let mut replies = 0u32;
        for i in 1..30u32 {
            let f = screen.frame(i as u64 * 33_333);
            let step = send.on_frame(&f).unwrap();
            // 快照先取（这一步的内容还没落地）
            let snap = if i % 3 == 2 { Some(recv.report()) } else { None };
            feed(&mut recv, step);
            if let Some(Message::Report { layout_id, versions, epochs }) = snap {
                let reply = send
                    .on_report(layout_id, &versions, &epochs, i as u64 * 500)
                    .unwrap();
                replies += reply.len() as u32;
                feed(&mut recv, reply);
            }
            let bad = recv
                .canvas
                .px
                .iter()
                .zip(f.px.iter())
                .position(|(a, b)| a != b);
            if let Some(o) = bad {
                let px = o / 4;
                panic!(
                    "第 {i} 步画布与画面不一致：首个差异在 x={} y={}（块行 {}，顶行=0；底行窄块从 y=320 起），报告答复共 {} 条消息",
                    px % 640,
                    px / 640,
                    (px / 640) / 64,
                    replies
                );
            }
        }
        // 这份夹具在 30 步里一次报告答复都没产生（中途快照没造出缺口），所以 `replies` 只当读数。
        // 要判"丢一步之后要回来的是不是当前内容"，注入步必须按消息实际落点选，不能猜步号。
        eprintln!("滚动 + 中途快照：30 步逐帧逐字节一致；报告答复共 {replies} 条消息");
    }

    /// 纯滚动（不进平面）时，观看端画布必须**每一帧**都逐字节等于当时那一帧。
    ///
    /// 为什么写这条：现场 `ESS_DEMO_BLOCK=0` 的臂（`switches=0`、平面一秒没开、滚动宣告 77 次）
    /// 仍然会红，而且差异形状是**整条顶部块行**（1280x720 上 y0..63、20 块、75 200 像素）——
    /// 也就是滚动平移腾出来的那条边没被正确回填。逐帧比对能把第一次发散钉在第几步。
    #[test]
    fn a_scrolling_scene_stays_pixel_exact_on_every_step() {
        use ess_capture::{ScreenSource, SyntheticScreen};
        let mut screen = SyntheticScreen {
            w: 640,
            h: 360,
            animate_frames: 40,
            animate_delay: 0,
            flash: false,
            // 纯滚动：没有独立移动的物体，滚动检测器该认出 dy
            block: false,
            step_at: 0,
            video_like: false,
        };
        let pal = ess_codec_palette::PaletteZstdCodec::default();
        let png = ess_codec_png::PngCodec;
        let codecs = ess_core::pipeline::CodecSet { codecs: &[&pal, &png] };
        let f0 = screen.frame(0);
        let mut send = ess_core::pipeline::SendPipeline::new(f0.clone(), codecs);
        send.motion_enabled = false;
        let mut recv = ess_core::pipeline::RecvPipeline::new(codecs);

        let mut scroll_msgs = 0u32;
        for i in 0..30u32 {
            let f = screen.frame(i as u64 * 33_333);
            for m in &send.on_frame(&f).unwrap() {
                if matches!(m, ess_core::wire::WireMsg::Scroll { .. }) {
                    scroll_msgs += 1;
                }
                let b = ess_core::wire::encode(m).unwrap();
                recv.on_wire(&ess_core::wire::decode(&b).unwrap()).unwrap();
            }
            let bad = recv
                .canvas
                .px
                .iter()
                .zip(f.px.iter())
                .position(|(a, b)| a != b);
            if let Some(o) = bad {
                let px = o / 4;
                panic!(
                    "第 {i} 步（ts={} ms）画布与画面不一致：首个差异像素在 x={} y={}；滚动宣告 {} 条；块行 {}（底行窄块从 y=320 起）",
                    i as u64 * 33,
                    px % 640,
                    px / 640,
                    scroll_msgs,
                    (px / 640) / 64
                );
            }
        }
        assert!(scroll_msgs > 0, "这一臂本该走滚动宣告（现场 77 条），一条都没发出来说明夹具没被识别成平移");
    }

    /// 现场形状的可复现版：合成屏一路滚动（内容每帧都在变），预算执行器把画面交给媒体平面，
    /// 按住几秒后松开、退回无损并发精确修复——观看端画布必须逐字节等于退出那一刻的画面。
    ///
    /// 为什么写这条：存档的失败臂 `target/motion-fail` 就是这个形状——平面按了 5 秒（成功臂 4 秒），
    /// 终帧差最底行一个窄块（1280x720 上 x768..831 y704..719），而 `stale`/`missing`/`behind` 全程 0、
    /// 一条脱落记案都没有、退出修复块数与成功臂同为 238：**版本对得上，像素差一步**。
    /// 360 高配 64 的块 ⇒ 底行是 40 像素的窄块，正是出问题那一行的形状。
    #[test]
    fn a_plane_held_over_a_scrolling_scene_returns_pixel_exact() {
        use ess_capture::{ScreenSource, SyntheticScreen};
        let mut screen = SyntheticScreen {
            w: 640,
            h: 360,
            // 5 步之后画面冻结：退出平面要等"画面 settle"，冷启动不是切换
            animate_frames: 5,
            animate_delay: 0,
            flash: false,
            block: true,
            step_at: 0,
            video_like: false,
        };
        let pal = ess_codec_palette::PaletteZstdCodec::default();
        let png = ess_codec_png::PngCodec;
        let codecs = ess_core::pipeline::CodecSet { codecs: &[&pal, &png] };
        let mut send = ess_core::pipeline::SendPipeline::new(screen.frame(0), codecs);
        send.video_plane = true;
        // 隔离出预算这一条判据：不许走 JPEG 运动通路盖画布
        send.motion_enabled = false;
        send.hold_frames = 2;
        let mut recv = ess_core::pipeline::RecvPipeline::new(codecs);

        let mut last = screen.frame(0);
        let m0 = send.on_frame(&last).unwrap();
        for m in &m0 {
            let b = ess_core::wire::encode(m).unwrap();
            recv.on_wire(&ess_core::wire::decode(&b).unwrap()).unwrap();
        }
        assert_eq!(recv.canvas.px, last.px, "首帧落画");

        // 30 fps 走 30 步：第 4 步压预算逼出平面，第 14 步松开等迟滞退出
        for i in 1..30u32 {
            if i == 4 {
                send.set_budget_bps(200_000);
            }
            if i == 14 {
                send.set_budget_bps(0);
            }
            last = screen.frame(i as u64 * 33_333);
            for m in &send.on_frame(&last).unwrap() {
                let b = ess_core::wire::encode(m).unwrap();
                recv.on_wire(&ess_core::wire::decode(&b).unwrap()).unwrap();
            }
        }
        assert!(send.stats.video_on >= 1, "这一臂应当真的被预算逼进过平面");
        assert!(!send.video_mode(), "平面松开后应退回无损");
        let bad = recv
            .canvas
            .px
            .iter()
            .zip(last.px.iter())
            .position(|(a, b)| a != b);
        if let Some(o) = bad {
            let px = o / 4;
            panic!(
                "退回无损后画布与当时画面不一致：首个不同像素在 x={} y={}（底行窄块从 y=320 开始），平面进入 {} 次、退出修复 {} 块",
                px % 640,
                px / 640,
                send.stats.video_on,
                send.stats.repair_tiles
            );
        }
    }

    /// 静止画面每帧产出的**空批次**不得占用发送队列。
    ///
    /// 占用的后果不是浪费一次 channel op：空批次会在新鲜度闸门里 aging 成「脱落」，
    /// 脱落累计去设 `loss_claimed`，于是观看端一份「我全对上了」的报告被读成「提交即
    /// 记账把缺口掩盖了」，每 1.5 s 补一次整帧重建。实测 1080p 半屏单帧突变 + 之后完全
    /// 静止：`stale` 0→68、`tile_bytes` 2.75→11.57 MB、`forced_plane` 0→2、
    /// `switches` 0→4 —— 其中 9 MB 是这么白花出来的，观看端从头到尾一块都不缺。
    #[tokio::test]
    async fn empty_batches_never_enter_the_send_queue() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Batch>(2);
        let dropped = std::sync::atomic::AtomicU64::new(0);
        let lost = std::sync::Mutex::new(LossAccount::default());

        enqueue(&tx, &dropped, &lost, Vec::new()).await;
        assert_eq!(rx.capacity(), 2, "空批次占了发送队列的位");

        enqueue(
            &tx,
            &dropped,
            &lost,
            vec![WireMsg::State {
                layout_id: 1,
                cols: 2,
                rows: 2,
                versions: vec![],
                epochs: vec![],
            }],
        )
        .await;
        assert_eq!(rx.capacity(), 1, "非空批次必须入队");
        assert_eq!(
            dropped.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "队列没满不该记脱落"
        );
    }

    /// 脱落账本：只要掺进一次**叫不出块 id** 的脱落（JPEG 运动帧那一类），整笔账就必须
    /// 退回"整帧兜底"那条路。实测把它放宽成"有些 id 就按块补"，`motion-stays-lossless`
    /// 的"退回无损后画布逐字节等于期望终帧"当场挂——省字节不能省掉正确性。
    #[test]
    fn loss_account_falls_back_when_any_drop_has_no_tile_ids() {
        use ess_core::model::TileId;
        let mut a = LossAccount::default();
        merge_lost(&mut a, vec![TileId(0), TileId(1)]);
        assert!(!a.unknown && a.ids.len() == 2, "Update 批次的脱落应按块记账");
        merge_lost(&mut a, Vec::new());
        assert!(a.unknown, "说不清块 id 的脱落必须把整帧兜底那条路留回来");
    }

    /// 进入视频平面的送入尺寸 = 请求码率能不能装满尺寸跑帧率的直接读数。
    /// 数值对着 1280x720@30 的实测：0.09 bpp（平衡档自动码率 2 488 320 bps）实测
    /// 编码器要两秒才自己降到 960x540，所以进入就该是 0.75；0.15 bpp（流畅优先档
    /// 4 147 200 bps）实测原尺寸不受限，所以进入就是 1.0。
    #[test]
    fn entry_scale_follows_the_requested_bitrate() {
        let s = |m: ProductMode, bps: u64, fps: u64| m.entry_scale(bps, 1280, 720, fps);
        assert_eq!(s(ProductMode::Smooth, 4_147_200, 30), Some(1.0));
        assert_eq!(s(ProductMode::Balanced, 2_488_320, 30), Some(0.75));
        // 码率再低就再缩，但不越 1.0 上界
        assert_eq!(s(ProductMode::Smooth, 300_000, 30), Some(0.25));
        assert_eq!(s(ProductMode::Smooth, 12_000_000, 30), Some(1.0));
        // 自动码率随帧率等比放大（像素 × 帧率 × 系数），所以 60 fps 的系数没变，
        // 进入尺寸也不该变——变的只是每帧更便宜，装得下原尺寸照样装得下
        assert_eq!(s(ProductMode::Balanced, 4_976_640, 60), Some(0.75));
    }

    /// 清晰优先档的反向对照：分辨率是它要保的东西，进入尺寸不由码率决定。
    #[test]
    fn clear_mode_never_shrinks_on_entry() {
        assert_eq!(ProductMode::Clear.entry_scale(300_000, 1280, 720, 30), None);
        assert!(ProductMode::Clear.fps_short().is_none());
    }
}
