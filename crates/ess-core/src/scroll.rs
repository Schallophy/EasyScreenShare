//! 滚动检测（§6.7 / §7.5）。
//!
//! 只检测**整帧垂直整数位移**这一最常见形态：逐行指纹，搜索使重叠区
//! 行指纹最大匹配的 `dy`。非整数位移与水平滚动回退到图块路径（实测
//! 半像素滚动劣于整帧，见附录 A）。

use crate::frame::Frame;
use crate::model::TileId;

/// 行数匹配率阈值：重叠区至少这么多行指纹一致才算一次滚动。
const MIN_MATCH_RATIO: f32 = 0.95;

#[derive(Clone, Debug, PartialEq)]
pub struct ScrollDetect {
    /// 正值 = 内容向上移动（浏览内容向下滚）。
    pub dy: i32,
    /// 受影响的块：整个网格（epoch 推进的范围由调用方决定）。
    pub affected: Vec<TileId>,
    /// 重叠区匹配率，供日志/调参。
    pub match_ratio: f32,
}

fn row_hash(frame: &Frame, y: usize) -> u64 {
    crate::frame::tile_hash(frame.row(y))
}

/// 计算逐行指纹（缓存供两帧比较）。
pub struct RowHashes {
    pub hashes: Vec<u64>,
}

impl RowHashes {
    pub fn of(frame: &Frame) -> Self {
        Self { hashes: (0..frame.h as usize).map(|y| row_hash(frame, y)).collect() }
    }
}

/// 检测 `prev → cur` 的垂直滚动。
///
/// `max_dy` 取帧高的一半：更大的位移视为场景切换而非滚动。
pub fn detect_vscroll(prev: &RowHashes, cur: &RowHashes) -> Option<ScrollDetect> {
    let h = prev.hashes.len().min(cur.hashes.len());
    if h == 0 {
        return None;
    }
    // 无任何变化时 dy=0 没有意义，直接返回 None
    let any_change = (0..h).any(|y| prev.hashes[y] != cur.hashes[y]);
    if !any_change {
        return None;
    }
    let max_dy = (h / 2) as i32;
    // 先按匹配率收集候选，再按「不匹配行只能落在新暴露条带」校验——
    // 仅靠匹配率会把大面积局部编辑误判为 dy=1 的滚动（实测踩到）。
    let mut candidates: Vec<(i32, f32)> = Vec::new();
    for dy in 1..=max_dy {
        for &d in &[dy, -dy] {
            let overlap = h as i32 - d.abs();
            if overlap < 8 {
                continue;
            }
            let (mut hits, mut total) = (0u32, 0u32);
            for y in 0..h as i32 {
                let sy = y + d;
                if !(0..h as i32).contains(&sy) {
                    continue;
                }
                total += 1;
                if prev.hashes[sy as usize] == cur.hashes[y as usize] {
                    hits += 1;
                }
            }
            let ratio = hits as f32 / total as f32;
            if ratio >= MIN_MATCH_RATIO {
                candidates.push((d, ratio));
            }
        }
    }
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (dy, ratio) in candidates {
        if band_matches(&prev.hashes, &cur.hashes, dy) {
            return Some(ScrollDetect { dy, affected: Vec::new(), match_ratio: ratio });
        }
    }
    None
}

/// 不匹配的行必须全部位于新暴露条带：
/// `dy > 0`（内容上移）→ 底部 `dy` 行；`dy < 0` → 顶部 `|dy|` 行。
fn band_matches(prev: &[u64], cur: &[u64], dy: i32) -> bool {
    let h = prev.len().min(cur.len()) as i32;
    let d = dy.abs();
    for y in 0..h {
        let sy = y + dy;
        if !(0..h).contains(&sy) {
            continue;
        }
        if prev[sy as usize] != cur[y as usize] {
            let in_band = if dy > 0 { y >= h - d } else { y < d };
            if !in_band {
                return false;
            }
        }
    }
    true
}

/// 从逐行指纹直接检测（便捷入口）。
pub fn detect(prev: &Frame, cur: &Frame) -> Option<ScrollDetect> {
    if prev.w != cur.w || prev.h != cur.h {
        return None;
    }
    detect_vscroll(&RowHashes::of(prev), &RowHashes::of(cur))
}

/// 把滚动作用到画布上（接收端重建用）：`dy > 0` 时内容上移。
pub fn apply_vscroll(canvas_px: &mut [u8], stride: usize, h: u32, dy: i32) {
    let h = h as usize;
    match dy.cmp(&0) {
        std::cmp::Ordering::Greater => {
            let d = dy as usize;
            if d >= h {
                return;
            }
            canvas_px.copy_within(d * stride..h * stride, 0);
        }
        std::cmp::Ordering::Less => {
            let d = (-dy) as usize;
            if d >= h {
                return;
            }
            canvas_px.copy_within(0..(h - d) * stride, d * stride);
        }
        std::cmp::Ordering::Equal => {}
    }
}
