#!/usr/bin/env bash
# 慢观看端（手机）负载验收。
#
# 为什么单独测这个：发送端的 lag 只覆盖"采集→会话"这一段，消息一旦交给 LiveKit
# 就进了观看端自己的**无界**事件队列。手机 CPU 只有桌面的几分之一，1080p 大面积
# 变化时应用速率 < 到达速率，队列长度随时间线性涨——按队列论 age ≈ inbox / 到达率，
# 这就是"时间轴一直漂移、观看端不追最新内容、延迟越来越大"。桌面 ess-view 太快，
# 永远量不到；所以用 ESS_VIEW_SLOW_US 把每块的应用代价放大到手机量级。
#
# 两臂都是 1920x1080 视频+弹幕内容、ESS_VIDEO=0（全部走无损图块，弱客户端的最坏情况）：
#   A 闸门关掉（ESS_VIEW_BACKLOG 设到天量）：必须看到 inbox 单调上涨 —— 负对照，
#     它不涨就说明这个测试量不到东西，B 臂的"通过"也就没有意义。
#   B 闸门开（默认留 6 条）：inbox 必须有界，过时更新被丢弃（skip > 0），
#     基线类消息（整帧/滚动宣告/修复）一条不能丢。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/slow-client
FPS=${FPS:-30}
W=${W:-1920}
H=${H:-1080}
SECS=${SECS:-30}
SLOW=${SLOW:-350}          # 每块忙等 µs：把应用代价放大到手机量级
KEEP=${KEEP:-6}             # B 臂保留的队列深度
LAG_BUDGET=${LAG_BUDGET:-40} # inbox 峰值允许超过 KEEP 的条数

[ -x "$BIN/ess-view.exe" ] && [ -x "$BIN/ess-share.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端"; exit 1
fi
rm -rf "$OUT"; mkdir -p "$OUT"
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
grep -qi "address already in use" "$OUT/server.log" && { echo "FAIL 服务端没绑上 7880"; exit 1; }

pass=0; fail=0
ok()  { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }

run() { # $1=臂 $2=ESS_VIEW_BACKLOG
  local A=$1 BK=$2 D="$OUT/$1"
  mkdir -p "$D"
  ESS_VIEW_BACKLOG="$BK" ESS_VIEW_SLOW_US="$SLOW" \
    "$BIN/ess-view" --no-window --seconds "$SECS" >"$D/view.log" 2>&1 &
  local view=$!; sleep 1
  ESS_PERF=1 ESS_DEMO_ANIM=99999 ESS_DEMO_BLOCK=1 ESS_DEMO_VIDEO=1 \
  ESS_DEMO_W=$W ESS_DEMO_H=$H ESS_VIDEO=0 \
    "$BIN/ess-share" --demo --fps "$FPS" --mode balanced --seconds $((SECS + 4)) >"$D/share.log" 2>&1 &
  local share=$!
  wait "$view"
  taskkill //PID "$share" //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1
  sleep 1
  echo "== 臂 $A（ESS_VIEW_BACKLOG=$BK，每块忙等 ${SLOW}µs）"
  awk 'NF' "$D/view.log" | grep -oE 'inbox_peak=[0-9]+ skip=[0-9]+ duty=[0-9.]+ applied=[0-9]+ tile_rate=[0-9]+' \
    | awk '{split($1,a,"=");split($2,b,"=");split($3,c,"=");split($5,e,"=");I[NR]=a[2];S[NR]+=b[2];D[NR]=c[2];R[NR]=e[2]}
           END{for(i=1;i<=NR;i++){if(i%3==1||i==NR)printf "  t=%02ds inbox=%s duty=%s skip/s=%s tile_rate=%s\n", i, I[i], D[i], S[i]/NR, R[i]; m=(I[i]>m)?I[i]:m}
               printf "  inbox 峰值=%d 累计 skip=%d\n", m, S[NR]}'
  INB[$A]=$(awk -F'inbox_peak=' 'NF>1{split($2,a," "); if(a[1]+0>m) m=a[1]+0} END{print m+0}' "$D/view.log")
  # 后半段 vs 前半段的 inbox 均值：涨 = 队列没排空 = 观看端在放历史
  GROW[$A]=$(awk -F'inbox_peak=' 'NF>1{split($2,a," "); L[NR]=a[1]+0}
    END{n=NR;t=int(n/2); if(t<2){print "0";exit} s1=0;s2=0;
        for(i=1;i<=t;i++)s1+=L[i]; for(i=n-t+1;i<=n;i++)s2+=L[i];
        printf "%.1f", s2/t - s1/t}' "$D/view.log")
  SKIP[$A]=$(awk -F'skip=' 'NF>1{split($2,a," "); s+=a[1]+0} END{print s+0}' "$D/view.log")
  printf '  指标：inbox 峰值=%s 前半→后半增幅=%s 条 累计 skip=%s\n' "${INB[$A]}" "${GROW[$A]}" "${SKIP[$A]}"
}

declare -A INB GROW SKIP
run A 1000000000
run B "$KEEP"

echo
echo "门禁（${W}x${H} @ ${FPS}fps，每块 ${SLOW}µs 模拟手机）"
# 负对照：闸门关掉时必须看到积压上涨，否则这一轮什么都没测到。
awk -v g="${GROW[A]}" 'BEGIN{exit !(g+0 > 5)}' \
  && ok "A 闸门关闭时 inbox 持续上涨（+${GROW[A]} 条）—— 负对照成立" \
  || bad "A 闸门关闭时 inbox 没有上涨（+${GROW[A]} 条）—— 负载不够，这一轮的 B 结果不作数"
awk -v i="${INB[B]}" -v k="$KEEP" -v m="$LAG_BUDGET" 'BEGIN{exit !(i+0 <= k+m)}' \
  && ok "B 闸门开启时 inbox 峰值 ${INB[B]} ≤ $((KEEP + LAG_BUDGET)) —— 观看端队列有界，延迟不漂移" \
  || bad "B 闸门开启时 inbox 峰值 ${INB[B]} > $((KEEP + LAG_BUDGET)) —— 队列仍在涨，观看端会按序播旧画面"
awk -v g="${GROW[B]}" 'BEGIN{exit !(g+0 <= 2)}' \
  && ok "B inbox 前后半段增幅 ${GROW[B]} ≤ 2 条 —— 无单调积压" \
  || bad "B inbox 前后半段增幅 ${GROW[B]} > 2 条"
awk -v s="${SKIP[B]}" 'BEGIN{exit !(s+0 > 0)}' \
  && ok "B 丢弃了 ${SKIP[B]} 条过时更新（追最新内容的代价是丢历史，正确行为）" \
  || bad "B 一条都没丢：要么负载不够，要么闸门没生效"
# 基线类不许丢：B 臂画布必须真的在动。无损通路走 applied/scrolls，运动通路走 motion，
# 整帧基线走 fulls——只看 applied 会把"全走 JPEG"的正常会话误判成卡死。
WORK=$(awk '/^t=[0-9]+s canvas=/{for(i=1;i<=NF;i++){if($i~/^applied=/){split($i,a,"=");p=a[2]}
        if($i~/^fulls=/){split($i,a,"=");fu=a[2]} if($i~/^motion=/){split($i,a,"=");mo=a[2]}}
      if(f==""){f=p+fu+mo} l=p+fu+mo} END{print l-f}' "$OUT/B/view.log")
awk -v a="$WORK" 'BEGIN{exit !(a+0 > 0)}' \
  && ok "B 仍处理了 ${WORK} 项工作（整帧/滚动/运动帧照常上屏，基线没被误丢）" \
  || bad "B 处理了 ${WORK} 项：闸门把该留的也挡掉了"

echo
echo "slow-client：PASS=$pass FAIL=$fail"
[ "$fail" = 0 ]
