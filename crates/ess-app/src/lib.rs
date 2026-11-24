//! 分享端「一步起全链」需要的零件，CLI 与图形外壳共用同一条实现。
//!
//! - [`viewer`]：浏览器观看页的静态服务 + 令牌（原 `ess-serve` 的循环；实现已抽到 `ess-viewer`）
//! - [`lkserver`]：端口没人监听时才把官方 LiveKit 服务端拉起来，退出时收掉
//! - [`netinfo`]：本机局域网 IPv4
//!
//! 验收脚本一律不经过 [`bring_up`]：它只在产品入口调用，默认不多开任何监听。

pub mod lkserver;
pub mod netinfo;

/// 观看页实现住在独立 crate（不依赖 livekit/webrtc-sys），这里保持原路径兼容
pub use ess_viewer as viewer;

use std::sync::Mutex;