#!/bin/bash
# EasyScreenShare 卸载
set -e
systemctl stop easyscreenshare 2>/dev/null || true
systemctl disable easyscreenshare 2>/dev/null || true
rm -f /etc/systemd/system/easyscreenshare.service
systemctl daemon-reload
rm -f /usr/local/bin/ess-server
rm -f /usr/local/bin/livekit-server
rm -rf /tmp/easyscreenshare   # LiveKit 日志与配置
echo "已卸载"
