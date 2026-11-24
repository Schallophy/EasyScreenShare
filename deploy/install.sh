#!/bin/bash
# EasyScreenShare 极简部署 (Debian/Ubuntu)
# 
# 用法：
#   sudo bash install.sh                    # 下载预编译的 ess-server
#   sudo bash install.sh --key xxx --secret yyy   # 固定密钥
#
# 脚本做的事：
#   1. 下载 livekit-server 到 /usr/local/bin/
#   2. 下载（或本地构建后拷贝）ess-server 到 /usr/local/bin/
#   3. 安装 systemd 服务
#   4. 开放防火墙端口
#   5. 启动服务并打印连接信息

set -e

LIVEKIT_VERSION="1.13.11"
INSTALL_DIR="/usr/local/bin"
SERVICE_FILE="/etc/systemd/system/easyscreenshare.service"

# 解析命令行参数
EXTRA_ARGS=""
if [ $# -gt 0 ]; then
    EXTRA_ARGS="$*"
fi

echo "=== EasyScreenShare 部署 ===\n"

# 检查 root
if [ "$EUID" -ne 0 ]; then
    echo "请用 sudo 运行" >&2
    exit 1
fi

# 1. 下载 livekit-server
echo "[1/5] 下载 LiveKit $LIVEKIT_VERSION ..."
if [ ! -f "$INSTALL_DIR/livekit-server" ]; then
    wget -q "https://github.com/livekit/livekit/releases/download/v$LIVEKIT_VERSION/livekit_${LIVEKIT_VERSION}_linux_amd64.tar.gz" \
        -O /tmp/livekit.tar.gz
    tar -xzf /tmp/livekit.tar.gz -C /tmp
    mv /tmp/livekit-server "$INSTALL_DIR/"
    chmod +x "$INSTALL_DIR/livekit-server"
    rm -f /tmp/livekit.tar.gz
    echo "    ✓ 已安装到 $INSTALL_DIR/livekit-server"
else
    echo "    已存在，跳过"
fi

# 2. 安装 ess-server
echo "[2/5] 安装 ess-server ..."
if [ -f "./ess-server" ]; then
    # 本地已编译好，直接拷
    cp ./ess-server "$INSTALL_DIR/"
else
    echo "    ⚠ 找不到 ./ess-server 二进制" >&2
    echo "    请先在开发机运行：cargo build --release -p ess-app --features server --bin ess-server" >&2
    echo "    然后把 target/release/ess-server 拷到这里再运行此脚本" >&2
    exit 1
fi
chmod +x "$INSTALL_DIR/ess-server"

# 3. systemd 服务
echo "[3/5] 安装 systemd 服务 ..."
cat > "$SERVICE_FILE" << EOF
[Unit]
Description=EasyScreenShare 服务端
After=network.target

[Service]
Type=simple
ExecStart=$INSTALL_DIR/ess-server $EXTRA_ARGS
Restart=on-failure
RestartSec=5s
StandardOutput=journal
StandardError=journal
KillMode=mixed
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable easyscreenshare

# 4. 防火墙
echo "[4/5] 配置防火墙 ..."
if command -v ufw >/dev/null; then
    ufw allow 7880/tcp comment 'ESS-Signal' >/dev/null 2>&1 || true
    ufw allow 7881/tcp comment 'ESS-RTC-TCP' >/dev/null 2>&1 || true
    ufw allow 7882/udp comment 'ESS-RTC-UDP' >/dev/null 2>&1 || true
    ufw allow 50000:60000/udp comment 'ESS-RTC-Media' >/dev/null 2>&1 || true
    ufw allow 8088/tcp comment 'ESS-Viewer' >/dev/null 2>&1 || true
elif command -v firewall-cmd >/dev/null; then
    firewall-cmd --permanent --add-port=7880/tcp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=7881/tcp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=7882/udp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=50000-60000/udp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=8088/tcp >/dev/null 2>&1 || true
    firewall-cmd --reload >/dev/null 2>&1 || true
fi
echo "    已开放 7880/7881/7882/8088 + RTC 媒体端口 50000-60000/udp"

# 5. 启动
echo "[5/5] 启动服务 ..."
systemctl start easyscreenshare
sleep 2

# 打印连接信息
echo "\n=== 部署完成 ===\n"

# 读公网 IP（优先）
PUBLIC_IP=$(curl -s --max-time 3 https://api.ipify.org 2>/dev/null || echo "")
if [ -z "$PUBLIC_IP" ]; then
    # 回退：从 ess-server 日志里抓
    PUBLIC_IP=$(journalctl -u easyscreenshare -n 20 --no-pager 2>/dev/null | \
        grep -oE "观看页:     http://[0-9.]+" | head -1 | \
        grep -oE "[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+" || echo "本机IP")
fi

# 读 API Key/Secret（从日志）
sleep 1
API_KEY=$(journalctl -u easyscreenshare -n 20 --no-pager 2>/dev/null | \
    grep -oE "API Key:    [a-f0-9]+" | head -1 | awk '{print $3}' || echo "")
API_SECRET=$(journalctl -u easyscreenshare -n 20 --no-pager 2>/dev/null | \
    grep -oE "API Secret: [a-f0-9]+" | head -1 | awk '{print $3}' || echo "")

echo "  观看页:   http://$PUBLIC_IP:8088/"
echo "  LiveKit:  ws://$PUBLIC_IP:7880"
echo "  API Key:    $API_KEY"
echo "  API Secret: $API_SECRET"
echo ""
echo "  分享端命令:"
echo "    ess-share --url ws://$PUBLIC_IP:7880 --room default --key $API_KEY --secret $API_SECRET"
echo ""
echo "  管理命令:"
echo "    查看状态: systemctl status easyscreenshare"
echo "    查看日志: journalctl -u easyscreenshare -f"
echo "    重启:    systemctl restart easyscreenshare"
echo "    停止:    systemctl stop easyscreenshare"
echo "    卸载:    systemctl disable easyscreenshare && rm $SERVICE_FILE && rm $INSTALL_DIR/ess-server $INSTALL_DIR/livekit-server"
echo ""
