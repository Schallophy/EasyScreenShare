//! 线格式（protobuf）<-> 管线消息的转换。
//!
//! 传输层只认识字节；本模块负责模型语义 + 块像素负载的打包/解包。
//! 单条消息的尺寸上限由传输层负责（实测 64000 B，见 O-1）。

use crate::codec::TileCodecId;
use crate::frame::TileGrid;
use crate::model::TileId;
use crate::proto;

/// 一块的线上形态：模型字段 + 编码后的像素。
#[derive(Clone, Debug, PartialEq)]
pub struct WireTile {
    pub id: TileId,
    pub codec: TileCodecId,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub epoch: u32,
    pub version: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WireMsg {
    Update {
        seq: u32,
        layout_id: u32,
        cols: u16,
        rows: u16,
        tiles: Vec<WireTile>,
        /// true = 运动退出的精确修复：走可靠通道（见 TilesUpdate.repair）
        repair: bool,
    },
    Scroll {
        seq: u32,
        layout_id: u32,
        scroll_id: u32,
        dy: i32,
        /// 全部平移块 → 目标 epoch
        targets: Vec<(TileId, u32)>,
        /// 将重发的条带块
        dirty: Vec<TileId>,
    },
    Full {
        seq: u32,
        layout_id: u32,
        frame_id: u32,
        w: u32,
        h: u32,
        codec: TileCodecId,
        versions: Vec<u32>,
        epochs: Vec<u32>,
        /// 按块 id 顺序
        payloads: Vec<Vec<u8>>,
    },
    State {
        layout_id: u32,
        cols: u16,
        rows: u16,
        versions: Vec<u32>,
        epochs: Vec<u32>,
    },
    Nack {
        from_seq: u32,
        to_seq: u32,
    },
    /// 运动帧：整帧降采样 JPEG（有损）。静态/文字仍走 Update。
    Motion {
        seq: u32,
        layout_id: u32,
        frame_id: u32,
        w: u32,
        h: u32,
        data: Vec<u8>,
    },
    /// 平面切换：视频轨开始/结束。
    VideoMode {
        seq: u32,
        layout_id: u32,
        on: bool,
    },
    /// 大消息分片（编码后的整帧等），由传输层按 msg_id 重组。
    Chunk {
        msg_id: u32,
        idx: u32,
        total: u32,
        data: Vec<u8>,
    },
}

#[derive(Debug)]
pub enum WireError {
    Decode(prost::DecodeError),
    Encode(prost::EncodeError),
    /// 向量长度不一致等结构性错误
    Malformed(&'static str),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "decode: {e}"),
            Self::Encode(e) => write!(f, "encode: {e}"),
            Self::Malformed(s) => write!(f, "malformed: {s}"),
        }
    }
}

fn codec_to_proto(c: TileCodecId) -> i32 {
    match c {
        TileCodecId::WebpLossless => proto::TileCodec::WebpLossless as i32,
        TileCodecId::PaletteZstd => proto::TileCodec::PaletteZstd as i32,
        TileCodecId::Png => proto::TileCodec::Png as i32,
        TileCodecId::Raw => proto::TileCodec::Raw as i32,
    }
}

fn codec_from_proto(v: i32) -> TileCodecId {
    match v {
        v if v == proto::TileCodec::PaletteZstd as i32 => TileCodecId::PaletteZstd,
        v if v == proto::TileCodec::Png as i32 => TileCodecId::Png,
        v if v == proto::TileCodec::Raw as i32 => TileCodecId::Raw,
        _ => TileCodecId::WebpLossless,
    }
}

fn header(seq: u32, layout_id: u32) -> proto::Header {
    proto::Header { seq, ts_us: 0, layout_id }
}

pub fn encode(msg: &WireMsg) -> Result<Vec<u8>, WireError> {
    let body = match msg {
        WireMsg::Update { seq, layout_id, cols, rows, tiles, repair } => {
            proto::envelope::Body::Update(proto::TilesUpdate {
                hdr: Some(header(*seq, *layout_id)),
                cols: *cols as u32,
                rows: *rows as u32,
                repair: *repair,
                tiles: tiles
                    .iter()
                    .map(|t| proto::Tile {
                        id: t.id.0 as u32,
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        epoch: t.epoch,
                        version: t.version,
                        codec: codec_to_proto(t.codec),
                        payload: t.payload.clone(),
                        meta_mac: Vec::new(),
                    })
                    .collect(),
                scroll: None,
            })
        }
        WireMsg::Scroll { seq, layout_id, scroll_id, dy, targets, dirty } => {
            proto::envelope::Body::Scroll(proto::Scroll {
                hdr: Some(header(*seq, *layout_id)),
                scroll_id: *scroll_id,
                info: Some(proto::ScrollInfo {
                    rect: None,
                    dy: *dy,
                    affected_tiles: targets.iter().map(|(id, _)| id.0 as u32).collect(),
                    epochs: targets.iter().map(|(_, e)| *e).collect(),
                    dirty_tiles: dirty.iter().map(|id| id.0 as u32).collect(),
                }),
            })
        }
        WireMsg::Full { seq, layout_id, frame_id, w, h, codec, versions, epochs, payloads } => {
            let mut data = Vec::with_capacity(payloads.iter().map(|p| p.len()).sum());
            let mut lens = Vec::with_capacity(payloads.len());
            for p in payloads {
                data.extend_from_slice(p);
                lens.push(p.len() as u32);
            }
            proto::envelope::Body::Full(proto::FullFrame {
                hdr: Some(header(*seq, *layout_id)),
                frame_id: *frame_id,
                idx: 0,
                total: 1,
                w: *w,
                h: *h,
                codec: codec_to_proto(*codec),
                data,
                version_vec: versions.clone(),
                meta_mac: Vec::new(),
                epochs: epochs.clone(),
                tile_lens: lens,
            })
        }
        WireMsg::State { layout_id, cols, rows, versions, epochs } => {
            proto::envelope::Body::State(proto::StateVector {
                hdr: Some(header(0, *layout_id)),
                tile_size: crate::TILE,
                cols: *cols as u32,
                rows: *rows as u32,
                versions: versions.clone(),
                epochs: epochs.clone(),
            })
        }
        WireMsg::Nack { from_seq, to_seq } => proto::envelope::Body::Nack(proto::Nack {
            hdr: Some(header(0, 0)),
            from_seq: *from_seq,
            to_seq: *to_seq,
            tiles: Vec::new(),
            attempt: 0,
        }),
        WireMsg::Motion { seq, layout_id, frame_id, w, h, data } => {
            proto::envelope::Body::Motion(proto::MotionFrame {
                hdr: Some(header(*seq, *layout_id)),
                frame_id: *frame_id,
                w: *w,
                h: *h,
                jpeg: data.clone(),
            })
        }
        WireMsg::VideoMode { seq, layout_id, on } => {
            proto::envelope::Body::Video(proto::VideoMode {
                hdr: Some(header(*seq, *layout_id)),
                on: *on,
            })
        }
        WireMsg::Chunk { msg_id, idx, total, data } => {
            proto::envelope::Body::Chunk(proto::DataChunk {
                msg_id: *msg_id,
                idx: *idx,
                total: *total,
                data: data.clone(),
            })
        }
    };
    let env = proto::Envelope { body: Some(body) };
    let mut out = Vec::new();
    prost::Message::encode(&env, &mut out).map_err(WireError::Encode)?;
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<WireMsg, WireError> {
    let env = <proto::Envelope as prost::Message>::decode(bytes).map_err(WireError::Decode)?;
    let body = env.body.ok_or(WireError::Malformed("empty envelope"))?;
    match body {
        proto::envelope::Body::Update(u) => {
            let hdr = u.hdr.unwrap_or_default();
            Ok(WireMsg::Update {
                seq: hdr.seq,
                layout_id: hdr.layout_id,
                cols: u.cols as u16,
                rows: u.rows as u16,
                repair: u.repair,
                tiles: u
                    .tiles
                    .into_iter()
                    .map(|t| WireTile {
                        id: TileId(t.id as u16),
                        codec: codec_from_proto(t.codec),
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        epoch: t.epoch,
                        version: t.version,
                        payload: t.payload,
                    })
                    .collect(),
            })
        }
        proto::envelope::Body::Scroll(s) => {
            let hdr = s.hdr.unwrap_or_default();
            let info = s.info.ok_or(WireError::Malformed("scroll without info"))?;
            if info.affected_tiles.len() != info.epochs.len() {
                return Err(WireError::Malformed("scroll targets/epochs length mismatch"));
            }
            Ok(WireMsg::Scroll {
                seq: hdr.seq,
                layout_id: hdr.layout_id,
                scroll_id: s.scroll_id,
                dy: info.dy,
                targets: info
                    .affected_tiles
                    .iter()
                    .zip(info.epochs.iter())
                    .map(|(&id, &e)| (TileId(id as u16), e))
                    .collect(),
                dirty: info.dirty_tiles.iter().map(|&id| TileId(id as u16)).collect(),
            })
        }
        proto::envelope::Body::Full(f) => {
            let hdr = f.hdr.unwrap_or_default();
            let n = f.version_vec.len();
            if f.epochs.len() != n || f.tile_lens.len() != n {
                return Err(WireError::Malformed("full frame vectors length mismatch"));
            }
            let mut payloads = Vec::with_capacity(n);
            let mut off = 0usize;
            for &len in &f.tile_lens {
                let len = len as usize;
                if off + len > f.data.len() {
                    return Err(WireError::Malformed("full frame data truncated"));
                }
                payloads.push(f.data[off..off + len].to_vec());
                off += len;
            }
            Ok(WireMsg::Full {
                seq: hdr.seq,
                layout_id: hdr.layout_id,
                frame_id: f.frame_id,
                w: f.w,
                h: f.h,
                codec: codec_from_proto(f.codec),
                versions: f.version_vec,
                epochs: f.epochs,
                payloads,
            })
        }
        proto::envelope::Body::State(s) => {
            let hdr = s.hdr.unwrap_or_default();
            Ok(WireMsg::State {
                layout_id: hdr.layout_id,
                cols: s.cols as u16,
                rows: s.rows as u16,
                versions: s.versions,
                epochs: s.epochs,
            })
        }
        proto::envelope::Body::Nack(n) => {
            Ok(WireMsg::Nack { from_seq: n.from_seq, to_seq: n.to_seq })
        }
        proto::envelope::Body::Motion(m) => {
            let hdr = m.hdr.unwrap_or_default();
            Ok(WireMsg::Motion {
                seq: hdr.seq,
                layout_id: hdr.layout_id,
                frame_id: m.frame_id,
                w: m.w,
                h: m.h,
                data: m.jpeg,
            })
        }
        proto::envelope::Body::Video(v) => {
            let hdr = v.hdr.unwrap_or_default();
            Ok(WireMsg::VideoMode { seq: hdr.seq, layout_id: hdr.layout_id, on: v.on })
        }
        proto::envelope::Body::Chunk(c) => Ok(WireMsg::Chunk {
            msg_id: c.msg_id,
            idx: c.idx,
            total: c.total,
            data: c.data,
        }),
    }
}

/// 线格式里的 Full 提供网格（由 w/h 推出）。
pub fn grid_of_full(w: u32, h: u32) -> TileGrid {
    TileGrid::for_frame(w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_roundtrip() {
        let msg = WireMsg::Update {
            seq: 7,
            layout_id: 3,
            cols: 30,
            rows: 17,
            repair: true,
            tiles: vec![WireTile {
                id: TileId(42),
                codec: TileCodecId::PaletteZstd,
                x: 128,
                y: 64,
                w: 64,
                h: 64,
                epoch: 5,
                version: 9,
                payload: vec![1, 2, 3, 4],
            }],
        };
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn scroll_roundtrip() {
        let msg = WireMsg::Scroll {
            seq: 1,
            layout_id: 2,
            scroll_id: 0,
            dy: -37,
            targets: vec![(TileId(0), 4), (TileId(1), 4)],
            dirty: vec![TileId(1)],
        };
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn full_roundtrip() {
        let msg = WireMsg::Full {
            seq: 2,
            layout_id: 5,
            frame_id: 1,
            w: 640,
            h: 480,
            codec: TileCodecId::Raw,
            versions: vec![1, 2, 3],
            epochs: vec![9, 9, 9],
            payloads: vec![vec![0xaa; 5], vec![], vec![0xbb; 7]],
        };
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn malformed_full_rejected() {
        let mut f = proto::FullFrame {
            hdr: Some(header(0, 1)),
            frame_id: 0,
            idx: 0,
            total: 1,
            w: 64,
            h: 64,
            codec: proto::TileCodec::Raw as i32,
            data: vec![1, 2],
            version_vec: vec![1, 2],
            meta_mac: vec![],
            epochs: vec![1],
            tile_lens: vec![1, 1],
        };
        let env = proto::Envelope { body: Some(proto::envelope::Body::Full(f.clone())) };
        let mut bytes = Vec::new();
        prost::Message::encode(&env, &mut bytes).unwrap();
        assert!(matches!(decode(&bytes), Err(WireError::Malformed(_))));
        // data 截断
        f.epochs = vec![1, 1];
        f.tile_lens = vec![1, 9];
        let env = proto::Envelope { body: Some(proto::envelope::Body::Full(f)) };
        let mut bytes = Vec::new();
        prost::Message::encode(&env, &mut bytes).unwrap();
        assert!(matches!(decode(&bytes), Err(WireError::Malformed(_))));
    }
}
