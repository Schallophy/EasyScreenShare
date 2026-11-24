#!/usr/bin/env bash
# 从 release 包解压目录安装（等价于 sudo ./ess-server install）
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ "$EUID" -ne 0 ]; then
    echo "需要 root：sudo bash install.sh" >&2
    exit 1
fi
exec "$DIR/ess-server" install "$@"
