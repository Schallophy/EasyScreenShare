// 声音链路验收的分析器：读 PCM16 WAV，报时长/RMS/峰值，并用 Goertzel 比对
// 「激励频率」与「对照频率」的能量增益。
//
// 只看 RMS 是不够的：任何噪声、回声、串进来的系统声音都会让 RMS > 0。加上对照
// 频点，才能证明穿过 Opus 到达观看端的确实是那个音，而不是"有点响声"。
import { readFileSync } from 'node:fs';

const [,, path, fSig = '440', fRef = '1000'] = process.argv;
if (!path) {
  console.error('用法: node audio-analyze.mjs in.wav [激励频率] [对照频率]');
  process.exit(2);
}
const buf = readFileSync(path);
if (buf.toString('ascii', 0, 4) !== 'RIFF') {
  console.error(`${path}: 不是 RIFF/WAVE`);
  process.exit(1);
}
let rate = 48000, channels = 2, bits = 16, dataOff = -1, dataLen = 0;
for (let p = 12; p + 8 <= buf.length; ) {
  const id = buf.toString('ascii', p, p + 4);
  const sz = buf.readUInt32LE(p + 4);
  if (id === 'fmt ') {
    channels = buf.readUInt16LE(p + 10);
    rate = buf.readUInt32LE(p + 12);
    bits = buf.readUInt16LE(p + 22);
  } else if (id === 'data') {
    dataOff = p + 8;
    dataLen = Math.min(sz, buf.length - dataOff);
  }
  p += 8 + sz + (sz & 1);
}
if (dataOff < 0 || bits !== 16) {
  console.error(`${path}: 解析失败（data=${dataOff} bits=${bits}）`);
  process.exit(1);
}
const n = Math.floor(dataLen / 2);
const samples = new Int16Array(n);
for (let i = 0; i < n; i++) samples[i] = buf.readInt16LE(dataOff + i * 2);

let sumSq = 0, peak = 0;
for (let i = 0; i < n; i++) {
  const v = samples[i] / 32767;
  sumSq += v * v;
  if (Math.abs(v) > peak) peak = Math.abs(v);
}
const rms = Math.sqrt(sumSq / Math.max(n, 1));

// 交织的立体声直接做频点分析会把时间轴压缩一半（440 Hz 会被看成 220 Hz），
// 所以先取出 0 声道，再按真实采样率算。
const mono = new Float64Array(Math.floor(n / channels));
for (let i = 0; i < mono.length; i++) mono[i] = samples[i * channels] / 32767;

// Goertzel：单频点 DFT，比整段 FFT 便宜，也避免频率分辨率掩盖问题
function goertzel(f0) {
  const m = Math.min(mono.length, Math.floor(rate * 4)); // 4 秒足够分辨 440 与 1000
  if (m < 8) return 0;
  const w = (2 * Math.PI * f0) / rate;
  const coeff = 2 * Math.cos(w);
  let s1 = 0, s2 = 0;
  for (let i = 0; i < m; i++) {
    const s0 = mono[i] + coeff * s1 - s2;
    s2 = s1;
    s1 = s0;
  }
  const power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
  return Math.sqrt(Math.max(power, 0)) / (m / 2);
}
const gSig = goertzel(Number(fSig));
const gRef = goertzel(Number(fRef));
console.log(
  `${path} ${n} 样本 ${rate}Hz x ${channels} 时长=${(n / channels / rate).toFixed(2)}s ` +
    `rms=${rms.toFixed(4)} peak=${peak.toFixed(3)} ` +
    `g(${fSig})=${gSig.toFixed(4)} g(${fRef})=${gRef.toFixed(4)} 比值=${(gSig / (gRef + 1e-12)).toFixed(1)}`
);
