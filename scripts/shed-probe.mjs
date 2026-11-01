// 浏览器观看端新鲜度闸门的探针：通过 CDP 在观看页面里采样 `window.__ess` 的读数，
// 并在中途把闸门钉住打开一段时间，看它到底丢了什么、画布还画不画。
//
// 前置：观看页以 --app=http://127.0.0.1:8088/ --remote-debugging-port=9223 启动
// （复用 target/edge-viewer 这个固定 profile，绝不新建可同步的 profile）。
//
// 用法：node scripts/shed-probe.mjs [--cdp 9223] [--seconds 26] [--pin-after 8] [--pin-secs 10]
const arg = (n, d) => {
  const i = process.argv.indexOf(n);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : d;
};
const PORT = arg('--cdp', '9223');
const SECONDS = Number(arg('--seconds', 26));
const PIN_AFTER = Number(arg('--pin-after', 8));
const PIN_SECS = Number(arg('--pin-secs', 10));
const URL_MATCH = arg('--match', '127.0.0.1:8088');
// CPU 降频倍率：把这台桌面浏览器的解码能力压到手机量级，让闸门**自己**决定要不要开。
// 钉住（setShed）只能证明开关接了线，证明不了反馈环稳不稳——占用高→丢包→占用回落→
// 恢复绘制。这条环路只有在自动模式下才量得到，而"降帧但不冻屏"才是要交付的行为。
const THROTTLE = Number(arg('--throttle', 1));

const pages = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
const page = pages.find((p) => p.webSocketDebuggerUrl && p.url.includes(URL_MATCH));
if (!page) {
  console.log(JSON.stringify({ err: `没找到观看页（匹配 ${URL_MATCH}）`, pages: pages.map((p) => p.url) }));
  process.exit(1);
}

const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0;
const pending = new Map();
const send = (method, params) =>
  new Promise((res, rej) => {
    const mid = ++id;
    pending.set(mid, { res, rej });
    ws.send(JSON.stringify({ id: mid, method, params }));
  });
ws.addEventListener('message', (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) {
    const { res, rej } = pending.get(m.id);
    pending.delete(m.id);
    m.error ? rej(new Error(m.error.message)) : res(m.result);
  }
});
await new Promise((res, rej) => {
  ws.addEventListener('open', res);
  ws.addEventListener('error', rej);
});

if (THROTTLE > 1) {
  await send('Emulation.setCPUThrottlingRate', { rate: THROTTLE });
}
// 必须把页面带到前台并**持续**保持：隐藏标签页里 rAF 被暂停，draw/tick 不跑，占用采样
// 恒为 0、画布代数冻结；更糟的是 feed 耗时会在恢复可见的那一瞬间被算成"占用爆表"，
// 闸门因此丢掉一批本来该画的帧——整组读数会变成假的。单次 bringToFront 会被别的窗口
// 抢回去，所以每秒补一次。
await send('Page.bringToFront', {});
const focusTimer = setInterval(() => {
  send('Page.bringToFront', {}).catch(() => {});
}, 1000);

const evalIn = async (expr) => {
  const r = await send('Runtime.evaluate', {
    expression: expr,
    awaitPromise: true,
    returnByValue: true,
  });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || '页内异常');
  return r.result.value;
};

// 页内一次跑完：等待就绪 → 每秒采样 → 中途钉住开闸门 → 解除 → 汇总。
const out = await evalIn(`(async () => {
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  for (let i = 0; i < 150; i++) {
    if (window.__ess && window.__ess.state.msgs > 0) break;
    await wait(100);
  }
  if (!window.__ess) return JSON.stringify({ err: '__ess 不存在（wasm 没加载？）' });
  const snap = () => {
    const s = window.__ess.shed();
    return {
      t: Math.round(performance.now() / 1000),
      msgs: window.__ess.state.msgs,
      serial: window.__ess.state.serial,
      w: window.__ess.state.w,
      h: window.__ess.state.h,
      // 可见性自证：rAF 在隐藏标签页里被暂停，draw/tick 不跑 → duty 恒 0、画布代数冻结，
      // 整臂读数全是假的（实测把"窗口没在前台"读成"8 个整秒冻屏 + 闸门没触发"）。
      hidden: document.hidden ? 1 : 0,
      dropped: s.dropped,
      duty: Number((s.duty || 0).toFixed(3)),
      feed_ms: Math.round(s.feed_ms),
      feed_n: s.feed_n,
      on: s.on ? 1 : 0,
      pinned: s.pinned ? 1 : 0,
      plane: window.__ess.plane(),
      err: window.__ess.errors(),
    };
  };
  const t0 = Math.round(performance.now() / 1000);
  const rows = [];
  let pinnedAt = -1, unpinnedAt = -1;
  while (true) {
    rows.push(snap());
    const now = Math.round(performance.now() / 1000) - t0;
    if (${PIN_AFTER} >= 0 && pinnedAt < 0 && now >= ${PIN_AFTER}) { window.__ess.setShed(true); pinnedAt = now; }
    if (${PIN_AFTER} >= 0 && pinnedAt >= 0 && unpinnedAt < 0 && now >= pinnedAt + ${PIN_SECS}) { window.__ess.clearShed(); unpinnedAt = now; }
    if (now >= ${SECONDS}) break;
    await wait(1000);
  }
  const at = (i) => rows[Math.min(i, rows.length - 1)];
  // 环路是否"降帧但不冻屏"：逐秒画布代数增量的最小值（0 = 那一秒完全没更新）。
  const sd = [];
  for (let i = 1; i < rows.length; i++) sd.push(rows[i].serial - rows[i - 1].serial);
  sd.sort((a, b) => a - b);
  const pre = at(0), pinStart = at(pinnedAt < 0 ? rows.length - 1 : pinnedAt), pinEnd = at(unpinnedAt < 0 ? rows.length - 1 : unpinnedAt), last = at(rows.length - 1);
  return JSON.stringify({
    rows, pinnedAt, unpinnedAt,
    throttle: ${THROTTLE},
    hidden_secs: rows.filter((r) => r.hidden === 1).length,
    on_n: rows.filter((r) => r.on === 1).length,
    serial_ps_min: sd.length ? sd[0] : -1,
    serial_ps_med: sd.length ? sd[Math.floor(sd.length / 2)] : -1,
    serial_ps_max: sd.length ? sd[sd.length - 1] : -1,
    stall_secs: sd.filter((v) => v <= 0).length,
    drop_total: last.dropped,
    // 媒体平面接管的秒数（plane != tiles）与占用首尾读数：观看端跟不上时，正确的
    // 处置是换到硬件解码的视频轨，把浏览器占用压下来——这两个数就是那条链路的证据。
    video_n: rows.filter((r) => r.plane !== 'tiles').length,
    duty_first: rows.length ? rows[0].duty : -1,
    duty_last: last.duty,
    recv_total: last.msgs,
    duty_max: Math.max(...rows.map((r) => r.duty)),
    // "自动闸门是否自己开过"：只看钉住之前的窗口；不钉住（pin-after<0）时看整段。
    auto_on_before: rows.slice(0, pinnedAt < 0 ? rows.length : pinnedAt).some((r) => r.on === 1),
    drop_pre: pinStart.dropped - pre.dropped,
    drop_pin: pinEnd.dropped - pinStart.dropped,
    drop_post: last.dropped - pinEnd.dropped,
    msgs_pre: pinStart.msgs - pre.msgs,
    msgs_pin: pinEnd.msgs - pinStart.msgs,
    serial_pin: pinEnd.serial - pinStart.serial,
    serial_post: last.serial - pinEnd.serial,
    feed_per_msg: last.feed_n > 0 ? Math.round(last.feed_ms / last.feed_n) : 0,
    dims: last.w + 'x' + last.h, plane: last.plane, errors: last.err,
  });
})()`);

console.log(out);
clearInterval(focusTimer);
ws.close();
