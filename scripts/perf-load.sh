#!/usr/bin/env bash
# 1080p 性能负载验收（用户的真实工作点）。720p 的读数系统性偏乐观：无损差分是
# 逐块拷贝比对，代价随像素数线性涨（720p ~10 ms/帧，1080p ~33 ms/帧）。
#
# 两组负载各证明一件事：
#   T 只走无损平面（ESS_VIDEO=0，大面积持续变化）：量的是**无损管线本身的吞吐**——
#     tile_enc 必须待在帧预算内，否则观看端看到的就是按序播放的旧画面。
#   P 产品路径（视频平面可用，流畅优先）：量的是**端到端新鲜度**——发送端积压
#     lag 必须有界且不随时间上涨，队列 q 不积压，交付帧率达标。
#
# 用法：bash scripts/perf-load.sh   （W/H/FPS/MODE 可覆盖）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/perf-load
FPS=${FPS:-30}
W=${W:-1920}
H=${H:-1080}
VIEW_SECS=${VIEW_SECS:-30}
SECS=$((VIEW_SECS + 6))
BUD=$(awk -v f="$FPS" 'BEGIN{printf "%.1f", 1000/f}')
LAG_MAX=${LAG_MAX:-250}

command -v awk >/dev/null || { echo "需要 awk"; exit 1; }
[ -x "$BIN/ess-view.exe" ] && [ -x "$BIN/ess-share.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端"; exit 1
fi

rm -rf "$OUT"; mkdir -p "$OUT"
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
if grep -qi "address already in use" "$OUT/server.log"; then
  echo "FAIL 服务端没绑上 7880（上一轮残留还在监听）"; exit 1
fi

pass=0; fail=0
ok()  { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }

declare -A ENC LAG TRE DEL VIDOF APD
for L in T P V W; do ENC[$L]=0; LAG[$L]=0; TRE[$L]=0; DEL[$L]="-"; VIDOF[$L]=1; APD[$L]=0; done
collect() {
  local L=$1 F=$2
  ENC[$L]=$(awk '{for(i=1;i<=NF;i++) if($i~/^tile_enc=/){split($i,a,"=");t=a[2]+0; if(t>m) m=t}} END{print m+0}' "$F")
  LAG[$L]=$(awk '{for(i=1;i<=NF;i++) if($i~/^lag=/){split($i,a,"=");sub(/ms/,"",a[2]);l=a[2]+0; if(l>m) m=l}} END{print m+0}' "$F")
  # 积压趋势 = 后 1/3 均值 − 前 1/3 均值（绝对毫秒）。用比值会在 lag 本来就接近 0
  # 时自己放大自己（0 → 2 ms 也算"无穷倍"），绝对增长才是"积压有没有排空"。
  TRE[$L]=$(awk '{for(i=1;i<=NF;i++) if($i~/^lag=/){split($i,a,"=");sub(/ms/,"",a[2]);L[NR]=a[2]+0}}
    END{n=NR;t=int(n/3); if(t<1){print "0";exit} s1=0;s3=0;
        for(i=1;i<=t;i++)s1+=L[i]; for(i=n-t+1;i<=n;i++)s3+=L[i];
        printf "%.1f", s3/t - s1/t}' "$F")
}

# 跑一组：$1=组字母 $2=标签 $3=mode $4=ESS_VIDEO $5=ESS_DEMO_VIDEO（视频+弹幕内容）
run() {
  local G=$1 L=$2 M=$3 VID=$4 VIDC=${5:-0} D="$OUT/$2"
  mkdir -p "$D"
  "$BIN/ess-view" --no-window --seconds "$VIEW_SECS" >"$D/view.log" 2>&1 &
  local view=$!; sleep 1
  ESS_PERF=1 ESS_DEMO_ANIM=99999 ESS_DEMO_BLOCK=1 ESS_DEMO_W=$W ESS_DEMO_H=$H \
  ESS_VIDEO="$VID" ESS_DEMO_VIDEO="$VIDC" \
    "$BIN/ess-share" --demo --fps "$FPS" --mode "$M" --seconds "$SECS" >"$D/share.log" 2>&1 &
  local share=$!
  wait "$view"
  taskkill //PID "$share" //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1
  sleep 1
  echo "== $L（$M 档，${W}x${H}，视频平面 $([ "$VID" = 0 ] && echo 关 || echo 开)，内容 $([ "$VIDC" = 1 ] && echo "视频+弹幕" || echo 滚动+红块)，帧预算 ${BUD}ms）"
  # 每秒一组：单帧无损耗时 / 采集→会话积压 / 队列深度 / 过期丢弃
  awk '{for(i=1;i<=NF;i++){if($i~/^tile_enc=/){split($i,a,"=");t=a[2]+0}
        if($i~/^lag=/){split($i,a,"=");sub(/ms/,"",a[2]);l=a[2]+0}
        if($i~/^q=/){split($i,a,"=");q=a[2]+0}
        if($i~/^stale=/){split($i,a,"=");s=a[2]+0}
        if($i~/^vsent=/){split($i,a,"=");v=a[2]+0}}
       if(NR>2) printf "  s%02d tile_enc=%-6s lag=%-4s q=%-3s stale=%-5s vsent=%s\n", NR, t, l, q, s, v}' "$D/share.log"
  # 趋势：把秒序列三等分，看最后 1/3 的 lag 是否比第 1 个 1/3 高（涨 = 积压没排空）
  ST_trend=$(awk '{for(i=1;i<=NF;i++) if($i~/^lag=/){split($i,a,"=");sub(/ms/,"",a[2]);L[NR]=a[2]+0}}
    END{n=NR; t=int(n/3); if(t<1){print "0 0"; exit}
      s1=0;s3=0; for(i=1;i<=t;i++)s1+=L[i]; for(i=n-t+1;i<=n;i++)s3+=L[i];
      printf "%.1f %.1f", s1/t, s3/t}' "$D/share.log")
  ST_lag1=$(awk '{print $1}' <<<"$ST_trend"); ST_lag3=$(awk '{print $2}' <<<"$ST_trend")
  ST_deliver=$(grep -oE 'video=[0-9.]+帧/s' "$D/view.log" | sed 's/video=//; s/帧\/s//' | tail -n +3)
  DEL[$G]=$(awk 'NF{if($1+0>=0 && (min==""||$1+0<min))min=$1+0} END{print (min=="")?"-":min}' <<<"$ST_deliver")
  collect "$G" "$D/share.log"
  VIDOF[$G]=$VID
  # 无损/运动通路是否在画：观看端 5 秒报告里 applied+fulls+motion+scrolls 的增长。
  # 只看 applied 会误判：滚动宣告重建画布不计图块，JPEG 运动帧走的也不是图块计数。
  APD[$G]=$(awk '/^t=[0-9]+s canvas=/{for(i=1;i<=NF;i++){if($i~/^applied=/){split($i,a,"=");p=a[2]+0}
          if($i~/^fulls=/){split($i,a,"=");fu=a[2]+0} if($i~/^motion=/){split($i,a,"=");mo=a[2]+0}
          if($i~/^scrolls=/){split($i,a,"=");sc=a[2]+0}}
          v=p+fu+mo+sc; if(f==""){f=v} l=v} END{print l-f+0}' "$D/view.log")
  printf '  摘要：tile_enc 峰值=%sms lag 峰值=%sms lag 增长(后-前 1/3)=%sms 稳态交付=%s fps 无损上屏块数=+%s\n' \
    "${ENC[$G]}" "${LAG[$G]}" "${TRE[$G]}" "${DEL[$G]}" "${APD[$G]}"
}

echo "组 T：只走无损平面（量管线吞吐的地板）"
run T T-lossless-only balanced 0
echo "组 P：产品路径 · 流畅优先（量端到端新鲜度）"
run P P-product smooth 1
echo "组 V：视频+弹幕内容 · 只走无损平面（大面积非平移变化，无损必须逐块重画）"
run V V-lossless-video balanced 0 1
echo "组 W：视频+弹幕内容 · 产品路径（该由媒体平面承接）"
run W W-product-video smooth 1 1
echo "门禁（${W}x${H} @ ${FPS}fps，帧预算 ${BUD}ms）"
# 四组共用同一套判据，各自回答一个问题：
#   tile_enc ≤ 85% 帧预算 —— 无损管线的单帧代价追得上采集节拍；
#   lag 峰值有界 + 后 1/3 不减前 1/3 —— 发送端不攒历史；
#   交付：媒体组看 video= 帧率（合同帧率），纯无损组没有媒体轨，看画布是否真的在画。
for L in T P V W; do
  awk -v e="${ENC[$L]}" -v b="$BUD" 'BEGIN{exit !(e+0 <= b*0.85)}' \
    && ok "$L tile_enc 峰值 ${ENC[$L]}ms ≤ 预算 85%（$BUD ms）" \
    || bad "$L tile_enc 峰值 ${ENC[$L]}ms 超出预算 85%（$BUD ms → 上限 $(awk -v b="$BUD" 'BEGIN{printf "%.1f", b*0.85}')ms）"
  awk -v l="${LAG[$L]}" "BEGIN{exit !(l+0 <= $LAG_MAX)}" \
    && ok "$L 积压 lag 峰值 ${LAG[$L]}ms ≤ ${LAG_MAX}ms" \
    || bad "$L 积压 lag 峰值 ${LAG[$L]}ms > ${LAG_MAX}ms —— 观看端会按序播旧画面"
  awk -v t="${TRE[$L]}" 'BEGIN{exit !(t+0 <= 15)}' \
    && ok "$L lag 增长（后 1/3 − 前 1/3）${TRE[$L]}ms ≤ 15ms —— 无单调积压" \
    || bad "$L lag 增长 ${TRE[$L]}ms > 15ms —— 积压在涨，越看越延迟"
  if [ "${VIDOF[$L]}" = 0 ]; then
    awk -v a="${APD[$L]}" 'BEGIN{exit !(a+0 > 0)}' \
      && ok "$L 无损平面在画（上屏 +${APD[$L]} 块；本组无媒体轨，video= 不适用）" \
      || bad "$L 无损平面没有上屏（+${APD[$L]} 块）：管线卡死"
  else
    awk -v d="${DEL[$L]}" -v f="$FPS" 'BEGIN{exit !(d+0 >= f*0.8 && d+0 <= f*1.35)}' \
      && ok "$L 稳态交付 ${DEL[$L]} fps 合同 ${FPS}fps" \
      || bad "$L 稳态交付 ${DEL[$L]} fps 偏离合同 ${FPS}fps"
  fi
done
# W 组必须把大面积非平移变化交给媒体平面：稳态里 vsent 要有帧在跑。
VS=$(awk '{for(i=1;i<=NF;i++) if($i~/^vsent=/){split($i,a,"=");v=a[2]+0; if(v>m) m=v}} END{print m+0}' "$OUT/W-product-video/share.log")
awk -v v="$VS" 'BEGIN{exit !(v+0 > 5)}' \
  && ok "W 视频平面承接大面积变化（vsent 峰值 ${VS} fps）" \
  || bad "W 视频平面没有跑起来（vsent 峰值 ${VS} fps）：无损平面在硬扛逐块重画"

echo
echo "perf-load：PASS=$pass FAIL=$fail"
