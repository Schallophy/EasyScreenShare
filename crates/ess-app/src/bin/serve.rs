//! ess-serve：浏览器观看方的落地页（静态文件 + LiveKit 令牌）。
//!
//! 实现住在 `ess_app::viewer`——分享端「一键就绪」用的是同一份代码，这里只是命令行外壳。
//! 观看方只需打开 `http://<host>:<port>/`，无需安装客户端，也不用填房间。

use std::path::PathBuf;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bind = arg(&args, "--bind", "127.0.0.1");
    let port: u16 = arg(&args, "--port", "8088").parse().unwrap_or(8088);
    let url = arg(&args, "--url", "ws://127.0.0.1:7880");
    let room = arg(&args, "--room", "demo");
    let cfg = ess_app::viewer::Config {
        web_dir: PathBuf::from(arg(&args, "--web-dir", "crates/ess-web/web")),
        key: arg(&args, "--key", "devkey"),
        secret: arg(&args, "--secret", "secret"),
        bind: bind.clone(),
        port,
        url: url.clone(),
        room: room.clone(),
    };
    let (real_port, _live) = match ess_app::viewer::start(cfg, &mut |m| eprintln!("{m}")) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    println!("viewer page: http://{bind}:{real_port}/  (livekit {url}, room {room})");
    loop {
        std::thread::park();
    }
}
