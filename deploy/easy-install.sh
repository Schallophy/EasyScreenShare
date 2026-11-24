#!/usr/bin/env bash
# EasyScreenShare 服务端 —— 一条命令安装/卸载
#
# 安装：
#   curl -fsSL https://raw.githubusercontent.com/Schallophy/EasyScreenShare/main/deploy/easy-install.sh | sudo bash
#
# 带固定密钥：
#   curl -fsSL ... | sudo bash -s -- --key mykey --secret mysecret32chars
#
# 卸载：
#   curl -fsSL ... | sudo bash -s -- --uninstall
#   （或已安装后直接：sudo ess-server uninstall）
set -euo pipefail

REPO="Schallophy/EasyScreenShare"
ASSET="ess-server-linux-x86_64.tar.gz"

if [ "$EUID" -ne 0 ]; then
    echo "需要 root：请用 curl ... | sudo bash 运行" >&2
    exit 1
fi

# 卸载：优先用已安装的命令
if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "uninstall" ]; then
    shift || true
    if [ -x /usr/local/bin/ess-server ]; then
        exec /usr/local/bin/ess-server uninstall "$@"
    fi
    echo "ess-server 未安装，无需卸载"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

URL="https://github.com/$REPO/releases/latest/download/$ASSET"
echo "下载 $URL"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$URL" -o "$TMP/pkg.tar.gz"
else
    wget -qO "$TMP/pkg.tar.gz" "$URL"
fi

tar -xzf "$TMP/pkg.tar.gz" -C "$TMP"
DIR="$TMP/ess-server-linux-x86_64"
[ -x "$DIR/ess-server" ] || { echo "包结构不对：缺 ess-server" >&2; exit 1; }

exec "$DIR/ess-server" install "$@"
