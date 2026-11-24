#!/usr/bin/env bash
# 扇出验收：一份上传喂 N 个观看端。
#
# 这是这个立项的第一约束（服务器上行带宽），也是主流远控做不到的那一条——
# 1:1 直连的远控每加一个观看方就要多传一份。所以这里量的不是"能不能看"，
# 而是**上行是否随观看人数保持不变**，同时每个观看端的画布仍然逐字节正确。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/fanout
COUNTS="${*:-1 3 6}"
SECS=20

rm -rf "$OUT"; mkdir -p "$OUT"

if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
sleep 3
trap 'kill $SERVER 2>/dev/null; wait 2>/dev/null' EXIT

"$BIN/ess-gen-expected" "$OUT/expected.ppm" --w 1280 --h 720 --anim 30

printf '%-8s %-12s %-14s %-16s %-10s %s\n' 观看数 全程kbps 稳态kbps 编码耗时/秒 图块数 画布一致
for N in $COUNTS; do
  VIEWS=()
  for i in $(seq 1 "$N"); do
    "$BIN/ess-view" --no-window --identity "v$i" --seconds $((SECS - 2)) \
      --dump "$OUT/v$i.ppm" >"$OUT/v$i.log" 2>&1 &
    VIEWS+=($!)
  done
  sleep 1
  ESS_PERF=1 "$BIN/ess-share" --demo --fps 10 --seconds "$SECS" >"$OUT/share-$N.log" 2>&1 &
  SHARE=$!

  for p in "${VIEWS[@]}"; do wait "$p"; done
  kill "$SHARE" 2>/dev/null; wait "$SHARE" 2>/dev/null

  # 发送端自己报的累计量。两列分开看才有意义：
  #   全程 = 一次性加入成本摊进整段；
  #   稳态 = 最后两个采样点之间的增量，即"所有人都追平之后"还要发多少。
  LAST=$(grep -oE 't=[0-9]+s frames=[0-9]+ tiles=[0-9]+ msgs=[0-9]+ bytes=[0-9]+' "$OUT/share-$N.log" | tail -1)
  PREV=$(grep -oE 't=[0-9]+s frames=[0-9]+ tiles=[0-9]+ msgs=[0-9]+ bytes=[0-9]+' "$OUT/share-$N.log" | tail -2 | head -1)
  UP=$(echo "$LAST" | sed -E 's/.*bytes=([0-9]+).*/\1/'); UP=${UP:-0}
  ELAPSED=$(echo "$LAST" | sed -E 's/t=([0-9]+)s.*/\1/'); ELAPSED=${ELAPSED:-1}
  TILES=$(echo "$LAST" | sed -E 's/.*tiles=([0-9]+).*/\1/'); TILES=${TILES:-0}
  KBPS=$(( UP * 8 / 1000 / ELAPSED ))
  PUP=$(echo "$PREV" | sed -E 's/.*bytes=([0-9]+).*/\1/'); PUP=${PUP:-$UP}
  PEL=$(echo "$PREV" | sed -E 's/t=([0-9]+)s.*/\1/'); PEL=${PEL:-0}
  DT=$(( ELAPSED - PEL )); DT=${DT#-}; [ "$DT" -lt 1 ] && DT=1
  STEADY=$(( (UP - PUP) * 8 / 1000 / DT ))
  # 发送端每秒的管线编码耗时（ESS_PERF 的 PERF 行）。观看端数不同而这个数基本不变，
  # 才说明"编一次、发一次"，扇出是服务器的事。
  ENCAVG=$(sed -nE 's/.*enc=([0-9]+)ms.*/\1/p' "$OUT/share-$N.log" | awk '{s+=$1; n++} END{if(n) printf "%.0f", s/n; else printf "?"}')

  OK=1; BAD=0; FR=""
  for i in $(seq 1 "$N"); do
    if ! cmp -s "$OUT/expected.ppm" "$OUT/v$i.ppm"; then OK=0; BAD=$((BAD + 1)); fi
    FR="$FR v$i:$(grep -oE 'applied=[0-9]+' "$OUT/v$i.log" | tail -1 | cut -d= -f2)"
  done
  # 扇出真正要断言的是**各端彼此一致**（同一份上传喂出来的画面必须相同）。
  # 与 expected.ppm 的比对只在"动画已结束、终帧确定"时成立——ESS_DEMO_ANIM 拉长
  # 动画后终帧本来就不是那张图，那种情况下看 PEERS。
  PEERS=PASS; PBAD=0
  for i in $(seq 2 "$N"); do
    cmp -s "$OUT/v1.ppm" "$OUT/v$i.ppm" || { PEERS=FAIL; PBAD=$((PBAD + 1)); }
  done
  [ "$OK" = 1 ] && VERDICT="PASS" || VERDICT="FAIL（$BAD 端 ≠ 期望终帧）"
  [ "$PEERS" = PASS ] && PEERS="一致" || PEERS="FAIL（$PBAD 端不同）"
  printf '%-8s %-12s %-14s %-16s %-10s %-24s %s\n' "$N" "$KBPS" "$STEADY" "${ENCAVG}ms" "$TILES" "$VERDICT" "各端互相$PEERS"
  # 每端收到的块数应当一模一样：不一致就说明有人被落下（要靠周期报告补，
  # 但一次会话里不该出现"某一端明显收得少"）
  echo "    每端收到的块数$FR" | sed 's/applied=//g'
done
