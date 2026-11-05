// 观看页「详情」传输速率的探针：每秒取一次 `window.__ess.net()`，最后再读一遍 pc
// 的 getStats 明细，用来核对"我们加出来的两条路"和"transport 的真实接收量"是否闭合。
//
// 前置：观看页以 --app=http://127.0.0.1:8088/ --remote-debugging-port=9223 启动
// （复用 target/edge-viewer 这个固定 profile，绝不新建可同步的 profile）。
//
// 用法：node scripts/net-rate-probe.mjs [--cdp 9223] [--seconds 8] [--shot out.png]
import { writeFileSync } from 'node:fs';

const arg = (n, d) => {
  const i = process.argv.indexOf(n);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : d;
};
const PORT = arg('--cdp', '9223');
const SECONDS = Number(arg('--seconds', 8));
const SHOT = arg('--shot', '');
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

// 详情面板默认收起；验收要看的是它显示出来的样子，所以先按开启状态再读。
await evalJs("document.body.classList.add('debug')");

const rows = [];
for (let i = 0; i <= SECONDS; i++) {
  const s = await evalJs(`JSON.stringify({
    plane: window.__ess.plane(),
    net: window.__ess.net(),
    hud: document.getElementById('hud').textContent,
  })`);
  const r = JSON.parse(s);
  rows.push({ plane: r.plane, hud: r.hud, ...r.net });
  await new Promise((res) => setTimeout(res, 1000));
}

// 明细：我们报的两条路加不加得起来、和 transport 的接收量差多少。差额来自 SCTP/RTP
// 头与 RTCP——量级上应该是个零头，不该是一倍。
const detail = JSON.parse(await evalJs(`(async () => {
  const out = { rtp: [], transport_rx: 0, transport_tx: 0 };
  for (const pc of window.__ess.netPeers()) {
    const r = await pc.getStats();
    r.forEach((s) => {
      if (s.type === 'inbound-rtp') out.rtp.push({ kind: s.kind, bytes: s.bytesReceived,
        w: s.frameWidth, h: s.frameHeight });
      if (s.type === 'transport') { out.transport_rx += s.bytesReceived || 0; out.transport_tx += s.bytesSent || 0; }
    });
  }
  return JSON.stringify(out);
})()`));

if (SHOT) {
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  writeFileSync(SHOT, Buffer.from(shot.data, 'base64'));
}

const last = rows[rows.length - 1] || { net: {} };
const rtpBytes = detail.rtp.reduce((a, r) => a + (r.bytes || 0), 0);
console.log(JSON.stringify({
  docs: '传输速率读数：down = tiles + media，tiles 是数据通道到达字节，media 是 RTP 接收字节',
  hud: (rows[rows.length - 1] || {}).hud || '',
  visible: true,
  rows,
  net: {
    down: last.down || 0, tiles: last.tiles || 0, media: last.media || 0, up: last.up || 0,
    rx: last.rx || 0, tx: last.tx || 0,
  },
  detail,
  // 两条路之和必须等于 down（显示值就是这两个平滑量的和）
  sum_ok: Math.abs(((last.tiles || 0) + (last.media || 0)) - (last.down || 0)) < Math.max(1, (last.down || 0) * 0.02),
  rtp_bytes: rtpBytes,
  shot: SHOT || null,
}, null, 2));
ws.close();
