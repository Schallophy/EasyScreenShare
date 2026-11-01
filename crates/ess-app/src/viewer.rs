//! 浏览器观看方的落地页：静态文件 + LiveKit 令牌。
//!
//! 观看方只需打开 `http://<host>:<port>/`，无需安装客户端，也不用填房间——
//! 房间号是这里给的（`app.js` 只 `fetch('token')`）。
//! 依赖只有 tokio：HTTP/1.1 的静态服务不值得引入完整框架。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 观看页端口，也是 `ess-serve` 一直用的默认值
pub const DEFAULT_PORT: u16 = 8088;

#[derive(Clone)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub web_dir: PathBuf,
    pub url: String,
    pub room: String,
    pub key: String,
    pub secret: String,
}

struct Inner {
    web: PathBuf,
    key: String,
    secret: String,
    url: Mutex<String>,
    room: Mutex<String>,
    next: AtomicU64,
}

/// 已经起来的观看页。房间与信令地址可以事后改：用户点了停止又改了房间再点开始，
/// 页面必须跟着改，否则观看端连进的是上一次的房间。
#[derive(Clone)]
pub struct Live {
    inner: Arc<Inner>,
}

impl Live {
    pub fn reconfigure(&self, url: &str, room: &str) {
        if let Ok(mut g) = self.inner.url.lock() {
            *g = url.to_string();
        }
        if let Ok(mut g) = self.inner.room.lock() {
            *g = room.to_string();
        }
    }
}

/// 观看页文件的找法：`ESS_WEB_DIR` > 程序目录下 `web/`（安装版）> 开发树的 `crates/ess-web/web`
pub fn find_web_dir() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("ESS_WEB_DIR") {
        cands.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("web"));
            cands.push(dir.join("..").join("web"));
            cands.push(dir.join("..").join("..").join("crates/ess-web/web"));
        }
    }
    cands.push(PathBuf::from("crates/ess-web/web"));
    cands.retain(|p| p.join("index.html").is_file() && p.join("ess_web.wasm").is_file());
    cands.into_iter().next()
}

fn mime_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

async fn respond(mut s: TcpStream, status: &str, mime: &str, body: &[u8], no_store: bool) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
         Cache-Control: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len(),
        if no_store { "no-store" } else { "public, max-age=60" }
    );
    let _ = s.write_all(head.as_bytes()).await;
    let _ = s.write_all(body).await;
    let _ = s.shutdown().await;
}

async fn handle(mut s: TcpStream, inner: Arc<Inner>) {
    let mut buf = vec![0u8; 8192];
    let mut filled = 0usize;
    loop {
        match s.read(&mut buf[filled..]).await {
            Ok(0) => return,
            Ok(n) => {
                filled += n;
                if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") || filled == buf.len() {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&buf[..filled]);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return respond(s, "400 Bad Request", "text/plain", b"bad request", true).await;
    };
    if method != "GET" && method != "HEAD" {
        return respond(s, "405 Method Not Allowed", "text/plain", b"get only", true).await;
    }
    let path = target.split('?').next().unwrap_or("/");
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if path == "token" {
        let (url, room) = {
            (
                inner.url.lock().map(|g| g.clone()).unwrap_or_default(),
                inner.room.lock().map(|g| g.clone()).unwrap_or_default(),
            )
        };
        let id = inner.next.fetch_add(1, Ordering::Relaxed) + 1;
        let identity = format!("web-{id}");
        let tok = ess_transport::token(&inner.key, &inner.secret, &room, &identity);
        let body = format!(
            "{{\"url\":\"{url}\",\"room\":\"{room}\",\"identity\":\"{identity}\",\"token\":\"{tok}\"}}"
        );
        return respond(s, "200 OK", "application/json", body.as_bytes(), true).await;
    }

    if path.contains("..") || path.contains('\\') || path.contains('/') {
        return respond(s, "404 Not Found", "text/plain", b"not found", true).await;
    }
    let file = inner.web.join(path);
    match tokio::fs::read(&file).await {
        Ok(body) => {
            let mime = mime_of(&file);
            let no_store =
                path.ends_with(".html") || path.ends_with(".wasm") || path.ends_with(".js");
            respond(s, "200 OK", mime, &body, no_store).await
        }
        Err(_) => respond(s, "404 Not Found", "text/plain", b"not found", true).await,
    }
}

/// 起观看页服务。自带一条线程和一个 tokio 运行时，所以调用方不需要已经有运行时
/// （图形外壳是从 Tauri 主线程调的）。返回实际监听端口与可事后改配置的句柄。
pub fn start(cfg: Config, log: &mut dyn FnMut(String)) -> Result<(u16, Live), String> {
    if !cfg.web_dir.join("index.html").is_file() {
        return Err(format!("观看页目录里没有 index.html：{}", cfg.web_dir.display()));
    }
    let listener = std::net::TcpListener::bind((cfg.bind.as_str(), cfg.port))
        .map_err(|e| format!("绑定 {}:{} 失败：{e}", cfg.bind, cfg.port))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let live = Live {
        inner: Arc::new(Inner {
            web: cfg.web_dir,
            key: cfg.key,
            secret: cfg.secret,
            url: Mutex::new(cfg.url),
            room: Mutex::new(cfg.room),
            next: AtomicU64::new(0),
        }),
    };
    let thread_live = live.clone();
    std::thread::Builder::new()
        .name("ess-viewer".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build();
            let Ok(rt) = rt else { return };
            rt.block_on(async move {
                let Ok(listener) = TcpListener::from_std(listener) else { return };
                loop {
                    let Ok((s, _)) = listener.accept().await else { continue };
                    let inner = thread_live.inner.clone();
                    tokio::spawn(handle(s, inner));
                }
            });
        })
        .map_err(|e| e.to_string())?;
    log(format!("观看页已就绪 http://{}:{port}/", if cfg.bind == "0.0.0.0" { "本机局域网地址" } else { &cfg.bind }));
    Ok((port, live))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// 端到端跑一次：起服务 → 取 token → 令牌里的房间必须跟着 `reconfigure` 改。
    /// 这条就是「停止后再改房间开始」的产品行为，光编译看不出来。
    #[test]
    fn token_follows_room_change_and_files_are_served() {
        let Some(web) = super::find_web_dir() else {
            eprintln!("没有观看页文件，跳过");
            return;
        };
        let mut quiet = |_| {};
        let (port, live) = super::start(
            super::Config {
                bind: "127.0.0.1".into(),
                port: 0, // 让系统挑，避免和验收脚本抢 8088
                web_dir: web,
                url: "ws://127.0.0.1:7880".into(),
                room: "first".into(),
                key: "devkey".into(),
                secret: "secret".into(),
            },
            &mut quiet,
        )
        .expect("起观看页");

        let get = |path: &str| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            buf
        };
        let body = String::from_utf8_lossy(&get("token")).to_string();
        assert!(body.contains("\"room\":\"first\""), "第一次令牌该是 first：{body}");

        live.reconfigure("ws://10.0.0.2:7880", "second");
        let body2 = String::from_utf8_lossy(&get("token")).to_string();
        assert!(body2.contains("\"room\":\"second\""), "改完房间令牌没跟上：{body2}");
        assert!(body2.contains("ws://10.0.0.2:7880"), "信令地址没跟上：{body2}");

        let page = get("");
        assert!(String::from_utf8_lossy(&page).contains("200 OK"), "index.html 出不来");
        let wasm = get("ess_web.wasm");
        assert!(
            String::from_utf8_lossy(&wasm).contains("200 OK"),
            "wasm 出不来（观看端就是空白）"
        );
    }
}
