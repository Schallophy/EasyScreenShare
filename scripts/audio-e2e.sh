#!/usr/bin/env bash
# 系统声音端到端验收：本机播放 440 Hz 正弦 → 分享端 WASAPI 环回 → LiveKit 音频轨
# → 观看端解出 PCM 落盘 → 频点分析。
#
# 两个用例，顺序不能反：
#   off（负对照，--no-audio）—— 观看端必须 0 块。没有这条，"收到 N 块"可能只是
#       统计口径自己算了个东西，而不是真的有一路声音穿过来了。
#   on （默认共享声音）—— 到达率应接近满速 100 块/s，且 g(440) 远高于对照频点，
#       证明穿过 Opus 回来的确实是那个音（RMS 单独不为 0 只能证明"有响声"）。
#
# 观看端故意不接扬声器：分享端和观看端同机时播放会被环回再次抓走，形成回声环。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/audio-e2e
SECS=12

rm -rf "$OUT"; mkdir -p "$OUT"
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi

"$BIN/ess-tone" "$OUT/tone.wav" --freq 440 --seconds $((SECS * 2 + 14)) || exit 1
TONEW="$(cd "$OUT" && pwd -W)"
powershell -NoProfile -Command "(New-Object Media.SoundPlayer '$TONEW/tone.wav').PlaySync()" &
TONE=$!
sleep 2

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'kill "$TONE" "$SERVER" 2>/dev/null; wait 2>/dev/null' EXIT
sleep 3

run_case() {
  tag=$1; shift
  "$BIN/ess-view" --no-window --identity "v-$tag" --seconds "$SECS" \
    --audio-dump "$OUT/$tag.wav" >"$OUT/$tag-view.log" 2>&1 &
  V=$!
  sleep 1
  ESS_PERF=1 "$BIN/ess-share" --demo --fps 10 --seconds "$SECS" "$@" \
    >"$OUT/$tag-share.log" 2>&1 &
  S=$!
  wait "$V"
  kill "$S" 2>/dev/null; wait "$S" 2>/dev/null

  echo "── $tag"
  grep -E "系统声音" "$OUT/$tag-share.log" | head -3
  grep -E "audio" "$OUT/$tag-view.log" | head -3
  grep -E "^audio dumped" "$OUT/$tag-view.log"
  node scripts/audio-analyze.mjs "$OUT/$tag.wav" 440 1000 2>&1 | tail -1
}

run_case off --no-audio
run_case on

echo "── 到达率（观看端每秒块数，满速 100）"
for t in off on; do
  printf '%-5s %s\n' "$t" "$(grep -o 'audio=[0-9.]*块/s' "$OUT/$t-view.log" | tail -1)"
done
