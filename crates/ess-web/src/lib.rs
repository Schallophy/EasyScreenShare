//! 浏览器观看端：把已验证的接收管线编译成 WASM。
//!
//! 裸 C ABI（无 wasm-bindgen）：JS 只负责 LiveKit 连接、喂字节、画布绘制。
//! 版本判定、滚动组原子应用、整帧重建、分片重组全部在 Rust 侧，与原生
//! `ess-view` 共用同一套代码，避免"浏览器另写一份状态机"带来的语义漂移。

use std::collections::HashMap;

use ess_codec_palette::PaletteZstdCodec;
use ess_codec_png::PngCodec;
use ess_core::codec::TileCodec;
use ess_core::model::Message;
use ess_core::pipeline::{CodecSet, RecvPipeline};
use ess_core::wire::{self, WireMsg};

/// 与 ess-transport 相同的分片重组（这里不能依赖 ess-transport：它带 LiveKit）。
#[derive(Default)]
struct Assembler {
    partial: HashMap<u32, (u32, Vec<Option<Vec<u8>>>)>,
}

impl Assembler {
    fn push(&mut self, msg_id: u32, idx: u32, total: u32, data: Vec<u8>) -> Option<Vec<u8>> {
        if idx >= total || total == 0 || total as usize > 10_000 {
            return None;
        }
        let entry = self
            .partial
            .entry(msg_id)
            .or_insert_with(|| (total, vec![None; total as usize]));
        if entry.0 != total {
            self.partial.remove(&msg_id);
            return None;
        }
        entry.1[idx as usize] = Some(data);
        if !entry.1.iter().all(|p| p.is_some()) {
            return None;
        }
        let (_, parts) = self.partial.remove(&msg_id)?;
        let mut out = Vec::new();
        for p in parts {
            out.extend_from_slice(&p.unwrap());
        }
        Some(out)
    }
}

pub struct Viewer {
    recv: RecvPipeline<'static>,
    asm: Assembler,
    serial: u32,
    chunks_seen: u32,
    app_ok: u32,
    app_err: u32,
    decode_err: u32,
    upd_msgs: u32,
    full_msgs: u32,
    scroll_msgs: u32,
    state_msgs: u32,
}

impl Viewer {
    fn new() -> Self {
        // 一次性泄漏两个编码器：一个页面只有一个 Viewer，生命周期 = 页面。
        let codecs: &'static [&'static dyn TileCodec] = Box::leak(
            vec![
                Box::leak(Box::new(PaletteZstdCodec::default())) as &'static dyn TileCodec,
                Box::leak(Box::new(PngCodec)) as &'static dyn TileCodec,
            ]
            .into_boxed_slice(),
        );
        Self {
            recv: RecvPipeline::new(CodecSet { codecs }),
            asm: Assembler::default(),
            serial: 0,
            chunks_seen: 0,
            app_ok: 0,
            app_err: 0,
            decode_err: 0,
            upd_msgs: 0,
            full_msgs: 0,
            scroll_msgs: 0,
            state_msgs: 0,
        }
    }

    fn progress(&self) -> (u64, u64, u64, u64) {
        let s = &self.recv.stats;
        (s.applied_tiles, s.scrolls_applied, s.full_frames, s.motion_frames)
    }

    /// 返回：0 正常（含等待分片）、-1 线格式错误、-2 解码错误。
    fn feed(&mut self, bytes: &[u8]) -> i32 {
        let Ok(msg) = wire::decode(bytes) else {
            self.decode_err += 1;
            return -1;
        };
        let msg = match msg {
            WireMsg::Chunk { msg_id, idx, total, data } => {
                self.chunks_seen += 1;
                let Some(raw) = self.asm.push(msg_id, idx, total, data) else {
                    return 0;
                };
                match wire::decode(&raw) {
                    Ok(m) => m,
                    Err(_) => {
                        self.decode_err += 1;
                        return -1;
                    }
                }
            }
            m => m,
        };
        match &msg {
            WireMsg::Update { .. } => self.upd_msgs += 1,
            WireMsg::Full { .. } => self.full_msgs += 1,
            WireMsg::Scroll { .. } => self.scroll_msgs += 1,
            WireMsg::State { .. } => self.state_msgs += 1,
            _ => {}
        }
        let before = self.progress();
        match self.recv.on_wire(&msg) {
            Ok(()) => {
                self.app_ok += 1;
                if self.progress() != before {
                    self.serial = self.serial.wrapping_add(1);
                }
                0
            }
            Err(_) => {
                self.app_err += 1;
                -2
            }
        }
    }

    fn report(&self, out: &mut [u8]) -> i32 {
        let Message::Report { layout_id, versions, epochs } = self.recv.report() else {
            return -1;
        };
        let msg = WireMsg::State {
            layout_id,
            cols: self.recv.rx.grid.cols,
            rows: self.recv.rx.grid.rows,
            versions,
            epochs,
        };
        match wire::encode(&msg) {
            Ok(b) if b.len() <= out.len() => {
                out[..b.len()].copy_from_slice(&b);
                b.len() as i32
            }
            _ => -1,
        }
    }
}

#[no_mangle]
pub extern "C" fn ess_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr`/`len` 必须来自同一次 `ess_alloc`。
#[no_mangle]
pub unsafe extern "C" fn ess_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, 0, len));
    }
}

#[no_mangle]
pub extern "C" fn ess_viewer_new() -> *mut Viewer {
    Box::into_raw(Box::new(Viewer::new()))
}

/// # Safety
/// `p` 必须来自 `ess_viewer_new` 且只释放一次。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_free(p: *mut Viewer) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

/// # Safety
/// `p` 有效；`data`/`len` 指向可读内存。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_feed(p: *mut Viewer, data: *const u8, len: usize) -> i32 {
    if p.is_null() || (data.is_null() && len > 0) {
        return -1;
    }
    let v = &mut *p;
    let bytes = std::slice::from_raw_parts(data, len);
    v.feed(bytes)
}

/// # Safety
/// `p` 有效。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_canvas(p: *const Viewer) -> *const u8 {
    if p.is_null() {
        return std::ptr::null();
    }
    (*p).recv.canvas.px.as_ptr()
}

/// # Safety
/// `p` 有效。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_width(p: *const Viewer) -> u32 {
    if p.is_null() {
        return 0;
    }
    (*p).recv.canvas.w
}

/// # Safety
/// `p` 有效。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_height(p: *const Viewer) -> u32 {
    if p.is_null() {
        return 0;
    }
    (*p).recv.canvas.h
}

/// 画布代数：像素每变一次 +1，JS 据此跳过重复绘制。
///
/// # Safety
/// `p` 有效。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_serial(p: *const Viewer) -> u32 {
    if p.is_null() {
        return 0;
    }
    (*p).serial
}

/// 当前画面由哪个平面出：1 = 视频轨（`<video>`），0 = 图块画布。
///
/// # Safety
/// `p` 有效。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_video_mode(p: *const Viewer) -> u32 {
    if p.is_null() {
        return 0;
    }
    (*p).recv.video_mode() as u32
}

/// 视频轨就绪与否：JS 在 TrackSubscribed 后传 1、断开传 0。
///
/// 只有真的能显示视频轨时才忽略 JPEG 兜底帧，否则刚宣告切平面、轨道还没
/// 订阅上时画布会停在旧画面上。
///
/// # Safety
/// `p` 有效。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_video_ready(p: *mut Viewer, ready: u32) {
    if p.is_null() {
        return;
    }
    (*p).recv.accept_video_plane = ready != 0;
}

/// 诊断计数：[chunks, applied_ok, applied_err, decode_err, serial, layout_id]。
///
/// # Safety
/// `p` 有效；`out`/`cap` 指向可写内存。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_diag(p: *const Viewer, out: *mut u32, cap: usize) -> usize {
    if p.is_null() || out.is_null() {
        return 0;
    }
    let v = &*p;
    let vals = [
        v.chunks_seen,
        v.app_ok,
        v.app_err,
        v.decode_err,
        v.serial,
        v.recv.rx.layout_id,
        v.upd_msgs,
        v.full_msgs,
        v.scroll_msgs,
        v.state_msgs,
        v.recv.canvas.w,
        v.recv.canvas.h,
        v.recv.rx.grid.cols as u32,
        v.recv.rx.grid.rows as u32,
        v.recv.full_layout(),
        v.recv.video_mode() as u32,
        v.recv.stats.motion_skipped as u32,
    ];
    let n = vals.len().min(cap);
    std::ptr::copy_nonoverlapping(vals.as_ptr(), out, n);
    n
}

/// 生成 500 ms 周期上行状态报告，返回写入字节数（≤0 表示跳过）。
///
/// # Safety
/// `p` 有效；`out`/`cap` 指向可写内存。
#[no_mangle]
pub unsafe extern "C" fn ess_viewer_report(p: *const Viewer, out: *mut u8, cap: usize) -> i32 {
    if p.is_null() || out.is_null() || cap == 0 {
        return -1;
    }
    let v = &*p;
    let buf = std::slice::from_raw_parts_mut(out, cap);
    v.report(buf)
}
