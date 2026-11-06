#!/usr/bin/env bash
# 图块平面挂久了停喂视频轨：省掉那 ~100 kbps 的静止保活流量，且**恢复后画面还是对的**。
#
# 这条验收存在的理由：图块平面下的静止保活帧（~2 fps）买的是"切进视频平面时画面是
# 当前的"，代价是全程上行——实测 1080p 静止画面 `egress` 0.10 Mbps、`enc` 仍在
# 1440x808@2.0、`vsent` 3.3 fps；关掉保活立刻掉到 0.01 Mbps。但"关掉"不等于"可以
# 关掉"：编码器冷了十秒之后，再喂帧还能不能出画、观看端还能不能收到正确的画面，
# 只能真机验。
#
# 场景：1280x720 合成屏，**前 16 s 完全静止**（10 s 时停喂），之后 10 s 动画把画面
# 逼进视频平面，再静止退回图块。
#
# 四条断言：
#   1. vstop 单调涨到 ≥1 s      —— 停喂真的发生了（不是被别的路径挡住）
#   2. 停喂窗口内 vsent=0 且 egress ≈ 0  —— 省下来的确实是上行，不是只改了计数器
#   3. 动画开始后 vstop 归零、vsent>0     —— 停喂是可逆的，编码器冷启动能起来
#   4. 终帧逐字节等于期望       —— 冷启动不是拿一张旧画面糊过去，退出修复没漏块
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  （见 README「LiveKit 服务端怎么带」；也可用 LIVEKIT_SERVER=/c/path/to/livekit-server.exe 指定）"; exit 1; }
[ -x "$BIN/ess-share.exe" ] && [ -x "$BIN/ess-view.exe" ] && [ -x "$BIN/ess-gen-expected.exe" ] \
  || { echo "先 cargo build --release -p ess-app"; exit 1; }

OUT=${OUT:-target/tile-idle-cut}
W=${W:-1280}; H=${H:-720}
FPS=${FPS:-30}
DELAY=${DELAY:-200}      # 0.1 s/格 → 20 s 静止前导（> 10 s 停喂门槛，留 10 s 观察稳态）
ANIM=${ANIM:-100}        # 10 s 动画
# 观看端比分享端早 1 s 起、晚 3 s 停，**必须盖住"动画结束 + 平面退出"那一刻**：
# 少这几秒，--dump 拍到的是视频平面里那最后一张图块，MAE 会直接飙到几十。
SECS=${SECS:-$(( (DELAY + ANIM) / 10 + 8 ))}
STOP_AFTER=${ESS_IDLE_STOP:-10}
ANIM_AT=$(( DELAY * 100 ))   # ms：动画开始（画面第一次变化）

rm -rf "$OUT"; mkdir -p "$OUT"
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi

ESS_DEMO_ANIM_DELAY=$DELAY "$BIN/ess-gen-expected" "$OUT/expected.ppm" --w $W --h $H --anim $ANIM || exit 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
# 不要裸 wait：Git Bash 会死等一个已被 taskkill 干掉的 Windows 作业（见 forced-plane.sh）
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
grep -qi "address already in use" "$OUT/server.log" && { echo "FAIL 服务端没绑上 7880——本轮读数作废"; exit 1; }

"$BIN/ess-view" --no-window --seconds $((SECS - 4)) --dump "$OUT/received.ppm" >"$OUT/view.log" 2>&1 &
sleep 1
ESS_DECIDE=1 ESS_PERF=1 ESS_IDLE_STOP=$STOP_AFTER \
  ESS_DEMO_W=$W ESS_DEMO_H=$H ESS_DEMO_ANIM=$ANIM ESS_DEMO_ANIM_DELAY=$DELAY \
  "$BIN/ess-share" --demo --fps "$FPS" --seconds "$SECS" >"$OUT/share.log" 2>&1
sleep 2

python - "$OUT" "$ANIM_AT" <<'PY'
import sys
out, anim_at = sys.argv[1], int(sys.argv[2])
raw = open(out + '/share.log', 'rb').read()
for enc in ('utf-8', 'gbk', 'utf-16-le'):
    try:
        t = raw.decode(enc)
        if 'STATS' in t: break
    except Exception: pass

stats = []
for l in t.splitlines():
    if 'STATS ' not in l: continue
    def g(k):
        try: return l.split(k + '=')[1].split()[0]
        except Exception: return None
    stats.append({'vstop': float(g('vstop') or 0), 'vsent': float(g('vsent') or 0),
                  'egress': g('egress'), 'audio': g('audio'), 'enc': g('enc'),
                  # `updates` 是累计条数：它一变就说明画面又开始动了，停喂窗口到此为止
                  'updates': int(g('updates') or 0), 'raw': l})
planes = []
for l in t.splitlines():
    if 'DECIDE ts=' in l:
        planes.append((int(l.split('ts=')[1].split()[0]), l.split(' plane=')[1].split()[0]))

stopped = [s for s in stats if s['vstop'] >= 1.0]
print(f"  STATS 行数={len(stats)}  停喂窗口的行数={len(stopped)}")
for l in t.splitlines():
    if '停喂' in l or '恢复喂帧' in l:
        print('   ', l.strip())

def mbps(v):
    try: return float((v or '0').replace('Mbps', ''))
    except ValueError: return 0.0

def video_kbps(s):
    # `egress` 是整条上行，里面含系统声音（demo 跑起来时实测 4~140 kbps 都有）。
    # 这条验收只关心视频那一半，减掉音频再比，否则音频一动读数就没法判。
    try: a = float((s['audio'] or '0').split('kbps')[0])
    except ValueError: a = 0.0
    return mbps(s['egress']) * 1000.0 - a

fail = 0
if not stopped:
    print('  FAIL 没有任何一秒的 vstop≥1——停喂没发生'); fail = 1
else:
    print(f'  PASS 停喂生效（vstop 最大 {max(s["vstop"] for s in stopped):.1f} s）')

if stopped:
    # 停喂之后、动画之前的那一段：编码器不该出帧
    bad = [s for s in stopped if s['vsent'] > 0.1]
    if bad:
        print(f'  FAIL 停喂窗口内仍在出帧：vsent={[s["vsent"] for s in bad][:5]}'); fail = 1
    else:
        print('  PASS 停喂窗口内 vsent=0（编码器确实没在出帧）')
    # `egress` 是平滑过的读数，停喂后要几秒才掉下来（实测 0.11→0.06→0.02 用 4 s）。
    # 所以拿**窗口末行**跟停喂前的基线比，而不是要求整段立刻归零。
    # 窗口要**先把"画面又开始动"的那一秒切掉**再判：`vstop` 按整秒取整，动画开始的
    # 那一行 `vstop` 还 ≥1，但图块已经在往外发了（实测那一行 0.32 Mbps、`vsent` 仍是 0，
    # 判据会把"恢复"读成"没降下来"）。
    idx0 = stats.index(stopped[0])
    first_move = next((i for i in range(idx0, len(stats))
                       if stats[i]['updates'] > stats[idx0]['updates']), len(stats))
    idle_rows = stats[idx0:first_move] or stopped
    before = [video_kbps(s) for s in stats[max(0, idx0 - 4):idx0]]
    base = max(before) if before else 0.0
    tail = video_kbps(idle_rows[-1])
    if tail <= 40 and (base <= 0 or tail <= base * 0.4):
        print(f'  PASS 上行（扣掉声音）{base:.0f} → {tail:.0f} kbps（停喂稳定后）')
    else:
        print(f'  FAIL 上行没降到位：停喂前 {base:.0f} → 窗口末 {tail:.0f} kbps'); fail = 1
    # 编码器被放掉是"真的停了"的旁证：只停喂帧、编码器还在跑，说明有别的路径在喂
    if any(s['enc'] == '0x0@0.0' for s in stopped):
        print('  PASS 编码器降到 0x0@0.0（没有别的路径在喂帧）')
    else:
        print(f'  FAIL 停喂期间编码器仍按 {[s["enc"] for s in stopped][-1]} 在跑'); fail = 1

    # 停喂之后的第一个 vstop=0 行 = 恢复
    resume = next((i for i in range(idx0, len(stats)) if stats[i]['vstop'] == 0.0), None)
    if resume is None:
        print('  FAIL 停喂之后一直没有恢复（动画没能把编码器叫起来）'); fail = 1
    else:
        sent_after = [s['vsent'] for s in stats[resume:]]
        if not any(v > 0.5 for v in sent_after):
            print(f'  FAIL 恢复后编码器仍不出帧：vsent={sent_after[:6]}'); fail = 1
        else:
            print(f'  PASS 恢复喂帧（之后 vsent 最大 {max(sent_after):.1f} fps）')

enter = next((ts for ts, p in planes if p == 'true'), None)
if enter is None:
    print('  FAIL 画面从未进入视频平面——恢复路径没被真正走一遍'); fail = 1
elif enter < anim_at:
    print(f'  FAIL 动画还没开始就进了平面（{enter} < {anim_at}）'); fail = 1
else:
    print(f'  PASS 动画于 {anim_at}ms 开始，视频平面于 {enter}ms 进入（冷启动走了 {enter - anim_at} ms）')

# 冷启动真正的风险不是"停喂"，是"起来之后第一帧没有参考帧"：那会在观看端表现为
# 一串解码空洞。观看端自己量了平面开着时的帧间隔（`间隔p95=`，与 STATS 的 video=
# 同源），拿它当判据比看终帧贴切得多。
gaps = []
view_lines = []
vraw = open(out + '/view.log', 'rb').read()
for enc in ('utf-8', 'gbk', 'utf-16-le'):
    try:
        view_lines = vraw.decode(enc).splitlines(); break
    except Exception: pass
import re as _re
p95 = [_re.search(r'间隔p50=(\d+)ms p95=(\d+)ms max=(\d+)ms', l) for l in view_lines]
p95 = [m for m in p95 if m]
vis = [_re.search(r'video=([\d.]+)帧/s (\d+)x(\d+)', l) for l in view_lines]
vis = [m for m in vis if m]
if not p95:
    print('  FAIL 观看端没有帧间隔读数（视频平面没被看过？）'); fail = 1
else:
    worst = max(int(m.group(2)) for m in p95)
    worst_max = max(int(m.group(3)) for m in p95)
    best_fps = max(float(m.group(1)) for m in vis) if vis else 0.0
    if worst > 120 or worst_max > 400:
        print(f'  FAIL 冷启动后解码有洞：p95={worst}ms max={worst_max}ms'); fail = 1
    elif best_fps < 20:
        print(f'  FAIL 平面里只解到 {best_fps} 帧/s（编码器没真起来）'); fail = 1
    else:
        print(f'  PASS 冷启动后连续出画：{best_fps} 帧/s，间隔 p95={worst}ms max={worst_max}ms')

# 终帧：**逐字节比**。退出平面的修复基线取"交接那一刻"观看端手里的画面，差集是完备的，
# 所以退回来必须与期望帧一模一样。曾经这里只能卡 MAE——基线取的是"决定要交"那一刻的
# 快照，而预热期还在发无损差分，退出差集于是漏掉"中途变过、又变回原样"的块，留下一个
# 永远补不回来的残块（见 pipeline.rs 的 `refresh_exit_baseline`）。
exp = open(out + '/expected.ppm', 'rb').read()
try:
    got = open(out + '/received.ppm', 'rb').read()
except OSError:
    got = b''


def mae(a, b):
    ha = a.split(b'\n', 3)
    hb = b.split(b'\n', 3)
    if len(ha) < 4 or len(hb) < 4 or len(ha[3]) != len(hb[3]):
        return None
    x, y = ha[3], hb[3]
    return sum(abs(p - q) for p, q in zip(x, y)) / max(1, len(x))


m = mae(exp, got) if got else None
if m is None:
    print(f'  FAIL 终帧读不出来（期望 {len(exp)} B / 收到 {len(got)} B）'); fail = 1
elif got != exp:
    print(f'  FAIL 终帧与期望不一致：MAE={m:.3f}（这是残块/糊旧画面的样子）'); fail = 1
else:
    print('  PASS 终帧与期望逐字节一致（冷启动没糊旧画面，退出修复也没漏块）')

sys.exit(fail)
PY
exit $?
