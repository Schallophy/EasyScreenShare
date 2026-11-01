//! 视频平面：把帧转成 I420 推给 WebRTC 视频轨（媒体通道）。
//!
//! 数据通道的实测上限 ~3.7 MB/s 与帧率无关地卡死了无损图块路径；VP8 屏幕
//! 共享编码器有自己的带宽估计和码控，同样 4 Mbps 能稳住 30 fps。所以决策器
//! 判为大面积运动时，画面改由这条轨道出，`ess-core` 只负责决定何时切。

use livekit::webrtc::prelude::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::VideoResolution;
use ess_core::frame::Frame;

pub struct VideoPlane {
    src: NativeVideoSource,
    w: u32,
    h: u32,
    /// 实际送入编码器的尺寸（可以小于声明尺寸）。帧率合同靠它来守：码率就那么多，
    /// 720p 塞不下 30 帧时由我们主动降到更小的送入帧，而不是等 WebRTC 一帧帧丢。
    fw: u32,
    fh: u32,
    /// 送入缩放的下限，按产品档设（流畅优先档愿意缩得更小）。
    floor: f64,
    pub pushed: u64,
    pub dropped: u64,
    /// `push` 的三段耗时累计（µs）：新建 I420 缓冲 / BGRA→I420 转换 / 交给 libwebrtc。
    /// 归因用：视频平面臂实测每帧 13 ms 花在喂帧上，但不知道是分配、转换还是适配层
    /// （§14.7.12）。
    pub t_new_us: u128,
    pub t_cvt_us: u128,
    pub t_cap_us: u128,
}

/// 送入缩放的可调范围：默认下限 0.4（再小字就糊得没法看了），步进 0.25。
/// 产品档可以另设下限（`VideoPlane::set_feed_floor`）——流畅优先档把帧率看得比
/// 分辨率重，允许缩到 0.25。
pub const MIN_FEED_SCALE: f64 = 0.4;
pub const MAX_FEED_SCALE: f64 = 1.0;

impl VideoPlane {
    /// 分辨率在会话开始时定死（源轨的适配按声明尺寸做，中途改会掉帧）。
    /// 返回轨道本体和用于发布的 `NativeVideoSource`。
    ///
    /// `is_screencast` 决定 libwebrtc 用哪套码控：屏幕共享档偏重画质与文字锐度，
    /// 但会引入更高延迟。配对实测（一次开机交替三对，1080p30 平衡档 6 Mbps）：关掉
    /// 只省约 5 ms（96→91、98→93、77→76），解码尺寸与上屏帧数一模一样——原来记的
    /// "影响明显"不成立，留成开关但默认保持屏幕共享档（§14.7.27）。
    pub fn new(
        screen_w: u32,
        screen_h: u32,
        max_w: u32,
        max_h: u32,
        is_screencast: bool,
    ) -> (Self, NativeVideoSource) {
        let (w, h) = ess_core::motion::fit(screen_w, screen_h, max_w, max_h);
        let src = NativeVideoSource::new(VideoResolution { width: w, height: h }, is_screencast);
        (
            Self { src: src.clone(), w, h, fw: w, fh: h, floor: MIN_FEED_SCALE, pushed: 0, dropped: 0, t_new_us: 0, t_cvt_us: 0, t_cap_us: 0 },
            src,
        )
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// 当前送入缩放（1.0 = 按声明尺寸原样送）。
    pub fn scale(&self) -> f64 {
        if self.w == 0 {
            1.0
        } else {
            self.fw as f64 / self.w as f64
        }
    }

    /// 送入缩放下限（产品档用，见 [`Self::set_feed_floor`]）。
    pub fn floor(&self) -> f64 {
        self.floor
    }

    /// 按产品档设下限。默认 [`MIN_FEED_SCALE`]；流畅优先档愿意缩得更小，因为
    /// 那一档花掉的代价就是分辨率。
    pub fn set_feed_floor(&mut self, f: f64) {
        self.floor = f.clamp(0.1, MAX_FEED_SCALE);
    }

    /// 改送入尺寸，返回新的 (宽, 高)。声明给订阅端的轨道尺寸不变，只有实际编码的
    /// 帧变小——WebRTC 自己也是这么降的，差别在于它要几秒才动，而一次滚动只有几秒。
    pub fn set_feed_scale(&mut self, s: f64) -> (u32, u32) {
        let s = s.clamp(self.floor, MAX_FEED_SCALE);
        let even = |v: u32| (v & !1).max(2);
        self.fw = even((self.w as f64 * s) as u32);
        self.fh = even((self.h as f64 * s) as u32);
        (self.fw, self.fh)
    }

    /// 送一帧给编码器。时间戳留给 libwebrtc 取当前时钟：本函数就在采集
    /// 节拍上调用，与它自己的 µs 时钟一致，不会因时钟错位丢帧。
    pub fn push(&mut self, frame: &Frame) {
        let (fw, fh) = (self.fw, self.fh);
        let t0 = std::time::Instant::now();
        let mut buf = I420Buffer::new(fw, fh);
        self.t_new_us += t0.elapsed().as_micros();
        {
            let t1 = std::time::Instant::now();
            let (dy, du, dv) = buf.data_mut();
            // 直接写进编码器的平面：省掉整帧 2.7 MB 的中转拷贝
            ess_core::motion::write_i420(frame, fw, fh, dy, du, dv);
            self.t_cvt_us += t1.elapsed().as_micros();
        }
        let t2 = std::time::Instant::now();
        let vf = VideoFrame::new(VideoRotation::VideoRotation0, buf);
        // false = 被适配层丢弃（尺寸/帧率不匹配），计数暴露出来便于诊断
        if self.src.capture_frame(&vf) {
            self.pushed += 1;
        } else {
            self.dropped += 1;
        }
        self.t_cap_us += t2.elapsed().as_micros();
    }
}
