'use strict';

// 浏览器观看端：LiveKit 负责连接，ess-core 的接收管线（WASM）负责一切状态。
// JS 只做五件事：喂二进制消息、画图块画布、挂视频轨、挂系统声音轨、每 500 ms 上报状态。
// 画面由哪个平面出（图块 or 视频轨）由 WASM 侧的状态机决定，JS 只跟随。

'use strict';

const { Room, RoomEvent } = LivekitClient;

const hud = document.getElementById('hud');
const canvas = document.getElementById('screen');
const video = document.getElementById('stream');
const soundBtn = document.getElementById('sound');
const stage = document.getElementById('stage');
const content = document.getElementById('content');
const chromeEl = document.getElementById('chrome');
const statusEl = document.getElementById('status');
const zoomEl = document.getElementById('zoomLabel');
const fitBtn = document.getElementById('fit');
const fullBtn = document.getElementById('fullscreen');
const debugBtn = document.getElementById('debug');
const overlay = document.getElementById('overlay');
const ovTitle = document.getElementById('ovTitle');
const ovText = document.getElementById('ovText');
const ovSpin = document.getElementById('spin');
const retryBtn = document.getElementById('retry');
const bannerEl = document.getElementById('banner');
const toastEl = document.getElementById('toast');
const ctx = canvas.getContext('2d', { alpha: false, willReadFrequently: true });

let ex = null;          // wasm exports
let mem = null;         // WebAssembly.Memory
let viewer = 0;
let feedPtr = 0, feedCap = 0;
let reportPtr = 0, reportCap = 65536;
let diagPtr = 0;
let dirtyPtr = 0;
let img = null, img32 = null, curW = 0, curH = 0;
let drawnSerial = 0;
let frames = 0, fps = 0, lastFpsAt = performance.now(), vPrev = 0;
let connected = false;
let lastError = null;
let videoOn = false;    // 发送端宣告：现在由视频轨出画
let vTrack = null;      // 已订阅的视频轨
let videoArmed = true;  // 视频轨已交出 on 之后的新画面（未 armed 就继续显示画布）
let vArmedBase = 0;     // on 到达时的累计解码帧数
// 图块平面挂久了就**退订**视频轨。发送端图块平面下仍按 ~2 fps 喂编码器（保活，让
// 切进视频平面时画面是当前的），但那几帧在浏览器里会被真的收下来、解码、再丢掉——
// 原生观看端有 `VideoMode` 闸门，浏览器没有。实测图块平面下 inbound-rtp 视频仍有
// 1440x808@2fps。所以：`plane=tiles` 持续 10 s 就把订阅摘掉，进入平面时再挂上
// （代价是一次关键帧，和发送端停喂的代价是同一笔）。
const UNSUB_AFTER_MS = 10000;
let videoPub = null;    // 视频轨的 RemoteTrackPublication（退订/复订要它）
let videoSubOff = false;// 是我们主动摘掉的（不是发送端取消发布）
let unsubTimer = null;
let roomRef = null;     // connect 里的 room，给退订逻辑按 sid 找 publication 用
let aTrack = null;      // 已订阅的系统声音轨
let soundEl = null;     // 声音挂在独立的 <audio> 上（video 元素是 muted 的，为的是绕开自动播放限制）
let soundMuted = true; // 默认静音：观看端与分享端同机时外放会被环回再抓一遍；用户也要求默认不开声音。要听就点"声音 开"
// 开发期**不在刷新后恢复“开”**：同机测试时自动外放会立刻形成环回回声（分享端把
// 扬声器输出再抓回去），整台机器都没法用。只保留“本次会话内”的选择，每次加载
// 都从静音开始。
const state = { msgs: 0, errors: 0, w: 0, h: 0, serial: 0, feed_ms: 0, feed_n: 0, dropped: 0, loss: 0, duty: 0, partsDropped: 0, reports: 0 };

// 观看端新鲜度闸门（手机上最关键的一条）。RTC 数据通道在浏览器侧没有反压：解码跟不上
// 到达速率时，消息就堆在事件循环里按序播放——表现正是"时间轴一直漂移、观看端不追最新
// 内容、延迟越来越大"。手机/wasm 的解码能力只有桌面的几分之一，所以按**实测解码占用**
// 决定丢不弃：占用超上限就丢掉不可靠通路的包（逐块更新、JPEG 运动帧），可靠通路
// （整帧基线、滚动宣告、精确修复、状态报告）一条不丢。丢了不漏画：每 500 ms 的状态
// 报告会让发送端按**当前**画面把缺口补回来。
//
// 闸门开着时**限流到"最新一帧"**，不是整段丢弃。全丢在真机上的表现是画布整秒
// 冻结、然后成串补上（1920x1080 + 8× 降频实测逐秒代数 0/11 交替、2 个整秒冻屏），
// 那正是用户报的"不追最新内容"；信箱只留最新一帧则把"停走"变成"降帧"，队列深度恒
// ≤1 帧，积压不可能再长起来。
//
// **"一帧"不是"一条消息"**：发送端的 `split_update` 会把一次逐块更新按 60 KB 拆成
// 十几条消息，它们共享同一个 seq。只留最后一条等于把一帧的其余拆件全丢掉——变化的
// 块只上屏了最后几个，其余停在旧内容（用户报的"帧率一高就有图块残留"），报告重补
// 回来的块也会被同样丢掉。所以信箱按 seq 分组，整组保留、整组替换。
let shedOn = false;
/** 最新一帧：{ seq, parts: Uint8Array[] }；`seq` 相同 = 同一次更新的拆件 */
let shedMailbox = null;
// 自动化验收用的手动钉住：真机手机上"解码跟不上"要等得到，测试里直接钉住开/关，
// 量的是闸门本身的行为（丢不可靠、留可靠、画布仍然收敛）。
let shedPinned = false;
const SHED_ON = 0.45, SHED_OFF = 0.30;
let dutyWinStart = performance.now(), dutyWinFeed = 0, dutyWinDraw = 0;
const RELIABLE = (LivekitClient.DataPacket_Kind && LivekitClient.DataPacket_Kind.RELIABLE) || 0;

// 只从线格式里取 seq：Envelope{ body = field N (len) } → body.hdr (field 1, len) →
// Header.seq (field 1, varint)。只读几个字节，不替代 wasm 的判定；拆件要按 seq 分组，
// JS 侧必须先认得出"这两条是同一帧的"。
function readVarint(b, i) {
  let v = 0, shift = 0;
  for (;;) {
    if (i >= b.length || shift > 35) return null;
    const c = b[i++];
    v += (c & 0x7f) * 2 ** shift; // 用乘法而不是移位：seq 是 u32，移位会落进有符号 32 位
    if ((c & 0x80) === 0) return [v >>> 0, i];
    shift += 7;
  }
}

function skipField(b, i, wt) {
  if (wt === 0) {
    const r = readVarint(b, i);
    return r ? r[1] : -1;
  }
  if (wt === 1) return i + 8 <= b.length ? i + 8 : -1;
  if (wt === 2) {
    const r = readVarint(b, i);
    if (!r) return -1;
    const end = r[1] + r[0];
    return end <= b.length ? end : -1;
  }
  if (wt === 5) return i + 4 <= b.length ? i + 4 : -1;
  return -1;
}

function msgSeq(b) {
  try {
    let r = readVarint(b, 0);
    if (!r || (r[0] & 7) !== 2) return -1;
    let i = r[1];
    r = readVarint(b, i);
    if (!r) return -1;
    i = r[1];
    const end = Math.min(b.length, i + r[0]);
    // body 里 hdr 不一定是第一个字段，扫到 field 1 为止
    while (i < end) {
      r = readVarint(b, i);
      if (!r) return -1;
      i = r[1];
      const field = r[0] >>> 3, wt = r[0] & 7;
      if (field === 1 && wt === 2) {
        r = readVarint(b, i);
        if (!r) return -1;
        i = r[1];
        const hend = Math.min(end, i + r[0]);
        while (i < hend) {
          r = readVarint(b, i);
          if (!r) return -1;
          i = r[1];
          if (r[0] === 0x08) return (readVarint(b, i) || [-1])[0];
          i = skipField(b, i, r[0] & 7);
          if (i < 0) return -1;
        }
        return -1;
      }
      i = skipField(b, i, wt);
      if (i < 0) return -1;
    }
  } catch (e) {
    return -1;
  }
  return -1;
}

// 不可靠消息进信箱：同一 seq 的拆件全部留下，更新的 seq 一到就整组替换。
function shedPush(bytes) {
  const seq = msgSeq(bytes);
  // 认不出 seq（理论上只有控制消息会这样，而它们走可靠通路、根本到不了这里）：
  // 宁可立刻解，也不要吞掉一帧内容。
  if (seq < 0) {
    feed(bytes);
    return;
  }
  if (shedMailbox && seq < shedMailbox.seq) {
    state.dropped++; // 迟到的旧帧
    return;
  }
  if (shedMailbox && seq === shedMailbox.seq) {
    shedMailbox.parts.push(bytes);
    return;
  }
  // 更新的一帧：整组替换。旧组里每一条都是过时的局部内容。
  if (shedMailbox) state.dropped += shedMailbox.parts.length;
  shedMailbox = { seq, parts: [bytes] };
}

function shedUpdate(feedDelta, drawDelta) {
  dutyWinFeed += feedDelta;
  dutyWinDraw += drawDelta;
  const now = performance.now();
  const span = now - dutyWinStart;
  if (span < 1000) return;
  const duty = (dutyWinFeed + dutyWinDraw) / span;
  state.duty = duty;
  dutyWinStart = now; dutyWinFeed = 0; dutyWinDraw = 0;
  if (shedPinned) return;
  if (!shedOn && duty > SHED_ON) shedOn = true;
  else if (shedOn && duty < SHED_OFF) shedOn = false;
}

// ── 传输速率（详情面板） ───────────────────────────────────────────────────
// 速率只能从 WebRTC 的 getStats 读，而 LiveKit 的 umd 不导出它，SDK 内部字段名又会
// 随版本变（#60 的探针就是这么失效的）。所以在 SDK 之下包一层构造函数，把它自己建
// 的 RTCPeerConnection 收进来——只读 getStats 里的标准字段，与 SDK 版本无关。
// livekit 只在运行时 `new RTCPeerConnection(...)`、没有缓存构造函数（umd 里就那么一
// 处），所以在本文件（脚本顺序在它之后）替换依然生效。
const peerConns = [];
if (typeof window.RTCPeerConnection === 'function' && typeof Proxy === 'function') {
  window.RTCPeerConnection = new Proxy(window.RTCPeerConnection, {
    construct(target, args, newTarget) {
      const pc = Reflect.construct(target, args, newTarget);
      peerConns.push(pc);
      return pc;
    },
  });
}

// 数据通道（图块更新 / 状态报告）的字节由自己数：WebRTC 的 stats 只有 RTP 的收发
// 字节，没有 SCTP 应用消息的计数。两条通路分开报，才看得出"贵的是画面还是媒体"。
let tilesRx = 0, tilesTx = 0;
const net = { down: 0, up: 0, tiles: 0, media: 0 };    // bit/s
let netAt = 0, netMedia = 0, netRx = 0, netTx = 0;     // 上一次采样的基线（累计字节）
let netBusy = false;

function fmtRate(bps) {
  if (!(bps > 0)) return '0 bps';   // 带单位：详情里"图块 0"会被读成一个计数而非速率
  if (bps < 1e3) return `${Math.round(bps)} bps`;
  if (bps < 1e6) return `${bps < 1e5 ? (bps / 1e3).toFixed(1) : Math.round(bps / 1e3)} kbps`;
  return `${(bps / 1e6).toFixed(2)} Mbps`;
}

// 1 s 一拍，全部用累计字节做差分：这样对丢包/重传不敏感——要的是"链路上真实的
// 字节流速"，不是应用层有效载荷。
async function pumpNet() {
  if (netBusy) return;
  netBusy = true;
  const now = performance.now();
  let media = 0, seen = false;
  for (let i = peerConns.length - 1; i >= 0; i--) {
    const pc = peerConns[i];
    if (pc.signalingState === 'closed') { peerConns.splice(i, 1); continue; }
    try {
      const report = await pc.getStats();
      report.forEach((r) => {
        if (r.type === 'inbound-rtp' && typeof r.bytesReceived === 'number') media += r.bytesReceived;
      });
      seen = true;
    } catch (e) { /* 连接正在拆：这一拍跳过，别把异常当成 0 速率 */ }
  }
  netBusy = false;
  if (!seen) return;
  const dt = (now - netAt) / 1000;
  // 重连会换一对 pc，计数器从头开始。总量变小就只更新基线，否则会报一个假的负速率。
  if (netAt && dt > 0.2 && media >= netMedia) {
    const m = (media - netMedia) / dt;
    const t = Math.max(0, tilesRx - netRx) / dt;
    const u = Math.max(0, tilesTx - netTx) / dt;
    // 半衰平滑：1 s 窗口本身够短，不平滑的话读数每秒跳一次，看不出趋势
    net.media = net.media ? net.media * 0.5 + m * 0.5 : m;
    net.tiles = net.tiles ? net.tiles * 0.5 + t * 0.5 : t;
    net.up = net.up ? net.up * 0.5 + u * 0.5 : u;
    net.down = net.tiles + net.media;
  }
  netAt = now; netMedia = media; netRx = tilesRx; netTx = tilesTx;
  hud.textContent = hudText();
}

function hudText() {
  const s = connected ? '已连接' : '连接中…';
  const rate = fps > 0 ? `${fps} fps` : '静止';
  const plane = videoOn ? (vTrack ? '视频轨' : '视频(等待轨道)') : '图块';
  const snd = aTrack ? (soundMuted ? '声音:关' : '声音:开') : '声音:无';
  // 速率单独占一行：它每秒才变一次，混进状态行会把别的读数挤得对不齐。
  // 第一次采样完成（netAt 非 0）之前不显示，免得连接前挂一排 0。
  const flow = netAt
    ? `\n下行 ${fmtRate(net.down)}（图块 ${fmtRate(net.tiles)} · 媒体 ${fmtRate(net.media)}）` +
      `  上行 ${fmtRate(net.up)}`
    : '';
  return `${s}  ${state.w}x${state.h}  ${plane}  ${rate}  ${snd}  msgs=${state.msgs}` +
    (state.dropped ? ` 丢=${state.dropped}` : '') +
    (state.loss ? ` 漏=${state.loss}` : '') +
    (shedOn ? ` 解码${Math.round(state.duty * 100)}%` : '') +
    flow +
    (lastError ? `\n${lastError}` : '');
}

// ── 观看端界面状态（桌面优先） ─────────────────────────────────────────────
// 桌面：滚轮缩放、左键拖动平移、双击 适应/1:1、F 全屏、0/1 复位、S 声音。
// 触摸沿用同一套 Pointer Events（单指拖动、双指捏合），不额外维护两套代码。

let viewState = 'connecting';   // connecting | waiting | live | reconnecting | error
let viewMode = 'fit';           // fit | manual
let relZoom = 1;                // 相对“适应”的倍率（1 = 适应）
let panX = 0, panY = 0;         // 相对居中的屏幕像素偏移
let chromeTimer = 0;
let toastTimer = 0;
let soundHinted = false;
let wakeLock = null;
const pointers = new Map();
let drag = null;
let pinch = null;
let lastTapAt = 0;

function mediaSize() {
  if (videoOn && !video.hidden && video.videoWidth > 0) {
    return [video.videoWidth, video.videoHeight];
  }
  return [canvas.width || state.w, canvas.height || state.h];
}

function fitScale(w, h) {
  const vw = stage.clientWidth || innerWidth;
  const vh = stage.clientHeight || innerHeight;
  if (!w || !h) return 1;
  return Math.min(vw / w, vh / h);
}

function clampPan(s, w, h) {
  const vw = stage.clientWidth || innerWidth;
  const vh = stage.clientHeight || innerHeight;
  const mx = Math.max(0, (w * s - vw) / 2);
  const my = Math.max(0, (h * s - vh) / 2);
  panX = Math.max(-mx, Math.min(mx, panX));
  panY = Math.max(-my, Math.min(my, panY));
}

function layoutView() {
  const [w, h] = mediaSize();
  const vw = stage.clientWidth || innerWidth;
  const vh = stage.clientHeight || innerHeight;
  if (!w || !h) return;
  if (viewMode === 'fit') { relZoom = 1; panX = 0; panY = 0; }
  const s = fitScale(w, h) * relZoom;
  clampPan(s, w, h);
  const tx = (vw - w * s) / 2 + panX;
  const ty = (vh - h * s) / 2 + panY;
  content.style.transform = `translate(${tx}px, ${ty}px) scale(${s})`;
  zoomEl.textContent = viewMode === 'fit' ? '适应' : `${Math.round(s * 100)}%`;
  fitBtn.textContent = viewMode === 'fit' ? '1:1' : '适应';
}

/** 以 (cx, cy) 为锚缩放。factor = 1.25 表示放大 25%。 */
function zoomAt(factor, cx, cy) {
  const [w, h] = mediaSize();
  if (!w || !h) return;
  const vw = stage.clientWidth || innerWidth;
  const vh = stage.clientHeight || innerHeight;
  const fs = fitScale(w, h);
  const s0 = fs * relZoom;
  const s1 = Math.max(fs, Math.min(fs * 8, s0 * factor));
  const tx0 = (vw - w * s0) / 2 + panX;
  const ty0 = (vh - h * s0) / 2 + panY;
  const px = (cx - tx0) / s0;
  const py = (cy - ty0) / s0;
  const tx1 = cx - px * s1;
  const ty1 = cy - py * s1;
  relZoom = s1 / fs;
  panX = tx1 - (vw - w * s1) / 2;
  panY = ty1 - (vh - h * s1) / 2;
  viewMode = relZoom > 1.001 ? 'manual' : 'fit';
  layoutView();
}

function goFit() {
  viewMode = 'fit';
  layoutView();
}

/** 1:1：一个屏幕像素对一个画面像素，桌面端读小字最清楚。 */
function goOneToOne() {
  const [w, h] = mediaSize();
  if (!w || !h) return;
  const fs = fitScale(w, h);
  viewMode = 'manual';
  relZoom = 1 / fs;
  panX = 0;
  panY = 0;
  layoutView();
}

function toggleZoomMode() {
  if (viewMode === 'fit') goOneToOne();
  else goFit();
}

function showChrome() {
  document.body.classList.remove('idle');
  clearTimeout(chromeTimer);
  chromeTimer = setTimeout(() => {
    if (viewState === 'live' && overlay.hidden) document.body.classList.add('idle');
  }, 3000);
}

function banner(text, hideAfterMs) {
  bannerEl.textContent = text;
  bannerEl.classList.add('on');
  clearTimeout(banner._timer);
  if (hideAfterMs) banner._timer = setTimeout(() => bannerEl.classList.remove('on'), hideAfterMs);
}

function hideBanner() {
  clearTimeout(banner._timer);
  bannerEl.classList.remove('on');
}

function toast(text, ms = 2800) {
  toastEl.textContent = text;
  toastEl.classList.add('on');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toastEl.classList.remove('on'), ms);
}

function updateStatus() {
  document.title = viewState === 'live' ? '▶ 屏幕共享' : '屏幕共享 · 观看';
  if (viewState === 'error') { statusEl.textContent = '连接失败'; return; }
  if (viewState === 'connecting') { statusEl.textContent = '正在连接…'; return; }
  if (viewState === 'waiting') { statusEl.textContent = '已连接，等待画面…'; return; }
  if (viewState === 'reconnecting') { statusEl.textContent = '连接中断，正在重连…'; return; }
  const plane = videoOn ? '视频' : '图块';
  const rate = fps > 0 ? `${fps} fps` : '静止';
  const [w, h] = mediaSize();
  statusEl.innerHTML = `<b>${w}×${h}</b> · ${rate} · ${plane}`;
}

/**
 * 界面状态机。overlay 只用于“还没画面/彻底失败”，重连期间保留最后一帧，
 * 用顶部细条提示，避免整屏黑一下。
 */
function setViewState(name, detail) {
  viewState = name;
  updateStatus();
  if (name === 'live') {
    overlay.hidden = true;
    hideBanner();
    showChrome();
    keepAwake();
    layoutView();
    return;
  }
  const hadFrame = state.w > 0;
  if (name === 'reconnecting' && hadFrame) {
    overlay.hidden = true;
    banner('连接中断，正在重连…');
  } else {
    overlay.hidden = false;
    hideBanner();
    ovSpin.hidden = !(name === 'connecting' || name === 'waiting');
    retryBtn.hidden = name !== 'error';
    if (name === 'connecting') {
      ovTitle.textContent = '正在连接分享端…';
      ovText.textContent = '正在获取房间令牌';
    } else if (name === 'waiting') {
      ovTitle.textContent = '已连接，等待画面…';
      ovText.textContent = '分享端可能还没开始共享，或正在准备第一帧';
    } else if (name === 'reconnecting') {
      ovTitle.textContent = '连接中断，正在重连…';
      ovText.textContent = detail || '网络恢复后会自动继续';
    } else {
      ovTitle.textContent = '连接失败';
      ovText.textContent = detail || '无法连接到分享端';
    }
  }
  clearTimeout(chromeTimer);
  document.body.classList.remove('idle');
}

function markLive() {
  if (viewState === 'live' || state.w === 0) return;
  setViewState('live');
  updateStatus();
}

async function toggleFullscreen() {
  try {
    if (document.fullscreenElement) await document.exitFullscreen();
    else await document.documentElement.requestFullscreen();
  } catch (e) {
    toast('这个浏览器不允许网页全屏，可以按 F11');
  }
}

async function keepAwake() {
  if (!('wakeLock' in navigator) || document.visibilityState !== 'visible') return;
  try {
    wakeLock = await navigator.wakeLock.request('screen');
  } catch (e) { /* 不支持就算了，桌面端不依赖它 */ }
}

function toggleSound() {
  if (!soundBtn || soundBtn.hidden) return;
  soundMuted = !soundMuted;
  if (soundEl) {
    soundEl.muted = soundMuted;
    if (soundMuted) soundEl.pause();
    else {
      const p = soundEl.play();
      if (p && p.catch) p.catch(() => {});
    }
  }
  paintSoundBtn();
  showChrome();
  // 声音是默认关闭的，切换后给一句明确反馈（尤其“开启”那一下，用户需要知道确实生效）。
  toast(soundMuted ? '系统声音已静音' : '系统声音已开启', 1400);
}

// 键盘：桌面端的主路径。
window.addEventListener('keydown', (e) => {
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  const k = e.key;
  if (k === 'f' || k === 'F') { e.preventDefault(); toggleFullscreen(); }
  else if (k === '0') { e.preventDefault(); goFit(); }
  else if (k === '1') { e.preventDefault(); goOneToOne(); }
  else if (k === '+' || k === '=') { e.preventDefault(); zoomAt(1.25, innerWidth / 2, innerHeight / 2); }
  else if (k === '-' || k === '_') { e.preventDefault(); zoomAt(0.8, innerWidth / 2, innerHeight / 2); }
  else if (k === 's' || k === 'S') { e.preventDefault(); toggleSound(); }
  else if (k === 'd' || k === 'D') { e.preventDefault(); document.body.classList.toggle('debug'); }
  else if (k === 'r' || k === 'R') { e.preventDefault(); location.reload(); }
});

// 滚轮缩放（桌面主路径）：锚在鼠标位置，滚轮不再滚动页面。
stage.addEventListener('wheel', (e) => {
  e.preventDefault();
  zoomAt(Math.exp(-e.deltaY * 0.0015), e.clientX, e.clientY);
  showChrome();
}, { passive: false });

stage.addEventListener('pointerdown', (e) => {
  if (e.pointerType === 'mouse' && e.button !== 0) return;
  try { stage.setPointerCapture(e.pointerId); } catch (err) { /* 合成事件/已释放 */ }
  pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (pointers.size === 1) {
    drag = { x: e.clientX, y: e.clientY, panX, panY, moved: false };
    pinch = null;
  } else if (pointers.size === 2) {
    const [a, b] = [...pointers.values()];
    drag = null;
    pinch = { d: Math.hypot(a.x - b.x, a.y - b.y) || 1 };
    showChrome();
  }
});

stage.addEventListener('pointermove', (e) => {
  if (!pointers.has(e.pointerId)) {
    // 鼠标只是移动（没按键）：唤出 chrome
    if (e.pointerType === 'mouse') showChrome();
    return;
  }
  pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (pointers.size >= 2 && pinch) {
    const [a, b] = [...pointers.values()];
    const d = Math.hypot(a.x - b.x, a.y - b.y) || 1;
    const mx = (a.x + b.x) / 2;
    const my = (a.y + b.y) / 2;
    zoomAt(d / pinch.d, mx, my);
    pinch.d = d;
    showChrome();
    return;
  }
  if (drag && pointers.size === 1) {
    const dx = e.clientX - drag.x;
    const dy = e.clientY - drag.y;
    if (Math.abs(dx) + Math.abs(dy) > 4) drag.moved = true;
    if (relZoom > 1.001) {
      panX = drag.panX + dx;
      panY = drag.panY + dy;
      layoutView();
      showChrome();
    }
  }
});

function endPointer(e) {
  const wasSingle = pointers.size === 1;
  pointers.delete(e.pointerId);
  if (pointers.size < 2) pinch = null;
  if (pointers.size === 0) {
    const tapped = !!drag && !drag.moved;
    drag = null;
    if (wasSingle && tapped) {
      const now = performance.now();
      if (now - lastTapAt < 300) {
        toggleZoomMode();
        lastTapAt = 0;
      } else {
        lastTapAt = now;
        // 单击只负责“唤出”，不主动隐藏：隐藏交给 3 s 自动计时。
        // 这样双击（唤出 + 切换到 1:1）不会先把 chrome 藏起来。
        if (viewState === 'live') showChrome();
      }
    }
  }
}
stage.addEventListener('pointerup', endPointer);
stage.addEventListener('pointercancel', endPointer);

window.addEventListener('resize', () => layoutView());
window.addEventListener('orientationchange', () => setTimeout(layoutView, 120));
// 编码器可能在中途改分辨率（视频平面 960x540 ↔ 1920x1080），<video> 的固有尺寸
// 变化不会触发 window resize，得自己跟。
video.addEventListener('loadedmetadata', () => { if (!video.hidden) layoutView(); });
video.addEventListener('resize', () => { if (!video.hidden) layoutView(); });
// 鼠标停在顶栏上时不要把它淡出（正在找按钮的时候消失最恼人）。
chromeEl.addEventListener('pointerenter', () => clearTimeout(chromeTimer));
chromeEl.addEventListener('pointerleave', () => { if (viewState === 'live') showChrome(); });
document.addEventListener('fullscreenchange', () => {
  fullBtn.textContent = document.fullscreenElement ? '退出全屏' : '全屏';
  setTimeout(layoutView, 60);
});
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && viewState === 'live') keepAwake();
});

fitBtn.addEventListener('click', (e) => { e.stopPropagation(); toggleZoomMode(); showChrome(); });
fullBtn.addEventListener('click', (e) => { e.stopPropagation(); toggleFullscreen(); showChrome(); });
debugBtn.addEventListener('click', (e) => {
  e.stopPropagation();
  document.body.classList.toggle('debug');
  showChrome();
});
retryBtn.addEventListener('click', () => location.reload());
soundBtn.addEventListener('click', (e) => { e.stopPropagation(); toggleSound(); });

updateStatus();

function paintSoundBtn() {
  if (!soundBtn) return;
  soundBtn.hidden = !aTrack;
  // 文案描述**当前状态**，而不是“点一下会怎样”：旧文案「声音 开」既可以读成
  // “声音已经开着”，也可以读成“点一下打开”，语义不清。现在用「声音：关/开」
  // 加图标，title 里再写清点击动作。
  soundBtn.textContent = soundMuted ? '🔇 声音：关' : '🔊 声音：开';
  soundBtn.title = soundMuted ? '系统声音当前关闭，点击开启' : '系统声音当前开启，点击静音';
  soundBtn.setAttribute('aria-pressed', soundMuted ? 'false' : 'true');
  soundBtn.setAttribute('aria-label', soundMuted ? '系统声音：关，点击开启' : '系统声音：开，点击静音');
}

async function loadWasm() {
  const url = 'ess_web.wasm?v=20261105e';
  let inst;
  try {
    inst = await WebAssembly.instantiateStreaming(fetch(url), {});
  } catch (e) {
    const buf = await (await fetch(url)).arrayBuffer();
    inst = await WebAssembly.instantiate(buf, {});
  }
  ex = inst.instance.exports;
  mem = ex.memory;
  // 浏览器可能留着上一版的 wasm（文件名固定），导出对不上时宁可立刻说清楚
  if (typeof ex.ess_viewer_dirty !== 'function') {
    throw new Error('ess_web.wasm 过旧：缺 ess_viewer_dirty，请重跑 scripts/build-web.sh');
  }
  viewer = ex.ess_viewer_new();
  feedCap = 1 << 17; // 单条数据消息上限 = MAX_MESSAGE_BYTES(60 KB) + 头部，留足余量
  feedPtr = ex.ess_alloc(feedCap);
  reportPtr = ex.ess_alloc(reportCap);
  diagPtr = ex.ess_alloc(128);
  dirtyPtr = ex.ess_alloc(16);
}

function feed(bytes) {
  if (bytes.length > feedCap) {
    lastError = `消息过大: ${bytes.length} B`;
    state.errors++;
    return;
  }
  new Uint8Array(mem.buffer, feedPtr, bytes.length).set(bytes);
  const t0 = performance.now();
  const r = ex.ess_viewer_feed(viewer, feedPtr, bytes.length);
  const dt = performance.now() - t0;
  state.feed_ms += dt;
  dutyWinFeed += dt;
  state.feed_n++;
  if (r !== 0) {
    state.msgs++;
    if (r === -2) {
      state.errors++;
      lastError = `解码失败 (${r})`;
    }
    return;
  }
  state.msgs++;
  // 平面切换是控制消息：到了就立刻反映到 DOM，不等下一帧
  syncPlane();
}

function ensureCanvas(w, h) {
  if (w === curW && h === curH && canvas.width === w && canvas.height === h) return false;
  curW = w; curH = h;
  canvas.width = w;
  canvas.height = h;
  img = ctx.createImageData(w, h);
  img32 = new Uint32Array(img.data.buffer);
  drawnSerial = 0;
  return true;
}

// 画布内存是 BGRA（原生管线内部格式），ImageData 要 RGBA：交换 R/B。
//
// 交换与上传只覆盖**脏矩形**：整幅 1080p 是按帧大小付钱，实测一次大面积更新
// 就把解码占用顶过 shed 闸门（44 %），后续更新被本机丢掉，表现成"很多个图块
// 慢慢覆盖"。变化面积才是这件事的真实代价。
function draw() {
  const w = ex.ess_viewer_width(viewer) >>> 0;
  const h = ex.ess_viewer_height(viewer) >>> 0;
  const serial = ex.ess_viewer_serial(viewer) >>> 0;
  state.w = w; state.h = h; state.serial = serial;
  if (w === 0 || h === 0) return;
  const resized = ensureCanvas(w, h);
  const has = ex.ess_viewer_dirty(viewer, dirtyPtr);
  const dv = new Uint32Array(mem.buffer, dirtyPtr, 4);
  if (!has && serial === drawnSerial) return;
  // 尺寸刚变时 img 里除脏区之外是空的，必须整幅上传
  let x = 0, y = 0, bw = w, bh = h;
  if (has && !resized) {
    x = dv[0]; y = dv[1]; bw = dv[2]; bh = dv[3];
  }
  const ptr = ex.ess_viewer_canvas(viewer) >>> 0;
  const x1 = x + bw, y1 = y + bh;
  if (ptr % 4 === 0) {
    const src = new Uint32Array(mem.buffer, ptr, w * h);
    for (let row = y; row < y1; row++) {
      const base = row * w;
      for (let col = x; col < x1; col++) {
        const i = base + col;
        const v = src[i];
        img32[i] = (v & 0xff00ff00) | ((v & 0xff) << 16) | ((v >>> 16) & 0xff);
      }
    }
  } else {
    const src = new Uint8Array(mem.buffer, ptr, w * h * 4);
    const dst = img.data;
    for (let row = y; row < y1; row++) {
      for (let col = x; col < x1; col++) {
        const o = (row * w + col) * 4;
        dst[o] = src[o + 2];
        dst[o + 1] = src[o + 1];
        dst[o + 2] = src[o];
        dst[o + 3] = src[o + 3];
      }
    }
  }
  // `putImageData` 的 dx/dy 是整幅 image 的落点，脏矩形只是"只写哪一块"：
  // 写成 `(img, x, y, x, y, …)` 会把源 (x,y) 画到 (2x,2y)，偏移块整块错位/裁掉。
  // 要让脏块原地更新，落点必须是 (0,0)。
  ctx.putImageData(img, 0, 0, x, y, bw, bh);
  drawnSerial = serial;
  frames++;
  // 第一帧真正上屏 = 可以从“等待画面”切到 live（布局也在这时才有正确尺寸）
  markLive();
}

function videoModeNow() {
  return ex.ess_viewer_video_mode ? (ex.ess_viewer_video_mode(viewer) >>> 0) === 1 : false;
}

// 图块画布与视频轨互斥显示。进入视频平面要等视频轨真的交出一帧新画面，
// 否则 <video> 上还是上一次运动的旧帧（甚至黑帧），会看成画面回退。
function applyPlane() {
  const showVideo = videoOn && vTrack !== null && videoArmed;
  video.hidden = !showVideo;
  canvas.hidden = showVideo;
  // 两种平面的固有尺寸可能不同：切换后重新按“适应”算一次
  layoutView();
  syncVideoSub();
}

// 从 participant 的 publication 表里找回视频轨对应的那一条：TrackSubscribed 给的是
// RemoteTrack，它身上没有回指 publication 的字段，只有 sid 能对上。
function findVideoPub(track) {
  if (!roomRef) return null;
  try {
    const parts = roomRef.remoteParticipants;
    const it = parts && parts.values ? parts.values() : [];
    for (const p of it) {
      const pubs = p && p.videoTrackPublications;
      if (!pubs || !pubs.values) continue;
      for (const pub of pubs.values()) {
        if (track && pub.trackSid === track.sid) return pub;
      }
    }
  } catch (e) {}
  return null;
}

// 按当前平面决定视频轨该不该订阅。迟滞 10 s：短暂进出平面（滚动、翻页）不该来回
// 摘订阅——每次复订都要等一次关键帧，那才是用户看得见的代价。
function syncVideoSub() {
  if (!videoPub) return;
  if (videoOn) {
    if (unsubTimer) { clearTimeout(unsubTimer); unsubTimer = null; }
    if (videoSubOff) {
      videoSubOff = false;
      try { videoPub.setSubscribed(true); } catch (e) { lastError = '恢复视频订阅失败: ' + e.message; }
    }
    return;
  }
  if (videoSubOff || unsubTimer) return;
  unsubTimer = setTimeout(() => {
    unsubTimer = null;
    if (videoOn || !videoPub) return;
    videoSubOff = true;
    try { videoPub.setSubscribed(false); } catch (e) { lastError = '退订视频轨失败: ' + e.message; }
  }, UNSUB_AFTER_MS);
}

function armVideo() {
  videoArmed = false;
  vArmedBase = videoFramesTotal();
  if (typeof video.requestVideoFrameCallback === 'function') {
    // 首选：新帧真正呈现的那一刻（比轮询快一个合成周期）
    try {
      video.requestVideoFrameCallback(() => {
        if (!videoOn) return;
        videoArmed = true;
        applyPlane();
      });
    } catch (e) {}
  }
}

function videoFramesTotal() {
  if (!video.getVideoPlaybackQuality) return 0;
  try {
    return video.getVideoPlaybackQuality().totalVideoFrames;
  } catch (e) {
    return 0;
  }
}

function syncPlane() {
  const mode = videoModeNow();
  if (mode !== videoOn) {
    videoOn = mode;
    if (mode) {
      armVideo();
    } else {
      // 退出方向不需要等：发送端保证「无损修复先到、off 后到」（可靠通道有序）
      videoArmed = true;
    }
    applyPlane();
    hud.textContent = hudText();
    updateStatus();
    return;
  }
  // requestVideoFrameCallback 不可用时的兜底：解码帧计数前进了就接管
  if (videoOn && !videoArmed && videoFramesTotal() > vArmedBase) {
    videoArmed = true;
    applyPlane();
  }
}

// 1 s 统计与平面同步都独立于 rAF：后台标签页的 rAF 会被暂停，若只靠它，
// 切回前台时平面状态是旧的（发送端早已切到视频轨）。
function tick() {
  const now = performance.now();
  // 序列空洞与"残帧"计数每个拍子都要刷新（早报要用），与 1 s 的 HUD 刷新分开。
  // 残帧 = 接收端凑不齐整帧而丢掉的那一帧：拆片共享同一个 seq，丢片在 seq 空洞上
  // 完全看不出来，但它是"图块残留"的直接证据，值得立刻补一份报告。
  const dn = ex.ess_viewer_diag(viewer, diagPtr, 32);
  if (dn > 17) {
    const dv = new Uint32Array(mem.buffer, diagPtr, dn);
    state.loss = dv[17];
    if (dn > 18) state.partsDropped = dv[18];
  }
  // 尺寸/代数在 rAF 被暂停的标签页里也要正确（draw 不会跑）
  state.w = ex.ess_viewer_width(viewer) >>> 0;
  state.h = ex.ess_viewer_height(viewer) >>> 0;
  state.serial = ex.ess_viewer_serial(viewer) >>> 0;
  if (viewState === 'waiting' && state.w > 0) markLive();
  if (now - lastFpsAt < 1000) return;
  const total = videoFramesTotal();
  // 视频平面时图块计数不动，帧率要从播放器的累计解码帧里取
  fps = videoOn && total > 0 ? total - vPrev : frames;
  frames = 0;
  vPrev = total;
  lastFpsAt = now;
  // 序列空洞（diag 末槽）：没按序到达的发送端消息数。口径要说清——本地 shed 闸门挡下的
  // 消息同样不会进 wasm，所以这个数含「链路丢」与「本机解码闸门丢」两部分；闸门关掉
  // （平时 duty 不高）时它就等于链路丢的量。与 丢=（只数本机闸门）分开看。
  pumpNet();   // 异步：采完自己刷 HUD，这里不 await（它比一次 1 s 节拍慢不了）
  hud.textContent = hudText();
  updateStatus();
}

function loop() {
  syncPlane();
  // 闸门信箱在每帧开头解开：一次只解最新**一帧**（同一 seq 的所有拆件），画布每帧
  // 都有新内容，队列深度恒 ≤1 帧。放在 draw 之前，这一帧的新内容就跟着本次上屏画出去。
  // 只解最后一条会把一帧的其余拆件丢掉——那正是"图块残留"的来源。
  if (shedMailbox) {
    const parts = shedMailbox.parts;
    shedMailbox = null;
    for (const p of parts) feed(p);
  }
  // t0 在信箱解开之后：feed 自己已经按毫秒计入 duty，从这里再包一份会把同一段
  // 工时数两遍——闸门开着时几乎每条更新都走信箱，于是 duty 被系统性抬高，闸门
  // 越来越关得死。
  const t0 = performance.now();
  draw();
  // 解码占用按 feed + 画布上传两段算：draw 把 ImageData 推给 canvas，1080p 下
  // 每帧也是几毫秒，它和 feed 抢的是同一条主线程。
  shedUpdate(0, performance.now() - t0);
  tick();
  requestAnimationFrame(loop);
}

async function main() {
  await loadWasm();
  loop();

  const cfgRes = await fetch('token');
  if (!cfgRes.ok) throw new Error(`获取房间令牌失败（HTTP ${cfgRes.status}）`);
  const cfg = await cfgRes.json();
  const room = new Room({ adaptiveStream: false, dynacast: false });
  roomRef = room;
  room.on(RoomEvent.DataReceived, (payload, kind) => {
    const bytes = payload instanceof Uint8Array ? payload : new Uint8Array(payload);
    // 速率按**到达**算，所以在闸门之前累计：被本机丢掉的过时内容同样吃掉了链路带宽，
    // 详情里的下行不该因为"我没来得及解"而显得很省。
    tilesRx += bytes.length;
    // 闸门开着时只处理不可靠通路的**最新一帧**：它们是"某一帧的局部"，过时就没有
    // 价值；可靠通路是画布正确性的依据，任何时候都得解。
    if (shedOn && kind !== RELIABLE) {
      shedPush(bytes);
      return;
    }
    feed(bytes);
  });
  room.on(RoomEvent.Disconnected, () => {
    connected = false;
    lastError = '连接已断开（分享端可能已停止）';
    hud.textContent = hudText();
    setViewState('error', lastError);
  });
  // SDK 会自己重连。重连成功必须把 connected 恢复：它一为 false，上行报告就永久
  // 停止，发送端再也补不回断线期间丢掉的块，画面会停在断线前的旧帧上。
  room.on(RoomEvent.Reconnecting, () => {
    lastError = '连接中断，正在重连…';
    hud.textContent = hudText();
    setViewState('reconnecting');
  });
  room.on(RoomEvent.Reconnected, () => {
    connected = true;
    lastError = '';
    // 恢复上行报告：断线期间丢的块靠它要回来。若这次重连让发送端看到了
    // 参与者重加入，它会主动补整帧，画布不需要在这里重建。
    hud.textContent = hudText();
    setViewState(state.w > 0 ? 'live' : 'waiting');
    if (state.w > 0) banner('已恢复连接', 1500);
  });
  // 视频平面：轨道一到就挂上，并告诉 WASM 可以不再解码 JPEG 兜底帧
  room.on(RoomEvent.TrackSubscribed, (track) => {
    if (track.kind === 'audio') {
      // 系统声音：独立 <audio>，默认就放（会议场景下"共享了声音"等于"要听得见"）。
      // 浏览器的自动播放策略可能先拦一下，靠下面的一次性手势放行。
      aTrack = track;
      if (!soundEl) {
        soundEl = document.createElement('audio');
        soundEl.autoplay = true;
        soundEl.playsInline = true;
        document.body.appendChild(soundEl);
      }
      soundEl.muted = soundMuted;
      try {
        if (typeof track.attach === 'function') track.attach(soundEl);
        else soundEl.srcObject = new MediaStream([track.mediaStreamTrack]);
        // **必须在 attach 之后重新静音**：LiveKit 的 attach 会执行
        // `element.muted = (音频轨数量 === 0)`，对音频轨就是 muted=false，把我们
        // 刚设的静音覆盖掉。观看端与分享端同机时这一步会立刻形成环回回声。
        soundEl.muted = soundMuted;
        // 静音时干脆不播放：即使将来某个路径把 muted 改回去，也不会突然出声。
        if (soundMuted) soundEl.pause();
        else {
          const p = soundEl.play();
          if (p && p.catch) p.catch(() => {});
        }
      } catch (e) {
        lastError = '声音轨挂载失败: ' + e.message;
      }
      paintSoundBtn();
      hud.textContent = hudText();
      // 默认静音是产品决定（同机外放会环回成回声），但用户得知道"有声音可开"。
      if (soundMuted && !soundHinted) {
        soundHinted = true;
        toast('有系统声音，点顶部「声音」开启');
      }
      return;
    }
    if (track.kind !== 'video') return;
    const fresh = track !== vTrack;
    vTrack = track;
    videoPub = findVideoPub(track) || videoPub;
    videoSubOff = false;
    try {
      if (typeof track.attach === 'function') {
        track.attach(video);
      } else {
        video.srcObject = new MediaStream([track.mediaStreamTrack]);
      }
      const p = video.play();
      if (p && p.catch) p.catch(() => {});
      ex.ess_viewer_video_ready(viewer, 1);
    } catch (e) {
      lastError = '视频轨挂载失败: ' + e.message;
    }
    // 重连后是一条全新的轨：<video> 里还没有画面，直接接管会闪一下黑屏
    if (fresh && videoOn) armVideo();
    applyPlane();
    hud.textContent = hudText();
  });
  room.on(RoomEvent.TrackUnsubscribed, (track) => {
    if (track === aTrack) {
      aTrack = null;
      try {
        if (soundEl && typeof track.detach === 'function') track.detach(soundEl);
      } catch (e) {}
      if (soundEl) soundEl.srcObject = null;
      paintSoundBtn();
      hud.textContent = hudText();
      return;
    }
    if (track !== vTrack) return;
    try {
      if (typeof track.detach === 'function') track.detach(video);
    } catch (e) {}
    vTrack = null;
    video.srcObject = null;
    ex.ess_viewer_video_ready(viewer, 0);
    applyPlane();
    hud.textContent = hudText();
  });
  await room.connect(cfg.url, cfg.token);
  connected = true;
  hud.textContent = hudText();
  // 连上了但还没收到首帧：说清楚在等什么，不要留一块黑屏
  if (state.w === 0) setViewState('waiting');

  // 浏览器自动播放策略：声音要等一次用户手势才允许出声。不加遮罩（对着只想看画面的人
  // 是打扰），改成任何点击/按键都顺手放行一次。
  const resumeSound = () => {
    try {
      if (typeof room.startAudio === 'function') room.startAudio();
    } catch (e) {}
    if (soundEl && !soundMuted) {
      const p = soundEl.play();
      if (p && p.catch) p.catch(() => {});
    }
  };
  window.addEventListener('pointerdown', resumeSound);
  window.addEventListener('keydown', resumeSound);

  // 上行状态报告：驱动发送端对丢失块的差异重发（§6.4 兜底）
  setInterval(() => {
    syncPlane();
    tick();
  }, 250);
  let gapsReported = 0;
  function sendReport() {
    if (!connected) return;
    // `now_ms` 在 Rust 侧是 u64 → wasm 的 i64 形参，JS 必须传 BigInt。
    // 这里一直传 Number，浏览器每次都在 `sendReport` 抛 TypeError——报告实际
    // 一条都没发出去（发送端因此永远收不到观看端的版本向量与补块请求）。
    const n = ex.ess_viewer_report(viewer, reportPtr, reportCap, BigInt(Date.now()));
    if (n > 0) {
      gapsReported = state.loss;
      state.reports++;
      const bytes = new Uint8Array(mem.buffer, reportPtr, n).slice();
      tilesTx += bytes.length;
      room.localParticipant.publishData(bytes, { reliable: true }).catch((e) => {
        lastError = '报告发送失败: ' + e.message;
      });
    }
  }
  setInterval(sendReport, 500);
  // 空洞早报：看见新的链路空洞就立刻补一份报告，不等下一个 500 ms 刻度。实测半屏照片类突变
  // 约四分之三当秒到齐，剩下那一截纯粹在等这份报告的往返；报告只是几 KB 的上行可靠消息，
  // 早发不动下行带宽，也不动新鲜度判定（发送端仍按报告的版本向量算差集）。
  //
  // `!shedOn` 是必需的，不是保险：被本机闸门挡下的更新根本没进 wasm，所以它连自己的 seq 都没
  // 被记过，后来者一比对就成了"洞"。闸门开着时去追这些洞，等于把新鲜度判定刚丢掉的内容又要
  // 回来——那正是补账活锁的入口。
  setInterval(() => {
    if (!shedOn && state.loss > gapsReported) sendReport();
  }, 250);
  // 残帧早报：拆片共享同一个 `seq`，丢片在序列空洞上完全看不出来；只有接收端知道
  // 自己凑不齐整帧（`partsDropped` 递增）。动画/大面积变化期间，这份报告能把"图块
  // 残留"的等待从 ≤500 ms 压到一个往返。同样受 `!shedOn` 约束——闸门开着时丢的是
  // 本机主动丢的过时内容，去追它只会和新鲜度判定打架。
  let partsDroppedReported = 0;
  setInterval(() => {
    if (shedOn) return;
    if (state.partsDropped > partsDroppedReported) {
      partsDroppedReported = state.partsDropped;
      sendReport();
    }
  }, 250);

  // 验收钩子：像素哈希 / 采样，供自动化比对
  window.__ess = {
    state,
    // 延迟归因用（#60）：探针需要 room→engine→client 的真实字段名才能拿到 pc
    room,
    /** 界面读数（验收用）：状态机、缩放、chrome 是否自动隐藏。 */
    ui: () => ({
      state: viewState,
      zoom: relZoom,
      mode: viewMode,
      pan: [panX, panY],
      chromeIdle: document.body.classList.contains('idle'),
      fullscreen: !!document.fullscreenElement,
    }),
    /** 验收用：直接切界面状态，验证 overlay/toast 的文案与按钮。 */
    setUiState: (s, d) => setViewState(s, d),
    sample(x, y) {
      if (!curW || !curH) return null;
      const d = ctx.getImageData(x, y, 1, 1).data;
      return [d[0], d[1], d[2], d[3]];
    },
    hash() {
      if (!curW || !curH) return 0;
      const d = ctx.getImageData(0, 0, curW, curH).data;
      let h = 0x811c9dc5 >>> 0;
      for (let i = 0; i < d.length; i += 4) {
        h ^= d[i] | (d[i + 1] << 8) | (d[i + 2] << 16);
        h = Math.imul(h, 0x01000193) >>> 0;
      }
      return h;
    },
    errors: () => state.errors,
    plane: () => (videoOn ? (vTrack ? 'video' : 'video-pending') : 'tiles'),
    // 视频轨的订阅状态：`off` = 已主动退订（图块平面挂满 10 s），`timer` = 正在等迟滞
    sub: () => ({ pub: !!videoPub, off: videoSubOff, timer: !!unsubTimer }),
    // 新鲜度闸门的读数与钉住开关（自动化验收用）
    shed: () => ({
      on: shedOn,
      pinned: shedPinned,
      duty: state.duty,
      dropped: state.dropped,
      feed_ms: state.feed_ms,
      feed_n: state.feed_n,
    }),
    setShed: (v) => {
      shedPinned = true;
      shedOn = !!v;
      state.dropped = 0;
    },
    clearShed: () => {
      shedPinned = false;
    },
    videoFrames: videoFramesTotal,
    // 传输速率的读数与累计量（自动化验收用）：bps 是平滑后的显示值，
    // rx/tx 是数据通道的累计字节，media 是 RTP 的累计字节。
    net: () => ({
      down: net.down, up: net.up, tiles: net.tiles, media: net.media,
      rx: tilesRx, tx: tilesTx, pc: peerConns.length,
    }),
    // 明细：验收时要能核对"媒体"这一路统计的是哪条 RTP、多少分辨率
    netPeers: () => peerConns.slice(),
    diag() {
      if (!diagPtr) return [];
      const n = ex.ess_viewer_diag(viewer, diagPtr, 32);
      return Array.from(new Uint32Array(mem.buffer, diagPtr, n));
    },
  };
}

main().catch((e) => {
  lastError = '启动失败: ' + (e && e.message ? e.message : e);
  hud.textContent = hudText();
  setViewState('error', lastError);
  console.error(e);
});
