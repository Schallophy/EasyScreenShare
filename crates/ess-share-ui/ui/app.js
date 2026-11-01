'use strict';

// Tauri 全局 API（tauri.conf.json: withGlobalTauri = true）
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const pill = $('pill');
const logEl = $('log');

function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1048576).toFixed(2)} MB`;
}

function setRunning(running) {
  for (const id of ['server', 'room', 'identity', 'fps', 'mode', 'demo', 'key', 'secret', 'videoRes', 'videoBr']) {
    $(id).disabled = running;
  }
  $('start').disabled = running;
  $('stop').disabled = !running;
}

function setPhase(name, detail) {
  const map = {
    idle: ['空闲', 'idle'],
    connecting: ['连接中', 'busy'],
    live: ['共享中', 'live'],
    stopping: ['停止中', 'busy'],
    error: ['出错', 'err'],
  };
  const [label, cls] = map[name] || [name, 'idle'];
  pill.textContent = detail ? `${label} · ${detail}` : label;
  pill.className = `pill ${cls}`;
  if (name === 'live' && detail) $('s-geom').textContent = detail;
  if (name === 'idle' || name === 'error') {
    $('s-geom').textContent = '—';
    $('s-elapsed').textContent = '0 s';
    $('s-frames').textContent = '0';
    $('s-fps').textContent = '0';
    $('s-tiles').textContent = '0';
    $('s-msgs').textContent = '0';
    $('s-bytes').textContent = '0 B';
  }
  setRunning(name === 'connecting' || name === 'live' || name === 'stopping');
}

function logLine(text) {
  const t = new Date().toLocaleTimeString('zh-CN', { hour12: false });
  logEl.textContent += `${t}  ${text}\n`;
  const lines = logEl.textContent.split('\n');
  if (lines.length > 300) logEl.textContent = lines.slice(-300).join('\n');
  logEl.scrollTop = logEl.scrollHeight;
}

async function start() {
  const res = $('videoRes').value;              // "0" | "1280x720" | "1920x1080"
  const [rw, rh] = res === '0' ? [0, 0] : res.split('x').map(Number);
  const params = {
    server: $('server').value.trim(),
    room: $('room').value.trim(),
    identity: $('identity').value.trim(),
    key: $('key').value.trim(),
    secret: $('secret').value.trim(),
    fps: Number($('fps').value) || 10,
    mode: $('mode').value,
    demo: $('demo').checked,
    videoW: rw,
    videoH: rh,
    videoBitrateMbps: Number($('videoBr').value) || 0,
    audio: $('shareAudio').checked,
  };
  try {
    await invoke('start_share', params);
  } catch (e) {
    logLine(`启动失败: ${e}`);
  }
}

async function stop() {
  try {
    await invoke('stop_share');
  } catch (e) {
    logLine(`停止失败: ${e}`);
  }
}

window.addEventListener('DOMContentLoaded', async () => {
  setRunning(false);
  $('start').onclick = start;
  $('stop').onclick = stop;

  await listen('ess-phase', (ev) => setPhase(ev.payload.phase, ev.payload.detail));
  await listen('ess-log', (ev) => logLine(String(ev.payload)));
  await listen('ess-viewer', (ev) => {
    const a = $('viewerurl');
    a.href = ev.payload.url;
    a.textContent = ev.payload.url + '（房间 ' + ev.payload.room + '）';
    $('viewerlink').hidden = false;
  });
  let lastFrames = 0, lastAt = 0;
  await listen('ess-stats', (ev) => {
    const s = ev.payload;
    const now = performance.now();
    if (lastAt && now > lastAt && s.frames >= lastFrames) {
      $('s-fps').textContent = ((s.frames - lastFrames) * 1000 / (now - lastAt)).toFixed(0);
    }
    lastFrames = s.frames;
    lastAt = now;
    $('s-elapsed').textContent = `${s.elapsed_s} s`;
    $('s-frames').textContent = s.frames;
    $('s-plane').textContent = s.video_track
      ? (s.video_mode ? `视频 ${s.video_frames}` : '图块（无损）')
      : '图块（无视频轨）';
    $('s-tiles').textContent = s.tiles;
    $('s-msgs').textContent = s.msgs;
    $('s-bytes').textContent = fmtBytes(s.bytes);
    $('s-up').textContent = s.link_up || '—';
    $('s-enc').textContent = s.link_limited ? `${s.link_enc} · ${s.link_limited}` : s.link_enc;
    $('s-audio').textContent = s.audio || '—';
  });

  // autostart 的命令行参数回填到表单：不回填的话命令行说 60 fps、界面还写着 10，
  // 验收的人照着界面读数会误判（结构体字段是 snake_case，只有命令入参才转 camelCase）。
  const a = await invoke('start_args').catch(() => null);
  if (a) {
    $('server').value = a.server;
    $('room').value = a.room;
    $('identity').value = a.identity;
    $('fps').value = a.fps;
    $('demo').checked = a.demo;
    const res = a.video_w && a.video_h ? `${a.video_w}x${a.video_h}` : '0';
    if ([...$('videoRes').options].some((o) => o.value === res)) $('videoRes').value = res;
    const br = String(a.video_bitrate_mbps || 0);
    if ([...$('videoBr').options].some((o) => o.value === br)) $('videoBr').value = br;
    $('shareAudio').checked = a.audio !== false;
    if ([...$('mode').options].some((o) => o.value === a.mode)) $('mode').value = a.mode;
    logLine(`命令行已指定：${a.fps} fps、${$('mode').selectedOptions[0].text}、${a.demo ? '合成屏' : '真机屏幕'}、系统声音${a.audio === false ? '关' : '开'}（表单已按此回填）`);
  }

  if (await invoke('session_active').catch(() => false)) {
    setPhase('live', null);
  }
  logLine('就绪');
});
