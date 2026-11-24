#!/usr/bin/env bash
# DTX A/B：静音到底要不要花钱。
#
# 观看端的"块/s"不能回答这个问题——接收端音频 sink 空了也照样按 10 ms 交静音帧。
# 唯一的口径是发送端 audio outbound-rtp 的字节增量（ess-session 的 STATS 行）。
# 四格都要跑：只比"开"不看"关"，就不知道它到底省了多少，也不知道它有没有把
# 真声音一起省掉。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/audio-dtx
SECS=12

rm -rf "$OUT"; mkdir -p "$OUT"
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi

"$BIN/ess-tone" "$OUT/tone.wav" --freq 440 --seconds $((SECS * 4 + 20)) || exit 1
TONEW="$(cd "$OUT" && pwd -W)"

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'kill "$SERVER" "${TONE:-}" 2>/dev/null; wait 2>/dev/null' EXIT
sleep 3

play_tone() {
  powershell -NoProfile -Command "(New-Object Media.SoundPlayer '$TONEW/tone.wav').PlaySync()" &
  TONE=$!
  sleep 2
}
stop_tone() {
  if [ -n "${TONE:-}" ]; then kill "$TONE" 2>/dev/null; wait "$TONE" 2>/dev/null; TONE=; fi
}

# $1=状态(silent|tone) $2=DTX(1|0)
run_cell() {
  state=$1; dtx=$2
  if [ "$state" = tone ]; then play_tone; else stop_tone; fi
  "$BIN/ess-view" --no-window --identity "v-$state-$dtx" --seconds $SECS \
    >"$OUT/$state-$dtx-view.log" 2>&1 &
  V=$!
  sleep 1
  ESS_PERF=1 ESS_AUDIO_DTX=$dtx "$BIN/ess-share" --demo --fps 10 --seconds $SECS \
    >"$OUT/$state-$dtx-share.log" 2>&1 &
  S=$!
  wait "$V"
  kill "$S" 2>/dev/null; wait "$S" 2>/dev/null
  # 前两格落在预热里（整帧、订阅建立），从第三条 STATS 起算
  awk '/STATS /{ if (match($0, /audio=[0-9.]+kbps/)) { v=substr($0, RSTART+6, RLENGTH-6)+0; n++; if (n>2) { s+=v; c++ } } }
       /STATS /{ if (match($0, /audiotgt=[0-9]+/)) { t=substr($0, RSTART+9, RLENGTH-9)+0 } }
       /STATS /{ if (match($0, /pkt=[0-9.]+/)) { p=substr($0, RSTART+4, RLENGTH-4)+0 } }
       END { printf "%.1f %.0f %.0f", (c>0 ? s/c : 0), t, p }' "$OUT/$state-$dtx-share.log" >"$OUT/$state-$dtx-avg"
  r=$(grep -o 'audio=[0-9.]*块/s' "$OUT/$state-$dtx-view.log" | tail -1 | sed 's/audio=//;s/块\/s//')
  read -r kbps tg pkt <"$OUT/$state-$dtx-avg"
  printf '%-6s %-4s %8s kbps  目标 %6s  %5s pkt/s  %6s 块/s\n' \
    "$state" "$dtx" "$kbps" "$tg" "$pkt" "${r:-—}"
}

printf '%-6s %-4s %s\n' 状态 DTX "声音上行（字节增量）/ 编码器目标 / 发包速率 / 观看端到达率"
run_cell silent 0
run_cell silent 1
run_cell tone 0
run_cell tone 1
stop_tone
