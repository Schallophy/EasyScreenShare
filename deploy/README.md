# EasyScreenShare 服务端部署（Debian）

## 一条命令安装

```bash
curl -fsSL https://raw.githubusercontent.com/Schallophy/EasyScreenShare/main/deploy/easy-install.sh | sudo bash
```

脚本会下载最新 release，然后 `ess-server install` 自动完成：

1. 把 `ess-server` 注册为系统命令（`/usr/local/bin/ess-server`）
2. 安装浏览器观看页（`/usr/local/share/easyscreenshare/web`）
3. 下载 `livekit-server`（已有则跳过）
4. 生成 systemd 服务并启动（密钥写死，重启不变）

固定密钥：

```bash
curl -fsSL https://raw.githubusercontent.com/Schallophy/EasyScreenShare/main/deploy/easy-install.sh | sudo bash -s -- --key mykey --secret mysecret32chars
```

## 一条命令卸载

```bash
sudo ess-server uninstall          # 停服务、删命令与观看页
sudo ess-server uninstall --purge  # 连 livekit-server 一起删
```

没装过命令的话：

```bash
curl -fsSL https://raw.githubusercontent.com/Schallophy/EasyScreenShare/main/deploy/easy-install.sh | sudo bash -s -- --uninstall
```

## 状态与日志

```bash
ess-server status                  # 服务状态 + 最近 20 行日志
systemctl restart easyscreenshare
journalctl -u easyscreenshare -f
```

## 手动跑（不装 systemd）

```bash
./ess-server                       # 前台，Ctrl+C 停止
./ess-server --key k --secret s
./ess-server --help
```

## 客户端

**观看端**：浏览器打开 `http://<服务器IP>:8088/`（免安装）。

**分享端**（Windows 桌面）：

```bash
ess-share --url ws://<服务器IP>:7880 \
          --room default \
          --key <打印的Key> \
          --secret <打印的Secret>
```

或分享端 UI 的「高级：密钥」里填同一组。

## 端口

| 端口 | 协议 | 用途 |
|---|---|---|
| 7880 | TCP | LiveKit 信令 (HTTP/WebSocket) |
| 7881 | TCP | LiveKit RTC |
| 7882 | UDP | LiveKit RTC |
| 50000-60000 | UDP | RTC 媒体 (ICE) |
| 8088 | TCP | 浏览器观看页 |

## 生产环境清单

- [ ] 固定密钥（`--key/--secret`，否则每次重启都会变）
- [ ] 套 Nginx + TLS（`https`/`wss`），否则浏览器拦混合内容
- [ ] 服务器上行带宽够（视频流 2–10 Mbps/路）
- [ ] 客户端在 NAT 后时配 TURN
