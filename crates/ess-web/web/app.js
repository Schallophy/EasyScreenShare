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
let aTrack = null;      // 已订阅的系统声音轨
let soundEl = null;     // 声音挂在独立的 <audio> 上（video 元素是 muted 的，为的是绕开自动播放限制）
let soundMuted = true; // 默认静音：观看端与分享端同机时外放会被环回再抓一遍；用户也要求默认不开声音。要听就点"声音 开"
const state = { msgs: 0, errors: 0, w: 0, h: 0, serial: 0, feed_ms: 0, feed_n: 0, dropped: 0, loss: 0, duty: 0 };

// 观看端新鲜度闸门（手机上最关键的一条）。RTC 数据通道在浏览器侧没有反压：解码跟不上
// 到达速率时，消息就堆在事件循环里按序播放——表现正是"时间轴一直漂移、观看端不追最新
// 内容、延迟越来越大"。手机/wasm 的解码能力只有桌面的几分之一，所以按**实测解码占用**
// 决定丢不弃：占用超上限就丢掉不可靠通路的包（逐块更新、JPEG 运动帧），可靠通路
// （整帧基线、滚动宣告、精确修复、状态报告）一条不丢。丢了不漏画：每 500 ms 的状态
// 报告会让发送端按**当前**画面把缺口补回来。
//
// 闸门开着时**限流到"每帧一条最新的"**，不是整段丢弃。全丢在真机上的表现是画布整秒
// 冻结、然后成串补上（1920x1080 + 8× 降频实测逐秒代数 0/11 交替、2 个整秒冻屏），
// 那正是用户报的"不追最新内容"；信箱只留最新一条则把"停走"变成"降帧"，队列深度恒
// ≤1，积压不可能再长起来。
let shedOn = false;
let shedMailbox = null;
// 自动化验收用的手动钉住：真机手机上"解码跟不上"要等得到，测试里直接钉住开/关，
// 量的是闸门本身的行为（丢不可靠、留可靠、画布仍然收敛）。
let shedPinned = false;
const SHED_ON = 0.45, SHED_OFF = 0.30;
let dutyWinStart = performance.now(), dutyWinFeed = 0, dutyWinDraw = 0;
const RELIABLE = (LivekitClient.DataPacket_Kind && LivekitClient.DataPacket_Kind.RELIABLE) || 0;

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

function hudText() {
  const s = connected ? '已连接' : '连接中…';
  const rate = fps > 0 ? `${fps} fps` : '静止';
  const plane = videoOn ? (vTrack ? '视频轨' : '视频(等待轨道)') : '图块';
  const snd = aTrack ? (soundMuted ? '声音:关' : '声音:开') : '声音:无';
  return `${s}  ${state.w}x${state.h}  ${plane}  ${rate}  ${snd}  msgs=${state.msgs}` +
    (state.dropped ? ` 丢=${state.dropped}` : '') +
    (state.loss ? ` 漏=${state.loss}` : '') +
    (shedOn ? ` 解码${Math.round(state.duty * 100)}%` : '') +
    (lastError ? `\n${lastError}` : '');
}

function paintSoundBtn() {
  if (!soundBtn) return;
  soundBtn.hidden = !aTrack;
  soundBtn.textContent = soundMuted ? '声音 开' : '声音 关';
}

async function loadWasm() {
  const url = 'ess_web.wasm';
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
  if (now - lastFpsAt < 1000) return;
  const total = videoFramesTotal();
  // 视频平面时图块计数不动，帧率要从播放器的累计解码帧里取
  fps = videoOn && total > 0 ? total - vPrev : frames;
  frames = 0;
  vPrev = total;
  lastFpsAt = now;
  // 分辨率/代数在 rAF 被暂停的标签页里也要显示正确（draw 不会跑）
  state.w = ex.ess_viewer_width(viewer) >>> 0;
  state.h = ex.ess_viewer_height(viewer) >>> 0;
  state.serial = ex.ess_viewer_serial(viewer) >>> 0;
  // 序列空洞（diag 末槽）：没按序到达的发送端消息数。口径要说清——本地 shed 闸门挡下的
  // 消息同样不会进 wasm，所以这个数含「链路丢」与「本机解码闸门丢」两部分；闸门关掉
  // （平时 duty 不高）时它就等于链路丢的量。与 丢=（只数本机闸门）分开看。
  const dn = ex.ess_viewer_diag(viewer, diagPtr, 32);
  if (dn > 17) state.loss = new Uint32Array(mem.buffer, diagPtr, dn)[17];
  hud.textContent = hudText();
}

function loop() {
  syncPlane();
  // 闸门信箱在每帧开头解开：一次只解一条最新的，画布每帧都有新内容，队列深度恒 ≤1。
  // 放在 draw 之前，这一条最新更新就跟着本次上屏画出去。
  if (shedMailbox) {
    const pending = shedMailbox;
    shedMailbox = null;
    feed(pending);
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

  const cfg = await (await fetch('token')).json();
  const room = new Room({ adaptiveStream: false, dynacast: false });
  room.on(RoomEvent.DataReceived, (payload, kind) => {
    const bytes = payload instanceof Uint8Array ? payload : new Uint8Array(payload);
    // 闸门开着时只处理不可靠通路的**最新一条**：它们是"某一帧的局部"，过时就没有价值；
    // 可靠通路是画布正确性的依据，任何时候都得解。
    if (shedOn && kind !== RELIABLE) {
      // 信箱里那条被这条取代——丢的是过时的，留的是最新的
      if (shedMailbox) state.dropped++;
      shedMailbox = bytes;
      return;
    }
    feed(bytes);
  });
  room.on(RoomEvent.Disconnected, () => {
    connected = false;
    lastError = '连接已断开（刷新页面重连）';
    hud.textContent = hudText();
  });
  // SDK 会自己重连。重连成功必须把 connected 恢复：它一为 false，上行报告就永久
  // 停止，发送端再也补不回断线期间丢掉的块，画面会停在断线前的旧帧上。
  room.on(RoomEvent.Reconnecting, () => {
    lastError = '连接中断，正在重连…';
    hud.textContent = hudText();
  });
  room.on(RoomEvent.Reconnected, () => {
    connected = true;
    lastError = '';
    // 恢复上行报告：断线期间丢的块靠它要回来。若这次重连让发送端看到了
    // 参与者重加入，它会主动补整帧，画布不需要在这里重建。
    hud.textContent = hudText();
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
        const p = soundEl.play();
        if (p && p.catch) p.catch(() => {});
      } catch (e) {
        lastError = '声音轨挂载失败: ' + e.message;
      }
      paintSoundBtn();
      hud.textContent = hudText();
      return;
    }
    if (track.kind !== 'video') return;
    const fresh = track !== vTrack;
    vTrack = track;
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
  if (soundBtn) {
    soundBtn.addEventListener('click', () => {
      soundMuted = !soundMuted;
      if (soundEl) soundEl.muted = soundMuted;
      paintSoundBtn();
      hud.textContent = hudText();
    });
  }

  // 上行状态报告：驱动发送端对丢失块的差异重发（§6.4 兜底）
  setInterval(() => {
    syncPlane();
    tick();
  }, 250);
  let gapsReported = 0;
  function sendReport() {
    if (!connected) return;
    const n = ex.ess_viewer_report(viewer, reportPtr, reportCap, Date.now());
    if (n > 0) {
      gapsReported = state.loss;
      const bytes = new Uint8Array(mem.buffer, reportPtr, n).slice();
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

  // 验收钩子：像素哈希 / 采样，供自动化比对
  window.__ess = {
    state,
    // 延迟归因用（#60）：探针需要 room→engine→client 的真实字段名才能拿到 pc
    room,
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
  console.error(e);
});
