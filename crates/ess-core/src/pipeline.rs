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

#[derive(Default, Debug, Clone, Copy)]
pub struct SendStats {
    pub frames: u64,
    pub full_frames: u64,
    pub scrolls: u64,
    pub updates: u64,
    pub tiles_sent: u64,
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
    /// 脱落要补账，这一帧要决定怎么补：
    /// 见 [`Self::note_link_loss`] 与差分段的取舍（#52）。
    loss_pending: bool,
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
            budget_bps: 0,
            win_ts: 0,
            win_bytes: 0,
            win_tiles: 0,
            last_encoded: 0,
            budget_plane: false,
            encode_pressure: false,
            media_pending: false,
            warm_started_us: 0,
            warm_ok_run: 0,
            warm_size: (0, 0),
            warm_max_us: 6_000_000,
            receiver_strain: false,
            plane_prev: None,
            loss_claimed: false,
            loss_pending: false,
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
    pub fn set_encode_pressure(&mut self, on: bool) {
        self.encode_pressure = on;
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
                if !self.budget_room(demand_now) || self.encode_pressure {
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
                    // 视频平面退出：画布没被有损帧碰过，只补真正的差集（可以为空 =
                    // 画面已经回到运动前的样子，一个像素都不用发）
                    Some(base) if leaving_video => {
                        out.extend(self.emit_repair(&base, cur)?);
                    }
                    // JPEG 运动通路：画布被有损帧盖过，必须整帧重建精确状态
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
        if self.decide_log {
            // 归因出问题时唯一能看的证据：这一帧的无损需求量、预算、以及三条判据此刻
            // 各自成不成立（`forced_plane` 只有帧末累计值，看不出是谁该记账）。
            eprintln!(
                "BUDGET ts={} changed={} demand={}KB/s budget={}KB/s room={} enc_press={} strain={} plane={}",
                cur.ts_us / 1000,
                diff.changed.len(),
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
        let by_budget = !room || loss_forces;
        let by_encode = room && self.encode_pressure;
        let by_strain = room && !self.encode_pressure && self.receiver_strain;
        if self.video_plane
            && (!diff.changed.is_empty() || loss_forces)
            && (by_budget || by_encode || by_strain)
        {
            self.budget_plane = true;
            // 预算/编码逼出来的交接不等预热：无损通路自己都塞不下了，没什么可等的。
            // strain 逼出来的也不等——等它爬坡的那几秒正是它啃不动的画面。
            self.media_pending = false;
            self.plane_prev = Some(cur.clone());
            self.motion_active = true;
            self.motion_hold = self.hold_frames;
            if by_budget {
                // 差分修复的前提是"观看端画布 == 发送端 `last_sent`"。预算逼进平面时这个
                // 前提恰好不成立：那些块正是无损通路自己发不动的（§6.6 压住的需求）。退出
                // 时只补「基线 ↔ 当前帧」的差集就会漏掉它们——实测 motion 门禁在所有
                // `forced_plane=1` 的运行里停在旧画布（落画 7910 / 期望 10307，#56）。
                // 这种交接退出走整帧重建（既有 `_ => emit_full` 那条路，可靠通道）。
                self.repair_baseline = None;
            } else if self.repair_baseline.is_none() {
                self.repair_baseline = Some(self.state.last_sent.clone());
            }
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
        let mut chosen = None;
        for c in self.codecs.codecs {
            let mut payloads = Vec::with_capacity(n);
            let mut ok = true;
            for i in 0..n {
                let id = TileId(i as u16);
                let g = grid.geom(id, cur.w, cur.h);
                let raw = extract_tile(cur, g);
                match c.encode(&raw, g.w, g.h) {
                    Ok(p) => payloads.push(p),
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                chosen = Some((c.id(), payloads));
                break;
            }
        }
        let Some((full_codec, payloads)) = chosen else {
            return Err(CodecError::Unsupported);
        };
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
        let tiles = self.ctl.on_report(layout_id, versions, epochs, now_ms);
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
            if self.ctl.last_gap == 0 {
                self.loss_pending = true;
            }
        }
        if tiles.is_empty() {
            return Ok(Vec::new());
        }
        let msg = Message::Update {
            layout_id: self.ctl.layout_id,
            cols: self.grid.cols,
            rows: self.grid.rows,
            tiles,
        };
        let frame = self.state.last_sent.clone();
        Ok(vec![self.wire_update(msg, &frame, false)?])
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
            let (codec_id, payload) = self.codecs.encode(&raw, g.w, g.h)?;
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
            tiles: wt,
        })
    }
}

pub struct RecvPipeline<'c> {
    pub canvas: Frame,
    pub grid: TileGrid,
    pub rx: Receiver,
    pub codecs: CodecSet<'c>,
    pub stats: RecvStats,
    /// 未完成滚动组：(scroll_id) → (targets, dirty)
    pending: HashMap<u32, (Vec<(TileId, u32)>, Vec<TileId>)>,
    /// 组内等待完成的像素：(scroll_id, tile) → 解码后的紧排 BGRA
    grouped_raw: HashMap<(u32, TileId), Vec<u8>>,
    layout_id: u32,
    /// 已由整帧建立（像素完整）的布局代次。新布局只有收到整帧才算建立；
    /// 之前的更新可能已把网格推进，但画布像素不完整，报告必须请求整帧。
    full_layout: u32,
    /// 本端能显示视频轨（媒体通道）。为 true 时，视频模式期间的 JPEG 兜底帧
    /// 不再贴画布——否则低清帧会盖在视频上面，退出时又闪一下。
    pub accept_video_plane: bool,
    video_mode: bool,
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
            layout_id: 0,
            full_layout: 0,
            accept_video_plane: false,
            video_mode: false,
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
            WireMsg::Update { layout_id, cols, rows, tiles, .. } => {
                if *layout_id > self.rx.layout_id {
                    // 新布局的首批更新可能先于整帧到达：先按 Update 自带的
                    // 网格重建画布与判定状态，避免旧尺寸画布与新坐标混拼。
                    // 尺寸按整块网格估（最后一行/列可能多出余量），整帧到达后校准。
                    self.rx.on_message(&Message::Update {
                        layout_id: *layout_id,
                        cols: *cols,
                        rows: *rows,
                        tiles: Vec::new(),
                    });
                    self.layout_id = *layout_id;
                    self.grid = TileGrid { cols: *cols, rows: *rows };
                    self.canvas = Frame::new(
                        *cols as u32 * crate::TILE,
                        *rows as u32 * crate::TILE,
                        0,
                    );
                    self.pending.clear();
                    self.grouped_raw.clear();
                }
                for t in tiles {
                    let raw = self.decode_tile(t)?;
                    let model_tile = model::TileMsg {
                        id: t.id,
                        epoch: t.epoch,
                        version: t.version,
                        content: tile_hash(&raw),
                    };
                    let verdict = self.rx.on_tile(model_tile);
                    match verdict {
                        model::Verdict::Applied => {
                            self.blit(t.id, &raw);
                            self.stats.applied_tiles += 1;
                        }
                        model::Verdict::Buffered => {
                            if let Some(sid) = self.group_of(t.id, t.epoch) {
                                self.grouped_raw.insert((sid, t.id), raw);
                            }
                            self.stats.buffered_tiles += 1;
                        }
                        _ => self.stats.stale_dropped += 1,
                    }
                }
                self.drain_completed_scrolls();
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
                    // 有损帧不更新逐块版本。发送端在退出运动时会补一帧无损；
                    // 这里不把布局标记为“缺整帧”，否则运动期间每份报告都会
                    // 触发补帧（实测退化成每秒一帧大整帧的抖动）。
                    self.stats.motion_frames += 1;
                }
            }
            WireMsg::VideoMode { on, .. } => {
                self.video_mode = *on;
            }
            WireMsg::Full { layout_id, w, h, codec, versions, epochs, payloads, .. } => {
                self.layout_id = *layout_id;
                let grid = TileGrid::for_frame(*w, *h);
                self.grid = grid;
                self.canvas = Frame::new(*w, *h, 0);
                let mut contents = Vec::with_capacity(payloads.len());
                let n = grid.tile_count().min(payloads.len());
                for i in 0..n {
                    let id = TileId(i as u16);
                    let g = grid.geom(id, *w, *h);
                    let t = WireTile {
                        id,
                        codec: *codec,
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
                    self.blit(id, &raw);
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

    fn blit(&mut self, id: TileId, raw: &[u8]) {
        if self.canvas.w == 0 {
            return;
        }
        let g = self.grid.geom(id, self.canvas.w, self.canvas.h);
        blit_geom(&mut self.canvas, g, raw);
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
            }
            if let Some((_, dirty)) = self.pending.remove(&sid) {
                for id in dirty {
                    if let Some(raw) = self.grouped_raw.remove(&(sid, id)) {
                        self.blit(id, &raw);
                    }
                }
            }
            self.stats.scrolls_applied += 1;
        }
    }
}

fn blit_geom(canvas: &mut Frame, g: TileGeom, raw: &[u8]) {
    let row_bytes = g.w as usize * 4;
    if raw.len() < row_bytes * g.h as usize {
        return;
    }
    for y in 0..g.h as usize {
        let ds = (g.y as usize + y) * canvas.stride + g.x as usize * 4;
        let ss = y * row_bytes;
        canvas.px[ds..ds + row_bytes].copy_from_slice(&raw[ss..ss + row_bytes]);
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
