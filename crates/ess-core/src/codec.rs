//! 图块编解码 trait（§4.1）。
//!
//! 平台相关实现放在 `ess-codec-*` crate 中通过 trait 注入；本模块只定义
//! 接口与平台无关的 `Raw` 实现（测试与有损兜底的整帧路径用）。

/// 与 `ess.proto` 的 `TileCodec` 对应。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileCodecId {
    WebpLossless,
    PaletteZstd,
    Png,
    Raw,
    Jpeg,
}

#[derive(Debug)]
pub enum CodecError {
    /// 输入尺寸与块几何不符（多于/少于 `w*h*4` 字节允许 stride 补齐）
    BadGeometry,
    /// 输出缓冲过小或 stride 与宽度不符
    BadDst,
    /// 编码数据损坏或与声明的尺寸不符
    Corrupt(&'static str),
    /// 该编码器不适用于这份输入（如像素颜色数超过调色板上限）
    Unsupported,
    /// 实现侧的错误（如压缩器初始化失败）
    Backend(String),
}

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadGeometry => write!(f, "bad geometry"),
            Self::BadDst => write!(f, "bad dst"),
            Self::Corrupt(s) => write!(f, "corrupt: {s}"),
            Self::Unsupported => write!(f, "unsupported input"),
            Self::Backend(s) => write!(f, "backend: {s}"),
        }
    }
}

pub trait TileCodec: Send + Sync {
    fn encode(&self, raw: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CodecError>;
    /// 解码到调用方提供的缓冲，不分配新内存（INV-8：容量由调用方校验）。
    /// `dst` 至少 `stride * h` 字节；`stride >= w * 4`。
    fn decode_into(&self, data: &[u8], dst: &mut [u8], stride: u32) -> Result<(), CodecError>;
    fn id(&self) -> TileCodecId;
    /// 有损编码返回 true。选码器只在它**明显更便宜**时才选中它，见
    /// `crate::pipeline::CodecSet::encode_by_cost`。
    fn lossy(&self) -> bool {
        false
    }
}

/// 原样存储（BGRA 紧排）。仅用于测试与兜底：体积就是 `w*h*4`。
pub struct RawCodec;

impl TileCodec for RawCodec {
    fn encode(&self, raw: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CodecError> {
        let need = w as usize * h as usize * 4;
        if raw.len() < need {
            return Err(CodecError::BadGeometry);
        }
        Ok(raw[..need].to_vec())
    }

    fn decode_into(&self, data: &[u8], dst: &mut [u8], stride: u32) -> Result<(), CodecError> {
        let w = stride as usize / 4;
        if stride < 4 || dst.len() < stride as usize {
            return Err(CodecError::BadDst);
        }
        let rows = dst.len() / stride as usize;
        if data.len() < stride as usize * rows {
            return Err(CodecError::Corrupt("raw length mismatch"));
        }
        let n = stride as usize * rows;
        let _ = w;
        dst[..n].copy_from_slice(&data[..n]);
        Ok(())
    }

    fn id(&self) -> TileCodecId {
        TileCodecId::Raw
    }
}

/// 有损图块编码：全采样 4:4:4，**不做色度下采样**——下采样糊掉的正是字与细线，
/// 而"糊"是这条路线只肯付给照片、不肯付给文字的东西。
///
/// 为什么必须有它（实测，`crates/ess-codec-palette/tests/codec_cost.rs`，64×64 一块）：
/// 照片类一块 PNG 中位 5 563 B、JPEG(q80) 中位 1 210 B ⇒ **省 4.6×**；一次 1080p 半屏
/// 照片类变化 = 255 块 = 2.29 MB，按实测出口 3.5–5 MB/s 要 0.45–0.65 s 才铺完，走这里
/// 进 0.1 s 档。而同一张表里"照片背景上压字"只省 2.5×、平涂文本块调色板中位 **102 B**
/// （比 JPEG 还小 12 倍）——所以**判据是省几倍，不是内容像什么**：省不到门限就留无损。
pub struct JpegCodec {
    pub quality: u8,
}

impl Default for JpegCodec {
    fn default() -> Self {
        Self { quality: 80 }
    }
}

impl TileCodec for JpegCodec {
    fn encode(&self, raw: &[u8], w: u32, h: u32) -> Result<Vec<u8>, CodecError> {
        let n = w as usize * h as usize;
        if raw.len() < n * 4 {
            return Err(CodecError::BadGeometry);
        }
        // BGRA → RGB 紧排（`motion::encode_rgb` 收 RGB，且 4:4:4 不分采样）
        let mut rgb = vec![0u8; n * 3];
        for i in 0..n {
            rgb[i * 3] = raw[i * 4 + 2];
            rgb[i * 3 + 1] = raw[i * 4 + 1];
            rgb[i * 3 + 2] = raw[i * 4];
        }
        crate::motion::encode_rgb(&rgb, w, h, self.quality).map_err(CodecError::Backend)
    }

    fn decode_into(&self, data: &[u8], dst: &mut [u8], stride: u32) -> Result<(), CodecError> {
        let mut dec = zune_jpeg::JpegDecoder::new(data);
        let rgb = dec.decode().map_err(|e| CodecError::Backend(format!("jpeg: {e:?}")))?;
        let info = dec.info().ok_or(CodecError::Corrupt("jpeg missing info"))?;
        let iw = usize::from(info.width);
        let ih = usize::from(info.height);
        if iw == 0 || ih == 0 {
            return Err(CodecError::Corrupt("jpeg empty"));
        }
        let ch = (rgb.len() / (iw * ih)).max(1);
        if ch < 3 {
            return Err(CodecError::Corrupt("jpeg is not color"));
        }
        let st = stride as usize;
        if dst.len() < ih * st {
            return Err(CodecError::BadDst);
        }
        for y in 0..ih {
            let d = y * st;
            for x in 0..iw {
                let si = (y * iw + x) * ch;
                let di = d + x * 4;
                dst[di] = rgb[si + 2];
                dst[di + 1] = rgb[si + 1];
                dst[di + 2] = rgb[si];
                dst[di + 3] = 255;
            }
        }
        Ok(())
    }

    fn id(&self) -> TileCodecId {
        TileCodecId::Jpeg
    }

    fn lossy(&self) -> bool {
        true
    }
}
