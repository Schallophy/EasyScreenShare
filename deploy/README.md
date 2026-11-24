# EasyScreenShare 服务端部署（Debian）

## 最简方式：一条命令

开发机上编译一次：

```bash
cargo build --release -p ess-app --features server --bin ess-server
```

把产物传到服务器：

```bash
scp target/release/ess-server user@server:/tmp/
scp deploy/install.sh deploy/easyscreenshare.service user@server:/tmp/
```

服务器上跑：

```bash
cd /tmp && sudo bash install.sh
```

脚本会自动装 livekit-server、注册 systemd 服务、开防火墙、启动，并把连接信息打出来。

---

## 它做了什么

`ess-server` 是**一个二进制**，起来后同时提供：

| 组件 | 端口 | 说明 |
|---|---|---|
| LiveKit 信令 | 7880/tcp | WebSocket |
| LiveKit RTC | 7881/tcp + 7882/udp | 媒体通道 |
| RTC 媒体端口 | 50000-60000/udp | ICE 候选 |
| 浏览器观看页 | 8088/tcp | 静态文件 + `/token` |

密钥自动生成（`--key` / `--secret` 可覆盖），Ctrl+C 一键停掉自己起的 LiveKit。

---

## 客户端

**观看端**：任何浏览器打开 `http://<服务器IP>:8088/`

**分享端**（Windows 桌面）：

```bash
ess-share --url ws://<服务器IP>:7880 \
          --room default \
          --key <打印出来的Key> \
          --secret <打印出来的Secret>
```

分享端 UI 也能用，在「高级：密钥」里填同一组 Key/Secret。

---

## 生产环境清单

- [ ] 用 `--key` / `--secret` 固定密钥（不依赖自动生成）
- [ ] 套 Nginx + TLS（`https` / `wss`），否则浏览器会拦混合内容
- [ ] 服务器带宽够（视频流按 2–10 Mbps/路估）
- [ ] 如果客户端在 NAT 后，配 TURN

---

## 管理

```bash
systemctl status easyscreenshare   # 状态
journalctl -u easyscreenshare -f   # 实时日志
systemctl restart easyscreenshare  # 重启
sudo bash uninstall.sh             # 卸载
```
