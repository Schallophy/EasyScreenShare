#!/usr/bin/env bash
# 观看页「详情」里传输速率读数的机制验收（真 livekit + 真编码器 + 真 Edge）。
#
# 为什么必须用真浏览器：速率是从 WebRTC 的 getStats 里读的，而 LiveKit 的 umd 不导出
# 它——app.js 靠"在 SDK 之下包一层 RTCPeerConnection 构造函数"才拿到 pc。这条链一旦
# 断（SDK 换了建 pc 的方式、或页面里没有 pc），详情里的数字会静默变成 0，不报任何错。
# 三臂各锁一条：
#   A 视频平面：媒体有流量、图块很小；两条路之和 == 显示的下行；与 transport 的
#     真实接收量同量级（差额只该是头/RTCP 这点零头）
#   B 纯图块（ESS_VIDEO=0）：图块有流量、媒体只剩音频——证明两路没有互相串
#   C 静止（ESS_DEMO_ANIM=0）：下行回落到零头，不再挂着上一帧的速率
# 用法：bash scripts/net-rate.sh   （SECS 可覆盖每臂采样秒数）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/net-rate
PORT=8088
ROOM=demo
PROFILE="$PWD/target/edge-viewer"
EDGEBIN="/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe"
SECS=${SECS:-8}
SERVE=0; SHARE=0

[ -x "$BIN/ess-serve.exe" ] && [ -x "$BIN/ess-share.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }
[ -x "$EDGEBIN" ] || { echo "找不到 Edge：$EDGEBIN"; exit 1; }
[ -f crates/ess-web/web/ess_web.wasm ] || { echo "缺 ess_web.wasm，浏览器端测不了"; exit 1; }
for p in 7880 "$PORT" 9223; do
  if [ -n "$(netstat -ano | grep ":$p.*LISTENING" || true)" ]; then
    echo "$p 已被占用（多半是上一轮残留的 Edge 或服务端），先手工停掉那个进程再跑"
    exit 1
  fi
done

rm -rf "$OUT"; mkdir -p "$OUT"
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!

# 调试端口由**真正的浏览器进程**持有，`msedge.exe` 启动器一退出就和我们记下的 PID 不是
# 同一个进程——所以按端口找 PID 杀。杀之前核对命令行里有没有本脚本复用的 profile，
# 用户的其它 Edge 窗口绝不能被牵连。
kill_edge() {
  local pid cmd
  pid=$(netstat -ano | grep ':9223.*LISTENING' | awk '{print $NF}' | head -1 || true)
  [ -z "${pid:-}" ] && return 0
  cmd=$(powershell -NoProfile -Command "(Get-CimInstance Win32_Process -Filter 'ProcessId=$pid').CommandLine" 2>/dev/null || true)
  case "$cmd" in
    *edge-viewer*) taskkill //PID "$pid" //T //F >/dev/null 2>&1 || true ;;
    *) echo "  注意：9223 被非本脚本的进程 $pid 占用，不动它（探针可能连到错的窗口）" ;;
  esac
}
trap 'kill_edge; for pid in "$SERVER" "$SERVE" "$SHARE"; do [ "${pid:-0}" != 0 ] && taskkill //PID "$pid" //F >/dev/null 2>&1; done; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-serve.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1' EXIT
sleep 3
grep -qi "address already in use" "$OUT/server.log" && { echo "FAIL 服务端没绑上 7880"; exit 1; }
"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room "$ROOM" --port "$PORT" >"$OUT/serve.log" 2>&1 &
SERVE=$!
sleep 1

pass=0; fail=0
ok()  { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }

# $1=臂 $2=ESS_VIDEO(0/1) $3=合成屏动画帧数（0 = 完全静止）
run() {
  local A=$1 VID=$2 ANIM=$3
  kill_edge
  echo "== 臂 $A（1920x1080，视频平面 $([ "$VID" = 0 ] && echo 关 || echo 开)，动画 $([ "$ANIM" = 0 ] && echo 关 || echo 开)）"
  ESS_PERF=1 ESS_DEMO_ANIM=$ANIM ESS_DEMO_VIDEO=$VID ESS_VIDEO=$VID ESS_DEMO_W=1920 ESS_DEMO_H=1080 \
    "$BIN/ess-share" --demo --url ws://127.0.0.1:7880 --room "$ROOM" --identity share --fps 20 \
    --mode balanced --seconds $((SECS + 24)) >"$OUT/$A-share.log" 2>&1 &
  SHARE=$!
  sleep 6
  # 素材自证：少了 --demo 就会去采真桌面，桌面静止时读数"看着不对"其实是采错了内容
  if ! grep -q "合成屏" "$OUT/$A-share.log"; then
    bad "$A 采的不是合成屏内容（ess-share 少了 --demo？），这一臂读数作废"
    taskkill //PID "$SHARE" //F >/dev/null 2>&1
    return
  fi
  "$EDGEBIN" --user-data-dir="$PROFILE" --app="http://127.0.0.1:$PORT/" \
    --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
    --disable-sync --disable-extensions \
    --disable-component-extensions-with-background-pages \
    --disable-features=CalculateNativeWinOcclusion \
    --window-size=900,600 --window-position=100,100 >"$OUT/$A-edge.log" 2>&1 &
  sleep 9
  node scripts/net-rate-probe.mjs --seconds "$SECS" --shot "$OUT/$A-hud.png" \
    >"$OUT/$A.json" 2>"$OUT/$A.err"
  kill_edge
  taskkill //PID "$SHARE" //F >/dev/null 2>&1
  sleep 2
  if ! node -e 'process.exit(JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")).rows?0:1)' "$OUT/$A.json" 2>/dev/null; then
    bad "$A 探针没有输出（$OUT/$A.err）"
    return
  fi
  node -e 'const j=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));
    const k=bps=>bps>=1e6?(bps/1e6).toFixed(2)+" Mbps":(bps/1e3).toFixed(1)+" kbps";
    console.log("  读数：下行 "+k(j.net.down)+"（图块 "+k(j.net.tiles)+" · 媒体 "+k(j.net.media)+"）上行 "+k(j.net.up));
    console.log("  transport rx="+j.detail.transport_rx+" B，rtp="+j.rtp_bytes+" B，dc rx="+j.net.rx+" B，pc="+j.rows[j.rows.length-1].pc);
    console.log("  HUD："+j.hud.split("\n").join(" | "));' "$OUT/$A.json"
}

val() { node -e 'const j=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));
  const f=process.argv[2].split(".").reduce((o,k)=>o&&o[k],j);console.log(f===undefined?"nil":f);' "$OUT/$1.json" "$2"; }
gt() { local v=$1 t=$2; [ "$v" != nil ] && awk -v a="$v" -v b="$t" 'BEGIN{exit !(a>b)}'; }
lt() { local v=$1 t=$2; [ "$v" != nil ] && awk -v a="$v" -v b="$t" 'BEGIN{exit !(a<b)}'; }

run A 1 99999
run B 0 99999
run C 0 0

echo
echo "门禁"
[ "$(val A sum_ok)" = "true" ] && ok "A 图块 + 媒体 == 显示的下行" || bad "A 下行不是两条路之和"
gt "$(val A net.media)" 100000 && ok "A 视频平面：媒体有流量（$(val A net.media) B/s）" || bad "A 媒体没有流量（$(val A net.media) B/s），getStats 链断了？"
gt "$(val A detail.transport_rx)" 0 && ok "A transport 有接收量" || bad "A transport 接收量为 0"
# 两条路之和与 transport 的差额只该是头/RTCP 这类零头；漏了整条通路这里会差一倍以上
node -e 'const j=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));
  const ours=j.net.rx+j.rtp_bytes, all=j.detail.transport_rx;
  const d=all?Math.abs(all-ours)/all:1;
  process.exit(d<0.35?0:1)' "$OUT/A.json" && ok "A 两条路之和与 transport 接收量同量级（差 < 35%）" || bad "A 两条路对不上 transport"
gt "$(val B net.tiles)" 200000 && ok "B 纯图块：图块通道有流量（$(val B net.tiles) B/s）" || bad "B 图块通道没有流量（$(val B net.tiles) B/s）"
gt "$(val B net.media)" 60000 && bad "B 图块模式里媒体仍有 $(val B net.media) B/s，两条路串了" || ok "B 纯图块：媒体只剩音频零头（$(val B net.media) B/s）"
[ "$(val C net.down)" = nil ] && bad "C 没有读数" \
  || { # 阈值给到 60 kbps：静止画面仍会按 IDR 周期补少量帧，1080p 静止实测 16 kbps 上下；
# 动画时是 216~545 kbps，一个数量级的差已经说明读数在跟着流量走。
  lt "$(val C net.down)" 60000 && ok "C 静止：下行回落到 $(val C net.down) B/s" || bad "C 静止后下行仍为 $(val C net.down) B/s"; }

echo
echo "结果：$pass passed, $fail failed（截图 $OUT/A-hud.png）"
[ "$fail" -eq 0 ]
