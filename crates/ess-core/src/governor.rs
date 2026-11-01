//! 质量守门员（§9）。控制环每 250 ms 观测一次，两次**调整**间隔 ≥ 2 s（迟滞），
//! 紧急条件（队列延迟）可立即降级。降级阶梯按内容类区分（§9.3）。

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContentClass {
    Text,
    Video,
    Game,
}

/// 一次观测。字段对应 §9.2 的判据。
#[derive(Clone, Copy, Debug)]
pub struct Obs {
    pub encode_ms: f32,
    pub budget_ms: f32,
    pub queue_delay_ms: u32,
    pub qp: u8,
    pub qp_max: u8,
    pub bwe_bps: u64,
    pub needed_bps: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    pub fps: u32,
    /// 输出高度（宽按原宽高比）。1080 = 不缩放。
    pub height: u32,
    /// 图块平面已用尽 → 整帧有损兜底。
    pub lossy_fallback: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Down,
    Up,
}

/// 阶梯（§9.3 冻结前的当前版本）。
fn ladder(class: ContentClass) -> Vec<Settings> {
    match class {
        ContentClass::Text => vec![
            Settings { fps: 15, height: 1080, lossy_fallback: false },
            Settings { fps: 10, height: 1080, lossy_fallback: false },
            Settings { fps: 5, height: 1080, lossy_fallback: false },
            Settings { fps: 2, height: 1080, lossy_fallback: false },
            Settings { fps: 2, height: 1080, lossy_fallback: true },
        ],
        ContentClass::Video => vec![
            Settings { fps: 30, height: 1080, lossy_fallback: false },
            Settings { fps: 15, height: 1080, lossy_fallback: false },
            Settings { fps: 15, height: 900, lossy_fallback: false },
            Settings { fps: 15, height: 720, lossy_fallback: false },
        ],
        ContentClass::Game => vec![
            Settings { fps: 60, height: 1080, lossy_fallback: false },
            Settings { fps: 30, height: 1080, lossy_fallback: false },
            Settings { fps: 30, height: 900, lossy_fallback: false },
            Settings { fps: 30, height: 720, lossy_fallback: false },
            Settings { fps: 30, height: 540, lossy_fallback: false },
            Settings { fps: 30, height: 360, lossy_fallback: false },
            Settings { fps: 30, height: 360, lossy_fallback: true },
        ],
    }
}

/// 滚动/运动类内容 fps 下限（评审一.3）。滚动内容不允许降到 15 以下。
pub const MOTION_FPS_FLOOR: u32 = 15;

pub struct Governor {
    pub class: ContentClass,
    ladder: Vec<Settings>,
    rung: usize,
    last_adjust_ms: Option<u64>,
    /// bwe 持续富余的起始时刻（§9.2：持续 5 s 才升档）
    surplus_since: Option<u64>,
    /// 内容是否含运动（滚动/视频窗）；为 true 时 fps 不低于 MOTION_FPS_FLOOR
    pub motion: bool,
}

impl Governor {
    pub fn new(class: ContentClass) -> Self {
        Self {
            class,
            ladder: ladder(class),
            rung: 0,
            last_adjust_ms: None,
            surplus_since: None,
            motion: class != ContentClass::Text,
        }
    }

    pub fn settings(&self) -> Settings {
        let mut s = self.ladder[self.rung];
        if self.motion && s.fps < MOTION_FPS_FLOOR {
            s.fps = MOTION_FPS_FLOOR.max(self.ladder[0].fps.min(MOTION_FPS_FLOOR));
        }
        s
    }

    /// 阶梯顶端的帧率。调用方把用户上限当**天花板**，用 `settings().fps / top_fps()`
    /// 这个比例去缩放它——否则设 30 fps 的用户被降到一档后，最多只能回到阶梯顶的 15。
    pub fn top_fps(&self) -> u32 {
        self.ladder[0].fps.max(1)
    }

    pub fn set_class(&mut self, class: ContentClass) {
        if class != self.class {
            self.class = class;
            self.ladder = ladder(class);
            self.rung = self.rung.min(self.ladder.len() - 1);
            self.motion = class != ContentClass::Text;
        }
    }

    /// 每个观测周期调用（建议 250 ms）。返回本周期是否调整。
    pub fn tick(&mut self, now_ms: u64, obs: &Obs) -> Option<Step> {
        let hard = obs.queue_delay_ms > 100;
        let over_time = obs.encode_ms > obs.budget_ms * 0.9;
        let over_qp = obs.qp > obs.qp_max;
        let congested = obs.bwe_bps < obs.needed_bps;

        if hard || over_time || over_qp || congested {
            self.surplus_since = None;
            if self.rung + 1 < self.ladder.len() {
                if hard || self.can_adjust(now_ms) {
                    self.rung += 1;
                    self.last_adjust_ms = Some(now_ms);
                    return Some(Step::Down);
                }
            }
            return None;
        }

        // 升档：带宽持续富余 30% 达 5 s
        if obs.bwe_bps * 10 > obs.needed_bps * 13 {
            let since = *self.surplus_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= 5_000 && self.rung > 0 && self.can_adjust(now_ms) {
                self.rung -= 1;
                self.last_adjust_ms = Some(now_ms);
                self.surplus_since = None;
                return Some(Step::Up);
            }
        } else {
            self.surplus_since = None;
        }
        None
    }

    fn can_adjust(&self, now_ms: u64) -> bool {
        self.last_adjust_ms.map(|t| now_ms.saturating_sub(t) >= 2_000).unwrap_or(true)
    }
}
