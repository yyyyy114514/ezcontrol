'use strict';
const $ = (id) => document.getElementById(id);
const canvas = $('screen');
const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
const stage = $('stage');
const overlay = $('overlay');

// ---------- settings (remembered per browser) ----------
const prefs = (() => {
  try { return JSON.parse(localStorage.getItem('ezc.prefs') || '{}'); } catch { return {}; }
})();
function savePrefs() {
  try { localStorage.setItem('ezc.prefs', JSON.stringify(prefs)); } catch { /* ignore */ }
}
if (prefs.q) $('rngQ').value = prefs.q;
if (prefs.scaleMode) $('selScale').value = prefs.scaleMode; // new key: older saves pre-date 自动
if (prefs.fps) $('selFps').value = prefs.fps;
if (prefs.fit) $('selFit').value = prefs.fit;
if (prefs.refine === false) $('chkRefine').checked = false;
if (prefs.auto === false) $('chkAuto').checked = false;
if (prefs.codec) $('selCodec').value = prefs.codec;
let h264Ok = false; // browser can decode H.264 with WebCodecs
$('lblQ').textContent = $('rngQ').value;

/** Scale that matches the remote screen to the pixels the stage can actually show. */
function autoScale() {
  const mon = monitors[+$('selMon').value || 0];
  if (!mon || $('selFit').value === 'real') return 1;
  const dpr = window.devicePixelRatio || 1;
  const s = Math.min(1, (stage.clientWidth * dpr) / mon.w, (stage.clientHeight * dpr) / mon.h);
  // round up to 5% steps so small resizes don't restart the encoder
  return Math.min(1, Math.max(0.25, Math.ceil(s * 20) / 20));
}

function settings() {
  const sv = $('selScale').value;
  return {
    quality: +$('rngQ').value,
    scale: sv === 'auto' ? autoScale() : +sv,
    fps: +$('selFps').value,
    refine: $('chkRefine').checked,
    auto: $('chkAuto').checked,
    h264: h264Ok && $('selCodec').value === 'h264',
  };
}
function applyFit() {
  const real = $('selFit').value === 'real';
  stage.classList.toggle('real', real);
  stage.classList.toggle('fit', !real);
}
applyFit();

// ---------- connection ----------
let ws = null;
let connected = false;
let monitors = [];
let lastPing = 0;
let rtt = 0;
let adapt = null; // {q, s}: quality/scale the server is currently using in auto mode
const stats = { frames: 0, bytes: 0, t0: performance.now() };

function send(obj) {
  if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj));
}

function connect() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  ws = new WebSocket(`${proto}//${location.host}/ws`);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => {
    connected = true;
    overlay.textContent = '等待画面…';
    send({ t: 'cfg', ...settings() });
    send({ t: 'clip_watch', on: clipIn() !== 'off' });
    send({ t: 'info', ua: navigator.userAgent, codec: settings().h264 ? 'h264' : 'jpeg', h264: h264Probe });
  };
  ws.onclose = () => {
    connected = false;
    overlay.classList.remove('hidden');
    overlay.textContent = '连接已断开，正在重连…';
    setTimeout(async () => {
      // session expired? go back to login
      const r = await fetch('/api/files/list?path=', { cache: 'no-store' }).catch(() => null);
      if (r && r.status === 401) location.href = '/login';
      else connect();
    }, 1500);
  };
  ws.onmessage = (ev) => {
    if (typeof ev.data === 'string') onText(JSON.parse(ev.data));
    else onFrame(ev.data);
  };
}

function onText(m) {
  switch (m.t) {
    case 'hello': {
      monitors = m.monitors;
      const sel = $('selMon');
      sel.innerHTML = '';
      monitors.forEach((mon, i) => {
        const o = document.createElement('option');
        o.value = i;
        o.textContent = `${i + 1}: ${mon.w}×${mon.h}${mon.primary ? ' 主' : ''}`;
        sel.appendChild(o);
      });
      sel.value = m.monitor;
      sel.classList.toggle('hidden', monitors.length < 2);
      if ($('selScale').value === 'auto') pushCfg(); // screen size is known now
      break;
    }
    case 'pong': rtt = performance.now() - m.ts; break;
    case 'adapt': adapt = m; break;
    case 'cur': canvas.style.cursor = viewOnly() ? 'default' : m.c; break;
    case 'clip':
      if (m.auto) { onRemoteClip(m.s); break; }
      $('clipText').value = m.s ?? '';
      if (m.s == null) toast('远程剪贴板没有文本');
      break;
  }
}

// ---------- video ----------
// [1][seq u32][fw u16][fh u16][n u16] then n x {x,y,w,h u16, len u32, jpeg}
// Several frames can be in flight: decode them in parallel, but draw strictly in arrival
// order, otherwise an older frame finishing late would paint stale tiles over newer ones.
let drawChain = Promise.resolve();

function onFrame(buf) {
  const dv = new DataView(buf);
  if (dv.getUint8(0) === 2) { onH264(buf); return; }
  if (dv.getUint8(0) !== 1) return;
  const seq = dv.getUint32(1, true);
  const fw = dv.getUint16(5, true);
  const fh = dv.getUint16(7, true);
  const n = dv.getUint16(9, true);
  let off = 11;
  const parts = [];
  for (let i = 0; i < n; i++) {
    const x = dv.getUint16(off, true), y = dv.getUint16(off + 2, true);
    const len = dv.getUint32(off + 8, true);
    off += 12;
    parts.push({ x, y, blob: new Blob([new Uint8Array(buf, off, len)], { type: 'image/jpeg' }) });
    off += len;
  }
  const decoded = Promise.all(parts.map((p) => createImageBitmap(p.blob))).catch(() => null);
  drawChain = drawChain.then(async () => {
    const bitmaps = await decoded;
    if (!bitmaps) {
      send({ t: 'ack', s: seq });
      send({ t: 'key' });
      return;
    }
    if (canvas.width !== fw || canvas.height !== fh) {
      canvas.width = fw;
      canvas.height = fh;
    }
    for (let i = 0; i < n; i++) {
      ctx.drawImage(bitmaps[i], parts[i].x, parts[i].y);
      bitmaps[i].close();
    }
    send({ t: 'ack', s: seq });
    overlay.classList.add('hidden');
    stats.frames++;
    stats.bytes += buf.byteLength;
  });
}

// ---------- H.264 (WebCodecs) ----------
// [2][seq u32][w u16][h u16][key u8] then an Annex B access unit
let decoder = null;
let decoderCodec = '';
let awaitingKey = false;

// what the browser said about H.264, reported to the server log for diagnosis
const h264Probe = { webcodecs: typeof VideoDecoder !== 'undefined', secure: window.isSecureContext, ok: [], err: '' };

async function detectH264() {
  if (!h264Probe.webcodecs) return false;
  // the stream's own profile/level first; any of these means the decoder handles our stream
  // (decoders accept the actual level from the SPS once configured)
  for (const codec of ['avc1.42c033', 'avc1.42E034', 'avc1.42001f', 'avc1.640033', 'avc1.4d0033']) {
    try {
      const r = await VideoDecoder.isConfigSupported({ codec, optimizeForLatency: true });
      if (r.supported) h264Probe.ok.push(codec);
    } catch (e) {
      h264Probe.err = String(e);
    }
  }
  return h264Probe.ok.length > 0;
}

/** avc1.PPCCLL from the SPS in an Annex B access unit. */
function codecFromSps(u8) {
  for (let i = 0; i + 4 < u8.length; i++) {
    if (u8[i] === 0 && u8[i + 1] === 0 && u8[i + 2] === 1 && (u8[i + 3] & 0x1f) === 7) {
      const hex = (b) => b.toString(16).padStart(2, '0');
      return 'avc1.' + hex(u8[i + 4]) + hex(u8[i + 5]) + hex(u8[i + 6]);
    }
  }
  return null;
}

function resetDecoder() {
  if (decoder && decoder.state !== 'closed') {
    try { decoder.close(); } catch { /* ignore */ }
  }
  decoder = null;
  decoderCodec = '';
}

function makeDecoder(codec) {
  resetDecoder();
  decoder = new VideoDecoder({
    output: (frame) => {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
      const seq = Math.round(frame.timestamp / 1000);
      frame.close();
      send({ t: 'ack', s: seq });
      overlay.classList.add('hidden');
      stats.frames++;
    },
    error: () => {
      resetDecoder();
      awaitingKey = true;
      send({ t: 'key' });
    },
  });
  decoder.configure({ codec, optimizeForLatency: true });
  decoderCodec = codec;
}

function onH264(buf) {
  const dv = new DataView(buf);
  const seq = dv.getUint32(1, true);
  const key = dv.getUint8(9) === 1;
  const data = new Uint8Array(buf, 10);
  stats.bytes += buf.byteLength;
  if (key) {
    const codec = codecFromSps(data);
    if (codec && (!decoder || decoder.state !== 'configured' || codec !== decoderCodec)) makeDecoder(codec);
    awaitingKey = false;
  }
  if (!decoder || decoder.state !== 'configured' || awaitingKey) {
    send({ t: 'ack', s: seq }); // keep the server's flow control in step
    if (!awaitingKey) { awaitingKey = true; send({ t: 'key' }); }
    return;
  }
  // timestamp = sequence number (µs units), so the output frame tells which one to acknowledge
  decoder.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: seq * 1000, data }));
}

setInterval(() => {
  if (!connected) { $('stats').textContent = '未连接'; return; }
  const now = performance.now();
  const dt = (now - stats.t0) / 1000;
  const fps = stats.frames / dt;
  const kbps = (stats.bytes * 8) / 1000 / dt;
  const rate = kbps > 1000 ? (kbps / 1000).toFixed(1) + ' Mbps' : kbps.toFixed(0) + ' kbps';
  const s = settings();
  let auto = '';
  if (s.h264) {
    auto = ' · H.264';
    if (adapt && adapt.kbps) auto += ` 码率 ${(adapt.kbps / 1000).toFixed(1)}/${(adapt.max_kbps / 1000).toFixed(1)} Mbps`;
  } else if (adapt && s.auto && (adapt.q < s.quality || adapt.s < s.scale - 0.01)) {
    auto = ` · 自动降至 画质${adapt.q}/${Math.round(adapt.s * 100)}%`;
  }
  $('stats').textContent = `${fps.toFixed(0)} fps · ${rate} · ${rtt.toFixed(0)} ms${auto}`;
  stats.frames = 0; stats.bytes = 0; stats.t0 = now;
  send({ t: 'ping', ts: now });
}, 1000);

// ---------- toolbar ----------
function pushCfg() {
  prefs.q = +$('rngQ').value;
  prefs.scaleMode = $('selScale').value;
  prefs.fps = $('selFps').value;
  prefs.fit = $('selFit').value;
  prefs.refine = $('chkRefine').checked;
  prefs.auto = $('chkAuto').checked;
  prefs.codec = $('selCodec').value;
  savePrefs();
  send({ t: 'cfg', ...settings() });
}
$('rngQ').addEventListener('input', () => { $('lblQ').textContent = $('rngQ').value; });
$('rngQ').addEventListener('change', pushCfg);
$('selScale').addEventListener('change', pushCfg);
$('selFps').addEventListener('change', pushCfg);
$('chkRefine').addEventListener('change', pushCfg);
$('chkAuto').addEventListener('change', () => { adapt = null; pushCfg(); });
$('selCodec').addEventListener('change', () => { adapt = null; resetDecoder(); pushCfg(); });
$('selFit').addEventListener('change', () => { applyFit(); pushCfg(); });
$('selMon').addEventListener('change', () => {
  send({ t: 'mon', i: +$('selMon').value });
  if ($('selScale').value === 'auto') pushCfg();
});
// follow the browser window size in auto scale mode
let lastAutoScale = 0;
let resizeTimer;
window.addEventListener('resize', () => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => {
    if ($('selScale').value !== 'auto') return;
    const s = autoScale();
    if (s !== lastAutoScale) { lastAutoScale = s; pushCfg(); }
  }, 400);
});
$('btnHide').onclick = () => { $('bar').classList.add('hidden'); $('btnShow').classList.remove('hidden'); };
$('btnShow').onclick = () => { $('bar').classList.remove('hidden'); $('btnShow').classList.add('hidden'); };
$('btnLogout').onclick = async () => {
  await fetch('/api/logout', { method: 'POST', headers: { 'x-ezc': '1' } });
  location.href = '/login';
};
$('btnFull').onclick = async () => {
  if (document.fullscreenElement) { await document.exitFullscreen(); return; }
  await document.documentElement.requestFullscreen().catch(() => {});
  // In fullscreen Chromium lets the page capture Win, Alt+Tab, Esc etc.
  if (navigator.keyboard && navigator.keyboard.lock) navigator.keyboard.lock().catch(() => {});
};
document.addEventListener('fullscreenchange', () => {
  $('btnFull').textContent = document.fullscreenElement ? '退出全屏' : '全屏';
  if (!document.fullscreenElement && navigator.keyboard && navigator.keyboard.unlock) navigator.keyboard.unlock();
});

function viewOnly() { return $('chkView').checked; }

function togglePanel(id) {
  const el = $(id);
  const show = el.classList.contains('hidden');
  for (const p of ['clipPanel', 'textPanel', 'filePanel']) if (p !== id) $(p).classList.add('hidden');
  el.classList.toggle('hidden', !show);
  return show;
}
document.querySelectorAll('[data-close]').forEach((b) => {
  b.onclick = () => { $(b.dataset.close).classList.add('hidden'); stage.focus(); };
});
$('btnClip').onclick = () => togglePanel('clipPanel');
$('btnText').onclick = () => { if (togglePanel('textPanel')) $('typeText').focus(); };
$('btnFiles').onclick = () => { if (togglePanel('filePanel')) loadDir(prefs.dir ?? ''); };
$('btnKeys').onclick = () => $('kbd').classList.toggle('hidden');

let toastTimer;
function toast(text) {
  const t = $('toast');
  t.textContent = text;
  t.classList.remove('hidden');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add('hidden'), 2000);
}

// ---------- mouse / touch ----------
function norm(e) {
  const r = canvas.getBoundingClientRect();
  return {
    x: Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)),
    y: Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)),
  };
}

let pendingMove = null;
function queueMove(p) {
  if (!pendingMove) requestAnimationFrame(flushMove);
  pendingMove = p;
}
function flushMove() {
  if (pendingMove) send({ t: 'mm', ...pendingMove });
  pendingMove = null;
}
function button(b, down, p) {
  pendingMove = null;
  send({ t: down ? 'md' : 'mu', b, ...p });
}

const touches = new Map();
let touchState = null; // {start, p, timer, mode: 'tap'|'drag'|'right'|'scroll'}

canvas.addEventListener('pointerdown', (e) => {
  if (viewOnly()) return;
  stage.focus({ preventScroll: true });
  e.preventDefault();
  if (e.pointerType === 'touch') return touchDown(e);
  canvas.setPointerCapture(e.pointerId);
  button(e.button, true, norm(e));
});
canvas.addEventListener('pointermove', (e) => {
  if (viewOnly()) return;
  if (e.pointerType === 'touch') return touchMove(e);
  queueMove(norm(e));
});
canvas.addEventListener('pointerup', (e) => {
  if (viewOnly()) return;
  if (e.pointerType === 'touch') return touchUp(e);
  button(e.button, false, norm(e));
});
canvas.addEventListener('pointercancel', (e) => { if (e.pointerType === 'touch') touchUp(e, true); });
canvas.addEventListener('contextmenu', (e) => e.preventDefault());

let wheelAcc = { x: 0, y: 0 };
stage.addEventListener('wheel', (e) => {
  if (viewOnly()) return;
  e.preventDefault();
  const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1.2;
  wheelAcc.x += e.deltaX * k;
  wheelAcc.y += e.deltaY * k;
  const dx = Math.trunc(wheelAcc.x), dy = Math.trunc(wheelAcc.y);
  if (dx || dy) {
    wheelAcc.x -= dx; wheelAcc.y -= dy;
    send({ t: 'wh', dx, dy });
  }
}, { passive: false });

function touchDown(e) {
  touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (touches.size === 2) {
    if (touchState && touchState.mode === 'drag') button(0, false, touchState.p);
    if (touchState) clearTimeout(touchState.timer);
    touchState = { mode: 'scroll', y: avgY(), x: avgX() };
    return;
  }
  if (touches.size > 2) return;
  const p = norm(e);
  touchState = {
    mode: 'tap', p, sx: e.clientX, sy: e.clientY,
    timer: setTimeout(() => {
      if (touchState && touchState.mode === 'tap') {
        touchState.mode = 'right';
        send({ t: 'mm', ...p });
        button(2, true, p);
        button(2, false, p);
        if (navigator.vibrate) navigator.vibrate(20);
      }
    }, 550),
  };
}
function avgX() { let s = 0; touches.forEach((t) => { s += t.x; }); return s / touches.size; }
function avgY() { let s = 0; touches.forEach((t) => { s += t.y; }); return s / touches.size; }
function touchMove(e) {
  if (!touches.has(e.pointerId) || !touchState) return;
  touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (touchState.mode === 'scroll') {
    const x = avgX(), y = avgY();
    const dy = Math.round((touchState.y - y) * 3), dx = Math.round((touchState.x - x) * 3);
    if (Math.abs(dy) >= 10 || Math.abs(dx) >= 10) {
      send({ t: 'wh', dx, dy });
      touchState.x = x; touchState.y = y;
    }
    return;
  }
  const p = norm(e);
  if (touchState.mode === 'tap' && Math.hypot(e.clientX - touchState.sx, e.clientY - touchState.sy) > 8) {
    clearTimeout(touchState.timer);
    touchState.mode = 'drag';
    button(0, true, touchState.p);
  }
  if (touchState.mode === 'drag') { touchState.p = p; queueMove(p); }
}
function touchUp(e, cancel) {
  touches.delete(e.pointerId);
  if (!touchState) return;
  if (touchState.mode === 'scroll') { if (touches.size === 0) touchState = null; return; }
  clearTimeout(touchState.timer);
  if (touchState.mode === 'tap' && !cancel) {
    button(0, true, touchState.p);
    button(0, false, touchState.p);
  } else if (touchState.mode === 'drag') {
    button(0, false, touchState.p);
  }
  touchState = null;
}

// ---------- physical keyboard ----------
function typingInUi(e) {
  const t = e.target;
  return t !== stage && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT');
}
window.addEventListener('keydown', async (e) => {
  if (viewOnly() || typingInUi(e) || !e.code) return;
  e.preventDefault();
  if (e.code === 'KeyV' && (e.ctrlKey || e.metaKey) && !e.repeat && clipOut() !== 'off') {
    // pressing paste is explicit intent, so sync directly even in "ask" mode
    const text = await readLocalClip();
    if (text && text !== lastClip) pushClip(text);
  }
  send({ t: 'kd', code: e.code });
});
window.addEventListener('keyup', (e) => {
  if (viewOnly() || typingInUi(e) || !e.code) return;
  e.preventDefault();
  send({ t: 'ku', code: e.code });
});
window.addEventListener('blur', () => send({ t: 'rel' }));
document.addEventListener('visibilitychange', () => { if (document.hidden) send({ t: 'rel' }); });

// ---------- shortcuts ----------
const COMBOS = [
  ['Ctrl+Alt+Del 不可用：请用 Ctrl+Shift+Esc', null],
  ['任务管理器', 'ControlLeft+ShiftLeft+Escape'],
  ['切换窗口', 'AltLeft+Tab'],
  ['任务视图', 'MetaLeft+Tab'],
  ['开始菜单', 'MetaLeft'],
  ['显示桌面', 'MetaLeft+KeyD'],
  ['资源管理器', 'MetaLeft+KeyE'],
  ['运行', 'MetaLeft+KeyR'],
  ['截图', 'MetaLeft+ShiftLeft+KeyS'],
  ['关闭窗口', 'AltLeft+F4'],
  ['复制', 'ControlLeft+KeyC'],
  ['粘贴', 'ControlLeft+KeyV'],
  ['剪切', 'ControlLeft+KeyX'],
  ['撤销', 'ControlLeft+KeyZ'],
  ['全选', 'ControlLeft+KeyA'],
  ['保存', 'ControlLeft+KeyS'],
  ['切换输入法', 'MetaLeft+Space'],
  ['PrintScreen', 'PrintScreen'],
];
const comboMenu = $('comboMenu');
for (const [label, keys] of COMBOS) {
  const b = document.createElement('button');
  if (!keys) { b.disabled = true; b.innerHTML = `<small>${label}</small>`; comboMenu.appendChild(b); continue; }
  b.innerHTML = `<span>${label}</span><small>${keys.replace(/Left|Key/g, '').replace('Meta', 'Win').replace('Control', 'Ctrl')}</small>`;
  b.onclick = () => { send({ t: 'combo', keys: keys.split('+') }); comboMenu.classList.remove('open'); stage.focus(); };
  comboMenu.appendChild(b);
}
{
  const row = document.createElement('div');
  row.className = 'custom';
  row.innerHTML = '<input id="comboIn" placeholder="自定义，如 Ctrl+Shift+T"><button id="comboGo">发送</button>';
  comboMenu.appendChild(row);
}
const NAME_TO_CODE = {
  ctrl: 'ControlLeft', control: 'ControlLeft', shift: 'ShiftLeft', alt: 'AltLeft', win: 'MetaLeft',
  meta: 'MetaLeft', esc: 'Escape', enter: 'Enter', tab: 'Tab', space: 'Space', del: 'Delete',
  delete: 'Delete', backspace: 'Backspace', up: 'ArrowUp', down: 'ArrowDown', left: 'ArrowLeft',
  right: 'ArrowRight', home: 'Home', end: 'End', pgup: 'PageUp', pgdn: 'PageDown', ins: 'Insert',
};
function parseCombo(s) {
  return s.split('+').map((k) => k.trim()).filter(Boolean).map((k) => {
    const l = k.toLowerCase();
    if (NAME_TO_CODE[l]) return NAME_TO_CODE[l];
    if (/^[a-z]$/.test(l)) return 'Key' + l.toUpperCase();
    if (/^[0-9]$/.test(l)) return 'Digit' + l;
    if (/^f([1-9]|1[0-9]|2[0-4])$/.test(l)) return l.toUpperCase();
    return k;
  });
}
$('comboGo').onclick = () => {
  const keys = parseCombo($('comboIn').value);
  if (keys.length) send({ t: 'combo', keys });
  comboMenu.classList.remove('open');
  stage.focus();
};
$('btnCombo').onclick = (e) => { e.stopPropagation(); comboMenu.classList.toggle('open'); };
document.addEventListener('click', (e) => { if (!comboMenu.contains(e.target)) comboMenu.classList.remove('open'); });

// ---------- virtual keyboard ----------
const KB = [
  [['Esc', 'Escape'], ['F1'], ['F2'], ['F3'], ['F4'], ['F5'], ['F6'], ['F7'], ['F8'], ['F9'], ['F10'], ['F11'], ['F12'], ['PrtSc', 'PrintScreen'], ['Del', 'Delete']],
  [['`', 'Backquote'], ['1', 'Digit1'], ['2', 'Digit2'], ['3', 'Digit3'], ['4', 'Digit4'], ['5', 'Digit5'], ['6', 'Digit6'], ['7', 'Digit7'], ['8', 'Digit8'], ['9', 'Digit9'], ['0', 'Digit0'], ['-', 'Minus'], ['=', 'Equal'], ['⌫', 'Backspace', 1.6]],
  [['Tab', 'Tab', 1.4], ...'QWERTYUIOP'.split('').map((c) => [c, 'Key' + c]), ['[', 'BracketLeft'], [']', 'BracketRight'], ['\\', 'Backslash', 1.2]],
  [['Caps', 'CapsLock', 1.7], ...'ASDFGHJKL'.split('').map((c) => [c, 'Key' + c]), [';', 'Semicolon'], ["'", 'Quote'], ['Enter', 'Enter', 1.9]],
  [['Shift', 'ShiftLeft', 2.2], ...'ZXCVBNM'.split('').map((c) => [c, 'Key' + c]), [',', 'Comma'], ['.', 'Period'], ['/', 'Slash'], ['↑', 'ArrowUp'], ['Shift', 'ShiftRight', 1.4]],
  [['Ctrl', 'ControlLeft', 1.3], ['Win', 'MetaLeft', 1.2], ['Alt', 'AltLeft', 1.2], ['Space', 'Space', 5], ['Alt', 'AltRight', 1.2], ['Ctrl', 'ControlRight', 1.2], ['←', 'ArrowLeft'], ['↓', 'ArrowDown'], ['→', 'ArrowRight']],
  [['Home'], ['End'], ['PgUp', 'PageUp'], ['PgDn', 'PageDown'], ['Ins', 'Insert'], ['菜单', 'ContextMenu'], ['静音', 'AudioVolumeMute'], ['音量-', 'AudioVolumeDown'], ['音量+', 'AudioVolumeUp'], ['播放/暂停', 'MediaPlayPause', 1.5]],
];
const MODS = new Set(['ShiftLeft', 'ShiftRight', 'ControlLeft', 'ControlRight', 'AltLeft', 'AltRight', 'MetaLeft']);
const modState = new Map(); // code -> 'latched' | 'held'
const modButtons = new Map();

function setMod(code, state) {
  const prev = modState.get(code);
  if (!prev && state) send({ t: 'kd', code });
  if (prev && !state) send({ t: 'ku', code });
  if (state) modState.set(code, state); else modState.delete(code);
  for (const b of modButtons.get(code) || []) {
    b.classList.toggle('latched', state === 'latched');
    b.classList.toggle('held', state === 'held');
  }
}

const kbd = $('kbd');
for (const row of KB) {
  const r = document.createElement('div');
  r.className = 'kb-row';
  for (const [label, code0, grow] of row) {
    const code = code0 || label;
    const b = document.createElement('button');
    b.textContent = label;
    if (grow) b.style.flexGrow = grow;
    if (MODS.has(code)) {
      // tap: one-shot; tap again: hold; tap again: release
      if (!modButtons.has(code)) modButtons.set(code, []);
      modButtons.get(code).push(b);
      b.onclick = () => {
        const s = modState.get(code);
        setMod(code, !s ? 'latched' : s === 'latched' ? 'held' : null);
      };
    } else {
      b.addEventListener('pointerdown', (e) => {
        e.preventDefault();
        if (!viewOnly()) send({ t: 'kd', code });
      });
      const up = () => {
        if (viewOnly()) return;
        send({ t: 'ku', code });
        for (const [m, s] of [...modState]) if (s === 'latched') setMod(m, null);
      };
      b.addEventListener('pointerup', up);
      b.addEventListener('pointerleave', (e) => { if (e.buttons) up(); });
    }
    r.appendChild(b);
  }
  kbd.appendChild(r);
}

// ---------- clipboard sync ----------
// lastClip: text both sides are known to hold, so a synced value never bounces back
let lastClip = null;
let clipReadDenied = false;
let pendingRemote = null; // remote text waiting for this tab to get focus
const clipIn = () => $('selClipIn').value;
const clipOut = () => $('selClipOut').value;
$('selClipIn').value = prefs.clipIn || 'ask';
$('selClipOut').value = prefs.clipOut || 'ask';
$('selClipIn').onchange = () => {
  prefs.clipIn = clipIn(); savePrefs();
  send({ t: 'clip_watch', on: clipIn() !== 'off' });
};
$('selClipOut').onchange = () => {
  prefs.clipOut = clipOut(); savePrefs();
  clipReadDenied = false;
  if (clipOut() !== 'off') checkLocalClip(true);
};

function preview(s) {
  const one = s.replace(/\s+/g, ' ').trim();
  return (one.length > 60 ? one.slice(0, 60) + '…' : one) + `（${s.length} 字）`;
}

let noticeAction = null;
function showNotice(text, okLabel, action) {
  $('clipNoticeText').textContent = text;
  $('clipNoticeOk').textContent = okLabel;
  noticeAction = action;
  $('clipNotice').classList.remove('hidden');
}
function hideNotice() { $('clipNotice').classList.add('hidden'); noticeAction = null; }
$('clipNoticeOk').onclick = async () => { const a = noticeAction; hideNotice(); if (a) await a(); stage.focus(); };
$('clipNoticeNo').onclick = () => { hideNotice(); stage.focus(); };

/** Writes text to the local clipboard; falls back to execCommand (works on plain http with a click). */
async function writeLocalClip(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.cssText = 'position:fixed;opacity:0;top:0;left:0';
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch { /* ignore */ }
    ta.remove();
    return ok;
  }
}

async function readLocalClip() {
  if (!navigator.clipboard || !navigator.clipboard.readText) return null;
  try {
    return await navigator.clipboard.readText();
  } catch {
    return null;
  }
}

function pushClip(text) {
  lastClip = text;
  send({ t: 'clip_set', s: text });
}

async function applyRemoteClip(text) {
  if (await writeLocalClip(text)) {
    lastClip = text;
    toast('已同步到本机剪贴板');
  } else {
    // needs a user gesture: ask for a click
    showNotice('远程剪贴板：' + preview(text), '复制到本机', async () => {
      if (await writeLocalClip(text)) { lastClip = text; toast('已复制到本机'); }
    });
  }
}

function onRemoteClip(text) {
  if (!text || text === lastClip) return;
  const mode = clipIn();
  if (mode === 'off') return;
  if (mode === 'ask') {
    showNotice('远程复制了：' + preview(text), '复制到本机', () => applyRemoteClip(text));
    return;
  }
  if (!document.hasFocus()) { pendingRemote = text; return; }
  applyRemoteClip(text);
}

let checking = false;
async function checkLocalClip(baselineOnly) {
  if (clipOut() === 'off' || clipReadDenied || checking || !document.hasFocus()) return;
  if (!navigator.clipboard || !navigator.clipboard.readText) return;
  checking = true;
  try {
    let text;
    try {
      text = await navigator.clipboard.readText();
    } catch (e) {
      if (e && e.name === 'NotAllowedError' && !document.hasFocus()) return;
      clipReadDenied = true;
      toast('浏览器不允许读取剪贴板：本机→远程只能手动同步或用 Ctrl+V');
      return;
    }
    if (lastClip === null || baselineOnly) { lastClip = lastClip ?? text; return; }
    if (!text || text === lastClip) return;
    if (clipOut() === 'auto') {
      pushClip(text);
      toast('已同步到远程剪贴板');
    } else {
      showNotice('本机复制了：' + preview(text), '发送到远程', () => { pushClip(text); toast('已发送到远程'); });
      lastClip = text; // ask once per new text
    }
  } finally {
    checking = false;
  }
}

window.addEventListener('focus', () => {
  if (pendingRemote) { const t = pendingRemote; pendingRemote = null; onRemoteClip(t); }
  checkLocalClip(false);
});
setInterval(() => checkLocalClip(false), 1000);
// remember what's on the local clipboard now, so only later copies get synced
setTimeout(() => checkLocalClip(true), 500);

// ---------- clipboard & text ----------
$('clipFetch').onclick = () => send({ t: 'clip_get' });
$('clipSend').onclick = () => { pushClip($('clipText').value); toast('已写入远程剪贴板'); };
$('clipLocalRead').onclick = async () => {
  try { $('clipText').value = await navigator.clipboard.readText(); } catch { toast('浏览器不允许读取剪贴板（需要 HTTPS 并授权）'); }
};
$('clipLocalWrite').onclick = async () => {
  const t = $('clipText').value;
  if (await writeLocalClip(t)) { lastClip = t; toast('已复制到本地'); } else toast('复制失败');
};
$('typeSend').onclick = () => {
  const s = $('typeText').value;
  if (s) send({ t: 'txt', s });
  $('typeText').value = '';
  stage.focus();
};

// ---------- files ----------
let curDir = '';
const api = (url, body) => fetch(url, {
  method: 'POST',
  headers: { 'content-type': 'application/json', 'x-ezc': '1' },
  body: JSON.stringify(body),
}).then(async (r) => { if (!r.ok) throw new Error(await r.text()); return r; });

function joinPath(dir, name) {
  return dir.endsWith('\\') ? dir + name : dir + '\\' + name;
}
function fmtSize(n) {
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i ? n.toFixed(1) : n) + ' ' + u[i];
}

async function loadDir(path) {
  const list = $('fList');
  list.textContent = '加载中…';
  const r = await fetch('/api/files/list?path=' + encodeURIComponent(path), { cache: 'no-store' });
  if (!r.ok) { list.textContent = '打开失败: ' + await r.text(); return; }
  const d = await r.json();
  curDir = d.path;
  prefs.dir = curDir; savePrefs();
  $('fPath').value = curDir;
  $('fUp').dataset.parent = d.parent ?? '';
  $('fUp').disabled = d.parent == null;
  list.innerHTML = '';
  if (!d.entries.length) list.textContent = '（空）';
  for (const ent of d.entries) {
    const full = curDir ? joinPath(curDir, ent.name) : ent.name;
    const row = document.createElement('div');
    row.className = 'fitem';
    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = (ent.dir ? '📁 ' : '📄 ') + ent.name;
    name.title = full;
    const meta = document.createElement('span');
    meta.className = 'meta';
    meta.textContent = ent.dir ? '' : fmtSize(ent.size);
    const acts = document.createElement('span');
    acts.className = 'acts';
    if (ent.dir) {
      name.onclick = () => loadDir(full);
    } else {
      const dl = document.createElement('a');
      dl.href = '/api/files/download?path=' + encodeURIComponent(full);
      dl.download = ent.name;
      name.onclick = () => dl.click();
      const b = document.createElement('button');
      b.textContent = '下载';
      b.onclick = () => dl.click();
      acts.appendChild(b);
    }
    if (curDir) {
      const rn = document.createElement('button');
      rn.textContent = '改名';
      rn.onclick = async () => {
        const n = prompt('新名称', ent.name);
        if (!n || n === ent.name) return;
        try { await api('/api/files/rename', { from: full, to: joinPath(curDir, n) }); loadDir(curDir); } catch (e) { alert(e.message); }
      };
      const del = document.createElement('button');
      del.textContent = '删除';
      del.onclick = async () => {
        if (!confirm(`永久删除 ${full}${ent.dir ? ' 及其全部内容' : ''}？此操作不可恢复。`)) return;
        try { await api('/api/files/delete', { path: full }); loadDir(curDir); } catch (e) { alert(e.message); }
      };
      acts.append(rn, del);
    }
    row.append(name, meta, acts);
    list.appendChild(row);
  }
}
$('fGo').onclick = () => loadDir($('fPath').value.trim());
$('fPath').addEventListener('keydown', (e) => { if (e.key === 'Enter') loadDir($('fPath').value.trim()); });
$('fUp').onclick = () => loadDir($('fUp').dataset.parent || '');
$('fRefresh').onclick = () => loadDir(curDir);
$('fMkdir').onclick = async () => {
  if (!curDir) return alert('请先进入一个磁盘目录');
  const n = prompt('文件夹名称');
  if (!n) return;
  try { await api('/api/files/mkdir', { path: joinPath(curDir, n) }); loadDir(curDir); } catch (e) { alert(e.message); }
};
$('fUpload').onclick = () => { if (!curDir) return alert('请先进入一个磁盘目录'); $('fInput').click(); };
$('fInput').onchange = () => { uploadFiles([...$('fInput').files]); $('fInput').value = ''; };

const drop = $('fDrop');
drop.addEventListener('dragover', (e) => { e.preventDefault(); drop.classList.add('over'); });
drop.addEventListener('dragleave', () => drop.classList.remove('over'));
drop.addEventListener('drop', (e) => {
  e.preventDefault();
  drop.classList.remove('over');
  if (!curDir) return alert('请先进入一个磁盘目录');
  uploadFiles([...e.dataTransfer.files]);
});

const CHUNK = 16 * 1024 * 1024; // keeps each request well under Cloudflare's 100 MB limit

function putChunk(url, blob, onProgress) {
  return new Promise((resolve, reject) => {
    const x = new XMLHttpRequest();
    x.open('POST', url);
    x.setRequestHeader('x-ezc', '1');
    x.upload.onprogress = (e) => onProgress(e.loaded);
    x.onload = () => (x.status === 200 ? resolve() : reject(new Error(x.responseText || x.status)));
    x.onerror = () => reject(new Error('网络错误'));
    x.send(blob);
  });
}

async function uploadFiles(files) {
  const dir = curDir;
  for (const f of files) {
    const job = document.createElement('div');
    job.className = 'job';
    job.innerHTML = `<div></div><progress max="1" value="0"></progress>`;
    const label = job.firstChild;
    const bar = job.lastChild;
    $('fJobs').appendChild(job);
    label.textContent = `上传 ${f.name}`;
    const target = joinPath(dir, f.name);
    let overwrite = false;
    const t0 = performance.now();
    try {
      for (let off = 0; off < f.size || off === 0; off += CHUNK) {
        const end = Math.min(f.size, off + CHUNK);
        const url = `/api/files/upload?path=${encodeURIComponent(target)}&offset=${off}`
          + `&done=${end >= f.size}&overwrite=${overwrite}`;
        try {
          await putChunk(url, f.slice(off, end), (n) => {
            bar.value = f.size ? (off + n) / f.size : 1;
            const mbps = ((off + n) / 1048576) / ((performance.now() - t0) / 1000);
            label.textContent = `上传 ${f.name} · ${mbps.toFixed(1)} MB/s`;
          });
        } catch (e) {
          if (off === 0 && /已存在/.test(e.message) && confirm(`${f.name} 已存在，是否覆盖？`)) {
            overwrite = true;
            off -= CHUNK;
            continue;
          }
          throw e;
        }
        if (f.size === 0) break;
      }
      label.textContent = `✓ ${f.name}`;
      setTimeout(() => job.remove(), 3000);
    } catch (e) {
      label.textContent = `✗ ${f.name}: ${e.message}`;
    }
  }
  if (curDir === dir) loadDir(dir);
}

// ---------- start ----------
stage.focus();
detectH264().then((ok) => {
  h264Ok = ok;
  const opt = $('selCodec').querySelector('option[value="h264"]');
  if (!ok) {
    opt.disabled = true;
    opt.textContent = 'H.264（此浏览器/连接不支持）';
    $('selCodec').value = 'jpeg';
  }
  connect();
});
