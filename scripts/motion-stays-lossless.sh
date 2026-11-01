#!/usr/bin/env bash
# 换平面的判据是"无损装不下"，不是"画面大面积变了"——真机验收。
#
# 媒体通道在这类内容上的实测交付是 12–17 fps、SSIM 0.49–0.66（§14.4 结论 4），
# 而无损图块平面同一素材是 30 fps、SSIM 1.0、约 1.2 Mbps（§14.4 M-4）。本机链路
# 估计有 4–8 Mbps，装得下，所以正确的产品行为是**根本不切平面**。
#
# 四条断言：
#   switches=0        —— 8 秒全屏滚动重绘期间平面一次都没开
#   fps_cap 全程 30    —— 帧率合同没被动过
#   无损通路在干活      —— 滚动宣告/逐块落画在增长，且报出真实上行
#   终帧逐字节一致      —— 观看端画布与期望终帧完全相同（无损的定义）
# 用 `ESS_DEMO_BLOCK=0` 跑：demo 合成屏默认带一块**独立移动**的红块，那种内容不是
# 平移，滚动检测判 None（差分要整屏重算），切平面是对的。红块开关见
# `SyntheticScreen::block` / O-23，`ess-gen-expected` 读同一个环境变量保证期望帧同源。
# 窄链路那一侧由 `crates/ess-core/tests/video_plane.rs` 的 wide/narrow 对照对锁定，
# 预算逼平面的真机路径由 `scripts/forced-plane.sh` 锁定。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/motion-lossless
SECS=${SECS:-16}
FPS=${FPS:-30}
ANIM=${ANIM:-80}

rm -rf "$OUT"; mkdir -p "$OUT"
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi
"$BIN/ess-gen-expected" "$OUT/expected.ppm" --w 1280 --h 720 --anim "$ANIM" || exit 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
# 不要裸 wait：Git Bash 的 wait 会死等一个已被 taskkill 干掉的 Windows 作业，
# 脚本印完断言就回不来，孤儿服务端留给下一轮把读数污染（forced-plane 同款坑）。
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
if grep -qi "address already in use" "$OUT/server.log"; then
  echo "FAIL 服务端没绑上 7880（上一轮残留还在监听）——本轮读数作废"; exit 1
fi

"$BIN/ess-view" --no-window --seconds $((SECS - 3)) --dump "$OUT/received.ppm" >"$OUT/view.log" 2>&1 &
VIEW=$!
sleep 1
ESS_PERF=1 ESS_DEMO_ANIM=$ANIM "$BIN/ess-share" --demo --fps "$FPS" --seconds "$SECS" >"$OUT/share.log" 2>&1 &
SHARE=$!
wait "$VIEW"; taskkill //PID "$SHARE" //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; sleep 1

echo "── 每秒：链路估计 / 图块通路需求 / 平面切换计数"
grep -E "^MOTION" "$OUT/share.log" | awk '{print "  " $0}' | tail -9

SW=$(grep -o 'switches=[0-9]*' "$OUT/share.log" | sed 's/switches=//' | sort -n | tail -1)
LOWCAP=$(grep -o 'fps_cap=[0-9]*' "$OUT/share.log" | sed 's/fps_cap=//' | sort -n | head -1)
VMODE=$(grep -c 'vmode=true' "$OUT/share.log" || true)
# 无损通路扛下这 8 秒的真实代价：整个会话里最大的那一秒上行
EG=$(grep -o 'egress=[0-9.]*' "$OUT/share.log" | sed 's/egress=//' | sort -n | tail -1)
# 滚动内容画布是**平移重建**的，不会计进 applied（那是逐块落数）；两条任一命中即算在更新
SCROLLS=$(grep -oE 'scrolls=[0-9]+' "$OUT/view.log" | sed 's/scrolls=//' | sort -n | tail -1)
APPLIED=$(grep -oE 'applied=[0-9]+' "$OUT/view.log" | sed 's/applied=//' | sort -n | tail -1)
# 混合内容走的是媒体通道：画面在不在更新，看送进编码器的帧数比看落块数更准
VIDEO=$(grep -o 'video=[0-9]*' "$OUT/share.log" | sed 's/video=//' | sort -n | tail -1)
echo "── 断言（内容模式：$([ "${ESS_DEMO_BLOCK:-1}" = "0" ] && echo 纯滚动 || echo 滚动+移动物体)）"
if [ "${ESS_DEMO_BLOCK:-1}" = "0" ]; then
  if [ "${SW:-1}" = "0" ]; then echo "PASS 平面一次都没开（switches=0，vmode=true 的秒数=$VMODE）"; else echo "FAIL 宽链路上还是切了平面：switches=$SW"; fi
else
  if [ "${SW:-0}" -ge 2 ]; then echo "PASS 混着独立移动物体的内容切了平面（switches=$SW）——那不是平移，无损要整屏重算"; else echo "FAIL 混合内容没切平面：switches=$SW"; fi
fi
if [ "${LOWCAP:-$FPS}" -ge "$FPS" ]; then echo "PASS 帧率合同没动：fps_cap 最低 $LOWCAP（设定 $FPS）"; else echo "FAIL 帧率被压到 $LOWCAP"; fi
if [ "${SCROLLS:-0}" -gt 20 ] || [ "${APPLIED:-0}" -gt 100 ] || [ "${VIDEO:-0}" -gt 100 ]; then
  echo "PASS 画面在实时更新：滚动宣告 $SCROLLS 次、逐块落画 $APPLIED、送入编码器 $VIDEO 帧、上行峰值 ${EG}Mbps"
else
  echo "FAIL 没切平面却也没更新：scrolls=${SCROLLS:-0} applied=${APPLIED:-0}"
fi
if cmp -s "$OUT/received.ppm" "$OUT/expected.ppm"; then
  echo "PASS 观看端画布逐字节等于期望终帧（$(stat -c%s "$OUT/received.ppm") bytes）——这就是 SSIM 1.0"
else
  echo "FAIL 画布与期望终帧不一致"
fi
grep -o 'avail=[0-9.]*Mbps' "$OUT/share.log" | sort -u | head -3
