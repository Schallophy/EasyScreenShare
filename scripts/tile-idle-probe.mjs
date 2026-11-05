// 图块平面下视频轨按需订阅的探针：每 0.5 s 取一次页面状态，输出一条时间线。
//
// 要看的四件事都在这一行里：`plane`（发送端宣告的平面）、`sub`（我们主动摘/挂订阅
// 的状态）、`media`（inbound-rtp 的累计字节，退订之后应当不再涨）、`vf`（<video>
// 已解码帧数，复订之后必须继续涨，否则图块画布会一直冻在切换前那一帧）。
//
// 前置：观看页以 --app=http://127.0.0.1:8088/ --remote-debugging-port=9223 启动。
// 用法：node scripts/tile-idle-probe.mjs [--cdp 9223] [--seconds 30] [--every 500]
const arg = (n, d) => {
  const i = process.argv.indexOf(n);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : d;
};
const PORT = arg('--cdp', '9223');
const SECONDS = Number(arg('--seconds', 30));
const EVERY = Number(arg('--every', 500));
const URL_MATCH = arg('--match', '127.0.0.1:8088');

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
  if (m.id && pending.has(m.id)) { pending.get(m.id).res(m.result); pending.delete(m.id); }
});
await new Promise((r) => ws.addEventListener('open', r));
const evalJs = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 300));
  return r.result.value;
};

const rows = [];
const t0 = Date.now();
for (let i = 0; i * EVERY <= SECONDS * 1000; i++) {
  const s = JSON.parse(await evalJs(`(async () => {
    const out = { plane: window.__ess.plane(), sub: window.__ess.sub(),
      net: window.__ess.net(), vf: window.__ess.videoFrames(), vbytes: 0 };
    for (const pc of window.__ess.netPeers()) {
      (await pc.getStats()).forEach((x) => {
        if (x.type === 'inbound-rtp' && x.kind === 'video') out.vbytes += x.bytesReceived || 0;
      });
    }
    return JSON.stringify(out);
  })()`));
  rows.push({
    t: Date.now() - t0,
    plane: s.plane,
    off: !!(s.sub && s.sub.off),
    timer: !!(s.sub && s.sub.timer),
    pub: !!(s.sub && s.sub.pub),
    media: s.net ? s.net.media : 0,
    vbytes: s.vbytes || 0,
    tiles: s.net ? s.net.tiles : 0,
    down: s.net ? s.net.down : 0,
    vf: s.vf,
  });
  await new Promise((res) => setTimeout(res, EVERY));
}
console.log(JSON.stringify({ rows }, null, 1));
ws.close();
