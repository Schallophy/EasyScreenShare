//! 屏幕共享核心：协议、差分、滚动、图块策略、守门员。
//!
//! 约束：本 crate 不得依赖 OS 特有的 API（网络、线程、文件系统），
//! 必须能编译到 wasm32-unknown-unknown，供网页观看端使用。
//! 平台相关的图块编码器实现放在 ess-codec-* crate，通过 trait 注入。

pub mod proto {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/ess.rs"));
}

/// 图块边长（§7.2 的两级粒度：16 子块 / 64 整块）。
pub const TILE: u32 = 64;
/// 子块边长。
pub const SUBTILE: u32 = 16;

pub mod codec;
pub mod frame;
pub mod governor;
pub mod model;
pub mod motion;
pub mod pipeline;
pub mod policy;
pub mod scroll;
pub mod wire;
