#!/usr/bin/env bash
# #42 量具判别：观看窗口为什么报 vis:"hidden"。
# 同一套栈里只切换"全屏白闪窗在/不在"，各读一次 document.visibilityState，
# 用来区分「被内容窗遮挡 → Chromium 判定隐藏」和「启动顺序/profile 的问题」。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/latency
ROOM=latency
PORT=8088
PROFILE="C:/Users/Schallophy/code/screenshare/target/edge-viewer"
mkdir -p "$OUT"
taskkill //F //IM ess-share.exe >/dev/null 2>&1
taskkill //F //IM ess-serve.exe >/dev/null 2>&1
taskkill //F //IM livekit-server.exe >/dev/null 2>&1
sleep 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/ab-server.log" 2>&1 &
SERVER=$!
sleep 3
"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room "$ROOM" --port "$PORT" >"$OUT/ab-serve.log" 2>&1 &
SERVE=$!
sleep 1
ESS_PERF=1 ESS_VIDEO_CODEC=h264 ESS_VIDEO_MAX_W=1920 ESS_VIDEO_MAX_H=1080 \
ESS_VIDEO_BITRATE=6000000 ESS_MOTION_HOLD=600 \
  "$BIN/ess-share" --url ws://127.0.0.1:7880 --room "$ROOM" --identity share \
  --fps 30 >"$OUT/ab-share.log" 2>&1 &
SHARE=$!
sleep 5

vis() { # $1 = 标签
  local line
  line=$(node scripts/latency-probe.mjs --mode video --seconds 3 --bright 0.55 \
    --match "127.0.0.1:$PORT" 2>&1 | tail -1)
  echo "$1: $(echo "$line" | grep -oE '"vis":"[a-z]+"|"dims":\[[0-9,]+\]|"frames":[0-9]+|"dropped":[0-9]+|"plane":"[a-z]+"' | tr '\n' ' ')"
}

"/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
  --user-data-dir="$PROFILE" --app=http://127.0.0.1:$PORT/ \
  --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
  --disable-sync --disable-extensions \
  --disable-component-extensions-with-background-pages \
  --window-size=380,300 --window-position=1520,760 >/dev/null 2>&1 &
EDGE=$!
sleep 9

echo "=== A：没有内容窗（只有观看窗）"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/window-state.ps1 -Match msedge 2>&1 | grep -a "pid="
vis A
echo "=== A2：ShowWindow(SW_RESTORE)+SetForegroundWindow 之后再读"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/window-state.ps1 -Match msedge -Restore 2>&1 | grep -a "pid="
sleep 2
vis A2
kill $EDGE $SHARE $SERVE $SERVER 2>/dev/null
wait 2>/dev/null
echo "完成"
