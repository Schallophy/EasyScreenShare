#!/usr/bin/env bash
# 视频平面档位扫描：同一台机器、真机 1920x1080 桌面采集、墙钟白闪源，
# 逐个配置测「发送端帧率 / 浏览器呈现帧率 / 丢帧 / 玻璃到玻璃延迟」。
# 用法：bash scripts/video-tier-sweep.sh [秒数]   （默认每档 16 s）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/video-tier
SECS=${1:-16}
ROOM=tiersweep
PORT=8088
PROFILE="C:/Users/Schallophy/code/screenshare/target/edge-viewer"

rm -rf "$OUT"; mkdir -p "$OUT"
taskkill //F //IM ess-share.exe >/dev/null 2>&1
taskkill //F //IM ess-serve.exe >/dev/null 2>&1
taskkill //F //IM livekit-server.exe >/dev/null 2>&1
sleep 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
sleep 3
"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room "$ROOM" --port "$PORT" >"$OUT/serve.log" 2>&1 &
SERVE=$!
sleep 1

# 墙钟对齐的白闪源（全屏），保证每档都在视频平面里出画
powershell -NoProfile -ExecutionPolicy Bypass -File target/video-demo/flash.ps1 \
  -Seconds 400 -X 0 -Y 0 -W 1920 -H 1080 >/dev/null 2>&1 &
FLASH=$!

# 复用同一个 Edge profile：新建 profile 会让 Edge 静默装内置插件并弹安装提示
"/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
  --user-data-dir="$PROFILE" --app=http://127.0.0.1:$PORT/ \
  --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
  --disable-sync --disable-extensions \
  --disable-component-extensions-with-background-pages \
  --window-size=380,300 --window-position=1520,760 >/dev/null 2>&1 &
EDGE=$!
sleep 8
# 观看窗口必须真的在前景可见：被遮挡时 Chrome 停止呈现，
# requestVideoFrameCallback 不触发、droppedVideoFrames 会假报到接近 100%。
powershell -NoProfile -ExecutionPolicy Bypass -File target/video-demo/focus2.ps1 >/dev/null 2>&1
sleep 1

run_tier() { # name fps maxw maxh bitrate
  local name=$1 fps=$2 mw=$3 mh=$4 br=$5
  ESS_PERF=1 ESS_VIDEO_CODEC=h264 ESS_MOTION_HOLD=4000 \
  ESS_VIDEO_MAX_W="$mw" ESS_VIDEO_MAX_H="$mh" ESS_VIDEO_BITRATE="$br" \
    "$BIN/ess-share" --url ws://127.0.0.1:7880 --room "$ROOM" --identity share \
    --fps "$fps" >"$OUT/$name.share.log" 2>&1 &
  local share=$!
  sleep 6
  local probe
  probe=$(node scripts/latency-probe.mjs --mode video --seconds "$SECS" --bright 0.55 \
    --match "127.0.0.1:$PORT" 2>&1 | tail -1)
  # 呈现层数据只有在窗口真可见时才有意义，否则整行作废（发送端 PERF 不受影响）
  case "$probe" in
    *'"vis":"visible"'*) ;;
    *) probe="$probe  <<< INVALID: 观看窗口不可见，呈现帧率/延迟数据无效";;
  esac
  echo "$name|$probe" >> "$OUT/results.txt"
  # 发送端自己那一秒的数据（帧率 + 喂进编码器的帧数）
  grep -E "^PERF|^MOTION|^LINK|视频轨已发布" "$OUT/$name.share.log" | tail -2 >> "$OUT/results.txt"
  kill $share 2>/dev/null; wait $share 2>/dev/null
  sleep 1
}

: > "$OUT/results.txt"
run_tier 60fps-720p-4M   60 1280 720  4000000
run_tier 60fps-1080p-8M  60 1920 1080 8000000
run_tier 60fps-1080p-4M  60 1920 1080 4000000
run_tier 30fps-1080p-6M  30 1920 1080 6000000

kill $EDGE $FLASH $SERVE $SERVER 2>/dev/null
wait 2>/dev/null
echo "===== $OUT/results.txt ====="
cat "$OUT/results.txt"
