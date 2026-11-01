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
