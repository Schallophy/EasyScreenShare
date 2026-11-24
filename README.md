# EasyScreenShare

以屏幕共享为核心的会议应用。当前版本是**可运行的原型**：Windows 桌面共享端
（`ess-share`）+ 观看端（`ess-view` 原生窗口，或**纯浏览器、免安装**）。
传输走 LiveKit 数据通道，图块平面逐像素无损。

## 一键部署服务端（Debian）

```bash
# 下载（CI 构建的预编译包，服务器上不需要 Rust 工具链）
wget https://github.com/Schallophy/EasyScreenShare/releases/latest/download/ess-server-linux-x86_64.tar.gz
tar -xzf ess-server-linux-x86_64.tar.gz
cd ess-server-linux-x86_64

# 部署
sudo bash install.sh
```

服务启动后会打印观看页地址、API Key/Secret、分享端命令。浏览器打开观看页即可。

从源码构建只要一条命令（不编译 WebRTC，一两分钟）：

```bash
cargo build --release -p ess-server
```

详细说明见 [deploy/README.md](deploy/README.md)。

---

内部前缀一律 `ess`：crate 名、线格式 `package ess`（`crates/ess-core/proto/ess.proto`）、
`ESS_*` 环境变量、LiveKit 轨名 `ess-screen` / `ess-audio`。

设计文档：[docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md)（v0.6，协议/测量/决策记录）。
使用说明：[docs/APP.md](docs/APP.md)。

## 组成

| crate | 内容 |
|---|---|
| `ess-core` | 协议状态机（`model/`，10 条属性测试 + 变异测试）、差分/滚动/守门员（`frame/scroll/policy/governor`）、线格式（`wire`）、像素管线（`pipeline`）；可编译到 wasm32 |
| `ess-codec-palette` | 调色板 + RLE + zstd 图块编解码（快、小；>256 色拒绝） |
| `ess-codec-png` | PNG 图块编解码（真彩兜底，纯 Rust，WASM 可用） |
| `ess-transport` | LiveKit 连接、令牌、消息拆批（lossy）、整帧分片/重组（reliable） |
| `ess-capture` | Windows GDI 采集 + 合成屏（验收用） |
| `ess-app` | `ess-share` / `ess-view` / `ess-serve` / `ess-gen-expected` 可执行程序 |
| `ess-viewer` | 观看页服务 + LiveKit 令牌（只依赖 tokio + livekit-token，**不碰 webrtc-sys**） |
| `ess-server` | 一体化服务端：拉起 LiveKit + 托管观看页；一条命令部署，编译不依赖 X11/glib |
| `ess-session` | 共享会话库（采集→管线→LiveKit），CLI 与图形外壳共用 |
| `ess-share-ui` | 分享端图形外壳（Tauri v2，开始/停止、参数、实时统计、日志） |
| `ess-web` | 浏览器观看端 WASM：接收管线（判定/滚动原子应用/分片重组）裸 C ABI |
| `lk-probe` | O-1/O-2 LiveKit 数据通道探针（尺寸上限、流独立性） |
| `ess-bench` | 历史基准（编码体积/码率，见文档附录 A） |

## 快速开始

```bash
# 1) LiveKit 服务端（不进仓库，获取与校验见下面「LiveKit 服务端怎么带」）
export LIVEKIT_SERVER=/c/path/to/livekit-server.exe    # 不设则默认找 ../livekit-probe/livekit-server.exe
"$LIVEKIT_SERVER" --dev

# 2) 构建
cargo build --release -p ess-app

# 3) 观看端（先起，窗口显示收到的画面）
target/release/ess-view

# 4a) 图形分享端（推荐；Tauri 窗口：开始/停止 + 实时统计）
cargo build -p ess-share-ui --release --features custom-protocol
target/release/ess-share-ui

# 4b) 命令行分享端（采集主屏，10 fps）
target/release/ess-share   # 默认 30 fps
```

## 给第二台设备看（局域网，当前的准确步骤）

```bash
# 1) 服务端绑到所有网卡（--dev 无鉴权：任何能连到 7880 的人都能进房，只用于局域网/临时验证）
"$LIVEKIT_SERVER" --dev --bind 0.0.0.0

# 2) 观看页服务也要显式绑局域网地址（默认只听 127.0.0.1）
target/release/ess-serve --bind 0.0.0.0 --url ws://<本机IP>:7880 --room wlan --port 8088

# 3) 共享端连同一个地址
target/release/ess-share --url ws://<本机IP>:7880 --room wlan --identity share --fps 30 --mode smooth
```

第二台设备用浏览器打开 `http://<本机IP>:8088/` 就自动入房——房间号由服务端一侧决定，页面上没有输入框。
**图形分享端不需要手工这三条**，见下面「一步就绪（图形分享端）」。
`--fps 60` 会自动改用 DXGI 桌面复制，采集相位代价（真实桌面上实测平均 16.7 ms，见
[docs/IMPLEMENTATION.md §14.7.32](docs/IMPLEMENTATION.md)）随之减半。

## 一步就绪（图形分享端）

点「开始共享」时，只要「服务器」那一格是回环地址（默认就是），分享端自己把全链拉起来：

1. 7880 没人监听 → 拉起 `livekit-server --dev --bind 0.0.0.0`。找法依次是
   `LIVEKIT_SERVER` 环境变量 → 程序目录同级 → 仓库兄弟目录 `livekit-probe/`；
   应用退出时收掉**我们自己起的那个**，别人在跑的一个字节都不动。
2. 进程内起好观看页（等价于 `ess-serve --bind 0.0.0.0`，不用再开一条命令）。
   8088 被别的程序占了就退回系统分配的端口，端口表不用人去查。
3. 自动挑本机局域网地址：枚举网卡，按「物理网卡 + 同网段网关 > 有网关 > 物理 > 其余私有地址」打分，
   并把全部候选写进日志——选错地址的症状是「本机一切正常、别人连不上」，所以必须看得见。
   要钉死某张网卡就设 `ESS_ADVERTISE_IP=192.168.x.x`。
4. 界面出现可点的观看链接 `http://<本机IP>:8088/`，同一局域网的设备打开即自动进房。

命令行分享端要同样一步就绪就加 `--host-viewer`（**默认关**：验收脚本一律不带它，
免得每个门禁多一处监听）。还没做的只有二维码——手机上目前要手输一次那个地址。

## 前置依赖

| 依赖 | 情况 |
|---|---|
| LiveKit 服务端 | **不进仓库**（它是上游官方二进制，里面没有一行我们的代码）。获取、校验、放置与启动见下面「LiveKit 服务端怎么带」。 |
| `protoc` | 不需要安装：构建期由 `protoc-bin-vendored` 提供。 |
| libwebrtc 预编译 | `livekit` crate 的 build.rs 首次构建要从 GitHub 取静态库；本机走的是镜像。`cargo clean` 会把它一起清掉，之后要重新取。 |
| WebView2 运行时 | 图形分享端（Tauri v2）需要，Win10/11 一般自带。 |
| 平台 | 采集与系统声音是 Windows 专属（GDI/DXGI + WASAPI）；`ess-core` / `ess-web` 可编到 wasm32。 |

## LiveKit 服务端怎么带

1. **取**：官方 release 资产 `livekit_1.13.11_windows_amd64.zip`（19.7 MB，内含 `livekit-server.exe`
   与 `LICENSE`），发布页 <https://github.com/livekit/livekit/releases/tag/v1.13.11>。
   本项目全部验收都跑在 **v1.13.11**，换版本请重跑门禁。
2. **校验**：zip 的公开校验和在同 release 的 `checksums.txt`；解出来的 `livekit-server.exe` 应当是
   `7c63b07e78121edab0c81554af97754cc7abbaac05313cdb91c81f51287e16c2`（本机现有那份已与官方包逐字节比对过）。
   国内网络直连 GitHub 不通时可以走代理或镜像，但**镜像拿到的东西同样按这个哈希核对**。
3. **放**：任意固定路径，用环境变量告诉脚本；不设时验收脚本默认找仓库的兄弟目录
   `../livekit-probe/livekit-server.exe`。
   ```bash
   export LIVEKIT_SERVER=/c/tools/livekit/livekit-server.exe      # Git Bash
   ```
   ```bat
   set LIVEKIT_SERVER=C:/tools/livekit/livekit-server.exe
   ```
   找不到时脚本会把它正在找的路径报出来，不会静默改用别的。
4. **跑**：`"$LIVEKIT_SERVER" --dev --bind 0.0.0.0`。端口是它自己的默认值：7880（HTTP/WS 信令）、
   7881（RTC TCP）、7882（RTC UDP）。**`--dev` 等于无鉴权**——内置占位密钥 `devkey`/`secret`，
   任何能连到 7880 的人都能进房，所以只用于本机验收与局域网临时验证；对外部署必须换成自己的
   API Key/Secret 与配置文件（分享端界面上的「API Key / API Secret」两格就是为此留的）。
5. **许可**：Apache-2.0。自用无碍；将来若把它和安装包一起分发，需要连同 zip 里的 `LICENSE` 一起给到。

## 本机验收（自动比对，不需要看窗口）

```bash
scripts/demo-e2e.sh
# → E2E PASS: received canvas == expected final frame
```

## 浏览器观看（免安装）

观看方只需要浏览器，不用装任何客户端：

```bash
# 1) 构建（观看页的 wasm 与上游 client-js 都不入库，一条命令生成/取回）
scripts/build-web.sh
cargo build --release -p ess-app

# 2) LiveKit + 观看页服务
"$LIVEKIT_SERVER" --dev
target/release/ess-serve                     # http://127.0.0.1:8088/

# 3) 共享端
target/release/ess-share

# 4) 浏览器打开 http://127.0.0.1:8088/
```

浏览器里跑的是**同一份接收管线**（编译成 WASM，裸 C ABI）：epoch/version 判定、
滚动组原子应用、整帧分片重组都在 Rust 侧，JS 只负责 LiveKit 连接与画布绘制。
迟到加入的观看方会收到发送端强制重发的整帧。

一键演示：`scripts/demo-browser.sh`（起 LiveKit + 服务 + 合成屏共享，打印 URL）。

## 测试

```bash
cargo test -p ess-core -p ess-codec-palette -p ess-codec-png -p ess-transport
PROPTEST_CASES=10000 cargo test -p ess-core --release --test prop_protocol   # 浸泡
cargo test -p ess-core --release --test fluency -- --nocapture               # 弱网矩阵/突变
```

`ess-codec-palette` 的 `tests/pipeline_e2e.rs` 把「真实像素 → 差分/滚动 → 编码 →
protobuf → 判定 → 画布」跑成逐字节断言；`scripts/demo-e2e.sh` 再把它放到真实
LiveKit 链路上跑一遍。

## 当前边界（诚实声明）

- 采集为 GDI BitBlt；WGC（脏矩形/更低开销）是下一步。
- NACK 未接入（LiveKit Rust SDK 不暴露丢包回调），修复依赖 500 ms 状态报告兜底；
  §6.6 修复预算在模型层验证过，传输层尚未接入。
- §9 守门员组件已实现（阶梯/迟滞/15 fps 运动下限），尚未接带宽估计。
- 数据通道实测约 3.7 MB/s，无损图块撑不住 1080p 全屏运动（30 fps 需 ~27 MB/s）。
  所以有**两个平面**：静态/文字/滚动走无损图块（数据通道），大面积运动由决策器
  切到 WebRTC 视频轨（默认 H264 屏幕共享，1280×720@30、4 Mbps）。实测 localhost
  玻璃到玻璃延迟 55 ms（最好）/ 74 ms（中位）、丢帧 0.4%；交接遵守"先修复后关
  平面"的顺序契约，退出运动只补差集块而不是 1.2 MB 整帧。
- 延迟已达标但**尚未对标远控的其余项**：60 fps 档、按网络自适应码率、断线重连、
  光标显示与 WGC 采集，见 `docs/APP.md` 与任务列表。
- E2EE、音频、TUI/GUI 外壳未做。
