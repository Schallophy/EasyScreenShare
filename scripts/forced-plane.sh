#!/usr/bin/env bash
# 带宽执行器的真机样本：让"无损图块塞不进链路"这件事真的发生一次。
#
# localhost 上没有差网络，但预算不是猜的——它由编码器自己报的受限原因算出来：
# 把视频轨目标码率压到 300 kbps 并让画面持续重绘，H264 就会报 bandwidth，于是
#   tile_budget = (可用上行 − 视频轨目标) x 0.6
# 落到 200 kbps 的下限。这就是那条塞不下无损更新的窄管子。
#
# ESS_MOTION=0 关掉「大面积运动 → 视频平面」那套策略判定，于是平面切换**只能**由预算
# 引起；不加这一条，日志里的 vmode=1 可能只是策略本来就要切，证明不了执行器。
#
# ESS_STRAIN=0 同理关掉第三条理由（观看端啃不动图块）。这条是 2026-11-06 补的：
# 同一段整屏动画在慢机器上会让观看端**真的**落后，于是 strain 抢在预算前面翻平面，
# 预算那条契约当帧没有机会计数（实测 forced_plane=0 / strain_forced=1，终帧与帧率
# 都是对的）。门禁要么证明预算，要么证明 strain，不能两个都想要——strain 那条由
# `strain-static-exit.sh` 单独验。
#
# 三条断言：
#   forced_plane > 0      —— 执行器真的动过（不是只报判定）
#   fps_cap 全程 30        —— 带宽受限没有拿去动帧率（帧率是对用户的承诺）
#   终帧逐字节等于期望      —— 预算松开之后按"先修复再 off"退回无损通路
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/forced-plane
SECS=22
FPS=30
# ESS_DEMO_ANIM 的单位是 0.1 秒（合成屏按 ts/100ms 取动画帧），与 --fps 无关：80 = 动画 8 秒
ANIM=80

rm -rf "$OUT"; mkdir -p "$OUT"
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi

"$BIN/ess-gen-expected" "$OUT/expected.ppm" --w 1280 --h 720 --anim "$ANIM" || exit 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
# //PID 杀不干净时按镜像名补一刀，并且**不要裸 wait**：上一版就是 trap 里的裸 wait
# 等一个已被 taskkill 干掉的 Windows 作业，脚本印完断言就再也回不来，孤儿服务端
# 留给下一轮——下一轮的新服务端绑不上 7880，量出来的全是脏数据。
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1' EXIT
sleep 3
# 自检：这一轮的服务端真的起来了、而且只有我们这一个。绑定失败的典型症状是
# "address already in use"，此时房间由上一轮的孤儿进程服务，读数一律不可信。
if grep -qi "address already in use" "$OUT/server.log"; then
  echo "FAIL 服务端没绑上 7880（上一轮残留还在监听）——本轮读数作废"; exit 1
fi
if [ "$(tasklist //FI "IMAGENAME eq livekit-server.exe" | grep -c livekit)" != "1" ]; then
  echo "FAIL livekit-server 实例数不是 1，房间里有别的发送端"; exit 1
fi

"$BIN/ess-view" --no-window --seconds $((SECS - 3)) --dump "$OUT/received.ppm" \
  >"$OUT/view.log" 2>&1 &
VIEW=$!
sleep 1
ESS_PERF=1 ESS_MOTION=0 ESS_STRAIN=0 ESS_DEMO_ANIM=$ANIM \
  "$BIN/ess-share" --demo --fps "$FPS" --video-bitrate 300000 --seconds "$SECS" \
  >"$OUT/share.log" 2>&1 &
SHARE=$!

wait "$VIEW"
taskkill //PID "$SHARE" //F >/dev/null 2>&1; wait "$SHARE" 2>/dev/null; sleep 1

echo "── 每秒：采集上限 / 图块通路预算 / 被预算逼进平面的累计帧数 / 编码器实际输出"
grep -E "^STATS " "$OUT/share.log" | awk '{
  cap=""; bud=""; fp=""; enc="";
  for (i = 1; i <= NF; i++) {
    if ($i ~ /^fps_cap=/)          cap = substr($i, 9);
    else if ($i ~ /^tile_budget=/) bud = substr($i, 13);
    else if ($i ~ /^forced_plane=/) fp = substr($i, 14);
    else if ($i ~ /^enc=/)         enc = substr($i, 5);
  }
  printf "cap=%-3s budget=%-8s forced=%-4s enc=%s\n", cap, bud "bps", fp, enc;
}'
grep -E "^MOTION " "$OUT/share.log" | tail -3

FORCED=$(grep -o 'forced_plane=[0-9]*' "$OUT/share.log" | sed 's/forced_plane=//' | sort -n | tail -1)
LOWCAP=$(grep -o 'fps_cap=[0-9]*' "$OUT/share.log" | sed 's/fps_cap=//' | sort -n | head -1)
echo "── 断言"
if [ "${FORCED:-0}" -gt 0 ]; then echo "PASS 执行器动过：forced_plane=$FORCED 帧"; else echo "FAIL 全程 forced_plane=0，预算没真的压制图块通路"; fi
if [ "${LOWCAP:-$FPS}" -ge "$FPS" ]; then echo "PASS 帧率没被动过：fps_cap 最低 $LOWCAP（设定 $FPS）"; else echo "FAIL 带宽受限却砍了帧率：fps_cap 掉到 $LOWCAP"; fi
SW=$(grep -o 'switches=[0-9]*' "$OUT/share.log" | sed 's/switches=//' | tail -1)
if [ "${SW:-0}" -ge 2 ]; then
  echo "PASS 静止后退回无损：切换计数 $SW（进一次 + 退一次）"
else
  echo "FAIL 画面停了还留在有损平面（switches=$SW），观看端拿到的是糊掉的小字"
fi
if cmp -s "$OUT/received.ppm" "$OUT/expected.ppm"; then
  echo "PASS 退回无损后画布逐字节等于期望终帧（$(stat -c%s "$OUT/received.ppm") bytes）"
else
  echo "FAIL 画布与期望终帧不一致：$(stat -c%s "$OUT/received.ppm") vs $(stat -c%s "$OUT/expected.ppm")"
fi
