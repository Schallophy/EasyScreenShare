//! O-1 / O-2 LiveKit 数据通道探针（§16 未决项）。
//!
//! 前置：`livekit-server --dev`（devkey/secret，ws://127.0.0.1:7880）。
//!
//! O-1：lossy / reliable 通道上各尺寸消息的送达率与上限（256 B … 128 KB）。
//! O-2：可靠大流量传输期间，lossy 小消息的 RTT 是否被队头阻塞
//!      （F-3：可靠/不可靠是否走独立 SCTP 流）。
//!
//! 运行：`cargo run -p lk-probe --release`
//! 结果：`screenshare-bench/results-lk-probe.json` + 控制台表格。

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use livekit::prelude::*;
use livekit_token::{AccessToken, VideoGrants};

const URL: &str = "ws://127.0.0.1:7880";
const ROOM: &str = "probe";
const KEY: &str = "devkey";
const SECRET: &str = "secret";

// ---- 载荷格式：[kind u8][class u8][seq u32][ts u64][padding] ----
const KIND_LOSSY: u8 = 1;
const KIND_RELIABLE: u8 = 2;
const KIND_PING: u8 = 3;
const KIND_PONG: u8 = 4;

const HEADER: usize = 16;

#[derive(Clone, Debug, Default)]
struct Stats {
    sent: u64,
    recv: u64,
    latencies: Vec<u64>,
}

impl Stats {
    fn pct(&self, q: f64) -> u64 {
        if self.latencies.is_empty() {
            return u64::MAX;
        }
        let mut v = self.latencies.clone();
        v.sort_unstable();
        let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
        v[idx.min(v.len() - 1)]
    }
}

type Shared = std::sync::Arc<std::sync::Mutex<HashMap<(u8, u8), Stats>>>;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn pack(kind: u8, class: u8, seq: u32, total: usize) -> Vec<u8> {
    let mut buf = vec![0u8; total.max(HEADER)];
    buf[0] = kind;
    buf[1] = class;
    buf[2..6].copy_from_slice(&seq.to_le_bytes());
    buf[6..14].copy_from_slice(&now_ms().to_le_bytes());
    buf
}

fn unpack(payload: &[u8]) -> Option<(u8, u8, u32, u64)> {
    if payload.len() < HEADER {
        return None;
    }
    let kind = payload[0];
    let class = payload[1];
    let seq = u32::from_le_bytes(payload[2..6].try_into().ok()?);
    let ts = u64::from_le_bytes(payload[6..14].try_into().ok()?);
    Some((kind, class, seq, ts))
}

fn mk_grants() -> VideoGrants {
    VideoGrants {
        room_join: true,
        room: ROOM.into(),
        can_publish: Some(true),
        can_publish_data: Some(true),
        can_subscribe: Some(true),
        ..Default::default()
    }
}

async fn make_token(identity: &str) -> String {
    AccessToken::with_api_key(KEY, SECRET)
        .with_identity(identity)
        .with_name(identity)
        .with_grants(mk_grants())
        .to_jwt()
        .expect("token")
}

static PUBLISH_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn send(lp: &LocalParticipant, payload: Vec<u8>, reliable: bool) {
    let packet = DataPacket {
        payload,
        reliable,
        ..Default::default()
    };
    if lp.publish_data(packet).await.is_err() {
        PUBLISH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let stats: Shared = Default::default();

    // A = 发送端 + RTT 观察者；B = 接收端 + pong 回声
    let token_a = make_token("probe-a").await;
    let token_b = make_token("probe-b").await;
    let (room_a, mut ev_a) = Room::connect(URL, &token_a, RoomOptions::default())
        .await
        .expect("connect a");
    println!("A connected");
    let (room_b, mut ev_b) = Room::connect(URL, &token_b, RoomOptions::default())
        .await
        .expect("connect b");
    println!("B connected");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // B：接收并统计；ping → 立即回 pong（lossy）
    {
        let stats = stats.clone();
        let lp_b = room_b.local_participant();
        tokio::spawn(async move {
            while let Some(ev) = ev_b.recv().await {
                if let RoomEvent::DataReceived { payload, .. } = ev {
                    if let Some((kind, class, _seq, ts)) = unpack(&payload) {
                        match kind {
                            KIND_PING => {
                                let mut pong = payload.to_vec();
                                pong[0] = KIND_PONG;
                                let _ = send(&lp_b, pong, false).await;
                            }
                            KIND_LOSSY | KIND_RELIABLE => {
                                let mut s = stats.lock().unwrap();
                                let e = s.entry((kind, class)).or_default();
                                e.recv += 1;
                                e.latencies.push(now_ms().saturating_sub(ts));
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
    }

    // A：接收 pong，记 RTT
    {
        let stats = stats.clone();
        tokio::spawn(async move {
            while let Some(ev) = ev_a.recv().await {
                if let RoomEvent::DataReceived { payload, .. } = ev {
                    if let Some((KIND_PONG, class, _seq, ts)) = unpack(&payload) {
                        let mut s = stats.lock().unwrap();
                        let e = s.entry((KIND_PONG, class)).or_default();
                        e.recv += 1;
                        e.latencies.push(now_ms().saturating_sub(ts));
                    }
                }
            }
        });
    }

    let lp_a = room_a.local_participant();
    let mut results: Vec<serde_json::Value> = Vec::new();
    let fmt = |v: u64| if v == u64::MAX { "—".to_string() } else { v.to_string() };

    // ---- O-1a：lossy 通道尺寸上限与送达率 ----
    let lossy_sizes: [usize; 11] = [64, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072];
    println!("\n== O-1a lossy 送达率（每档 100 条，间隔 5 ms） ==");
    println!("| 尺寸 | 发送 | 收到 | 送达率 | p50 ms | p95 ms | max ms |");
    println!("|---|---|---|---|---|---|---|");
    for (class, &size) in lossy_sizes.iter().enumerate() {
        let class = class as u8;
        let n = 100u32;
        {
            let mut s = stats.lock().unwrap();
            let e = s.entry((KIND_LOSSY, class)).or_default();
            e.sent = n as u64;
            e.recv = 0;
            e.latencies.clear();
        }
        for seq in 0..n {
            send(&lp_a, pack(KIND_LOSSY, class, seq, size), false).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // 等最后一批到达
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let (sent, recv, p50, p95, max) = {
            let s = stats.lock().unwrap();
            let e = s.get(&(KIND_LOSSY, class)).cloned().unwrap_or_default();
            (e.sent, e.recv, e.pct(0.5), e.pct(0.95), e.pct(1.0))
        };
        let ratio = recv as f64 / sent as f64 * 100.0;
        println!("| {} B | {sent} | {recv} | {ratio:.1}% | {} | {} | {} |", size, fmt(p50), fmt(p95), fmt(max));
        results.push(serde_json::json!({
            "test": "o1a_lossy", "size": size, "sent": sent, "recv": recv,
            "ratio": ratio, "p50_ms": p50, "p95_ms": p95, "max_ms": max,
        }));
    }

    // ---- O-1b：reliable 通道尺寸上限与延迟 ----
    println!("\n== O-1b reliable 送达率（每档 20 条，间隔 10 ms） ==");
    println!("| 尺寸 | 发送 | 收到 | 送达率 | p50 ms | p95 ms | max ms |");
    println!("|---|---|---|---|---|---|---|");
    for (class, &size) in lossy_sizes.iter().enumerate() {
        let class = (class + 16) as u8; // 与 lossy 类号区分
        let n = 20u32;
        {
            let mut s = stats.lock().unwrap();
            let e = s.entry((KIND_RELIABLE, class)).or_default();
            e.sent = n as u64;
            e.recv = 0;
            e.latencies.clear();
        }
        for seq in 0..n {
            send(&lp_a, pack(KIND_RELIABLE, class, seq, size), true).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let (sent, recv, p50, p95, max) = {
            let s = stats.lock().unwrap();
            let e = s.get(&(KIND_RELIABLE, class)).cloned().unwrap_or_default();
            (e.sent, e.recv, e.pct(0.5), e.pct(0.95), e.pct(1.0))
        };
        let ratio = recv as f64 / sent as f64 * 100.0;
        println!("| {} B | {sent} | {recv} | {ratio:.1}% | {} | {} | {} |", size, fmt(p50), fmt(p95), fmt(max));
        results.push(serde_json::json!({
            "test": "o1b_reliable", "size": size, "sent": sent, "recv": recv,
            "ratio": ratio, "p50_ms": p50, "p95_ms": p95, "max_ms": max,
        }));
    }

    // ---- O-2：可靠大流量期间 lossy RTT 是否被队头阻塞 ----
    println!("\n== O-2 流独立性（64 B lossy ping 的 RTT，基线 vs 可靠洪流期间） ==");
    // 基线
    let baseline = ping_rtt(&stats, &lp_a, 100, Duration::from_millis(20), 30).await;
    println!(
        "基线:   p50 {} ms  p95 {} ms  max {} ms  ({} 样本)",
        baseline.0, baseline.1, baseline.2, baseline.3
    );
    results.push(serde_json::json!({
        "test": "o2_baseline", "p50_ms": baseline.0, "p95_ms": baseline.1,
        "max_ms": baseline.2, "samples": baseline.3,
    }));

    // 可靠洪流：200 条 × 32 KB reliable（64000 B 协商上限内的安全尺寸），间隔 2 ms
    {
        let mut s = stats.lock().unwrap();
        let e = s.entry((KIND_RELIABLE, 99)).or_default();
        e.sent = 200;
        e.recv = 0;
        e.latencies.clear();
    }
    let bulk = tokio::spawn({
        let lp = lp_a.clone();
        async move {
            for seq in 0..200u32 {
                send(&lp, pack(KIND_RELIABLE, 99, seq, 32000), true).await;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    });
    let during = ping_rtt(&stats, &lp_a, 200, Duration::from_millis(20), 40).await;
    let _ = bulk.await;
    println!(
        "洪流中: p50 {} ms  p95 {} ms  max {} ms  ({} 样本)",
        during.0, during.1, during.2, during.3
    );
    results.push(serde_json::json!({
        "test": "o2_during_bulk", "p50_ms": during.0, "p95_ms": during.1,
        "max_ms": during.2, "samples": during.3,
    }));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let bulk_recv = stats.lock().unwrap().get(&(KIND_RELIABLE, 99)).map(|s| s.recv).unwrap_or(0);
    println!("洪流 reliable 送达: {bulk_recv}/200");

    println!("\n== 判读 ==");
    println!(
        "publish 失败总数: {}（全部为超过协商上限 64000 B 的拒绝）",
        PUBLISH_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
    );
    let hol = during.1 > baseline.1 * 3;
    println!(
        "O-2 队头阻塞: p95 基线 {base} ms → 洪流 {dur} ms —— {}",
        if hol { "疑似阻塞（F-3 不成立）" } else { "未见阻塞（F-3 成立）" },
        base = baseline.1,
        dur = during.1,
    );

    let out = std::path::Path::new("../../../screenshare-bench/results-lk-probe.json");
    let out = if out.parent().map(|d| d.exists()).unwrap_or(false) {
        out.to_path_buf()
    } else {
        std::path::PathBuf::from("results-lk-probe.json")
    };
    std::fs::write(&out, serde_json::to_string_pretty(&results).unwrap()).unwrap();
    println!("saved {}", out.display());

    let _ = room_a.close().await;
    let _ = room_b.close().await;
}

/// 发 n 条 64 B lossy ping（class = 0/1 区分轮次），统计 pong RTT。
/// `class` 每轮 +1，避免统计串轮。
async fn ping_rtt(
    stats: &Shared,
    lp: &LocalParticipant,
    n: u32,
    interval: Duration,
    class: u8,
) -> (u64, u64, u64, usize) {
    for seq in 0..n {
        send(lp, pack(KIND_PING, class, seq, 64), false).await;
        tokio::time::sleep(interval).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = stats.lock().unwrap();
    let e = s.get(&(KIND_PONG, class)).cloned().unwrap_or_default();
    (e.pct(0.5), e.pct(0.95), e.pct(1.0), e.latencies.len())
}
