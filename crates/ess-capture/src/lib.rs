//! 屏幕采集。Windows：GDI BitBlt（v1；WGC 是后续替换项，接口不变）。
//! 其他平台：编译期报错，接口已留好。

use ess_core::frame::Frame;

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC,
        ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
        HGDIOBJ, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo,
        GetSystemMetrics, ICONINFO, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    pub struct WindowsCapture {
        screen_dc: HDC,
        mem_dc: HDC,
        bmp: HBITMAP,
        old: HGDIOBJ,
        bits: *mut u8,
        pub w: u32,
        pub h: u32,
        x: i32,
        y: i32,
        stride: usize,
        /// 把鼠标光标画进帧里（GDI 抓不到硬件光标，观看端就完全看不到指针）
        pub(crate) draw_cursor: bool,
    }

    // GDI 句柄与位图指针由本结构独占，跨线程传递是安全的（BitBlt 自身线程安全）。
    unsafe impl Send for WindowsCapture {}

    impl WindowsCapture {
        pub fn new() -> Result<Self, String> {
            unsafe {
                // 采整个虚拟桌面而不是主屏：以前用 SM_CXSCREEN（主屏尺寸）配
                // SM_XVIRTUALSCREEN（虚拟桌面原点），单屏恰好正确，多屏就会从错误的
                // 原点裁一块主屏大小的画面——副屏内容根本不进画面。
                let (mut w, mut h) = (
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                );
                if w <= 0 || h <= 0 {
                    w = GetSystemMetrics(SM_CXSCREEN);
                    h = GetSystemMetrics(SM_CYSCREEN);
                }
                let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
                if w <= 0 || h <= 0 {
                    return Err("no screen metrics".into());
                }
                let screen_dc = GetDC(std::ptr::null_mut());
                if screen_dc.is_null() {
                    return Err("GetDC failed".into());
                }
                let mem_dc = CreateCompatibleDC(screen_dc);
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // 负数 = 自上而下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..std::mem::zeroed()
                };
                let mut bits: *mut c_void = std::ptr::null_mut();
                let bmp = CreateDIBSection(mem_dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
                if bmp.is_null() || bits.is_null() {
                    return Err("CreateDIBSection failed".into());
                }
                let old = SelectObject(mem_dc, bmp);
                Ok(Self {
                    screen_dc,
                    mem_dc,
                    bmp,
                    old,
                    bits: bits as *mut u8,
                    w: w as u32,
                    h: h as u32,
                    x,
                    y,
                    stride: w as usize * 4,
                    draw_cursor: std::env::var("ESS_CURSOR").map(|v| v != "0").unwrap_or(true),
                })
            }
        }

        /// 光标画到采集位图上。位置 = 屏幕坐标 - 采集原点 - 热点偏移；
        /// 尺寸交给 DrawIconEx（传 0,0 即按光标原始尺寸绘制）。
        unsafe fn paint_cursor_raw(&self, ignore_visibility: bool) {
            let mut ci = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                flags: 0,
                hCursor: std::ptr::null_mut(),
                ptScreenPos: POINT { x: 0, y: 0 },
            };
            if GetCursorInfo(&mut ci) == 0 || ci.hCursor.is_null() {
                return;
            }
            // 只在指针真的在显示时画：隐藏（全屏游戏锁定指针）与触控抑制都不该出现
            if !ignore_visibility && ci.flags != CURSOR_SHOWING {
                return;
            }
            let (mut hx, mut hy) = (0i32, 0i32);
            let mut ii = ICONINFO {
                fIcon: 0,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: std::ptr::null_mut(),
                hbmColor: std::ptr::null_mut(),
            };
            if GetIconInfo(ci.hCursor, &mut ii) != 0 {
                hx = ii.xHotspot as i32;
                hy = ii.yHotspot as i32;
                if !ii.hbmColor.is_null() {
                    DeleteObject(ii.hbmColor);
                }
                if !ii.hbmMask.is_null() {
                    DeleteObject(ii.hbmMask);
                }
            }
            DrawIconEx(
                self.mem_dc,
                ci.ptScreenPos.x - self.x - hx,
                ci.ptScreenPos.y - self.y - hy,
                ci.hCursor,
                0,
                0,
                0,
                std::ptr::null_mut(),
                DI_NORMAL,
            );
        }

        /// 只跑 BitBlt（不含拷贝/转换）的吞吐，用于定位 60 fps 卡在哪儿。
        pub fn bench_bitblt(&mut self, rop: u32, n: u32) -> f64 {
            let t0 = std::time::Instant::now();
            unsafe {
                for _ in 0..n {
                    BitBlt(
                        self.mem_dc,
                        0,
                        0,
                        self.w as i32,
                        self.h as i32,
                        self.screen_dc,
                        self.x,
                        self.y,
                        rop,
                    );
                }
            }
            n as f64 / t0.elapsed().as_secs_f64()
        }

        /// 只量「画光标」这一步改了哪些像素：BitBlt 一次 → 快照 → 画光标 → 再快照。
        /// 不重复抓屏，桌面自己在动也不会污染断言。返回 (像素数, 左上, 右下)。
        pub fn cursor_footprint(&mut self) -> (usize, (usize, usize), (usize, usize)) {
            unsafe {
                BitBlt(
                    self.mem_dc,
                    0,
                    0,
                    self.w as i32,
                    self.h as i32,
                    self.screen_dc,
                    self.x,
                    self.y,
                    SRCCOPY | CAPTUREBLT,
                );
                let len = self.stride * self.h as usize;
                let before = std::slice::from_raw_parts(self.bits, len).to_vec();
                self.paint_cursor_raw(true);
                let after = std::slice::from_raw_parts(self.bits, len).to_vec();
                let mut min = (usize::MAX, usize::MAX);
                let mut max = (0usize, 0usize);
                let mut n = 0usize;
                for y in 0..self.h as usize {
                    for x in 0..self.w as usize {
                        let i = y * self.stride + x * 4;
                        if before[i..i + 4] != after[i..i + 4] {
                            n += 1;
                            min = (min.0.min(x), min.1.min(y));
                            max = (max.0.max(x), max.1.max(y));
                        }
                    }
                }
                (n, min, max)
            }
        }

        pub fn frame(&mut self, ts_us: u64) -> Frame {
            unsafe {
                // CAPTUREBLT：带上分层窗口（tooltip、部分覆盖层）
                BitBlt(
                    self.mem_dc,
                    0,
                    0,
                    self.w as i32,
                    self.h as i32,
                    self.screen_dc,
                    self.x,
                    self.y,
                    SRCCOPY | CAPTUREBLT,
                );
                if self.draw_cursor {
                    self.paint_cursor_raw(false);
                }
                let len = self.stride * self.h as usize;
                let src = std::slice::from_raw_parts(self.bits, len);
                Frame { w: self.w, h: self.h, stride: self.stride, px: src.to_vec(), ts_us }
            }
        }
    }

    impl Drop for WindowsCapture {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.mem_dc, self.old);
                DeleteObject(self.bmp);
                DeleteDC(self.mem_dc);
                ReleaseDC(std::ptr::null_mut(), self.screen_dc);
            }
        }
    }
}

#[cfg(windows)]
pub use win::WindowsCapture;

#[cfg(windows)]
mod dxgi;

/// 通路选择策略在调用方（`ess-session`）：那里知道设定的帧率——本机满载实测
/// GDI 只能给 53–56 fps、桌面复制能给到 60+，而桌面复制对多显示器只复制主输出，
/// 所以 >30 fps 才值得切过去。两条通路各自独立可用，`ESS_CAPTURE=gdi|dxgi` 可强制。
/// 按通路名创建采集源。`dxgi` 失败时调用方应回落到 `gdi`。
#[cfg(windows)]
pub fn create(mode: &str) -> Result<Box<dyn ScreenSource>, String> {
    if mode == "dxgi" {
        return Ok(Box::new(dxgi::DupCapture::new()?));
    }
    Ok(Box::new(WindowsCapture::new()?))
}

/// 桌面上接了几个显示输出。桌面复制只复制主输出，所以多屏时不能盲切 DXGI——
/// 这条判据就是给通路选择用的（见 `ess-session::make_source`）。
#[cfg(windows)]
pub fn monitor_count() -> usize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
    unsafe { GetSystemMetrics(SM_CMONITORS).max(0) as usize }
}

/// 采集源统一接口。
pub trait ScreenSource: Send {
    fn dims(&self) -> (u32, u32);
    fn frame(&mut self, ts_us: u64) -> Frame;
}

#[cfg(windows)]
impl ScreenSource for WindowsCapture {
    fn dims(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn frame(&mut self, ts_us: u64) -> Frame {
        WindowsCapture::frame(self, ts_us)
    }
}

#[cfg(windows)]
impl ScreenSource for dxgi::DupCapture {
    fn dims(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn frame(&mut self, ts_us: u64) -> Frame {
        dxgi::DupCapture::frame(self, ts_us)
    }
}

/// 合成屏（无采集环境 / 自动化验收用）：确定性动画 + 固定终帧。
pub struct SyntheticScreen {
    pub w: u32,
    pub h: u32,
    /// 前动画秒数（按 10 fps 计帧）
    pub animate_frames: u32,
    /// 动画开始前的静止帧数（按 10 fps 计）。平面切换的验收需要"先静止若干秒再动"，
    /// 否则测的是会话冷启动而不是静止→运动这一沿。
    pub animate_delay: u32,
    /// 延迟测量用：每 2 s 墙钟的前 100 ms 输出纯白帧。上升沿对齐秒边界，
    /// 观看端不需要任何带内信令就能算出「注入 → 呈现」的延迟。
    pub flash: bool,
    /// 是否画那块**独立移动**的红块。开着 = "滚动 + 局部动画"的混合内容（走整屏差分/
    /// 媒体通道）；关掉 = 纯文本滚动（走滚动宣告）。两种内容落在不同通路上，验收必须
    /// 说清测的是哪一种（O-23）。
    pub block: bool,
    /// "视频窗口 + 弹幕"内容：背景完全静止，屏幕中部一大块内容每帧重排（不是平移，
    /// 滚动检测器认不出来），上方一条窄带横向飘字。用户的真实工作点是这个——播放基本
    /// 不动的视频、弹幕飘过；滚动型 demo 会让无损通路看起来比实际便宜一个数量级。
    pub video_like: bool,
    /// 单帧大面积突变（0 = 关）：第 N 帧把左半屏一次性重绘，之后保持不变。
    ///
    /// 存在的理由：渐进动画里「一次大变化的代价」摊在几十帧上，丢包/丢批次读起来跟
    /// 「链路慢」一模一样。要分清是丢还是慢，必须让整场会话只发生一次大变化。
    /// 用 `ESS_DEMO_STEP` 选，配 `ESS_DEMO_ANIM=0`（背景全静止）使用。
    pub step_at: u32,
}

impl SyntheticScreen {
    pub fn new(w: u32, h: u32, animate_frames: u32) -> Self {
        Self {
            w,
            h,
            animate_frames,
            animate_delay: 0,
            flash: false,
            block: true,
            video_like: false,
            step_at: 0,
        }
    }

    /// 当前是否处于白闪窗口（墙钟对齐，观看端用同一套判定）
    pub fn flash_now() -> bool {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        ms % 2000 < 100
    }

    /// "视频 + 弹幕"帧：静止桌面背景 + 中部一块每帧重排的"播放器" + 顶部飘字带。
    /// 播放器按 32 px 格子取色，列方向位移随 `idx` 变，所以整块**不是**任何平移量下
    /// 的自身拷贝——滚动检测器只能返回 None，无损通路必须逐块重画。这才是用户真实
    /// 工作点的负载（1080p 约 36% 面积每帧变化 ≈ 180/510 块）。
    fn video_frame(&self, mut f: ess_core::frame::Frame, idx: u32) -> ess_core::frame::Frame {
        let (w, h) = (self.w, self.h);
        let bg = 0xff20_2020u32;
        for y in 0..h {
            for x in 0..w {
                // 背景留一点行纹理，避免纯色被压缩成极端便宜的数据
                let v = if (x / 2) % 9 == 0 { 0xff28_2828 } else { bg };
                let s = y as usize * f.stride + x as usize * 4;
                f.px[s..s + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        let pal = [
            0xff30_3050u32, 0xff80_6030, 0xffe0_c860, 0xff40_a060, 0xffd0_4040, 0xff90_6090,
        ];
        let (vx, vy) = (w / 6, h / 6);
        let (vw, vh) = (w * 2 / 3, h / 2);
        let cell = 32u32;
        let mut cy = vy;
        while cy < vy + vh {
            let mut cx = vx;
            while cx < vx + vw {
                let gx = cx / cell;
                let gy = cy / cell;
                // 每列的相位不同：位移量依赖 gx，因此整块画面无法用单一 dy 描述
                let k = gx.wrapping_mul(7)
                    ^ gy.wrapping_mul(19)
                    ^ idx.wrapping_mul(3 + gx % 5);
                let c = pal[(k as usize) % pal.len()];
                let (ew, eh) = (cell.min(vx + vw - cx), cell.min(vy + vh - cy));
                for dy in 0..eh {
                    for dx in 0..ew {
                        // 格子内加一条暗边，让画面有结构（不是纯色块拼贴）
                        let v = if dx + 1 == ew || dy + 1 == eh { c & 0x00_ff_ff_ff } else { c };
                        let s = (cy + dy) as usize * f.stride + (cx + dx) as usize * 4;
                        f.px[s..s + 4].copy_from_slice(&v.to_le_bytes());
                    }
                }
                cx += cell;
            }
            cy += cell;
        }
        // 弹幕带：三条白字横块整体左移，窄带平移，模拟 danmaku
        let dx0 = (idx * 37) % (w + 320);
        for band in 0..3u32 {
            let y = h * 4 / 100 + band * 20;
            let run = w / 5;
            let x0 = w.saturating_sub(dx0 + band * run / 2).min(w);
            for dy in 0..14u32 {
                for dx in 0..run {
                    let px = x0.wrapping_add(dx);
                    if px >= w {
                        continue;
                    }
                    let s = (y + dy) as usize * f.stride + px as usize * 4;
                    f.px[s..s + 4].copy_from_slice(&0xffee_eeeeu32.to_le_bytes());
                }
            }
        }
        f
    }
}

impl ScreenSource for SyntheticScreen {
    fn dims(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    fn frame(&mut self, ts_us: u64) -> Frame {
        let idx = (ts_us / 100_000) as u32; // 10 fps
        let mut f = Frame::new(self.w, self.h, ts_us);
        if self.flash && Self::flash_now() {
            for o in (0..f.px.len()).step_by(4) {
                f.px[o] = 255;
                f.px[o + 1] = 255;
                f.px[o + 2] = 255;
                f.px[o + 3] = 255;
            }
            return f;
        }
        let colors = [0xff10_1010u32, 0xffee_eeee, 0xff20_2020, 0xffe0_e0e0, 0xff00_66cc];
        if self.video_like {
            // 视频内容必须按**设定帧率**变，不能沿用其它模式的 10 fps 量化：那样 30 fps
            // 里有 2 帧是重复的，运动比会掉到迟滞阈值以下，平面进出抖动（实测 24 s 里
            // switches=4、egress 只有 0.2 Mbps、编码器只喂进 4.8 fps）——测出来的是
            // 素材的假象，不是产品行为。
            return self.video_frame(f, (ts_us / 33_333) as u32);
        }
        // 文本页风格背景（16 px 行纹）；动画期随帧变化，静止期固定
        let anim = idx.saturating_sub(self.animate_delay).min(self.animate_frames);
        for y in 0..self.h {
            for x in 0..self.w {
                // 纹理坐标：整帧 = 同一张图纵向平移 16×anim 像素。行纹和行噪点必须
                // 用**同一个**坐标，否则"滚动"里混进一块不跟着动的静态层，滚动检测
                // 认不出来（返回 dy=0），于是一次滚动被当成整屏重绘 —— 实测代价是
                // 无损通路从 1.19 Mbps 涨到 4–5 Mbps 还只能到 10 fps（O-23）。
                let yy = y.wrapping_add(anim * 16);
                let sx = x / 16;
                let sy = yy / 16;
                let line = sy ^ (sx >> 2);
                let c = colors[((line ^ (line >> 3)) as usize) % colors.len()];
                let px = if ((x / 2) ^ yy) % 7 == 0 { colors[1] } else { c };
                let s = y as usize * f.stride + x as usize * 4;
                f.px[s..s + 4].copy_from_slice(&px.to_le_bytes());
            }
        }
        // 红块由 `block` 决定（见结构体注释）：关掉它就是一场纯文本滚动。
        if self.block {
            let bx = (anim * 23) % (self.w.saturating_sub(96)).max(1);
            let by = (anim * 11) % (self.h.saturating_sub(64)).max(1);
            for y in by..(by + 64).min(self.h) {
                for x in bx..(bx + 96).min(self.w) {
                    let s = y as usize * f.stride + x as usize * 4;
                    f.px[s..s + 4].copy_from_slice(&0xffff_0000u32.to_le_bytes());
                }
            }
        }
        // 单帧大面积突变：整场会话只发生这一次，之后左半屏固定不变。
        //
        // 内容刻意做成**照片类**（逐像素渐变 + 轻微哈希噪声）：调色板编码器会拒绝、
        // 自动降级 PNG。真实屏幕上一次半屏切换就是这个量级，而平涂色每块只花几十字节，
        // 用它量不出用户报的"很多个图块慢慢覆盖"（实测 255 块 = 11.4 KB，同秒落地）。
        if self.step_at > 0 && idx >= self.step_at {
            let half = self.w / 2;
            for y in 0..self.h {
                for x in 0..half {
                    let s = y as usize * f.stride + x as usize * 4;
                    let r = ((x / 3 + y / 7) ^ (x * y / 61)) as u8;
                    let g = ((x * 2 + y) / 5) as u8;
                    let b = ((x ^ y) as u32 * 31 / 17) as u8;
                    let px = 0xff00_0000u32 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
                    f.px[s..s + 4].copy_from_slice(&px.to_le_bytes());
                }
            }
        }
        f
    }
}

/// 真实桌面下的光标验证（需要交互会话，故 #[ignore]）：
/// `cargo test -p ess-capture --release -- --ignored --nocapture`
///
/// 桌面随时在变（时钟、动画、光标闪烁），所以不能只比一帧：把光标停到屏幕中央，
/// 取 开/关/开/关 四次抓帧，要求两次开关差**都**命中的像素才算光标。内容变化
/// 不太可能在同一批像素上连续两次都撞车，光标则一定会。
#[cfg(windows)]
#[cfg(test)]
mod tests {
    use super::WindowsCapture;

    #[test]
    #[ignore]
    fn cursor_is_painted_into_the_frame() {
        let mut c = WindowsCapture::new().unwrap();
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(
                (c.w / 2) as i32,
                (c.h / 2) as i32,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        let (n, min, max) = c.cursor_footprint();
        let (bw, bh) = (max.0 - min.0 + 1, max.1 - min.1 + 1);
        println!("cursor pixels={n} bbox={min:?}..{max:?} size={bw}x{bh}");
        assert!(n > 50, "画光标这一步没有改动任何像素");
        assert!(bw <= 64 && bh <= 64, "改动区域 {bw}x{bh} 过大，不像一个光标");
        let (cx, cy) = ((c.w / 2) as usize, (c.h / 2) as usize);
        assert!(
            (min.0 as i64 - cx as i64).abs() <= 48 && (min.1 as i64 - cy as i64).abs() <= 48,
            "光标像素不在停放位置附近"
        );
    }
}

/// 诊断探针：打印采集画面里某个点的颜色（`ESS_PX=x,y`，默认画面中心）。
/// 用来验证 `WDA_EXCLUDEFROMCAPTURE` 真的生效：同一个坐标，被隐藏的窗口在采集里
/// 应该"不存在"（露出后面的桌面），而人眼在屏幕上仍然看得见它。
#[cfg(windows)]
#[cfg(test)]
mod probe {
    use super::WindowsCapture;

    #[test]
    #[ignore]
    fn dump_pixel() {
        let mut c = WindowsCapture::new().unwrap();
        let spec = std::env::var("ESS_PX").unwrap_or_default();
        let f = c.frame(0);
        let at = |x: usize, y: usize| {
            let i = y * f.stride + x * 4;
            &f.px[i..i + 4]
        };
        if spec.contains(';') {
            // 多点同帧采样：排除窗口前后的像素要落在同一帧里比才有意义。
            // ESS_PX="x,y;x,y;..."，同时打印一个窗口外的参考点判断桌面是否在动。
            for pair in spec.split(';') {
                let (x, y) = pair.split_once(',').unwrap();
                let (x, y) = (x.trim().parse::<usize>().unwrap(), y.trim().parse::<usize>().unwrap());
                println!("capture px=({x},{y}) BGRA={:?}", at(x, y));
            }
            return;
        }
        let (x, y) = match spec.split_once(',') {
            Some((a, b)) => (a.trim().parse::<usize>().unwrap(), b.trim().parse::<usize>().unwrap()),
            None => ((c.w / 2) as usize, (c.h / 2) as usize),
        };
        assert!(x < f.w as usize && y < f.h as usize, "ESS_PX 越界");
        println!(
            "capture {}x{} px=({},{}) BGRA={:?}",
            f.w,
            f.h,
            x,
            y,
            &f.px[y * f.stride + x * 4..y * f.stride + x * 4 + 4]
        );
    }
}
