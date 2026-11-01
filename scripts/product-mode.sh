#!/usr/bin/env bash
# §13.1「画质·流畅度」三档的产品验收：每档**承诺什么、拿什么换**，都要在真链路上量出来。
#
# 为什么必须分开量帧率和画质：观看端每帧落一张 2 MB PPM 会把它自己的 sink 队列
# （容量 1）拖慢，实测同一素材落盘时解出 18–19 fps、不落盘 29.6–30.4 fps。
# 所以这里**不落盘**，只用观看端自己的每秒计数当交付事实；画质列交给
# scripts/video-plane-quality.sh（它落盘，因此那一列要按落盘代价读）。
#
# 两个阶段各证明一件事：
#   A 宽链路（本机 ~5.8 Mbps）自动码率：流畅优先花掉的其实是**带宽**——它按 0.15 bpp
#     要码率，所以 720p 原尺寸就能保住 30 帧；平衡档 0.09 bpp 时编码器整程报带宽受限，
#     靠把送入缩到 960x540 才保住 30 帧。两档交付帧率都达标，差别是像素数与码率。
#   B 饿码率（强制 300 kbps，最坏运动内容）：这才是真取舍。流畅优先缩尺寸保帧率，
#     清晰优先保分辨率掉帧率。若有人把 degradation_preference 或控制器阈值调反，
#     这两条断言就挂——它们是档位的存在理由，不是装饰。
#   C 先静止 6 s 再动：真实的平面切换沿（阶段 A 测的是冷启动，两回事）。判据是
#     切换后稳态帧率 + 最坏有流窗口的帧间隔合同，外加一条机制断言：切换那一瞬间
#     不许改送入尺寸（改尺寸要编码器重配 + 补关键帧，#41 的 263–422 ms 洞就是这么来的）。
#
# 用法：bash scripts/product-mode.sh   （需要 livekit-server、release 构建的 ess-share/ess-view）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/product-mode
FPS=${FPS:-30}
# 合成屏动画单位是 0.1 s：220 = 22 s 持续大面积运动，覆盖整个观看窗口
ANIM=${ANIM:-220}
VIEW_SECS=${VIEW_SECS:-20}
SECS=${SECS:-26}
STARVE=${STARVE:-300000}

command -v awk >/dev/null || { echo "需要 awk"; exit 1; }
[ -x "$BIN/ess-view.exe" ] && [ -x "$BIN/ess-share.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端"; exit 1
fi

rm -rf "$OUT"; mkdir -p "$OUT"
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
# 必须用 taskkill：Git Bash 的 kill 对本机原生 exe 无效，进程会留到下一轮把
# 7880 占死（上一版就是这么把一次扫描污染成两个发送端的）。
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
# 服务端必须真的绑上 7880 且只有我们这一个实例：上一轮残留会让这一轮的观看端
# 订阅到别的房间，读数全部作废（2026-10-29 的五档扫描就是这么被污染的）。
if grep -qi "address already in use" "$OUT/server.log"; then
  echo "FAIL 服务端没绑上 7880（上一轮残留还在监听）"; exit 1
fi
if [ "$(tasklist //FI 'IMAGENAME eq livekit-server.exe' | grep -c livekit)" != "1" ]; then
  echo "FAIL livekit-server 实例数不是 1"; exit 1
fi

pass=0; fail=0
ok()   { echo "  PASS $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL $*"; fail=$((fail + 1)); }
# 流畅度合同的唯一口径（$1=p95 $2=max，理由见 run() 里的注释）：尺子用**设定帧率的
# 周期**，不用这条流自己的 p50——用 p50 当尺子会让一条整体慢下来的流自己给自己放宽
# 标准。读数取整到毫秒，另加 2 ms 抖动容差：30 fps → p95≤85ms、max≤137ms。
gap_pass() {
  awk -v p="${1:-9999}" -v x="${2:-9999}" -v f="$FPS" \
    'BEGIN{per=1000/f; exit (p <= per*2.5+2 && x <= per*4+4)?0:1}'
}

# 跑一档：$1=标签 $2=mode $3=码率（0=自动）$4=静止期长度（10 fps 计帧，默认 0 = 一开场
# 就动，测的是冷启动；60 = 先静止 6 s，测的才是"平面切换沿"）。产出 ST 里的一组变量。
run() {
  local L=$1 M=$2 BR=$3 DELAY=${4:-0} D="$OUT/$1"
  mkdir -p "$D"
  ESS_GAP_TRACE=1 "$BIN/ess-view" --no-window --seconds "$VIEW_SECS" >"$D/view.log" 2>&1 &
  local view=$!; sleep 1
  ESS_PERF=1 ESS_DEMO_ANIM=$ANIM ESS_DEMO_ANIM_DELAY=$DELAY ESS_DEMO_BLOCK=1 \
    "$BIN/ess-share" --demo --fps "$FPS" --mode "$M" --video-bitrate "$BR" --seconds "$SECS" \
    >"$D/share.log" 2>&1 &
  local share=$!
  wait "$view"
  # 每一行都要把发送端杀干净：//PID 在 Git Bash 里对某些启动方式拿到的是包装进程，
  # 漏一个发送端在房间里，下一行的观看端就会同时解两条轨（读数翻倍，实测 59.2/120.8
  # 帧/s 对 30/60 的设定）。//IM 补一刀，并且下面有解出/送入 ≤ 1 的自检兜底。
  taskkill //PID "$share" //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; sleep 1

  # 观看端每 5 s 报一次交付帧率。第 1 个窗口是平面进入 + WebRTC 起步（实测 13–21），
  # 不能算稳态；取第 2–4 窗口的**最小值**当交付事实（合同看最差，不看平均）。
  local wins
  wins=$(grep -oE 'video=[0-9.]+帧/s' "$D/view.log" | sed 's/video=//; s/帧\/s//' | tail -n +2 | tr '\n' ' ')
  ST_steady=$(awk '{m=1e9; for (i=1;i<=NF;i++) if ($i+0 < m) m=$i+0; if (NF==0) m=-1; print m}' <<<"$wins")
  ST_windows="$wins"
  # 阶段 C 还要再让一个窗口：切换沿落在第 2 窗口（静止期固定 6 s），那一窗口的爬坡
  # 读数是"起步慢半拍"，由间隔合同管，不该算进稳态帧率。$wins 已经从第 2 窗口起，
  # 所以这里从它的第 2 个元素起取（= 原始序列的第 3 窗口）。
  ST_steady3=$(awk '{m=-1; for (i=2;i<=NF;i++) if (m<0 || $i+0<m) m=$i+0; if (m<0) m=-1; print m}' <<<"$wins")
  # **帧间隔才是流畅度合同**：窗口平均把"平面还没开"的时间也算进分母（那几秒画面由
  # 无损平面出，本来就不该有视频帧），于是 30 fps 的档被读成 19 fps。观看端现在只在
  # 真的有帧在流的时候统计间隔（断流 >500 ms 就清零重算），所以取"有流的窗口"里最差的
  # p50 / p95 / max 就是用户看到的卡顿。
  # 口径（合同在 gap_pass 里，只有一处）：p95 ≤ 2.5 个设定周期 + 2 ms，max ≤ 4 个周期 + 4 ms。
  #   p95 那一条 = 允许 5% 的间隙跳一帧再多给一点抖动（跳一帧在连续运动里是"抖一下"，不是卡）；
  #   max 那一条 = 任何一次间隙都不许到 4 帧以上，那是肉眼可辨的画面冻结。
  # 为什么 p95 不是更严：运动开始后的第 1 秒编码器只出 12–15 帧/s（爬坡，实测跨不过去），
  # 那一段的间隙正好是 2 个周期；这一档容的是"起步慢半拍"，不容的是"画面冻住"。
  # 这两条按**最坏的有流窗口**判，不看平均。
  local gv
  gv=$(grep -oE '间隔p50=[0-9]+ms p95=[0-9]+ms max=[0-9]+ms n=[0-9]+' "$D/view.log" \
    | awk -v minn=20 '
        function num(s) { sub(/.*=/, "", s); gsub(/[^0-9]/, "", s); return s + 0 }
        { p50 = num($1); p95 = num($2); mx = num($3); n = num($4);
          # R50 = 最快那个有流窗口的 p50，只当"这条流实际跑起来的节拍"报出来看；
          # W95/WMAX = 最坏窗口的读数，是合同的判据。
          if (n >= minn) { k++; if (R50 == 0 || p50 < R50) R50 = p50; if (p95 > W95) W95 = p95; if (mx > WMAX) WMAX = mx } }
        END { printf "%d %d %d %d", R50, W95, WMAX, k + 0 }')
  ST_g50=$(awk '{print $1}' <<<"$gv"); ST_g95=$(awk '{print $2}' <<<"$gv")
  ST_gmax=$(awk '{print $3}' <<<"$gv"); ST_gwin=$(awk '{print $4}' <<<"$gv")
  # 编码器实际输出：取**帧率最高的那一行**（= 编码器真在干活的那段）。上一版取"最高的
  # 分辨率"，结果抓到的是静止期的那几行 1280x720@0.0——那是声明尺寸，不是实际输出，
  # 两档就被读成了"分不开"。
  ST_enc=$(grep -oE 'enc=[0-9]+x[0-9]+@[0-9.]+' "$D/share.log" | sed 's/enc=//' \
    | awk -F'[x@]' '{k=$3*100000+$2; if (k>best) {best=k; line=$0}} END{print line}')
  ST_enc_h=$(awk -F'[x@]' '{print $2+0}' <<<"${ST_enc:-0x0@0}")
  ST_enc_fps=$(awk -F'@' '{print $2+0}' <<<"${ST_enc:-0x0@0}")
  # 这一档实际请求的码率（0.15 vs 0.09 bpp 的落点）——档位是不是空转，先看这个
  ST_pub=$(grep -m1 -oE '视频轨已发布：[0-9]+x[0-9]+ [A-Z0-9]+ [0-9]+ bps' "$D/share.log" \
    | grep -oE '[0-9]+ bps' | sed 's/ bps//')
  # 送入编码器的累计帧数（发送端合同）
  ST_fed=$(grep -oE 'video=[0-9]+' "$D/share.log" | sed 's/video=//' | sort -n | tail -1)
  ST_switches=$(grep -oE 'switches=[0-9]+' "$D/share.log" | sed 's/switches=//' | sort -n | tail -1)
  ST_qlbw=$(grep -oE 'ql_bw=[0-9.]+' "$D/share.log" | sed 's/ql_bw=//' | sort -n | tail -1)
  ST_nack=$(grep -oE 'nack=[0-9]+' "$D/share.log" | sed 's/nack=//' | sort -n | tail -1)
  printf '  %-9s 稳态窗口=%s 交付最低=%s fps 请求码率=%s 编码器实际=%s 带宽受限=%s nack=%s 切换=%s 送入=%s\n' \
    "$M" "[$ST_windows]" "${ST_steady}" "${ST_pub:-?}" "${ST_enc:-无}" "${ST_qlbw:-?}" "${ST_nack:-?}" "${ST_switches:-?}" "${ST_fed:-?}"
  printf '           帧间隔（有流的窗口 %s 个）最快窗口 p50=%sms 最坏 p95=%sms max=%sms —— 设定周期 %sms\n' \
    "${ST_gwin:-0}" "${ST_g50:-0}" "${ST_g95:-0}" "${ST_gmax:-0}" "$((1000 / FPS))"
}

echo "阶段 A：宽链路自动码率（合同=帧率都达标，看谁花带宽换像素）"
run A-smooth smooth 0; SM_A=$ST_steady; SM_A_ENC=$ST_enc; SM_A_H=$ST_enc_h; SM_A_SW=$ST_switches; SM_A_FED=$ST_fed; SM_A_PUB=$ST_pub; SM_A_QL=$ST_qlbw; SM_A_G50=$ST_g50; SM_A_G95=$ST_g95; SM_A_GMAX=$ST_gmax; SM_A_GW=$ST_gwin
run A-balanced balanced 0; BA_A=$ST_steady; BA_A_ENC=$ST_enc; BA_A_H=$ST_enc_h; BA_A_FED=$ST_fed; BA_A_QL=$ST_qlbw; BA_A_PUB=$ST_pub; BA_A_G50=$ST_g50; BA_A_G95=$ST_g95; BA_A_GMAX=$ST_gmax; BA_A_GW=$ST_gwin

for pair in "流畅优先 $SM_A" "平衡 $BA_A"; do
  set -- $pair
  if awk -v a="${2:-0}" -v f="$FPS" 'BEGIN{print (a >= f*0.9)?1:0}' <<<"" | grep -q 1; then
    ok "$1：稳态交付 ${2} fps ≥ 设定 $FPS 的 90%（帧率合同）"
  else
    bad "$1：稳态交付 ${2} fps 低于设定 $FPS 的 90%——档位没把帧率合同守住"
  fi
done
# 帧间隔合同（真正的流畅度判据）：平面出画期间最坏窗口的 p95 / max 过 gap_pass。
# 窗口平均帧率不做这个判断——它把平面还没开的时间也算进分母，30 fps 的档会被读成
# 19 fps（#41 当初就是这么立起来的）。
for pair in "流畅优先 $SM_A_G95 $SM_A_GMAX $SM_A_GW" "平衡 $BA_A_G95 $BA_A_GMAX $BA_A_GW"; do
  set -- $pair
  if [ "${4:-0}" -lt 2 ]; then
    bad "$1：只有 ${4:-0} 个有流的窗口，间隔样本不足，不能下流畅度结论"
  elif gap_pass "$2" "$3"; then
    ok "$1：平面出画期间最坏窗口帧间隔 p95=${2}ms / max=${3}ms（周期 $((1000 / FPS))ms，合同 p95≤2.5×、max≤4×）"
  else
    bad "$1：帧间隔 p95=${2}ms / max=${3}ms 超合同（周期 $((1000 / FPS))ms）——用户看得到卡顿"
  fi
done
# 档位必须是**可分辨的**：同一素材同一链路，流畅优先必须真的多要了码率。多要来的
# 那份码率**在平衡档受限时**必须换到东西（解除带宽受限，或保住更多像素）；如果
# 平衡档本来就不受限（60 fps@720p 时它只要 4.98 Mbps 也够），那"什么都没换到"才是
# 正确结果——这一档在这条链路上就是保险档，只体现在码率账单上。上一版把这种情况
# 判成 FAIL，是把"没有可买的东西"当成了"档位空转"。
if awk -v s="${SM_A_PUB:-0}" -v b="${BA_A_PUB:-0}" 'BEGIN{print (s > b)?1:0}' <<<"" | grep -q 1; then
  if awk -v sq="${SM_A_QL:-1}" -v bq="${BA_A_QL:-1}" -v sh="${SM_A_H:-0}" -v bh="${BA_A_H:-0}" \
    'BEGIN{print ((sq < bq) || (sh > bh))?1:0}' <<<"" | grep -q 1; then
    ok "流畅优先多要码率 ${SM_A_PUB} > ${BA_A_PUB} bps，且真的换到东西（带宽受限 ${SM_A_QL} vs ${BA_A_QL}，编码器实际 ${SM_A_ENC} vs ${BA_A_ENC}）"
  elif awk -v bq="${BA_A_QL:-1}" 'BEGIN{print (bq <= 0.02)?1:0}' <<<"" | grep -q 1; then
    ok "流畅优先多要码率 ${SM_A_PUB} > ${BA_A_PUB} bps；平衡档本来就不受限（带宽受限 ${BA_A_QL}、${BA_A_ENC}），所以这一档在这里是**保险档**——只多花带宽，不换可见差别（两档交付帧率都达标）"
  else
    bad "流畅优先多要了码率（${SM_A_PUB} > ${BA_A_PUB}）却什么都没换到：带宽受限 ${SM_A_QL} 不低于 ${BA_A_QL}，分辨率也不更高"
  fi
else
  bad "两档请求的码率分不开（流畅 ${SM_A_PUB} vs 平衡 ${BA_A_PUB}）——bpp 系数没进编码器"
fi
if [ "${SM_A_SW:-0}" -gt 0 ] 2>/dev/null; then
  ok "流畅优先进过媒体平面（切换计数 $SM_A_SW）"
else
  bad "流畅优先从未进平面：大面积运动还由无损块扛，帧率合同没人执行"
fi

echo "阶段 B：饿码率（强制 $((STARVE/1000)) kbps）——这才是真取舍"
run B-smooth smooth $STARVE; SM_B=$ST_steady; SM_B_H=$ST_enc_h; SM_B_FED=$ST_fed; SM_B_ENC=$ST_enc; SM_B_G50=$ST_g50; SM_B_G95=$ST_g95; SM_B_GMAX=$ST_gmax; SM_B_GW=$ST_gwin
run B-clear clear $STARVE; CL_B=$ST_steady; CL_B_H=$ST_enc_h; CL_B_ENC=$ST_enc; CL_B_G95=$ST_g95; CL_B_GW=$ST_gwin

if awk -v s="${SM_B:-0}" -v c="${CL_B:-0}" 'BEGIN{print (s >= c*1.2)?1:0}' <<<"" | grep -q 1; then
  ok "同一条 $((STARVE/1000)) kbps：流畅优先交付 ${SM_B} fps，清晰优先只有 ${CL_B} fps（≥1.2 倍）——帧率确实是用别的换来的"
else
  bad "流畅优先交付 ${SM_B} fps 没有明显高于清晰优先 ${CL_B} fps：缩尺寸的杠杆没生效"
fi
if awk -v c="${CL_B_H:-0}" -v s="${SM_B_H:-0}" 'BEGIN{print (c > s)?1:0}' <<<"" | grep -q 1; then
  ok "清晰优先保住 ${CL_B_ENC}，流畅优先让给 ${SM_B_ENC}：分辨率是流畅档花掉的代价"
else
  bad "清晰优先输出高度 ${CL_B_H} 没高于流畅优先 ${SM_B_H}：MaintainResolution 或控制器档位开关反了"
fi

# 饿码率下流畅优先仍要"看得出是连续的"：间隔合同不因码率放松（缩尺寸正是为了它）
if [ "${SM_B_GW:-0}" -ge 2 ] && gap_pass "${SM_B_G95}" "${SM_B_GMAX}"; then
  ok "300 kbps 下流畅优先帧间隔 p95=${SM_B_G95}ms / max=${SM_B_GMAX}ms 仍在合同内（最快窗口节拍 ${SM_B_G50}ms，缩尺寸换来的就是这个）"
else
  bad "300 kbps 下流畅优先帧间隔 p95=${SM_B_G95}ms / max=${SM_B_GMAX}ms 超合同（有流窗口 ${SM_B_GW} 个）——档位的承诺没兑现"
fi
# 反向对照：同一条窄管子，清晰优先必须明显更卡（否则两档没有差别）
if [ "${CL_B_GW:-0}" -lt 1 ]; then
  ok "同一条窄管子：清晰优先连一个「有流窗口」都凑不出来（交付 ${CL_B} fps）——保分辨率的代价就是这个"
elif awk -v c="${CL_B_G95:-0}" -v s="${SM_B_G95:-9999}" 'BEGIN{print (c > s*1.5)?1:0}' <<<"" | grep -q 1; then
  ok "同一条窄管子：清晰优先帧间隔 p95=${CL_B_G95}ms 远差于流畅优先 ${SM_B_G95}ms（有流窗口 ${CL_B_GW} vs ${SM_B_GW} 个）"
else
  bad "清晰优先帧间隔 p95=${CL_B_G95}ms 没比流畅优先 ${SM_B_G95}ms 差——两档在窄管子上分不开"
fi

echo "阶段 C：先静止 6 s 再动——这才是真实的平面切换沿（#41）"
run C-smooth smooth 0 60; SM_C=$ST_steady3; SM_C_G50=$ST_g50; SM_C_G95=$ST_g95; SM_C_GMAX=$ST_gmax; SM_C_GW=$ST_gwin; SM_C_ENC=$ST_enc; SM_C_QL=$ST_qlbw; SM_C_FED=$ST_fed; SM_C_SW=$ST_switches
run C-balanced balanced 0 60; BA_C=$ST_steady3; BA_C_G50=$ST_g50; BA_C_G95=$ST_g95; BA_C_GMAX=$ST_gmax; BA_C_GW=$ST_gwin; BA_C_ENC=$ST_enc; BA_C_QL=$ST_qlbw; BA_C_FED=$ST_fed; BA_C_SW=$ST_switches
for pair in "流畅优先 $SM_C $SM_C_G95 $SM_C_GMAX $SM_C_GW" "平衡 $BA_C $BA_C_G95 $BA_C_GMAX $BA_C_GW"; do
  set -- $pair
  if [ "${5:-0}" -lt 2 ]; then
    bad "$1：切换沿这一跑只凑出 ${5:-0} 个有流窗口，判不了（平面没开或观看端没解出帧）"
  elif awk -v a="${2:-0}" -v f="$FPS" 'BEGIN{print (a >= f*0.9)?1:0}' <<<"" | grep -q 1 \
       && gap_pass "$3" "$4"; then
    ok "$1：切换后稳态交付 ${2} fps，最坏窗口帧间隔 p95=$3ms / max=$4ms（周期 $((1000 / FPS))ms，合同 p95≤2.5×、max≤4×）"
  else
    bad "$1：切换后稳态 ${2} fps、最坏窗口间隔 p95=$3ms / max=$4ms（周期 $((1000 / FPS))ms）——切换沿没达标"
  fi
done
# 机制断言：切换沿上不许出现 ≥120 ms 的画面空洞（观看端 ESS_GAP_TRACE 逐条打）。
# 改送入尺寸 = 编码器重配 + 补关键帧，那笔账落在切换那一帧上就是这种洞（修 #41 之前
# 实测 263 / 287 / 341 / 422 ms）。尺寸必须预置在平面关闭期间（开场与退出沿）。
for pair in "C-smooth $OUT/C-smooth" "C-balanced $OUT/C-balanced"; do
  set -- $pair
  holes=$(grep -cE "^卡顿" "$2/view.log")
  if [ "$holes" != "0" ]; then
    bad "$1：切换这一跑出现 ${holes} 个 ≥120 ms 的画面空洞：$(grep -m1 -E '^卡顿' "$2/view.log")"
  else
    ok "$1：切换沿没有 ≥120 ms 的画面空洞（送入尺寸预置在平面关闭期间）"
  fi
done

# 污染自检：房间里挂了第二个发送端时，解出的帧会比送入还多，整表作废
for pair in "A-smooth $SM_A_FED" "A-balanced $BA_A_FED" "B-smooth $SM_B_FED" "C-smooth $SM_C_FED" "C-balanced $BA_C_FED"; do
  set -- $pair
  n=$(grep -oE 'video=[0-9.]+帧/s' "$OUT/$1/view.log" | sed 's/video=//; s/帧\/s//' \
      | awk '{s+=$1} END{print int(s*5+0.5)}')
  if awk -v a="${n:-0}" -v b="${2:-1}" 'BEGIN{print (a > b*1.05)?1:0}' <<<"" | grep -q 1; then
    bad "$1：观看端解出 ${n} > 发送端送入 ${2}——房间里挂了第二个发送端，本表作废"
  fi
done

echo
echo "三档验收：PASS=$pass FAIL=$fail"
[ "$fail" -eq 0 ] || exit 1
