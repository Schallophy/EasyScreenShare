#!/usr/bin/env bash
# 滚动基线的**质量列**重做（IMPLEMENTATION.md 的 O-18 / 任务「修正 SSIM 测量方法」）。
#
# 旧测法为什么整列作废：拿 `.ivf` 直接与 PNG 参考比，ffmpeg 的 ssim 滤镜会把**参考帧**
# 也转成视频的 4:2:0，于是参考的色度被二次下采样。那份误差与码率无关，把 CRF 4 和
# CRF 28 压在 0.0001 以内——码率档位完全不可区分。
#
# 正确做法：两条流都先解到 PNG（同一像素域、同一色度历史）再逐帧比。
#
# 目的不止是"把数字修对"：判据 1（等质量对比）一直被 O-18 挡着——**图块平面是无损
# （SSIM=1.0），视频平面要在滚动文字上达到同样的观感得花多少码率？** 这个比值就是
# 路径切换阈值（O-17）缺的那块证据。
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
OUT=target/scroll-quality
W=${W:-1280}
H=${H:-720}
FPS=${FPS:-30}
# 每帧滚 16 px（合成屏的一行），所以 30 fps × step=1 = 480 px/s
STEP=${STEP:-1}
SECS=${SECS:-6}
FRAMES=$((FPS * SECS))
TIERS=${TIERS:-18 23 28 32 38}

rm -rf "$OUT"; mkdir -p "$OUT/ref" "$OUT/dec"
command -v ffmpeg >/dev/null || { echo "需要 ffmpeg"; exit 1; }

# 素材只生成一次：参考帧与所有码率档同源
"$BIN/ess-scroll-dump" --w "$W" --h "$H" --frames "$FRAMES" --fps "$FPS" --step "$STEP" \
  > "$OUT/src.raw" || exit 1
RAW=$(stat -c%s "$OUT/src.raw")
echo "素材 ${W}x${H} ${FRAMES} 帧 @${FPS} fps，step=$STEP（$((16 * STEP * FPS)) px/s），${RAW} bytes"

ffmpeg -y -v error -f rawvideo -pix_fmt bgra -s "${W}x${H}" -r "$FPS" -i "$OUT/src.raw" \
  "$OUT/ref/%04d.png" || exit 1

printf '%-6s %-12s %-11s %-10s %s\n' CRF 码率kbps "SSIM(RGB域)" PSNR 说明
for CRF in $TIERS; do
  rm -f "$OUT/dec"/*.png
  ffmpeg -y -v error -f rawvideo -pix_fmt bgra -s "${W}x${H}" -r "$FPS" -i "$OUT/src.raw" \
    -c:v libx264 -preset medium -pix_fmt yuv420p -profile:v high -crf "$CRF" -g 60 \
    -keyint_min 60 -sc_threshold 0 -an "$OUT/v$CRF.mp4" || exit 1
  ffmpeg -y -v error -i "$OUT/v$CRF.mp4" "$OUT/dec/%04d.png" || exit 1
  # ffmpeg 9 对 RGB 输入按 R/G/B 三通道算，汇总值在 "All:" 里（旧版是 "SSIM Y:…"）。
  # 域是 RGB 而不是亮度 Y，比旧表更严格，也正好是彩色文字在暗底上被 4:2:0 抹掉的
  # 那部分损失——只看 Y 会把这项损失藏起来。
  S=$(ffmpeg -y -v info -i "$OUT/dec/%04d.png" -i "$OUT/ref/%04d.png" \
        -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | grep -oE "All:[0-9.]+" | tail -1 | cut -d: -f2)
  P=$(ffmpeg -y -v info -i "$OUT/dec/%04d.png" -i "$OUT/ref/%04d.png" \
        -lavfi "[0:v][1:v]psnr" -f null - 2>&1 | grep -oE "average:[0-9.]+" | tail -1 | cut -d: -f2)
  BR=$(ffprobe -v error -show_entries format=bit_rate -of csv=p=0 "$OUT/v$CRF.mp4")
  printf '%-6s %-12s %-10s %-10s %s\n' "$CRF" "$((BR / 1000))" "$S" "$P" ""
done

echo
echo "── 反向对照：旧测法（码流直接与 PNG 参考比）"
for CRF in 18 38; do
  S=$(ffmpeg -y -v info -i "$OUT/v$CRF.mp4" -framerate "$FPS" -i "$OUT/ref/%04d.png" \
        -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | grep -oE "All:[0-9.]+|Y:[0-9.]+" | tail -1)
  echo "  CRF $CRF 旧测法 $S"
done
echo "  → 两档若几乎相等，说明旧列没有区分力（O-18），上面的 PNG-vs-PNG 才是有效数字"

echo
echo "同素材、同尺寸下无损图块平面的实测码率见 IMPLEMENTATION.md §14.4（M-4）："
echo "  代码滚动 400 px/s：5 fps 426 kbps / 15 fps 596 kbps / 30 fps 1,194 kbps，质量恒为 SSIM 1.0（无损）"
# 裸流有 600+ MB，测完就丢（下次跑会重新生成）
rm -f "$OUT/src.raw"
