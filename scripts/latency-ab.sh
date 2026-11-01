#!/usr/bin/env bash
# #42 采集通路 A/B：**一次开机、交替跑臂**，让机器漂移对两条通路同等作用。
# 单次跑差异 ±25 ms 是这台的常态（dxgi 一次 93、一次 120），所以不做配对就不下结论。
# 用法：scripts/latency-ab.sh [--fps N] [--seconds S] [--reps K]
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
FPS=30 SECS=12 REPS=2
# AXIS=capture（默认，比采集通路）| screencast（比编码器档位 `ESS_VIDEO_SCREENCAST`）
# | bitrate（比送入码率档，其余条件全同）。同一个臂表机制，配对逻辑与漂移控制都一样。
AXIS=${AXIS:-capture}
case "$AXIS" in
  screencast) ARMS="1 0" ;;
  bitrate) ARMS="6000000 12000000" ;;
  *) ARMS="dxgi gdi" ;;
esac
while [[ $# -gt 0 ]]; do
  case "$1" in
    --fps) shift; FPS=$1 ;;
    --seconds) shift; SECS=$1 ;;
    --reps) shift; REPS=$1 ;;
  esac
  shift
done

mkdir -p "$OUT"
taskkill //F //IM ess-share.exe >/dev/null 2>&1
taskkill //F //IM ess-serve.exe >/dev/null 2>&1
taskkill //F //IM livekit-server.exe >/dev/null 2>&1
sleep 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/ab2-server.log" 2>&1 &
SERVER=$!
sleep 3
"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room "$ROOM" --port "$PORT" >"$OUT/ab2-serve.log" 2>&1 &
SERVE=$!
sleep 1

# 白闪源整个会话都开着，两条臂看到的是同一支激励
powershell -NoProfile -ExecutionPolicy Bypass -File target/video-demo/flash.ps1 \
  -Seconds $(( (SECS + 20) * 2 * REPS )) -X 0 -Y 0 -W 1920 -H 1080 >/dev/null 2>&1 &
FLASH=$!

"/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
  --user-data-dir="$PROFILE" --app=http://127.0.0.1:$PORT/ \
  --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
  --disable-sync --disable-extensions \
  --disable-component-extensions-with-background-pages \
  --window-size=380,300 --window-position=1520,760 >/dev/null 2>&1 &
EDGE=$!
sleep 9

: > "$OUT/ab2.txt"
for r in $(seq 1 "$REPS"); do
  for arm in $ARMS; do
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/window-state.ps1 \
      -Match msedge -Restore >/dev/null 2>&1
    case "$AXIS" in
      screencast) AV=(ESS_VIDEO_SCREENCAST="$arm" ESS_VIDEO_BITRATE=6000000) ;;
      bitrate) AV=(ESS_VIDEO_BITRATE="$arm") ;;
      *) AV=(ESS_CAPTURE="$arm" ESS_VIDEO_BITRATE=6000000) ;;
    esac
    env ESS_PERF=1 "${AV[@]}" ESS_VIDEO_CODEC=h264 ESS_VIDEO_MAX_W=1920 ESS_VIDEO_MAX_H=1080 \
      ESS_MOTION_HOLD=600 \
      "$BIN/ess-share" --url ws://127.0.0.1:7880 --room "$ROOM" --identity share \
      --fps "$FPS" >"$OUT/ab2-$arm-$r.log" 2>&1 &
    share=$!
    sleep 7
    src=$(grep -a "采集源" "$OUT/ab2-$arm-$r.log" | head -1 | sed 's/.*（//;s/），.*//')
    line=$(node scripts/latency-probe.mjs --mode video --seconds "$SECS" --bright 0.55 \
      --match "127.0.0.1:$PORT" 2>&1 | tail -1)
    vis=$(echo "$line" | grep -oE '"vis":"[a-z]+"')
    p50=$(echo "$line" | grep -oE '"p50":[0-9]+' | cut -d: -f2)
    lo=$(echo "$line" | grep -oE '"min":[0-9]+' | cut -d: -f2)
    hi=$(echo "$line" | grep -oE '"max":[0-9]+' | cut -d: -f2)
    fr=$(echo "$line" | grep -oE '"frames":[0-9]+' | cut -d: -f2)
    rv=$(echo "$line" | grep -oE '"recv":[0-9.]+' | cut -d: -f2)
    # 画质侧也一起看：观看端真正解码出来的尺寸（比 screencast 档时要用它看代价）
    dm=$(echo "$line" | grep -oE '"dims":\[[0-9]+,[0-9]+\]' | sed 's/.*\[//;s/\]//')
    kb=$(grep -a "视频轨已发布" "$OUT/ab2-$arm-$r.log" | head -1 | sed 's/.*H264 //;s/ bps.*//')
    echo "r$r $arm|$src|码率=$kb|vis=$vis p50=$p50 min=$lo max=$hi 上屏=$fr/$((SECS * FPS)) 接收滞留=${rv}ms 尺寸=$dm" | tee -a "$OUT/ab2.txt"
    taskkill //F //IM ess-share.exe >/dev/null 2>&1
    kill $share 2>/dev/null; wait $share 2>/dev/null
    sleep 2
  done
done

kill $EDGE $FLASH $SERVE $SERVER 2>/dev/null
wait 2>/dev/null
echo "配对结果存 $OUT/ab2.txt（同一次开机内交替，漂移对两臂同等）"
