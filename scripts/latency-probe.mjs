// 延迟测量台：通过 CDP 把一个探针注入到「可见」的观看窗口里，测量
// 墙钟对齐白闪从注入到呈现的耗时（同机 → Date.now() 是同一时钟）。
//
// 用法：node scripts/latency-probe.mjs [--url http://127.0.0.1:8088/] [--seconds 25]
// 前置：观看窗口以 --app=<url> --remote-debugging-port=9223 启动，
//       共享端以 ESS_DEMO_FLASH=1 运行（合成屏每 2 s 前 100 ms 输出纯白帧）。
const arg = (n, d) => {
  const i = process.argv.indexOf(n);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : d;
};
const PORT = arg('--cdp', '9223');
const SECONDS = Number(arg('--seconds', 25));
const URL_MATCH = arg('--match', '127.0.0.1:8088');
const MODE = arg('--mode', 'video');
const BRIGHT = Number(arg('--bright', '0.8'));   // video = 呈现层（需窗口可见）；canvas = 数据通路（后台标签页也能测）

// canvas 模式：合成屏静止（ESS_DEMO_ANIM=0）+ 墙钟白闪，画布代数只在白闪帧
// 到达时变化。用 MessageChannel 自调度循环采样——隐藏标签页里 setTimeout 会被
// 钳到 1 s，而 port 消息不受节流影响。
const CANVAS_PROBE = (secs) => `(async () => {
  const diagAt = () => { const d = window.__ess.diag(); return { s: d[4], t: Date.now() }; };
  let prev = diagAt();
  const edges = [];
  const ch = new MessageChannel();
  const tick = () => {
    const now = diagAt();
    if (now.s !== prev.s) {
      const phase = now.t % 2000;
      if (phase < 900) edges.push(phase);   // 只认白闪窗口内的上升沿
    }
    prev = now;
    ch.port2.postMessage(0);
  };
  ch.port1.onmessage = tick;
  ch.port2.postMessage(0);
  await new Promise((r) => setTimeout(r, ${secs} * 1000));
  edges.sort((a, b) => a - b);
  return JSON.stringify({
    mode: 'canvas', n: edges.length,
    min: edges[0], p50: edges[Math.floor(edges.length / 2)],
    max: edges[edges.length - 1], edges,
    plane: window.__ess.plane(), hud: document.getElementById('hud').textContent,
  });
})()`;

const PROBE = (secs) => `(async () => {
  const BRIGHT = ${BRIGHT};
  const v = document.getElementById('stream');
  if (!v) return JSON.stringify({ error: 'no video element' });
  const off = document.createElement('canvas');
  off.width = 64; off.height = 36;
  const octx = off.getContext('2d', { willReadFrequently: true });
  const edges = [];
  let inFlash = false, frames = 0, dup = 0, lastHash = -1;
  // §14.7.17：那 ~290 ms 落在编码器**之后**，发送端遥测（qdelay/q/stale）看不见它。
  // 这里从页面里拿 inbound-rtp 的累计量，用首末差算：到的帧数 / 解出来的帧数 /
  // 抖动缓冲平均滞留，三个数就能把"送出排队—网络—接收缓冲—合成"分开。
  const pcOf = async () => {
    // 路径来自页面自己交出的字段名（§14.7.17 的 shape 转储）：观看端的视频走
    // subscriber 这条 pc，room.engine.subscriberPrimary.pc 才是我们要的
    // RTCPeerConnection。包 RTCPeerConnection 构造器那条路已否（wrap:false，
    // 注入没接到），不再保留。注意这段在一个反引号模板里，注释里不能再出现反引号。
    const r = window.__ess && window.__ess.room;
    const list = [];
    try {
      for (const t of [r?.engine?.subscriberPrimary, r?.engine?.publisher]) {
        if (t && typeof t.getStats === 'function') list.push(t);
        if (t?.pc && typeof t.pc.getStats === 'function') list.push(t.pc);
      }
      for (const p of (window.__pcs || [])) if (p && typeof p.getStats === 'function') list.push(p);
    } catch (e) { return null; }
    for (const c of list) {
      const pc = typeof c.getStats === 'function' ? c : null;
      if (!pc) continue;
      try {
        const rep = await pc.getStats();
        let ok = false;
        if (rep && typeof rep.forEach === 'function') rep.forEach((s) => { if (s && s.type === 'inbound-rtp') ok = true; });
        if (ok) return pc;
      } catch (e) {}
    }
    return null;
  };
  const pc = await pcOf();
  const snap = async () => {
    if (!pc) return null;
    const rep = await pc.getStats();
    let inb = null, cand = null;
    rep.forEach((s) => {
      if (s.type === 'inbound-rtp' && s.kind === 'video') inb = s;
      if (s.type === 'candidate-pair' && s.state === 'succeeded' && !cand) cand = s;
    });
    if (!inb) return null;
    return {
      recv: inb.framesReceived, dec: inb.framesDecoded, drop: inb.framesDropped,
      jb: inb.jitterBufferDelay || 0, jbn: inb.jitterBufferEmittedCount || 0,
      asm: inb.totalAssemblyTime || 0, asmN: inb.framesAssembledFromMultiplePackets || 0,
      qp: inb.qpSum || 0, bytes: inb.bytesReceived,
      rtt: cand && cand.currentRoundTripTime ? cand.currentRoundTripTime * 1000 : null,
      nack: inb.nackCount || 0,
    };
  };
  let s0 = await snap();
  let capN = 0, capSum = 0, capMin = 1e9, capMax = -1e9, capKeys = ''
  let recvN = 0, recvSum = 0, recvMin = 1e9, recvMax = -1e9;
  let presN = 0, presSum = 0;
  let dispLag = 0, dispN = 0, procSum = 0;
  const step = (ts, md) => {
    frames++;
    if (md) {
      if (!capKeys) capKeys = Object.keys(md).join(',');
      // 没有 captureTime，但有 receiveTime（这一帧最后一个包到达观看端的时刻）。
      // ts - receiveTime 就是"帧到了观看端、到被算作呈现"滞留了多久——接收缓冲 +
      // 解码 + 合成全在里面，且完全用页面标准 API，不需要碰 SDK 私有结构（#60）。
      if (typeof md.receiveTime === 'number' && md.receiveTime > 0) {
        const age = ts - md.receiveTime;
        if (age > -50 && age < 5000) {
          recvN++; recvSum += age; if (age < recvMin) recvMin = age; if (age > recvMax) recvMax = age;
        }
        if (typeof md.presentationTime === 'number' && md.presentationTime > md.receiveTime) {
          presN++; presSum += md.presentationTime - md.receiveTime;
        }
      }
      if (typeof md.expectedDisplayTime === 'number') { dispLag += md.expectedDisplayTime - ts; dispN++; }
      if (typeof md.processingDuration === 'number') procSum += md.processingDuration;
    }
    try {
      octx.drawImage(v, 0, 0, 64, 36);
      const d = octx.getImageData(0, 0, 64, 36).data;
      let bright = 0;
      for (let i = 0; i < d.length; i += 4) {
        if (d[i] > 248 && d[i + 1] > 248 && d[i + 2] > 248) bright++;
      }
      const isFlash = bright > (d.length / 4) * BRIGHT;
      if (isFlash && !inFlash) edges.push(Date.now() % 2000);
      inFlash = isFlash;
      // 上屏帧里有几帧是**重复内容**？这区分"帧按序到但晚"与"画面停在旧内容"：
      // 前者 dup 低、后者 dup 高（发送端 vsent 满帧而观看端内容不动）。
      let hsh = 0;
      for (let i = 0; i < d.length; i += 16) hsh = (hsh * 31 + d[i]) >>> 0;
      if (hsh === lastHash) dup++; else lastHash = hsh;
    } catch (e) {}
    v.requestVideoFrameCallback(step);
  };
  v.requestVideoFrameCallback(step);
  await new Promise((r) => setTimeout(r, ${secs} * 1000));
  const s1 = await snap();
  const dl = (a, b) => (a && b ? b - a : null);
  const rtc = !pc ? {
    pc: 'notfound', npcs: (window.__pcs || []).length,
    wrap: typeof RTCPeerConnection === 'function' && !!RTCPeerConnection.__essWrap,
    // 只读 ownKeys，不调 getter：拿到 room→engine→client 的真实字段名，
    // 下一版直接把 pc 的路径写死，不再靠包构造器（这版 npcs=0，包装没接到）。
    shape: (() => {
      const r = window.__ess && window.__ess.room;
      if (!r) return 'no-room-hook';
      const k = (o) => { try { return Reflect.ownKeys(o).filter((x) => typeof x === 'string').slice(0, 26).join(','); } catch (e) { return 'x'; } };
      const e = r.engine;
      const sp = e && e.subscriberPrimary;
      return 'room{' + k(r) + '}|engine{' + (e ? k(e) : '-') + '}|sp{' + (sp ? k(sp) : '-') +
        '}|spProto{' + (sp ? k(Object.getPrototypeOf(sp)) : '-') + '}';
    })(),
  } : !s1 ? { pc: 'no-inbound' } : {
    recv: dl(s0 && s0.recv, s1.recv), dec: dl(s0 && s0.dec, s1.dec),
    drop: dl(s0 && s0.drop, s1.drop),
    jb_ms: (s0 && s1 && s1.jbn > (s0.jbn || 0))
      ? +(((s1.jb - s0.jb) / (s1.jbn - s0.jbn)) * 1000).toFixed(1) : null,
    asm_ms: (s0 && s1 && s1.asmN > (s0.asmN || 0))
      ? +(((s1.asm - s0.asm) / (s1.asmN - s0.asmN))).toFixed(1) : null,
    mbps: s0 && s1 ? +(((s1.bytes - s0.bytes) * 8) / (${secs} * 1000000)).toFixed(2) : null,
    rtt: s1.rtt, nack: dl(s0 && s0.nack, s1.nack),
  };
  edges.sort((a, b) => a - b);
  const q = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : {};
  return JSON.stringify({
    frames, n: edges.length, dup,
    min: edges[0], p50: edges[Math.floor(edges.length / 2)],
    max: edges[edges.length - 1], edges,
    playback: { total: q.totalVideoFrames, dropped: q.droppedVideoFrames },
    // 解码尺寸 = 编码器实际交出的分辨率：MaintainFramerate 的降级方向看得见的地方
    dims: [v.videoWidth, v.videoHeight],
    vis: document.visibilityState,
    cap: {
      keys: capKeys,
      recv: recvN ? +(recvSum / recvN).toFixed(1) : null,
      recvMin: recvN ? +recvMin.toFixed(1) : null, recvMax: recvN ? +recvMax.toFixed(1) : null,
      recvN, pres: presN ? +(presSum / presN).toFixed(1) : null,
      dispLag: dispN ? +(dispLag / dispN).toFixed(1) : null,
      proc: frames ? +(procSum / frames).toFixed(2) : null,
    },
    rtc,
    plane: window.__ess ? window.__ess.plane() : null,
    hud: document.getElementById('hud').textContent,
  });
})()`;

async function main() {
  const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
  const page = list.find((t) => t.type === 'page' && t.url.includes(URL_MATCH));
  if (!page) {
    console.error(`找不到观看页（${URL_MATCH}）。当前页面：`, list.map((t) => t.url));
    process.exit(1);
  }
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  const pending = new Map();
  let seq = 0;
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    const w = pending.get(m.id);
    if (w) { pending.delete(m.id); clearTimeout(w.t); w.res(m); }
  };
  const call = (method, params, timeoutMs = 1000) =>
    new Promise((res, rej) => {
      const id = ++seq;
      pending.set(id, { res, rej, t: setTimeout(() => { pending.delete(id); rej(new Error(`${method} 超时`)); }, timeoutMs) });
      ws.send(JSON.stringify({ id, method, params }));
    });

  await call('Runtime.enable', {});
  if (arg('--reload', '1') === '1') {
    // 换过一次共享端就要重载：旧视频轨的订阅者状态是脏的
    await call('Page.navigate', { url: page.url });
    await new Promise((r) => setTimeout(r, 5000));
  }
  const expr = (MODE === 'canvas' ? CANVAS_PROBE : PROBE)(SECONDS);
  if (MODE !== 'canvas') {
    // 呈现层测量必须在可见窗口下做；遮挡时 Chrome 停止呈现，rVFC 不触发
    await call('Page.bringToFront', {});
    await new Promise((r) => setTimeout(r, 1200));
  }
  const rsp = await call(
    'Runtime.evaluate',
    { expression: expr, returnByValue: true, awaitPromise: true },
    (SECONDS + 15) * 1000,
  );
  if (rsp.error) { console.error('evaluate 失败', rsp.error); process.exit(1); }
  const r = rsp.result;
  if (r.exceptionDetails) { console.error('页面异常', r.exceptionDetails); process.exit(1); }
  console.log(r.result.value);
  ws.close();
}
main().catch((e) => { console.error(e); process.exit(1); });
