#!/usr/bin/env bash
# 窄链路验收（#32）：在**真的会丢包**的链路上跑既有门禁。
#
# 为什么要单独一条：localhost 上"排队"和"脱落"永远不会发生，所以 §6.6 的预算执行器、
# 报告驱动的修复回补、以及"设定 30 帧就要 30 帧"这三件事，以前全部是在不会拥塞的链路上
# 量绿的。`ESS_EGRESS_CAP`（发送任务里的令牌桶，默认 0=关）把上限真的加回来：可靠消息排队、
# 不可靠消息丢弃，与真实窄链路同构。
#
# 这条脚本**不新造判据**，只把已有的门禁在不同上限下重跑一遍——判据的语义由被调用的脚本
# 自己维护，这里负责"上限 × 场景"的组合与汇总：
#   * demo-e2e（1280x720@10，无损通路独自扛，没有媒体平面）：终帧逐字节相等 = 脱落必须收敛；
#   * motion-stays-lossless（1920x1080，滚动 + 独立移动物体）：切平面、帧率合同不被动、
#     画面实时更新、终帧逐字节相等——上限压到 2 Mbps 时这四条必须还成立；
#   * ramp-start（1920x1080 视频内容、平衡档）：图块上限低于无损需求时应**交接**给媒体
#     平面，而不是把无损通路硬塞成幻灯片（前 31 帧 ≤ 1.5×直尺、平面内 0 空洞）。
# 用法：bash scripts/narrow-link.sh        （CAPS / CAPS_RAMP 可覆盖）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
CAPS=${CAPS:-"1000000 2000000"}
CAPS_MOTION=${CAPS_MOTION:-"2000000 4000000"}
CAP_RAMP=${CAP_RAMP:-3000000}
pass=0; fail=0
ok()  { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }
[ -x "$BIN/ess-share.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }

for CAP in $CAPS; do
  out=$(ESS_PERF=1 ESS_EGRESS_CAP=$CAP bash scripts/demo-e2e.sh 2>&1 | grep -E "^E2E" | tail -1)
  shed=$(grep -oE "shed=[0-9]+" target/e2e/share.log 2>/dev/null | tail -1 | sed 's/shed=//')
  sent=$(grep -E "^PERF" target/e2e/share.log 2>/dev/null | tail -1 | grep -oE "sent=[0-9]+" | sed 's/sent=//')
  # sent 必须真的在长：上一版仪器把桶写成"睡到还清"，一条整帧钉住发送任务 12 秒，
  # sent 冻在 1，我却把读数当成"产品不收敛"立了个案（#51）。先自证出门再谈网络。
  case "$out" in
    *PASS*)
      [ "${sent:-0}" -gt 1 ] && ok "无损通路 @$((${CAP}/1000))kbps：脱落 ${shed:-0} 条后终帧逐字节相等（sent=$sent）" \
        || bad "无损通路 @$((${CAP}/1000))kbps：终帧相等但 sent=$sent——发送任务没出门，读数作废（仪器 bug，不是产品绿灯）";;
    *) bad "无损通路 @$((${CAP}/1000))kbps：终帧不一致 $out（shed=${shed:-?} sent=${sent:-?}）";;
  esac
done

for CAP in $CAPS_MOTION; do
  out=$(ESS_EGRESS_CAP=$CAP bash scripts/motion-stays-lossless.sh 2>&1)
  n_ok=$(grep -cE "^ *PASS" <<<"$out"); n_bad=$(grep -cE "^ *FAIL" <<<"$out")
  [ "$n_bad" = "0" ] && [ "$n_ok" -ge 4 ] \
    && ok "1080p 移动内容 @$((${CAP}/1000))kbps：$n_ok 条断言全过（切平面 / 帧率合同 / 实时更新 / 终帧逐字节）" \
    || bad "1080p 移动内容 @$((${CAP}/1000))kbps：PASS=$n_ok FAIL=$n_bad"
done

# 无损需求 > 上限时，正确行为是**交接**：平面开着、按声明帧率走、平面内没有 ≥120 ms 空洞。
out=$(ESS_EGRESS_CAP=$CAP_RAMP W=1920 H=1080 MODE=balanced CONTENT=video SECS=20 REPS=1 ARMS="capped:0" \
  bash scripts/ramp-start.sh 2>&1)
r=$(grep -E "^== capped" <<<"$out" | tail -1)
sw=$(grep -oE "平面开关 [0-9]+ 次" <<<"$r" | grep -oE "[0-9]+" | head -1)
bad_n=$(grep -cE "^ *FAIL" <<<"$out")
if [ "${bad_n:-1}" = "0" ] && [ "${sw:-0}" -ge 1 ]; then
  ok "1080p 视频内容 @$((${CAP_RAMP}/1000))kbps（低于无损 5.5–11 Mbps 的需求）：平面开关 $sw 次并持有，直尺与空洞都达标"
else
  bad "1080p 视频内容 @$((${CAP_RAMP}/1000))kbps：FAIL=$bad_n、平面开关=${sw:-无}——上限压住时没交接或交接后不平滑"
fi
echo "$r"

echo
echo "narrow-link：PASS=$pass FAIL=$fail"
[ "$fail" = 0 ]
