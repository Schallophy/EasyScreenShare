#!/usr/bin/env bash
# 媒体平面起步爬坡 + 开场回放尺子（#44）：把画面交给视频轨之后的头 30 个帧周期，
# 观看端收到的到底是"匀速的 30 帧"还是"慢两秒"或"攒一堆再一次性倒出来"。
#
# 为什么要单独测它：稳态交付率一直是绿的（perf-load 17/17、product-mode 稳态窗口
# 29.6–30.4 fps），但那些都是**窗口平均**。平均数把两种完全不同的体感算成同一个数：
# "头两秒幻灯片、后面满帧"和"全程 15 帧"都报 15.2 帧/s；而"攒 1 秒再一次性倒出来"
# 平均起来甚至是满分——那正是用户报的"时间轴漂移、不追最新内容"。所以尺子用第 1 帧
# 到第 31 帧的耗时（30 个帧周期 = 声明帧率的直尺），两头都卡：
#   太慢 = 起步斜坡（编码器/码控没收敛）；太快 = 回放式突发（旧画面成串涌进来）。
# 统计只看**平面开着**的那段：平面关着时观看端不画视频帧，发送端只是按静止节拍喂
# 编码器保温，那些 500 ms 间隔不是卡顿（不加这一层判定，一次 17 秒的跑误报 23 个洞）。
#
# 臂的格式是 `名字:ESS_IDLE_FEED:ESS_WARM_MS`：
#   * ESS_WARM_MS=0 = 关掉平面预热（"决定即交接"的旧行为），这一臂**预期要失败**——
#     它就是这条缺陷的反向对照，证明洞是预热消掉的，不是脚本自己造出来的；
#   * ESS_IDLE_FEED 曾经用来证伪一个假设："起步斜坡是 H264 的输入帧率估计被静止期
#     2 fps 节拍校准坏了"（日志里那句 `Actual input framerate 0.000000 is different
#     from framerate in setting 29.000000`）。满节拍保温（=1）一样慢，假设不成立，
#     真正的账是关键帧 pacing + 码控收敛。留着是因为它是唯一能改这个节拍的旋钮。
#
# 用法：bash scripts/ramp-start.sh     （FPS/W/H/MODE/CONTENT/SECS/REPS/ARMS/DECIDE 可覆盖）
#   例：MODE=balanced CONTENT=block REPS=4 ARMS="cold:0:0 warm:0" bash scripts/ramp-start.sh
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/ramp-start
PORT=7880
FPS=${FPS:-30}
W=${W:-1920}
H=${H:-1080}
MODE=${MODE:-smooth}
SECS=${SECS:-16}
REPS=${REPS:-2}
# 臂名=ESS_IDLE_FEED 值（0 = 不覆盖，用内定的 ~2 fps）
ARMS=${ARMS:-"cold:0:0 warm:0"}

rm -rf "$OUT"; mkdir -p "$OUT"
[ -x "$BIN/ess-share.exe" ] && [ -x "$BIN/ess-view.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }
if [ -n "$(netstat -ano | grep ":$PORT.*LISTENING" || true)" ]; then
  echo "$PORT 已被占用（多半是上一轮残留的服务端），先停掉它再跑"; exit 1
fi
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'for pid in "${SERVER:-0}" "${P_SHARE:-0}" "${P_VIEW:-0}"; do [ "$pid" != 0 ] && taskkill //PID "$pid" //F >/dev/null 2>&1; done; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
grep -qi "address already in use" "$OUT/server.log" && { echo "FAIL 服务端没绑上 $PORT，读数作废"; exit 1; }

pass=0; fail=0
ok()  { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }
P_SHARE=0; P_VIEW=0
declare -A R16 R31 HOLES WORST SW INBOX SKIPD TILER

run() { # $1=臂名 $2=ESS_IDLE_FEED（0 = 不覆盖） $3=第几次 $4=ESS_WARM_MS
  local A=$1 IDLE=$2 N=$3 WM=${4:-} D="$OUT/$1-$3"
  mkdir -p "$D"
  # 观看端**先**连上：让它完整经历从第 1 帧起的爬坡。反过来（观看端后加入）时斜坡
  # 已经走完，量到的永远是稳态——这正是这条缺陷躲过全部既有门禁的原因。
  ESS_GAP_TRACE=1 "$BIN/ess-view" --no-window --seconds $((SECS - 3)) >"$D/view.log" 2>&1 &
  P_VIEW=$!
  sleep 1
  local E=(ESS_PERF=1 ESS_DEMO_ANIM=99999 ESS_DEMO_BLOCK=1 "ESS_DEMO_W=$W" "ESS_DEMO_H=$H" ESS_VIDEO=1)
  [ "${CONTENT:-video}" = video ] && E+=(ESS_DEMO_VIDEO=1)
  # 数组元素展开后不能放在赋值前缀位置（引号词会被当成命令），所以统一走 env
  [ "$IDLE" != 0 ] && E+=(ESS_IDLE_FEED="$IDLE")
  # 第 4 参数 = ESS_WARM_MS（0 = 关掉平面预热，回到"决定即交接"，反向对照臂）
  [ -n "${WM:-}" ] && E+=(ESS_WARM_MS="$WM")
  [ -n "${DECIDE:-}" ] && E+=(ESS_DECIDE=1)

  env "${E[@]}" "$BIN/ess-share" --demo --url "ws://127.0.0.1:$PORT" --room demo --identity share \
    --fps "$FPS" --mode "$MODE" --seconds "$SECS" >"$D/share.log" 2>&1 &
  P_SHARE=$!
  wait "$P_VIEW"
  taskkill //PID "$P_SHARE" //F >/dev/null 2>&1
  taskkill //IM ess-share.exe //F >/dev/null 2>&1
  sleep 1
  # 素材自证：读数必须建立在持续变动的视频类内容上，静止真桌面会让平面根本不开
  grep -q "合成屏" "$D/share.log" || { echo "  $A 采的不是合成屏（少了 --demo？）"; return; }
  R16[$A-$N]=$(grep -oE "爬坡 前 16 帧用了 [0-9]+ms" "$D/view.log" | head -1 | sed -E "s/.*用了 ([0-9]+)ms/\1/")
  R31[$A-$N]=$(grep -oE "爬坡 前 31 帧用了 [0-9]+ms" "$D/view.log" | head -1 | sed -E "s/.*用了 ([0-9]+)ms/\1/")
  HOLES[$A-$N]=$(grep -cE "^卡顿" "$D/view.log")
  WORST[$A-$N]=$(grep -oE "max=[0-9]+ms" "$D/view.log" | grep -oE "[0-9]+" | sort -n | tail -1)
  # 平面整轮没开时这条尺子没有可量的段——那种情况下要改验无损通路自己的新鲜度，
  # 所以把开关次数与观看端的积压/脱落也取下来。
  SW[$A-$N]=$(grep -oE "switches=[0-9]+" "$D/share.log" | tail -1 | sed 's/switches=//')
  INBOX[$A-$N]=$(grep -oE "inbox_peak=[0-9]+" "$D/view.log" | grep -oE "[0-9]+" | sort -n | tail -1)
  SKIPD[$A-$N]=$(grep -oE " skip=[0-9]+" "$D/view.log" | grep -oE "[0-9]+" | sort -n | tail -1)
  TILER[$A-$N]=$(grep -oE "tile_rate=[0-9]+" "$D/view.log" | grep -oE "[0-9]+" | sort -n | tail -1)
  local desc="内定 2fps"
  [ "$IDLE" != 0 ] && desc="每 ${IDLE} 帧一次"
  local wm="预热开"
  [ "${WM:-}" = "0" ] && wm="预热关(反向对照)"
  echo "== $A 第 $N 次（静止期喂帧 $desc，$wm，${W}x${H} ${MODE} 档）：前 16 帧 ${R16[$A-$N]:-无}ms / 前 31 帧 ${R31[$A-$N]:-无}ms / 平面内 ≥120ms 洞 ${HOLES[$A-$N]} 个 / 最大间隔 ${WORST[$A-$N]:-?}ms / 平面开关 ${SW[$A-$N]:-?} 次 / 块速率 ${TILER[$A-$N]:-?}/s"
}

for arm in $ARMS; do
  A=$(cut -d: -f1 <<<"$arm"); I=$(cut -d: -f2 <<<"$arm"); WM=$(cut -d: -f3 <<<"$arm")
  for ((n = 1; n <= REPS; n++)); do run "$A" "$I" "$n" "$WM"; done
done

echo
echo "门禁（${W}x${H} @ ${FPS}fps，${MODE} 档，视频类合成内容，每臂 ${REPS} 次）"
PER=$((1000 / FPS))
for arm in $ARMS; do
  A=${arm%%:*}
  for ((n = 1; n <= REPS; n++)); do
    r16=${R16[$A-$n]:-}; r31=${R31[$A-$n]:-}
    if [ -z "$r31" ]; then
      # 没有"平面开着"的连续 31 帧。两种成因必须分开：
      #   * switches=0 —— 平面整轮没开，无损通路自己扛住了这条链路，尺子没有段可量；
      #     这不是缺陷，但必须换一把尺子：观看端不积压、不脱落、确实有块落画。
      #   * 平面开着却没解出 31 帧 —— 交接后就没有视频帧，是真缺陷。
      if [ "${SW[$A-$n]:-0}" = "0" ]; then
        if [ "${INBOX[$A-$n]:-99}" -le 46 ] && [ "${SKIPD[$A-$n]:-99}" = "0" ] \
           && [ "${TILER[$A-$n]:-0}" -gt 0 ]; then
          ok "$A#$n 平面整轮没开（无损通路独自扛住）：inbox 峰值 ${INBOX[$A-$n]} ≤46、脱落 ${SKIPD[$A-$n]}、块速率 ${TILER[$A-$n]}/s"
        else
          bad "$A#$n 平面没开且无损通路也不干净：inbox 峰值 ${INBOX[$A-$n]:-?}、脱落 ${SKIPD[$A-$n]:-?}、块速率 ${TILER[$A-$n]:-?}/s"
        fi
      else
        bad "$A#$n 平面开着（switches=${SW[$A-$n]}）却没解出 31 帧：交接后就没有视频帧"
      fi
      continue
    fi
    # 30 个帧周期 = 1000 ms。容忍到 1.5 倍：起步那一个关键帧的排队是一次性的，
    # 但两秒的幻灯片不是（那就是用户说的"开场特别卡"）。
    awk -v r="$r31" -v p="$PER" 'BEGIN{exit !(r+0 <= p*30*1.5)}' \
      && ok "$A#$n 前 31 帧 ${r31}ms ≤ $((PER*30*3/2))ms：开场就按声明帧率走" \
      || bad "$A#$n 前 31 帧 ${r31}ms（直尺 $((PER*30))ms）：开场 $((r31/1000)) 秒是幻灯片"
    # 反向：太快 = 突发回放。16 帧至少该花 0.4×15 个周期。阈值不取 0.6 是因为**交接时
    # 播放缓冲本来就会排掉两三帧**（实测 720p 流畅优先档第 1→16 帧 275 ms，被 0.6 误判成
    # 回放）；而真的"攒了两秒再补播"是 16 帧挤在 50–100 ms 里出来，0.4 照样抓得住。
    # 这条尺子防的是"顺序播放旧画面"，与 perf-load 的 lag/积压增长是同一件事的两面。
    awk -v r="$r16" -v p="$PER" 'BEGIN{exit !(r+0 >= p*15*0.4)}' \
      && ok "$A#$n 前 16 帧 ${r16}ms ≥ $((PER*15*2/5))ms：不是攒一堆再回放" \
      || bad "$A#$n 前 16 帧 ${r16}ms：帧是成串涌进来的（回放式突发，平均帧率会骗人）"
    [ "${HOLES[$A-$n]}" = "0" ] && ok "$A#$n 平面期间没有 ≥120ms 的画面空洞" \
      || bad "$A#$n 平面期间有 ${HOLES[$A-$n]} 个 ≥120ms 空洞：$(grep -m1 -E '^卡顿' "$OUT/$A-$n/view.log")"
  done
done

echo
echo "ramp-start：PASS=$pass FAIL=$fail"
[ "$fail" = 0 ]
