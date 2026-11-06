//! 像素管线：真实帧 ⇄ 模型消息 ⇄ 线格式（§7 / §6.7）。
//!
//! 发送端：滚动检测 → 差分（对平移基准）→ §7.2 粒度 → 编码 → WireMsg。
//! 接收端：WireMsg → 模型判定（原子滚动组）→ 解码 → 画布。
//!
//! 模型接收端仍是**唯一**的判定权威；本模块只负责在判定为 `Applied` /
//! 组完成时把像素落到画布上。

use std::collections::HashMap;

use crate::codec::{CodecError, TileCodec, TileCodecId};
use crate::frame::{
    extract_tile, tile_hash, Frame, SendState, TileGeom, TileGrid,
};
use crate::model::{self, Message, Receiver, Sender, TileId};
use crate::scroll::{apply_vscroll, detect};
use crate::wire::{WireMsg, WireTile};
use crate::{TILE, SUBTILE};

/// 报告重发的分块粒度：每块自成一条消息（各自一个 `seq`），接收端逐块原子应用。
///
/// 报告差集在动画/大面积变化之后可能是整屏（几百块、上兆字节），而传输层会把一条
/// 消息按 60 KB 拆成十几片**共享 `seq`**——接收端只能整组到齐才落画布（§14.7.35 的
/// 撕裂修法）。丢任意一片就整组作废，下一份报告从头再来一次；现场表现正是"图块残留
/// 要好几秒才补上"（快速打开/最小化窗口的弹出收回动画最容易撞上）。
///
/// 切小以后，丢一片只损失 8 块，其余立即上屏；下一份报告只补缺的那些，画面是渐进的
/// 而不是全有或全无。8 块在 64x64 JPEG 最坏情况下约 32 KB，仍在单条消息预算内。
const REPORT_CHUNK_TILES: usize = 8;

#[derive(Default, Debug, Clone, Copy)]
pub struct SendStats {
    pub frames: u64,
    pub full_frames: u64,
    pub scrolls: u64,
    pub updates: u64,
    pub tiles_sent: u64,
    /// 走有损图块（JPEG）发出去的块数：这是"流畅度优先"那条线真正生效的证据。
    pub tiles_lossy: u64,
    pub bytes_encoded: u64,
    /// 分段耗时累计（µs）。存在的理由见 §14.7.8：会话侧 `tile_enc=` 的 t0 架在整个
    /// `on_frame` 外面（差分 + 策略判定 + 逐块编码 + 打包），归因不出到底是哪一段
    /// 压掉了帧预算。这两条把**差分**与**编码打包**（含整帧）各自夹出来。
    pub diff_us: u64,
    pub encode_us: u64,
    /// 滚动检测 `detect()` 的累计耗时：它是整帧比对（代码注释自己写着 1080p 下
    /// ~10 ms/帧），是 §14.7.10 剩下那段未归因成本的头号候选。
    pub detect_us: u64,
    pub motion_frames: u64,
    pub motion_bytes: u64,
    pub motion_dropped: u64,
    /// 平面切换次数（图块 ⇄ 视频）：决策器的抖动程度
    pub video_on: u64,
    pub video_off: u64,
    /// 运动退出走精确修复的次数 / 修复块数（对照整帧退出的全块数）
    pub repairs: u64,
    pub repair_tiles: u64,
    /// 因链路预算不足被逼进视频平面的次数（图块通路装不下）
    pub budget_forced: u64,
    /// 因**单帧编码耗时超预算**被逼进视频平面的次数（图块通路做不快）
    pub encode_forced: u64,
    /// 因**观看端报告大面积落后**被逼进视频平面的次数（观看端啃不动图块）
    pub strain_forced: u64,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct RecvStats {
    pub applied_tiles: u64,
    pub stale_dropped: u64,
    pub buffered_tiles: u64,
    pub scrolls_applied: u64,
    pub full_frames: u64,
    pub bytes_decoded: u64,
    pub motion_frames: u64,
    /// 视频模式下被跳过的 JPEG 兜底帧（本端走媒体通道）
    pub motion_skipped: u64,
    /// 凑齐并原子应用的拆片更新帧数
    pub updates_atomic: u64,
    /// 新帧到来时仍未凑齐、被丢弃的残帧数（缺块由状态报告补）
    pub update_frames_dropped: u64,
    /// 迟到/重复的旧拆片数
    pub update_parts_stale: u64,
}

/// 编码器集合：按顺序尝试（首个为主编码器），解码按 id 路由。
/// 真实桌面：调色板优先（快、小），>256 色自动降级 PNG。
#[derive(Clone, Copy)]
pub struct CodecSet<'c> {
    pub codecs: &'c [&'c dyn TileCodec],
}

impl<'c> CodecSet<'c> {
    pub fn encode(&self, raw: &[u8], w: u32, h: u32) -> Result<(TileCodecId, Vec<u8>), CodecError> {
        let mut last = CodecError::Unsupported;
        for c in self.codecs {
            match c.encode(raw, w, h) {
                Ok(bytes) => return Ok((c.id(), bytes)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// 按"流畅度优先"选码。规则只用**可算的价**，不看内容语义：
    /// 1. 按顺序试无损链（调色板在前——平涂块它最便宜，实测中位 102 B，比 JPEG 还小 12 倍）；
    /// 2. 首个成功的结果不超过 `trial_floor` 就直接收——64×64 照片块的 JPEG 中位也要 1.2 KB，
    ///    再比一次只是白花 0.09–0.25 ms；
    /// 3. 否则把有损候选也编出来，**省够 `save_x` 倍**才降级。实测照片块省 4.6×，
    ///    而"照片背景上压字"只省 2.5× —— 后者降级会把字糊掉、还只省几十毫秒，不值。
    pub fn encode_by_cost(
        &self,
        raw: &[u8],
        w: u32,
        h: u32,
        trial_floor: usize,
        save_x: u32,
        save_x_edgy: u32,
        edges_min: usize,
    ) -> Result<(TileCodecId, Vec<u8>), CodecError> {
        // 两档门限：**含高对比结构的块**（字、细线、图标边缘）要求省得更多才许降级。
        // 依据（实测，`codec_cost.rs`）：满量程跳变计数在照片块=0、"照片上压字"=462、
        // 文本块=192..544 —— 这条能算的量把"糊了会心疼"的那类分出来，且只花一次 4096 px 扫描。
        let save_x = if full_scale_edges(raw, w, h) >= edges_min { save_x_edgy } else { save_x };
        let mut last = CodecError::Unsupported;
        for c in self.codecs {
            match c.encode(raw, w, h) {
                Ok(bytes) => {
                    if c.lossy() || bytes.len() <= trial_floor {
                        return Ok((c.id(), bytes));
                    }
                    for l in self.codecs.iter().filter(|l| l.lossy()) {
                        if let Ok(lb) = l.encode(raw, w, h) {
                            if lb.len().saturating_mul(save_x as usize) <= bytes.len() {
                                return Ok((l.id(), lb));
                            }
                        }
                    }
                    return Ok((c.id(), bytes));
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub fn decode_into(
        &self,
        id: TileCodecId,
        data: &[u8],
        dst: &mut [u8],
        stride: u32,
    ) -> Result<(), CodecError> {
        for c in self.codecs {
            if c.id() == id {
                return c.decode_into(data, dst, stride);
            }
        }
        Err(CodecError::Corrupt("no codec for id"))
    }
}

pub struct SendPipeline<'c> {
    pub grid: TileGrid,
    pub ctl: Sender,
    pub state: SendState,
    pub codecs: CodecSet<'c>,
    /// 关闭后全部走逐块差分（对照实验用）
    pub scroll_enabled: bool,
    /// 选码门限：无损结果 ≤ 这个字节数就不试有损（64×64 照片块的 JPEG 中位 1.2 KB）。
    pub lossy_trial_floor: usize,
    /// 有损要省到这个倍数才允许降级（2 = 平缓的照片块省不到两倍就留无损）。
    pub lossy_save_x: u32,
    /// 含高对比结构（字/细线，见 `full_scale_edges`）的块要求省到这个倍数——更保守。
    pub lossy_save_x_edgy: u32,
    /// 满量程跳变达到多少算"含高对比结构"（实测：照片块 0，含字块 ≥192）。
    pub lossy_edges_min: usize,
    pub stats: SendStats,
    seq: u32,
    frame_id: u32,
    initialized: bool,
    /// 运动帧：大面积变化时整帧降采样 JPEG（有损），保帧率
    pub motion_enabled: bool,
    pub motion_max_w: u32,
    pub motion_max_h: u32,
    pub motion_quality: u8,
    pub motion_threshold: f32,
    motion_active: bool,
    /// 退出运动的迟滞：避免动静交替时反复补整帧
    motion_hold: u32,
    /// 迟滞窗口长度（帧数），`ESS_MOTION_HOLD` 可调；测量台用它把平面停在视频侧
    pub hold_frames: u32,
    probe: Vec<u32>,
    probe_dims: (usize, usize),
    /// 视频平面可用（调用方已发布视频轨并会消费 `video_mode()`）：
    /// 大面积运动时画面改由媒体通道出，JPEG 运动通路让位（不再发有损帧，
    /// 所以观看端画布在视频平面期间始终等于上一次无损画面）。
    pub video_plane: bool,
    video_mode: bool,
    motion_seq: u32,
    /// 进入运动那一刻的画面 = 观看端画布此刻真正持有的内容。退出运动时按它
    /// 做精确差分修复（比 1.2 MB 整帧小一个数量级，交接延迟从 0.4–1 s 降到几十
    /// 毫秒）。运动期间发过整帧（迟到者/丢帧）时基线不再唯一，置 None 退回整帧。
    repair_baseline: Option<Frame>,
    /// 本次平面 episode 开始时 `stats.motion_frames` 的读数。
    ///
    /// 退出时如果两数相等，说明这段期间**没有一帧有损 JPEG 落过观看端画布**（媒体通道自己
    /// 在出画），那么"基线 ↔ 当前帧"的定点差集就是完备的；不相等说明画布被有损帧盖过，
    /// 基线不可信，必须整帧重建（#56 的教训）。
    plane_motion_start: u64,
    /// 图块通路的每秒预算 bps（0 = 不限制）。由调用方按真实链路统计设定：
    /// 无损更新已经装不下这条链路时，画面交给媒体通道而不是继续堆队列。
    budget_bps: u64,
    /// 无损平面的**编码耗时**已经装不下一帧的预算（调用方按实测单帧耗时设定）。
    /// 这跟链路装不下是同一种病：继续留在图块通路，画面就是按序播放的旧片子，
    /// 降帧率只是把延迟摊平。装不下就把画面交给媒体通道（H264 一帧 5–10 ms）。
    pub encode_pressure: bool,
    /// 预热最长多久就一定交接（µs）。0 = 关掉预热，回到"决定即交接"的旧行为
    /// （测量台用它做反向对照）。由调用方设定，见 [`Self::set_warm_max_us`]。
    warm_max_us: u64,
    /// 最近一次观看端报告是否大面积落后 ≥2 版（观看端解码跟不上图块通路）。
    /// 由 `on_report` 自己更新，不需要调用方接线。
    pub receiver_strain: bool,
    /// 1 s 窗口的起点与已用字节/新变化块数（用帧自带的时间戳，测试里可确定性推进）
    win_ts: u64,
    win_bytes: u64,
    win_tiles: u64,
    last_encoded: u64,
    /// 当前平面是被**预算**逼进来的（不是大面积运动），退出条件不同
    budget_plane: bool,
    /// 本次平面是被"无损编码耗时超预算"单独逼进来的。平面期内图块通路不编码，
    /// 实测不到的新耗时不能当续期理由，退出改看当前的差异占比。
    encode_plane: bool,
    /// 算力路径进入平面的最低变化占比（变化块 / 总块）。低于它先降帧率/尺寸，
    /// 不切有损平面——小改动不值得付出 H264 的清晰度代价。
    pub encode_plane_min_ratio: f32,
    /// 预热中：决策已经把画面判给媒体平面，但**还没对它做宣告**，无损图块仍是画面
    /// 来源。见 [`Self::plane_handover`]。
    media_pending: bool,
    /// 预热开始时刻（µs）。用采集帧时间戳而不是墙钟：单测要能自己推进它。
    warm_started_us: u64,
    /// 预热期内"交付达到声明帧率 90%"的连续 1 s 窗计数。
    warm_ok_run: u32,
    /// 预热期里上一次观察到的**编码器自己选的**尺寸（0,0 = 还没观察到）。
    /// 尺寸在变就说明码控还没安顿下来，见 [`Self::note_media_delivery`]。
    warm_size: (u32, u32),
    /// 预算平面期内上一帧的画面，用来量"这一帧相比上一帧又变了多少"。
    /// 退出判据必须看当前的变化量，而不是被逼进来那一刻的量。
    plane_prev: Option<Frame>,
    /// 传输层报告过脱落（`qdrop` / `stale` / 限速桶 `shed`），正在等下一次观看端报告的裁决：
    /// 报告指出缺口 → 既有逐块修复路径接手；报告说"全齐" → 盲区，升级补账。见 [`Self::on_report`]。
    loss_claimed: bool,
    /// 脱落那批里**具体是哪些块**（发送任务丢的是哪个批次我们知道）。空 = 脱落信息没带
    /// 块 id（外部只记了案），那种情况才需要走整帧兑现。见 `note_link_loss_tiles`。
    loss_tiles: Vec<TileId>,
    /// 脱落要补账，这一帧要决定怎么补：
    /// 见 [`Self::note_link_loss`] 与差分段的取舍（#52）。
    loss_pending: bool,
    /// 上一次**排上补账重建**的时刻（用报告的 `now_ms`，与它同一把钟）。`None` = 还没排过，
    /// 会话第一次补账不该被冷却挡住。
    ///
    /// 必须按墙钟计，不能按"报告份数"计：观看端一发现链路空洞就会提前上报（早路），报告的
    /// 疏密因此不再是常数。实测踩过一次——冷却写成"攒满 3 份报告"后，早报把 3 份压缩到
    /// 0.45 s，反活锁被自己削掉，同一臂 `tile_bytes` 从 8.25 MB 涨到 16.33 MB、整帧 2→4。
    rebuild_armed_ms: Option<u64>,
    /// 脱落要补账，但一次整帧重建塞不进预算 ⇒ 交给媒体平面，并由**预算**那条路记账。
    loss_needs_plane: bool,
    /// 链路可用上行的**原始**估计 bps（0 = 还不知道）。运动判定要问的不是
    /// "变了吗"，而是"无损通路还装得下吗"——见 [`Self::motion_needs_plane`]。
    link_bps: u64,
    /// 送入帧率提示，用来把"每帧新变化的块数"折成每秒字节需求。
    feed_fps: u32,
    /// 流畅优先档（§13.1 画质·流畅度）：帧率是对用户的承诺，面积够大就走媒体
    /// 通道，不再问无损通路装不装得下。取舍（分辨率/清晰度）由调用方的档位定义，
    /// 这里只负责"不再用带宽判据拦着"。
    pub smooth_motion: bool,
    /// 每帧打一行平面判定轨迹（`ESS_DECIDE=1`）：路径选错时唯一能看的证据
    pub decide_log: bool,
}

impl<'c> SendPipeline<'c> {
    pub fn new(first: Frame, codecs: CodecSet<'c>) -> Self {
        let grid = TileGrid::for_frame(first.w, first.h);
        let mut ctl = Sender::new(model::Grid::new(grid.cols, grid.rows));
        // 布局代次必须跨进程重启保持单调：否则新会话的 layout_id 回到 1，
        // 仍连着的观看方会把它当成同一布局，只换尺寸而不重建状态。
        // 用毫秒时间戳做初值（u32 回绕周期 ~49 天，远大于一次会话）。
        ctl.layout_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u32)
            .unwrap_or(2)
            .max(2);
        Self {
            grid,
            ctl,
            state: SendState::new(first),
            codecs,
            scroll_enabled: true,
            lossy_trial_floor: 512,
            lossy_save_x: 2,
            lossy_save_x_edgy: 3,
            lossy_edges_min: 64,
            stats: SendStats::default(),
            seq: 0,
            frame_id: 0,
            initialized: false,
            motion_enabled: true,
            motion_max_w: 800,
            motion_max_h: 450,
            motion_quality: 55,
            motion_threshold: 0.25,
            motion_active: false,
            motion_hold: 0,
            hold_frames: 10,
            probe: Vec::new(),
            probe_dims: (0, 0),
            video_plane: false,
            video_mode: false,
            motion_seq: 0,
            repair_baseline: None,
            plane_motion_start: 0,
            budget_bps: 0,
            win_ts: 0,
            win_bytes: 0,
            win_tiles: 0,
            last_encoded: 0,
            budget_plane: false,
            encode_plane: false,
            encode_plane_min_ratio: 0.10,
            encode_pressure: false,
            media_pending: false,
            warm_started_us: 0,
            warm_ok_run: 0,
            warm_size: (0, 0),
            warm_max_us: 6_000_000,
            receiver_strain: false,
            plane_prev: None,
            loss_claimed: false,
            loss_tiles: Vec::new(),
            loss_pending: false,
            rebuild_armed_ms: None,
            loss_needs_plane: false,
            link_bps: 0,
            feed_fps: 0,
            smooth_motion: false,
            decide_log: false,
        }
    }

    fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    /// 新观看方加入：下一帧强制整帧，补齐它错过的历史状态。
    pub fn force_full(&mut self) {
        self.initialized = false;
    }

    /// 设定图块通路的每秒预算（bps，0 = 不限制）。调用方按真实链路统计来定。
    pub fn set_budget_bps(&mut self, bps: u64) {
        self.budget_bps = bps;
    }

    pub fn budget_bps(&self) -> u64 {
        self.budget_bps
    }

    /// 无损平面**编码速度**装不下帧预算时置 true（调用方按实测单帧耗时判定，带迟滞）。
    /// 置上之后与"链路装不下"走同一条路：把画面交给媒体通道，而不是把延迟堆成旧片子。
    ///
    /// 平面期内图块通路不编码，调用方手里的还是进入前那次的判定；继续认它会让
    /// 退出的迟滞每帧续期，平面再也回不到无损。所以平面一开就当作废，退出后重新测。
    pub fn set_encode_pressure(&mut self, on: bool) {
        self.encode_pressure = on && !self.video_mode;
    }

    /// 传输层真的把无损更新丢掉了（发送队列溢出 `qdrop`、新鲜度闸门 `stale`、限速桶 `shed`）。
    ///
    /// 为什么必须由管线来处理：版本向量与 `last_sent` 是在**产出消息**那一刻提交的，链路丢掉
    /// 一条并不会退回去——于是发送端与观看端的向量**可能一致**而像素不一致，观看端报告看不出
    /// 缺口（实测 `resends=0` 而画布整片旧着，#52）。取舍交给这里而不是调用方：整帧重建塞得进
    /// 预算就重建；塞不下就别再往窄管子里灌整帧，改由媒体平面接手，并记在**预算**那条账上
    /// （否则脱落会顶掉执行器自己的证明）。
    ///
    /// 这里只是**记案**，不立刻补账：版本向量在产出时提交，脱落通常仍会让报告出现缺口，而那正是
    /// 逐块修复路径的活。健康链路上一次偶发脱落会被下一次报告自己捞回来，提前重建整帧等于每条
    /// 链路都白花一个整帧（实测 `full=2`，#53）。裁决见 [`Self::on_report`]。
    pub fn note_link_loss(&mut self) {
        self.loss_claimed = true;
    }

    /// 同上，但**带上丢掉的是哪些块**（发送任务丢的总是某个 Update 批次，块 id 是已知的）。
    /// 有这份信息时，下一次报告的补账只补那些块，不兑现成整帧：实测 1080p 半屏单帧突变，
    /// 一次 `stale=1` 先正确逐块补了 121 块，随后又被一份"全齐"的报告兑现成一次
    /// 4.2 MB 整帧重建（同一份变化的总价 2.45→6.65 MB）。
    pub fn note_link_loss_tiles(&mut self, ids: &[TileId]) {
        self.loss_claimed = true;
        for id in ids {
            if !self.loss_tiles.contains(id) {
                self.loss_tiles.push(*id);
            }
        }
    }

    /// 设预热上限（µs，0 = 关闭预热）。
    pub fn set_warm_max_us(&mut self, us: u64) {
        self.warm_max_us = us;
    }

    /// 会话该不该按声明帧率喂编码器：平面开着要喂，**预热期也要喂**——预热的全部内容
    /// 就是把编码器从静止节拍（~2 fps）推到满帧率，不喂就没有东西可等。
    pub fn media_feeding(&self) -> bool {
        self.video_mode || self.media_pending
    }

    /// 还在预热（决定要交、尚未宣告）吗。
    pub fn media_pending(&self) -> bool {
        self.media_pending
    }

    /// 调用方每 1 s 报一次编码器实际上线的帧率与**它自己编码的尺寸**。达标即结束预热；
    /// 平面已经开着时这个调用什么都不做。
    ///
    /// 尺寸也要稳定才算达标：**"交付满帧率"不等于"稳定"**。码控一直按着同一档尺寸发得
    /// 动，才算安顿下来；尺寸还在变就说明后面还有一次关键帧在等着——实测它落在交接后
    /// 0.8 s，观看端看到的正是那一次 311–378 ms 的定格。
    pub fn note_media_delivery(&mut self, sent_fps: f64, want_fps: u64, enc_w: u32, enc_h: u32) {
        if !self.media_pending || want_fps == 0 {
            return;
        }
        let sized = enc_w != 0 && enc_h != 0;
        // 第一次观察到尺寸只是"记下"，不算它在变；之后尺寸一变就重新开始计数
        let changed = sized && self.warm_size.0 != 0 && self.warm_size != (enc_w, enc_h);
        let ok = sent_fps + 0.05 >= want_fps as f64 * 0.9 && !changed;
        self.warm_ok_run = if ok { self.warm_ok_run + 1 } else { 0 };
        if sized {
            self.warm_size = (enc_w, enc_h);
        }
    }

    /// 退出平面时用于收差集的基线：**交接这一刻**观看端手里的画面，也就是最后一条
    /// 真正发出去的无损更新（`last_sent`）。
    ///
    /// 为什么不能沿用"决定要交"那一刻的快照：**预热期画面仍由无损差分出**（那是预热存在
    /// 的前提，见 [`Self::plane_handover`]），那几帧的更新照常发出、照常 `commit`，
    /// 观看端画布因此往前走，快照却停在决策点上。退出时按过期基线收差集，凡是在预热期
    /// 变过、又变回基线样子的块都被判成"没变"，一个字节都不发——观看端永久停在中途
    /// 内容上；而两端版本账还是一致的（版本只在真正发出时推进），报告看不出缺口，
    /// 脱落补账那条路也轮不到它（实测 `missing=0 behind=0`、`REPAIR` 只剩退出那一次）。
    /// 周期性内容很容易撞上这种巧合：1280x720 合成屏上底部一块停在动画第 5 帧的内容上，
    /// 终帧 MAE 0.128、逐块只错这一块。
    ///
    /// `None` 保持 `None`：那是"基线不可信、退出补整帧"的既有语义（脱落补账、多人画布
    /// 不同源），不能被这里悄悄复活。
    fn refresh_exit_baseline(&mut self) {
        if self.repair_baseline.is_some() {
            self.repair_baseline = Some(self.state.last_sent.clone());
        }
    }

    /// 现在就把画面交给媒体平面吗？`false` = 还要预热，画面继续由无损图块出。
    ///
    /// 预热存在的理由：**交接这一帧要付一个关键帧**。1080p 平衡档实测那一次 pacing
    /// 吃掉 283–492 ms（观看端一个看得见的定格），而编码器从静止节拍爬到声明帧率还要
    /// 一两秒（观看端第 1→31 帧实测 1428–1487 ms，直尺 990 ms，即头一秒只有 ~20 帧）。
    /// 这两笔代价本身消不掉——码率就那么多，第一个关键帧就是这么大——但可以让它们
    /// 发生在观众看不见的时候：无损通路本来就在画这一帧，再顶它一两秒不改变任何像素，
    /// 而交接一旦发生，观众就该看到满帧率的画面。
    ///
    /// 两个不等预热的例外：
    ///   * 被链路**预算**逼进来的平面：那条管子连无损差分都塞不下，多顶一秒就多堵一秒；
    ///   * `warm_max_us = 0`：测量台的反向对照臂，用它证明洞确实是预热消掉的。
    ///
    /// 达标要**连续两个 1 s 窗**（交付 ≥0.9×声明帧率，且编码器自己选的尺寸没再变）。
    /// 上限只是兜底，而且必须**排在 WebRTC 自己的尺寸自适应之后**（它约 3 s 走一步，那一步
    /// 同样要付一个关键帧）：上限给 3.5 s 时交接正好卡在那一步之前，观看端在平面内第 2–3 根
    /// 帧吃到 242–280 ms 的洞；放到 6 s 后同样的臂 0–1 个洞、最大 280 ms（对照 2–4 个、
    /// 最大 467 ms）。等它走完再交，观众看到的第一个视频帧就已经站在最终尺寸和声明帧率上。
    fn plane_handover(&mut self, ts_us: u64) -> bool {
        if self.warm_max_us == 0 {
            self.media_pending = false;
            self.warm_ok_run = 0;
            return true;
        }
        if self.budget_plane {
            self.media_pending = false;
            return true;
        }
        if !self.media_pending {
            self.media_pending = true;
            self.warm_started_us = ts_us;
            self.warm_ok_run = 0;
            self.warm_size = (0, 0);
            return false;
        }
        if self.warm_ok_run >= 2 || ts_us.saturating_sub(self.warm_started_us) >= self.warm_max_us {
            self.media_pending = false;
            self.warm_ok_run = 0;
            return true;
        }
        false
    }

    /// 记一笔数据通道占用（按 `bytes_encoded` 的增量），返回最近 1 s 窗口内的字节数。
    /// 在编码**之前**调用，所以拿到的是上一帧的实际需求——用它当预测子，就不会出现
    /// "先把这帧编出来再决定不发"的浪费，也不会碰模型发送端的状态。
    /// 时间戳取自帧本身，离线测试可以确定性推进窗口。
    fn frame_egress(&mut self, ts: u64, new_tiles: u64) -> u64 {
        if ts.saturating_sub(self.win_ts) >= 1_000_000 {
            self.win_ts = ts;
            self.win_bytes = 0;
            self.win_tiles = 0;
        }
        let d = self.stats.bytes_encoded.saturating_sub(self.last_encoded);
        self.last_encoded = self.stats.bytes_encoded;
        self.win_bytes += d;
        self.win_tiles += new_tiles;
        self.win_bytes
    }

    /// 预算平面期内**不编**无损块，所以窗口里没有真实字节数可用。这里按"这一秒相比
    /// 上一帧新变化的块数 x 已观测的平均块大小"折算出无损通路要是接管得占多少，
    /// 并且继续推进同一个窗口。
    fn predict_egress(&mut self, ts: u64, new_tiles: u64) -> u64 {
        self.frame_egress(ts, new_tiles);
        let per_tile = self.stats.bytes_encoded / self.stats.tiles_sent.max(1);
        self.win_tiles * per_tile.max(1)
    }

    /// 预算还装得下这种量级的无损更新吗？要求一秒至少塞得下**两次**——卡在
    /// "刚好够"的边界上会让平面来回抖，每次退出都要补一次差分修复，那正是压垮
    /// 链路的东西。
    fn budget_room(&self, bytes_per_s: u64) -> bool {
        if self.budget_bps == 0 {
            return true;
        }
        bytes_per_s.saturating_mul(2) <= self.budget_bps / 8
    }

    /// 告诉管线链路容量与送入帧率：大面积运动是否值得换到媒体通道，由这两个数
    /// 决定（`bps = 0` 表示还不知道，此时退回面积判据，不能因为缺信息就不切）。
    pub fn set_link_bps(&mut self, bps: u64, feed_fps: u32) {
        self.link_bps = bps;
        self.feed_fps = feed_fps;
    }

    /// 这一帧的变化量摊到一秒上，无损通路要吃掉多少字节。平均块字节取自已观测的
    /// 编码结果——同一种内容的块大小是稳定的，所以这个估计与实测同量级
    /// （§14.4：1280x720 代码滚动 30 fps 无损实测 1,194 kbps）。
    fn motion_demand_bytes(&self, ratio: f32) -> u64 {
        let tiles = (self.grid.cols * self.grid.rows) as u64;
        let per_tile = (self.stats.bytes_encoded / self.stats.tiles_sent.max(1)).max(1);
        let changed = (ratio as f64 * tiles as f64) as u64;
        changed.saturating_mul(per_tile).saturating_mul(self.feed_fps.max(1) as u64)
    }

    /// **换平面的判据是"无损装不下"，不是"画面大面积变了"。** 同一素材两条通路都
    /// 量过（§14.4 结论 4）：无损图块平面 30 fps / SSIM 1.0 / 1.19 Mbps，媒体通道
    /// 稳态也能 30 fps，但自动档要把送入尺寸降到 960x540、SSIM 0.74。所以链路装得
    /// 下就让无损平面出画，装不下才换——除非用户选了流畅优先档（见 `smooth_motion`）。
    fn motion_needs_plane(&self, ratio: f32) -> bool {
        if self.smooth_motion {
            return true;
        }
        if self.link_bps == 0 {
            return true;
        }
        self.motion_demand_bytes(ratio) * 2 > self.link_bps / 8
    }

    /// 运动模式中：接收端状态是有损的，逐块重发没有意义（退出时会补整帧）。
    pub fn motion_active(&self) -> bool {
        self.motion_active
    }

    /// 当前由视频平面出画（媒体通道）：调用方应把本帧喂给视频轨。
    pub fn video_mode(&self) -> bool {
        self.video_mode
    }

    fn video_msg(&mut self, on: bool) -> WireMsg {
        WireMsg::VideoMode {
            seq: self.next_seq(),
            layout_id: self.ctl.layout_id,
            on,
        }
    }

    /// 视频平面期间不发 JPEG 运动帧：那条通路只服务没有视频轨的观看端，
    /// 而且它会把有损像素盖到观看端画布上——一旦盖上，退出时就必须用整帧
    /// 才能恢复精确，得不偿失。让位给媒体通道后画布保持"上一次无损画面"，
    /// 退出只需补真正的差集。
    fn motion_due(&self) -> bool {
        !self.video_mode
    }

    /// 每 8 像素采样一次，计算与上一帧的变化比例（也顺带更新探针）。
    fn motion_ratio(&mut self, cur: &Frame) -> f32 {
        let step = 8u32;
        let sw = (cur.w / step).max(1) as usize;
        let sh = (cur.h / step).max(1) as usize;
        if self.probe_dims != (sw, sh) || self.probe.len() != sw * sh {
            // 新布局/新尺寸：先把探针填成当前帧，不能直接判定为运动
            // （否则整帧后的第一帧会被误判，跳过无损路径）。
            self.probe = vec![0u32; sw * sh];
            self.probe_dims = (sw, sh);
            for y in 0..sh {
                let row = y * step as usize * cur.stride;
                for x in 0..sw {
                    let o = row + x * step as usize * 4;
                    self.probe[y * sw + x] =
                        u32::from_le_bytes([cur.px[o], cur.px[o + 1], cur.px[o + 2], 255]);
                }
            }
            return 0.0;
        }
        let mut diff = 0usize;
        for y in 0..sh {
            let row = y * step as usize * cur.stride;
            for x in 0..sw {
                let o = row + x * step as usize * 4;
                let v = u32::from_le_bytes([cur.px[o], cur.px[o + 1], cur.px[o + 2], 255]);
                let i = y * sw + x;
                if self.probe[i] != v {
                    diff += 1;
                }
                self.probe[i] = v;
            }
        }
        diff as f32 / (sw * sh) as f32
    }

    /// 运动帧必须塞进单条不可靠消息（≤ 60 KB）：丢了只丢一帧，不排队、
    /// 不分片、不队头阻塞。超限就逐档降分辨率/质量，仍超限则本帧丢弃。
    fn emit_motion(&mut self, cur: &Frame) -> Result<Option<WireMsg>, CodecError> {
        let (bw, bh) = (self.motion_max_w, self.motion_max_h);
        let q = self.motion_quality;
        let attempts = [
            (bw, bh, q),
            ((bw * 2 / 3).max(160), (bh * 2 / 3).max(90), q.saturating_sub(10)),
            ((bw / 2).max(160), (bh / 2).max(90), q.saturating_sub(20)),
        ];
        for (w, h, q) in attempts {
            let (ew, eh, data) = crate::motion::encode(cur, w, h, q).map_err(CodecError::Backend)?;
            if data.len() > 60_000 {
                continue;
            }
            self.frame_id += 1;
            self.stats.motion_frames += 1;
            self.stats.motion_bytes += data.len() as u64;
            self.stats.bytes_encoded += data.len() as u64;
            return Ok(Some(WireMsg::Motion {
                seq: self.next_seq(),
                layout_id: self.ctl.layout_id,
                frame_id: self.frame_id,
                w: ew,
                h: eh,
                data,
            }));
        }
        self.stats.motion_dropped += 1;
        Ok(None)
    }

    /// 送入一帧，返回要发送的线消息（宣告必须先于更新投递，见 §6.7）。
    pub fn on_frame(&mut self, cur: &Frame) -> Result<Vec<WireMsg>, CodecError> {
        self.stats.frames += 1;

        // 首次 / 尺寸变化 → 整帧（进入新布局）
        // 脱落补账的取舍（#52）。放在最前面，因为它决定的正是"这一帧走哪条通路"，而整帧重建
        // 走的是上面那条 `!initialized` 分支——排在后面就要白等一帧。
        if self.loss_pending {
            self.loss_pending = false;
            let per_tile = (self.stats.bytes_encoded / self.stats.tiles_sent.max(1)).max(1);
            let rebuild = (self.grid.cols * self.grid.rows) as u64 * per_tile;
            if self.video_mode {
                // 平面期内：把退出时那次"精确差分修复"降级成整帧重建——基线已经不可信，
                // 差分修复补不回被丢的那批块（`_ => emit_full` 那条既有分支就是这条路）。
                self.repair_baseline = None;
            } else if self.budget_room(rebuild) {
                self.initialized = false; // 这一帧就走整帧重建（可靠通路，不会被脱落丢弃）
            } else {
                self.loss_needs_plane = true;
            }
        }

        let size_change = cur.w != self.state.last_sent.w || cur.h != self.state.last_sent.h;
        // 注意：运动中也必须响应整帧请求（新观看方加入 / 报告 layout=0），
        // 否则观看方永远拿不到布局，运动帧会被全部丢弃。
        if size_change || !self.initialized {
            self.probe_dims = (0, 0);
            // 这里不清 motion_active/motion_hold：补整帧只是给迟到或丢帧的
            // 观看端重建状态，不该打断运动判定。若在运动中途直接把它清零，
            // 视频平面的退出宣告就再也没有机会发出（平面会卡在 on）。
            // 但运动中途发过整帧后，各观看端画布不再同一基线，只能退回整帧退出。
            if self.motion_active {
                self.repair_baseline = None;
            }
            let msg = self.emit_full(cur)?;
            self.state.commit(cur);
            self.initialized = true;
            let mut out = vec![msg];
            // 仍在视频平面：补整帧的同时要告诉新观看端"画面由媒体通道出"，
            // 否则它会停在刚收到的静态画布上。
            if self.video_mode {
                out.push(self.video_msg(true));
            }
            return Ok(out);
        }

        // 滚动路径。运动/视频平面期间不做滚动检测：那是整帧比对（1080p 下
        // ~10 ms/帧），而画面反正由整帧通路出，比对结果用不上。
        let det = if self.scroll_enabled && !self.motion_active {
            let t_det = std::time::Instant::now();
            let d = detect(&self.state.last_sent, cur)
                .map(|d| d.dy)
                .filter(|dy| *dy != 0);
            self.stats.detect_us += t_det.elapsed().as_micros() as u64;
            d
        } else {
            None
        };

        // 大面积运动 → 切视频平面（媒体通道），JPEG 只作兜底（§7.6 运动路径）。
        // 但只有"无损装不下这条链路"时才真的切：面积大而无损又够，切平面是亏的。
        let ratio = if self.motion_enabled { self.motion_ratio(cur) } else { 0.0 };
        if self.decide_log {
            // 每帧一行判定轨迹：平面选错的时候，要能看到当时看见了什么
            eprintln!(
                "DECIDE ts={} detect={:?} ratio={:.3} need_plane={} demand={:.2}Mbps link={:.2}Mbps plane={} pending={}",
                cur.ts_us / 1000,
                det,
                ratio,
                self.motion_needs_plane(ratio),
                self.motion_demand_bytes(ratio) as f64 * 8.0 / 1e6,
                self.link_bps as f64 / 1e6,
                self.video_mode,
                self.media_pending
            );
        }
        if let Some(dy) = det {
            return self.emit_scroll(cur, dy);
        }
        let want_plane =
            self.motion_enabled && ratio >= self.motion_threshold && self.motion_needs_plane(ratio);
        if want_plane {
            self.motion_hold = self.hold_frames;
            self.motion_active = true;
            self.motion_seq += 1;
            if self.video_plane && self.repair_baseline.is_none() {
                // 快照观看端画布真正持有的画面（本窗口只拍一次）
                self.repair_baseline = Some(self.state.last_sent.clone());
            }
            let mut out = Vec::with_capacity(2);
            if self.video_plane && !self.video_mode && self.plane_handover(cur.ts_us) {
                self.video_mode = true;
                out.push(self.video_msg(true));
                self.stats.video_on += 1;
                self.refresh_exit_baseline();
            }
            // 预热中（决定要交、但还没交）：这一帧继续走下面的普通无损差分，不发
            // Motion——那是给有损平面兜底的 JPEG，而此刻画布还是无损的。
            if !(self.media_pending && !self.video_mode) {
                if self.motion_due() {
                    if let Ok(Some(m)) = self.emit_motion(cur) {
                        out.push(m);
                    }
                }
                self.state.commit(cur);
                return Ok(out);
            }
        }
        // 预热的取消与平面的退出**共用同一条迟滞**：一两帧没到阈值不算"内容静止"。
        // 块状跳变动物的 ratio 本来就在阈值上下抖，上一版在这里按 `!want_plane` 立刻静默
        // 取消，于是切换沿验收臂（C 臂）的预热反复从头计时、上限永远到不了，18 秒里平面
        // 一次也没交出去（实测 switches=0 / vmode=false）。
        if self.motion_active {
            // 被预算逼进来的平面：退出与否按**当前**的变化量判断。旧写法拿"被逼进来
            // 那一次的每秒需求"当尺子，那个数在平面期内再也不更新，于是画面早就静止了
            // 仍然赖在有损平面上——观看端看到的就是糊掉的小字，而主流远控是"停下来就
            // 变清晰"。仍然要保持不回退的理由是"每次都退出要补一次差分修复，而那正是
            // 链路装不下的量"，所以判据换成：当前这一秒的新变化还装不进预算就不退。
            if self.budget_plane {
                let fresh = match &self.plane_prev {
                    Some(prev) => self.state.diff_against(prev, cur, self.grid).changed.len() as u64,
                    None => 0,
                };
                let demand_now = self.predict_egress(cur.ts_us, fresh);
                self.plane_prev = Some(cur.clone());
                // 退出判据**不**看 strain：能力信号只决定"要不要交出去"，决定"什么时候
                // 收回来"的必须是画面本身是否静止——否则小字再也回不到无损（实测
                // forced-plane 里平面再也退不出来，终帧逐字节不一致）。
                //
                // 但**续期**必须看 strain：平面可以由 by_strain 单独举起（浏览器解码跟不上），
                // 而续期只认 budget/encode_pressure 时，它会在 10 帧迟滞后用"新变化装得下"为理由
                // 关掉，关完不到 1 s strain 再次达标又立刻举起——实测 720p+`ESS_DEMO_VIDEO`
                // 配浏览器观看端 12 s 内进出 6 次（每次退出只撑一拍，无损画面被 960x540 反复顶掉），
                // 而**同一内容配原生观看端 switches=0、全程 plane=false**：说明点平面的就是 strain，
                // 抖动的来源就是"举起它的信号不参与维持它"。
                // strain 自限：画面静止 → 无更新 → 两个 500 ms 报告内衰减 → 照样退回无损，
                // 由 `settled_picture_returns_to_lossless_even_while_budget_stays_tight` 判真伪。
                //
                // 算力平面：平面期内测不到无损编码耗时，续期只能用当前的差异占比代理——
                // 还在大改就继续由媒体通道出画，回到小改动/静止就退回无损、交给帧率控制器。
                let tiles = self.grid.tile_count().max(1) as f32;
                let fresh_ratio = fresh as f32 / tiles;
                // 续期只看**当前这一秒的新变化**还值不值得留在有损平面。
                //
                // 旧写法在这里无条件续期 `receiver_strain`。它是"观看端此刻啃不啃得动"
                // 的实时信号，但**平面期内根本采样不到**：会话在运动/平面期间直接丢弃
                // 观看端报告（`ess-session/src/lib.rs:1472`，`motion_active()` 且
                // `layout_id != 0` 就 continue，避免逐块补发去抢媒体通道的带宽），于是
                // 它冻结在"进入平面之前"那次报告的值上（true）。无条件续期的结果：
                // 静止画面也永远退不回图块。真机复现（2026-11-05，720p + 慢观看端，
                // `scripts` 同款方式）：4.2 s 由 strain 逼进平面、13 s 画面静止，旧写法
                // 到 28 s 仍停在平面（末端 `strain=true`），终帧与期望帧不一致。
                //
                // 同一条 `||` 里的 `encode_pressure` 在平面期内恒为 false——会话每个心跳
                // 都调 `set_encode_pressure`，而它的写法是 `on && !video_mode`（见上面
                // 的同名方法），进平面就被清零。所以移除它只是清理死条件，不改变行为：
                // 编码逼进来的平面靠**进入归因** `encode_plane` 续期，那一项与它配套的
                // `fresh_ratio` 门槛旧写法就有。
                //
                // 现在：预算仍然装不下 → 留；编码/落后逼进来的平面 → 只有当前变化量还大
                // 才留。画面一静止，`fresh_ratio` 掉到门槛以下，迟滞走完就退回无损
                // （实测：同一场景退出发生在静止后 0.37 s，终帧与期望帧逐字节一致）。
                if !self.budget_room(demand_now)
                    || ((self.encode_plane || self.receiver_strain)
                        && fresh_ratio >= self.encode_plane_min_ratio)
                {
                    self.motion_hold = self.hold_frames;
                }
            }
            // （试过在这里按 ratio 续期迟滞，想让"还在动就别退"，实测 switches 与
            // 喂入速率都没动（4 次 / 3.5 fps），说明瓶颈不在退出条件，已回退。）
            if self.motion_hold > 0 {
                self.motion_hold -= 1;
                if self.media_pending {
                    // 预热期的迟滞只用来"数到 0 就取消"。画面继续走下面的无损差分：
                    // 不发 JPEG 运动帧（那是有损平面的兜底），也不在这里 commit。
                } else {
                    self.motion_seq += 1;
                    // 迟滞期内的轻微变化继续算作运动帧；基本静止则什么都不发
                    let mut out = Vec::new();
                    if ratio >= self.motion_threshold * 0.4 {
                        if self.motion_due() {
                            if let Ok(Some(m)) = self.emit_motion(cur) {
                                out.push(m);
                            }
                        }
                        self.state.commit(cur);
                    }
                    return Ok(out);
                }
            }
            if self.motion_hold > 0 {
                // 只剩预热那种情况会走到这儿：迟滞还没数完，既不退出也不取消，
                // 这一帧继续走下面的无损差分。
            } else {
            self.motion_active = false;
            self.motion_seq = 0;
            self.budget_plane = false;
            self.encode_plane = false;
            self.plane_prev = None;
            if self.media_pending {
                // 还没交出去内容就静止了：线上从未宣告过任何事，画布一直是精确无损的，
                // 所以静默取消即可——不发 off、不补整帧（那 2.7 MB 是白花的）。
                // 这一帧按下面的普通差分正常走。
                self.media_pending = false;
                self.warm_ok_run = 0;
                self.repair_baseline = None;
            } else {
                // 运动结束：**先**发无损修复，**后**宣告关平面。
                // 可靠通道是有序的，所以观看端收到 off 时修复像素必然已经落上画布；
                // 反过来的话（旧行为）off 先到，画布还是运动前的旧画面，就会看到
                // 画面"回溯一下"再刷回来——产品级验收不接受这种跳变。
                let mut out = Vec::with_capacity(2);
                let leaving_video = self.video_plane && self.video_mode;
                let baseline = self.repair_baseline.take();
                match baseline {
                    // 视频平面退出、且这段期间**一帧 JPEG 都没编出来**（媒体通道自己出画，
                    // 观看端画布没被有损像素碰过）：基线仍然等于画布，只补真正的差集
                    // （可以为空 = 画面已经回到运动前的样子，一个像素都不用发）。
                    Some(base)
                        if leaving_video
                            && self.stats.motion_frames == self.plane_motion_start =>
                    {
                        out.extend(self.emit_repair(&base, cur)?);
                    }
                    // JPEG 运动通路盖过画布，或根本不在媒体平面：基线不可信，
                    // 必须整帧重建精确状态（#56）。
                    _ => {
                        out.push(self.emit_full(cur)?);
                    }
                }
                self.state.commit(cur);
                self.initialized = true;
                // 修复批次排在 off 之前发出（可靠通道有序）：观看端收到 off 时画布
                // 必然已经是正确的画面，交接不会出现"退回几秒前"的一瞬。
                if leaving_video {
                    self.video_mode = false;
                    out.push(self.video_msg(false));
                    self.stats.video_off += 1;
                }
                return Ok(out);
            }
            }
        }

        // 普通差分
        let t_diff = std::time::Instant::now();
        let diff = self.state.diff(cur, self.grid);
        self.stats.diff_us += t_diff.elapsed().as_micros() as u64;
        // 需求量用**折算**（窗口内新变化的块数 × 已观测平均块大小）而不是窗口的实测
        // 字节。两者平时同量级，但实测那条有一个自指洞：无损通路一旦把画面交给媒体
        // 平面就不再编码，窗口滚过一秒后读数归零，于是"越是被预算压住，越看不见预算
        // 不够"。平面期的迟滞早已因此改用 `predict_egress`（见上面 `budget_plane`
        // 那段），force 点用同一把尺子才对得上。
        // 注：真机 forced-plane 的 `forced_plane=0` **不是**这里造成的（换成折算后
        // 第一次复跑仍是 0），根因是 strain 在没有基线时就把平面翻走了 —— 见
        // `Sender::ever_current`。
        let demand = self.predict_egress(cur.ts_us, diff.changed.len() as u64);
        // 链路预算装不下无损更新 → 把画面交给媒体通道（有损但保帧率），而不是继续
        // 往太小的管子里塞无损块把自己堵死。这里**不** commit：这些块观看端没收到，
        // 回到无损时由退出平面的精确修复补上。
        // 同一条路也走"单帧编码耗时装不下帧预算"：那种负载下降帧率只是把延迟摊平，
        // 观看端看到的仍是按序播放的旧画面；交给 H264（1080p 一帧 5–10 ms）才追得平。
        let room = self.budget_room(demand);
        // 这一帧到底变了多少（占比）。预算/算力两条路径都要过这道门槛，见下面的
        // `plane_worthy`；判定日志里也带上，免得下次还得从 `changed` 反推阈值。
        let changed_ratio = diff.changed.len() as f32 / self.grid.tile_count().max(1) as f32;
        let plane_worthy = changed_ratio >= self.encode_plane_min_ratio;
        if self.decide_log {
            // 归因出问题时唯一能看的证据：这一帧的无损需求量、预算、以及三条判据此刻
            // 各自成不成立（`forced_plane` 只有帧末累计值，看不出是谁该记账）。
            eprintln!(
                "BUDGET ts={} changed={} ratio={:.3} worthy={} demand={}KB/s budget={}KB/s room={} enc_press={} strain={} plane={}",
                cur.ts_us / 1000,
                diff.changed.len(),
                changed_ratio,
                plane_worthy,
                demand / 1024,
                self.budget_bps / 8 / 1024,
                room,
                self.encode_pressure,
                self.receiver_strain,
                self.video_mode
            );
        }
        // 三种"无损通路交不出这一帧"的成因，各自都要把画面交给媒体平面，但**归因必须分开**
        // （STATS 里三个计数器就是三条契约的证明）：
        //   * `!room`：链路预算装不下无损块（§6.6 执行器的窄管子场景）；
        //   * `encode_pressure`：单帧无损编码耗时装不下一帧预算（降帧率只是把延迟摊平，
        //     观看端看到的仍是按序播放的旧片子；H264 一帧 5–10 ms 才追得平）；
        //   * `receiver_strain`：观看端啃不动图块（连续两次报告、≥25% 块落后 ≥2 版、
        //     网格 ≥16 块；修复差集被预算截断时不计数，见 `Sender::receiver_strain`）。
        // strain 必须**同时**要求 `room && !encode_pressure`：观看端落后有两种成因——它
        // 自己慢，或**我们自己把图块压住了**。前者才该由 strain 负责。上一版让 strain
        // 直接进或条件，它作为跨帧存活的报告量在一帧"差分很小、装得下"的时刻抢先把平面翻开，
        // 于是 forced-plane 那条"执行器真的压过图块"的契约当帧没机会计数
        // （实测 strain_forced=6 / forced_plane=0）。谁该被记账由**此刻**的判据决定，
        // 不由谁先说话决定。
        let loss_forces = self.loss_needs_plane;
        self.loss_needs_plane = false;
        // **预算路径同样要看变化量**（`plane_worthy` 是上面按这一帧算好的占比）。
        // 手机链路上拥塞时预算被压到 200 kbps 兜底，而 1080p 桌面上哪怕只有光标/
        // 时钟/闪烁光标在动（每秒几十块）折算下来也会超过它；旧写法 `!room` 一成立
        // 就切有损平面，于是"只动鼠标"甚至"画面看着没动"都会切过去，退出补帧又把
        // 队列顶出一次拥塞，来回抖。这种绝对量很小的更新错过一两条本就会由 500 ms
        // 状态报告补回，不该拿清晰度去换。算力路径同理：只动一两块（光标、一行字）
        // 就切到有损平面，付出的清晰度代价远大于省下的编码时间——这种量级交给
        // 帧率/尺寸控制器消化。
        // `loss_forces` 是正确性缺口（补账装不下），不受这条门槛约束。
        let by_budget = (!room && plane_worthy) || loss_forces;
        let by_encode = room && self.encode_pressure && plane_worthy;
        let by_strain = room && !self.encode_pressure && self.receiver_strain;
        if self.video_plane
            && (!diff.changed.is_empty() || loss_forces)
            && (by_budget || by_encode || by_strain)
        {
            self.budget_plane = true;
            self.encode_plane = by_encode;
            // 预算/编码逼出来的交接不等预热：无损通路自己都塞不下了，没什么可等的。
            // strain 逼出来的也不等——等它爬坡的那几秒正是它啃不动的画面。
            self.media_pending = false;
            self.plane_prev = Some(cur.clone());
            self.motion_active = true;
            self.motion_hold = self.hold_frames;
            // 基线一律记住"进入这一刻，发送端认为观看端持有什么画面"。要不要退化成整帧
            // 重建，改由退出时"这段期间有没有有损 JPEG 真的盖过画布"决定（见
            // `plane_motion_start` 与下面的 `match baseline`），而不是由被哪条理由逼进来决定。
            //
            // 原来这里对 `by_budget` 直接 `repair_baseline = None`，理由是"那些块正是无损
            // 通路自己发不动的，定点差集会漏掉它们（#56：motion 门禁里停在旧画布）"。那个
            // 结论在 **JPEG 盖过画布** 的场合成立，但预算逼进来而画面全程由 H264 出画时，
            // 观看端画布一动不动，`last_sent` 仍然等于画布，定点差集是完备的。补整帧的代价
            // 实测很贵：1080p 照片类内容是 510 块 / ~3.5 MB，观看端要逐块盖好几秒——那正是
            // 用户报的"很多个图块慢慢覆盖"（真机产品路径：`switches=4`、观看端 `fulls` 2→4，
            // 而真正变化的 255 块从未以定点修复落地）。
            if self.repair_baseline.is_none() {
                self.repair_baseline = Some(self.state.last_sent.clone());
            }
            self.plane_motion_start = self.stats.motion_frames;
            if by_budget {
                self.stats.budget_forced += 1;
            } else if by_encode {
                self.stats.encode_forced += 1;
            } else {
                self.stats.strain_forced += 1;
            }
            let mut out = Vec::new();
            if !self.video_mode {
                self.video_mode = true;
                self.stats.video_on += 1;
                out.push(self.video_msg(true));
                // 预热中途被预算/算力/落后逼成交接时，上面那份基线是决策点的、已经过期。
                self.refresh_exit_baseline();
            }
            return Ok(out);
        }
        if diff.changed.is_empty() {
            return Ok(Vec::new());
        }
        let mut updates = Vec::with_capacity(diff.changed.len());
        for &(id, _) in &diff.changed {
            let g = self.grid.geom(id, cur.w, cur.h);
            let raw = extract_tile(cur, g);
            updates.push((id, tile_hash(&raw)));
        }
        let msg = self.ctl.edit_many(&updates);
        let wire = self.wire_update(msg, cur, false)?;
        self.state.commit(cur);
        Ok(vec![wire])
    }

    fn emit_full(&mut self, cur: &Frame) -> Result<WireMsg, CodecError> {
        let grid = TileGrid::for_frame(cur.w, cur.h);
        if grid != self.grid {
            self.grid = grid;
            self.ctl.change_layout(model::Grid::new(grid.cols, grid.rows));
        }
        let n = grid.tile_count();
        // 整帧也是"逐块编码+打包"，与差分更新记在同一账上（`encode_us`）：归因要看的是
        // 无损通路花在编码上的总时间，不该按消息种类分家。
        let t_pack = std::time::Instant::now();
        // 整帧只有一个码流字段：选一个能编码全部块的编码器（整帧不常见，
        // 逐块回退不值得为它把整帧拆成多条消息）。
        // 逐块按价选码（同一个 `encode_by_cost`，与差分批、报告重发走完全一样的规则）。
        // 老版本这里"挑一个能编完所有块的编码器"，照片类桌面于是整帧只能走 PNG：实测
        // 1080p 首帧 3.72 MB，逐块 1.67 MB（2.22×，459/510 块 JPEG）。
        let mut payloads = Vec::with_capacity(n);
        let mut codecs = Vec::with_capacity(n);
        for i in 0..n {
            let id = TileId(i as u16);
            let g = grid.geom(id, cur.w, cur.h);
            let raw = extract_tile(cur, g);
            let (cid, p) = self.codecs.encode_by_cost(
                &raw,
                g.w,
                g.h,
                self.lossy_trial_floor,
                self.lossy_save_x,
                self.lossy_save_x_edgy,
                self.lossy_edges_min,
            )?;
            if matches!(cid, TileCodecId::Jpeg) {
                self.stats.tiles_lossy += 1;
            }
            payloads.push(p);
            codecs.push(cid);
        }
        let full_codec = codecs.first().copied().unwrap_or(TileCodecId::Png);
        self.stats.encode_us += t_pack.elapsed().as_micros() as u64;
        self.stats.bytes_encoded += payloads.iter().map(|p| p.len() as u64).sum::<u64>();
        self.frame_id += 1;
        self.stats.full_frames += 1;
        self.stats.tiles_sent += n as u64;
        Ok(WireMsg::Full {
            seq: self.next_seq(),
            layout_id: self.ctl.layout_id,
            frame_id: self.frame_id,
            w: cur.w,
            h: cur.h,
            codec: full_codec,
            codecs,
            versions: self.ctl.version.clone(),
            epochs: self.ctl.epoch.clone(),
            payloads,
        })
    }

    /// 运动退出的精确修复：只补「观看端画布（基线）↔ 当前画面」真正不同的块，
    /// 标记 repair 走可靠通道。差集为空时返回空 Vec —— 画面自己走回了原样。
    fn emit_repair(&mut self, base: &Frame, cur: &Frame) -> Result<Vec<WireMsg>, CodecError> {
        let diff = self.state.diff_against(base, cur, self.grid);
        if diff.changed.is_empty() {
            self.stats.repairs += 1;
            return Ok(Vec::new());
        }
        let mut updates = Vec::with_capacity(diff.changed.len());
        for &(id, _) in &diff.changed {
            let g = self.grid.geom(id, cur.w, cur.h);
            updates.push((id, tile_hash(&extract_tile(cur, g))));
        }
        let msg = self.ctl.edit_many(&updates);
        let wire = self.wire_update(msg, cur, true)?;
        self.stats.repairs += 1;
        self.stats.repair_tiles += updates.len() as u64;
        Ok(vec![wire])
    }

    fn emit_scroll(&mut self, cur: &Frame, dy: i32) -> Result<Vec<WireMsg>, CodecError> {
        let baseline = self.state.last_sent.vscroll(dy);
        let diff = self.state.diff_against(&baseline, cur, self.grid);
        let affected: Vec<TileId> =
            (0..self.grid.tile_count() as u16).map(TileId).collect();
        let mut dirty: Vec<(TileId, u64)> = Vec::with_capacity(diff.changed.len());
        for &(id, _) in &diff.changed {
            let g = self.grid.geom(id, cur.w, cur.h);
            dirty.push((id, tile_hash(&extract_tile(cur, g))));
        }
        let (announce, update) = self.ctl.scroll(&affected, &dirty);
        let Message::ScrollAnnounce { layout_id, scroll_id, targets, dirty: dirty_ids, .. } = announce
        else {
            unreachable!()
        };
        let scroll = WireMsg::Scroll {
            seq: self.next_seq(),
            layout_id,
            scroll_id,
            dy,
            targets,
            dirty: dirty_ids,
        };
        let wire = self.wire_update(update, cur, false)?;
        self.stats.scrolls += 1;
        self.state.commit(cur);
        Ok(vec![scroll, wire])
    }

    /// 接收端周期报告 → 差异重发（§6.4 兜底）。像素取 `last_sent`（接收端
    /// 应当拥有的画面），重发带当前版本与 epoch。
    pub fn on_report(
        &mut self,
        layout_id: u32,
        versions: &[u32],
        epochs: &[u32],
        now_ms: u64,
    ) -> Result<Vec<WireMsg>, CodecError> {
        self.ctl.set_now(now_ms);
        let mut tiles = self.ctl.on_report(layout_id, versions, epochs, now_ms);
        // 报告本身就是观看端能力的证据：它落后多少，决定要不要继续把画面交给图块通路。
        // 只在**无损通路正在承载画面**时取样：平面期间本来就不发图块，观看端的版本
        // 当然停在原地——那是我们不让它跟，不是它跟不上。混在一起判定会让平面永远
        // 退不出来（实测 forced-plane 里画面静止后仍留在有损平面，终帧逐字节不一致）。
        self.receiver_strain = !self.video_mode && self.ctl.receiver_strain();
        // 脱落的裁决（#53）。脱落发生在**产出之后**，报告是唯一能说清"观看端缺了哪些块"的证据：
        //  - 承认缺口 ⇒ 逐块修复路径的活，让它接手（健康链路上的偶发脱落正是这一类，提前重建
        //    整帧等于每条链路都白花一个整帧，实测 `full=2`）。
        //  - 一点缺口都不认 ⇒ 提交即记账把缺口掩盖了，逐块修复永远不会要这些块 ⇒ 补账。
        // 用 `last_gap`（观看端承认的缺口，预算截断之前）而不是返回的块数：预算扣光时返回为空
        // 而缺口仍在，那是"还没送到"，不是"看不见"。旧布局的报告由布局消息负责，不在这里升级。
        if self.loss_claimed && layout_id == self.ctl.layout_id {
            self.loss_claimed = false;
            if self.loss_tiles.is_empty() {
                // 脱落没带块 id（调用方只记了案）：报告说"全齐"而确实掉过东西，那就是产出时
                // 提交版本把缺口掩盖了，逐块修复永远不会要这些块 ⇒ 补一次整帧。
                //
                // 补账排上的整帧走 baseline + 可靠通路：新鲜度闸门不丢它、SCTP 会重传它。所以排上
                // 之后必须给它一次整帧的往返时间，否则每份"看不出缺口"的报告就要一次整帧重建——
                // 实测（1080p 照片类半屏单帧突变）一张静止画面为了一次 2.3 MB 的变化累计发了
                // **10 个整帧 = 36 MB**（加上这条冷却后是 2 个 / 8.25 MB），队列被顶到 3–5、
                // 后续 108 个批次被判过时丢掉。锁死测试
                // `link_loss_rebuilds_the_baseline_when_the_budget_can_carry_it` 钉的是"要补这一次
                // 账"，没有钉"每半秒补一次"。
                //
                // 冷却按**报告时钟的墙钟**算（1500 ms ≈ 三个默认报告周期），不按报告份数算：
                // 观看端一发现链路空洞就会提前上报（`view.rs` 与 `app.js` 里那条早路），"份数"
                // 这把尺子会被早报压短——实测那样反而把重建从 2 次顶回 4 次、字节 8.25 → 16.33 MB。
                let cooled = match self.rebuild_armed_ms {
                    None => true,
                    Some(t) => now_ms.saturating_sub(t) >= 1500,
                };
                if self.ctl.last_gap == 0 && cooled {
                    self.loss_pending = true;
                    self.rebuild_armed_ms = Some(now_ms);
                }
            } else {
                // 知道掉了哪些块：报告已经要回去的那些正在逐块修复的路上，剩下的才是真欠的账
                // ——按块补（受 §6.6 修复预算约束），不兑现成整帧。实测 1080p 半屏单帧突变：
                // 一次 `stale=1` 先正确补了 121 块，随后又被一份"全齐"的报告兑现成 4.2 MB 整帧，
                // 同一份变化的总价从 2.45 MB 涨到 6.65 MB。
                let asked: Vec<TileId> = self.ctl.last_gap_ids.clone();
                let owed: Vec<TileId> = std::mem::take(&mut self.loss_tiles)
                    .into_iter()
                    .filter(|id| !asked.contains(id))
                    .collect();
                if !owed.is_empty() {
                    let back = self.ctl.resend_tiles(&owed, now_ms);
                    let sent: Vec<TileId> = back.iter().map(|t| t.id).collect();
                    // 被预算扣下的那部分不许悄悄销案：留到下一次报告，否则"我们知道掉了谁、
                    // 却再也没补"正是 #52 那类静默画面错误。
                    let still: Vec<TileId> = owed.into_iter().filter(|id| !sent.contains(id)).collect();
                    if !still.is_empty() {
                        self.loss_claimed = true;
                        self.loss_tiles = still;
                    }
                    tiles.extend(back);
                }
            }
        }
        if tiles.is_empty() {
            return Ok(Vec::new());
        }
        let layout_id = self.ctl.layout_id;
        let frame = self.state.last_sent.clone();
        // 这里仍然走 lossy，不是疏忽：把"承认的缺口"改挂可靠通路实测**更差**（照片素材同一臂
        // 单轮：`report_resends` 2409→2707、`tile_bytes` 11.55→12.81 MB、闸门丢掉 26→31）。
        // 可靠是有序且队头阻塞的——一份修复压在前面，其后几帧的新内容反而被判过时丢掉，
        // 于是又要更多修复。要治的是那批字节本身，不是这条通道的顺序。
        //
        // 但**不能再让整份差集共享一个 seq**：那样丢一片就整份作废（见
        // `REPORT_CHUNK_TILES` 的注释）。逐块切条后每条独立原子落地，丢片只损失那一小条。
        let mut out = Vec::with_capacity(tiles.len().div_ceil(REPORT_CHUNK_TILES));
        for chunk in tiles.chunks(REPORT_CHUNK_TILES) {
            let msg = Message::Update {
                layout_id,
                cols: self.grid.cols,
                rows: self.grid.rows,
                tiles: chunk.to_vec(),
            };
            out.push(self.wire_update(msg, &frame, false)?);
        }
        Ok(out)
    }

    fn wire_update(&mut self, msg: Message, cur: &Frame, repair: bool) -> Result<WireMsg, CodecError> {
        let Message::Update { layout_id, cols, rows, tiles } = msg else {
            return Err(CodecError::Backend("expected update".into()));
        };
        let t_pack = std::time::Instant::now();
        let mut wt = Vec::with_capacity(tiles.len());
        for t in tiles {
            let g = self.grid.geom(t.id, cur.w, cur.h);
            let raw = extract_tile(cur, g);
            let (codec_id, payload) = self.codecs.encode_by_cost(
                &raw,
                g.w,
                g.h,
                self.lossy_trial_floor,
                self.lossy_save_x,
                self.lossy_save_x_edgy,
                self.lossy_edges_min,
            )?;
            if matches!(codec_id, TileCodecId::Jpeg) {
                self.stats.tiles_lossy += 1;
            }
            self.stats.bytes_encoded += payload.len() as u64;
            wt.push(WireTile {
                id: t.id,
                codec: codec_id,
                x: g.x,
                y: g.y,
                w: g.w,
                h: g.h,
                epoch: t.epoch,
                version: t.version,
                payload,
            });
        }
        self.stats.encode_us += t_pack.elapsed().as_micros() as u64;
        self.stats.updates += 1;
        self.stats.tiles_sent += wt.len() as u64;
        Ok(WireMsg::Update {
            seq: self.next_seq(),
            layout_id,
            cols,
            rows,
            repair,
            part: 0,
            total: 0,
            tiles: wt,
        })
    }
}

/// 一次拆片更新在接收端的暂存：所有片到齐才一次性应用。
struct UpdateGroup {
    seq: u32,
    layout_id: u32,
    cols: u16,
    rows: u16,
    total: u32,
    /// part 序号 → 该片解码后的 (线格式块, BGRA) 列表
    parts: HashMap<u32, Vec<(WireTile, Vec<u8>)>>,
}

pub struct RecvPipeline<'c> {
    pub canvas: Frame,
    pub grid: TileGrid,
    pub rx: Receiver,
    pub codecs: CodecSet<'c>,
    pub stats: RecvStats,
    /// 未完成滚动组：(scroll_id) → (targets, dirty)
    pending: HashMap<u32, (Vec<(TileId, u32)>, Vec<TileId>)>,
    /// 组内等待完成的像素：(scroll_id, tile) → (线上几何, 解码后的紧排 BGRA)
    grouped_raw: HashMap<(u32, TileId), (TileGeom, Vec<u8>)>,
    /// 尚未凑齐的拆片更新。一次逐块更新被传输层拆成 N 条消息（共享 seq），
    /// 所有片到齐前**不落画布**：否则画布会在两次上屏之间出现半帧新、半帧旧
    /// 的拼接（撕裂）。新 seq 一到就丢弃旧的残缺组（缺块由状态报告补回）。
    pending_update: Option<UpdateGroup>,
    /// 最近一次见过的更新 seq（用于丢弃迟到的旧拆片）
    last_update_seq: u32,
    layout_id: u32,
    /// 已由整帧建立（像素完整）的布局代次。新布局只有收到整帧才算建立；
    /// 之前的更新可能已把网格推进，但画布像素不完整，报告必须请求整帧。
    full_layout: u32,
    /// 本端能显示视频轨（媒体通道）。为 true 时，视频模式期间的 JPEG 兜底帧
    /// 不再贴画布——否则低清帧会盖在视频上面，退出时又闪一下。
    pub accept_video_plane: bool,
    video_mode: bool,
    /// 自渲染端取走以来，画布被写过的最小矩形（像素坐标）。
    ///
    /// 观看端的上传代价按**变化面积**算，不是按帧大小算：1080p 下一次单块更新
    /// 不该付整幅逐像素 BGRA→RGBA 交换 + 整幅 putImageData 的价钱。那个价钱实测
    /// 会把解码占用顶过闸门（44 % → shed 开），大面积更新被观看端自己丢掉，
    /// 表现就是"很多个图块慢慢覆盖"。
    dirty: Option<TileGeom>,
}

impl<'c> RecvPipeline<'c> {
    pub fn new(codecs: CodecSet<'c>) -> Self {
        // 布局代次从 0 起步：首个整帧（layout_id=1）必须触发网格重建，
        // 否则版本数组为空，后续更新会被判为越界丢弃。
        let mut rx = Receiver::new(model::Grid::new(0, 0));
        rx.layout_id = 0;
        Self {
            canvas: Frame::new(0, 0, 0),
            grid: TileGrid::for_frame(0, 0),
            rx,
            codecs,
            stats: RecvStats::default(),
            pending: HashMap::new(),
            grouped_raw: HashMap::new(),
            pending_update: None,
            last_update_seq: 0,
            layout_id: 0,
            full_layout: 0,
            accept_video_plane: false,
            video_mode: false,
            dirty: None,
        }
    }

    /// 接收端周期报告。若当前布局尚未收到整帧（例如整帧在其数据通道就绪前
    /// 发出而丢失），上报 layout=0 让发送端补整帧——否则接收端会带着不完整
    /// 的画布永远“版本一致”。
    pub fn report(&self) -> Message {
        let mut m = self.rx.report();
        if let Message::Report { layout_id, .. } = &mut m {
            if *layout_id != self.full_layout {
                *layout_id = 0;
            }
        }
        m
    }

    pub fn full_layout(&self) -> u32 {
        self.full_layout
    }

    /// 发送端当前由视频平面出画（观看端据此在 `<video>` 与图块画布间切换）。
    pub fn video_mode(&self) -> bool {
        self.video_mode
    }

    pub fn on_wire(&mut self, msg: &WireMsg) -> Result<(), CodecError> {
        match msg {
            WireMsg::Update { seq, layout_id, cols, rows, tiles, part, total, .. } => {
                self.on_update_parts(*seq, *layout_id, *cols, *rows, tiles, *part, *total)?;
            }
            WireMsg::Scroll { layout_id, scroll_id, dy, targets, dirty, .. } => {
                self.layout_id = *layout_id;
                if !dirty.is_empty() {
                    self.pending.insert(*scroll_id, (targets.clone(), dirty.clone()));
                }
                let announce = Message::ScrollAnnounce {
                    layout_id: *layout_id,
                    scroll_id: *scroll_id,
                    dy: *dy,
                    targets: targets.clone(),
                    dirty: dirty.clone(),
                };
                self.rx.on_message(&announce);
                // 空 dirty 的组在宣告时即完成
                self.drain_completed_scrolls();
            }
            WireMsg::Motion { layout_id, w, h, data, .. } => {
                let _ = (layout_id, w, h);
                if self.video_mode && self.accept_video_plane {
                    // 视频轨正在出画，兜底 JPEG 不用解码（省下 wasm 的主线程）
                    self.stats.motion_skipped += 1;
                } else if self.canvas.w > 0 {
                    crate::motion::decode_into_canvas(data, &mut self.canvas)
                        .map_err(|_| CodecError::Corrupt("motion jpeg"))?;
                    self.mark_dirty_all();
                    // 有损帧不更新逐块版本。发送端在退出运动时会补一帧无损；
                    // 这里不把布局标记为“缺整帧”，否则运动期间每份报告都会
                    // 触发补帧（实测退化成每秒一帧大整帧的抖动）。
                    self.stats.motion_frames += 1;
                }
            }
            WireMsg::VideoMode { on, .. } => {
                self.video_mode = *on;
            }
            WireMsg::Full { layout_id, w, h, codec, codecs, versions, epochs, payloads, .. } => {
                // 旧布局的整帧晚到：模型会拒绝它，画布更不能先被它写一遍。
                if *layout_id < self.rx.layout_id {
                    self.stats.stale_dropped += 1;
                    return Ok(());
                }
                self.layout_id = *layout_id;
                let grid = TileGrid::for_frame(*w, *h);
                // 同布局的整帧只是一份**可能过时**的快照：它在可靠通道上要传好几秒
                // （1080p 一到几 MB），这段时间 lossy 通路的新块可能已经先到并落了画布。
                // 模型有"整帧不得覆盖更新状态"的保护（版本不回退），但旧代码在这里
                // 无条件把画布整体重建再写回整帧内容——于是出现"画布比版本旧"：
                // 报告认为一切齐了，残留永远补不回来，只有那块内容再次变化（鼠标划过）
                // 才会被新版本盖住。所以同布局时要保留已有像素，只补模型真正接受的块。
                let same_layout = *layout_id == self.rx.layout_id && self.grid == grid;
                let before: Vec<u32> = if same_layout { self.rx.version.clone() } else { Vec::new() };
                // 画布尺寸对齐到整帧的真实宽高（同布局下通常是从 cols*64 收敛到 1080），
                // 但**逐行搬运已有像素**，不整幅清零。
                if self.canvas.w != *w || self.canvas.h != *h {
                    let mut canvas = Frame::new(*w, *h, 0);
                    if same_layout && self.canvas.w > 0 {
                        let keep_w = (self.canvas.w.min(*w) as usize) * 4;
                        for y in 0..(self.canvas.h.min(*h) as usize) {
                            let ds = y * canvas.stride;
                            let ss = y * self.canvas.stride;
                            canvas.px[ds..ds + keep_w].copy_from_slice(&self.canvas.px[ss..ss + keep_w]);
                        }
                        canvas.ts_us = self.canvas.ts_us;
                    }
                    self.canvas = canvas;
                    self.mark_dirty_all();
                }
                self.grid = grid;
                let mut contents = Vec::with_capacity(payloads.len());
                let n = grid.tile_count().min(payloads.len());
                let mut decoded = Vec::with_capacity(n);
                for i in 0..n {
                    let id = TileId(i as u16);
                    let g = grid.geom(id, *w, *h);
                    let t = WireTile {
                        id,
                        codec: codecs.get(i).copied().unwrap_or(*codec),
                        x: g.x,
                        y: g.y,
                        w: g.w,
                        h: g.h,
                        epoch: epochs.get(i).copied().unwrap_or(0),
                        version: versions.get(i).copied().unwrap_or(0),
                        payload: payloads[i].clone(),
                    };
                    let raw = self.decode_tile(&t)?;
                    contents.push(tile_hash(&raw));
                    decoded.push((i, g, raw));
                }
                let full = Message::FullFrame {
                    layout_id: *layout_id,
                    cols: grid.cols,
                    rows: grid.rows,
                    versions: versions.clone(),
                    epochs: epochs.clone(),
                    contents,
                };
                self.rx.on_message(&full);
                for (i, g, raw) in decoded {
                    // 与模型的保护同一把尺子：整帧里比现有版本旧的块不写画布，
                    // 保留已经落上的新像素。
                    let accepted = !same_layout
                        || versions.get(i).copied().unwrap_or(0)
                            >= before.get(i).copied().unwrap_or(0);
                    if accepted {
                        self.blit(g, &raw);
                    } else {
                        self.stats.stale_dropped += 1;
                    }
                }
                self.pending.clear();
                self.grouped_raw.clear();
                self.rx.take_completed_scrolls();
                self.full_layout = *layout_id;
                self.stats.full_frames += 1;
            }
            WireMsg::State { .. } | WireMsg::Nack { .. } | WireMsg::Chunk { .. } => {}
        }
        Ok(())
    }

    fn decode_tile(&mut self, t: &WireTile) -> Result<Vec<u8>, CodecError> {
        let mut raw = vec![0u8; t.w as usize * t.h as usize * 4];
        self.codecs.decode_into(t.codec, &t.payload, &mut raw, t.w * 4)?;
        self.stats.bytes_decoded += t.payload.len() as u64;
        Ok(raw)
    }

    /// 一条逐块更新（可能是拆片的一部分）。
    ///
    /// 未拆分（`total <= 1`，含旧发送端）直接应用；拆片更新先按 `seq` 暂存，
    /// **所有片到齐**后一次性落画布。分片共享 `seq` 但以前没有片号，接收端
    /// 只能逐条应用——两次上屏之间到一半，画布就出现"半帧新、半帧旧"的
    /// 拼接（用户报的撕裂）。新 `seq` 一到就丢弃旧的残缺组：缺的块会由
    /// 500 ms 的状态报告按版本差要回来，宁可晚一帧也不要拼半帧。
    fn on_update_parts(
        &mut self,
        seq: u32,
        layout_id: u32,
        cols: u16,
        rows: u16,
        tiles: &[WireTile],
        part: u32,
        total: u32,
    ) -> Result<(), CodecError> {
        if total <= 1 {
            let decoded = self.decode_tiles(tiles)?;
            self.apply_update_tiles(layout_id, cols, rows, &decoded)?;
            if seq > self.last_update_seq {
                self.last_update_seq = seq;
            }
            return Ok(());
        }
        // 迟到的旧拆片：seq 已经翻过去，画面早就不等它了
        if seq < self.last_update_seq {
            self.stats.update_parts_stale += 1;
            return Ok(());
        }
        // 换帧：旧的残缺组整组丢弃（迟到片不可能再补齐这一帧）
        if self.pending_update.as_ref().map(|g| g.seq) != Some(seq) {
            if let Some(g) = self.pending_update.take() {
                if seq <= g.seq {
                    // 同 seq 的重复/更旧片：保留原组
                    if seq < g.seq {
                        self.stats.update_parts_stale += 1;
                    }
                    self.pending_update = Some(g);
                    return Ok(());
                }
                self.stats.update_frames_dropped += 1;
            }
            self.pending_update = Some(UpdateGroup {
                seq,
                layout_id,
                cols,
                rows,
                total,
                parts: HashMap::new(),
            });
            if seq > self.last_update_seq {
                self.last_update_seq = seq;
            }
        }
        let total_in_group = self.pending_update.as_ref().expect("刚建立").total;
        if part >= total_in_group {
            // 片号越界是协议错误；按单条应用，别把画面永久卡在待齐状态
            self.pending_update = None;
            let decoded = self.decode_tiles(tiles)?;
            self.apply_update_tiles(layout_id, cols, rows, &decoded)?;
            return Ok(());
        }
        if self
            .pending_update
            .as_ref()
            .map(|g| g.parts.contains_key(&part))
            .unwrap_or(false)
        {
            return Ok(()); // 重复片
        }
        let decoded = self.decode_tiles(tiles)?;
        let g = self.pending_update.as_mut().expect("刚建立");
        g.parts.insert(part, decoded);
        let complete = (g.parts.len() as u32) == g.total;
        if !complete {
            return Ok(());
        }
        let mut g = self.pending_update.take().expect("到齐");
        let mut idx: Vec<u32> = g.parts.keys().copied().collect();
        idx.sort_unstable();
        let mut all = Vec::new();
        for i in idx {
            if let Some(mut ts) = g.parts.remove(&i) {
                all.append(&mut ts);
            }
        }
        self.stats.updates_atomic += 1;
        self.apply_update_tiles(g.layout_id, g.cols, g.rows, &all)
    }

    fn decode_tiles(&mut self, tiles: &[WireTile]) -> Result<Vec<(WireTile, Vec<u8>)>, CodecError> {
        let mut out = Vec::with_capacity(tiles.len());
        for t in tiles {
            let raw = self.decode_tile(t)?;
            out.push((t.clone(), raw));
        }
        Ok(out)
    }

    /// 把一批已解码的块落到模型与画布上（整帧基线/拆片到齐时调用）。
    fn apply_update_tiles(
        &mut self,
        layout_id: u32,
        cols: u16,
        rows: u16,
        decoded: &[(WireTile, Vec<u8>)],
    ) -> Result<(), CodecError> {
        if layout_id > self.rx.layout_id {
            // 新布局的首批更新可能先于整帧到达：先按 Update 自带的
            // 网格重建画布与判定状态，避免旧尺寸画布与新坐标混拼。
            // 尺寸按整块网格估（最后一行/列可能多出余量），整帧到达后校准。
            self.rx.on_message(&Message::Update {
                layout_id,
                cols,
                rows,
                tiles: Vec::new(),
            });
            self.layout_id = layout_id;
            self.grid = TileGrid { cols, rows };
            self.canvas = Frame::new(cols as u32 * crate::TILE, rows as u32 * crate::TILE, 0);
            self.mark_dirty_all();
            self.pending.clear();
            self.grouped_raw.clear();
        }
        for (t, raw) in decoded {
            let model_tile = model::TileMsg {
                id: t.id,
                epoch: t.epoch,
                version: t.version,
                content: tile_hash(raw),
            };
            let verdict = self.rx.on_tile(model_tile);
            // 目标几何取**线上给的** x/y/w/h，不按画布估出来的网格算：新布局的首批
            // 更新到达时画布是按 `cols*64 × rows*64` 估的（1080p 会高 8 像素），
            // 网格算出的末行是 64 高，而线上块只有 56——那会让 blit 因"源不够长"
            // 静默跳过，模型版本却前进了，又是"画布比版本旧"的残留。
            let g = TileGeom { x: t.x, y: t.y, w: t.w, h: t.h };
            match verdict {
                model::Verdict::Applied => {
                    self.blit(g, raw);
                    self.stats.applied_tiles += 1;
                }
                model::Verdict::Buffered => {
                    if let Some(sid) = self.group_of(t.id, t.epoch) {
                        self.grouped_raw.insert((sid, t.id), (g, raw.clone()));
                    }
                    self.stats.buffered_tiles += 1;
                }
                _ => self.stats.stale_dropped += 1,
            }
        }
        self.drain_completed_scrolls();
        Ok(())
    }

    fn blit(&mut self, g: TileGeom, raw: &[u8]) {
        if self.canvas.w == 0 || g.w == 0 || g.h == 0 {
            return;
        }
        blit_geom(&mut self.canvas, g, raw);
        self.mark_dirty(g);
    }

    /// 记下画布上被写过的一块（越界部分裁掉，坐标始终落在 canvas.w×canvas.h 里）。
    fn mark_dirty(&mut self, g: TileGeom) {
        if self.canvas.w == 0 || g.w == 0 || g.h == 0 {
            return;
        }
        let x = g.x.min(self.canvas.w);
        let y = g.y.min(self.canvas.h);
        let right = (g.x + g.w).min(self.canvas.w);
        let bottom = (g.y + g.h).min(self.canvas.h);
        if right <= x || bottom <= y {
            return;
        }
        let g = TileGeom { x, y, w: right - x, h: bottom - y };
        self.dirty = Some(match self.dirty {
            Some(d) => {
                let x0 = d.x.min(g.x);
                let y0 = d.y.min(g.y);
                TileGeom {
                    x: x0,
                    y: y0,
                    w: (d.x + d.w).max(g.x + g.w) - x0,
                    h: (d.y + d.h).max(g.y + g.h) - y0,
                }
            }
            None => g,
        });
    }

    /// 整个画布都要重传（尺寸变化、有损帧、平移：每一行都换内容）。
    fn mark_dirty_all(&mut self) {
        self.mark_dirty(TileGeom { x: 0, y: 0, w: self.canvas.w, h: self.canvas.h });
    }

    /// 渲染端取走脏矩形并清空。`None` = 自上次上屏以来像素没变。
    pub fn take_dirty(&mut self) -> Option<TileGeom> {
        self.dirty.take()
    }

    /// 观看端的周期时钟。**必须定期调用**（现场节奏 = 报告周期 ~500 ms），不调用就有永久错画面：
    /// 滚动组等不齐时画布不平移，而报告要回来的块会被重新塞进组缓冲（版本却记成最新），
    /// 于是"零丢包、版本对得上、整条块行是旧的"。机制与实测数字见 `Receiver::tick` 的注释。
    ///
    /// 这里只把模型侧判定的组落屏（平移 + 已到条带块上屏），返回仍缺的块数；
    /// 缺的那些块版本仍落在发送端后面，下一份报告自己会把它们按块要回来。
    pub fn tick(&mut self, now_ms: u64) -> usize {
        // 比默认报告周期（~500 ms）略短：超时要落在"下一份报告还来得及救"的那一侧。
        const SCROLL_GROUP_TIMEOUT_MS: u64 = 400;
        let need = self.rx.tick(now_ms, SCROLL_GROUP_TIMEOUT_MS);
        self.drain_completed_scrolls();
        need.len()
    }

    fn group_of(&self, id: TileId, epoch: u32) -> Option<u32> {
        self.pending
            .iter()
            .find(|(_, (targets, _))| targets.iter().any(|&(t, e)| t == id && e == epoch))
            .map(|(sid, _)| *sid)
    }

    /// 模型判定组完成后：平移画布，再把组内 dirty 像素落上去（原子）。
    fn drain_completed_scrolls(&mut self) {
        for (sid, dy) in self.rx.take_completed_scrolls() {
            if dy != 0 {
                apply_vscroll(&mut self.canvas.px, self.canvas.stride, self.canvas.h, dy);
                // 平移后每一行都是别处的内容，整幅都得重传
                self.mark_dirty_all();
            }
            if let Some((_, dirty)) = self.pending.remove(&sid) {
                for id in dirty {
                    if let Some((g, raw)) = self.grouped_raw.remove(&(sid, id)) {
                        self.blit(g, &raw);
                    }
                }
            }
            self.stats.scrolls_applied += 1;
        }
    }
}

fn blit_geom(canvas: &mut Frame, g: TileGeom, raw: &[u8]) {
    if g.x >= canvas.w || g.y >= canvas.h {
        return;
    }
    let src_row = g.w as usize * 4;
    // 源块必须完整：短包会被静默跳过——调用方（`blit`）已经把"线上几何"传进来，
    // 所以这里只可能是真正的坏数据，不能拿缺行去写画布。
    if raw.len() < src_row * g.h as usize {
        return;
    }
    // 画布可能比真实帧大（新布局首批更新时按 cols*64 × rows*64 估的），
    // 越界部分裁掉即可，绝不能因为末行高度对不上就整块放弃。
    let copy_w = g.w.min(canvas.w - g.x) as usize;
    let copy_h = g.h.min(canvas.h - g.y) as usize;
    for y in 0..copy_h {
        let ds = (g.y as usize + y) * canvas.stride + g.x as usize * 4;
        let ss = y * src_row;
        canvas.px[ds..ds + copy_w * 4].copy_from_slice(&raw[ss..ss + copy_w * 4]);
    }
}

/// §7.2 的粒度选择在发送端的落地：把整块差分拆成 16 子块消息。
/// 返回需要发送的子块几何（`id` 仍是父块，几何是子块区域）。
pub fn sub16_geoms(grid: &TileGrid, parent: TileId, frame_w: u32, frame_h: u32) -> Vec<TileGeom> {
    let g = grid.geom(parent, frame_w, frame_h);
    let mut out = Vec::new();
    let mut y = 0;
    while y < g.h {
        let mut x = 0;
        while x < g.w {
            out.push(TileGeom {
                x: g.x + x,
                y: g.y + y,
                w: SUBTILE.min(g.w - x).min(TILE),
                h: SUBTILE.min(g.h - y).min(TILE),
            });
            x += SUBTILE;
        }
        y += SUBTILE;
    }
    out
}

/// 满量程跳变计数：相邻像素亮度差 >128 的次数（横 + 纵）。
/// 为什么用这条可算的量：字与细线是"一跳到头、成对出现"，而渐变/噪声是"跳得频繁但幅度小"。
/// 实测（`crates/ess-codec-palette/tests/codec_cost.rs`）：照片块 0、照片上压字 462、
/// 平涂文本 192..544 —— 用它把"糊了会心疼"的块挡在有损门外，只花一次 4096 px 扫描。
fn full_scale_edges(raw: &[u8], w: u32, h: u32) -> usize {
    let (w, h) = (w as usize, h as usize);
    let lum = |i: usize| -> u32 {
        (raw[i] as u32 * 19595 + raw[i + 1] as u32 * 38470 + raw[i + 2] as u32 * 7471) >> 16
    };
    let mut hits = 0usize;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            if x + 1 < w && lum(i).abs_diff(lum(i + 4)) > 128 {
                hits += 1;
            }
            if y + 1 < h && lum(i).abs_diff(lum(i + w * 4)) > 128 {
                hits += 1;
            }
        }
    }
    hits
}
