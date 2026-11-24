# EasyScreenShare 服务端部署（Debian）

一个二进制 `ess-server`，起来后同时提供 LiveKit、浏览器观看页、Token 接口。

## 部署

从 [Releases](https://github.com/Schallophy/EasyScreenShare/releases) 下载最新包：

```bash
wget https://github.com/Schallophy/EasyScreenShare/releases/latest/download/ess-server-linux-x86_64.tar.gz
tar -xzf ess-server-linux-x86_64.tar.gz
cd ess-server-linux-x86_64

# 一键部署（自动生成 Key/Secret）
sudo bash install.sh

# 或固定密钥（生产环境推荐）
sudo bash install.sh --key mykey --secret mysecret-at-least-32-chars
```

脚本会：装 livekit-server、装 ess-server + web 观看页、注册 systemd 服务、开防火墙、启动。

装完看连接信息：

```bash
journalctl -u easyscreenshare -n 30 --no-pager
```

输出里会打印观看页地址、API Key/Secret、分享端命令。

## 客户端

**观看端**：浏览器打开 `http://<服务器IP>:8088/`

**分享端**（Windows 桌面）：

```bash
ess-share --url ws://<服务器IP>:7880 \
          --room default \
          --key <打印的Key> \
          --secret <打印的Secret>
```

分享端 UI 也能用，在「高级：密钥」里填同一组 Key/Secret。

## ess-server 单独用（不装 systemd）

```bash
./ess-server                 # Ctrl+C 停止
./ess-server --key k --secret s
./ess-server --lk-port 7880 --port 8088 --room myroom
```

## 生产环境清单

- [ ] 用 `--key` / `--secret` 固定密钥
- [ ] 套 Nginx + TLS（`https`/`wss`），否则浏览器拦混合内容
- [ ] 服务器上行带宽够（视频流 2–10 Mbps/路）
- [ ] 客户端在 NAT 后时配 TURN

## 管理

```bash
systemctl status easyscreenshare    # 状态
journalctl -u easyscreenshare -f    # 实时日志
systemctl restart easyscreenshare   # 重启
sudo bash uninstall.sh              # 卸载
```

## 端口

| 端口 | 协议 | 用途 |
|---|---|---|
| 7880 | TCP | LiveKit 信令 (HTTP/WebSocket) |
| 7881 | TCP | LiveKit RTC |
| 7882 | UDP | LiveKit RTC |
| 50000-60000 | UDP | RTC 媒体 (ICE) |
| 8088 | TCP | 浏览器观看页 |
