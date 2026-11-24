#!/usr/bin/env bash
# 出分享端的安装包，并且**自证**它可用：
#   1) 产物存在（NSIS，当前用户安装，不需要管理员）；
#   2) 把 exe 拷到源码树之外的临时目录，还能建窗、开播，观看端收到完整画布。
# 第二条是"能装"之外真正的门槛：Tauri 产物偶尔会缺 WebView2Loader 之类的运行时
# 依赖，只在 target/release 里跑是看不出来的。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
VER=$(sed -nE 's/.*"version": "([^"]+)".*/\1/p' crates/ess-share-ui/tauri.conf.json | head -1)

( cd crates/ess-share-ui && npx --yes @tauri-apps/cli@2 build ) || exit 1

NSIS="$BIN/bundle/nsis/ScreenShare_${VER}_x64-setup.exe"
if [ ! -f "$NSIS" ]; then
  echo "FAIL: 没找到安装包 $NSIS"; ls -R "$BIN/bundle" 2>/dev/null | head -20; exit 1
fi
echo "安装包：$NSIS ($(du -h "$NSIS" | cut -f1), $(stat -c%s "$NSIS") bytes)"

if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端"; exit 1
fi
T=$(mktemp -d)
# 任何一条 exit 路径都要先把打包 exe 收掉（它在任何检查失败时都可能还在跑），
# 再删临时目录、停服务端。
trap 'taskkill //IM ScreenShare.exe //F >/dev/null 2>&1; rm -rf "$T"; kill $SERVER 2>/dev/null' EXIT
cp "$BIN/ess-share-ui.exe" "$T/ScreenShare.exe"

"$LKSERVER" --dev --bind 127.0.0.1 >"$T/server.log" 2>&1 &
SERVER=$!
sleep 3
"$T/ScreenShare.exe" --autostart --demo --fps 30 >"$T/app.log" 2>&1 &
sleep 9
grep -q "控制窗 main 从采集里隐藏: ok" "$T/app.log" && echo "控制窗排除：ok" || echo "控制窗排除：没报 ok（看 app.log）"
# 会话日志是发给 webview 的，exe 的 stdout 里没有「系统声音已共享」这行，所以这一路
# 只能按行为验：观看端必须真的收到音频块（0 就是打包漏了 ess-audio 或音频轨发布失败）。
"$BIN/ess-view" --no-window --seconds 6 --dump "$T/frame.ppm" >"$T/view.log" 2>&1
SZ=$(stat -c%s "$T/frame.ppm" 2>/dev/null || echo 0)
ARATE=$(grep -o 'audio=[0-9.]*块/s' "$T/view.log" | tail -1 | sed 's/audio=//;s/块\/s//')
awk -v a="${ARATE:-0}" 'BEGIN { exit (a+0 > 50) ? 0 : 1 }' || {
  echo "PORTABLE FAIL: 观看端没收到打包产物发出的系统声音（到达率 ${ARATE:-0} 块/s）"; tail -5 "$T/view.log"; exit 1
}
# 1280x720 PPM 头 17 字节 + 2764800 像素字节
if [ "$SZ" -gt 2764800 ]; then
  echo "PORTABLE PASS: 源码树之外的 exe 开播成功，观看端收到完整画布（$SZ bytes）+ 系统声音 ${ARATE} 块/s"
else
  echo "PORTABLE FAIL: 观看端只拿到 $SZ bytes（见 $T/view.log）"; tail -3 "$T/view.log"; exit 1
fi
taskkill //IM ScreenShare.exe //F >/dev/null 2>&1
