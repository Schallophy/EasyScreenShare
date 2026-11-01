//! PNG 图块编解码（纯 Rust，跨平台；`png` crate 可编译到 wasm32）。
//!
//! 用途：调色板编码器拒绝的块（>256 色，真实屏幕的照片/抗锯齿区域）。
//! 无损，体积介于调色板与 Raw 之间。

use ess_core::codec::{CodecError, TileCodec, TileCodecId};

pub struct PngCodec;

impl TileCodec for PngCodec {
    fn encode(&self, raw: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CodecError> {
        let need = w as usize * h as usize * 4;
        if raw.len() < need {
            return Err(CodecError::BadGeometry);
        }
        let mut out = Vec::with_capacity(need / 2);
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            // 单块小图，压缩级别偏速度
            enc.set_compression(png::Compression::Fast);
            let mut writer = enc
                .write_header()
                .map_err(|e| CodecError::Backend(e.to_string()))?;
            writer
                .write_image_data(&raw[..need])
                .map_err(|e| CodecError::Backend(e.to_string()))?;
        }
        Ok(out)
    }

    fn decode_into(&self, data: &[u8], dst: &mut [u8], stride: u32) -> Result<(), CodecError> {
        if stride < 4 || stride % 4 != 0 {
            return Err(CodecError::BadDst);
        }
        let row_bytes = stride as usize;
        let rows = dst.len() / row_bytes;
        if rows == 0 || dst.len() % row_bytes != 0 {
            return Err(CodecError::BadDst);
        }
        let mut decoder = png::Decoder::new(data);
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder
            .read_info()
            .map_err(|_| CodecError::Corrupt("png header"))?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|_| CodecError::Corrupt("png decode"))?;
        if info.width as usize > stride as usize / 4 || info.height as usize > rows {
            return Err(CodecError::Corrupt("png exceeds dst"));
        }
        let w = info.width as usize;
        let h = info.height as usize;
        let src_row = w * 4;
        match info.color_type {
            png::ColorType::Rgba => {
                for y in 0..h {
                    let s = y * src_row;
                    let d = y * row_bytes;
                    dst[d..d + src_row].copy_from_slice(&buf[s..s + src_row]);
                }
            }
            png::ColorType::Rgb => {
                for y in 0..h {
                    for x in 0..w {
                        let s = (y * w + x) * 3;
                        let d = y * row_bytes + x * 4;
                        dst[d] = buf[s];
                        dst[d + 1] = buf[s + 1];
                        dst[d + 2] = buf[s + 2];
                        dst[d + 3] = 255;
                    }
                }
            }
            png::ColorType::Grayscale => {
                for y in 0..h {
                    for x in 0..w {
                        let v = buf[y * w + x];
                        let d = y * row_bytes + x * 4;
                        dst[d] = v;
                        dst[d + 1] = v;
                        dst[d + 2] = v;
                        dst[d + 3] = 255;
                    }
                }
            }
            other => {
                return Err(CodecError::Corrupt(match other {
                    _ => "unsupported png color type",
                }))
            }
        }
        Ok(())
    }

    fn id(&self) -> TileCodecId {
        TileCodecId::Png
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_truecolor() {
        let (w, h) = (64u32, 64u32);
        let mut raw = Vec::with_capacity((w * h * 4) as usize);
        for i in 0..(w * h) {
            let c = i.wrapping_mul(2654435761u32);
            raw.extend_from_slice(&c.to_le_bytes());
        }
        let codec = PngCodec;
        let enc = codec.encode(&raw, w, h).unwrap();
        let mut dst = vec![0u8; (w * h * 4) as usize];
        codec.decode_into(&enc, &mut dst, w * 4).unwrap();
        assert_eq!(dst, raw);
    }

    #[test]
    fn stride_padding_roundtrip() {
        let (w, h) = (5u32, 3u32);
        let mut raw = Vec::new();
        for i in 0..(w * h) as usize {
            raw.extend_from_slice(&(i as u32 * 7).to_le_bytes());
        }
        let codec = PngCodec;
        let enc = codec.encode(&raw, w, h).unwrap();
        let stride = 32usize;
        let mut dst = vec![0u8; stride * h as usize];
        codec.decode_into(&enc, &mut dst, stride as u32).unwrap();
        for y in 0..h as usize {
            assert_eq!(
                &dst[y * stride..y * stride + w as usize * 4],
                &raw[y * w as usize * 4..(y + 1) * w as usize * 4]
            );
        }
    }

    #[test]
    fn corrupt_rejected() {
        let codec = PngCodec;
        let mut dst = vec![0u8; 64 * 64 * 4];
        assert!(codec.decode_into(&[1, 2, 3], &mut dst, 256).is_err());
    }
}
