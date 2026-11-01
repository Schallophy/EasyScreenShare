//! 模拟网络：可控的丢包、乱序、重复与延迟。
//!
//! 两条通道：
//! - **不可靠**（对应 `px`）：会丢、会乱序、会重复。
//! - **可靠**（对应 `ctl`）：不丢、不乱序。滚动宣告走这条——§6.7。

use super::TileId;

/// 单条消息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// 滚动宣告（可靠通道）。携带滚动组内每块的目标 epoch，以及
    /// **真正会重发的 dirty 块**（新暴露的条带）。
    ///
    /// 宣告永不丢失，因此接收端总能知道"这次滚动涉及哪些块"，滚动组可以
    /// 做成原子应用而不必担心等不到基准（原活锁缺陷的根治手段）。
    /// v0.7：组按 dirty 到齐应用——平移块只需 epoch 推进，像素由位移得到，
    /// 不再逐块重发（否则整屏滚动退化为全量重发）。
    ScrollAnnounce {
        layout_id: u32,
        scroll_id: u32,
        /// 正值 = 内容上移（浏览内容向下滚）。
        dy: i32,
        targets: Vec<(TileId, u32)>,
        dirty: Vec<TileId>,
    },
    /// 一批块更新（不可靠通道）。
    Update {
        layout_id: u32,
        cols: u16,
        rows: u16,
        tiles: Vec<TileMsg>,
    },
    /// 整帧：布局变更后的全量重同步。必须携带 epoch 向量（epoch 全局单调，
    /// 接收端要靠它初始化新网格，见 §6.3.1）。
    FullFrame {
        layout_id: u32,
        cols: u16,
        rows: u16,
        versions: Vec<u32>,
        epochs: Vec<u32>,
        contents: Vec<u64>,
    },
    /// 接收端周期报告（上行）：驱动发送端的差异重发（§6.5 校验）。
    /// 按 (epoch, version) 对比对，见 §6.4。
    Report {
        layout_id: u32,
        versions: Vec<u32>,
        epochs: Vec<u32>,
    },
}

/// 单块内容。**无补丁协议**：所有块都是绝对内容（评审二.5 / §14.5 决策）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileMsg {
    pub id: TileId,
    pub epoch: u32,
    pub version: u32,
    pub content: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct NetConfig {
    pub drop_rate: f64,
    pub dup_rate: f64,
    /// 单程延迟
    pub one_way_ms: u64,
    /// 是否打乱投递顺序
    pub reorder: bool,
    /// 出口带宽整形（bps）。`None` = 不限。只作用于不可靠通道（对应 px 上行），
    /// 可靠通道（ctl）不走整形——弱网下瓶颈在上行像素流。
    pub bandwidth_bps: Option<u64>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            drop_rate: 0.0,
            dup_rate: 0.0,
            one_way_ms: 20,
            reorder: false,
            bandwidth_bps: None,
        }
    }
}

#[derive(Clone)]
struct Envelope {
    seq: u32,
    deliver_at: u64,
    msg: Message,
    reliable: bool,
}

/// 出口整形队列的一帧。
struct EgressItem {
    seq: u32,
    size: u64,
    msg: Message,
}

/// 合成报文大小（字节）。模型不编码真实像素，用整块协议的典型值：
/// 块 ≈ 400 B（64×64 WebP 无损 [实测 350–560]），整帧 ≈ 60 KB [实测]。
pub fn msg_size(msg: &Message) -> u64 {
    match msg {
        Message::Update { tiles, .. } => 16 + tiles.len() as u64 * 400,
        Message::ScrollAnnounce { targets, .. } => 24 + targets.len() as u64 * 8,
        Message::FullFrame { .. } => 60_000,
        Message::Report { .. } => 4_100,
    }
}

pub struct Network {
    pub cfg: NetConfig,
    pub now_ms: u64,
    inflight: Vec<Envelope>,
    /// 出口整形队列（带宽受限时排队）
    egress: std::collections::VecDeque<EgressItem>,
    /// 出口链路空闲时刻。串行化模型：一条报文占用链路 `size / 带宽` 毫秒，
    /// 期间后续报文排队。比令牌桶更真实——令牌桶的突发上限会永久卡死
    /// 超过大小的报文（§14.6 初版实测踩到过）。
    link_busy_until: u64,
    /// 下一个 seq
    pub next_seq: u32,
    /// 随机源（xorshift，避免依赖外部 crate）
    rng: u64,
}

impl Network {
    pub fn new(cfg: NetConfig, seed: u64) -> Self {
        Self {
            cfg,
            now_ms: 0,
            inflight: Vec::new(),
            egress: std::collections::VecDeque::new(),
            link_busy_until: 0,
            next_seq: 1,
            rng: seed | 1,
        }
    }

    /// 出口排队中的消息数（带宽饥饿的观察窗口）。
    pub fn egress_len(&self) -> usize {
        self.egress.len()
    }

    fn rand(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64) / ((1u64 << 53) as f64)
    }

    /// 不可靠发送。返回实际分配的 seq（被丢弃时也分配，保持 seq 空间连续）。
    pub fn send(&mut self, msg: Message) -> u32 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.rand() < self.cfg.drop_rate {
            return seq;
        }
        if let Some(_bw) = self.cfg.bandwidth_bps {
            // 走令牌桶：排队，按可用带宽逐步放行
            self.egress.push_back(EgressItem { seq, size: msg_size(&msg), msg });
            return seq;
        }
        let deliver_at = self.now_ms + self.cfg.one_way_ms;
        self.inflight.push(Envelope { seq, deliver_at, msg: msg.clone(), reliable: false });
        if self.rand() < self.cfg.dup_rate {
            let extra = self.cfg.one_way_ms / 2 + 1;
            self.inflight.push(Envelope { seq, deliver_at: deliver_at + extra, msg, reliable: false });
        }
        seq
    }

    /// 出口链路串行化放行。在每个 advance 开头调用。
    fn drain_egress(&mut self) {
        let Some(bw) = self.cfg.bandwidth_bps else { return };
        // 每毫秒可发字节 = bps / 8 / 1000
        let per_ms = (bw as f64) / 8000.0;
        while let Some(front) = self.egress.front() {
            if self.link_busy_until > self.now_ms {
                break;
            }
            let _ = front;
            let item = self.egress.pop_front().unwrap();
            let tx_ms = (item.size as f64 / per_ms).ceil() as u64;
            self.link_busy_until = self.now_ms.max(self.link_busy_until) + tx_ms.max(1);
            self.inflight.push(Envelope {
                seq: item.seq,
                deliver_at: self.link_busy_until + self.cfg.one_way_ms,
                msg: item.msg,
                reliable: false,
            });
        }
    }

    /// 可靠发送（对应 `ctl`）：不丢、不乱序、即时入队。
    pub fn send_reliable(&mut self, msg: Message) -> u32 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.inflight.push(Envelope {
            seq,
            deliver_at: self.now_ms + self.cfg.one_way_ms,
            msg,
            reliable: true,
        });
        seq
    }

    /// 推进时间并返回到期可投递的消息。
    pub fn advance(&mut self, dt_ms: u64) -> Vec<(u32, Message)> {
        self.now_ms += dt_ms;
        self.drain_egress();
        let mut ready: Vec<Envelope> = Vec::new();
        let mut keep: Vec<Envelope> = Vec::new();
        for e in self.inflight.drain(..) {
            if e.deliver_at <= self.now_ms {
                ready.push(e);
            } else {
                keep.push(e);
            }
        }
        self.inflight = keep;
        // 可靠消息保持顺序且先投递（对应 ctl）；不可靠消息可乱序
        let mut reliable: Vec<Envelope> = ready.iter().filter(|e| e.reliable).cloned().collect();
        let mut lossy: Vec<Envelope> = ready.iter().filter(|e| !e.reliable).cloned().collect();
        if self.cfg.reorder && lossy.len() > 1 {
            lossy.reverse();
        }
        reliable.extend(lossy);
        reliable.into_iter().map(|e| (e.seq, e.msg)).collect()
    }

    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }
}
