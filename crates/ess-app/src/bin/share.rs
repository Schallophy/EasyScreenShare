//! ess-share：命令行共享端（采集屏幕 → 图块管线 → LiveKit）。
//!
//! 用法：
//! ```text
//! ess-share [--url ws://127.0.0.1:7880] [--room demo] [--identity share]
//!          [--demo] [--fps 30] [--seconds 0] [--key devkey --secret secret]
//!          [--host-viewer]   本机当服务端：需要时拉起 livekit-server + 起好观看页并打印地址
//!          [--mode clear|balanced|smooth] [--no-audio]
//! ```
//! `--demo` 用合成屏（自动化验收/无采集环境）。图形外壳见 `ess-share-ui`。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ess_session::{Config, Event};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = Config {
        url: arg(&args, "--url", "ws://127.0.0.1:7880"),
        room: arg(&args, "--room", "demo"),
        identity: arg(&args, "--identity", "share"),
        key: arg(&args, "--key", "devkey"),
        secret: arg(&args, "--secret", "secret"),
        fps: arg(&args, "--fps", "30").parse().unwrap_or(30),
        demo: flag(&args, "--demo"),
        video_max: match (
            arg(&args, "--video-w", "0").parse::<u32>(),
            arg(&args, "--video-h", "0").parse::<u32>(),
        ) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => Some((w, h)),
            _ => None,
        },
        video_bitrate: arg(&args, "--video-bitrate", "0")
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0),
        audio: !flag(&args, "--no-audio"),
        // 产品档：清晰优先 / 平衡 / 流畅优先。写错不猜，直接用默认并说明。
        mode: match ess_session::ProductMode::parse(
            &arg(&args, "--mode", &std::env::var("ESS_MODE").unwrap_or_else(|_| "balanced".into())),
        ) {
            Some(m) => m,
            None => {
                eprintln!("--mode 不认识，用默认档 balanced（可选 clear|balanced|smooth）");
                Default::default()
            }
        },
    };
    let seconds: u64 = arg(&args, "--seconds", "0").parse().unwrap_or(0);

    // 一键就绪：默认**关**。验收脚本一律不经过这里——多一个监听就多一处不确定性，
    // 而产品路径（图形外壳）默认开，见 ess-share-ui。
    if flag(&args, "--host-viewer") && ess_app::self_hosted(&cfg.url) {
        match ess_app::bring_up(&cfg.room, &cfg.key, &cfg.secret, &mut |m| println!("就绪: {m}")) {
            Ok(r) => {
                cfg.url = r.ws_url.clone();
                println!(
                    "观看端在同一局域网打开 {} 即可（房间 {}，页面自动进房）",
                    r.viewer_url, cfg.room
                );
            }
            Err(e) => eprintln!("一键就绪失败：{e}（仍按填好的地址继续共享）"),
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    if seconds > 0 {
        let stop = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            stop.store(true, Ordering::Relaxed);
        });
    }

    let mut tick = 0u32;
    let res = ess_session::run(cfg, stop, |ev| match ev {
        Event::Connected { width, height } => {
            println!("source {width}x{height}");
        }
        Event::Log(msg) => println!("{msg}"),
        Event::Error(msg) => eprintln!("{msg}"),
        Event::Stats(s) => {
            tick += 1;
            if tick % 5 == 0 {
                println!(
                    "t={}s frames={} tiles={} msgs={} bytes={}",
                    s.elapsed_s, s.frames, s.tiles, s.msgs, s.bytes
                );
            }
        }
    })
    .await;

    // --host-viewer 起的是我们自己的服务端：退出就收掉，别留一个占着 7880 的孤儿
    // （下次验收脚本会被它挡在端口检查上，而它看起来「什么都没做」）
    if flag(&args, "--host-viewer") {
        ess_app::lkserver::stop(&mut |m| println!("{m}"));
    }

    if let Err(e) = res {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
