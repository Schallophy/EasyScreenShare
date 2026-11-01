#!/usr/bin/env bash
# 产品视频平面的**交付画质**：观看端解出来的 H264(screencast) 帧到底是什么质量。
#
# 为什么必须从观看端量：§14.4 的 x264/AV1 档位是"编码器的能力"，不是"我们发出去、
# 被解出来的画面"。中间隔着 WebRTC 的码控（MaintainFramerate：带宽不够就降分辨率）、
# screencast 模式、丢帧与重传。对标 RustDesk / UU 远程要回答的是最后那个数。
#
# 方法（每条都是 §14.4 踩过的坑）：
#   1. 素材同源且逐帧可复现：合成屏每 100 ms 滚一格（16 px），内容唯一决定格号，
#      所以交付帧与参考帧的配对按**内容**匹配（scripts/video-quality.mjs），不按时间戳。
#   2. 两边都在 RGB 域比：参考保留原始色度，解码帧放大回 1280x720（观看端就是这么显示
#      的）后逐帧 ffmpeg ssim/psnr，与 §14.4 的 CRF 表同一把尺子。
#   3. 反向对照一：`--swap` 把解码帧红蓝互换后匹配，误差必须显著变大——证明落盘字节序
#      没搞反（搞反了整列画质数字都是假的）。
#   4. 反向对照二：各码率档必须**分得开**。若一档与四档的 SSIM 几乎相等，说明这台测量
#      台没有区分力（O-18 就是这么把结论埋了的），脚本直接判 FAIL。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/video-plane
W=${W:-1280}
H=${H:-720}
FPS=${FPS:-30}
# 合成屏动画 80 格 = 8 秒持续滚动，之后静止；平面靠运动策略进入（这才是产品路径）
ANIM=${ANIM:-80}
SECS=${SECS:-16}
VIEW_SECS=${VIEW_SECS:-13}
TIERS=${TIERS:-300000 800000 1500000 3000000}
# 产品档：默认只跑平衡档（历史上那张表就是它）。想量"流畅优先多花的带宽买回多少
# 画质"，用 MODES="balanced smooth" TIERS="0"（0 = 按档位的 bpp 自动定码率）。
MODES=${MODES:-balanced}
SAMPLE=${SAMPLE:-14}

command -v ffmpeg >/dev/null || { echo "需要 ffmpeg"; exit 1; }
command -v node >/dev/null || { echo "需要 node"; exit 1; }
[ -x "$BIN/ess-view.exe" ] && [ -x "$BIN/ess-share.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }
if [ -n "$(netstat -ano | grep ':7880.*LISTENING' || true)" ]; then
  echo "7880 已被占用，先停掉残留的服务端再跑"; exit 1
fi

rm -rf "$OUT"; mkdir -p "$OUT"
# 参考序列：81 帧覆盖动画全部格号（含静止后的最后一格）
"$BIN/ess-scroll-dump" --w "$W" --h "$H" --frames $((ANIM + 1)) --fps 10 --step 1 \
  > "$OUT/ref.raw" || exit 1
echo "参考序列 $((ANIM + 1)) 帧（每帧滚 16 px），${W}x${H}"

"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
# //PID 杀不干净时按镜像名补一刀；trap 里不要裸 wait——Git Bash 会死等一个已被
# taskkill 干掉的 Windows 作业，脚本印完表就回不来，孤儿服务端留给下一轮污染读数。
trap 'taskkill //PID "$SERVER" //F >/dev/null 2>&1; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1; taskkill //IM ess-view.exe //F >/dev/null 2>&1' EXIT
sleep 3
if grep -qi "address already in use" "$OUT/server.log"; then
  echo "FAIL 服务端没绑上 7880（上一轮残留还在监听）——本轮读数作废"; exit 1
fi

ref_png() { # 按格号取参考帧（缓存）
  local idx=$1 f="$OUT/ref_$idx.png"
  [ -s "$f" ] || ffmpeg -nostdin -y -v error -f rawvideo -pix_fmt bgra -s "${W}x${H}" -i "$OUT/ref.raw" \
    -vf "select=eq(n\,$idx)" -fps_mode passthrough -frames:v 1 "$f" >/dev/null 2>&1
  echo "$f"
}

printf '%-9s %-9s %-11s %-11s %-8s %-8s %-8s %-6s %s\n' 档 码率档 分辨率 "解出/送入" 落盘fps 内容格/s SSIM PSNR 编码器实际输出
declare -a SS=()
declare -a FPR=()
declare -a PGR=()
for M in $MODES; do
for T in $TIERS; do
  D="$OUT/${M}_t$T"; mkdir -p "$D/seq"
  "$BIN/ess-view" --no-window --seconds "$VIEW_SECS" --video-seq "$D/seq" >"$D/view.log" 2>&1 &
  VIEW=$!
  sleep 1
  ESS_PERF=1 ESS_DEMO_ANIM=$ANIM ESS_MODE=$M "$BIN/ess-share" --demo --fps "$FPS" \
    --mode "$M" --video-bitrate "$T" --seconds "$SECS" >"$D/share.log" 2>&1 &
  SHARE=$!
  wait "$VIEW"
  taskkill //PID "$SHARE" //F >/dev/null 2>&1; sleep 1

  # 反向对照：同一批帧，红蓝互换后去匹配参考
  SWAPMSE=$(node scripts/video-quality.mjs --seq "$D/seq" --ref "$OUT/ref.raw" \
    --refw "$W" --refh "$H" --secs "$VIEW_SECS" --swap 2>/dev/null \
    | grep -oE "中位 [0-9.]+" | awk '{print $2}')
  node scripts/video-quality.mjs --seq "$D/seq" --ref "$OUT/ref.raw" \
    --refw "$W" --refh "$H" --secs "$VIEW_SECS" --sample "$SAMPLE" \
    --pairs "$D/pairs.json" > "$D/match.log" 2>&1
  NORMMSE=$(grep -oE "中位 [0-9.]+" "$D/match.log" | awk '{print $2}')
  VFPS=$(grep -oE "交付帧率=[0-9.]+" "$D/match.log" | cut -d= -f2)
  # 内容进度 = 观看端净推进速率，**已裁掉素材停更后的静止尾**（裁法在 video-quality.mjs）。
  # 合成定标过三种采：完美无尾 10.0、完美带尾 6.7、真掉速带尾 6.7。不裁尾时连满分都读不到
  # 门槛 7（观看端写盘 13 s 而素材只推进 8 s），裁了才分得出"假罚"和"真掉速"（#64）。
  PROGRESS=$(grep -oE "内容进度 [0-9.]+" "$D/match.log" | awk '{print $2}')
  DIMS=$(grep -oE "解码 [0-9]+x[0-9]+" "$D/match.log" | awk '{print $2}')
  # 编码器实际输出（带宽不够时它降分辨率保帧率——这一列就是"对标远控"的那件事）
  # 取**我们送进轨道的帧率（`vsent`）最高**那一行的尺寸，并把 vsent 一起打出来。
  # 上一版按 `enc=WxH@F` 里的 @F 取最大（#57 记过：字典序那版更糟），但 @F 是编码器自己
  # 报的**输出**帧率，静止尾巴上喂 ~2 fps 时它反而虚高，报出来的尺寸就不是平面在跑那一段的
  # 真实交付尺寸。vsent 才是"平面开着、按合同帧率在喂"的直接证据。
  ENC=$(grep -aoE 'enc=[0-9]+x[0-9]+@[0-9.]+ vsent=[0-9.]+' "$D/share.log" | sed 's/enc=//' \
    | awk '{size=$1; sub(/@.*/,"",size); vs=$2; sub(/vsent=/,"",vs);
            if (vs+0 > best+0) {best=vs+0; out=size"（送帧"vs"/s）"}}
          END{if (out=="") out="NA"; print out}')

  if [ "$(awk -v a="${NORMMSE:-9e9}" -v b="${SWAPMSE:-0}" 'BEGIN{print (a>=b)?1:0}')" = "1" ]; then
    echo "  $M/t$T FAIL 通道序不对：正常匹配 MSE=$NORMMSE 不小于红蓝互换的 $SWAPMSE"
  fi
  # 送进编码器的帧数（发送端合同）vs 观看端解出来的帧数（交付事实）
  FED=$(grep -oE 'video=[0-9]+' "$D/share.log" | sed 's/video=//' | sort -n | tail -1)
  DELIV=$(grep -oE '^帧数=[0-9]+' "$D/match.log" | sed 's/帧数=//')
  if [ ! -s "$D/pairs.json" ]; then
    echo "  $M/t$T FAIL 没有可信匹配帧，这一行测不出画质（见 $D/match.log）"
    SS+=("NA"); FPR+=("$VFPS"); PGR+=("$PROGRESS"); continue
  fi

  # 逐对权威指标：解码帧放大回 WxH（观看端就是这么显示的），与原始参考帧比
  sum_s=0; sum_p=0; n=0
  node -e 'const fs=require("fs");const a=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));console.log(a.map(x=>[x.file,x.idx].join(" ")).join("\n"))' "$D/pairs.json" > "$D/pairs.txt"
  while read -r file idx; do
    [ -n "$file" ] || continue
    R=$(ref_png "$idx")
    base=${file%.ppm}
    ffmpeg -nostdin -y -v error -i "$D/seq/$file" -vf "scale=$W:$H:flags=bicubic" "$D/dec_$base.png" >/dev/null 2>&1
    S=$(ffmpeg -nostdin -y -v info -i "$D/dec_$base.png" -i "$R" -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | grep -oE "All:[0-9.]+" | tail -1 | cut -d: -f2)
    P=$(ffmpeg -nostdin -y -v info -i "$D/dec_$base.png" -i "$R" -lavfi "[0:v][1:v]psnr" -f null - 2>&1 | grep -oE "average:[0-9.]+" | tail -1 | cut -d: -f2)
    [ -n "$S" ] && { sum_s=$(awk -v a="$sum_s" -v b="$S" 'BEGIN{print a+b}'); n=$((n+1)); }
    [ -n "$P" ] && sum_p=$(awk -v a="$sum_p" -v b="$P" 'BEGIN{print a+b}')
    # 只留采样帧：一整档的 PPM 序列有 1 GB，测完就是负担
    cp "$D/seq/$file" "$D/keep_$file"
    rm -f "$D/dec_$base.png"
  done < "$D/pairs.txt"
  rm -rf "$D/seq"
  if [ "$n" -gt 0 ]; then
    MS=$(awk -v a="$sum_s" -v b="$n" 'BEGIN{printf "%.4f", a/b}')
    MP=$(awk -v a="$sum_p" -v b="$n" 'BEGIN{printf "%.1f", a/b}')
  else
    MS=NA; MP=NA
  fi
  SS+=("$MS")
  FPR+=("$VFPS")
  PGR+=("$PROGRESS")
  # 自检：房间里只能有一个发送端。解出帧数超过送入帧数 = 观看端订阅了多条
  # 视频轨（上一档的发送端没停干净），这一行是脏数据，不许进表。
  if [ "$(awk -v a="${DELIV:-0}" -v b="${FED:-1}" 'BEGIN{print (a > b*1.05)?1:0}')" = "1" ]; then
    echo "  $M/t$T FAIL 解出 ${DELIV} > 送入 ${FED}：房间里挂了第二个发送端，本行作废"
  fi
  printf '%-9s %-9s %-11s %-11s %-8s %-8s %-8s %-6s %s\n' "$M" "$((T/1000))k" "${DIMS:-?}" "${DELIV:-0}/${FED:-?}" "$VFPS" "$PROGRESS" "$MS" "$MP" "${ENC:-?}"
  echo "         匹配中位 MSE=$NORMMSE / swap=$SWAPMSE，采样 $n 帧，明细 $D/match.log"
done
done
rm -f "$OUT/ref.raw" "$OUT"/ref_*.png

echo
echo "── 断言"
# 分得开才算测到：最高档与最低档的 SSIM 差必须 > 0.01
OK=$(node -e '
const v=process.argv[1].split(" ").map(Number).filter(x=>!isNaN(x));
console.log(v.length>=2 && (v[v.length-1]-v[0])>0.01 ? "PASS" : "FAIL");' "${SS[*]}")
if [ "$OK" = "PASS" ]; then
  echo "PASS 行与行分得开（档 × 码率）：${SS[0]} → ${SS[${#SS[@]}-1]}（测量台有区分力）"
else
  echo "FAIL 码率档分不开：${SS[*]} —— 这台测量台没有区分力，数字不能用（O-18 教训）"
fi
echo "对照：无损图块平面在同一素材上是 SSIM 1.0（§14.4 M-4，400 px/s@30 fps 实测 1,194 kbps）"
echo "对照：不带屏幕内容工具的 libx264 要到 0.96 需要 14.7 Mbps（scripts/scroll-quality.sh）"
# 帧率是对用户的硬承诺，但**尺子必须是素材的内容速率**：参考素材每 100 ms 滚一格
# （上面 `--fps 10 --step 1`），观看端按内容匹配能看到的推进上限就是 10 步/s。拿设定的
# 30 fps 去比，这条永远 FAIL —— §14.5 早已警告"这一列当帧率合同读就是错的"，断言却还留着旧尺子。
# 真契约：媒体通道要把素材的内容推进大体送全（≥70% 的素材步速）。
SRC_CPS=10
TOP=${PGR[${#PGR[@]}-1]:-0}
TOPFPS=${FPR[${#FPR[@]}-1]:-0}
if [ "$(awk -v a="${TOP}" -v b="$SRC_CPS" 'BEGIN{print (a+0 >= 0.7*b)?1:0}')" = "1" ]; then
  echo "PASS 最富档内容推进 ${TOP} 步/s（素材 $SRC_CPS 步/s，落盘 ${TOPFPS} fps）——媒体通道把内容送全了"
else
  echo "FAIL 最富档只推进 ${TOP} 步/s（素材 $SRC_CPS 步/s）：观看端看到的是幻灯片，编码器/发包队列在压内容速率"
fi
