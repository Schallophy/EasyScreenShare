#!/usr/bin/env bash
# 生成浏览器观看端的运行时文件（两个都不入库，见 .gitignore）：
#   ess_web.wasm           本仓源码编出来的接收管线
#   livekit-client.umd.js  上游官方构建产物，钉版本 + 钉 sha256
set -uo pipefail
cd "$(dirname "$0")/.."
WEB=crates/ess-web/web
JS_VER=2.23.0
JS_SHA=fb646f2260d84f91238d0a2dc10726c456631d6a26c7cafc43cfee5959b4d31d

[ -f "$WEB/index.html" ] && [ -f "$WEB/app.js" ] || { echo "缺 $WEB/index.html 或 app.js"; exit 1; }

# 1) wasm
if ! rustup target list --installed | grep -qx wasm32-unknown-unknown; then
  echo "缺 wasm32 目标：rustup target add wasm32-unknown-unknown"
  exit 1
fi
cargo build -p ess-web --release --target wasm32-unknown-unknown || exit 1
cp target/wasm32-unknown-unknown/release/ess_web.wasm "$WEB/ess_web.wasm"

# 2) 上游 UMD：哈希已对就不碰网络；下载只落临时文件，核不过不留半成品
want() { [ -f "$1" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$JS_SHA" ]; }
if want "$WEB/livekit-client.umd.js"; then
  echo "livekit-client.umd.js 已是 $JS_VER 且哈希正确，跳过下载"
else
  T=$(mktemp)
  trap 'rm -f "$T"' EXIT
  got=0
  for u in "https://cdn.jsdelivr.net/npm/livekit-client@$JS_VER/dist/livekit-client.umd.js" \
           "https://unpkg.com/livekit-client@$JS_VER/dist/livekit-client.umd.js"; do
    if curl -fsSL -o "$T" "$u" && want "$T"; then
      got=1
      echo "取自 $u"
      break
    fi
  done
  [ "$got" -eq 1 ] || { echo "FAIL: 取不到 livekit-client@$JS_VER 或 sha256 不符（期望 $JS_SHA）"; exit 1; }
  mv "$T" "$WEB/livekit-client.umd.js"
  trap - EXIT
  echo "livekit-client.umd.js：$JS_VER，sha256 已核对"
fi
