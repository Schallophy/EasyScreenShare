#!/usr/bin/env bash
# #42 归因：把"固定那 ~60 ms"拆开——同机 localhost、同一个墙钟对齐白闪源，
# 只换通路：
#   tiles  = ESS_VIDEO=0（平面全程关闭）+ 合成屏白闪，探针 --mode canvas 读**数据通路**
#            （画布代数变化那刻，不含浏览器合成上屏）
#   video  = 真屏幕白闪 + --mode video 读**呈现层**（rVFC，含浏览器解码与合成）
# 用法：scripts/latency-tiles.sh [--fps N] [--seconds S]
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
FPS=30 SECS=20
while [[ $# -gt 0 ]]; do
  case "$1" in
    --fps) shift; FPS=$1 ;;
    --seconds) shift; SECS=$1 ;;
  esac
  shift
done

mkdir -p "$OUT"
taskkill //F //IM ess-share.exe >/dev/null 2>&1
taskkill //F //IM ess-serve.exe >/dev/null 2>&1
taskkill //F //IM livekit-server.exe >/dev/null 2>&1
sleep 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/tl-server.log" 2>&1 &
SERVER=$!
sleep 3
"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room "$ROOM" --port "$PORT" >"$OUT/tl-serve.log" 2>&1 &
SERVE=$!
sleep 1

"/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
  --user-data-dir="$PROFILE" --app=http://127.0.0.1:$PORT/ \
  --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
  --disable-sync --disable-extensions \
  --disable-component-extensions-with-background-pages \
  --window-size=380,300 --window-position=1520,760 >/dev/null 2>&1 &
EDGE=$!
sleep 9
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/window-state.ps1 \
  -Match msedge -Restore >/dev/null 2>&1

# 通路一：合成屏白闪（静止底 + 每 2 s 前 100 ms 纯白），平面关掉 → 只走无损图块
ESS_PERF=1 ESS_VIDEO=0 ESS_DEMO_FLASH=1 ESS_DEMO_ANIM=0 ESS_DEMO_W=1920 ESS_DEMO_H=1080 \
  "$BIN/ess-share" --url ws://127.0.0.1:7880 --room "$ROOM" --identity share --demo \
  --fps "$FPS" >"$OUT/tl-tiles.log" 2>&1 &
SHARE=$!
sleep 7
echo "--- tiles（无损图块通路，数据到画布）"
node scripts/latency-probe.mjs --mode canvas --seconds "$SECS" --match "127.0.0.1:$PORT" 2>&1 | tail -1
kill $SHARE 2>/dev/null; wait $SHARE 2>/dev/null

# 通路二：同一份合成屏白闪**继续开着**，只把平面放回来 → 走 H264，读呈现层
ESS_PERF=1 ESS_DEMO_FLASH=1 ESS_DEMO_ANIM=0 ESS_DEMO_W=1920 ESS_DEMO_H=1080 \
ESS_MOTION_HOLD=600 ESS_VIDEO_CODEC=h264 \
  "$BIN/ess-share" --url ws://127.0.0.1:7880 --room "$ROOM" --identity share --demo \
  --fps "$FPS" >"$OUT/tl-video.log" 2>&1 &
SHARE=$!
sleep 7
echo "--- video（H264 平面，presented）"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/window-state.ps1 \
  -Match msedge -Restore >/dev/null 2>&1
node scripts/latency-probe.mjs --mode video --seconds "$SECS" --bright 0.55 \
  --match "127.0.0.1:$PORT" 2>&1 | tail -1
grep -E "视频平面 (开|关)|采集源" "$OUT/tl-video.log" | tail -3

kill $EDGE $SHARE $SERVE $SERVER 2>/dev/null
wait 2>/dev/null
echo "完成"
