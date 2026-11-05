#!/usr/bin/env bash
# 静止退回图块：**strain 逼进**的视频平面，画面静止后必须退回无损图块。
#
# 这条验收存在的理由：单元测试只能直接注入 `receiver_strain`，证明不了"生产里这个
# 信号真的会冻结在 true"。而它确实会——会话在运动/平面期间直接丢弃观看端报告
# （`ess-session/src/lib.rs:1472`），于是平面期内 `receiver_strain` 停在进入前那次
# 报告的值上，旧写法拿它无条件续期，静止画面就永远退不回图块（现场：进入后 120 s
# 无退出、`tiles` 不动、`frames` 照涨）。
#
# 场景：720p 合成屏（图块编码 < 一帧预算，避免编码压力先把平面举起来）；
#   ESS_MOTION=0        关掉"大面积运动 → 平面"，平面只能由 strain 逼进；
#   前 3 s 静止          观看端要先"跟上"一次，strain 只在 `ever_current` 之后才计数；
#   3–13 s 动画 / 之后静止；
#   观看端每块忙等 1500 µs  把应用代价放大到手机量级，才能真的落后 ≥2 版。
#
# 三条断言：
#   strain_forced ≥ 1          —— 平面确实是被 strain 逼进来的（不是别的路径）
#   静止后 1.5 s 内退出         —— `视频平面退出` 出现在动画结束之后
#   终帧逐字节等于期望          —— 观看端画布回到无损
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  （见 README「LiveKit 服务端怎么带」；也可用 LIVEKIT_SERVER=/c/path/to/livekit-server.exe 指定）"; exit 1; }
[ -x "$BIN/ess-share.exe" ] && [ -x "$BIN/ess-view.exe" ] && [ -x "$BIN/ess-gen-expected.exe" ] \
  || { echo "先 cargo build --release -p ess-app"; exit 1; }

OUT=${OUT:-target/strain-static-exit}
W=${W:-1280}; H=${H:-720}
FPS=${FPS:-30}
DELAY=${DELAY:-30}       # 0.1 s/格 → 3 s 静止前导
ANIM=${ANIM:-100}        # 10 s 动画，之后全静止
SLOW=${SLOW:-1500}
SECS=${SECS:-28}
STATIC_AT=$(( (DELAY + ANIM) * 100 ))   # ms：画面从此不再变化
EXIT_BUDGET_MS=1500

rm -rf "$OUT"; mkdir -p "$OUT"
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi

"$BIN/ess-gen-expected" "$OUT/expected.ppm" --w $W --h $H --anim $ANIM || exit 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
# 不要裸 wait：Git Bash 会死等一个已被 taskkill 干掉的 Windows 作业（见 forced-plane.sh）
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
grep -qi "address already in use" "$OUT/server.log" && { echo "FAIL 服务端没绑上 7880——本轮读数作废"; exit 1; }

ESS_VIEW_SLOW_US=$SLOW "$BIN/ess-view" --no-window --seconds $((SECS - 4)) \
  --dump "$OUT/received.ppm" >"$OUT/view.log" 2>&1 &
sleep 1
ESS_DECIDE=1 ESS_PERF=1 ESS_MOTION=0 \
  ESS_DEMO_W=$W ESS_DEMO_H=$H ESS_DEMO_ANIM=$ANIM ESS_DEMO_ANIM_DELAY=$DELAY \
  "$BIN/ess-share" --demo --fps "$FPS" --seconds "$SECS" >"$OUT/share.log" 2>&1
sleep 2

python - "$OUT/share.log" "$STATIC_AT" "$EXIT_BUDGET_MS" "$OUT" <<'PY'
import sys
log, static_at, budget, out = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
raw = open(log, 'rb').read()
for enc in ('utf-8', 'gbk', 'utf-16-le'):
    try:
        t = raw.decode(enc)
        if '视频平面' in t or 'STATS' in t: break
    except Exception: pass

rows, stats = [], []
for l in t.splitlines():
    if 'DECIDE ts=' in l:
        rows.append((int(l.split('ts=')[1].split()[0]), l.split(' plane=')[1].split()[0]))
    if 'STATS ' in l:
        stats.append(l)

def last(name):
    vals = [l.split(name + '=')[1].split()[0] for l in stats if name + '=' in l]
    return vals[-1] if vals else None

enter = next((ts for ts, p in rows if p == 'true'), None)
exit_ts = next((ts for ts, p in rows if p == 'false' and enter is not None and ts > enter), None)
print(f"  strain_forced={last('strain_forced')}  平面进入={enter}ms  平面退出={exit_ts}ms  画面静止于={static_at}ms")
for l in t.splitlines():
    if '视频平面' in l:
        print('   ', l.strip())

fail = 0
if (last('strain_forced') or '0') == '0':
    print('  FAIL 平面不是被 strain 逼进来的（strain_forced=0），这条验收没测到东西'); fail = 1
if enter is None:
    print('  FAIL 平面从未进入'); fail = 1
elif exit_ts is None:
    print('  FAIL 平面进入后从未退出（静止画面卡在有损平面上）'); fail = 1
elif exit_ts < static_at:
    print(f'  FAIL 在画面还在变化时就退出了（{exit_ts} < {static_at}），清晰度会被无谓地换掉'); fail = 1
elif exit_ts - static_at > budget:
    print(f'  FAIL 退出太慢：静止后 {exit_ts - static_at} ms 才退回图块（预算 {budget} ms）'); fail = 1
else:
    print(f'  PASS 静止后 {exit_ts - static_at} ms 退回无损图块')

exp = open(out + '/expected.ppm', 'rb').read()
try:
    got = open(out + '/received.ppm', 'rb').read()
except OSError:
    got = b''
if got == exp:
    print('  PASS 终帧与期望逐字节一致')
else:
    print(f'  FAIL 终帧不一致（期望 {len(exp)} B / 收到 {len(got)} B）'); fail = 1

sys.exit(fail)
PY
exit $?
