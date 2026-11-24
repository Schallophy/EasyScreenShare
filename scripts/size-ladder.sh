#!/usr/bin/env bash
# #59 定参实验：把平面**持续**开着（连续视频+弹幕内容 + ESS_MOTION_HOLD 拉大），
# 逐秒记录编码器自报尺寸/帧率、`ql_bw`、以及尺寸决策行，量出"回升阶梯"实际会不会走。
# 用法：scripts/size-ladder.sh [--seconds S] [--bitrate B] [--fps N]
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/ladder
ROOM=ladder
FPS=30 SECS=90 BR=3000000 HOLD=60000
while [[ $# -gt 0 ]]; do
  case "$1" in
    --seconds) shift; SECS=$1 ;;
    --bitrate) shift; BR=$1 ;;
    --fps) shift; FPS=$1 ;;
    --hold) shift; HOLD=$1 ;;
  esac
  shift
done
mkdir -p "$OUT"
taskkill //F //IM ess-share.exe >/dev/null 2>&1
taskkill //F //IM livekit-server.exe >/dev/null 2>&1
sleep 1
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
sleep 3
"$BIN/ess-view" --no-window --seconds $((SECS + 6)) >"$OUT/view.log" 2>&1 &
VIEW=$!
sleep 1
# `--bitrate auto` = 不设 ESS_VIDEO_BITRATE，让码率由带宽估计自己定档——
# 用来验"估计涨了送入尺寸跟不跟着涨"（#59：起步那次折算只在会话开始时做一次）。
SHARE_ENV=(ESS_PERF=1 ESS_DEMO_ANIM=99999 ESS_DEMO_VIDEO=1 ESS_DEMO_W=1920 ESS_DEMO_H=1080
  ESS_MOTION_HOLD="$HOLD" ESS_VIDEO_CODEC=h264)
[ "$BR" = "auto" ] || SHARE_ENV+=(ESS_VIDEO_BITRATE="$BR")
# 展开出来的 `NAME=value` 词会被 bash 当命令名（`ESS_PERF=1: command not found`），
# 必须走 `env`。
env "${SHARE_ENV[@]}" \
  "$BIN/ess-share" --demo --fps "$FPS" --mode smooth --seconds "$SECS" >"$OUT/share.log" 2>&1 &
SHARE=$!
wait "$VIEW"
taskkill //PID "$SHARE" //F >/dev/null 2>&1
taskkill //IM ess-share.exe //F >/dev/null 2>&1
taskkill //PID "$SERVER" //F >/dev/null 2>&1
# 按 PID 杀会漏（实测留下过一只 livekit-server，把下一个脚本的"端口必须干净"挡掉）
taskkill //IM livekit-server.exe //F >/dev/null 2>&1
sleep 1

echo "== 逐秒轨迹（bitrate=$BR，$FPS fps 设定，1920x1080 视频+弹幕内容）"
# 空轨迹要自己喊出来：上一版分享端没起来时这里只是安静地什么都不印，看起来像"没结论"。
grep -qa "^STATS" "$OUT/share.log" || { echo "!! share.log 没有 STATS 行，分享端可能没起来："; tail -5 "$OUT/share.log"; }
# 只喂 STATS 行：`PERF 1s ... enc=333ms` 里的 enc= 是编码耗时，混进来会把轨迹读花。
grep -a "^STATS" "$OUT/share.log" | awk '{for(i=1;i<=NF;i++){if($i~/^enc=/){e=$i} if($i~/^vsent=/){v=$i}
      if($i~/^ql_bw=/){q=$i} if($i~/^target=/){t=$i} if($i~/^egress=/){g=$i}
      if($i~/^forced_plane=/){f=$i} if($i~/^avail=/){a=$i}}
     printf "  s%02d %-22s %-12s %-10s %-14s %-14s %s\n", NR, e, v, q, t, g, a}'
echo "== 尺寸决策行（按出现顺序去重——阶梯要看的是先后，不是次数）"
# 「守门员」也要进来：算力账改用像素付、回升用余量证据这两条分支各印一行，
# 不在这里抓出来就没人知道它们有没有真的生效过（#63）。
grep -a "送入尺寸\|尺寸回升预置\|预热期尺寸\|视频平面\|守门员" "$OUT/share.log" \
  | grep -av "^\[OpenH264\]" | awk '!s[$0]++' | head -20
echo "   （决策行合计 $(grep -ac "送入尺寸\|预热期尺寸\|视频平面\|守门员" "$OUT/share.log")，\
平面切换 $(grep -ac "视频平面" "$OUT/share.log")，\
帧率被动 $(grep -ac "→ [0-9]* fps）" "$OUT/share.log")）"
echo "== 观看端交付（ess-view 的 5 秒窗）"
grep -a "video=" "$OUT/view.log" | tail -6
