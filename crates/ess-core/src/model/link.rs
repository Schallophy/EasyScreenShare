//! 接收端侧的链路状态：seq 空洞检测（§6.5 丢失检测）。
//!
//! **无补丁协议**：没有 SACK——它的唯一用途（补丁底图确认）已随补丁消失。
//! 修复由 NACK（快）与周期报告比对（兜底）驱动。

use std::collections::BTreeSet;

pub struct Link {
    received: BTreeSet<u32>,
    /// 所有 seq ≤ contiguous 都已收到
    contiguous: u32,
    highest: u32,
    /// 已发出的 NACK 区间，避免重复
    pub nacks: Vec<(u32, u32)>,
}

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

impl Link {
    pub fn new() -> Self {
        Self { received: BTreeSet::new(), contiguous: 0, highest: 0, nacks: Vec::new() }
    }

    /// 收到一条 seq。
    pub fn observe(&mut self, seq: u32) {
        if seq == 0 {
            return;
        }
        self.received.insert(seq);
        self.highest = self.highest.max(seq);
        while self.received.contains(&(self.contiguous + 1)) {
            self.contiguous += 1;
        }
    }

    /// 当前需要请求补发的区间：累计水位之后到已见最大 seq。
    pub fn pending_nack(&mut self) -> Option<(u32, u32)> {
        if self.highest <= self.contiguous {
            return None;
        }
        let range = (self.contiguous + 1, self.highest + 1);
        if !self.nacks.contains(&range) {
            self.nacks.push(range);
        }
        Some(range)
    }

    pub fn contiguous(&self) -> u32 {
        self.contiguous
    }

    pub fn highest(&self) -> u32 {
        self.highest
    }
}
