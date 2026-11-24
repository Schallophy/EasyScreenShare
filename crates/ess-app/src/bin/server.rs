//! ess-server：EasyScreenShare 一体化服务端。
//!
//! 一条命令在 Debian 服务器上跑起全部服务端组件：
//! - LiveKit（拉起官方二进制，签发自己的 Key/Secret）
//! - 浏览器观看页 + Token 接口（复用 `ess_app::viewer`）
//!
//! 用法：
//!   ess-server                              # 自动生成 Key/Secret
//!   ess-server --key xxx --secret xxx       # 固定密钥（生产环境推荐）
//!   ess-server --lk-port 7880 --port 8088   # 指定端口
//!
//! 客户端：
//!   分享端 ess-share --url ws://<IP>:7880 --key <Key> --secret <Secret>
//!   观看端 浏览器打开 http://<IP>:8088/

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// LiveKit 信令端口（7880/HTTP-WS，7881/TCP，7882/UDP）
const DEFAULT_LK_PORT: u16 = 7880;

/// 我们亲手拉起的那一个；不是我们起的绝不碰
static LIVEKIT: Mutex<Option<Child>> = Mutex::new(None);

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().skip(1).collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_string())
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
        format!("{t:016x}").repeat((bytes + 7) / 8).chars().take(bytes * 2).collect()
    }
}

/// 找 livekit-server 二进制：`LIVEKIT_SERVER` > 程序目录同级/上级 > 常见系统路径 > 当前目录
fn find_livekit_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LIVEKIT_SERVER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) { "livekit-server.exe" } else { "livekit-server" };
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join(name));
            cands.push(dir.join("..").join(name));
        }
    }
    cands.push(PathBuf::from(format!("/usr/local/bin/{name}")));
    cands.push(PathBuf::from(format!("/usr/bin/{name}")));
    cands.push(PathBuf::from(format!("./{name}")));
    cands.retain(|p| p.is_file());
    cands.into_iter().next()
}

/// 端口上有人监听吗（只看回环，判断「本机是否已有 LiveKit」够了）
fn port_open(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpListener::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 启动 LiveKit（带 Key/Secret，绑 0.0.0.0）
fn start_livekit(port: u16, key: &str, secret: &str, log: &mut dyn FnMut(String)) -> Result<(), String> {
    if port_open(port) {
        log(format!("端口 {port} 已有 LiveKit 在跑，直接复用（不是我们起的，退出时不动它）"));
        return Ok(());
    }
    let exe = find_livekit_exe().ok_or_else(|| {
        "找不到 livekit-server。先装它：\n  \
         wget https://github.com/livekit/livekit/releases/download/v1.13.11/livekit_1.13.11_linux_amd64.tar.gz\n  \
         tar -xzf livekit_1.13.11_linux_amd64.tar.gz\n  \
         sudo mv livekit-server /usr/local/bin/\n  \
         （或设 LIVEKIT_SERVER=<路径>）"
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
    let child = cmd.spawn().map_err(|e| format!("启动 {} 失败: {e}", exe.display()))?;

    // 等它真的开始监听
    let t0 = Instant::now();
    while !port_open(port) {
        if t0.elapsed() > Duration::from_secs(10) {
            let _ = LIVEKIT.lock().map(|mut g| *g = Some(child));
            return Err(format!("等 10 s 端口 {port} 还没通，看看 {log_path:?} 里写了什么"));
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

/// 对外宣告的 IP：优先 192.168/10.x 私有地址（局域网访问入口），回退任意非回环地址
fn advertised_ip() -> String {
    if let Ok(addrs) = local_ip_address::list_afinet_netifas() {
        let mut cands: Vec<String> = addrs
            .into_iter()
            .filter_map(|(_, ip)| {
                if ip.is_loopback() || ip.is_link_local() { None } else { Some(ip.to_string()) }
            })
            .collect();
        cands.sort_by_key(|s| if s.starts_with("192.168.") || s.starts_with("10.") { 0 } else { 1 });
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

fn main() {
    let key = arg("--key", &random_hex(8));
    let secret = arg("--secret", &random_hex(24));
    let lk_port: u16 = arg("--lk-port", &DEFAULT_LK_PORT.to_string())
        .parse().unwrap_or(DEFAULT_LK_PORT);
    let viewer_port: u16 = arg("--port", &ess_app::viewer::DEFAULT_PORT.to_string())
        .parse().unwrap_or(ess_app::viewer::DEFAULT_PORT);
    let bind = arg("--bind", "0.0.0.0");
    let room = arg("--room", "default");

    let mut log = |m: String| eprintln!("[ess-server] {m}");

    // 1) LiveKit
    if let Err(e) = start_livekit(lk_port, &key, &secret, &mut log) {
        eprintln!("LiveKit 启动失败: {e}");
        cleanup();
        std::process::exit(1);
    }

    // 2) 观看页 + Token 接口（复用 viewer 模块）
    let Some(web_dir) = ess_app::viewer::find_web_dir() else {
        eprintln!("找不到观看页文件（index.html / ess_web.wasm）。先运行 `scripts/build-web.sh` 生成");
        cleanup();
        std::process::exit(1);
    };
    let ip = advertised_ip();
    let cfg = ess_app::viewer::Config {
        web_dir,
        key: key.clone(),
        secret: secret.clone(),
        bind: bind.clone(),
        port: viewer_port,
        url: format!("ws://{ip}:{lk_port}"),
        room: room.clone(),
    };
    let (real_port, _live) = match ess_app::viewer::start(cfg, &mut log) {
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
    ctrlc_wait();
    cleanup();
}

#[cfg(unix)]
fn ctrlc_wait() {
    use signal_hook::consts::SIGINT;
    use signal_hook::iterator::Signals;
    let mut sigs = Signals::new(&[SIGINT]).expect("注册 SIGINT");
    sigs.forever().next();
}

#[cfg(not(unix))]
fn ctrlc_wait() {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
