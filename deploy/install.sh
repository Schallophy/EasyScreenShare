#!/bin/bash
# EasyScreenShare 部署 (Debian/Ubuntu)
#
# 在 release 包解压目录里运行：
#   sudo bash install.sh                      # 自动生成 Key/Secret
#   sudo bash install.sh --key xxx --secret yyy   # 固定密钥（生产环境）
#
# 脚本做的事：
#   1. 下载 livekit-server
#   2. 安装 ess-server + web 观看页资产
#   3. 安装 systemd 服务
#   4. 开放防火墙端口
#   5. 启动并打印连接信息

set -e

LIVEKIT_VERSION="1.13.11"
BIN_DIR="/usr/local/bin"
WEB_DIR="/usr/local/share/easyscreenshare/web"
SERVICE_FILE="/etc/systemd/system/easyscreenshare.service"

EXTRA_ARGS="$*"

echo "=== EasyScreenShare 部署 ==="
echo

if [ "$EUID" -ne 0 ]; then
    echo "请用 sudo 运行" >&2
    exit 1
fi

# 1. livekit-server
echo "[1/5] 下载 LiveKit $LIVEKIT_VERSION ..."
if [ ! -f "$BIN_DIR/livekit-server" ]; then
    wget -q "https://github.com/livekit/livekit/releases/download/v$LIVEKIT_VERSION/livekit_${LIVEKIT_VERSION}_linux_amd64.tar.gz" \
        -O /tmp/livekit.tar.gz
    tar -xzf /tmp/livekit.tar.gz -C /tmp
    mv /tmp/livekit-server "$BIN_DIR/"
    chmod +x "$BIN_DIR/livekit-server"
    rm -f /tmp/livekit.tar.gz
    echo "    ✓ $BIN_DIR/livekit-server"
else
    echo "    已存在，跳过"
fi

# 2. ess-server + web 资产
echo "[2/5] 安装 ess-server ..."
# 找 ess-server：当前目录，或脚本所在目录（release 包解压后）
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "./ess-server" ]; then
    ESS_SRC="./ess-server"
elif [ -f "$SCRIPT_DIR/ess-server" ]; then
    ESS_SRC="$SCRIPT_DIR/ess-server"
else
    echo "    ✗ 找不到 ess-server 二进制" >&2
    echo "    请在 release 包解压目录里运行此脚本" >&2
    exit 1
fi
cp "$ESS_SRC" "$BIN_DIR/"
chmod +x "$BIN_DIR/ess-server"

# web 观看页：优先脚本同级 web/（release 包结构），其次当前目录
WEB_SRC=""
for d in "$SCRIPT_DIR/web" "./web"; do
    if [ -f "$d/index.html" ] && [ -f "$d/ess_web.wasm" ]; then
        WEB_SRC="$d"
        break
    fi
done
if [ -n "$WEB_SRC" ]; then
    mkdir -p "$WEB_DIR"
    cp "$WEB_SRC"/* "$WEB_DIR/"
    echo "    ✓ ess-server + web 观看页（$WEB_DIR）"
else
    echo "    ⚠ 找不到 web 观看页资产，ess-server 起来后无法用浏览器看" >&2
    echo "    （release 包应包含 web/ 目录）" >&2
fi

# 3. systemd
echo "[3/5] 安装 systemd 服务 ..."
cat > "$SERVICE_FILE" << EOF
[Unit]
Description=EasyScreenShare 服务端
After=network.target

[Service]
Type=simple
Environment=ESS_WEB_DIR=$WEB_DIR
ExecStart=$BIN_DIR/ess-server $EXTRA_ARGS
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

# 4. 防火墙（可选）
echo "[4/5] 防火墙 ..."
if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q "Status: active"; then
    ufw allow 7880/tcp >/dev/null 2>&1 || true
    ufw allow 7881/tcp >/dev/null 2>&1 || true
    ufw allow 7882/udp >/dev/null 2>&1 || true
    ufw allow 50000:60000/udp >/dev/null 2>&1 || true
    ufw allow 8088/tcp >/dev/null 2>&1 || true
    echo "    已开放端口 (ufw active)"
elif command -v firewall-cmd >/dev/null && firewall-cmd --state 2>/dev/null | grep -q "running"; then
    firewall-cmd --permanent --add-port=7880/tcp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=7881/tcp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=7882/udp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=50000-60000/udp >/dev/null 2>&1 || true
    firewall-cmd --permanent --add-port=8088/tcp >/dev/null 2>&1 || true
    firewall-cmd --reload >/dev/null 2>&1 || true
    echo "    已开放端口 (firewalld running)"
else
    echo "    未检测到活跃防火墙，跳过（端口需自行确保可达）"
fi

# 5. 启动
echo "[5/5] 启动服务 ..."
systemctl restart easyscreenshare
sleep 3

echo
echo "=== 部署完成 ==="
echo
echo "查看连接信息（观看页地址 + API Key/Secret）："
echo "  journalctl -u easyscreenshare -n 30 --no-pager"
echo
echo "管理："
echo "  systemctl status easyscreenshare"
echo "  systemctl restart easyscreenshare"
echo "  journalctl -u easyscreenshare -f"
echo
echo "卸载：sudo bash $(dirname "$0")/uninstall.sh"
echo
