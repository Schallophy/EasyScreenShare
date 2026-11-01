//! 「该有就复用、没有才拉起」的 LiveKit 服务端监管。
//!
//! 我们只传官方二进制的 `--dev --bind`，服务端里没有一行本项目代码（令牌由分享端自己签）。
//! 因此这里的全部职责是：找到它、把它带起来、别让它在应用退出后占着端口当孤儿。

use std::fs::File;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 信令端口，也是它自己的默认值（RTC 用 7881/TCP 与 7882/UDP）
pub const DEFAULT_PORT: u16 = 7880;

/// 我们亲手拉起来的那一个；不是我们起的就绝不碰
static OWNED: Mutex<Option<Child>> = Mutex::new(None);

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "livekit-server.exe"
    } else {
        "livekit-server"
    }
}

/// 找法：`LIVEKIT_SERVER` 环境变量 > 程序目录同级（安装版）> 仓库兄弟目录（开发树）
pub fn find_exe() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("LIVEKIT_SERVER") {
        cands.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join(exe_name()));
            cands.push(dir.join("livekit-probe").join(exe_name()));
            // target/release/ess-share.exe → 仓库的兄弟目录 livekit-probe/
            cands.push(dir.join("..").join("..").join("livekit-probe").join(exe_name()));
        }
    }
    cands.push(PathBuf::from("../livekit-probe").join(exe_name()));
    cands.retain(|p| p.is_file());
    cands.into_iter().next()
}

/// 端口上有人监听吗（只看本机回环，够判断「这台机器上是否已有服务端」）
pub fn port_open(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 需要时拉起服务端并等它真的开始监听；已有服务在跑就直接复用，不抢也不重启
pub fn ensure(port: u16, log: &mut dyn FnMut(String)) -> Result<(), String> {
    if port_open(port) {
        log(format!("端口 {port} 已有 LiveKit 在服务，直接复用（不是我们起的，退出时不动它）"));
        return Ok(());
    }
    let exe = find_exe().ok_or_else(|| {
        "找不到 LiveKit 服务端二进制。获取与校验见 README「LiveKit 服务端怎么带」；\n  \
         也可以设 LIVEKIT_SERVER=<它的路径>。"
            .to_string()
    })?;
    let logfile = std::env::temp_dir().join("ess-livekit-server.log");
    let out = File::create(&logfile).map_err(|e| format!("写日志文件失败 {logfile:?}: {e}"))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.args(["--dev", "--bind", "0.0.0.0"]).stdout(out).stderr(err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：弹一条控制台窗会被摄进共享画面，验收时还以为那是内容
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败：{e}", exe.display()))?;
    let t0 = Instant::now();
    while !port_open(port) {
        if t0.elapsed() > Duration::from_secs(10) {
            // 先收下再报错：留着一个不通的进程比留个孤儿好收
            let _ = OWNED.lock().map(|mut g| *g = Some(child));
            return Err(format!(
                "等了 10 s 端口 {port} 还没通，看看 {} 里写了什么",
                logfile.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = OWNED.lock().map(|mut g| *g = Some(child));
    log(format!(
        "已拉起 LiveKit 服务端 {}（日志 {}）",
        exe.display(),
        logfile.display()
    ));
    Ok(())
}

/// 只收我们自己起的那个
pub fn stop(log: &mut dyn FnMut(String)) {
    let mine = OWNED.lock().ok().and_then(|mut g| g.take());
    if let Some(mut c) = mine {
        let _ = c.kill();
        let _ = c.wait();
        log("已收掉我们自己拉起的 LiveKit 服务端".into());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn find_exe_never_panics_when_server_absent() {
        // 测试跑在 target/debug/，往上两级就是仓库根，其兄弟目录 livekit-probe/ 有官方二进制。
        // 本机没放服务端时这里应当返回 None 而不是 panic——它就是产品入口要给用户看的那条错误。
        let _ = super::find_exe();
    }

    #[test]
    fn port_open_is_false_when_nobody_listens() {
        // 39333 不是本项目的任何端口；若哪天恰好被占，这条会红，换一个即可
        assert!(!super::port_open(39333));
    }
}
