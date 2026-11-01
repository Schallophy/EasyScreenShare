//! 协议属性测试：§14.2 的 PROP-1 分解（P1-1…P1-7）。
//!
//! 这些测试跑的是 `ess_core::model`（§6 的可执行镜像），不是真实编解码器。
//! 目的：在写传输层之前，先把协议语义钉死。
//!
//! **无补丁协议**（§14.5 决策）：原 P1-3（底图安全）、P1-4（补丁 epoch）、
//! PROP-E（补丁占比）随补丁一起删除——它们描述的概念已不存在。
//! 新增 **P1-7（滚动组原子性）**：画布不允许出现发送端从未存在过的拼接。

use std::collections::HashSet;

use proptest::prelude::*;
use ess_core::model::{Defects, Grid, Link, Message, NetConfig, Network, Receiver, RxDefects, Sender, TileId, TileMsg};

const COLS: u16 = 4;
const ROWS: u16 = 3;
const TILES: u16 = COLS * ROWS;
const STEP_MS: u64 = 5;
/// 接收端周期报告间隔（步数）。对应 §6.5 的 2 s StateVector，模型里缩短。
const REPORT_EVERY: usize = 10;
/// 滚动组超时。对应"组迟迟不齐就请求重同步"。
const GROUP_TIMEOUT_MS: u64 = 100;

#[derive(Clone, Debug)]
enum Op {
    Edit { tile: u16, content: u64 },
    /// tiles = 全部平移块；dirty = 将重发的条带块（⊆ tiles）
    Scroll { tiles: Vec<u16>, dirty: Vec<u16> },
    Layout { cols: u16, rows: u16 },
}

struct Harness {
    net: Network,
    sender: Sender,
    receiver: Receiver,
    link: Link,
    rtt: u64,
    rng: u64,
    content_seq: u64,
    step: usize,
    /// 发送端产生过的 (块, 版本, 内容) —— 供 P1-7 的内容有效性检查
    produced: HashSet<(u16, u32, u64)>,
}

impl Harness {
    fn new(cfg: NetConfig, seed: u64) -> Self {
        Self::with_defects(cfg, seed, Defects::default(), RxDefects::default())
    }

    fn with_defects(cfg: NetConfig, seed: u64, tx: Defects, rx: RxDefects) -> Self {
        let grid = Grid::new(COLS, ROWS);
        Self {
            net: Network::new(cfg, seed),
            sender: Sender::with_defects(grid, tx),
            receiver: Receiver::with_defects(grid, rx),
            link: Link::new(),
            rtt: cfg.one_way_ms * 2,
            rng: seed | 1,
            content_seq: 1,
            step: 0,
            produced: HashSet::new(),
        }
    }

    fn rand(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64) / ((1u64 << 53) as f64)
    }

    fn next_content(&mut self) -> u64 {
        self.content_seq += 1;
        self.content_seq
    }

    fn send(&mut self, msg: Message) {
        self.net.send(msg);
    }

    fn record(&mut self, msg: &Message) {
        if let Message::Update { tiles, .. } = msg {
            for t in tiles {
                self.produced.insert((t.id.0, t.version, t.content));
            }
        }
    }

    /// 推进一个时间步：投递 → 应用 → NACK → 周期报告 → 组超时 → P1-7 检查。
    fn pump(&mut self) {
        self.sender.set_now(self.net.now_ms);

        for (seq, msg) in self.net.advance(STEP_MS) {
            self.link.observe(seq);
            self.record(&msg);
            self.receiver.on_message(&msg);
        }

        // seq 空洞 → 补发请求（快路径）
        if let Some((from, to)) = self.link.pending_nack() {
            let resends = self.sender.on_nack(from, to, self.net.now_ms);
            if !resends.is_empty() {
                let msg = self.sender_update(resends);
                self.record(&msg);
                self.send(msg);
            }
        }

        // 周期报告 → 差异重发（兜底路径，§6.5 版本向量校验）
        self.step += 1;
        if self.step % REPORT_EVERY == 0 {
            if let Message::Report { layout_id, versions, epochs } = self.receiver.report() {
                let resends = self.sender.on_report(layout_id, &versions, &epochs, self.net.now_ms);
                if !resends.is_empty() {
                    let msg = self.sender_update(resends);
                    self.record(&msg);
                    self.send(msg);
                }
                if let Some(m) = self.sender.take_layout_msg() {
                    self.net.send_reliable(m);
                }
            }
        }

        // 滚动组超时 → 请求重同步（组不解散）
        let need = self.receiver.tick(self.net.now_ms, GROUP_TIMEOUT_MS);
        if !need.is_empty() {
            let resends = self.sender.resend_tiles(&need, self.net.now_ms);
            if !resends.is_empty() {
                let msg = self.sender_update(resends);
                self.record(&msg);
                self.send(msg);
            }
        }

        // P1-7：画布不允许出现部分应用的滚动组
        if let Err(e) = self.receiver.canvas_is_consistent() {
            panic!("P1-7 违反：{e}");
        }
    }

    fn sender_update(&self, tiles: Vec<TileMsg>) -> Message {
        Message::Update {
            layout_id: self.sender.layout_id,
            cols: self.sender.grid.cols,
            rows: self.sender.grid.rows,
            tiles,
        }
    }

    fn apply(&mut self, op: &Op) {
        self.sender.set_now(self.net.now_ms);
        let n = self.sender.grid.len();
        match op {
            Op::Edit { tile, content } => {
                // 布局变更后网格可能变小；不存在的块直接跳过
                if (*tile as usize) >= n {
                    return;
                }
                let msg = self.sender.edit(TileId(*tile), *content);
                self.record(&msg);
                self.send(msg);
            }
            Op::Scroll { tiles, dirty } => {
                let ids: Vec<TileId> =
                    tiles.iter().filter(|&&t| (t as usize) < n).map(|&t| TileId(t)).collect();
                if ids.is_empty() {
                    return;
                }
                let dirty: Vec<(TileId, u64)> = dirty
                    .iter()
                    .filter(|&&t| (t as usize) < n && ids.contains(&TileId(t)))
                    .map(|&t| (TileId(t), self.next_content()))
                    .collect();
                let (announce, update) = self.sender.scroll(&ids, &dirty);
                // 宣告走可靠通道（§6.7：ctl 上的 Scroll）
                self.net.send_reliable(announce);
                self.record(&update);
                self.send(update);
            }
            Op::Layout { cols, rows } => {
                let msg = self.sender.change_layout(Grid::new(*cols, *rows));
                self.send(msg);
            }
        }
    }

    /// 发送端认为接收端应当拥有的状态。
    fn sender_state(&self) -> Vec<(u32, u32, u64)> {
        self.sender
            .content
            .iter()
            .enumerate()
            .map(|(i, &c)| (self.sender.epoch[i], self.sender.version[i], c))
            .collect()
    }

    fn converged(&self) -> bool {
        self.receiver.state() == self.sender_state()
    }

    /// 失败时打印收发两端的差异，而不是只说"不相等"。
    fn diff(&self) -> String {
        let s = self.sender_state();
        let r = self.receiver.state();
        let mut out = format!(
            "layout sender={} receiver={} | grid sender={}x{} receiver={}x{} | missing={:?} groups={}",
            self.sender.layout_id,
            self.receiver.layout_id,
            self.sender.grid.cols,
            self.sender.grid.rows,
            self.receiver.grid.cols,
            self.receiver.grid.rows,
            self.receiver.missing,
            self.receiver.pending_groups(),
        );
        for i in 0..s.len().min(r.len()) {
            if s[i] != r[i] {
                out.push_str(&format!(
                    "\n  tile {i}: sender=(e{},v{},c{}) receiver=(e{},v{},c{})",
                    s[i].0, s[i].1, s[i].2, r[i].0, r[i].1, r[i].2
                ));
            }
        }
        out
    }

    fn run(&mut self, ops: &[Op], settle_steps: usize) {
        for op in ops {
            self.apply(op);
            for _ in 0..2 {
                self.pump();
            }
        }
        for _ in 0..settle_steps {
            self.pump();
        }
    }
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u16..TILES, any::<u64>())
            .prop_map(|(tile, content)| Op::Edit { tile, content }),
        prop::collection::vec(0u16..TILES, 0..4).prop_flat_map(|tiles| {
            let n = tiles.len();
            let pick = prop::collection::vec(0..n.max(1), 0..=n);
            (Just(tiles.clone()), pick).prop_map(move |(tiles, idx)| Op::Scroll {
                dirty: idx.into_iter().filter(|&i| i < tiles.len()).map(|i| tiles[i]).collect(),
                tiles,
            })
        }),
        (2u16..6, 2u16..4).prop_map(|(cols, rows)| Op::Layout { cols, rows }),
    ]
}

fn config(drop: f64, dup: f64, reorder: bool) -> NetConfig {
    NetConfig { drop_rate: drop, dup_rate: dup, one_way_ms: 20, reorder, bandwidth_bps: None }
}

// ---------------------------------------------------------------- P1-1

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// P1-1：无损直传 —— 无丢包无乱序时，接收端状态逐块等于发送端 last_sent。
    #[test]
    fn p1_1_clean_link_converges(ops in prop::collection::vec(op_strategy(), 0..24)) {
        let mut h = Harness::new(config(0.0, 0.0, false), 0x5eed);
        h.run(&ops, 200);
        prop_assert!(h.converged(), "receiver != sender\n{}", h.diff());
        prop_assert!(h.receiver.violations.is_empty(), "violations: {:?}", h.receiver.violations);
    }
}

// ---------------------------------------------------------------- P1-2

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// P1-2：丢包收敛 —— 任意丢包/重复/乱序下，只要传输持续，最终收敛。
    #[test]
    fn p1_2_converges_under_loss(
        ops in prop::collection::vec(op_strategy(), 0..24),
        drop in 0.0f64..0.2,
        dup in 0.0f64..0.2,
        reorder in any::<bool>(),
    ) {
        let mut h = Harness::new(config(drop, dup, reorder), 0xc0ffee);
        h.run(&ops, 400);
        prop_assert!(h.converged(), "receiver != sender (drop={drop} dup={dup} reorder={reorder})\n{}", h.diff());
        prop_assert!(h.receiver.violations.is_empty(), "violations: {:?}", h.receiver.violations);
    }
}

// ---------------------------------------------------------------- P1-7

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// P1-7：滚动组原子性 —— 任意丢包/乱序下，画布**任何时刻**都不允许出现
    /// "一半新基准一半旧基准"的拼接（发送端从未存在过的画面）。
    /// 该断言在每次 pump 后都执行（见 `Harness::pump`）。
    #[test]
    fn p1_7_canvas_never_tears(
        ops in prop::collection::vec(op_strategy(), 0..24),
        drop in 0.0f64..0.25,
        reorder in any::<bool>(),
    ) {
        let mut h = Harness::new(config(drop, 0.05, reorder), 0x7ea1);
        h.run(&ops, 200);
        prop_assert!(h.receiver.violations.is_empty(), "violations: {:?}", h.receiver.violations);
    }
}

// ---------------------------------------------------------------- P1-5

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// P1-5：布局切换 —— layout_id 变更后，旧布局消息被丢弃，最终收敛到新布局。
    #[test]
    fn p1_5_layout_switch(
        edits in prop::collection::vec((0u16..TILES, any::<u64>()), 0..8),
        cols in 2u16..6,
        rows in 2u16..4,
    ) {
        let mut h = Harness::new(config(0.05, 0.0, false), 0x1234);
        let ops: Vec<Op> = edits
            .iter()
            .map(|(t, c)| Op::Edit { tile: *t, content: *c })
            .collect();
        h.run(&ops, 100);

        let old_layout = h.sender.layout_id;
        h.apply(&Op::Layout { cols, rows });
        for _ in 0..300 { h.pump(); }

        prop_assert!(h.sender.layout_id > old_layout, "layout_id 未递增");
        prop_assert!(h.receiver.layout_id == h.sender.layout_id, "接收端布局未跟上");
        prop_assert!(h.converged(), "布局切换后未收敛\n{}", h.diff());
        prop_assert!(h.receiver.violations.is_empty(), "violations: {:?}", h.receiver.violations);
    }
}

/// P1-5 的直接断言：旧布局的消息必须被丢弃。
#[test]
fn old_layout_messages_are_dropped() {
    let grid = Grid::new(COLS, ROWS);
    let mut sender = Sender::new(grid);
    let mut receiver = Receiver::new(grid);

    let old = sender.edit(TileId(0), 42);
    let new = sender.change_layout(Grid::new(2, 2));
    receiver.on_message(&new);

    let verdict = receiver.on_message(&old);
    assert_eq!(verdict, ess_core::model::Verdict::DroppedOldLayout, "旧布局消息必须被丢弃");
}

// ---------------------------------------------------------------- P1-6

/// P1-6：整块重发的幂等性 —— 重发任意次与重发一次结果相同。
#[test]
fn whole_block_resend_is_idempotent() {
    let grid = Grid::new(COLS, ROWS);
    let mut sender = Sender::new(grid);
    let mut receiver = Receiver::new(grid);

    let msg = sender.edit(TileId(3), 0xabcd);
    receiver.on_message(&msg);
    let once = receiver.state();

    for _ in 0..5 {
        let resends = sender.on_nack(1, 100, 0);
        let msg = Message::Update {
            layout_id: sender.layout_id,
            cols: sender.grid.cols,
            rows: sender.grid.rows,
            tiles: resends,
        };
        receiver.on_message(&msg);
    }
    assert_eq!(receiver.state(), once, "整块重发改变了状态");
    assert!(receiver.violations.is_empty(), "{:?}", receiver.violations);
}

// ================================================================ 变异测试
//
// 把修掉的缺陷重新注入，确认对应的属性**确实会失败**。没有这一步，
// "属性测试全部通过"只说明模型没 bug，不说明测试抓得到 bug。
//
// 注：补丁相关的 3 个缺陷（链未重置、底图误标、未来 epoch 补丁）已随补丁
// 一起从协议中删除（§14.5），它们的变异测试也随之删除——**缺陷类别的消失
// 本身就是砍掉补丁的收益之一**。

/// 缺陷：布局变更后不清 `seq → 块` 映射 ⇒ NACK 重发越界的块。
#[test]
fn mutation_keep_sent_log_is_caught() {
    let mut sender = Sender::with_defects(
        Grid::new(COLS, ROWS),
        Defects { keep_sent_log: true, ..Default::default() },
    );
    let _ = sender.edit(TileId(11), 1);              // 旧布局里合法的块
    let _ = sender.change_layout(Grid::new(2, 2));    // 网格缩到 4 块
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sender.on_nack(1, 100, 0)
    }));
    assert!(out.is_err(), "缺陷未被检出：旧布局的块编号没有导致越界");
}

/// 缺陷：报告驱动的差异重发被禁用 ⇒ 布局消息丢失后接收端永远停在旧布局。
#[test]
fn mutation_no_report_resync_is_caught() {
    let mut sender = Sender::with_defects(
        Grid::new(COLS, ROWS),
        Defects { no_report_resync: true, ..Default::default() },
    );
    let mut receiver = Receiver::new(Grid::new(COLS, ROWS));
    // 布局消息丢失，且之后再无任何块更新
    let _lost = sender.change_layout(Grid::new(2, 2));
    let mut now = 0u64;
    for _ in 0..60 {
        now += 5;
        sender.set_now(now);
        if let Message::Report { layout_id, versions, epochs } = receiver.report() {
            let resends = sender.on_report(layout_id, &versions, &epochs, now);
            assert!(resends.is_empty(), "no_report_resync 下不应有差异重发");
        }
        if let Some(m) = sender.take_layout_msg() {
            receiver.on_message(&m);
        }
    }
    assert_ne!(
        receiver.layout_id,
        sender.layout_id,
        "缺陷未被检出：布局消息丢失后接收端仍然跟上了"
    );
}

/// 缺陷：滚动组的块不等组齐就应用 ⇒ 画布撕裂（P1-7 抓到）。
#[test]
fn mutation_apply_before_group_complete_is_caught() {
    let grid = Grid::new(COLS, ROWS);
    let mut sender = Sender::new(grid);
    let mut receiver = Receiver::with_defects(
        grid,
        RxDefects { apply_before_group_complete: true, ..Default::default() },
    );

    let ids = [TileId(0), TileId(1), TileId(2)];
    let dirty: Vec<(TileId, u64)> = vec![(TileId(0), 10), (TileId(1), 11), (TileId(2), 12)];
    let (announce, update) = sender.scroll(&ids, &dirty);
    receiver.on_message(&announce);

    // 只投递组内第一块（其余丢失）
    if let Message::Update { layout_id, cols, rows, tiles } = update {
        let partial = Message::Update {
            layout_id,
            cols,
            rows,
            tiles: vec![tiles[0]],
        };
        receiver.on_message(&partial);
    }

    let check = receiver.canvas_is_consistent();
    assert!(check.is_err(), "缺陷未被检出：部分应用的滚动组没有被 P1-7 发现");
}

/// 缺陷：整帧无条件覆盖 ⇒ 状态回退，违反单调性。
#[test]
fn mutation_full_frame_clobbers_is_caught() {
    let grid = Grid::new(COLS, ROWS);
    let mut receiver = Receiver::with_defects(
        grid,
        RxDefects { full_frame_clobbers: true, ..Default::default() },
    );
    let whole = TileMsg { id: TileId(0), epoch: 0, version: 9, content: 42 };
    let msg = Message::Update { layout_id: 1, cols: COLS, rows: ROWS, tiles: vec![whole] };
    receiver.on_message(&msg);
    // 迟到的整帧快照（版本 0）
    let ff = Message::FullFrame {
        layout_id: 1,
        cols: COLS,
        rows: ROWS,
        versions: vec![0; (COLS * ROWS) as usize],
        epochs: vec![0; (COLS * ROWS) as usize],
        contents: vec![0; (COLS * ROWS) as usize],
    };
    receiver.on_message(&ff);
    assert_eq!(receiver.version[0], 0, "缺陷未被检出：整帧没有把已更新的状态回退");
}
