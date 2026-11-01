#!/usr/bin/env bash
# 端到端验收：真实 LiveKit 链路，合成屏 → 图块管线 → 观看端画布，逐字节比对。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/e2e
rm -rf "$OUT"; mkdir -p "$OUT"

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
sleep 3

"$BIN/ess-gen-expected" "$OUT/expected.ppm" --w 1280 --h 720 --anim 30

"$BIN/ess-view" --no-window --seconds 14 --dump "$OUT/received.ppm" >"$OUT/view.log" 2>&1 &
VIEW=$!
sleep 1
"$BIN/ess-share" --demo --fps 10 --seconds 16 >"$OUT/share.log" 2>&1 &
SHARE=$!

wait $VIEW
kill $SHARE 2>/dev/null
wait $SHARE 2>/dev/null
kill $SERVER 2>/dev/null
wait $SERVER 2>/dev/null

echo "--- view tail ---"; tail -3 "$OUT/view.log"
echo "--- share tail ---"; tail -3 "$OUT/share.log"
if cmp -s "$OUT/expected.ppm" "$OUT/received.ppm"; then
  echo "E2E PASS: received canvas == expected final frame ($(stat -c%s "$OUT/received.ppm") bytes)"
else
  echo "E2E FAIL: frames differ"
  ls -la "$OUT"
  exit 1
fi
