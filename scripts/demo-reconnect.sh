#!/usr/bin/env bash
# 断线重连验收（两个阶段，缺一不可）：
#   阶段 1 短暂中断：服务器杀掉 2 s 后原端口重启
#     → 发送端必须走 Reconnecting → Reconnected 并自动恢复视频轨、补发整帧
#     → 恢复后观看端画布仍逐字节等于期望终帧（旧画面不能永久留在画布上）
#   阶段 2 永久中断：服务器杀掉不再重启
#     → 发送端必须在 ESS_RECONNECT_GRACE_S 之内以错误结束会话（不能永远挂在"共享中"）
# 注意：每次杀完进程都要确认 7880 真的没人监听了——之前有一次测试用 taskkill 打
# 算杀服务器却打偏了，"断线"根本没发生，结论全是假的。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/reconnect
rm -rf "$OUT"; mkdir -p "$OUT"

listening() { netstat -ano | grep -c ':7880.*LISTENING'; }
kill_server() {
  kill "$1" 2>/dev/null
  wait "$1" 2>/dev/null
  for _ in 1 2 3 4 5; do
    [[ "$(listening)" == "0" ]] && return 0
    sleep 1
  done
  echo "FATAL: 服务器没真的停掉（7880 仍在监听），测试结论无效" >&2
  exit 2
}


# 预检：端口必须干净。之前有一次别的实验把 livekit-server 留在 7880 上，
# 脚本自己起的服务绑不上端口直接退出，`kill $SERVER` 杀的是那个已死的进程，
# 于是"断线"根本没发生——整轮结论都是假的。
if [[ "$(listening)" != "0" ]]; then
  echo "FATAL: 7880 已被占用，先杀掉残留的 livekit-server 再跑" >&2
  exit 2
fi

echo "===== 阶段 1：2 s 中断后恢复 ====="
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
sleep 3
"$BIN/ess-gen-expected" "$OUT/expected.ppm" --w 1280 --h 720 --anim 30
"$BIN/ess-view" --no-window --seconds 22 --dump "$OUT/received.ppm" >"$OUT/view.log" 2>&1 &
VIEW=$!
sleep 1
"$BIN/ess-share" --demo --fps 10 --seconds 24 >"$OUT/share.log" 2>&1 &
SHARE=$!
sleep 8
kill_server $SERVER
echo "--- 服务器杀掉 $(date +%T) ---" >> "$OUT/timeline.log"
sleep 2
"$LKSERVER" --dev --bind 127.0.0.1 >>"$OUT/server.log" 2>&1 &
SERVER=$!
echo "--- 服务器重启 $(date +%T) ---" >> "$OUT/timeline.log"

wait $VIEW
kill $SHARE 2>/dev/null; wait $SHARE 2>/dev/null
kill_server $SERVER

echo "--- 发送端连接生命周期 ---"
grep -nE "重连|断开|中断|视频轨" "$OUT/share.log" || echo "（没有重连相关日志）"
echo "--- 观看端尾部 ---"; tail -3 "$OUT/view.log"
if cmp -s "$OUT/expected.ppm" "$OUT/received.ppm"; then
  echo "PHASE1 PASS: 重连后画布 == 期望终帧 ($(stat -c%s "$OUT/received.ppm") bytes)"
else
  echo "PHASE1 FAIL: 画布与期望终帧不一致"; ls -la "$OUT"; exit 1
fi

echo "===== 阶段 2：永久中断，要求有界失败 ====="
GRACE=8
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server2.log" 2>&1 &
SERVER=$!
sleep 3
ESS_RECONNECT_GRACE_S=$GRACE ESS_PERF=1 \
  "$BIN/ess-share" --demo --fps 10 --seconds 120 >"$OUT/share2.log" 2>&1 &
SHARE=$!
sleep 6
kill_server $SERVER
T0=$SECONDS
exited=no
for _ in $(seq 1 12); do
  sleep 2
  if ! kill -0 $SHARE 2>/dev/null; then exited=yes; break; fi
done
USED=$((SECONDS - T0))
echo "--- 发送端在服务器消失后 ${USED}s 退出（exited=$exited）---"
tail -3 "$OUT/share2.log"
grep -q "LINK Reconnecting" "$OUT/share2.log" || { echo "PHASE2 FAIL: 没看到 Reconnecting 状态"; exit 1; }
if [[ "$exited" == "yes" && $USED -le $((GRACE + 6)) ]]; then
  grep -q "会话结束" "$OUT/share2.log" || { echo "PHASE2 FAIL: 退出但没有错误原因"; exit 1; }
  echo "PHASE2 PASS: 永久中断在 ${USED}s 内有界失败并给出原因"
else
  echo "PHASE2 FAIL: 会话没有在有界时间内结束（${USED}s，grace=${GRACE}s）"
  exit 1
fi
kill $SHARE 2>/dev/null; wait $SHARE 2>/dev/null
echo "RECONNECT ACCEPTANCE PASS"
