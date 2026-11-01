//! 调色板 + RLE + zstd 图块编解码（纯 Rust，跨平台；WASM 时 zstd 需换 ruzstd，见 O-14）。
//!
//! 格式（zstd 压缩前的体）：
//! ```text
//! [0]      version = 1
//! [1]      bits = 4（保留字段；索引按 1 字节存储）
//! [2..4]   palette_len u16 LE（1..=256）
//! [4..6]   w u16 LE
//! [6..8]   h u16 LE
//! [8..]    palette：palette_len × BGRA
//! [..]     索引流：RLE，[run u8 (1..=255)][idx u8]，按行主序铺满 w×h
//! ```
//!
//! 宽高必须在格式里：解码方给的是带 stride 的缓冲，不能从 stride 反推块宽。

use ess_core::codec::{CodecError, TileCodec, TileCodecId};

const VERSION: u8 = 1;
const MAX_PALETTE: usize = 256;

pub struct PaletteZstdCodec {
    level: i32,
}

impl PaletteZstdCodec {
    pub fn new(level: i32) -> Self {
        Self { level }
    }
}

impl Default for PaletteZstdCodec {
    fn default() -> Self {
        Self::new(1)
    }
}

fn palette_index(pal: &[u32], color: u32) -> Option<u8> {
    pal.iter().position(|&c| c == color).map(|i| i as u8)
}

#[cfg(not(target_arch = "wasm32"))]
fn compress(body: &[u8], level: i32) -> Result<Vec<u8>, CodecError> {
    zstd::bulk::compress(body, level).map_err(|e| CodecError::Backend(e.to_string()))
}

/// 浏览器侧只做解码；编码由采集端（原生）负责。
#[cfg(target_arch = "wasm32")]
fn compress(_body: &[u8], _level: i32) -> Result<Vec<u8>, CodecError> {
    Err(CodecError::Unsupported)
}

#[cfg(not(target_arch = "wasm32"))]
fn decompress(data: &[u8], cap: usize) -> Result<Vec<u8>, CodecError> {
    zstd::bulk::decompress(data, cap).map_err(|_| CodecError::Corrupt("zstd decompress failed"))
}

/// ruzstd 是纯 Rust 的 zstd 解码器，与原生端产出的标准帧格式兼容。
#[cfg(target_arch = "wasm32")]
fn decompress(data: &[u8], cap: usize) -> Result<Vec<u8>, CodecError> {
    use std::io::Read;
    let dec = ruzstd::StreamingDecoder::new(data)
        .map_err(|_| CodecError::Corrupt("zstd stream init failed"))?;
    let mut out = Vec::new();
    dec.take(cap as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| CodecError::Corrupt("zstd decompress failed"))?;
    if out.len() > cap {
        return Err(CodecError::Corrupt("zstd output exceeds cap"));
    }
    Ok(out)
}

impl TileCodec for PaletteZstdCodec {
    fn encode(&self, raw: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CodecError> {
        let px = w as usize * h as usize;
        if raw.len() < px * 4 {
            return Err(CodecError::BadGeometry);
        }
        // 收集调色板
        let mut pal: Vec<u32> = Vec::with_capacity(64);
        let mut indices = Vec::with_capacity(px);
        for i in 0..px {
            let color = u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap());
            let idx = match palette_index(&pal, color) {
                Some(i) => i,
                None => {
                    if pal.len() == MAX_PALETTE {
                        return Err(CodecError::Unsupported);
                    }
                    pal.push(color);
                    (pal.len() - 1) as u8
                }
            };
            indices.push(idx);
        }
        // RLE
        let mut rle = Vec::with_capacity(px / 2);
        let mut i = 0;
        while i < indices.len() {
            let v = indices[i];
            let mut run = 1u8;
            while i + (run as usize) < indices.len()
                && indices[i + run as usize] == v
                && run < u8::MAX
            {
                run += 1;
            }
            rle.push(run);
            rle.push(v);
            i += run as usize;
        }
        let mut body = Vec::with_capacity(8 + pal.len() * 4 + rle.len());
        body.push(VERSION);
        body.push(4);
        body.extend_from_slice(&(pal.len() as u16).to_le_bytes());
        body.extend_from_slice(&(w as u16).to_le_bytes());
        body.extend_from_slice(&(h as u16).to_le_bytes());
        for c in &pal {
            body.extend_from_slice(&c.to_le_bytes());
        }
        body.extend_from_slice(&rle);
        compress(&body, self.level)
    }

    fn decode_into(&self, data: &[u8], dst: &mut [u8], stride: u32) -> Result<(), CodecError> {
        if stride < 4 || stride % 4 != 0 {
            return Err(CodecError::BadDst);
        }
        let w = stride as usize / 4;
        let rows = dst.len() / stride as usize;
        if rows == 0 || dst.len() % stride as usize != 0 {
            return Err(CodecError::BadDst);
        }
        // 解压上限：头 + 最大调色板 + 最坏 RLE（2 字节/像素）
        let cap = 8 + MAX_PALETTE * 4 + w * rows * 2;
        let body = decompress(data, cap)?;
        if body.len() < 8 || body[0] != VERSION {
            return Err(CodecError::Corrupt("bad header"));
        }
        let pal_len = u16::from_le_bytes([body[2], body[3]]) as usize;
        let tw = u16::from_le_bytes([body[4], body[5]]) as usize;
        let th = u16::from_le_bytes([body[6], body[7]]) as usize;
        if pal_len == 0 || pal_len > MAX_PALETTE {
            return Err(CodecError::Corrupt("bad palette len"));
        }
        if tw == 0 || th == 0 || tw > w || th > rows {
            return Err(CodecError::Corrupt("tile exceeds dst"));
        }
        let px = tw * th;
        let pal_start = 8;
        let pal_end = pal_start + pal_len * 4;
        if body.len() < pal_end {
            return Err(CodecError::Corrupt("truncated palette"));
        }
        let pal = &body[pal_start..pal_end];
        let rle = &body[pal_end..];

        let row_bytes = stride as usize;
        let mut y = 0usize;
        let mut x = 0usize;
        let mut i = 0usize;
        let mut written = 0usize;
        while i + 1 < rle.len() {
            let run = rle[i] as usize;
            let idx = rle[i + 1] as usize;
            if run == 0 || idx >= pal_len {
                return Err(CodecError::Corrupt("bad rle"));
            }
            if written + run > px {
                return Err(CodecError::Corrupt("rle overflow"));
            }
            let c = &pal[idx * 4..idx * 4 + 4];
            for _ in 0..run {
                if x == tw {
                    x = 0;
                    y += 1;
                }
                if y >= th {
                    return Err(CodecError::Corrupt("too many pixels"));
                }
                let s = y * row_bytes + x * 4;
                dst[s..s + 4].copy_from_slice(c);
                x += 1;
                written += 1;
            }
            i += 2;
        }
        if written != px {
            return Err(CodecError::Corrupt("not enough pixels"));
        }
        Ok(())
    }

    fn id(&self) -> TileCodecId {
        TileCodecId::PaletteZstd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_roundtrip(w: u32, h: u32, colors: &[u32]) {
        let mut raw = Vec::with_capacity((w * h * 4) as usize);
        for i in 0..(w * h) as usize {
            raw.extend_from_slice(&colors[i % colors.len()].to_le_bytes());
        }
        let codec = PaletteZstdCodec::default();
        let enc = codec.encode(&raw, w, h).unwrap();
        let mut dst = vec![0u8; (w * h * 4) as usize];
        codec.decode_into(&enc, &mut dst, w * 4).unwrap();
        assert_eq!(dst, raw, "roundtrip mismatch {w}x{h}");
    }

    #[test]
    fn roundtrip_simple() {
        run_roundtrip(16, 16, &[0x0011_2233, 0xff44_5566, 0x0000_0000]);
        run_roundtrip(64, 64, &[0x0000_00ff]);
        run_roundtrip(5, 3, &[1, 2, 3, 4]);
    }

    #[test]
    fn roundtrip_256_colors() {
        let colors: Vec<u32> = (0..256u32).map(|i| i * 0x0001_0101).collect();
        run_roundtrip(16, 16, &colors);
    }

    #[test]
    fn too_many_colors_rejected() {
        let (_w, _h) = (32u32, 32u32);
        let colors: Vec<u32> = (0..300u32).collect();
        let mut raw = Vec::new();
        for i in 0..1024usize {
            raw.extend_from_slice(&colors[i % colors.len()].to_le_bytes());
        }
        let codec = PaletteZstdCodec::default();
        assert!(matches!(codec.encode(&raw, 32, 32), Err(CodecError::Unsupported)));
    }

    #[test]
    fn corrupt_input_rejected() {
        let codec = PaletteZstdCodec::default();
        let mut dst = vec![0u8; 16 * 16 * 4];
        assert!(codec.decode_into(&[0, 1, 2], &mut dst, 64).is_err());
        let junk = vec![0xaa; 64];
        assert!(codec.decode_into(&junk, &mut dst, 64).is_err());
    }

    #[test]
    fn stride_handling() {
        // stride 大于 w*4（行尾补齐）
        let codec = PaletteZstdCodec::default();
        let (w, h) = (5u32, 4u32);
        let mut raw = Vec::new();
        for i in 0..(w * h) as usize {
            raw.extend_from_slice(&(i as u32).to_le_bytes());
        }
        let enc = codec.encode(&raw, w, h).unwrap();
        let stride = 32usize; // 8 像素宽
        let mut dst = vec![0u8; stride * h as usize];
        codec.decode_into(&enc, &mut dst, stride as u32).unwrap();
        for y in 0..h as usize {
            assert_eq!(&dst[y * stride..y * stride + w as usize * 4], &raw[y * w as usize * 4..(y + 1) * w as usize * 4]);
        }
    }
}
