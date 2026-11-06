//! 发送端模型：版本分配、seq 分配、NACK 重发、报告驱动的差异重发、布局变更。
//!
//! **无补丁协议**（§14.5 决策）：所有块都是绝对内容。因此没有 SACK、没有
//! 在途版本链、没有 `acked_version`——它们存在的唯一理由（补丁底图确认）
//! 已经消失。修复路径只剩两条：接收端 NACK（快）+ 周期报告比对（兜底）。

use super::net::{Message, TileMsg};
use super::{Grid, TileId};

/// 已知缺陷的注入开关。用于**变异测试**：把修掉的缺陷重新打开，
/// 确认对应的属性测试确实会失败（§14.2 三.2）。
#[derive(Clone, Copy, Default, Debug)]
pub struct Defects {
    /// 布局变更后不清 `seq → 块` 映射（原 §6.5.1 缺陷）
    pub keep_sent_log: bool,
    /// 报告驱动的差异重发被禁用（布局消息丢失后接收端永远停在旧布局）
    pub no_report_resync: bool,
    /// 修复预算被禁用（§6.6）：弱网下修复流量无上限，
    /// 会和报告 resync 形成正反馈、挤死出口队列（§14.6 实测）。
    pub no_repair_budget: bool,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub tiles_sent: u64,
    pub resends: u64,
    pub report_resends: u64,
    pub layout_resends: u64,
    pub scrolls: u64,
    /// 因预算不足被扣下的修复块数（§6.6）
    pub repair_throttled: u64,
    /// 预算耗尽后升级整帧的次数（§6.6）
    pub escalations: u64,
}

pub struct Sender {
    pub layout_id: u32,
    pub grid: Grid,
    pub epoch: Vec<u32>,
    pub version: Vec<u32>,
    pub content: Vec<u64>,
    /// 全局单调版本计数器，**不随 epoch 或 layout 重置**（§6.3）
    pub version_counter: u32,
    /// 全局单调 epoch 计数器，**不随 layout 重置**。
    ///
    /// 若 epoch 随布局归零，接收端漏掉布局消息时，上一代的 e1 与这一代的
    /// e1 无法区分（3000 用例浸泡抓到的真实碰撞）。epoch 与 version 一样
    /// 全局单调后，(块, epoch) 对跨代唯一。
    pub epoch_counter: u32,

    pub seq: u32,
    /// 滚动事件计数器（滚动组 id）
    scroll_counter: u32,

    /// seq → 出现过的块（§6.5 环形缓冲），供 NACK 重发定位
    pub sent_log: Vec<(u32, TileId)>,
    /// 最近一次报告覆盖的块数 / 其中"落后 ≥2 版"的块数（能力信号，见 `receiver_strain`）
    pub last_report_tiles: usize,
    pub last_behind: usize,
    /// 最近一次报告**承认的缺口块数**（要重发的块，含被预算截掉的那部分）。
    /// 脱落补账要靠它分辨"观看端说要不到"和"观看端说全都齐了但像素是旧的"，
    /// 见 `SendPipeline::on_report`（#52 / #53）。
    pub last_gap: usize,
    /// `last_gap` 那份缺口的**块 id**（预算截断之前）。脱落补账要靠它把「报告已经要回去的块」
    /// 与「报告没要、但我们自己丢掉的块」分开：前者逐块修复已经在送，后者才需要补，
    /// 而且补的应该是那些块本身，不是整帧。见 `SendPipeline::on_report`。
    pub last_gap_ids: Vec<TileId>,
    /// 连续"大面积落后"的报告次数（能力判据，见 `receiver_strain`）
    pub strain_run: u32,
    /// 是否**见过一次**完全跟上的报告。能力判据比的是"本来跟得上，现在跟不上了"，
    /// 而刚加入的观看端在补齐整帧之前报告里全是 0——那份落后没有基线。实测
    /// `forced-plane` 里它因此在会话第 1 s 就被判成"啃不动"，抢在预算执行器前面把
    /// 画面交给被压到 300 kbps 的媒体轨道，观看端拿到 384x216 的糊画面，而它本来
    /// 下一秒就要把无损整帧落画了。
    pub ever_current: bool,
    /// 最近一次布局消息，丢失后由报告驱动重发
    pending_layout: Option<Message>,
    /// 上一轮 `on_report` 判定需要重发的布局消息
    pending_layout_msg: Option<Message>,
    /// 预算耗尽后升级出的整帧（§6.6），由调用方取走发送
    pending_escalation: Option<Message>,

    // ---- 修复预算（§6.6：修复流量 ≤ 0.2 × 带宽 × 1 s 窗口）----
    /// 出口带宽提示，由调用方随网络条件更新；`None` = 不限
    repair_bw: Option<u64>,
    repair_window_start: u64,
    repair_window_used: u64,

    pub stats: Stats,
    pub defects: Defects,
    /// 发送端时钟，与模拟网络共用同一时间基准
    now_hint: u64,
}

impl Sender {
    pub fn new(grid: Grid) -> Self {
        Self::with_defects(grid, Defects::default())
    }

    pub fn with_defects(grid: Grid, defects: Defects) -> Self {
        let n = grid.len();
        Self {
            layout_id: 1,
            grid,
            epoch: vec![0; n],
            version: vec![0; n],
            content: vec![0; n],
            version_counter: 0,
            epoch_counter: 0,
            seq: 0,
            scroll_counter: 0,
            sent_log: Vec::new(),
            last_report_tiles: 0,
            last_behind: 0,
            last_gap: 0,
            last_gap_ids: Vec::new(),
            strain_run: 0,
            ever_current: false,
            pending_layout: None,
            pending_layout_msg: None,
            pending_escalation: None,
            repair_bw: None,
            repair_window_start: 0,
            repair_window_used: 0,
            stats: Stats::default(),
            defects,
            now_hint: 0,
        }
    }

    // ---- 版本与内容 ----

    /// 分配一个消息 seq。
    pub fn alloc_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    /// 分配一个新的内容版本（全局单调）。
    fn alloc_version(&mut self) -> u32 {
        self.version_counter += 1;
        self.version_counter
    }

    fn emit(&mut self, id: TileId, content: u64, seq: u32) -> TileMsg {
        let i = id.0 as usize;
        self.sent_log.push((seq, id));
        self.stats.tiles_sent += 1;
        TileMsg { id, epoch: self.epoch[i], version: self.version[i], content }
    }

    fn update_msg(&self, tiles: Vec<TileMsg>) -> Message {
        Message::Update {
            layout_id: self.layout_id,
            cols: self.grid.cols,
            rows: self.grid.rows,
            tiles,
        }
    }

    // ---- 内容变更 ----

    /// 一次编辑：整块发送（无补丁）。
    pub fn edit(&mut self, id: TileId, content: u64) -> Message {
        let i = id.0 as usize;
        self.version[i] = self.alloc_version();
        self.content[i] = content;
        let seq = self.alloc_seq();
        let msg = self.emit(id, content, seq);
        self.update_msg(vec![msg])
    }

    /// 批量编辑：多个块合并为**一条**消息（一个 seq）。NACK 命中该 seq 时
    /// 会重发这批块的全部当前状态。
    pub fn edit_many(&mut self, updates: &[(TileId, u64)]) -> Message {
        let seq = self.alloc_seq();
        let mut tiles = Vec::new();
        for &(id, content) in updates {
            let i = id.0 as usize;
            self.version[i] = self.alloc_version();
            self.content[i] = content;
            tiles.push(self.emit(id, content, seq));
        }
        self.update_msg(tiles)
    }

    /// 一次滚动：受影响块（全部平移块）的 epoch 推进，只有 `dirty`（新暴露
    /// 条带）重发内容（§7.5 / §6.7，v0.7）。
    ///
    /// `affected` 是位移覆盖的全部块；`dirty` 是 `(块, 新内容指纹)`，必须 ⊆
    /// affected。平移块不重发：它们的像素可由位移从画布得到，版本与内容指纹
    /// 保持不变（状态流语义：基准之外的块没有新状态）。
    ///
    /// 返回 `(宣告, 更新)`：宣告走可靠通道，更新走不可靠通道。
    pub fn scroll(&mut self, affected: &[TileId], dirty: &[(TileId, u64)]) -> (Message, Message) {
        self.scroll_counter += 1;
        let scroll_id = self.scroll_counter;
        self.epoch_counter += 1;
        let e = self.epoch_counter;
        let mut targets = Vec::with_capacity(affected.len());
        for &id in affected {
            let i = id.0 as usize;
            self.epoch[i] = e;
            targets.push((id, e));
        }
        let mut tiles = Vec::new();
        for &(id, content) in dirty {
            let i = id.0 as usize;
            self.version[i] = self.alloc_version();
            self.content[i] = content;
            let seq = self.alloc_seq();
            tiles.push(self.emit(id, content, seq));
        }
        self.stats.scrolls += 1;
        let announce = Message::ScrollAnnounce {
            layout_id: self.layout_id,
            scroll_id,
            dy: 0, // 模型不关心位移量；真实管线在 wire 层填写
            targets,
            dirty: dirty.iter().map(|&(id, _)| id).collect(),
        };
        (announce, self.update_msg(tiles))
    }

    /// 布局变更：网格改变 ⇒ `layout_id` 递增，重建块数组，发整帧重同步（§6.3.1）。
    pub fn change_layout(&mut self, grid: Grid) -> Message {
        self.layout_id += 1;
        let n = grid.len();
        self.grid = grid;
        // 新网格的块从当前 epoch 代际起步（全局单调，不归零）
        self.epoch = vec![self.epoch_counter; n];
        self.version = vec![0; n];
        self.content = vec![0; n];
        // 旧布局的 seq → 块映射必须丢弃：网格变小后旧块编号已不存在，
        // 留着会让后续 NACK 去重发一个越界的块。
        if !self.defects.keep_sent_log {
            self.sent_log.clear();
        }
        let msg = Message::FullFrame {
            layout_id: self.layout_id,
            cols: grid.cols,
            rows: grid.rows,
            versions: self.version.clone(),
            epochs: self.epoch.clone(),
            contents: self.content.clone(),
        };
        self.pending_layout = Some(msg.clone());
        msg
    }

    // ---- 修复路径 ----

    /// 每块修复消息的合成大小（与 `net::msg_size` 的单块值一致）。
    const REPAIR_UNIT: u64 = 400;
    /// 整帧的合成大小（与 `net::msg_size` 一致）。
    const FULLFRAME_SIZE: u64 = 60_000;
    /// 预算窗口长度（§6.6）。
    const REPAIR_WINDOW_MS: u64 = 1_000;

    /// 出口带宽提示（修复预算用），随网络条件由调用方更新。
    pub fn set_bandwidth(&mut self, bw: Option<u64>) {
        self.repair_bw = bw;
    }

    /// 预算耗尽后升级出的整帧（§6.6：超预算 → 停止逐块修复，升级 FullFrame）。
    pub fn take_escalation(&mut self) -> Option<Message> {
        self.pending_escalation.take()
    }

    /// 当前 1 s 窗口内剩余的修复预算（字节）。`None` = 不限（无带宽提示或
    /// 缺陷开关关闭了预算）。
    fn repair_remaining(&mut self, now_ms: u64) -> Option<u64> {
        if self.defects.no_repair_budget {
            return None;
        }
        let bw = self.repair_bw?;
        if now_ms >= self.repair_window_start + Self::REPAIR_WINDOW_MS {
            self.repair_window_start = now_ms;
            self.repair_window_used = 0;
        }
        // 0.2 × 带宽 × 1 s 窗口
        let budget = bw / 8 / 5;
        Some(budget.saturating_sub(self.repair_window_used))
    }

    /// NACK 驱动的整块重发（§6.5）。
    pub fn on_nack(&mut self, from_seq: u32, to_seq: u32, now_ms: u64) -> Vec<TileMsg> {
        let mut ids: Vec<TileId> = Vec::new();
        for (seq, id) in self.sent_log.iter() {
            if *seq >= from_seq && *seq < to_seq && !ids.contains(id) {
                ids.push(*id);
            }
        }
        // 防御：块编号必须在当前网格内（布局变更后旧编号可能已越界）
        if !self.defects.keep_sent_log {
            let n = self.version.len();
            ids.retain(|id| (id.0 as usize) < n);
        }
        self.resend(&ids, now_ms)
    }

    /// 接收端**啃不动无损图块通路**的能力信号：连续两次报告里 ≥25% 的块落后 ≥2 个版本。
    ///
    /// 落后一版是在途/一次丢包（下一次报告就补平），大面积落后两版以上才是解码速度
    /// 低于内容变化速度。这时候继续逐块重画只会把观看端拖成"按序播历史"，正确处置是
    /// 把画面交给媒体平面——手机/浏览器解 H264 走硬件，几乎不吃 CPU。
    pub fn receiver_strain(&self) -> bool {
        self.strain_run >= 2
    }

    /// 画布要被重建（新观看方加入 / 重连 / 请求整帧）时，把能力基线一并清掉。
    ///
    /// "曾经跟上过"是对**某一块画布**的证明。整帧意味着那块画布不再作数：新观看端
    /// 从零开始，它第一秒的落后和当初冷启动那次一样没有基线。不清掉，重连后的手机
    /// 会继承上一个观看端的基线，一秒内就被判成"啃不动"，画面被交给窄媒体轨道。
    pub fn forget_baseline(&mut self) {
        self.ever_current = false;
        self.strain_run = 0;
    }

    fn receiver_strain_raw(&self, tiles: usize, behind: usize) -> bool {
        tiles >= 16 && behind * 100 >= tiles * 25
    }

    /// 接收端周期报告驱动的差异重发（§6.5 版本向量校验）。
    ///
    /// 返回需要重发的块；布局消息的重发放在 `pending_layout_msg`，
    /// 预算耗尽时的整帧升级放在 `pending_escalation`。
    pub fn on_report(&mut self, layout_id: u32, versions: &[u32], epochs: &[u32], now_ms: u64) -> Vec<TileMsg> {
        let mut out = Vec::new();
        if layout_id < self.layout_id {
            // 接收端还在旧布局：重发布局消息（逐块修复救不回来，§6.5.1）。
            // 布局消息不受逐块预算扣留——它是唯一能把接收端从旧布局拉出来的
            // 消息（§6.4 原文约束），但同样计入预算窗口（占满即天然限频）。
            if !self.defects.no_report_resync {
                if let Some(msg) = self.pending_layout.clone() {
                    self.stats.layout_resends += 1;
                    self.pending_layout_msg = Some(msg);
                }
            }
            self.last_gap = 0;
            return out;
        }
        if !self.defects.no_report_resync {
            let n = self.version.len().min(versions.len());
            let mut ids = Vec::new();
            let mut behind = 0usize;
            for i in 0..n {
                let epoch_mismatch = epochs.get(i).map(|&e| e != self.epoch[i]).unwrap_or(true);
                // 落后 ≥2 个版本才算"跟不上"，落后 1 版是正常的一帧在途/一次丢包。
                if self.version[i].saturating_sub(versions[i]) >= 2 {
                    behind += 1;
                }
                if self.version[i] > versions[i] || epoch_mismatch {
                    ids.push(TileId(i as u16));
                }
            }
            // 预算只够发一部分时，最陈旧的块必须先走——否则部分发送永远取
            // 低编号块，高编号块被系统性饿死（组合场景实测出现过）。
            ids.sort_by_key(|&TileId(i)| {
                let i = i as usize;
                let gap = self.version[i].saturating_sub(versions.get(i).copied().unwrap_or(0));
                let epoch_miss = u32::from(epochs.get(i).map(|&e| e != self.epoch[i]).unwrap_or(true));
                std::cmp::Reverse((epoch_miss, gap))
            });
            self.stats.report_resends += ids.len() as u64;
            self.last_report_tiles = n;
            self.last_behind = behind;
            self.last_gap = ids.len();
            self.last_gap_ids = ids.clone();
            out = self.resend(&ids, now_ms);
            // 能力证据只在"要发的都发出去了"时才计数：预算把差集截断过，观看端的落后
            // 就是我们没发，不是它啃不动（实测 forced-plane 里这种落后抢在预算执行器
            // 前面翻平面，forced_plane 全程 0，那条契约无法再被证明）。
            // 也不在"从未跟上过"时计数：新观看端补齐整帧之前的报告全是 0，那不是能力证据。
            self.strain_run = if out.len() < ids.len() || !self.ever_current {
                0
            } else if self.receiver_strain_raw(n, behind) {
                self.strain_run.saturating_add(1)
            } else {
                0
            };
            // **空差集只有在"确实有东西可比"时才等于"跟上了"**。观看端比发送端先连上是
            // 常态（`forced-plane` 里隔 1 s），它每 500 ms 发一份报告，很可能在发送端发出
            // 第一帧之前就送到一份「全是 0、发送端也全是 0」——差集天然为空，基线于是被
            // 立成真。之后整帧还在路上（实测 `missing=240`）就被读成"啃不动"，两次即
            // strain，冷启动第一秒就把画面交给一条被压到 300 kbps 的轨道（384x216），
            // 而它下一秒本来就要把无损整帧落画。§14.7.4 的护栏因此形同虚设。
            //
            // 判据是"我们确实描述过这块画布"（发过至少一块），**不是**"版本号被推起来
            // 过"：静止画面只有整帧、没有差分，版本号可以一直停在 0，而那时观看端报
            // "全齐"是真的齐了（`strain-static-exit.sh` 的静止前导就卡在这里）。
            if ids.is_empty() && self.stats.tiles_sent > 0 {
                self.ever_current = true;
            }
        }
        out
    }

    /// 指定块的重发（滚动组超时后的重同步用）。
    pub fn resend_tiles(&mut self, ids: &[TileId], now_ms: u64) -> Vec<TileMsg> {
        self.resend(ids, now_ms)
    }

    /// 上一轮 `on_report` 判定需要重发的布局消息。
    pub fn take_layout_msg(&mut self) -> Option<Message> {
        self.pending_layout_msg.take()
    }

    fn full_frame_msg(&self) -> Message {
        Message::FullFrame {
            layout_id: self.layout_id,
            cols: self.grid.cols,
            rows: self.grid.rows,
            versions: self.version.clone(),
            epochs: self.epoch.clone(),
            contents: self.content.clone(),
        }
    }

    /// 重发一批块，受修复预算约束（§6.6）：
    /// - 需求在预算内 → 正常逐块重发；
    /// - 超预算但整帧装得下且更便宜 → 升级为一次 FullFrame（510 块 ≈ 204 KB
    ///   > 整帧 60 KB）；
    /// - 整帧也装不下 → 在预算内**部分发送**（按块序取前 k 块），其余留给
    ///   下一轮报告。全部丢弃会让低带宽下修复彻底断流（初版实测踩到过）。
    fn resend(&mut self, ids: &[TileId], now_ms: u64) -> Vec<TileMsg> {
        let Some(remaining) = self.repair_remaining(now_ms) else {
            return self.resend_unbudgeted(ids);
        };
        let demand = ids.len() as u64 * Self::REPAIR_UNIT;
        if demand <= remaining {
            self.repair_window_used += demand;
            return self.resend_unbudgeted(ids);
        }
        if remaining >= Self::FULLFRAME_SIZE && demand > Self::FULLFRAME_SIZE * 2 {
            // 升级整帧：只在实际发出时计数
            self.repair_window_used += Self::FULLFRAME_SIZE;
            self.pending_escalation = Some(self.full_frame_msg());
            self.stats.escalations += 1;
            self.stats.repair_throttled += ids.len() as u64;
            return Vec::new();
        }
        let k = (remaining / Self::REPAIR_UNIT).min(ids.len() as u64) as usize;
        self.repair_window_used += k as u64 * Self::REPAIR_UNIT;
        self.stats.repair_throttled += (ids.len() - k) as u64;
        self.resend_unbudgeted(&ids[..k])
    }

    fn resend_unbudgeted(&mut self, ids: &[TileId]) -> Vec<TileMsg> {
        let mut out = Vec::new();
        for &id in ids {
            let i = id.0 as usize;
            let seq = self.alloc_seq();
            self.sent_log.push((seq, id));
            self.stats.resends += 1;
            out.push(TileMsg {
                id,
                epoch: self.epoch[i],
                version: self.version[i],
                content: self.content[i],
            });
        }
        out
    }

    /// 发送端时钟由调用方同步推进（模型里用同一时间基准）。
    pub fn set_now(&mut self, now_ms: u64) {
        self.now_hint = now_ms;
    }
}
