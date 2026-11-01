#!/usr/bin/env bash
# 一键玻璃到玻璃延迟测量：墙钟对齐的白闪源 + 真机采集 + 浏览器观看窗口（必须可见）。
# 用法：
#   scripts/latency-run.sh [--fps N] [--maxw W] [--maxh H] [--bitrate B] [--seconds S] [--codec c]
# 说明：
#   - 观看窗口用一个**固定复用**的 Edge profile（target/edge-viewer），并显式关掉扩展与同步。
#     每次新建 profile 会让 Edge 静默安装内置插件并弹一堆"安装成功"提示，很扰民。
#   - 结果里的 vis 必须是 visible，否则 dropped/呈现帧率全是假的（脚本会直接判无效）。
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
FPS=30 MAXW=1920 MAXH=1080 BR=6000000 SECS=20 CODEC=h264
# --auto：完全不设分辨率与码率，跑产品默认档（码率由帧率×像素自动定）
AUTO=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --fps) shift; FPS=$1 ;;
    --maxw) shift; MAXW=$1 ;;
    --maxh) shift; MAXH=$1 ;;
    --bitrate) shift; BR=$1 ;;
    --seconds) shift; SECS=$1 ;;
    --codec) shift; CODEC=$1 ;;
    --auto) AUTO=1 ;;
  esac
  shift
done

mkdir -p "$OUT"
taskkill //F //IM ess-share.exe >/dev/null 2>&1
taskkill //F //IM ess-serve.exe >/dev/null 2>&1
taskkill //F //IM livekit-server.exe >/dev/null 2>&1
sleep 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
sleep 3
[[ "$(netstat -ano | grep -c ':7880.*LISTENING')" == "1" ]] || { echo "服务器没起来"; exit 2; }

"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room "$ROOM" --port "$PORT" >"$OUT/serve.log" 2>&1 &
SERVE=$!
sleep 1

# 全屏白闪源：每 2 s 的前 100 ms 纯白，相位对齐 Unix 毫秒 epoch（与探针同一时钟）
powershell -NoProfile -ExecutionPolicy Bypass -File target/video-demo/flash.ps1 \
  -Seconds $((SECS + 40)) -X 0 -Y 0 -W 1920 -H 1080 >/dev/null 2>&1 &
FLASH=$!

"/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
  --user-data-dir="$PROFILE" --app=http://127.0.0.1:$PORT/ \
  --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
  --disable-sync --disable-extensions \
  --disable-component-extensions-with-background-pages \
  --window-size=380,300 --window-position=1520,760 >/dev/null 2>&1 &
EDGE=$!
sleep 9
# 观看窗必须**真的在屏上**：Chromium 的原生遮挡检测会在窗口被别的窗口盖满 /
# 处于最小化时把 document.visibilityState 判成 hidden，此时 rVFC 不触发、
# 呈现帧全被丢掉，测出来的"延迟"是量具的锅。focus2.ps1 那套 AppActivate 只发
# 激活消息、不解最小化，实测同一台机器上同一份脚本两次跑出 hidden / visible
# 两种结果（2026-11-01）。这里直接 SW_RESTORE + SetForegroundWindow，并把窗口
# 矩形打出来，读数才有据可查。
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/window-state.ps1 \
  -Match msedge -Restore >/dev/null 2>&1
sleep 1

VOPT=()
[[ "$AUTO" == "1" ]] || VOPT=(ESS_VIDEO_MAX_W="$MAXW" ESS_VIDEO_MAX_H="$MAXH" ESS_VIDEO_BITRATE="$BR")
ESS_PERF=1 ESS_VIDEO_CODEC="$CODEC" ESS_MOTION_HOLD=600 \
  env "${VOPT[@]}" "$BIN/ess-share" --url ws://127.0.0.1:7880 --room "$ROOM" \
  --identity share --fps "$FPS" >"$OUT/share.log" 2>&1 &
SHARE=$!
sleep 7

probe=$(node scripts/latency-probe.mjs --mode video --seconds "$SECS" --bright 0.55 \
  --match "127.0.0.1:$PORT" 2>&1 | tail -1)
# 量具自检：EnumWindows 从上往下返回，z0=最顶层。观看窗若被内容窗压在下面，
# Chromium 的原生遮挡检测就判 hidden，rVFC 一律不触发。
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/window-state.ps1 -All -Limit 8 \
  2>&1 | grep -a "^z" | sed 's/title=.*/title=DOT/'
kill $SHARE $EDGE $FLASH $SERVE $SERVER 2>/dev/null
wait 2>/dev/null

perf=$(grep -E "^PERF" "$OUT/share.log" | tail -1)
grep -E "采集源|视频轨已发布" "$OUT/share.log" | head -2
echo "发送端: $perf"
echo "观看端: $probe"
case "$probe" in
  *'"vis":"visible"'*) echo "VALID";;
  *) echo "INVALID：观看窗口不可见，呈现层数据作废";;
esac
