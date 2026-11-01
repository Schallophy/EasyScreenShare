//! 分享端「一步起全链」需要的零件，CLI 与图形外壳共用同一条实现。
//!
//! - [`viewer`]：浏览器观看页的静态服务 + 令牌（原 `ess-serve` 的循环）
//! - [`lkserver`]：端口没人监听时才把官方 LiveKit 服务端拉起来，退出时收掉
//! - [`netinfo`]：本机局域网 IPv4
//!
//! 验收脚本一律不经过 [`bring_up`]：它只在产品入口调用，默认不多开任何监听。

pub mod lkserver;
pub mod netinfo;
pub mod viewer;

use std::sync::Mutex;

/// 进程内只允许一份观看页服务；重复点「开始共享」只改房间，不再多绑一个端口。
static HOST: Mutex<Option<(u16, viewer::Live)>> = Mutex::new(None);

/// 一步就绪的结果：两个地址都要给出去——`viewer_url` 是给观看端打开的，
/// `ws_url` 是分享端自己该连的（同一块网卡，别一个走回环一个走局域网）。
pub struct Ready {
    pub viewer_url: String,
    pub ws_url: String,
}

/// URL 指向回环，说明用户没打算连远端服务器——这台机器自己就是服务端。
/// 填了远端地址时一律不动：既不去抢别人的端口，也不在本机多起一个没人用的服务。
pub fn self_hosted(url: &str) -> bool {
    url.contains("127.0.0.1") || url.contains("localhost") || url.contains("[::1]")
}

/// 一步就绪：需要时拉起 LiveKit 服务端、起好观看页（或把房间改成这次的），
/// 返回给观看端用的地址。`log` 把每一步结论写到用户看得见的地方。
pub fn bring_up(
    room: &str,
    key: &str,
    secret: &str,
    log: &mut dyn FnMut(String),
) -> Result<Ready, String> {
    lkserver::ensure(lkserver::DEFAULT_PORT, log)?;
    let (ip, listed) = netinfo::lan_ipv4_list();
    if listed.len() > 1 {
        // 选错地址的表现是「本机看着一切正常，别人连不上」，所以候选必须看得见
        log(format!("网卡地址候选：{}", listed.join(" · ")));
    }
    let host = match ip {
        Some(a) => a.to_string(),
        None => {
            log("没找到局域网地址（这台机器没有可用默认路由？），观看页只监听 127.0.0.1".into());
            "127.0.0.1".to_string()
        }
    };
    let bind = if ip.is_some() { "0.0.0.0" } else { "127.0.0.1" };
    let url = format!("ws://{host}:{}", lkserver::DEFAULT_PORT);

    let mut guard = HOST.lock().map_err(|_| "观看页状态锁被打坏".to_string())?;
    let port = if let Some((p, live)) = guard.as_mut() {
        live.reconfigure(&url, room);
        log(format!("观看页已在跑，房间改成 {room}"));
        *p
    } else {
        let web = viewer::find_web_dir().ok_or_else(|| {
            "找不到观看页文件（需要 index.html 与 ess_web.wasm）。开发树默认找 crates/ess-web/web，\n  \
             安装版找程序目录下的 web/，也可以用 ESS_WEB_DIR 指定。"
                .to_string()
        })?;
        let base = viewer::Config {
            bind: bind.to_string(),
            port: viewer::DEFAULT_PORT,
            web_dir: web,
            url: url.clone(),
            room: room.to_string(),
            key: key.to_string(),
            secret: secret.to_string(),
        };
        // 8088 被别的程序占了就退回系统分配，别把「一键」变成「先去看端口表」
        let (p, live) = match viewer::start(base.clone(), log) {
            Ok(v) => v,
            Err(e) => {
                log(format!("默认端口起不来（{e}），改用系统分配的端口"));
                let mut alt = base.clone();
                alt.port = 0;
                viewer::start(alt, log)?
            }
        };
        *guard = Some((p, live));
        p
    };
    Ok(Ready {
        viewer_url: format!("http://{host}:{port}/"),
        ws_url: url,
    })
}
