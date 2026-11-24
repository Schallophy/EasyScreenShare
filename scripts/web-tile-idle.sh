#!/usr/bin/env bash
# 观看页的图块平面按需订阅：`plane=tiles` 挂满 10 s 就摘掉视频轨订阅，进入平面时挂回来。
#
# 为什么必须有这条：发送端在图块平面下按 ~2 fps 喂保活帧（让切换时画面是当前的），
# 原生观看端有 `VideoMode` 闸门、平面关着时根本不画那些帧；**浏览器没有那道闸门**——
# `TrackSubscribed` 一到就 attach + autoplay，`applyPlane()` 只改 `video.hidden`，
# 于是保活帧在手机上是白收白解。实测图块平面下 inbound-rtp 视频仍有 1440x808@2fps。
#
# 场景：1280x720 合成屏，前 35 s 静止（浏览器约在第 13 s 加入 → 第 23 s 左右摘订阅），
# 之后 10 s 动画把画面逼进视频平面，再静止退出。
#
# 四条断言：
#   1. 图块平面挂满 10 s 后 off=true   —— 订阅真的被摘了（不是只在 UI 上藏起来）
#   2. 摘掉之后 RTP 视频字节不再增长    —— 省下来的是**链路**，不是解码
#   3. 动画开始后 plane 到 video、帧数涨 —— 复订能拿到关键帧，画面不会冻在切换前那一帧
#   4. 复订期间没有长时间停在 video-pending —— 停在那就是"等到超时也没有可解帧"
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/web-tile-idle
PORT=8088
ROOM=demo
PROFILE="$PWD/target/edge-viewer"
EDGEBIN="/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe"
DELAY=${DELAY:-350}      # 0.1 s/格 → 35 s 静止前导
ANIM=${ANIM:-100}
PROBE_SECS=${PROBE_SECS:-40}
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

ESS_PERF=1 ESS_DEMO_W=1280 ESS_DEMO_H=720 ESS_DEMO_ANIM=$ANIM ESS_DEMO_ANIM_DELAY=$DELAY \
  "$BIN/ess-share" --demo --url ws://127.0.0.1:7880 --room "$ROOM" --identity share \
  --fps 30 --mode balanced --seconds $((PROBE_SECS + 40)) >"$OUT/share.log" 2>&1 &
SHARE=$!
sleep 6
grep -q "合成屏" "$OUT/share.log" || echo "  注意：ess-share 可能没采合成屏（少了 --demo？）"
"$EDGEBIN" --user-data-dir="$PROFILE" --app="http://127.0.0.1:$PORT/" \
  --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
  --disable-sync --disable-extensions \
  --disable-component-extensions-with-background-pages \
  --disable-features=CalculateNativeWinOcclusion \
  --window-size=900,600 --window-position=100,100 >"$OUT/edge.log" 2>&1 &
sleep 9
node scripts/tile-idle-probe.mjs --seconds "$PROBE_SECS" >"$OUT/probe.json" 2>"$OUT/probe.err"

pass=0; fail=0
ok()  { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }

if ! node -e 'process.exit(JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")).rows?0:1)' "$OUT/probe.json" 2>/dev/null; then
  echo "FAIL 探针没有输出（$OUT/probe.err）"; exit 1
fi

node -e '
const rows = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).rows;
let pass = 0, fail = 0;
const ok = (m) => { console.log("  PASS " + m); pass++; };
const bad = (m) => { console.log("  FAIL " + m); fail++; };

// 时间线压缩成"状态变化"几行，读起来比 80 行原始采样有用
let last = null;
for (const r of rows) {
  const k = r.plane + "|" + r.off;
  if (k !== last) { console.log(`   t=${(r.t/1000).toFixed(1)}s plane=${r.plane} off=${r.off} vbytes=${r.vbytes} vf=${r.vf}`); last = k; }
}

const offRows = rows.filter((r) => r.off);
if (offRows.length < 4) {
  bad(`退订没发生（off 只有 ${offRows.length} 个采样）`);
} else {
  ok(`图块平面挂满后摘掉了视频轨订阅（共 ${offRows.length} 个采样处于摘除状态）`);
  // 摘掉之后视频 RTP 应当彻底停止增长；音频与 RTCP 不在这个口径里
  const i0 = rows.indexOf(offRows[0]);
  const seg = rows.slice(i0, i0 + Math.min(4, offRows.length));
  const grow = seg[seg.length - 1].vbytes - seg[0].vbytes;
  if (seg.length >= 3 && grow <= 200) {
    // 复订/退订会把 inbound-rtp 这一项整个摘掉，计数从 0 重新开始——所以"不增长"
    // 既可能是原地不动，也可能是这一项消失了（实测是后者：210568 → 0）。
    ok(`摘除后 RTP 视频不再增长（${seg[0].vbytes} → ${seg[seg.length - 1].vbytes} B）`);
  } else bad(`摘除后 RTP 视频仍在增长：${grow} B / ${seg.length} 个采样`);
}

// 平面真的到过 video（不是一直 video-pending）
const vid = rows.filter((r) => r.plane === "video");
const pend = rows.filter((r) => r.plane === "video-pending");
if (!vid.length) bad("平面从未在浏览器里接管（plane 一直是 tiles 或 video-pending）");
else {
  ok(`进入视频平面后浏览器接管了（video 采样 ${vid.length} 个）`);
  const vfGrow = vid[vid.length - 1].vf - vid[0].vf;
  if (vfGrow >= 5) ok(`复订后 <video> 继续出帧（+${vfGrow} 帧）`);
  else bad(`复订后 <video> 只有 ${vfGrow} 帧，画面会冻在切换前`);
  const vb = vid[vid.length - 1].vbytes - vid[0].vbytes;
  if (vb > 10000) ok(`复订后 RTP 视频恢复流动（+${vb} B）`); else bad(`复订后 RTP 视频只有 ${vb} B`);
}
if (pend.length * 0.5 > 3) bad(`停在 video-pending 达 ${(pend.length * 0.5).toFixed(1)} s（等不到可解帧）`);
else ok(`video-pending 只持续 ${(pend.length * 0.5).toFixed(1)} s（关键帧等得不长）`);

console.log(`  结果：${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
' "$OUT/probe.json"
rc=$?
echo
echo "原始时间线：$OUT/probe.json"
exit $rc
