//! ess-server：EasyScreenShare 一体化服务端。
//!
//! 子命令：
//!   ess-server install [--key K --secret S] [--lk-port 7880] [--port 8088] [--room default]
//!       把自己注册为系统命令（/usr/local/bin/ess-server）、装 web 观看页、
//!       下载 livekit-server（缺时）、生成带固定密钥的 systemd 服务并启动。
//!   ess-server uninstall [--purge]
//!       停服务、删 systemd unit、删自身与 web 资产；--purge 连 livekit-server 一起删。
//!   ess-server status
//!       打印服务状态与最近日志。
//!   ess-server [run] [--key ...]
//!       前台运行（不起 systemd）。默认行为，兼容裸参数调用。
//!
//! 这个 crate **不依赖 livekit/webrtc-sys**——LiveKit 是外部二进制，
//! 这里只负责它的生命周期。所以编译快、产物小，服务器也不需要 X11/glib。

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// LiveKit 信令端口（7880/HTTP-WS，7881/TCP，7882/UDP）
const DEFAULT_LK_PORT: u16 = 7880;
const LIVEKIT_VERSION: &str = "1.13.11";

const BIN_DEST: &str = "/usr/local/bin/ess-server";
const LIVEKIT_DEST: &str = "/usr/local/bin/livekit-server";
const WEB_DEST: &str = "/usr/local/share/easyscreenshare/web";
const UNIT_PATH: &str = "/etc/systemd/system/easyscreenshare.service";
const SERVICE: &str = "easyscreenshare";

/// 我们亲手拉起的那一个；不是我们起的绝不碰
static LIVEKIT: Mutex<Option<Child>> = Mutex::new(None);

// ============================================================================
// 参数
// ============================================================================

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// 随机十六进制串。Linux 从 /dev/urandom 取；取不到时退到时间戳（仅本机测试够用，
/// 生产环境应当显式传 --key/--secret，不要依赖这个兜底）。
fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    let urandom = if cfg!(windows) { "NUL" } else { "/dev/urandom" };
    if std::fs::File::open(urandom)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        buf.iter().map(|b| format!("{b:02x}")).collect()
    } else {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{t:016x}")
            .repeat((bytes + 7) / 8)
            .chars()
            .take(bytes * 2)
            .collect()
    }
}

// ============================================================================
// 通用小工具（install/uninstall 用）
// ============================================================================

/// 只有 root 能写 /usr/local 与 /etc/systemd
fn require_root() -> Result<(), String> {
    let is_root = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false);
    if is_root {
        Ok(())
    } else {
        Err("需要 root：请用 sudo 运行".into())
    }
}

fn run_cmd(prog: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(prog)
        .args(args)
        .status()
        .map_err(|e| format!("执行 {prog} 失败: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog} {} 失败（{status}）", args.join(" ")))
    }
}

fn run_quiet(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn chmod_x(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {path:?}: {e}"))
}

#[cfg(not(unix))]
fn chmod_x(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// 平铺复制目录里的文件（观看页就是一层：index.html/app.js/wasm/umd）
fn copy_dir_files(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("建目录 {dst:?}: {e}"))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("读目录 {src:?}: {e}"))?;
    for ent in entries {
        let ent = ent.map_err(|e| e.to_string())?;
        let path = ent.path();
        if path.is_file() {
            let name = ent.file_name();
            let to = dst.join(&name);
            std::fs::copy(&path, &to).map_err(|e| format!("复制 {path:?} → {to:?}: {e}"))?;
        }
    }
    Ok(())
}

/// 安装时可用的观看页来源：ESS_WEB_DIR > 自身同级 web/ > 已安装位置 > 当前目录 web/
fn locate_web_src() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("ESS_WEB_DIR") {
        cands.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("web"));
            cands.push(dir.join("..").join("share").join("easyscreenshare").join("web"));
        }
    }
    cands.push(PathBuf::from(WEB_DEST));
    cands.push(PathBuf::from("web"));
    cands.into_iter().find(|p| {
        p.join("index.html").is_file() && p.join("ess_web.wasm").is_file()
    })
}

fn try_download(url: &str, out: &Path) -> Result<(), String> {
    let ok = Command::new("curl")
        .args(["-fsSL", url, "-o"])
        .arg(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        return Ok(());
    }
    let ok = Command::new("wget")
        .args(["-qO"])
        .arg(out)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err("下载失败：curl/wget 都不可用或网络不通".into())
    }
}

// ============================================================================
// install / uninstall / status
// ============================================================================

fn install(args: &[String]) -> Result<(), String> {
    require_root()?;
    if !cfg!(unix) {
        return Err("install/uninstall 仅支持 Linux（Debian/Ubuntu）".into());
    }

    let key = arg(args, "--key", &random_hex(8));
    let secret = arg(args, "--secret", &random_hex(24));
    let lk_port = arg(args, "--lk-port", &DEFAULT_LK_PORT.to_string());
    let viewer_port = arg(args, "--port", &ess_viewer::DEFAULT_PORT.to_string());
    let room = arg(args, "--room", "default");

    // 1) 把自己注册为命令
    print!("[1/4] 注册命令 {BIN_DEST} ... ");
    let me = std::env::current_exe().map_err(|e| format!("找不到自身路径: {e}"))?;
    if me != Path::new(BIN_DEST) {
        std::fs::copy(&me, BIN_DEST).map_err(|e| format!("复制自身到 {BIN_DEST}: {e}"))?;
    }
    chmod_x(Path::new(BIN_DEST))?;
    println!("完成");

    // 2) 观看页资产
    print!("[2/4] 安装观看页 -> {WEB_DEST} ... ");
    let web_src = locate_web_src().ok_or_else(|| {
        "找不到观看页文件（index.html / ess_web.wasm）。\
         请在 release 包解压目录里运行，或用 ESS_WEB_DIR 指定"
            .to_string()
    })?;
    copy_dir_files(&web_src, Path::new(WEB_DEST))?;
    println!("完成（来源 {}）", web_src.display());

    // 3) livekit-server（缺时才下载）
    print!("[3/4] livekit-server ... ");
    if Path::new(LIVEKIT_DEST).is_file() || which("livekit-server") {
        println!("已存在");
    } else {
        println!("未安装，下载 v{LIVEKIT_VERSION}");
        let tmp = std::env::temp_dir().join("ess-livekit-download");
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let url = format!(
            "https://github.com/livekit/livekit/releases/download/v{LIVEKIT_VERSION}/livekit_{LIVEKIT_VERSION}_linux_amd64.tar.gz"
        );
        let tgz = tmp.join("livekit.tar.gz");
        try_download(&url, &tgz)?;
        run_cmd("tar", &["-xzf", &tgz.to_string_lossy(), "-C", &tmp.to_string_lossy()])?;
        let exe = tmp.join("livekit-server");
        std::fs::copy(&exe, LIVEKIT_DEST).map_err(|e| format!("安装 livekit-server: {e}"))?;
        chmod_x(Path::new(LIVEKIT_DEST))?;
        let _ = std::fs::remove_dir_all(&tmp);
        println!("      已安装 {LIVEKIT_DEST}");
    }

    // 4) systemd 服务（密钥写死，重启不会变）
    println!("[4/4] systemd 服务 ...");
    let unit = format!(
        "[Unit]\n\
         Description=EasyScreenShare 服务端\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment=ESS_WEB_DIR={WEB_DEST}\n\
         Environment=LIVEKIT_SERVER={LIVEKIT_DEST}\n\
         ExecStart={BIN_DEST} run --key {key} --secret {secret} --lk-port {lk_port} --port {viewer_port} --room {room}\n\
         Restart=on-failure\n\
         RestartSec=5s\n\
         KillMode=mixed\n\
         TimeoutStopSec=10\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    );
    std::fs::write(UNIT_PATH, unit).map_err(|e| format!("写 {UNIT_PATH}: {e}"))?;
    run_cmd("systemctl", &["daemon-reload"])?;
    run_cmd("systemctl", &["enable", SERVICE])?;
    run_cmd("systemctl", &["restart", SERVICE])?;

    // 等它真的起来
    std::thread::sleep(Duration::from_secs(3));
    let active = run_quiet("systemctl", &["is-active", "--quiet", SERVICE]);

    let ip = advertised_ip();
    println!();
    if active {
        println!("  ✓ 已安装并启动（systemd: {SERVICE}）");
    } else {
        println!("  ✗ 服务没有起来，看日志：journalctl -u {SERVICE} -n 50 --no-pager");
    }
    println!();
    println!("  观看页:     http://{ip}:{viewer_port}/");
    println!("  LiveKit:    ws://{ip}:{lk_port}");
    println!("  API Key:    {key}");
    println!("  API Secret: {secret}");
    println!();
    println!("  分享端命令:");
    println!("    ess-share --url ws://{ip}:{lk_port} --room {room} --key {key} --secret {secret}");
    println!();
    println!("  管理:");
    println!("    ess-server status       # 状态 + 最近日志");
    println!("    ess-server uninstall    # 卸载");
    println!("    systemctl restart {SERVICE}");
    println!();
    Ok(())
}

fn uninstall(args: &[String]) -> Result<(), String> {
    require_root()?;
    if !cfg!(unix) {
        return Err("install/uninstall 仅支持 Linux（Debian/Ubuntu）".into());
    }
    let purge = has_flag(args, "--purge");

    println!("停止并删除服务 ...");
    let _ = run_quiet("systemctl", &["stop", SERVICE]);
    let _ = run_quiet("systemctl", &["disable", SERVICE]);
    let _ = std::fs::remove_file(UNIT_PATH);
    let _ = run_quiet("systemctl", &["daemon-reload"]);

    println!("删除二进制与观看页 ...");
    let _ = std::fs::remove_file(BIN_DEST);
    let _ = std::fs::remove_dir_all("/usr/local/share/easyscreenshare");
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("easyscreenshare"));

    if purge {
        println!("--purge：删除 livekit-server ...");
        let _ = std::fs::remove_file(LIVEKIT_DEST);
    } else if Path::new(LIVEKIT_DEST).is_file() {
        println!("保留 {LIVEKIT_DEST}（要一起删加 --purge）");
    }

    println!();
    println!("  已卸载 EasyScreenShare 服务端");
    println!();
    Ok(())
}

fn status() -> Result<(), String> {
    if !cfg!(unix) {
        return Err("status 仅在 Linux 上可用".into());
    }
    let _ = Command::new("systemctl")
        .args(["status", SERVICE, "--no-pager", "-l"])
        .status();
    println!();
    let _ = Command::new("journalctl")
        .args(["-u", SERVICE, "-n", "20", "--no-pager"])
        .status();
    Ok(())
}

fn which(prog: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(prog);
                p.is_file()
            })
        })
        .unwrap_or(false)
}

fn help() {
    println!(
        "ess-server — EasyScreenShare 一体化服务端\n\
         \n\
         用法:\n\
         \x20 ess-server install [--key K --secret S] [--lk-port 7880] [--port 8088] [--room default]\n\
         \x20     注册为系统命令、安装观看页与 livekit-server、建 systemd 服务并启动\n\
         \x20 ess-server uninstall [--purge]\n\
         \x20     停服务并删除；--purge 连 livekit-server 一起删\n\
         \x20 ess-server status\n\
         \x20     服务状态 + 最近日志\n\
         \x20 ess-server [run] [--key ...]\n\
         \x20     前台运行（默认），Ctrl+C 停止\n"
    );
}

// ============================================================================
// run：前台运行（拉起 LiveKit + 观看页）
// ============================================================================

/// 找 livekit-server 二进制：`LIVEKIT_SERVER` > 程序目录同级/上级 > 常见系统路径 > PATH > 当前目录
fn find_livekit_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LIVEKIT_SERVER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) {
        "livekit-server.exe"
    } else {
        "livekit-server"
    };
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join(name));
            cands.push(dir.join("..").join(name));
        }
    }
    cands.push(PathBuf::from(LIVEKIT_DEST));
    cands.push(PathBuf::from(format!("/usr/bin/{name}")));
    cands.push(PathBuf::from(format!("./{name}")));
    cands.retain(|p| p.is_file());
    cands.into_iter().next()
}

/// 端口上有人监听吗（只看回环，判断「本机是否已有 LiveKit」够了）
fn port_open(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 启动 LiveKit（带 Key/Secret，绑 0.0.0.0）
fn start_livekit(
    port: u16,
    key: &str,
    secret: &str,
    log: &mut dyn FnMut(String),
) -> Result<(), String> {
    if port_open(port) {
        log(format!(
            "端口 {port} 已有 LiveKit 在跑，直接复用（不是我们起的，退出时不动它）"
        ));
        return Ok(());
    }
    let exe = find_livekit_exe().ok_or_else(|| {
        "找不到 livekit-server。先运行 `sudo ess-server install`，\
         或设 LIVEKIT_SERVER=<路径>"
            .to_string()
    })?;

    // 写配置：Key/Secret + RTC 端口范围
    let dir = std::env::temp_dir().join("easyscreenshare");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录 {dir:?} 失败: {e}"))?;
    let cfg_path = dir.join("livekit.yaml");
    std::fs::write(
        &cfg_path,
        format!(
            "port: {port}\n\
             rtc:\n\
             \x20 port_range_start: 50000\n\
             \x20 port_range_end: 60000\n\
             keys:\n\
             \x20 - name: {key}\n\
             \x20   secret: {secret}\n"
        ),
    )
    .map_err(|e| format!("写配置失败: {e}"))?;

    let log_path = dir.join("livekit.log");
    let out = std::fs::File::create(&log_path).map_err(|e| format!("建日志失败: {e}"))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;

    let mut cmd = Command::new(&exe);
    cmd.arg("--config").arg(&cfg_path).stdout(out).stderr(err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：不弹控制台窗（会被摄进共享画面）
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败: {e}", exe.display()))?;

    // 等它真的开始监听
    let t0 = Instant::now();
    while !port_open(port) {
        if t0.elapsed() > Duration::from_secs(10) {
            let _ = LIVEKIT.lock().map(|mut g| *g = Some(child));
            return Err(format!(
                "等 10 s 端口 {port} 还没通，看看 {log_path:?} 里写了什么"
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = LIVEKIT.lock().map(|mut g| *g = Some(child));
    log(format!(
        "LiveKit 已启动 {}（日志 {}）",
        exe.display(),
        log_path.display()
    ));
    Ok(())
}

/// 对外宣告的 IP：优先 192.168/10.x 私有地址（局域网访问入口），回退任意非回环 IPv4
fn advertised_ip() -> String {
    if let Ok(addrs) = local_ip_address::list_afinet_netifas() {
        let mut cands: Vec<String> = addrs
            .into_iter()
            .filter_map(|(_, ip)| match ip {
                std::net::IpAddr::V4(v4) if v4.is_loopback() || v4.is_link_local() => None,
                std::net::IpAddr::V4(v4) => Some(v4.to_string()),
                std::net::IpAddr::V6(_) => None, // LiveKit 这里用 IPv4 地址最省事
            })
            .collect();
        cands.sort_by_key(|s| {
            if s.starts_with("192.168.") || s.starts_with("10.") {
                0
            } else {
                1
            }
        });
        if let Some(first) = cands.first() {
            return first.clone();
        }
    }
    "127.0.0.1".to_string()
}

/// 收掉我们自己拉起的 LiveKit
fn cleanup() {
    let mine = LIVEKIT.lock().ok().and_then(|mut g| g.take());
    if let Some(mut c) = mine {
        let _ = c.kill();
        let _ = c.wait();
        eprintln!("已停止 LiveKit");
    }
}

fn run_server(args: &[String]) {
    let key = arg(args, "--key", &random_hex(8));
    let secret = arg(args, "--secret", &random_hex(24));
    let lk_port: u16 = arg(args, "--lk-port", &DEFAULT_LK_PORT.to_string())
        .parse()
        .unwrap_or(DEFAULT_LK_PORT);
    let viewer_port: u16 = arg(args, "--port", &ess_viewer::DEFAULT_PORT.to_string())
        .parse()
        .unwrap_or(ess_viewer::DEFAULT_PORT);
    let bind = arg(args, "--bind", "0.0.0.0");
    let room = arg(args, "--room", "default");

    let mut log = |m: String| eprintln!("[ess-server] {m}");

    // 1) LiveKit
    if let Err(e) = start_livekit(lk_port, &key, &secret, &mut log) {
        eprintln!("LiveKit 启动失败: {e}");
        cleanup();
        std::process::exit(1);
    }

    // 2) 观看页 + Token 接口
    let Some(web_dir) = ess_viewer::find_web_dir() else {
        eprintln!(
            "找不到观看页文件（index.html / ess_web.wasm）。\
             用 ESS_WEB_DIR 指定 web 目录，或先运行 `sudo ess-server install`"
        );
        cleanup();
        std::process::exit(1);
    };
    let ip = advertised_ip();
    let cfg = ess_viewer::Config {
        web_dir,
        key: key.clone(),
        secret: secret.clone(),
        bind: bind.clone(),
        port: viewer_port,
        url: format!("ws://{ip}:{lk_port}"),
        room: room.clone(),
    };
    let (real_port, _live) = match ess_viewer::start(cfg, &mut log) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("观看页启动失败: {e}");
            cleanup();
            std::process::exit(1);
        }
    };

    // 3) 连接信息（这是用户唯一要看的东西）
    println!();
    println!("  EasyScreenShare 服务端已就绪");
    println!();
    println!("  观看页:     http://{ip}:{real_port}/");
    println!("  LiveKit:    ws://{ip}:{lk_port}");
    println!("  API Key:    {key}");
    println!("  API Secret: {secret}");
    println!();
    println!("  分享端命令:");
    println!("    ess-share --url ws://{ip}:{lk_port} --room {room} --key {key} --secret {secret}");
    println!();
    println!("  Ctrl+C 停止");
    println!();

    // 4) 等退出信号，然后只收我们自己起的那个
    wait_for_signal();
    cleanup();
}

#[cfg(unix)]
fn wait_for_signal() {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    // systemd stop 发 SIGTERM；Ctrl+C 发 SIGINT。两个都要收，否则子进程留成孤儿
    let mut sigs = Signals::new([SIGINT, SIGTERM]).expect("注册信号");
    sigs.forever().next();
}

#[cfg(not(unix))]
fn wait_for_signal() {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

// ============================================================================
// 入口：子命令分发
// ============================================================================

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let first = argv.first().map(String::as_str);

    let result = match first {
        Some("install") => install(&argv[1..]),
        Some("uninstall") => uninstall(&argv[1..]),
        Some("status") => status(),
        Some("run") => {
            run_server(&argv[1..]);
            Ok(())
        }
        Some("help" | "--help" | "-h") => {
            help();
            Ok(())
        }
        // 裸参数（--key 等）保持向后兼容：前台运行
        _ => {
            run_server(&argv);
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}
