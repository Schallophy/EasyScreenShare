//! 协议模型：发送端 / 接收端 / 模拟网络。
//!
//! 这是 §6 的**可执行镜像**。它不涉及真实编码与网络，只验证协议语义：
//! epoch/version 判定、NACK 重发、报告驱动重发、滚动组原子应用、布局代次切换。
//!
//! **无补丁协议**（§14.5 决策）：所有块都是绝对内容。像素内容用 `u64` 指纹
//! 代替，逐块比较指纹等价于逐字节比较（指纹由真实像素哈希得到时）。

pub mod link;
pub mod net;
pub mod receiver;
pub mod sender;

pub use link::Link;
pub use net::{Message, NetConfig, Network, TileMsg};
pub use receiver::{Receiver, RxDefects, Verdict};
pub use sender::{Defects, Sender};

/// 块编号。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct TileId(pub u16);

/// 块网格。分辨率或块尺寸变化会改变网格本身，因此网格是显式状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    pub fn len(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
