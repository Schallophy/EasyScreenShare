#!/usr/bin/env bash
# 浏览器观看演示：LiveKit + 观看页（静态 + 令牌）+ 共享端。
# 用法：scripts/demo-browser.sh [--real]   （--real 采集主屏，默认合成屏）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/browser-demo
WASM=target/wasm32-unknown-unknown/release/ess_web.wasm
PORT=8088

if [[ ! -f crates/ess-web/web/ess_web.wasm || ! -f crates/ess-web/web/livekit-client.umd.js ]]; then
  echo "缺少网页端产物。先构建："
  echo "  cargo build -p ess-web --release --target wasm32-unknown-unknown"
  echo "  cp $WASM crates/ess-web/web/"
  exit 1
fi
if [[ ! -x $BIN/ess-serve || ! -x $BIN/ess-share ]]; then
  echo "先构建：cargo build --release -p ess-app"
  exit 1
fi

rm -rf "$OUT"; mkdir -p "$OUT"
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
sleep 3
"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room demo --port "$PORT" >"$OUT/serve.log" 2>&1 &
SERVE=$!
sleep 1

if [[ "${1:-}" == "--real" ]]; then
  "$BIN/ess-share" --room demo >"$OUT/share.log" 2>&1 &
else
  "$BIN/ess-share" --demo --fps 10 --room demo >"$OUT/share.log" 2>&1 &
fi
SHARE=$!

echo "观看页: http://127.0.0.1:$PORT/"
echo "（共享端日志 $OUT/share.log；Ctrl+C 结束）"
trap 'kill $SHARE $SERVE $SERVER 2>/dev/null; wait 2>/dev/null' INT TERM
wait $SHARE
