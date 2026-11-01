//! 帧与差分（§5 / §7.1）。
//!
//! 差分基准是 `last_sent`（接收端应当拥有的画面），不是上一帧采集结果；
//! 帧被丢弃时脏区并入 `pending_dirty`。

use crate::model::TileId;

/// 屏幕帧，BGRA 紧排。`stride` 允许行尾补齐（采集 API 常见）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub stride: usize,
    pub px: Vec<u8>,
    pub ts_us: u64,
}

impl Frame {
    pub fn new(w: u32, h: u32, ts_us: u64) -> Self {
        let stride = w as usize * 4;
        Self { w, h, stride, px: vec![0; stride * h as usize], ts_us }
    }

    pub fn row(&self, y: usize) -> &[u8] {
        let s = y * self.stride;
        &self.px[s..s + self.w as usize * 4]
    }

    /// 返回按 `dy` 垂直平移后的副本（正 = 内容上移），空出的条带为零。
    /// 用于滚动检测后的差分基准：接收端会得到"平移后的旧画面"。
    pub fn vscroll(&self, dy: i32) -> Frame {
        let mut out = self.clone();
        crate::scroll::apply_vscroll(&mut out.px, out.stride, out.h, dy);
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn right(&self) -> u32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> u32 {
        self.y + self.h
    }
    pub fn union(&self, o: &Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = self.right().max(o.right());
        let b = self.bottom().max(o.bottom());
        Rect { x, y, w: r - x, h: b - y }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TileGeom {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl TileGeom {
    pub fn right(&self) -> u32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> u32 {
        self.y + self.h
    }
}

/// 64 块网格（§6.3 的 layout 网格）。`TileId` 在这个网格上分配。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TileGrid {
    pub cols: u16,
    pub rows: u16,
}

impl TileGrid {
    pub fn for_frame(w: u32, h: u32) -> Self {
        Self {
            cols: w.div_ceil(crate::TILE).min(u16::MAX as u32) as u16,
            rows: h.div_ceil(crate::TILE).min(u16::MAX as u32) as u16,
        }
    }

    pub fn geom(&self, id: TileId, w: u32, h: u32) -> TileGeom {
        let t = crate::TILE;
        let cx = (id.0 % self.cols) as u32;
        let cy = (id.0 / self.cols) as u32;
        let x = cx * t;
        let y = cy * t;
        TileGeom { x, y, w: (w - x).min(t), h: (h - y).min(t) }
    }

    pub fn tile_count(&self) -> usize {
        self.cols as usize * self.rows as usize
    }
}

/// 把一块区域拷成紧排的 BGRA（去掉 stride 补齐）。
pub fn extract_tile(frame: &Frame, g: TileGeom) -> Vec<u8> {
    let row_bytes = g.w as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * g.h as usize);
    for y in g.y..g.bottom() {
        let s = y as usize * frame.stride + g.x as usize * 4;
        out.extend_from_slice(&frame.px[s..s + row_bytes]);
    }
    out
}

/// 块像素指纹：FNV-1a 64。存储端不关心碰撞（状态流靠 version 判定），
/// 指纹只用于差分跳过未变化块与测试比对。
pub fn tile_hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// 一次差分的结果。
#[derive(Clone, Debug, Default)]
pub struct Diff {
    /// 内容变化的块（64 网格），含每块内变化的 16 子块数量（供 §7.2 选择）。
    pub changed: Vec<(TileId, u32)>,
    /// 变化像素的包围矩形（供调试与脏区合并；精确到像素）。
    pub dirty: Option<Rect>,
}

/// 差分基准（§7.1）。
pub struct SendState {
    pub last_sent: Frame,
    pub pending_dirty: Vec<Rect>,
}

impl SendState {
    pub fn new(initial: Frame) -> Self {
        Self { last_sent: initial, pending_dirty: Vec::new() }
    }

    /// 与 `last_sent` 做逐块差分；返回后调用方应把已发送内容的帧写入
    /// `last_sent`（`commit`），未发送帧的脏区进 `pending_dirty`（`defer`）。
    pub fn diff(&self, cur: &Frame, grid: TileGrid) -> Diff {
        self.diff_against(&self.last_sent, cur, grid)
    }

    /// 以 `base` 为基准做差分。滚动后接收端拥有的是"平移后的旧帧"，
    /// 因此滚动路径用 `diff_against(&last_sent.vscroll(dy), cur, grid)`。
    pub fn diff_against(&self, base: &Frame, cur: &Frame, grid: TileGrid) -> Diff {
        let t = crate::TILE;
        let sub = crate::SUBTILE;
        let mut changed = Vec::new();
        let mut dirty: Option<Rect> = None;
        for id in 0..grid.tile_count() {
            let id = TileId(id as u16);
            let g = grid.geom(id, cur.w, cur.h);
            let a = extract_tile(base, g);
            let b = extract_tile(cur, g);
            if a == b {
                continue;
            }
            // 统计 16 子块的变化数（§7.2 choose 的输入）
            let mut subs = 0u32;
            let sw = t.min(g.w);
            let sh = t.min(g.h);
            for sy in (0..sh).step_by(sub as usize) {
                for sx in (0..sw).step_by(sub as usize) {
                    let hit = sub_block_differs(&a, &b, g.w, sx, sy, sub);
                    if hit {
                        subs += 1;
                    }
                }
            }
            changed.push((id, subs));
            let r = Rect { x: g.x, y: g.y, w: g.w, h: g.h };
            dirty = Some(match dirty {
                Some(d) => d.union(&r),
                None => r,
            });
        }
        Diff { changed, dirty }
    }

    /// 记下"这一帧已经发出去了"。像素**原地覆盖**而不是接管一个 clone：1080p BGRA
    /// 一帧 8.3 MB，每条更新路径都要提交一次，按 clone 算就是每秒几十 MB 的
    /// 分配 + 复制 + 释放（§14.7.10 归因剩下那 ~17 ms 时点名的一类）。`clear` +
    /// `extend_from_slice` 保住已分配的容量，只付一次 memcpy。
    pub fn commit(&mut self, sent: &Frame) {
        self.last_sent.px.clear();
        self.last_sent.px.extend_from_slice(&sent.px);
        self.last_sent.w = sent.w;
        self.last_sent.h = sent.h;
        self.last_sent.stride = sent.stride;
        self.last_sent.ts_us = sent.ts_us;
        self.pending_dirty.clear();
    }

    pub fn defer(&mut self, cur: &Frame, grid: TileGrid) {
        let d = self.diff(cur, grid);
        if let Some(r) = d.dirty {
            self.pending_dirty.push(r);
        }
    }
}

fn sub_block_differs(a: &[u8], b: &[u8], w: u32, sx: u32, sy: u32, sub: u32) -> bool {
    let row_bytes = w as usize * 4;
    let ah = (a.len() / row_bytes) as u32;
    let x0 = sx as usize * 4;
    let x1 = ((sx + sub) as usize * 4).min(row_bytes);
    for y in sy..(sy + sub).min(ah) {
        let s = y as usize * row_bytes;
        if a[s + x0..s + x1] != b[s + x0..s + x1] {
            return true;
        }
    }
    false
}
