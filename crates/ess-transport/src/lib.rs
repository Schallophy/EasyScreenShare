//! LiveKit 传输层：连接、令牌、分片发送、分片重组。
//!
//! 通道约定：
//! - **不可靠（lossy）**：逐块更新（已按 ≤ `MAX_MESSAGE_BYTES` 拆批，每条自包含）；
//! - **可靠（reliable）**：滚动宣告、状态报告、整帧分片、NACK。
//!
//! 协商上限实测 64000 B（O-1）；可靠大块会队头阻塞 lossy（O-2），
//! 因此整帧分片走可靠通道，且只在布局变更时发送。

use std::collections::HashMap;

use livekit::prelude::*;
use ess_core::wire::{self, WireMsg};

/// 单条数据消息的载荷预算。64000 是协商上限，留出分片头与协议开销。
pub const MAX_MESSAGE_BYTES: usize = 60_000;
/// 每条消息的固定开销估计（proto 字段 + Tile 头）。
const PER_TILE_OVERHEAD: usize = 40;

/// 测量台：`ESS_MSG_BYTES=N` 覆盖单条消息预算，让拆批粒度的对照臂跑在同一份二进制里
/// （上一轮改常量→重编译→改回来，对照臂之间隔着一次构建，任务中断时还得靠人把常量复位）。
/// 不设就是 `MAX_MESSAGE_BYTES`；产品启动不设置这个变量，取的就是那个常量。
pub fn message_budget() -> usize {
    static B: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        std::env::var("ESS_MSG_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| (1024..=64_000).contains(n))
            .unwrap_or(MAX_MESSAGE_BYTES)
    })
}

pub fn token(api_key: &str, api_secret: &str, room: &str, identity: &str) -> String {
    use livekit_token::{AccessToken, VideoGrants};
    AccessToken::with_api_key(api_key, api_secret)
        .with_identity(identity)
        .with_name(identity)
        .with_grants(VideoGrants {
            room_join: true,
            room: room.to_string(),
            can_publish: Some(true),
            can_publish_data: Some(true),
            can_subscribe: Some(true),
            ..Default::default()
        })
        .to_jwt()
        .expect("token")
}

pub struct Conn {
    pub room: Room,
    pub events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
}

pub async fn connect(url: &str, token: &str) -> Result<Conn, String> {
    let (room, events) = Room::connect(url, token, RoomOptions::default())
        .await
        .map_err(|e| format!("connect: {e}"))?;
    Ok(Conn { room, events })
}

/// 把一条 Update 按块拆成多条自包含消息（每条都带完整头，可独立应用）。
pub fn split_update(msg: &WireMsg) -> Vec<WireMsg> {
    let WireMsg::Update { seq, layout_id, cols, rows, tiles, repair } = msg else {
        return vec![msg.clone()];
    };
    let mut out = Vec::new();
    let mut batch: Vec<ess_core::wire::WireTile> = Vec::new();
    let mut size = 0usize;
    for t in tiles {
        let t_size = t.payload.len() + PER_TILE_OVERHEAD;
        if !batch.is_empty() && size + t_size > message_budget() {
            out.push(WireMsg::Update {
                seq: *seq,
                layout_id: *layout_id,
                cols: *cols,
                rows: *rows,
                repair: *repair,
                tiles: std::mem::take(&mut batch),
            });
            size = 0;
        }
        batch.push(t.clone());
        size += t_size;
    }
    if !batch.is_empty() {
        out.push(WireMsg::Update {
            seq: *seq,
            layout_id: *layout_id,
            cols: *cols,
            rows: *rows,
            repair: *repair,
            tiles: batch,
        });
    }
    out
}

/// 整帧分片（可靠通道）。
pub fn chunk_full(msg: &WireMsg, msg_id: u32) -> Result<Vec<WireMsg>, String> {
    let bytes = wire::encode(msg).map_err(|e| e.to_string())?;
    let budget = message_budget();
    let total = bytes.len().div_ceil(budget).max(1) as u32;
    let mut out = Vec::with_capacity(total as usize);
    for (idx, part) in bytes.chunks(budget).enumerate() {
        out.push(WireMsg::Chunk {
            msg_id,
            idx: idx as u32,
            total,
            data: part.to_vec(),
        });
    }
    Ok(out)
}

/// 分片重组：可靠通道有序到达；仍容忍乱序与重复。
#[derive(Default)]
pub struct Assembler {
    partial: HashMap<u32, (u32, Vec<Option<Vec<u8>>>)>,
}

impl Assembler {
    pub fn push(&mut self, msg_id: u32, idx: u32, total: u32, data: Vec<u8>) -> Option<Vec<u8>> {
        if idx >= total || total as usize > 10_000 {
            return None;
        }
        let entry = self
            .partial
            .entry(msg_id)
            .or_insert_with(|| (total, vec![None; total as usize]));
        if entry.0 != total {
            self.partial.remove(&msg_id);
            return None;
        }
        entry.1[idx as usize] = Some(data);
        if entry.1.iter().all(|p| p.is_some()) {
            let (_, parts) = self.partial.remove(&msg_id)?;
            let mut out = Vec::new();
            for p in parts {
                out.extend_from_slice(&p.unwrap());
            }
            return Some(out);
        }
        None
    }
}

/// 测量台：`ESS_DROP_EVERY=N` → 每第 N 条**不可靠**包在发出前丢掉。位置必须在 `send_wire`
/// 里面：一条 255 块的 Update 在会话侧只是**一条消息**，140 个（或 38 个）包是这里
/// `split_update` 才拆出来的——上一版把注入放在会话侧，四臂 `inj` 全是 0，量的根本不是丢包。
/// 只注入不可靠通路；可靠通路（整帧、滚动宣告、精确修复）的失败模式是队头阻塞，不是丢包。
/// 发送端对被丢的包不留痕迹（不记块 id、不报错），跟真实链路一样，只能靠观看端报告发现。
pub static DROPPED_LOSSY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LOSSY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn drop_this_lossy() -> bool {
    static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let every = *EVERY.get_or_init(|| {
        std::env::var("ESS_DROP_EVERY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    });
    if every == 0 {
        return false;
    }
    let n = LOSSY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n % every == 0 {
        DROPPED_LOSSY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return true;
    }
    false
}

/// 发送一条线消息（自动拆批/分片）。`next_msg_id` 只在分片时使用。
pub async fn send_wire(
    lp: &LocalParticipant,
    msg: &WireMsg,
    next_msg_id: &mut u32,
) -> Result<(), String> {
    match msg {
        WireMsg::Update { repair, .. } => {
            // 普通逐块更新丢了由状态报告兜底（lossy，不排队、不队头阻塞）；
            // 运动退出的精确修复必须可靠送达（观看端把 off 当作"画布已正确"）。
            // 逐条 await 不是疏忽：并发排空（8 路 join_all）实测换不到墙钟时间，
            // 排空是按字节铺开的，见 docs/IMPLEMENTATION.md「并发排空被配对跑否掉」。
            for part in split_update(msg) {
                if !*repair && drop_this_lossy() {
                    continue;
                }
                let bytes = wire::encode(&part).map_err(|e| e.to_string())?;
                let pkt = DataPacket { payload: bytes, reliable: *repair, ..Default::default() };
                lp.publish_data(pkt).await.map_err(|e| format!("publish: {e}"))?;
            }
        }
        WireMsg::Motion { .. } => {
            // 运动帧：单条不可靠消息。发送端已保证 ≤ 60 KB。
            if drop_this_lossy() {
                return Ok(());
            }
            let bytes = wire::encode(msg).map_err(|e| e.to_string())?;
            let pkt = DataPacket { payload: bytes, reliable: false, ..Default::default() };
            lp.publish_data(pkt).await.map_err(|e| format!("publish: {e}"))?;
        }
        WireMsg::Full { .. } => {
            *next_msg_id += 1;
            for part in chunk_full(msg, *next_msg_id)? {
                let bytes = wire::encode(&part).map_err(|e| e.to_string())?;
                let pkt = DataPacket { payload: bytes, reliable: true, ..Default::default() };
                lp.publish_data(pkt).await.map_err(|e| format!("publish: {e}"))?;
            }
        }
        other => {
            let bytes = wire::encode(other).map_err(|e| e.to_string())?;
            let reliable = !matches!(other, WireMsg::Update { .. });
            let pkt = DataPacket { payload: bytes, reliable, ..Default::default() };
            lp.publish_data(pkt).await.map_err(|e| format!("publish: {e}"))?;
        }
    }
    Ok(())
}

/// 收到一条数据消息 → 线消息（分片自动重组后解码）。
pub fn decode_incoming(payload: &[u8], asm: &mut Assembler) -> Option<WireMsg> {
    let msg = wire::decode(payload).ok()?;
    match msg {
        WireMsg::Chunk { msg_id, idx, total, data } => {
            let bytes = asm.push(msg_id, idx, total, data)?;
            wire::decode(&bytes).ok()
        }
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ess_core::codec::TileCodecId;
    use ess_core::model::TileId;
    use ess_core::wire::WireTile;

    fn tile(id: u16, size: usize) -> WireTile {
        WireTile {
            id: TileId(id),
            codec: TileCodecId::Raw,
            x: 0,
            y: 0,
            w: 64,
            h: 64,
            epoch: 0,
            version: 1,
            payload: vec![7u8; size],
        }
    }

    #[test]
    fn update_split_respects_budget() {
        // 夹具按常量推出来，别把某个具体数字当契约：钉的是"超过上限必须拆、没超过就别拆、
        // 拆完一块不丢、每条都不越界"。上限从 16 KB 抬到 60 KB 时，原来那句
        // "100×440 B 必须拆"的前提自己就不成立了（实测把这条测试压红过）。
        let per = 400;
        let unit = per + PER_TILE_OVERHEAD;
        let needed = MAX_MESSAGE_BYTES / unit + 1;
        let msg = WireMsg::Update {
            seq: 1,
            layout_id: 1,
            cols: 30,
            rows: 17,
            repair: false,
            tiles: (0..needed).map(|i| tile(i as u16, per)).collect(),
        };
        let parts = split_update(&msg);
        assert!(
            parts.len() > 1,
            "{needed} 块 × {unit} B 超过 {MAX_MESSAGE_BYTES} B 上限，必须拆"
        );

        // 反向：刚好装得下就不该拆
        let fits = MAX_MESSAGE_BYTES / unit;
        let small = WireMsg::Update {
            seq: 2,
            layout_id: 1,
            cols: 30,
            rows: 17,
            repair: false,
            tiles: (0..fits).map(|i| tile(i as u16, per)).collect(),
        };
        assert_eq!(split_update(&small).len(), 1, "装得下却拆了");

        let total: usize = parts
            .iter()
            .map(|p| {
                let WireMsg::Update { tiles, .. } = p else { panic!() };
                tiles.iter().map(|t| t.payload.len() + PER_TILE_OVERHEAD).sum::<usize>()
            })
            .sum();
        assert_eq!(total, needed * unit, "拆包不许丢块");
        for p in &parts {
            let bytes = wire::encode(p).unwrap();
            assert!(bytes.len() <= MAX_MESSAGE_BYTES + 512, "part too big: {}", bytes.len());
        }
    }

    #[test]
    fn full_chunks_reassemble() {
        let msg = WireMsg::Full {
            seq: 1,
            layout_id: 2,
            frame_id: 1,
            w: 640,
            h: 480,
            codec: TileCodecId::Raw,
            codecs: Vec::new(),
            versions: vec![0; 80],
            epochs: vec![0; 80],
            payloads: (0..80).map(|_| vec![9u8; 1000]).collect(),
        };
        let chunks = chunk_full(&msg, 42).unwrap();
        assert!(chunks.len() > 1);
        let mut asm = Assembler::default();
        let mut got = None;
        for c in chunks {
            let bytes = wire::encode(&c).unwrap();
            if let Some(w) = decode_incoming(&bytes, &mut asm) {
                got = Some(w);
            }
        }
        assert_eq!(got.unwrap(), msg);
    }

    #[test]
    fn assembler_tolerates_duplicates_and_reorder() {
        let mut asm = Assembler::default();
        assert!(asm.push(1, 0, 3, vec![1]).is_none());
        assert!(asm.push(1, 2, 3, vec![3]).is_none());
        assert!(asm.push(1, 0, 3, vec![1]).is_none()); // 重复
        assert_eq!(asm.push(1, 1, 3, vec![2]), Some(vec![1, 2, 3]));
    }
}
