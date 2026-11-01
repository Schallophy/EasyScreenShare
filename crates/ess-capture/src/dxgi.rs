//! DXGI 桌面复制（`IDXGIOutputDuplication`）采集通路。
//!
//! 存在的理由：GDI `BitBlt` 抓全屏在这台机器上是 31.5 fps 的硬上限（纯 BitBlt 循环
//! 与带拷贝/转换的 `frame()` 同速，也与 `CAPTUREBLT` 无关），60 fps 档不可能靠它做出来。
//! 桌面复制是 RustDesk / OBS 这类产品用的同一条路：帧由显示合成器推过来，节奏跟着
//! 刷新率走（本机 165 Hz），还额外给脏矩形和移动矩形。
//!
//! 已知代价：
//! - **桌面纹理里没有鼠标指针**，所以要走 `GetFramePointerShape` 单独合成——这条已经实现
//!   （见 `composite_pointer` 与 `pointers_composited`），活体测试是 `live_tests::duplication_paints_pointer`。
//! - 输出被拔掉/分辨率改变会让复制失效（`DXGI_ERROR_ACCESS_LOST`），这里把错误
//!   记在 `error` 上，由调用方决定是回落到 GDI 还是重建。
//! - 同一块输出**只允许一路复制**：同进程里再开第二路会让 `DupCapture::new()` 直接失败，
//!   所以两条 live 测试不能并行跑（`--test-threads=1` 或单独指定名字）。

use ess_core::frame::Frame;
use windows::core::Interface;
use windows::core::HRESULT;

/// `DXGI_ERROR_WAIT_TIMEOUT`（0x887A0027）。windows 0.62 把它藏在按 feature
/// 切分的模块里，直接写数值比猜 feature 更省事。
const DXGI_WAIT_TIMEOUT: HRESULT = HRESULT(0x887A_0027u32 as i32);
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// 门口龄分布的桶上界（毫秒）。最后一桶是"≥16 ms"，也就是 30 fps 周期的一半。
const AGE_EDGES: [f64; 4] = [1.0, 3.0, 8.0, 16.0];

/// 「一帧新画面在复制队列里等了多久才被我们取走」的累计读数。
///
/// 这是帧率上限的**相位代价**的直接量法：采集按截止网格睡觉，DWM 的 present 落在网格
/// 之间，于是每一帧都要在门口等 δ 毫秒。到达即发能省下的最多就是 δ，所以先量它。
#[derive(Default)]
pub struct AgeDiag {
    /// 取到新画面的次数（复用上一帧的不计）
    pub n: u64,
    pub sum_ms: f64,
    pub max_ms: f64,
    /// 驱动没填 `LastPresentTime` 的次数（远程会话/虚拟显示常见），这些不进分布
    pub no_stamp: u64,
    /// 与 `AGE_EDGES` 对齐的桶，最后一格是 ≥16 ms
    pub buckets: [u64; 5],
    freq: f64,
}

impl AgeDiag {
    /// QPC 频率（每秒计数），拿不到就报 0，`record` 会把这一帧归入「量不到」
    fn freq() -> f64 {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f) }.map(|_| f as f64).unwrap_or(0.0)
    }

    fn record(&mut self, lpt_qpc: i64) {
        self.n += 1;
        // 一些驱动把 present 时刻写成 0，那就只能报"量不到"而不是编一个 0 ms
        let mut now = 0i64;
        let usable = lpt_qpc != 0
            && self.freq > 0.0
            && unsafe { QueryPerformanceCounter(&mut now) }.is_ok();
        if !usable {
            self.no_stamp += 1;
            return;
        }
        let age = (now - lpt_qpc).max(0) as f64 / self.freq * 1000.0;
        self.sum_ms += age;
        self.max_ms = self.max_ms.max(age);
        let b = AGE_EDGES.iter().position(|e| age < *e).unwrap_or(AGE_EDGES.len());
        self.buckets[b] += 1;
    }

    /// 累计分布打到 stderr，`ESS_PERF=1` 的日志里 grep「门口龄」即可。数字是**从头累计**的，
    /// 所以最后一行就是整场跑的结论。
    fn report(&self) {
        let stamped = self.n - self.no_stamp;
        let mean = if stamped > 0 { self.sum_ms / stamped as f64 } else { 0.0 };
        eprintln!(
            "门口龄 n={} 均值 {:.2} ms 最大 {:.1} ms 分布 <1:{} 1-3:{} 3-8:{} 8-16:{} ≥16:{} 无present时刻:{} \
             帧龄上限=一个采集周期（30 fps 33.3 ms / 60 fps 16.7 ms）",
            self.n,
            mean,
            self.max_ms,
            self.buckets[0],
            self.buckets[1],
            self.buckets[2],
            self.buckets[3],
            self.buckets[4],
            self.no_stamp
        );
    }
}

/// 等一帧新画面的最长毫秒数。超时不是错误：静止桌面本来就很久没有新帧，
/// 这时把上一帧原样交回去，管线自己会比对出「没有变化」而不发任何东西。
const WAIT_MS: u32 = 2;

pub struct DupCapture {
    dup: IDXGIOutputDuplication,
    ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    pub w: u32,
    pub h: u32,
    stride: usize,
    last: Frame,
    pub error: Option<String>,
    /// 累计「没有新内容、直接复用上一帧」的次数（诊断用，不是错误）
    pub reused: u64,
    /// 最近一次合成进来的指针形状类型（1=单色 2=彩色 4=带掩码彩色），0=没有指针
    pub pointer_type: u32,
    pub pointers_composited: u64,
    /// 上一帧合成位置的左上角与尺寸，验收用来确认指针落在哪儿
    pub pointer_bbox: Option<(i32, i32, u32, u32)>,
    /// 指针形状只在「变化时」随帧给出；只移动不变化时 Position 有值而 buffer 为 0，
    /// 必须用缓存的上一份形状，否则指针会在移动后消失。
    shape_cache: Option<(Vec<u8>, DXGI_OUTDUPL_POINTER_SHAPE_INFO)>,
    /// 门口龄累计（`ESS_PERF` 打开时按 150 帧打印一次）
    pub age: AgeDiag,
    diag: bool,
}

// COM 接口指针在这里由本结构独占，采集只跑在一条线程上。
unsafe impl Send for DupCapture {}

impl DupCapture {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut ctx),
            )
            .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
            let device = device.ok_or("no device")?;
            let ctx = ctx.ok_or("no immediate context")?;

            let dxgi: IDXGIDevice = device.cast().map_err(|e| format!("cast IDXGIDevice: {e}"))?;
            let adapter = dxgi.GetAdapter().map_err(|e| format!("GetAdapter: {e}"))?;
            // 输出 0 = 桌面所在的主输出，和 GDI 通路的 GetDC(NULL) 视同一块屏
            let output = adapter.EnumOutputs(0).map_err(|e| format!("EnumOutputs: {e}"))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| format!("cast IDXGIOutput1: {e}"))?;
            let dup = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("DuplicateOutput: {e}"))?;

            // DXGI_OUTDUPL_DESC 只有 ModeDesc/Rotation/DesktopImageInSystemMemory，
            // 桌面尺寸与像素格式都从 ModeDesc 拿
            let d = dup.GetDesc();
            let w = d.ModeDesc.Width;
            let h = d.ModeDesc.Height;
            if w == 0 || h == 0 {
                return Err("duplication reports an empty desktop".into());
            }
            let stride = (w as usize) * 4;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w as u32,
                Height: h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: d.ModeDesc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| format!("CreateTexture2D: {e}"))?;
            let staging = staging.ok_or("no staging texture")?;

            let mut px = vec![0u8; stride * h as usize];
            // 第一帧先等一小会儿，让复制通道把当前桌面推过来
            let mut me = Self {
                dup,
                ctx,
                staging,
                w,
                h,
                stride,
                last: Frame { w, h, stride, px: std::mem::take(&mut px), ts_us: 0 },
                error: None,
                reused: 0,
                pointer_type: 0,
                pointers_composited: 0,
                pointer_bbox: None,
                shape_cache: None,
                age: AgeDiag {
                    freq: AgeDiag::freq(),
                    ..Default::default()
                },
                diag: std::env::var("ESS_PERF").is_ok(),
            };
            match me.grab(300) {
                Some(f) => me.last = f,
                None => me.error = me.error.take().or(Some("第一帧没拿到（超时或通道失效）".into())),
            }
            Ok(me)
        }
    }

    /// 取一帧像素。返回 None = 超时或通道失效；错误原因写进 `self.error`。
    pub(crate) fn grab(&mut self, wait_ms: u32) -> Option<Frame> {
        // 整段都在跟 COM 打交道，边界在这里收一次
        unsafe { self.grab_raw(wait_ms) }
    }

    unsafe fn grab_raw(&mut self, wait_ms: u32) -> Option<Frame> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        if let Err(e) = self.dup.AcquireNextFrame(wait_ms, &mut info, &mut resource) {
            if e.code() != DXGI_WAIT_TIMEOUT {
                self.error = Some(format!("AcquireNextFrame: {e}"));
            }
            return None;
        }
        // 取到新画面，先记它在复制队列里已经等了多久——这就是"节拍网格 vs present 时刻"
        // 的相位差，也是到达即发唯一能省下的东西
        if self.diag {
            self.age.record(info.LastPresentTime);
            if self.age.n % 150 == 0 {
                self.age.report();
            }
        }
        let out = (|| -> Option<Frame> {
            let tex: ID3D11Texture2D = resource?.cast().ok()?;
            self.ctx.CopyResource(&self.staging, &tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
            let mut px = vec![0u8; self.stride * self.h as usize];
            unsafe {
                let src = mapped.pData as *const u8;
                for y in 0..self.h as usize {
                    let s = std::slice::from_raw_parts(
                        src.add(y * mapped.RowPitch as usize),
                        self.stride,
                    );
                    px[y * self.stride..(y + 1) * self.stride].copy_from_slice(&s);
                }
            }
            self.ctx.Unmap(&self.staging, 0);

            // 指针形状必须在 ReleaseFrame 之前取；桌面纹理本身不含指针。
            if info.PointerShapeBufferSize > 0 && info.PointerPosition.Visible.0 != 0 {
                let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
                let mut need = 0u32;
                let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                if self
                    .dup
                    .GetFramePointerShape(
                        buf.len() as u32,
                        buf.as_mut_ptr() as *mut core::ffi::c_void,
                        &mut need,
                        &mut si,
                    )
                    .is_ok()
                    && need as usize <= buf.len()
                {
                    self.shape_cache = Some((buf, si));
                }
            }
            let visible = info.PointerPosition.Visible.0 != 0;
            if let (true, Some((buf, si))) = (visible, self.shape_cache.as_ref()) {
                let x = info.PointerPosition.Position.x - si.HotSpot.x;
                let y = info.PointerPosition.Position.y - si.HotSpot.y;
                let n = composite_pointer(&mut px, self.stride, self.w, self.h, x, y, buf, si);
                self.pointer_type = si.Type;
                // bbox 只在真的写上了像素时记录，否则验收测试会被"形状收到了但没画"骗过去
                match (n, shape_layout(buf.len(), si)) {
                    (1.., Some((vh, _, _))) => {
                        self.pointers_composited += 1;
                        self.pointer_bbox = Some((x, y, si.Width, vh.max(0) as u32));
                    }
                    _ => self.pointer_bbox = None,
                }
            } else if !visible {
                self.pointer_bbox = None;
            }
            Some(Frame { w: self.w, h: self.h, stride: self.stride, px, ts_us: 0 })
        })();
        let _ = self.dup.ReleaseFrame();
        out
    }

    pub fn frame(&mut self, ts_us: u64) -> Frame {
        match self.grab(WAIT_MS) {
            Some(mut f) => {
                f.ts_us = ts_us;
                self.last = f.clone();
                f
            }
            None => {
                self.reused += 1;
                let mut f = self.last.clone();
                f.ts_us = ts_us;
                f
            }
        }
    }
}

/// 把 DXGI 交来的指针形状画进 BGRA 缓冲。三种类型：
/// - 单色（MONOCHROME）：1 bpp，前半 AND、后半 XOR，`dst = (dst & and) ^ xor`
/// - 彩色（COLOR）：32 bpp **预乘** alpha，`dst = src + dst * (1 - a)`
/// - 带掩码彩色（MASKED_COLOR）：32 bpp 彩色 + 顶部若干行 1 bpp AND 掩码（掩码位 1 = 透明）
///
/// 越界、坏 pitch、缓冲区不足都只裁掉多余部分，不能 panic：采集线程上的一次
/// panic 就是整个共享会话没了。
/// 从形状缓冲区推出「可见高度」与各平面偏移。
/// 返回 `(可见高度, XOR 平面偏移, 彩色像素偏移)`；数据不足时 None。
///
/// 单色形状要 AND + XOR 两个平面，掩码彩色要掩码 + 彩色两段，而 Windows 报上来的
/// `Height` 有时是整个缓冲区的行数、有时是可见高度（本机同一个箭头先后报过 32 和 64），
/// 所以按缓冲区实际装得下多少行来判，不能信字面值。
fn shape_layout(
    shape_len: usize,
    info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
) -> Option<(i32, usize, usize)> {
    let pitch = info.Pitch as usize;
    if info.Width == 0 || info.Height == 0 || pitch == 0 || shape_len < pitch {
        return None;
    }
    let rows_have = shape_len / pitch;
    let mono = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
    let masked = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
    let color = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
    if !(mono || masked || color) {
        return None;
    }
    if mono {
        if rows_have >= 2 * info.Height as usize {
            Some((info.Height as i32, pitch * info.Height as usize, 0))
        } else if info.Height % 2 == 0 && rows_have >= info.Height as usize {
            let half = (info.Height / 2) as usize;
            Some((half as i32, pitch * half, 0))
        } else {
            None
        }
    } else if masked {
        if rows_have >= 2 * info.Height as usize {
            Some((info.Height as i32, 0, pitch * info.Height as usize))
        } else {
            None
        }
    } else if rows_have >= info.Height as usize {
        Some((info.Height as i32, 0, 0))
    } else {
        None
    }
}

pub fn composite_pointer(
    px: &mut [u8],
    stride: usize,
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    shape: &[u8],
    info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
) -> usize {
    let sw = info.Width as i32;
    let pitch = info.Pitch as usize;
    let (sh, xor_off, rgb_off) = match shape_layout(shape.len(), info) {
        Some(v) => v,
        None => return 0,
    };
    let mono = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
    let masked = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;

    let mut written = 0usize;
    for row in 0..sh {
        let dy = y + row;
        if dy < 0 || dy >= h as i32 {
            continue;
        }
        for col in 0..sw {
            let dx = x + col;
            if dx < 0 || dx >= w as i32 {
                continue;
            }
            let o = dy as usize * stride + dx as usize * 4;
            if o + 4 > px.len() {
                continue;
            }
            let bit = (col / 8) as usize;
            let mask = 0x80u8 >> (col % 8);
            if mono {
                let zeroed = shape[row as usize * pitch + bit] & mask == 0;
                let flipped = shape[xor_off + row as usize * pitch + bit] & mask != 0;
                for c in 0..3 {
                    let v = if zeroed { 0 } else { px[o + c] };
                    px[o + c] = if flipped { 255 - v } else { v };
                }
                px[o + 3] = 255;
                written += 1;
                continue;
            }
            let base = rgb_off + row as usize * pitch + (col as usize) * 4;
            if base + 4 > shape.len() {
                continue;
            }
            let (b, g, r, a) = (shape[base], shape[base + 1], shape[base + 2], shape[base + 3]);
            if masked {
                // AND 掩码位为 1 = 透明，保留底下的桌面像素
                if shape[row as usize * pitch + bit] & mask != 0 {
                    continue;
                }
                px[o..o + 4].copy_from_slice(&[b, g, r, 255]);
                written += 1;
                continue;
            }
            if a == 0 {
                continue;
            }
            if a == 255 {
                px[o..o + 4].copy_from_slice(&[b, g, r, 255]);
                written += 1;
                continue;
            }
            // 预乘 alpha：out = src + dst * (1 - a)，src 已经乘过不用再乘
            let inv = (255 - a) as u32;
            for c in 0..3 {
                let v = shape[base + c] as u32 + (px[o + c] as u32 * inv) / 255;
                px[o + c] = v.min(255) as u8;
            }
            px[o + 3] = 255;
            written += 1;
        }
    }
    written
}

#[cfg(test)]
mod pointer_tests {
    use super::*;

    fn si(type_: u32, w: u32, h: u32, pitch: u32) -> DXGI_OUTDUPL_POINTER_SHAPE_INFO {
        DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: type_,
            Width: w,
            Height: h,
            Pitch: pitch,
            HotSpot: windows::Win32::Foundation::POINT { x: 0, y: 0 },
        }
    }

    fn frame4(w: u32, h: u32, fill: [u8; 4]) -> (Vec<u8>, usize) {
        let stride = (w as usize) * 4;
        let mut px = vec![0u8; stride * h as usize];
        for i in 0..(w * h) as usize {
            px[i * 4..i * 4 + 4].copy_from_slice(&fill);
        }
        (px, stride)
    }

    #[test]
    fn color_shape_writes_opaque_pixels_and_blends_partial_alpha() {
        let (mut px, stride) = frame4(4, 1, [10, 20, 30, 255]);
        // 4x1 预乘彩色：p0 全不透明、p1 半透明(a=127)、p2 全透明、p3 越界数据无所谓
        let shape = vec![
            200, 0, 0, 255, // p0 B=200
            50, 0, 0, 127, // p1 预乘半透明
            0, 0, 0, 0, //   p2 透明 → 保持桌面
            0, 200, 0, 255, // p3 G=200
        ];
        composite_pointer(
            &mut px,
            stride,
            4,
            1,
            0,
            0,
            &shape,
            &si(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32, 4, 1, 16),
        );
        assert_eq!(&px[0..4], &[200, 0, 0, 255], "不透明像素应直接落色");
        // p1: 50 + 10*(128/255) ≈ 55, G: 20*0.5 ≈ 10
        assert!(px[4] > 50 && px[4] < 62, "预乘半透明应叠加在桌面上: {}", px[4]);
        assert_eq!(&px[8..12], &[10, 20, 30, 255], "a=0 的像素不该动桌面");
        assert_eq!(&px[12..16], &[0, 200, 0, 255]);
    }

    #[test]
    fn monochrome_shape_uses_and_then_xor_masks() {
        let (mut px, stride) = frame4(8, 1, [70, 70, 70, 255]);
        // AND 行：bit7=0 → 涂黑；XOR 行：bit6=1 → 反色；其余保持
        // pitch=1：AND 行在前 1 字节，XOR 行紧跟其后（偏移 = Pitch * Height）
        let shape = vec![0b0111_1111u8, 0b1011_1111u8];
        composite_pointer(
            &mut px,
            stride,
            4,
            1,
            0,
            0,
            &shape,
            &si(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32, 8, 1, 1),
        );
        // p0：AND bit7=0 → 涂黑，XOR bit7=1 → 再反色 → 白
        assert_eq!(&px[0..3], &[255, 255, 255], "AND=0 且 XOR=1 应该是白");
        // p1：AND bit6=1（不清零）、XOR bit6=0（不反色）→ 原样保留
        assert_eq!(&px[4..7], &[70, 70, 70]);
    }

    #[test]
    fn hotspot_offset_and_clipping_do_not_panic_or_spill() {
        let (mut px, stride) = frame4(4, 4, [1, 2, 3, 255]);
        let mut shape = vec![0u8; 16 * 4]; // 4x4 全不透明蓝
        for i in 0..16 {
            shape[i * 4] = 200;
            shape[i * 4 + 3] = 255;
        }
        // 放在右上角之外：只有左下 2x2 落进画面
        composite_pointer(
            &mut px,
            stride,
            4,
            4,
            2,
            2,
            &shape,
            &si(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32, 4, 4, 16),
        );
        assert_eq!(&px[2 * stride + 8..2 * stride + 12], &[200, 0, 0, 255]);
        assert_eq!(&px[0..4], &[1, 2, 3, 255], "画面外不应回头污染首行");
        // 缓冲区比声明的形状小：必须原样返回，不能越界读
        let before = px.clone();
        composite_pointer(
            &mut px,
            stride,
            4,
            4,
            0,
            0,
            &[0u8; 8],
            &si(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32, 4, 4, 16),
        );
        assert_eq!(px, before, "形状数据不足时应整体放弃");
    }

    #[test]
    fn monochrome_height_may_count_both_planes() {
        // 本机实测：同一个单色箭头，一次报 Height=32（含两个平面）一次报 64。
        // 缓冲区只有 Height 行时，可见高度必须取一半，否则判成数据不足、指针不画。
        let (mut px, stride) = frame4(8, 4, [40, 40, 40, 255]);
        let shape = vec![0b0111_1111u8, 0b0111_1111, 0b1011_1111, 0b1011_1111];
        let n = composite_pointer(
            &mut px,
            stride,
            8,
            4,
            0,
            0,
            &shape,
            &si(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32, 8, 4, 1),
        );
        assert!(n >= 8, "Height 含两个平面时也要画出指针，实际写了 {n} 像素");
        assert_eq!(&px[0..3], &[255, 255, 255], "AND=0 且 XOR=1 应该是白");
        assert_eq!(&px[4..7], &[40, 40, 40], "AND=1、XOR=0 应原样保留");
        // 同一份布局下 shape_layout 报的可见高度也必须是 2 而不是 4
        let (vh, _, _) = shape_layout(shape.len(), &si(1, 8, 4, 1)).unwrap();
        assert_eq!(vh, 2, "可见高度应取 Height 的一半");
    }
}

/// 真机验证：桌面复制通路确实把指针画进了帧里，而且位置跟着光标走。
/// **单独跑**（一块输出只允许一路复制，同进程并行会抢）：
/// `cargo test -p ess-capture --release -- --ignored --nocapture --test-threads=1 duplication_paints_pointer`
#[cfg(windows)]
#[cfg(test)]
mod live_tests {
    use super::DupCapture;
    // SetCursorPos 走 windows-sys（windows crate 没开 UI feature，不为此加依赖）
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

    #[test]
    #[ignore]
    fn duplication_paints_pointer() {
        let mut c = DupCapture::new().unwrap();
        let (cx, cy) = ((c.w / 2) as i32, (c.h / 2) as i32);
        // 复制通路只在「指针有变化」的帧里带形状/位置，所以要真的动一下鼠标。
        // SetCursorPos 是程序化移动，触控抑制（CURSOR_SUPPRESSED）状态下不会把指针
        // 重新点亮，必须发一次真实的 MOUSEEVENTF_MOVE。
        unsafe {
            let _ = SetCursorPos(cx - 30, cy - 30);
            windows_sys::Win32::UI::Input::KeyboardAndMouse::mouse_event(
                windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                30,
                30,
                0,
                0,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
        let t0 = std::time::Instant::now();
        while c.pointer_bbox.is_none() && t0.elapsed() < std::time::Duration::from_secs(4) {
            c.frame(0);
            unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::mouse_event(
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                    if (t0.elapsed().as_millis() / 100) % 2 == 0 { 3 } else { -3 },
                    0,
                    0,
                    0,
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        println!(
            "pointer_type={} composited={} bbox={:?} reused={}",
            c.pointer_type, c.pointers_composited, c.pointer_bbox, c.reused
        );
        let (x, y, w, h) = c
            .pointer_bbox
            .expect("桌面复制一直没交出指针形状：可能是触控抑制状态或系统未合成指针");
        assert!(
            (x - cx).abs() <= 4 && (y - cy).abs() <= 4,
            "指针不在停放位置：bbox=({x},{y}) 期望≈({cx},{cy})"
        );
        assert!(w > 0 && w <= 64 && h > 0 && h <= 64, "形状尺寸异常 {w}x{h}");
        assert!(
            matches!(c.pointer_type, 1 | 2 | 4),
            "未知的指针形状类型 {}",
            c.pointer_type
        );
        // 光有 bbox 不能证明真的画上了：把光标移开后再抓一帧，比较原位置的 32x32 区域。
        // 静止桌面下如果指针确实被合成过，这块像素一定会变回去。
        let f1 = c.frame(1);
        let (ax, ay) = (x.max(0) as usize, y.max(0) as usize);
        unsafe {
            let _ = SetCursorPos(cx + 400, cy + 300);
            windows_sys::Win32::UI::Input::KeyboardAndMouse::mouse_event(
                windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                0,
                0,
                0,
                0,
            );
        }
        let mut f2 = f1.clone();
        let t1 = std::time::Instant::now();
        while t1.elapsed() < std::time::Duration::from_millis(600) {
            f2 = c.frame(2);
        }
        let mut changed = 0usize;
        for ry in ay..(ay + h as usize).min(f1.h as usize) {
            for rx in ax..(ax + w as usize).min(f1.w as usize) {
                let i = ry * f1.stride + rx * 4;
                if f1.px[i..i + 4] != f2.px[i..i + 4] {
                    changed += 1;
                }
            }
        }
        println!(
            "指针移开后原区域变化像素={changed}（形状 {w}x{h} @{ax},{ay}），composited={}",
            c.pointers_composited
        );
        assert!(changed > 20, "指针区域像素没有变化：形状实际上没写进帧里");
        if let Some(e) = &c.error {
            panic!("复制通道报错: {e}");
        }
    }

    #[test]
    #[ignore]
    fn duplication_dump_pixel() {
        // 与 GDI 侧的 dump_pixel 同一测法，只是换到桌面复制通路：
        // WDA_EXCLUDEFROMCAPTURE 是否真的对复制 API 也生效，要各通路分别实测。
        let mut c = DupCapture::new().unwrap();
        let spec = std::env::var("ESS_PX").unwrap_or_default();
        let (x, y) = match spec.split_once(',') {
            Some((a, b)) => (a.trim().parse::<usize>().unwrap(), b.trim().parse::<usize>().unwrap()),
            None => ((c.w / 2) as usize, (c.h / 2) as usize),
        };
        // 静止桌面下 AcquireNextFrame 会超时并复用上一帧，动一下鼠标逼出真帧。
        let mut f = c.frame(0);
        for i in 1..40 {
            unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::mouse_event(
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                    if i % 2 == 0 { 2 } else { -2 },
                    0,
                    0,
                    0,
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            f = c.frame(i);
        }
        assert!(x < f.w as usize && y < f.h as usize, "ESS_PX 越界");
        let i = y * f.stride + x * 4;
        println!(
            "dup capture {}x{} px=({},{}) BGRA={:?} reused={} err={:?}",
            f.w, f.h, x, y, &f.px[i..i + 4], c.reused, c.error
        );
    }
}
