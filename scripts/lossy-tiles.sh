#!/usr/bin/env bash
# 图块选码的 A/B：默认档（省几倍就降级到 JPEG）vs `ESS_LOSSY_SAVE_X=0`（强制无损）。
#
# 夹具：`ESS_DEMO_ANIM=0 ESS_DEMO_STEP=90` —— 整场只发生**一次**半屏照片类突变
# （合成源按 `ts/100 ms` 数格，90 ≈ t=9 s），之后画面静止。这正是用户报"很多图块慢慢覆盖"
# 的那个形状，也是 2.29 MB 地板测出来的那个夹具。
#
# 同一份二进制、只换一个环境变量（单变量），比三件事：
#   `tile_bytes` 峰值 —— 这一次变化实际花了多少字节（无损口径实测 2.29 MB）
#   `lossy=` 峰值     —— 走有损图块的块数（默认档必须 > 0；无损档必须 = 0）
#   `switches` / `fps_cap` / 落定秒数 —— 别为了省字节把平面或帧率弄坏
#
# 判据（默认档）：tile_bytes 峰值 < 1.0 MB（无量降级会是 2.3 MB 量级），且 lossy>0，
# 且 switches=0、fps_cap≥设定。画布只报"等/不等"不判死：逐字节相等已经不是这条档位的契约
# （差异的定量判据要等 `ess-gen-expected` 的 diff 模式，见 docs/IMPLEMENTATION.md）。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER（见 README）"; exit 1; }
OUT=target/lossy-tiles
SECS=${SECS:-16}
FPS=${FPS:-30}
STEP=${STEP:-90}

rm -rf "$OUT"; mkdir -p "$OUT"
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi
# 期望帧必须与 share **同源**：`step_at` 与动画格数都从环境读，漏一个就变成"拿没有
# 那半屏变化的图去比"，实测 MAE=52（半屏照片类的整块内容差），会被误读成有损糊了。
ESS_DEMO_STEP="$STEP" "$BIN/ess-gen-expected" "$OUT/expected.ppm" --w 1920 --h 1080 --anim 0 || exit 1

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
if grep -qi "address already in use" "$OUT/server.log"; then
  echo "FAIL 服务端没绑上 7880（上一轮残留还在监听）——本轮读数作废"; exit 1
fi

run_arm() {
  local label=$1 knob=$2 dir="$OUT/$1"
  mkdir -p "$dir"
  "$BIN/ess-view" --no-window --seconds $((SECS - 3)) --dump "$dir/received.ppm" >"$dir/view.log" 2>&1 &
  local view=$!
  sleep 1
  # 单变量：只有 ESS_LOSSY_SAVE_X 不同（knob 为空 = 默认档）
  if [ -n "$knob" ]; then
    ESS_PERF=1 ESS_LOSSY_SAVE_X="$knob" ESS_DEMO_ANIM=0 ESS_DEMO_STEP="$STEP" \
      ESS_DEMO_W=1920 ESS_DEMO_H=1080 "$BIN/ess-share" --demo --fps "$FPS" --seconds "$SECS" >"$dir/share.log" 2>&1 &
  else
    ESS_PERF=1 ESS_DEMO_ANIM=0 ESS_DEMO_STEP="$STEP" \
      ESS_DEMO_W=1920 ESS_DEMO_H=1080 "$BIN/ess-share" --demo --fps "$FPS" --seconds "$SECS" >"$dir/share.log" 2>&1 &
  fi
  local share=$!
  wait "$view"; taskkill //PID "$share" //F >/dev/null 2>&1
  taskkill //IM ess-share.exe //F >/dev/null 2>&1; sleep 1

  MAX_TB=$(grep -o 'tile_bytes=[0-9]*' "$dir/share.log" | sed 's/tile_bytes=//' | sort -n | tail -1)
  MAX_LOSSY=$(grep -o 'lossy=[0-9]*' "$dir/share.log" | sed 's/lossy=//' | sort -n | tail -1)
  SW=$(grep -o 'switches=[0-9]*' "$dir/share.log" | sed 's/switches=//' | sort -n | tail -1)
  LOWCAP=$(grep -o 'fps_cap=[0-9]*' "$dir/share.log" | sed 's/fps_cap=//' | sort -n | head -1)
  # 落定秒数：`missing`（观看端承认的缺口）最后一次 >0 之后，还有多少秒能干活——只看趋势
  GAPSECS=$(grep -c 'missing=[1-9]' "$dir/share.log" || true)
  if cmp -s "$dir/received.ppm" "$OUT/expected.ppm"; then SAME="逐字节相等"; else SAME="不等（有损档允许）"; fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "${MAX_TB:-?}" "${MAX_LOSSY:-?}" "${SW:-?}" "${LOWCAP:-?}" "${GAPSECS:-?}" "$SAME"
}

echo "── 单变量 A/B：同一份二进制，只换 ESS_LOSSY_SAVE_X（1920x1080@${FPS}，t≈$((STEP / 10)) s 一次性改左半屏）"
printf 'arm\ttile_bytes峰值\tlossy峰值\tswitches\tfps_cap最低\tmissing>0的秒数\t画布\n'
run_arm default ""
run_arm lossless 0

DEF_TB=$(grep -o 'tile_bytes=[0-9]*' "$OUT/default/share.log" | sed 's/tile_bytes=//' | sort -n | tail -1)
DEF_LOSSY=$(grep -o 'lossy=[0-9]*' "$OUT/default/share.log" | sed 's/lossy=//' | sort -n | tail -1)
LL_TB=$(grep -o 'tile_bytes=[0-9]*' "$OUT/lossless/share.log" | sed 's/tile_bytes=//' | sort -n | tail -1)

echo "── 断言"
if [ "${DEF_LOSSY:-0}" -gt 0 ]; then echo "PASS 默认档真的降级了：lossy=$DEF_LOSSY"; else echo "FAIL 默认档一块都没降级（lossy=0）"; fi
if [ "${DEF_TB:-999999999}" -lt 1000000 ]; then echo "PASS 默认档这次变化 $DEF_TB B < 1.0 MB（无损档 ${LL_TB:-?} B）"; else echo "FAIL 默认档仍是 $DEF_TB B，没吃到有损的好处"; fi
if [ "${DEF_TB:-0}" -gt 0 ] && [ "${LL_TB:-0}" -gt 0 ]; then
  awk -v a="$LL_TB" -v b="$DEF_TB" 'BEGIN { printf "INFO 单次变化总价 %.2f MB → %.2f MB（%.2f×）\n", a/1048576, b/1048576, a/b }'
fi

# 画布判据：无损档仍要求逐字节相等；默认档改成可算的 MAE（"流畅度第一"允许有损，但要看得清）
if "$BIN/ess-gen-expected" diff "$OUT/default/received.ppm" "$OUT/expected.ppm" --mae 8 >"$OUT/default/diff.log" 2>&1; then
  echo "PASS 默认档画面 $(tail -1 "$OUT/default/diff.log")"
else
  echo "FAIL 默认档画面 $(tail -1 "$OUT/default/diff.log")"
fi
if cmp -s "$OUT/lossless/received.ppm" "$OUT/expected.ppm"; then
  echo "PASS 无损档画布逐字节等于期望终帧（$(stat -c%s "$OUT/lossless/received.ppm") bytes）"
else
  echo "FAIL 无损档画布与期望终帧不一致"
fi
