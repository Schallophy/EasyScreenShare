//! LiveKit 传输层：连接、令牌、分片发送、分片重组。
//!
//! 通道约定：
//! - **不可靠（lossy）**：逐块更新（已按 ≤ ~16 KB 拆批，每条自包含）；
//! - **可靠（reliable）**：滚动宣告、状态报告、整帧分片、NACK。
//!
//! 协商上限实测 64000 B（O-1）；可靠大块会队头阻塞 lossy（O-2），
//! 因此整帧分片走可靠通道，且只在布局变更时发送。

use std::collections::HashMap;

use livekit::prelude::*;
use ess_core::wire::{self, WireMsg};

/// 单条数据消息的载荷预算。64000 是协商上限，留出分片头与协议开销。
pub const MAX_MESSAGE_BYTES: usize = 16_000;
/// 每条消息的固定开销估计（proto 字段 + Tile 头）。
const PER_TILE_OVERHEAD: usize = 40;

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
        if !batch.is_empty() && size + t_size > MAX_MESSAGE_BYTES {
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
    let total = bytes.len().div_ceil(MAX_MESSAGE_BYTES).max(1) as u32;
    let mut out = Vec::with_capacity(total as usize);
    for (idx, part) in bytes.chunks(MAX_MESSAGE_BYTES).enumerate() {
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
            for part in split_update(msg) {
                let bytes = wire::encode(&part).map_err(|e| e.to_string())?;
                let pkt =
                    DataPacket { payload: bytes, reliable: *repair, ..Default::default() };
                lp.publish_data(pkt).await.map_err(|e| format!("publish: {e}"))?;
            }
        }
        WireMsg::Motion { .. } => {
            // 运动帧：单条不可靠消息。发送端已保证 ≤ 60 KB。
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
        let msg = WireMsg::Update {
            seq: 1,
            layout_id: 1,
            cols: 30,
            rows: 17,
            repair: false,
            tiles: (0..100).map(|i| tile(i, 400)).collect(),
        };
        let parts = split_update(&msg);
        assert!(parts.len() > 1, "100×440 B must split");
        let total: usize = parts
            .iter()
            .map(|p| {
                let WireMsg::Update { tiles, .. } = p else { panic!() };
                tiles.iter().map(|t| t.payload.len() + PER_TILE_OVERHEAD).sum::<usize>()
            })
            .sum();
        assert_eq!(total, 100 * (400 + PER_TILE_OVERHEAD));
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
