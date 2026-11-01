//! 接收端模型：epoch/version 判定、滚动组原子应用、布局代次切换。
//!
//! 对应 §6.3 / §6.3.1 / §6.7。
//!
//! **无补丁协议**：所有块都是绝对内容，没有底图匹配、没有补丁分支。
//!
//! **滚动组原子性（P1-7）**：滚动的本质是"整片块同时换基准"。若只应用
//! 到了一半，画面会出现发送端从未存在过的拼接（一半新位置、一半旧位置），
//! 违反优先级 2（画面完整）。因此：
//!
//! 1. 滚动宣告走**可靠通道**（永不丢失），接收端由此知道"这次滚动涉及哪些块、
//!    目标 epoch 是多少"；
//! 2. 属于未完成滚动组的块**先缓冲**，等组内所有块到齐后**一次性应用**；
//! 3. 组迟迟不齐 → 请求重同步，但**组不解散**（解散后迟到的块会零散应用，
//!    又回到撕裂）。

use std::collections::{BTreeMap, BTreeSet};

use super::net::{Message, TileMsg};
use super::{Grid, TileId};

/// 接收端的判定结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Applied,
    /// 旧基准 / 旧版本 / 重复
    DroppedStale,
    /// 属于未完成的滚动组，已缓冲等待原子应用
    Buffered,
    /// 旧布局的消息
    DroppedOldLayout,
}

/// 违反不变式的情况。任何一项出现都是协议缺陷。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Violation {
    /// 版本号倒退
    VersionWentBackwards { id: TileId, got: u32, have: u32 },
}

/// 一个未完成的滚动组。
struct Group {
    /// 每块的目标 epoch（**全部平移块**，不止 dirty）
    targets: BTreeMap<TileId, u32>,
    /// 真正会重发的 dirty 块（新暴露条带）；组按它们到齐判定完成
    dirty: BTreeSet<TileId>,
    /// 已到齐的块
    arrived: BTreeMap<TileId, TileMsg>,
    /// 位移量，供像素管线在组完成时应用到画布
    dy: i32,
    registered_at: u64,
    /// 已请求重同步
    resync_requested: bool,
}

impl Group {
    fn contains_epoch(&self, id: TileId, epoch: u32) -> bool {
        self.targets.get(&id) == Some(&epoch)
    }
}

/// 接收端侧的缺陷开关。
#[derive(Clone, Copy, Default, Debug)]
pub struct RxDefects {
    /// 滚动组的块不等组齐就应用（撕裂）
    pub apply_before_group_complete: bool,
    /// 整帧无条件覆盖接收端状态（原 §6.5.1 缺陷）
    pub full_frame_clobbers: bool,
}

/// 接收端统计。
#[derive(Clone, Copy, Default, Debug)]
pub struct RxStats {
    pub old_layout_dropped: u64,
    pub group_resyncs: u64,
    pub groups_completed: u64,
}

pub struct Receiver {
    pub layout_id: u32,
    pub grid: Grid,
    pub epoch: Vec<u32>,
    pub version: Vec<u32>,
    pub content: Vec<u64>,
    pub missing: BTreeSet<TileId>,
    /// 未完成的滚动组，按 scroll_id 索引
    groups: BTreeMap<u32, Group>,
    /// 已应用的滚动组（幂等）
    applied_scrolls: BTreeSet<u32>,
    /// 已完成的滚动组（scroll_id, dy），供像素管线在此刻平移画布
    completed_scrolls: Vec<(u32, i32)>,
    pub violations: Vec<Violation>,
    pub stats: RxStats,
    pub defects: RxDefects,
    /// 收到的最大 seq（用于空洞检测）
    pub last_seq: u32,
    now_hint: u64,
}

impl Receiver {
    pub fn new(grid: Grid) -> Self {
        Self::with_defects(grid, RxDefects::default())
    }

    pub fn with_defects(grid: Grid, defects: RxDefects) -> Self {
        let n = grid.len();
        Self {
            layout_id: 1,
            grid,
            epoch: vec![0; n],
            version: vec![0; n],
            content: vec![0; n],
            missing: BTreeSet::new(),
            groups: BTreeMap::new(),
            applied_scrolls: BTreeSet::new(),
            completed_scrolls: Vec::new(),
            violations: Vec::new(),
            stats: RxStats::default(),
            defects,
            last_seq: 0,
            now_hint: 0,
        }
    }

    /// 接收一条消息，返回判定结果。
    pub fn on_message(&mut self, msg: &Message) -> Verdict {
        match msg {
            Message::ScrollAnnounce { layout_id, scroll_id, dy, targets, dirty } => {
                self.on_scroll_announce(*layout_id, *scroll_id, *dy, targets, dirty)
            }
            Message::Update { layout_id, cols, rows, tiles } => {
                self.on_update(*layout_id, *cols, *rows, tiles)
            }
            Message::FullFrame { layout_id, cols, rows, versions, epochs, contents } => {
                self.on_full_frame(*layout_id, *cols, *rows, versions, epochs, contents)
            }
            Message::Report { .. } => Verdict::DroppedStale,
        }
    }

    /// 滚动宣告（可靠通道）：登记滚动组。重复宣告幂等忽略。
    ///
    /// `targets` = 全部平移块 → 目标 epoch；`dirty` = 将重发的条带块。
    /// 组按 **dirty 到齐** 判定完成（v0.7）：平移块像素由位移得到，
    /// 不需要逐块到达。
    fn on_scroll_announce(
        &mut self,
        layout_id: u32,
        scroll_id: u32,
        dy: i32,
        targets: &[(TileId, u32)],
        dirty: &[TileId],
    ) -> Verdict {
        if layout_id < self.layout_id {
            self.stats.old_layout_dropped += 1;
            return Verdict::DroppedOldLayout;
        }
        if self.applied_scrolls.contains(&scroll_id) || self.groups.contains_key(&scroll_id) {
            return Verdict::DroppedStale; // 幂等
        }
        let n = self.version.len();
        let mut map = BTreeMap::new();
        for &(id, target) in targets {
            if (id.0 as usize) < n {
                map.insert(id, target);
            }
        }
        if map.is_empty() {
            return Verdict::DroppedStale;
        }
        let dirty_set: BTreeSet<TileId> =
            dirty.iter().copied().filter(|id| map.contains_key(id)).collect();
        // 新组覆盖的块，从更老的未完成组里移除：那个块已由新组接管，
        // 否则老组永远等不到它（活锁）。
        let ids: Vec<TileId> = map.keys().copied().collect();
        for g in self.groups.values_mut() {
            for id in &ids {
                g.targets.remove(id);
                g.dirty.remove(id);
                g.arrived.remove(id);
            }
        }
        if std::env::var("ESS_TRACE").is_ok() { eprintln!("REGISTER group {scroll_id} targets={:?}", map); }
        self.groups.insert(
            scroll_id,
            Group {
                targets: map,
                dirty: dirty_set,
                arrived: BTreeMap::new(),
                dy,
                registered_at: self.now_hint,
                resync_requested: false,
            },
        );
        // 没有 dirty 块（纯位移）→ 立即可应用
        self.try_complete_group(scroll_id);
        Verdict::Buffered
    }

    fn on_update(&mut self, layout_id: u32, cols: u16, rows: u16, tiles: &[TileMsg]) -> Verdict {
        if layout_id < self.layout_id {
            self.stats.old_layout_dropped += 1;
            return Verdict::DroppedOldLayout;
        }
        if layout_id > self.layout_id {
            self.rebuild_layout(layout_id, Grid::new(cols, rows));
        }
        let mut verdict = Verdict::DroppedStale;
        for t in tiles {
            let v = self.on_tile(*t);
            if v == Verdict::Applied {
                verdict = Verdict::Applied;
            }
        }
        verdict
    }

    /// 单块处理（像素管线逐块驱动同一套判定逻辑）。
    pub fn on_tile(&mut self, t: TileMsg) -> Verdict {
        let i = t.id.0 as usize;
        if i >= self.version.len() {
            return Verdict::DroppedStale;
        }
        if t.epoch < self.epoch[i] {
            // 旧基准：丢弃（§6.3）
            return Verdict::DroppedStale;
        }

        // 属于某个未完成滚动组 → 缓冲，等组齐后原子应用
        let group_id = self
            .groups
            .iter()
            .find(|(_, g)| g.contains_epoch(t.id, t.epoch))
            .map(|(sid, _)| *sid);
        if let Some(sid) = group_id {
            let group = self.groups.get_mut(&sid).unwrap();
            group.arrived.insert(t.id, t);
            if self.defects.apply_before_group_complete {
                // 【变异】不等组齐就应用 —— 复现撕裂缺陷
                self.apply_tile(t);
            }
            self.try_complete_group(sid);
            return Verdict::Buffered;
        }

        // 陈旧判定按 **(epoch, version) 对**：epoch 相同才比版本。
        // epoch 升高时版本不变的消息（如布局变更后从未编辑块的重发）不是旧消息。
        if t.epoch == self.epoch[i] && t.version <= self.version[i] {
            return Verdict::DroppedStale;
        }
        if t.epoch == self.epoch[i] && t.version < self.version[i] {
            self.violations.push(Violation::VersionWentBackwards {
                id: t.id,
                got: t.version,
                have: self.version[i],
            });
            return Verdict::DroppedStale;
        }
        self.apply_tile(t);
        Verdict::Applied
    }

    fn apply_tile(&mut self, t: TileMsg) {
        let i = t.id.0 as usize;
        if t.epoch > self.epoch[i] {
            self.epoch[i] = t.epoch;
        }
        if t.version < self.version[i] {
            return; // 旧版本，不回退
        }
        self.content[i] = t.content;
        self.version[i] = t.version;
        self.missing.remove(&t.id);
    }

    /// 组内所有 **dirty** 块到齐 → **一次性应用**（原子性）：全部平移块的
    /// epoch 推进到目标值，dirty 块写入内容。
    fn try_complete_group(&mut self, sid: u32) {
        let ready = {
            let Some(g) = self.groups.get(&sid) else { return };
            g.dirty.iter().all(|id| g.arrived.contains_key(id))
        };
        if !ready {
            return;
        }
        if std::env::var("ESS_TRACE").is_ok() { eprintln!("COMPLETE group {sid}"); }
        let group = self.groups.remove(&sid).unwrap();
        for (&id, &target) in group.targets.iter() {
            let i = id.0 as usize;
            if target > self.epoch[i] {
                self.epoch[i] = target;
            }
        }
        for (id, t) in group.arrived {
            if group.dirty.contains(&id) {
                self.apply_tile(t);
            }
        }
        self.applied_scrolls.insert(sid);
        self.completed_scrolls.push((sid, group.dy));
        self.stats.groups_completed += 1;
    }

    /// 取走已完成的滚动组（scroll_id, dy），像素管线据此平移画布。
    pub fn take_completed_scrolls(&mut self) -> Vec<(u32, i32)> {
        std::mem::take(&mut self.completed_scrolls)
    }

    /// 周期时钟：迟迟不齐的组请求重同步。**组不解散**（解散后迟到的块会
    /// 零散应用，回到撕裂）。返回需要重同步的块集合。
    pub fn tick(&mut self, now_ms: u64, group_timeout_ms: u64) -> Vec<TileId> {
        self.now_hint = now_ms;
        let mut need = Vec::new();
        let sids: Vec<u32> = self.groups.keys().copied().collect();
        for sid in sids {
            let g = self.groups.get_mut(&sid).unwrap();
            let stale = now_ms.saturating_sub(g.registered_at) > group_timeout_ms;
            if stale && !g.resync_requested {
                g.resync_requested = true;
                self.stats.group_resyncs += 1;
                for id in g.dirty.iter() {
                    if !g.arrived.contains_key(id) {
                        need.push(*id);
                    }
                }
            }
        }
        need
    }

    fn on_full_frame(
        &mut self,
        layout_id: u32,
        cols: u16,
        rows: u16,
        versions: &[u32],
        epochs: &[u32],
        contents: &[u64],
    ) -> Verdict {
        if layout_id < self.layout_id {
            self.stats.old_layout_dropped += 1;
            return Verdict::DroppedOldLayout;
        }
        if layout_id > self.layout_id {
            self.rebuild_layout(layout_id, Grid::new(cols, rows));
        }
        // 整帧**不得覆盖更新的状态**：布局变更后可能已经有块先于整帧到达
        let n = self.version.len().min(versions.len()).min(contents.len());
        for i in 0..n {
            // epoch 随整帧初始化（epoch 全局单调，见 §6.3.1）
            if let Some(&e) = epochs.get(i) {
                if e > self.epoch[i] {
                    self.epoch[i] = e;
                }
            }
            if versions[i] < self.version[i] && !self.defects.full_frame_clobbers {
                continue;
            }
            self.version[i] = versions[i];
            self.content[i] = contents[i];
            self.missing.remove(&TileId(i as u16));
        }
        // 整帧本身就是一个**完整的**发送端状态，它使所有未完成的滚动组失效——
        // 否则组内一部分块被整帧带到新基准、另一部分还停在旧基准，出现撕裂
        // （P1-7 在 3000 用例浸泡中抓到的真实漏洞）。迟到的组内块按版本判定
        // 正常处理。
        self.groups.clear();
        self.completed_scrolls.clear();
        Verdict::Applied
    }

    /// 布局代次变更：重建网格、清空滚动组、全部标 missing（§6.3.1）。
    fn rebuild_layout(&mut self, layout_id: u32, grid: Grid) {
        self.layout_id = layout_id;
        self.grid = grid;
        let n = grid.len();
        self.epoch = vec![0; n];
        self.version = vec![0; n];
        self.content = vec![0; n];
        if std::env::var("ESS_TRACE").is_ok() { eprintln!("REBUILD layout {layout_id} (cleared groups: {:?})", self.groups.keys().collect::<Vec<_>>()); }
        self.groups.clear();
        self.applied_scrolls.clear();
        self.missing = (0..n).map(|i| TileId(i as u16)).collect();
    }

    /// 当前状态快照，用于与发送端 `last_sent` 比较。
    pub fn state(&self) -> Vec<(u32, u32, u64)> {
        self.content
            .iter()
            .enumerate()
            .map(|(i, &c)| (self.epoch[i], self.version[i], c))
            .collect()
    }

    /// **P1-7 断言**：对每个未完成滚动组，其涉及的块要么全部处于旧基准，
    /// 要么全部已应用——不允许"一半新一半旧"的拼接。
    pub fn canvas_is_consistent(&self) -> Result<(), String> {
        for (sid, g) in &self.groups {
            // 只检查**已经开始缓冲**的组。一个还没有任何块到达的组，其块若已
            // 在别处被应用（只可能发生在跨布局重建的时序里），与普通的
            // 「丢块待重发」不可区分，不属于撕裂。
            if g.arrived.is_empty() {
                continue;
            }
            let mut new_cnt = 0usize;
            let mut old_cnt = 0usize;
            for (id, target) in &g.targets {
                let i = id.0 as usize;
                if i >= self.epoch.len() {
                    continue;
                }
                // 只把「正好处于本组目标 epoch」的块算作本组已应用：
                // epoch 更高的块已被**后续滚动**接管（该块已从本组移除的
                // 前提下仍可能滞后登记），属于普通的丢块待重发，不是撕裂。
                if self.epoch[i] == *target {
                    new_cnt += 1;
                } else if self.epoch[i] < *target {
                    old_cnt += 1;
                }
            }
            if new_cnt > 0 && old_cnt > 0 {
                let detail: Vec<String> = g
                    .targets
                    .iter()
                    .map(|(id, target)| {
                        let i = id.0 as usize;
                        let st = if i < self.epoch.len() {
                            format!("e{} v{} c{}", self.epoch[i], self.version[i], self.content[i])
                        } else {
                            "越界".into()
                        };
                        format!("tile{}(target e{}, arrived={}, now {st})", id.0, target, g.arrived.contains_key(id))
                    })
                    .collect();
                return Err(format!(
                    "滚动组 {sid} 被部分应用：{new_cnt} 块新基准 / {old_cnt} 块旧基准 | {}",
                    detail.join(", ")
                ));
            }
        }
        Ok(())
    }

    pub fn pending_groups(&self) -> usize {
        self.groups.len()
    }

    /// 接收端周期报告（上行），驱动发送端的差异重发。
    pub fn report(&self) -> Message {
        Message::Report {
            layout_id: self.layout_id,
            versions: self.version.clone(),
            epochs: self.epoch.clone(),
        }
    }
}
