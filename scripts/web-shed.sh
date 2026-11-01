#!/usr/bin/env bash
# 浏览器观看端（手机实际跑的那一份）新鲜度闸门的机制验收。
#
# 为什么需要它：手机收到的图块通路要在 wasm 里逐块解码再画到 canvas，能力只有桌面
# 的几分之一；跟不上时浏览器侧的 RTC 数据通道**没有反压**，消息就堆在事件循环里
# 按序播放——用户报的"时间轴一直漂移、观看端不追最新内容、延迟越来越大"。闸门加在
# app.js 里，而 Rust 的 ess-view 太快，永远量不到这条路径，所以必须用真浏览器 + CDP
# 读数来测。窗口用**固定复用**的 target/edge-viewer profile 且显式关同步与扩展：
# 每次新建可同步 profile 会让 Edge 静默装内置插件并弹一串"安装成功"提示。
#
# 两臂：
#   A 只走无损图块（ESS_VIDEO=0）：解码负担最重，用来量闸门本身的行为——桌面跟得上
#     时一条不许丢（不乱开闸门），钉住开启时**限流不冻屏**（画布代数必须继续每秒前进；
#     旧的"整段丢弃"语义下这里是 +0 代，那才是缺陷），且解除后仍收敛。
#   B 产品路径（视频平面可用）：证明真实产品配置下浏览器不需要硬扛图块。
# 用法：bash scripts/web-shed.sh   （SECS/PIN_AFTER/PIN_SECS/FPS/W/H 可覆盖）
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/release
LKSERVER=${LIVEKIT_SERVER:-../livekit-probe/livekit-server.exe}
[ -x "$LKSERVER" ] || { echo "找不到 LiveKit 服务端：$LKSERVER
  放法与校验和见 README「LiveKit 服务端怎么带」；也可用环境变量指定：
  LIVEKIT_SERVER=/c/path/to/livekit-server.exe bash scripts/xxx.sh"; exit 1; }
OUT=target/web-shed
PORT=8088
ROOM=demo
PROFILE="$PWD/target/edge-viewer"
EDGEBIN="/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe"
FPS=${FPS:-30}
W=${W:-1920}
H=${H:-1080}
SECS=${SECS:-26}
PIN_AFTER=${PIN_AFTER:-8}
PIN_SECS=${PIN_SECS:-10}
SERVE=0; SHARE=0; WIN=0

[ -x "$BIN/ess-serve.exe" ] && [ -x "$BIN/ess-share.exe" ] || { echo "先 cargo build --release -p ess-app"; exit 1; }
[ -x "$EDGEBIN" ] || { echo "找不到 Edge：$EDGEBIN"; exit 1; }
[ -f crates/ess-web/web/ess_web.wasm ] || { echo "缺 ess_web.wasm，浏览器端测不了"; exit 1; }
for p in 7880 "$PORT"; do
  if [ -n "$(netstat -ano | grep ":$p.*LISTENING" || true)" ]; then
    echo "$p 已被占用（多半是上一轮残留的 Edge 或服务端），先手工停掉那个进程再跑"
    exit 1
  fi
done

rm -rf "$OUT"; mkdir -p "$OUT"
"$LKSERVER" --dev --bind 127.0.0.1 >"$OUT/server.log" 2>&1 &
SERVER=$!
trap 'kill_edge 2>/dev/null; for pid in "$SERVER" "$SERVE" "$SHARE" "$WIN"; do [ "$pid" != 0 ] && taskkill //PID "$pid" //F >/dev/null 2>&1; done; taskkill //IM livekit-server.exe //F >/dev/null 2>&1; taskkill //IM ess-serve.exe //F >/dev/null 2>&1; taskkill //IM ess-share.exe //F >/dev/null 2>&1' EXIT
sleep 3
grep -qi "address already in use" "$OUT/server.log" && { echo "FAIL 服务端没绑上 7880"; exit 1; }
"$BIN/ess-serve" --url ws://127.0.0.1:7880 --room "$ROOM" --port "$PORT" >"$OUT/serve.log" 2>&1 &
SERVE=$!
sleep 1

pass=0; fail=0
ok()  { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }

# Edge 的调试端口由**真正的浏览器进程**持有，而 `msedge.exe` 启动器一退出就和我们
# 记下的 PID 不是同一个进程——所以按端口找 PID 杀。杀之前必须核对命令行里有没有
# 本脚本复用的 profile 路径：用户的其它 Edge 窗口绝不能被牵连。
kill_edge() {
  local pid cmd
  pid=$(netstat -ano | grep ':9223.*LISTENING' | awk '{print $NF}' | head -1 || true)
  [ -z "${pid:-}" ] && return 0
  cmd=$(powershell -NoProfile -Command "(Get-CimInstance Win32_Process -Filter 'ProcessId=$pid').CommandLine" 2>/dev/null || true)
  case "$cmd" in
    *edge-viewer*) taskkill //PID "$pid" //T //F >/dev/null 2>&1 || true ;;
    *) echo "  注意：9223 被非本脚本的进程 $pid 占用，不动它（探针可能连到错的窗口）" ;;
  esac
}

run() { # $1=臂 $2=ESS_VIDEO $3=CPU 降频倍率 $4=钉住起点秒(-1=不钉住)
  local A=$1 VID=$2 THR=${3:-1} PIN=${4:-$PIN_AFTER}
  kill_edge
  echo "== 臂 $A（${W}x${H} @ ${FPS}fps，视频平面 $([ "$VID" = 0 ] && echo 关 || echo 开)，CPU ${THR}×，$([ "$PIN" -ge 0 ] && echo "第 ${PIN}s 钉住开闸门" || echo 全自动)）"
  ESS_PERF=1 ESS_DEMO_ANIM=99999 ESS_DEMO_VIDEO=1 ESS_DEMO_W=$W ESS_DEMO_H=$H ESS_VIDEO="$VID" \
    "$BIN/ess-share" --demo --url ws://127.0.0.1:7880 --room "$ROOM" --identity share --fps "$FPS" --mode balanced \
    --seconds $((SECS + 22)) >"$OUT/$A-share.log" 2>&1 &
  SHARE=$!
  sleep 6
  # 素材自证：这一臂所有读数都建立在"合成屏内容"上。少一个 --demo 就会去采真桌面，
  # 桌面静止时平面进出与喂入速率全都"看着不对"——实测就这样把正常行为误读成发送侧缺陷。
  if ! grep -q "合成屏" "$OUT/$A-share.log"; then
    bad "$A 采的不是合成屏内容（ess-share 少了 --demo？），这一臂读数作废"
    taskkill //PID "$SHARE" //F >/dev/null 2>&1
    return
  fi
  "$EDGEBIN" --user-data-dir="$PROFILE" --app="http://127.0.0.1:$PORT/" \
    --remote-debugging-port=9223 --no-first-run --no-default-browser-check \
    --disable-sync --disable-extensions \
    --disable-component-extensions-with-background-pages \
    --disable-features=CalculateNativeWinOcclusion \
    --window-size=900,600 --window-position=100,100 >"$OUT/$A-edge.log" 2>&1 &
  WIN=$!
  sleep 9
  node scripts/shed-probe.mjs --seconds "$SECS" --pin-after "$PIN" --pin-secs "$PIN_SECS" --throttle "$THR" \
    >"$OUT/$A-probe.json" 2>"$OUT/$A-probe.err"
  kill_edge
  taskkill //PID "$WIN" //F >/dev/null 2>&1 || true
  taskkill //PID "$SHARE" //F >/dev/null 2>&1
  sleep 2
  KV=$(node -e 'const fs=require("fs");const j=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));
    const keys=["msgs_pre","msgs_pin","drop_pre","drop_pin","drop_post","drop_total","serial_pin","serial_post","serial_ps_min","serial_ps_med","serial_ps_max","stall_secs","on_n","throttle","duty_max","duty_first","duty_last","video_n","hidden_secs","errors","dims","plane","feed_per_msg","pinnedAt","unpinnedAt","recv_total"];
    for(const k of keys) console.log(k+"="+(j[k]===undefined?"nil":String(j[k])));
    console.log("auto_on_before="+(j.auto_on_before?"true":"false"));
    console.log("rows="+((j.rows||[]).length));' "$OUT/$A-probe.json" 2>/dev/null || true)
  if [ -z "$KV" ]; then bad "$A 探针没有输出（$OUT/$A-probe.err）"; return; fi
  get() { echo "$KV" | grep "^$1=" | head -1 | cut -d= -f2-; }
  echo "  读数：$(echo "$KV" | tr '\n' ' ')"
  KVS[$A]="$KV"
}
declare -A KVS
run A 0
run B 1 1 -1
# C：8× CPU 降频 ≈ 手机量级的解码能力，全自动（不钉住）。这一臂量的是反馈环本身：
# 占用高→开闸门丢历史→占用回落→恢复绘制。要的行为是"降帧但不冻屏"。
run C 0 8 -1
# D：手机场景的完整链路（产品路径 + 8× 降频）。观看端啃不动图块时，发送端必须从它
# 的报告里读出来并把画面交给媒体平面（硬件 H264），而不是让它继续逐块硬扛。
run D 1 8 -1

echo
echo "门禁（${W}x${H} @ ${FPS}fps，钉住开闸门 ${PIN_SECS}s，第 ${PIN_AFTER}s 起）"
val() { echo "${KVS[$1]}" | grep "^$2=" | head -1 | cut -d= -f2-; }
gate() { # $1=臂 $2=键 $3=op(>0|==0|<0.45) $4=说明
  local v=$2
  case "$3" in
    gt0) awk -v x="$v" 'BEGIN{exit !(x+0>0)}' ;;
    eq0) awk -v x="$v" 'BEGIN{exit !(x+0==0)}' ;;
    lt45) awk -v x="$v" 'BEGIN{exit !(x+0<0.45)}' ;;
    *) false ;;
  esac
}
for A in A B C D; do
  if [ -z "${KVS[$A]:-}" ]; then bad "$A 没有可用读数"; continue; fi
  HID=$(val "$A" hidden_secs)
  if [ "${HID:-0}" != "0" ]; then
    bad "$A 窗口有 $HID 秒不可见（rAF 被暂停：占用恒 0、画布代数冻结），这一臂读数不可信"
    continue
  fi
  MP=$(val "$A" msgs_pre); ER=$(val "$A" errors); VNV=$(val "$A" video_n)
  # "在收数据"有两条通道：图块走数据通道（msgs_*），媒体平面走视频轨（plane=video 时
  # 数据通道本来就该安静）。只看 msgs 会把"画面由视频轨出"的正常臂读成"什么都没测到"。
  if [ "${MP:-0}" -gt 0 ]; then
    ok "$A 页面真的在收数据（前段收到 $MP 条数据通道消息）"
  elif [ "${VNV:-0}" -gt 0 ]; then
    ok "$A 页面真的在收数据（数据通道安静是对的：$VNV 秒有视频帧上屏，plane=$(val "$A" plane)）"
  else
    bad "$A 页面没收到数据：这一臂什么都没测到"
  fi
  gate "$A" "$ER" eq0 && ok "$A 页内解码零错误" || bad "$A 页内解码错误 $ER 次"
  # 产品路径两臂额外量媒体平面本身：反复抖动进出（每次进入都要重新爬坡 + 关键帧）和
  # 喂不满编码器，都是观看端"卡"的直接来源，与观看端强弱无关。
  case "$A" in
    B|D)
      SW=$(grep -oE 'switches=[0-9]+' "$OUT/$A-share.log" | sed 's/switches=//' | tail -1)
      VS=$(awk '{for(i=1;i<=NF;i++) if($i~/^vsent=/){split($i,a,"="); if(a[2]+0>m) m=a[2]+0}} END{printf "%.1f", m+0}' "$OUT/$A-share.log")
      FED=$(grep -oE 'video=[0-9]+' "$OUT/$A-share.log" | tail -1 | sed 's/video=//')
      FEDED=$(awk -v f="$FED" -v s="$SECS" 'BEGIN{printf "%.1f", f/(s+8)}')
      # 真实切换次数按字段精确匹配统计（`plane=false` 是 `need_plane=false` 的后缀，
      # 用子串匹配会把翻转数算成上千，我这样误判过一次）。要求：平面一旦进入就持有
      # 足够长的游程，或整场切换次数很少——两者都不满足就是用户可见的糊-清循环。
      TRANS=$(awk '/^DECIDE/{p=-1;for(i=1;i<=NF;i++){if($i=="plane=true"){p=1;break}if($i=="plane=false"){p=0;break}}
          if(p<0)next; if(f==0){cur=p;len=1;f=1;next} if(p!=cur){t++; if(len>mx)mx=len; cur=p; len=1} else len++}
          END{if(len>mx)mx=len; printf "%d %d %d", t+0, mx+0, NR+0}' "$OUT/$A-share.log")
      if [ "$(awk '{print $3}' <<<"$TRANS")" -gt 100 ]; then
        TR_N=$(awk '{print $1}' <<<"$TRANS"); TR_MX=$(awk '{print $2}' <<<"$TRANS"); TR_F=$(awk '{print $3}' <<<"$TRANS")
        echo "  DECIDE：切换 $TR_N 次，最长持有 $TR_MX 帧 / $TR_F 帧"
        awk -v t="$TR_N" -v mx="$TR_MX" 'BEGIN{exit !(t+0<=6 || mx+0>=150)}' \
          && ok "$A 平面持有稳定（切换 $TR_N 次 / 最长持有 $TR_MX 帧）" \
          || bad "$A 平面碎进碎出（切换 $TR_N 次，最长只持有 $TR_MX 帧）：每次进入都要爬坡+关键帧"
      fi
      awk -v s="${SW:-99}" 'BEGIN{exit !(s+0<=3)}' \
        && ok "$A 平面切换计数正常（switches=${SW:-?}）" \
        || echo "  注意：$A 的 switches=${SW:-?} 高于 3；这个计数含订阅/重连引起的进出，真实判据看下面的 DECIDE 游程"
      awk -v v="${VS:-0}" -v f="$FPS" 'BEGIN{exit !(v+0>=f*0.8)}' \
        && ok "$A 媒体平面喂得满（vsent 峰值 ${VS} fps / 设定 ${FPS}，喂入均速 ${FEDED} fps）" \
        || bad "$A 媒体平面只喂到 vsent 峰值 ${VS} fps（设定 ${FPS}，喂入均速 ${FEDED}）：画面实际帧率由它决定，观看端必然卡"
      ;;
  esac
  if [ "$A" = "A" ]; then
    DP0=$(val A drop_pre); DP=$(val A drop_pin); SP=$(val A serial_pin); SP2=$(val A serial_post)
    AUTO=$(val A auto_on_before); DUTY=$(val A duty_max); PMP=$(val A msgs_pin)
    gate A "$DP0" eq0 && ok "A 跟得上时一条不丢（钉住前 drop=$DP0）——闸门不会乱开，不会自造空洞" || bad "A 跟得上也丢了 $DP0 条：白丢历史会造出可见空洞"
    # 钉住开闸门测的是**机制本身**：它必须"限流不冻屏"。信箱每帧解一条，桌面跟得上时
    # 一条都不用丢；能力不够时才丢被取代的那些（那一面由 C 臂在 8× 降频下判）。
    # 旧断言写的是"钉住期间必须丢 >0 条"，那钉的是"整段丢弃"的老语义——它会把正确
    # 行为（每帧都解得开，所以无须丢）判成 FAIL。
    gate A "$SP" gt0 && ok "A 钉住开闸门期间画布仍在前进 +$SP 代（限流不是断流）" || bad "A 钉住开闸门后画布整段冻结（+$SP 代）：脱落把会话拖死了"
    if [ "${DP:-0}" -gt 0 ]; then
      ok "A 钉住期间丢了 $DP 条被取代的过时更新（期间另收到 $PMP 条可靠消息）"
    else
      ok "A 钉住期间一条不丢：每帧都解得开信箱，没有过时件可丢（该丢的是能力不够时，见 C 臂）"
    fi
    gate A "$SP2" gt0 && ok "A 解除闸门后画布恢复更新 +$SP2（脱落可逆，没有把会话拖死）" || bad "A 解除后画布不再更新"
    [ "$AUTO" = "false" ] && ok "A 自动闸门在桌面负载下没触发（占用峰值 $DUTY < 0.45）" || bad "A 自动闸门在桌面就触发了（占用峰值 $DUTY）：阈值太松，会白丢帧"
  elif [ "$A" = "B" ]; then
    DUTY=$(val B duty_max); PL=$(val B plane); DM=$(val B dims); DR=$(val B drop_total)
    gate B "$DUTY" lt45 && ok "B 产品路径（${DM}，平面 ${PL}）浏览器解码占用峰值 $DUTY < 0.45：真实会话里闸门不该开，也不开" || bad "B 产品路径占用峰值 $DUTY ≥ 0.45：浏览器在硬扛解码，手机只会更差"
    gate B "$DR" eq0 && ok "B 全程零丢弃（drop=$DR）" || bad "B 产品路径也在丢：$DR 条"
  elif [ "$A" = "C" ]; then
    AUTO=$(val C auto_on_before); DT=$(val C drop_total); ST=$(val C stall_secs)
    MED=$(val C serial_ps_med); ON=$(val C on_n); ROWS=$(val C rows); DUTY=$(val C duty_max)
    [ "$AUTO" = "true" ] && ok "C 8× 降频下闸门自己触发了（占用峰值 $DUTY ≥ 0.45）——阈值不是摆设" || bad "C 8× 降频下闸门没触发（占用峰值 $DUTY）：手机上根本不会生效，这闸门白加"
    gate C "$DT" gt0 && ok "C 自动模式丢弃 $DT 条过时更新" || bad "C 自动模式一条没丢：过载时闸门没动作"
    gate C "$MED" gt0 && ok "C 逐秒画布代数中位数 +$MED（画面在动，只是帧率降下来）" || bad "C 逐秒画布代数中位数 $MED：画面基本不动"
    gate C "$ST" eq0 && ok "C 没有一整秒的冻屏（stall=$ST / $ROWS 秒）" || bad "C 出现 $ST 个整秒冻屏（共 $ROWS 秒）：丢得太狠，反馈环没收敛"
    # "常开"本身不是病：限流会一直开着，直到能力回来。病的是开着就不动——所以这条
    # 判据从"开启秒数 < 80%"改成"开着的时候画布仍然每秒前进"（旧写法钉的是整段丢弃
    # 时代的抖动，反而把"降帧不冻屏"判成 FAIL）。
    awk -v st="$ST" -v m="$MED" 'BEGIN{exit !(st+0==0 && m+0>=5)}' \
      && ok "C 闸门开启 $ON/$ROWS 秒期间画布每秒都在前进（中位 +$MED 代、stall=$ST）——是降帧不是断流" \
      || bad "C 闸门开启 $ON/$ROWS 秒却 stall=$ST、中位代数 +$MED：脱落没在限流，是在断流"
  else
    VN=$(val D video_n); ROWSD=$(val D rows); D1=$(val D duty_first); DL=$(val D duty_last)
    st_note="$(val D stall_secs)s（视频平面期间图块画布本来不动，仅记录）"
    SF=$(grep -oE 'strain_forced=[0-9]+' "$OUT/D-share.log" | sed 's/strain_forced=//' | sort -n | tail -1)
    # 视频平面出画时图块画布**本来就不动**，stall_secs（画布代数停顿的秒数）在 D 臂不是
    # 缺陷信号——别拿它当门禁，那会把正确行为判成冻屏。观看端是否真的流畅由媒体平面自己
    # 的计数（视频轨解码帧）证明，这里量的是"平面是否持续承接"。
    gate D "$VN" gt0 && ok "D 弱观看端（8× 降频）由媒体平面持续承接（${VN}/${ROWSD}s 在视频平面，strain_forced=${SF:-0}）" \
      || bad "D 弱观看端下画面仍由图块通路硬扛（${VN}/${ROWSD}s 在视频平面）：手机会继续卡"
    echo "  读数：D 解码占用 首秒 $D1 → 末秒 $DL；图块画布停顿 $st_note"
  fi
done

echo
echo "web-shed：PASS=$pass FAIL=$fail"
[ "$fail" = 0 ]
