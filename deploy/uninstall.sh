#!/usr/bin/env bash
# 卸载（等价于 sudo ess-server uninstall）
set -euo pipefail
if [ "$EUID" -ne 0 ]; then
    echo "需要 root：sudo bash uninstall.sh" >&2
    exit 1
fi
if [ -x /usr/local/bin/ess-server ]; then
    exec /usr/local/bin/ess-server uninstall "$@"
fi
# 命令不在（比如手工删过），尽力清理
systemctl stop easyscreenshare 2>/dev/null || true
systemctl disable easyscreenshare 2>/dev/null || true
rm -f /etc/systemd/system/easyscreenshare.service
systemctl daemon-reload 2>/dev/null || true
rm -rf /usr/local/share/easyscreenshare /tmp/easyscreenshare
echo "已清理"
