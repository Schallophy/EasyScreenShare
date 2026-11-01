#!/usr/bin/env bash
# 码率阶梯：在**最难压**的内容（全屏逐帧随机噪声 + 持续运动，保证一直待在视频平面）
# 下，看每个码率档编码器实际交出多大分辨率、掉帧多少、发送端是否守住帧率。
# 这一组数据用来定默认码率，不是用来测延迟的（内容里没有白闪参考）。
# 用法：scripts/bitrate-ladder.sh [fps] [档1 档2 ...]   （默认 60fps，2M 4M 6M 10M）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/bitrate
ROOM=ladder
PORT=8088
PROFILE="C:/Users/Schallophy/code/screenshare/target/edge-viewer"
FPS=${1:-60}
shift || true
if [[ $# -gt 0 ]]; then TIERS=("$@"); else TIERS=(2000000 4000000 6000000 10000000); fi
SECS=${SECS:-12}

mkdir -p "$OUT"
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
# 最坏情况内容：全屏逐帧随机色重绘
powershell -NoProfile -ExecutionPolicy Bypass -File target/video-demo/fast.ps1 \
  -Seconds 300 >/dev/null 2>&1 &
FLASH=$!
"/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
  --user-data-dir="$PROFILE" --app=http://127.0.0.1:$PORT/ \
  --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
  --disable-sync --disable-extensions \
  --disable-component-extensions-with-background-pages \
  --window-size=380,300 --window-position=1520,760 >/dev/null 2>&1 &
EDGE=$!
sleep 9
powershell -NoProfile -ExecutionPolicy Bypass -File target/video-demo/focus2.ps1 >/dev/null 2>&1
sleep 1

: > "$OUT/results.txt"
for br in "${TIERS[@]}"; do
  ESS_PERF=1 ESS_VIDEO_CODEC=h264 ESS_VIDEO_MAX_W=1920 ESS_VIDEO_MAX_H=1080 \
  ESS_VIDEO_BITRATE="$br" ESS_MOTION_HOLD=600 \
    "$BIN/ess-share" --url ws://127.0.0.1:7880 --room "$ROOM" --identity share \
    --fps "$FPS" >"$OUT/ess-$br.log" 2>&1 &
  share=$!
  sleep 6
  line=$(node scripts/latency-probe.mjs --mode video --seconds "$SECS" --bright 0.55 \
    --match "127.0.0.1:$PORT" 2>&1 | tail -1)
  perf=$(grep -E "^PERF" "$OUT/ess-$br.log" | tail -1)
  path=$(head -4 "$OUT/ess-$br.log" | grep 采集源 | sed 's/.*（//;s/）.*//')
  kill $share 2>/dev/null; wait $share 2>/dev/null
  echo "$br|$path|$perf|$line" >> "$OUT/results.txt"
  echo "--- ${br} bps / fps=$FPS / 采集=$path"
  echo "    发送端 $perf"
  echo "    观看端 $line"
done

kill $EDGE $FLASH $SERVE $SERVER 2>/dev/null
wait 2>/dev/null
echo "结果存 $OUT/results.txt（观看端 dims = 编码器实际交出的分辨率）"
