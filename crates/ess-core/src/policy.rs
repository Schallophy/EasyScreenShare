//! 每块粒度选择（§7.2）。v0.6 只有两级：16×16 子块 vs 整块 64×64。

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Sub16,
    Whole64,
}

/// 运行时标定值（§9.4），初值来自实测。
#[derive(Clone, Copy, Debug)]
pub struct PolicyConsts {
    pub est_64_bytes: f32,
    pub est_16_bytes: f32,
}

impl Default for PolicyConsts {
    fn default() -> Self {
        Self { est_64_bytes: 380.0, est_16_bytes: 110.0 }
    }
}

/// `sub16` = 该 64 块内变化的 16 子块数量。
pub fn choose(sub16: u32, c: &PolicyConsts) -> Choice {
    if sub16 * (c.est_16_bytes as u32 + 2) < c.est_64_bytes as u32 {
        Choice::Sub16
    } else {
        Choice::Whole64
    }
}
