// 把观看端解码落盘的视频帧**按内容**对回合成屏的动画格，从而算交付画质。
//
// 为什么不能按时间戳或帧号对齐：媒体通道会丢帧、会重排、编码器还会降分辨率，
// 到达顺序和内容进度之间的关系正是我们要测的东西，不能拿来当对齐依据。合成屏
// 每 100 ms 推进一格（一格 = 16 px 纵向滚动），画面内容唯一决定格号，所以对齐
// 用"哪一格和这一帧最像"来回答——这是可检验的：匹配上的格号应当单调不降。
//
// 用法：
//   node scripts/video-quality.mjs --seq DIR --ref ref.raw [--refw 1280 --refh 720]
//        [--swap] [--secs 12] [--pairs pairs.json]
// --swap 是反向对照：把解出来的帧红蓝互换后再匹配。通道序搞对了，正常匹配的整体
// 误差必须显著小于 swap；反过来则说明落盘的字节序是错的（那样画质数字全是假的）。
import { readdirSync, readFileSync, writeFileSync, statSync } from 'node:fs';
import { join } from 'node:path';

function arg(name, def) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : def;
}
const flag = (name) => process.argv.includes(`--${name}`);

const seqDir = arg('seq', null);
const refPath = arg('ref', null);
if (!seqDir || !refPath) {
  console.error('usage: video-quality.mjs --seq DIR --ref ref.raw [--refw 1280 --refh 720]');
  process.exit(2);
}
const refW = Number(arg('refw', 1280));
const refH = Number(arg('refh', 720));
const secs = Number(arg('secs', 12));
const swap = flag('swap');

// PPM(P6) → {w,h,rgb} ；顺手把 BGRA 参考流切成 RGB 帧
function readPpm(buf) {
  let p = 0;
  const tok = () => {
    while (p < buf.length && (buf[p] === 0x20 || buf[p] === 0x0a || buf[p] === 0x0d)) p++;
    let s = '';
    while (p < buf.length && buf[p] !== 0x20 && buf[p] !== 0x0a && buf[p] !== 0x0d) s += String.fromCharCode(buf[p++]);
    return s;
  };
  if (tok() !== 'P6') throw new Error('not P6');
  const w = Number(tok());
  const h = Number(tok());
  if (tok() !== '255') throw new Error('only 8-bit');
  p++; // 头后面的单个空白
  return { w, h, rgb: buf.subarray(p, p + w * h * 3) };
}

const refRaw = readFileSync(refPath);
const refIn = refW * refH * 4; // ess-scroll-dump 输出的是**裸 BGRA**，不是 RGB
const refStride = refW * refH * 3;
const nRef = Math.floor(refRaw.length / refIn);
// 参考是 BGRA 裸流，PPM 是 RGB：按同一套通道读，才比得出真色差
const refFrames = [];
for (let i = 0; i < nRef; i++) {
  const o = i * refIn;
  const px = new Uint8Array(refStride);
  for (let k = 0; k < refW * refH; k++) {
    const s = o + k * 4;
    px[k * 3] = refRaw[s + 2];
    px[k * 3 + 1] = refRaw[s + 1];
    px[k * 3 + 2] = refRaw[s];
  }
  refFrames.push(px);
}

// 代理特征：CW×CH 个格子，每格 4×4 均匀采样，与分辨率无关（编码器降了也能对）
const CW = 64, CH = 36;
function proxy(f) {
  const out = new Float64Array(CW * CH * 3);
  const cw = f.w / CW, ch = f.h / CH;
  for (let gy = 0; gy < CH; gy++) {
    for (let gx = 0; gx < CW; gx++) {
      let r = 0, g = 0, b = 0;
      for (let sy = 0; sy < 4; sy++) {
        let y = Math.floor(gy * ch + (sy + 0.5) * ch / 4);
        if (y >= f.h) y = f.h - 1;
        for (let sx = 0; sx < 4; sx++) {
          let x = Math.floor(gx * cw + (sx + 0.5) * cw / 4);
          if (x >= f.w) x = f.w - 1;
          const o = (y * f.w + x) * 3;
          r += f.rgb[o]; g += f.rgb[o + 1]; b += f.rgb[o + 2];
        }
      }
      const i = (gy * CW + gx) * 3;
      out[i] = r / 16; out[i + 1] = g / 16; out[i + 2] = b / 16;
    }
  }
  return out;
}
const refProxy = refFrames.map((px, i) => proxy({ w: refW, h: refH, rgb: px }));

function match(p) {
  let best = -1, bestErr = Infinity;
  for (let i = 0; i < refProxy.length; i++) {
    const q = refProxy[i];
    let e = 0;
    for (let k = 0; k < p.length; k++) {
      const d = p[k] - q[k];
      e += d * d;
    }
    e /= p.length;
    if (e < bestErr) { bestErr = e; best = i; }
  }
  return [best, bestErr];
}

const files = readdirSync(seqDir).filter((n) => n.endsWith('.ppm')).sort();
const rows = [];
let prev = -1, backwards = 0;
for (const [i, name] of files.entries()) {
  const f = readPpm(readFileSync(join(seqDir, name)));
  // 落盘时间就是到达时间（写任务按序提交）：平面只在运动的那 8 秒里出画，用整个
  // 会话窗口去除会把交付帧率算低——必须先量出真正的时间跨度。
  const mtime = statSync(join(seqDir, name)).mtimeMs;
  if (swap) {
    for (let k = 0; k < f.w * f.h; k++) {
      const o = k * 3;
      const t = f.rgb[o];
      f.rgb[o] = f.rgb[o + 2];
      f.rgb[o + 2] = t;
    }
  }
  const [idx, err] = match(proxy(f));
  if (idx < prev) backwards++;
  prev = idx;
  rows.push({ n: i, file: name, w: f.w, h: f.h, idx, err, mtime });
}

const errs = rows.map((r) => r.err).sort((a, b) => a - b);
const med = errs[Math.floor(errs.length / 2)] ?? 0;
const first = rows[0], last = rows[rows.length - 1];
const span = last && first ? last.idx - first.idx : 0;
// 用首末帧的落盘时间差当作平面开启窗口；样本太少时退回调用方给的窗口
const wall = last && last.mtime > first.mtime ? (last.mtime - first.mtime) / 1000 : 0;
const win = wall > 0.5 ? wall : secs;
// 交付的"画面进度"：合成屏每 0.1 s 推进一格，所以 10 格/s 就是"观看端和内容同速"。
//
// 分母不能带素材停更后的静止尾。合成定标实测（同一把尺跑已知良好的采）：
//   完美交付、无尾 → 10.0；完美交付 + 3 s 静止尾 → **6.7**；门槛却是 7。
// 也就是说只要尾巴在，满分也会被判成不及格——观看端写盘 13 s 而素材只推进 8 s，
// 每次跑都带 ~5 s 静止尾。原则化的口径：内容推进到最后一格之后就没有"该看而没
// 看到"的东西了，所以分母只算到**第一次到达最终格号**那一刻；开头卡住不动的帧
// 不裁，那是真实的欠交付。未裁的值仍打出来，便于和旧读数对齐。
const pts2 = rows.filter((r) => r.idx >= 0 && r.mtime > 0);
const maxIdx = pts2.length ? Math.max(...pts2.map((r) => r.idx)) : 0;
const stopAt = pts2.find((r) => r.idx === maxIdx) ?? last;
const tWin = stopAt && pts2.length ? (stopAt.mtime - pts2[0].mtime) / 1000 : 0;
const netRate = tWin > 0.5 ? (maxIdx - pts2[0].idx) / tWin : span / win;
console.log(`帧数=${rows.length} 解码 ${rows.length ? rows[0].w + 'x' + rows[0].h : '-'} 参考格数=${nRef}`);
console.log(`平面出画窗口=${win.toFixed(1)} s（首末帧落盘时间差）`);
console.log(`交付帧率=${(rows.length / win).toFixed(1)} fps`);
console.log(`匹配格号 ${first?.idx}→${maxIdx}（跨度 ${span}，覆盖 ${(100 * span / Math.max(1, Math.round(win * 10))).toFixed(0)}% 的真实进度）→ 内容进度 ${netRate.toFixed(1)} 格/s（裁尾窗 ${tWin.toFixed(1)} s，未裁 ${(span / win).toFixed(1)}）`);
console.log(`代理 MSE：中位 ${med.toFixed(1)} 最小 ${(errs[0] ?? 0).toFixed(1)} 最大 ${(errs[errs.length - 1] ?? 0).toFixed(1)}`);
console.log(`乱序帧数=${backwards}（应为 0 或极少；多则说明到达顺序/内容对不上）`);
if (rows.length) {
  const dims = [...new Set(rows.map((r) => r.w + 'x' + r.h))];
  console.log(`出现过的分辨率：${dims.join(' ')}`);
}
if (flag('verbose')) {
  for (const r of rows) console.log(`${r.n}\t${r.file}\t${r.w}x${r.h}\tidx=${r.idx}\tmse=${r.err.toFixed(1)}`);
}
// 供 ffmpeg 出权威指标用：把运动段里均匀挑出的 K 帧（及其匹配格号、分辨率）写出去
const out = arg('pairs', null);
if (out) {
  const K = Number(arg('sample', 16));
  const motion = rows.filter((r) => r.err < Math.max(med * 20, 2000));
  const picked = [];
  for (let i = 0; i < Math.min(K, motion.length); i++) {
    picked.push(motion[Math.floor((i * motion.length) / K)]);
  }
  writeFileSync(out, JSON.stringify(picked, null, 0));
  console.log(`pairs → ${out}（${picked.length} 帧，取自 ${motion.length} 个可信匹配）`);
}
