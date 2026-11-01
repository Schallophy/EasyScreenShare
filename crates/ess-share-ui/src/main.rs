//! 分享端图形外壳：Tauri 窗口 + 内嵌共享会话。
//!
//! 会话跑在独立线程的 tokio 运行时里（与 `ess-share` 完全相同的 `ess_session::run`），
//! UI 通过事件 `ess-phase` / `ess-stats` / `ess-log` 更新；命令只做启停。
//!
//! 验收用：`ess-share-ui.exe --autostart --demo` 会跳过点击直接开播。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use ess_session::{Config, Event};
use tauri::{AppHandle, Emitter, Manager, State};

struct AppState {
    /// 正在运行的会话的停止开关；None = 空闲
    session: Mutex<Option<Arc<AtomicBool>>>,
}

/// `--autostart` 这一轮实际生效的参数，前端启动时取回去回填表单。
/// 不回填的话，命令行说 60 fps、表单还写着 10，验收时会误判。
#[derive(Clone, Serialize)]
struct StartArgs {
    server: String,
    room: String,
    identity: String,
    fps: u32,
    demo: bool,
    video_w: u32,
    video_h: u32,
    video_bitrate_mbps: u32,
    audio: bool,
    /// 产品档：clear / balanced / smooth（§13.1 画质·流畅度）
    mode: String,
}

static AUTOSTART: std::sync::OnceLock<StartArgs> = std::sync::OnceLock::new();

#[tauri::command]
fn start_args() -> Option<StartArgs> {
    AUTOSTART.get().cloned()
}

#[derive(Clone, Serialize)]
struct Phase {
    phase: String,
    detail: Option<String>,
}

#[derive(Clone, Serialize)]
struct Stats {
    elapsed_s: u64,
    frames: u64,
    tiles: u64,
    msgs: u64,
    bytes: u64,
    video_frames: u64,
    video_mode: bool,
    video_track: bool,
    /// 系统声音："共享中 · 已送 N 块（10 ms/块）" / "未共享" / "关"
    audio: String,
    /// 拥塞控制估计的可用上行，如 "5.6 Mbps"
    link_up: String,
    /// 编码器实际输出，如 "1920x1080@60"
    link_enc: String,
    /// 受限原因（空串 = 不受限）
    link_limited: String,
}

fn phase(app: &AppHandle, name: &str, detail: Option<String>) {
    let _ = app.emit("ess-phase", Phase { phase: name.into(), detail });
}

fn spawn_session(app: AppHandle, mut cfg: Config) -> Result<(), String> {
    // 一步就绪：填的是回环地址 = 这台机器自己当服务端。填了远端地址一律不动，
    // 既不去抢别人的端口，也不在本机多起一个没人用的服务。
    if ess_app::self_hosted(&cfg.url) {
        let (room, key, secret) = (cfg.room.clone(), cfg.key.clone(), cfg.secret.clone());
        let reporter = app.clone();
        match ess_app::bring_up(&room, &key, &secret, &mut |m| {
            let _ = reporter.emit("ess-log", m);
        }) {
            Ok(r) => {
                cfg.url = r.ws_url.clone();
                let _ = app.emit("ess-viewer", ViewerLink { url: r.viewer_url.clone(), room: room.clone() });
                let _ = app.emit(
                    "ess-log",
                    format!("观看端在同一局域网打开 {} 即可（房间 {room}，免安装）", r.viewer_url),
                );
            }
            Err(e) => {
                let _ = app.emit("ess-log", format!("一键就绪失败：{e}（共享按填好的地址继续）"));
            }
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let st = app.state::<AppState>();
        let mut slot = st.session.lock().unwrap();
        if slot.is_some() {
            return Err("已经在共享中".into());
        }
        *slot = Some(stop.clone());
    }
    phase(&app, "connecting", Some(cfg.url.clone()));
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                phase(&app, "error", Some(format!("运行时初始化失败: {e}")));
                *app.state::<AppState>().session.lock().unwrap() = None;
                return;
            }
        };
        let ev_app = app.clone();
        let res = rt.block_on(ess_session::run(cfg, stop, move |ev| match ev {
            Event::Connected { width, height } => {
                phase(&ev_app, "live", Some(format!("{width}x{height}")));
            }
            Event::Stats(s) => {
                let _ = ev_app.emit(
                    "ess-stats",
                    Stats {
                        elapsed_s: s.elapsed_s,
                        frames: s.frames,
                        tiles: s.tiles,
                        msgs: s.msgs,
                        bytes: s.bytes,
                        video_frames: s.video_frames,
                        video_mode: s.video_mode,
                        video_track: s.video_track,
                        audio: if !s.audio_track {
                            "未共享".into()
                        } else if s.audio_frames == 0 {
                            "轨道已建 · 0 块（无输出设备/静音路径异常）".into()
                        } else {
                            format!(
                                "共享中 · 已送 {} 块 · {:.0} kbps",
                                s.audio_frames,
                                s.link.map(|l| l.audio_egress_bps / 1e3).unwrap_or(0.0)
                            )
                        },
                        link_up: s
                            .link
                            .map(|l| format!("{:.1} Mbps", l.avail_bps / 1e6))
                            .unwrap_or_else(|| "—".into()),
                        link_enc: s
                            .link
                            .filter(|l| l.enc_w > 0)
                            .map(|l| format!("{}x{}@{:.0}", l.enc_w, l.enc_h, l.enc_fps))
                            .unwrap_or_else(|| "—".into()),
                        link_limited: match s.link {
                            Some(l) if l.bw_limited => "带宽受限".into(),
                            Some(l) if l.cpu_limited => "算力受限".into(),
                            _ => String::new(),
                        },
                    },
                );
            }
            Event::Log(m) => {
                let _ = ev_app.emit("ess-log", m);
            }
            Event::Error(e) => {
                let _ = ev_app.emit("ess-log", format!("错误: {e}"));
            }
        }));
        *app.state::<AppState>().session.lock().unwrap() = None;
        match res {
            Ok(()) => phase(&app, "idle", None),
            Err(e) => phase(&app, "error", Some(e)),
        }
    });
    Ok(())
}

#[tauri::command]
fn start_share(
    app: AppHandle,
    server: String,
    room: String,
    identity: String,
    key: String,
    secret: String,
    fps: u32,
    demo: bool,
    // 0 = 自动档（分辨率跟随屏幕 / 码率按像素 x 帧率定）
    video_w: u32,
    video_h: u32,
    video_bitrate_mbps: u32,
    audio: bool,
    mode: String,
) -> Result<(), String> {
    let cfg = Config {
        url: server,
        room,
        identity,
        key,
        secret,
        // 帧率上限 60：实测 GDI 在 50 fps 内精确达标，60 fps 由桌面复制通路承担
        fps: fps.clamp(1, 60),
        demo,
        video_max: if video_w > 0 && video_h > 0 {
            Some((video_w, video_h))
        } else {
            None
        },
        video_bitrate: if video_bitrate_mbps > 0 {
            Some(video_bitrate_mbps as u64 * 1_000_000)
        } else {
            None
        },
        audio,
        mode: parse_mode(&mode),
    };
    spawn_session(app, cfg)
}

/// 表单/命令行传来的档位字符串。不认识时用默认档，但要说出来——这一项直接决定
/// 帧率与画质的取舍，静默改档比报错更糟。
fn parse_mode(s: &str) -> ess_session::ProductMode {
    match ess_session::ProductMode::parse(s) {
        Some(m) => m,
        None => {
            eprintln!("mode={s:?} 不认识，用默认档 balanced");
            Default::default()
        }
    }
}

#[tauri::command]
fn stop_share(app: AppHandle, state: State<'_, AppState>) {
    if let Some(stop) = state.session.lock().unwrap().as_ref() {
        stop.store(true, Ordering::Relaxed);
        phase(&app, "stopping", None);
    }
}

#[tauri::command]
fn session_active(state: State<'_, AppState>) -> bool {
    state.session.lock().unwrap().is_some()
}

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// 让本进程自己的窗口从抓屏里消失（Windows 10 2004+）。
///
/// 全屏采集会把分享端的控制窗摄进画面——观看方看到一个「正在共享」的浮窗跟着鼠标
/// 走，这是产品级验收不接受的。`WDA_EXCLUDEFROMCAPTURE` 让窗口照常显示在屏幕上，
/// 但对所有采集 API（GDI BitBlt、桌面复制、WGC）透明。手写 user32 声明，不为此加依赖。
#[cfg(windows)]
fn hide_from_capture(app: &tauri::App) {
    #[link(name = "user32")]
    extern "system" {
        fn SetWindowDisplayAffinity(hwnd: *mut core::ffi::c_void, affinity: u32) -> i32;
    }
    const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;
    if std::env::var("ESS_HIDE_SELF").map(|v| v == "0").unwrap_or(false) {
        eprintln!("ESS_HIDE_SELF=0：控制窗仍会被摄进画面");
        return;
    }
    let wins = app.webview_windows();
    if wins.is_empty() {
        eprintln!("hide_from_capture: setup 阶段还没有窗口，控制窗可能被摄进画面");
        return;
    }
    for (label, w) in wins {
        match w.hwnd() {
            Ok(hwnd) => {
                let raw = hwnd.0 as *mut core::ffi::c_void;
                let ok = unsafe { SetWindowDisplayAffinity(raw, WDA_EXCLUDEFROMCAPTURE) };
                eprintln!(
                    "控制窗 {label} 从采集里隐藏: {}",
                    if ok != 0 { "ok" } else { "失败（系统不支持？）" }
                );
            }
            Err(e) => eprintln!("取 {label} 的 hwnd 失败: {e}"),
        }
    }
}

#[derive(Clone, serde::Serialize)]
struct ViewerLink {
    url: String,
    room: String,
}

fn main() {
    tauri::Builder::default()
        .manage(AppState { session: Mutex::new(None) })
        .invoke_handler(tauri::generate_handler![
            start_share,
            stop_share,
            session_active,
            start_args
        ])
        .setup(|app| {
            hide_from_capture(app);
            let args: Vec<String> = std::env::args().collect();
            if args.iter().any(|a| a == "--autostart") {
                let video_w: u32 = arg(&args, "--video-w", "0").parse().unwrap_or(0);
                let video_h: u32 = arg(&args, "--video-h", "0").parse().unwrap_or(0);
                let bitrate: u64 = arg(&args, "--video-bitrate", "0").parse().unwrap_or(0);
                let audio = !args.iter().any(|a| a == "--no-audio");
                let cfg = Config {
                    url: arg(&args, "--url", "ws://127.0.0.1:7880"),
                    room: arg(&args, "--room", "demo"),
                    identity: arg(&args, "--identity", "share"),
                    key: arg(&args, "--key", "devkey"),
                    secret: arg(&args, "--secret", "secret"),
                    fps: arg(&args, "--fps", "30").parse().unwrap_or(30),
                    demo: args.iter().any(|a| a == "--demo"),
                    video_max: if video_w > 0 && video_h > 0 {
                        Some((video_w, video_h))
                    } else {
                        None
                    },
                    video_bitrate: if bitrate > 0 { Some(bitrate) } else { None },
                    audio,
                    mode: parse_mode(&arg(&args, "--mode", "balanced")),
                };
                let _ = AUTOSTART.set(StartArgs {
                    server: cfg.url.clone(),
                    room: cfg.room.clone(),
                    identity: cfg.identity.clone(),
                    fps: cfg.fps,
                    demo: cfg.demo,
                    video_w,
                    video_h,
                    video_bitrate_mbps: (bitrate / 1_000_000) as u32,
                    audio,
                    mode: arg(&args, "--mode", "balanced"),
                });
                let handle = app.handle().clone();
                eprintln!("autostart demo={} url={} room={}", cfg.demo, cfg.url, cfg.room);
                // 等 WebView 挂好监听再开播，避免早期事件丢失
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    if let Err(e) = spawn_session(handle, cfg) {
                        eprintln!("autostart: {e}");
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_, ev| {
            // 只收我们自己拉起的那个服务端；不是我们起的一个字节都不碰
            if matches!(ev, tauri::RunEvent::Exit) {
                ess_app::lkserver::stop(&mut |m| eprintln!("{m}"));
            }
        });
}
