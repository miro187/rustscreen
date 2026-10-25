/* RustScreen Connect — browser viewer for the RustScreen relay. */
(() => {
  'use strict';

  const $ = (sel, root = document) => root.querySelector(sel);
  const el = {
    connectView: $('#connect-view'),
    connectForm: $('#connect-form'),
    code: $('#code'),
    pin: $('#pin'),
    connectButton: $('#connect-button'),
    connectError: $('#connect-error'),
    viewer: $('#viewer-view'),
    stage: $('#stage'),
    canvas: $('#screen-canvas'),
    hostName: $('#host-name'),
    statusText: $('#status-text'),
    connDot: $('#conn-dot'),
    connText: $('#conn-text'),
    hud: $('#hud'),
    hudSize: $('#hud-size'),
    hudFps: $('#hud-fps'),
    hudLatency: $('#hud-latency'),
    hudTransport: $('#hud-transport'),
    overlay: $('#overlay'),
    overlaySpinner: $('#overlay-spinner'),
    overlayIcon: $('#overlay-icon'),
    overlayText: $('#overlay-text'),
    overlayRetry: $('#overlay-retry'),
    controlPill: $('#control-pill'),
    controlHint: $('#control-hint'),
    btnControl: $('#btn-control'),
    btnType: $('#btn-type'),
    btnFit: $('#btn-fit'),
    btnFullscreen: $('#btn-fullscreen'),
    btnDisconnect: $('#btn-disconnect'),
    typeModal: $('#type-modal'),
    typeText: $('#type-text'),
    btnSendType: $('#btn-send-type'),
    btnCancelType: $('#btn-cancel-type'),
    btnPasteLocal: $('#btn-paste-local'),
    toasts: $('#toasts'),
  };

  const state = {
    code: '',
    pin: '',
    ws: null,
    paired: false,
    connecting: false,
    manualClose: false,
    decoder: null,
    configured: false,
    pendingFrames: [],
    fps: 30,
    width: 0,
    height: 0,
    frames: 0,
    clientFps: 0,
    lastFrames: 0,
    lastFramesAt: 0,
    rtt: 0,
    live: false,
    controlOn: false,
    keysDown: new Set(),
    pressed: null,
    pendingMove: null,
    moveRaf: 0,
    wheelAcc: { x: 0, y: 0 },
    wheelTimer: 0,
    retryTimer: 0,
    retryAttempts: 0,
    pingTimer: 0,
    fpsTimer: 0,
    fit: 'fit',
  };

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
  const normalizeCode = (value) => value.replace(/\D/g, '').slice(0, 9);
  const prettyCode = (digits) => digits.replace(/(\d{3})(?=\d)/g, '$1 ').trim();

  function clientName() {
    const ua = navigator.userAgent.toLowerCase();
    const browser = ua.includes('edg/') ? 'Edge'
      : ua.includes('chrome') ? 'Chrome'
      : ua.includes('firefox') ? 'Firefox'
      : ua.includes('safari') ? 'Safari' : 'Browser';
    const os = ua.includes('android') ? 'Android'
      : ua.includes('iphone') || ua.includes('ipad') ? 'iOS'
      : ua.includes('windows') ? 'Windows'
      : ua.includes('mac os') ? 'macOS'
      : ua.includes('linux') ? 'Linux' : 'Unknown';
    return `${browser} on ${os}`;
  }

  function wsUrl() {
    const relay = new URLSearchParams(location.search).get('relay');
    if (relay) {
      const base = relay.trim().replace(/\/+$/, '');
      const ws = base.startsWith('https://') ? `wss://${base.slice(8)}`
        : base.startsWith('http://') ? `ws://${base.slice(7)}`
        : base;
      return `${ws}/viewer`;
    }
    const scheme = location.protocol === 'https:' ? 'wss://' : 'ws://';
    return `${scheme}${location.host}/viewer`;
  }

  function setStatus(text, kind = '') {
    el.statusText.textContent = text;
    el.statusText.className = kind;
  }

  function updateConn(kind, text) {
    el.connDot.className = `conn-dot ${kind}`;
    el.connText.textContent = text;
  }

  function toast(message, kind = 'info', ttl = 3200) {
    if (!message) return;
    const node = document.createElement('div');
    node.className = `toast ${kind}`;
    node.textContent = message;
    el.toasts.appendChild(node);
    while (el.toasts.children.length > 4) el.toasts.removeChild(el.toasts.firstChild);
    setTimeout(() => {
      node.classList.add('leaving');
      setTimeout(() => node.remove(), 240);
    }, ttl);
  }

  function showOverlay(text, { spinner = true, retry = false, icon = false } = {}) {
    el.overlay.hidden = false;
    el.overlay.classList.remove('fade');
    el.overlayText.textContent = text;
    el.overlaySpinner.hidden = !spinner;
    el.overlayIcon.hidden = !icon;
    el.overlayRetry.hidden = !retry;
  }

  function hideOverlay() {
    if (!state.live) return;
    el.overlay.classList.add('fade');
    setTimeout(() => { if (state.live) el.overlay.hidden = true; }, 220);
  }

  function showConnectError(message) {
    el.connectError.textContent = message || '';
  }

  function setConnectBusy(busy) {
    state.connecting = busy;
    el.connectButton.disabled = busy;
    el.connectButton.querySelector('span').textContent = busy ? 'Connecting…' : 'Connect';
  }

  function showConnectView(message = '') {
    teardownViewer();
    el.viewer.hidden = true;
    el.connectView.hidden = false;
    setConnectBusy(false);
    showConnectError(message);
    setTimeout(() => {
      if (el.code.value.replace(/\D/g, '').length === 9) el.pin.focus();
      else el.code.focus();
    }, 50);
  }

  function showViewer(host, source) {
    el.connectView.hidden = true;
    el.viewer.hidden = false;
    el.hostName.textContent = host || 'Remote computer';
    if (source) el.hostName.title = source;
    setStatus('Connecting…');
    updateConn('warn', 'connecting');
    showOverlay('Waiting for the host video…');
    paintHud();
  }

  function paintHud() {
    el.hudSize.textContent = state.width ? `${state.width}×${state.height}` : '—';
    el.hudFps.textContent = state.clientFps ? `${state.clientFps} fps` : '—';
    el.hudLatency.textContent = state.rtt ? `${Math.round(state.rtt)} ms` : '—';
    el.hudTransport.textContent = state.configured ? 'WebCodecs' : '—';
  }

  function startTimers() {
    stopTimers();
    state.pingTimer = setInterval(() => {
      if (!state.paired || !state.ws || state.ws.readyState !== WebSocket.OPEN) return;
      try {
        state.ws.send(JSON.stringify({ type: 'ping', ts: Math.round(performance.now()) }));
      } catch { /* ignore */ }
    }, 3000);
    state.fpsTimer = setInterval(() => {
      const now = performance.now();
      if (!state.lastFramesAt) {
        state.lastFramesAt = now;
        state.lastFrames = state.frames;
        return;
      }
      const dt = (now - state.lastFramesAt) / 1000;
      if (dt < 0.8) return;
      state.clientFps = Math.max(0, Math.round((state.frames - state.lastFrames) / dt));
      state.lastFrames = state.frames;
      state.lastFramesAt = now;
      paintHud();
    }, 500);
  }

  function stopTimers() {
    clearInterval(state.pingTimer);
    clearInterval(state.fpsTimer);
    state.pingTimer = state.fpsTimer = 0;
  }

  function closeSocket() {
    if (state.ws) {
      const ws = state.ws;
      state.ws = null;
      try {
        ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null;
        ws.close();
      } catch { /* ignore */ }
    }
    closeDecoder();
  }

  function teardownViewer() {
    clearTimeout(state.retryTimer);
    stopTimers();
    state.manualClose = true;
    closeSocket();
    state.paired = false;
    state.controlOn = false;
    state.live = false;
    state.keysDown.clear();
    state.pressed = null;
    state.wheelAcc = { x: 0, y: 0 };
    clearTimeout(state.wheelTimer);
    el.canvas.hidden = true;
    el.controlPill.hidden = true;
    el.stage.classList.remove('controlling', 'actual');
    updateConn('', 'offline');
  }

  function closeDecoder() {
    closeDecoderInternal();
  }

  function closeDecoderInternal() {
    state.configured = false;
    state.pendingFrames = [];
    if (state.decoder) {
      try { state.decoder.close(); } catch { /* ignore */ }
      state.decoder = null;
    }
  }

  function resetDecoder() {
    closeDecoderInternal();
    state.live = false;
    el.canvas.hidden = true;
  }

  function connect() {
    const code = normalizeCode(el.code.value);
    const pin = el.pin.value.trim();
    if (code.length !== 9) {
      showConnectError('Enter the 9-digit access code.');
      el.code.focus();
      return;
    }
    if (!pin) {
      showConnectError('Enter the PIN shown on the host.');
      el.pin.focus();
      return;
    }
    state.code = code;
    state.pin = pin;
    state.manualClose = false;
    state.retryAttempts = 0;
    showConnectError('');
    openSocket();
  }

  function openSocket() {
    if (state.manualClose) return;
    closeSocket();
    resetDecoder();
    setConnectBusy(true);
    let ws;
    try {
      ws = new WebSocket(wsUrl());
    } catch (err) {
      setConnectBusy(false);
      showConnectError('Could not open the connection.');
      return;
    }
    ws.binaryType = 'arraybuffer';
    state.ws = ws;

    ws.onopen = () => {
      try {
        ws.send(JSON.stringify({
          type: 'connect',
          code: state.code,
          pin: state.pin,
          name: clientName(),
        }));
      } catch { /* ignore */ }
    };
    ws.onmessage = onMessage;
    ws.onerror = () => { /* onclose handles it */ };
    ws.onclose = () => onSocketClose();
  }

  function onSocketClose() {
    const wasPaired = state.paired;
    state.paired = false;
    setConnectBusy(false);
    closeDecoderInternal();
    stopTimers();
    updateConn('bad', 'offline');
    if (state.manualClose) return;

    if (!wasPaired) {
      if (state.retryAttempts < 2) {
        state.retryAttempts += 1;
        clearTimeout(state.retryTimer);
        state.retryTimer = setTimeout(openSocket, 800 * state.retryAttempts);
        showConnectError('Trying to reach the relay…');
      } else {
        showConnectError('Could not reach the relay. Check the URL and try again.');
      }
      return;
    }

    // The stream or relay went away. Re-authenticate and resume.
    state.retryAttempts += 1;
    if (state.retryAttempts > 6) {
      showConnectView('Connection lost. Ask the host for a fresh code.');
      return;
    }
    setStatus('Reconnecting…', 'error');
    showOverlay('Connection lost — reconnecting…', { spinner: true });
    clearTimeout(state.retryTimer);
    const delay = Math.min(800 * 1.6 ** Math.min(state.retryAttempts, 4), 6000);
    state.retryTimer = setTimeout(openSocket, delay);
  }

  function onMessage(event) {
    if (typeof event.data === 'string') {
      let msg;
      try { msg = JSON.parse(event.data); } catch { return; }
      handleJson(msg);
      return;
    }
    const buffer = event.data;
    const bytes = new Uint8Array(buffer);
    if (bytes.length < 17 || bytes[0] !== 0x52 || bytes[1] !== 0x53) return;
    const keyframe = (bytes[3] & 1) === 1;
    const ts = Number(new DataView(buffer, 8, 8).getBigUint64(0, true));
    feedFrame(keyframe, ts, bytes.slice(16));
  }

  function handleJson(msg) {
    switch (msg.type) {
      case 'paired':
        state.paired = true;
        state.manualClose = false;
        state.retryAttempts = 0;
        setConnectBusy(false);
        showConnectError('');
        showViewer(msg.host, msg.source);
        startTimers();
        setStatus('Connected', 'live');
        break;
      case 'denied':
        state.manualClose = true;
        closeSocket();
        showConnectError(msg.reason || 'Connection denied.');
        showConnectView(msg.reason || 'Connection denied.');
        break;
      case 'config':
        configureDecoder(msg);
        break;
      case 'reset':
        showOverlay(msg.reason || 'Stream restarting…', { spinner: true });
        closeSocket();
        state.paired = false;
        clearTimeout(state.retryTimer);
        state.retryTimer = setTimeout(openSocket, 900);
        break;
      case 'error':
        state.manualClose = true;
        closeSocket();
        showConnectView(msg.message || 'The host went offline.');
        break;
      case 'pong': {
        const sent = Number(msg.ts);
        if (Number.isFinite(sent) && sent > 0) {
          state.rtt = Math.max(0, Math.round(performance.now() - sent));
          paintHud();
        }
        break;
      }
      default:
        break;
    }
  }

  async function configureDecoder(msg) {
    const codec = msg.codec || 'avc1.42E01E';
    state.fps = msg.fps || 30;
    state.width = msg.width || 0;
    state.height = msg.height || 0;
    paintHud();

    if (!('VideoDecoder' in window) || !('EncodedVideoChunk' in window)) {
      showOverlay('This browser does not support WebCodecs. Use Chrome or Edge.', {
        spinner: false, icon: true, retry: true,
      });
      return;
    }
    const config = {
      codec,
      optimizeForLatency: true,
      avc: { format: 'annexb' },
      hardwareAcceleration: 'no-preference',
    };
    let support = null;
    try {
      support = await VideoDecoder.isConfigSupported(config);
    } catch { /* ignore */ }
    if (!support || !support.supported) {
      showOverlay(`This browser cannot decode ${codec}. Use a recent Chrome or Edge.`, {
        spinner: false, icon: true, retry: true,
      });
      return;
    }
    closeDecoderInternal();
    try {
      state.decoder = new VideoDecoder({
        output(frame) {
          drawFrame(frame);
          frame.close();
        },
        error(err) {
          showOverlay(`Video decoder failed: ${err.message}`, { spinner: false, icon: true, retry: true });
        },
      });
      state.decoder.configure(config);
      state.configured = true;
      el.canvas.hidden = false;
      paintHud();
      const queued = state.pendingFrames;
      state.pendingFrames = [];
      for (const frame of queued) feedFrame(frame.key, frame.ts, frame.data);
    } catch (err) {
      showOverlay(`Video decoder failed: ${err.message}`, { spinner: false, icon: true, retry: true });
    }
  }

  function feedFrame(key, ts, data) {
    if (!state.configured || !state.decoder) {
      if (state.pendingFrames.length < 150) state.pendingFrames.push({ key, ts, data });
      return;
    }
    try {
      state.decoder.decode(new EncodedVideoChunk({
        type: key ? 'key' : 'delta',
        timestamp: ts,
        duration: Math.round(1_000_000 / state.fps),
        data,
      }));
    } catch {
      /* A single bad frame should not take down the viewer; the next keyframe
         recovers the stream. */
    }
  }

  let ctx = null;
  function drawFrame(frame) {
    const canvas = el.canvas;
    if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
      state.width = frame.displayWidth;
      state.height = frame.displayHeight;
      paintHud();
    }
    ctx = ctx || canvas.getContext('2d', { alpha: false, desynchronized: true });
    if (ctx) ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
    state.frames += 1;
    if (!state.live) {
      state.live = true;
      setStatus('Live', 'live');
      updateConn('ok', 'live');
      hideOverlay();
      updateHint();
    }
  }

  // ── Input ──────────────────────────────────────────────────────
  function send(message) {
    if (!state.ws || state.ws.readyState !== WebSocket.OPEN || !state.paired) return false;
    try {
      state.ws.send(JSON.stringify({ type: 'input', event: message }));
      return true;
    } catch {
      return false;
    }
  }

  function setControl(on) {
    state.controlOn = on;
    el.btnControl.classList.toggle('active', on);
    el.stage.classList.toggle('controlling', on);
    el.controlPill.hidden = !on;
    updateHint();
    if (on) el.stage.focus({ preventScroll: true });
    else releaseLocal();
  }

  function updateHint() {
    el.controlHint.hidden = !(state.live && !state.controlOn);
  }

  function releaseLocal() {
    for (const code of state.keysDown) send({ t: 'key', code, down: false });
    state.keysDown.clear();
    if (state.pressed) {
      send({ t: 'up', button: buttonName(state.pressed.button) });
      state.pressed = null;
    }
  }

  function pointerNorm(event) {
    const rect = el.canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return null;
    const x = (event.clientX - rect.left) / rect.width;
    const y = (event.clientY - rect.top) / rect.height;
    if (x < -0.02 || x > 1.02 || y < -0.02 || y > 1.02) return null;
    return { x: clamp(x, 0, 1), y: clamp(y, 0, 1) };
  }

  function buttonName(button) {
    return button === 1 ? 'middle' : button === 2 ? 'right' : button === 3 ? 'back' : button === 4 ? 'forward' : 'left';
  }

  function flushMove() {
    state.moveRaf = 0;
    const point = state.pendingMove;
    state.pendingMove = null;
    if (point) send({ t: 'move', x: point.x, y: point.y });
  }

  function onPointerDown(event) {
    if (!state.controlOn) {
      setControl(true);
      return;
    }
    if (!el.typeModal.hidden) return;
    const point = pointerNorm(event);
    if (!point) return;
    event.preventDefault();
    try { el.stage.setPointerCapture(event.pointerId); } catch { /* ignore */ }
    state.pressed = { button: event.button, pointerId: event.pointerId };
    send({ t: 'down', button: buttonName(event.button), x: point.x, y: point.y });
  }

  function onPointerMove(event) {
    if (!state.controlOn) return;
    const point = pointerNorm(event);
    if (!point) return;
    state.pendingMove = point;
    if (!state.moveRaf) state.moveRaf = requestAnimationFrame(flushMove);
  }

  function onPointerUp(event) {
    if (state.pressed && state.pressed.pointerId === event.pointerId) {
      const button = state.pressed.button;
      state.pressed = null;
      send({ t: 'up', button: buttonName(button) });
      try { el.stage.releasePointerCapture(event.pointerId); } catch { /* ignore */ }
    }
  }

  function flushWheel() {
    state.wheelTimer = 0;
    const { x, y } = state.wheelAcc;
    state.wheelAcc = { x: 0, y: 0 };
    if (!x && !y) return;
    const lines = (value) => {
      if (!value) return 0;
      return Math.sign(value) * clamp(Math.round(Math.abs(value) / 100) || 1, 1, 10);
    };
    send({ t: 'wheel', dx: lines(x), dy: lines(y) });
  }

  function onWheel(event) {
    if (!state.controlOn) return;
    event.preventDefault();
    const scale = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 400 : 1;
    state.wheelAcc.x += event.deltaX * scale;
    state.wheelAcc.y += event.deltaY * scale;
    if (!state.wheelTimer) state.wheelTimer = setTimeout(flushWheel, 90);
  }

  function keyboardTargetOk() {
    if (!state.controlOn) return false;
    if (!el.typeModal.hidden) return false;
    const active = document.activeElement;
    return !(active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA' || active.tagName === 'SELECT' || active.isContentEditable));
  }

  async function pasteToRemote() {
    try {
      const text = await navigator.clipboard.readText();
      if (!text) return toast('Clipboard is empty', 'info');
      send({ t: 'text', text });
      toast('Clipboard sent to the host', 'ok', 1800);
    } catch {
      toast('Clipboard access was blocked. Use “Send text”.', 'error');
    }
  }

  function onKeyDown(event) {
    if (!keyboardTargetOk()) return;
    if (event.code === 'F5' || event.code === 'F11' || event.code === 'F12') return;
    if (!event.code) return;
    if (event.code === 'KeyV' && (event.ctrlKey || event.metaKey) && !event.shiftKey) {
      event.preventDefault();
      pasteToRemote();
      return;
    }
    event.preventDefault();
    if (event.repeat) {
      send({ t: 'key', code: event.code, down: true });
      return;
    }
    send({ t: 'key', code: event.code, down: true });
    state.keysDown.add(event.code);
  }

  function onKeyUp(event) {
    if (!state.keysDown.has(event.code)) return;
    state.keysDown.delete(event.code);
    event.preventDefault();
    send({ t: 'key', code: event.code, down: false });
  }

  function openTypeModal() {
    el.typeModal.hidden = false;
    el.typeText.value = '';
    setTimeout(() => el.typeText.focus(), 50);
  }

  function closeTypeModal() {
    el.typeModal.hidden = true;
    el.stage.focus({ preventScroll: true });
  }

  function sendTypedText() {
    const text = el.typeText.value;
    if (text) send({ t: 'text', text });
    toast(text ? 'Text sent' : 'Nothing to send', text ? 'ok' : 'info', 1800);
    closeTypeModal();
  }

  async function pasteIntoModal() {
    try {
      el.typeText.value = await navigator.clipboard.readText();
    } catch {
      toast('Clipboard access was blocked', 'error');
    }
  }

  function toggleFullscreen() {
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => {});
    } else {
      el.viewer.requestFullscreen?.().catch(() => {
        toast('Fullscreen was blocked by the browser', 'error');
      });
    }
  }

  function toggleFit() {
    state.fit = state.fit === 'fit' ? 'actual' : 'fit';
    el.stage.classList.toggle('actual', state.fit === 'actual');
    el.btnFit.querySelector('span').textContent = state.fit === 'fit' ? 'Fit' : 'Actual';
  }

  function disconnect() {
    state.manualClose = true;
    closeSocket();
    state.paired = false;
    showConnectView();
  }

  // ── Wiring ─────────────────────────────────────────────────────
  function wire() {
    el.code.addEventListener('input', () => {
      const digits = normalizeCode(el.code.value);
      el.code.value = prettyCode(digits);
      if (digits.length === 9) el.pin.focus();
    });
    el.connectForm.addEventListener('submit', (event) => {
      event.preventDefault();
      connect();
    });
    el.overlayRetry.addEventListener('click', () => {
      state.manualClose = false;
      state.retryAttempts = 0;
      openSocket();
    });
    el.btnControl.addEventListener('click', () => setControl(!state.controlOn));
    el.controlPill.addEventListener('click', () => setControl(false));
    el.btnType.addEventListener('click', openTypeModal);
    el.btnFit.addEventListener('click', toggleFit);
    el.btnFullscreen.addEventListener('click', toggleFullscreen);
    el.btnDisconnect.addEventListener('click', disconnect);
    el.btnSendType.addEventListener('click', sendTypedText);
    el.btnCancelType.addEventListener('click', closeTypeModal);
    el.btnPasteLocal.addEventListener('click', pasteIntoModal);
    el.typeModal.addEventListener('click', (event) => {
      if (event.target === el.typeModal) closeTypeModal();
    });
    el.typeText.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) sendTypedText();
      if (event.key === 'Escape') closeTypeModal();
    });
    document.addEventListener('fullscreenchange', () => {
      el.btnFullscreen.querySelector('span').textContent = document.fullscreenElement ? 'Exit full screen' : 'Full screen';
    });

    el.stage.addEventListener('pointerdown', onPointerDown);
    el.stage.addEventListener('pointermove', onPointerMove);
    el.stage.addEventListener('pointerup', onPointerUp);
    el.stage.addEventListener('pointercancel', onPointerUp);
    el.stage.addEventListener('wheel', onWheel, { passive: false });
    el.stage.addEventListener('contextmenu', (event) => event.preventDefault());
    window.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('keyup', onKeyUp, true);
    window.addEventListener('blur', () => releaseLocal());
    document.addEventListener('visibilitychange', () => { if (document.hidden) releaseLocal(); });
    window.addEventListener('beforeunload', () => closeSocket());

    window.addEventListener('keydown', (event) => {
      if (event.key !== 'Escape') return;
      if (!el.typeModal.hidden) closeTypeModal();
    });
  }

  window.__rustscreenRelay = {
    debug: () => ({
      code: state.code,
      paired: state.paired,
      live: state.live,
      configured: state.configured,
      width: state.width,
      height: state.height,
      frames: state.frames,
      clientFps: state.clientFps,
      rtt: state.rtt,
      controlOn: state.controlOn,
      retries: state.retryAttempts,
    }),
  };

  function boot() {
    wire();
    const params = new URLSearchParams(location.search);
    const code = normalizeCode(params.get('code') || '');
    if (code) {
      el.code.value = prettyCode(code);
      el.pin.focus();
    } else {
      el.code.focus();
    }
    updateHint();
  }

  boot();
})();
