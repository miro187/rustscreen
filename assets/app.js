/* RustScreen web client — no build step, no external dependencies. */
(() => {
  'use strict';

  // ── DOM ───────────────────────────────────────────────────────
  const $ = (sel, root = document) => root.querySelector(sel);
  const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

  const el = {
    loginView: $('#login-view'),
    loginForm: $('#login-form'),
    pin: $('#pin'),
    loginButton: $('#login-button'),
    loginError: $('#login-error'),
    loginHost: $('#login-host'),
    loginNote: $('#login-note'),
    badgeTls: $('#badge-tls'),
    badgeControl: $('#badge-control'),
    loginVersion: $('#login-version'),

    viewer: $('#viewer-view'),
    stage: $('#stage'),
    canvas: $('#screen-canvas'),
    video: $('#screen-video'),
    audio: $('#stream-audio'),
    overlay: $('#overlay'),
    overlaySpinner: $('#overlay-spinner'),
    overlayIcon: $('#overlay-icon'),
    overlayText: $('#overlay-text'),
    overlayRetry: $('#overlay-retry'),
    controlPill: $('#control-pill'),
    controlHint: $('#control-hint'),
    actualHint: $('#actual-hint'),

    hostName: $('#host-name'),
    statusText: $('#status-text'),
    connDot: $('#conn-dot'),
    connText: $('#conn-text'),
    hud: $('#hud'),
    hudSize: $('#hud-size'),
    hudFps: $('#hud-fps'),
    hudBitrate: $('#hud-bitrate'),
    hudLatency: $('#hud-latency'),
    hudTransport: $('#hud-transport'),

    btnSettings: $('#btn-settings'),
    btnControl: $('#btn-control'),
    btnType: $('#btn-type'),
    btnAudio: $('#btn-audio'),
    btnFit: $('#btn-fit'),
    btnFullscreen: $('#btn-fullscreen'),
    btnCloseSettings: $('#btn-close-settings'),
    panel: $('#settings-panel'),
    scrim: $('#panel-scrim'),

    presetRow: $('#preset-row'),
    sourceSelect: $('#source-select'),
    sourceThumb: $('#source-thumb'),
    thumbPlaceholder: $('#thumb-placeholder'),
    btnRefreshSources: $('#btn-refresh-sources'),
    fpsRange: $('#fps-range'),
    fpsValue: $('#fps-value'),
    bitrateRange: $('#bitrate-range'),
    bitrateValue: $('#bitrate-value'),
    scaleRange: $('#scale-range'),
    scaleValue: $('#scale-value'),
    encoderSelect: $('#encoder-select'),
    btnApplyStream: $('#btn-apply-stream'),
    streamNote: $('#stream-note'),

    transportSelect: $('#transport-select'),
    toggleHud: $('#toggle-hud'),
    toggleFit: $('#toggle-fit'),
    toggleAudio: $('#toggle-audio'),
    audioToggleRow: $('#audio-toggle-row'),

    infoHost: $('#info-host'),
    infoClient: $('#info-client'),
    infoControl: $('#info-control'),
    infoViewers: $('#info-viewers'),
    infoVersion: $('#info-version'),
    infoSecurity: $('#info-security'),
    certLink: $('#cert-link'),
    btnLogout: $('#btn-logout'),

    typeModal: $('#type-modal'),
    typeText: $('#type-text'),
    btnSendType: $('#btn-send-type'),
    btnCancelType: $('#btn-cancel-type'),
    btnPasteLocal: $('#btn-paste-local'),

    toasts: $('#toasts'),
  };

  // ── State ─────────────────────────────────────────────────────
  const state = {
    preauth: null,
    info: null,
    settings: { source: 'primary', fps: 30, bitrateKbps: 6000, scale: 1, encoder: null },
    encoders: [],
    sources: null,
    myName: '',
    controlMode: 'all',
    holder: null,
    wantControl: false,
    controlOn: false,
    controlReady: false,
    controlWs: null,
    controlRetry: 500,
    controlRetryTimer: 0,
    rtt: 0,
    lastPing: 0,

    videoGen: 0,
    videoWs: null,
    videoAbort: null,
    videoStopFn: null,
    videoDecoder: null,
    videoReader: null,
    mseUrl: null,
    videoMse: null,
    activeTransport: null,
    forcedMse: false,
    webcodecsSupported: false,
    live: false,
    framesDecoded: 0,
    lastFramesAt: 0,
    lastFrames: 0,
    clientFps: 0,
    serverBitrate: 0,
    streamWidth: 0,
    streamHeight: 0,
    vfcToken: 0,

    pressed: null,
    touches: new Map(),
    touchScroll: null,
    lastInput: null,
    lastPointer: null,
    moveRaf: 0,
    pendingMove: null,
    keysDown: new Set(),
    wheelAcc: { x: 0, y: 0 },
    wheelTimer: 0,
    mouseOverStage: false,
    hintTimer: 0,

    panelOpen: false,
    statsTimer: 0,
    pingTimer: 0,
    fpsTimer: 0,
    idleTimer: 0,
    audioOn: false,
    audio: null,
    preferences: {
      transport: 'auto',
      fit: 'fit',
      hud: true,
    },
  };

  const prefsKey = 'rustscreen.prefs';

  // ── Small helpers ─────────────────────────────────────────────
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

  function loadPreferences() {
    try {
      const raw = localStorage.getItem(prefsKey);
      if (!raw) return;
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed === 'object') {
        if (['auto', 'webcodecs', 'mse'].includes(parsed.transport)) state.preferences.transport = parsed.transport;
        if (['fit', 'actual'].includes(parsed.fit)) state.preferences.fit = parsed.fit;
        if (typeof parsed.hud === 'boolean') state.preferences.hud = parsed.hud;
      }
    } catch { /* localStorage can be unavailable */ }
  }

  function savePreferences() {
    try { localStorage.setItem(prefsKey, JSON.stringify(state.preferences)); } catch { /* ignore */ }
  }

  function formatBitrate(kbps) {
    if (!kbps || kbps < 1) return '—';
    if (kbps >= 1000) return `${(kbps / 1000).toFixed(1)} Mb/s`;
    return `${Math.round(kbps)} kb/s`;
  }

  function wsUrl(path) {
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${scheme}//${location.host}${path}`;
  }

  async function fetchJson(path, options = {}) {
    const response = await fetch(path, { cache: 'no-store', ...options });
    let data = null;
    try { data = await response.json(); } catch { /* empty body */ }
    if (!response.ok) {
      const err = new Error((data && data.error) || `Request failed (${response.status})`);
      err.status = response.status;
      if (response.status === 401 && !options.silent401) handleUnauthorized();
      throw err;
    }
    return data;
  }

  function handleUnauthorized() {
    if (el.viewer.hidden) return;
    showLogin('Your session expired. Enter the PIN again.');
  }

  function toast(message, kind = 'info', ttl = 3800) {
    if (!message) return;
    const node = document.createElement('div');
    node.className = `toast ${kind}`;
    node.textContent = message;
    el.toasts.appendChild(node);
    while (el.toasts.children.length > 4) el.toasts.removeChild(el.toasts.firstChild);
    setTimeout(() => {
      node.classList.add('leaving');
      setTimeout(() => node.remove(), 260);
    }, ttl);
  }

  function setStatus(text, kind = '') {
    el.statusText.textContent = text;
    el.statusText.className = kind;
  }

  function updateConn(kind, text) {
    el.connDot.className = `conn-dot ${kind}`;
    el.connText.textContent = text;
  }

  function showOverlay(text, { spinner = true, retry = false } = {}) {
    el.overlay.hidden = false;
    el.overlay.classList.remove('fade');
    el.overlayText.textContent = text;
    el.overlaySpinner.hidden = !spinner;
    el.overlayIcon.hidden = spinner;
    el.overlayRetry.hidden = !retry;
  }

  function hideOverlay() {
    el.overlay.classList.add('fade');
    setTimeout(() => {
      if (state.live) el.overlay.hidden = true;
    }, 250);
  }

  function setStreamSize(width, height) {
    if (!width || !height) return;
    if (state.streamWidth === width && state.streamHeight === height) return;
    state.streamWidth = width;
    state.streamHeight = height;
    paintHud();
  }

  function paintHud() {
    el.hudSize.textContent = state.streamWidth ? `${state.streamWidth}×${state.streamHeight}` : '—';
    el.hudFps.textContent = state.clientFps ? `${state.clientFps} fps` : '—';
    el.hudBitrate.textContent = formatBitrate(state.serverBitrate);
    el.hudLatency.textContent = state.rtt ? `${Math.round(state.rtt)} ms` : '—';
    el.hudTransport.textContent =
      state.activeTransport === 'webcodecs' ? 'WebCodecs' :
      state.activeTransport === 'mse' ? 'MSE' : '—';
    el.hud.hidden = !state.preferences.hud;
  }

  function activeScreen() {
    return el.canvas.hidden ? el.video : el.canvas;
  }

  function showCanvas() {
    el.canvas.hidden = false;
    el.video.hidden = true;
  }

  function showVideo() {
    el.canvas.hidden = true;
    el.video.hidden = false;
  }

  function markActive() {
    el.viewer.classList.remove('idle');
    clearTimeout(state.idleTimer);
    if (state.panelOpen || !el.typeModal.hidden) return;
    state.idleTimer = setTimeout(() => {
      if (!state.panelOpen) el.viewer.classList.add('idle');
    }, 3600);
  }

  // ── Sign in ───────────────────────────────────────────────────
  function showLogin(message = '') {
    teardownAll();
    el.viewer.hidden = true;
    el.loginView.hidden = false;
    el.loginError.textContent = message;
    el.pin.value = '';
    setTimeout(() => el.pin.focus(), 60);
  }

  function applyPreauth(preauth) {
    state.preauth = preauth;
    el.loginHost.textContent = `${preauth.hostname} · ${preauth.os}`;
    el.badgeTls.hidden = false;
    el.badgeTls.className = `badge ${preauth.tls ? 'ok' : 'warn'}`;
    el.badgeTls.textContent = preauth.tls ? 'Encrypted (TLS)' : 'Unencrypted HTTP';
    el.badgeControl.hidden = false;
    el.badgeControl.className = `badge ${preauth.controlMode === 'none' ? 'warn' : 'info'}`;
    el.badgeControl.textContent =
      preauth.controlMode === 'none' ? 'View only' :
      preauth.controlMode === 'single' ? 'Single controller' : 'Remote control available';
    el.loginNote.textContent = preauth.authRequired
      ? 'Ask the person at the host computer for the PIN shown in their terminal.'
      : 'This host does not require a PIN. Click Connect to start.';
    el.loginVersion.textContent = `v${preauth.version}`;
    el.loginVersion.classList.remove('hidden');
    if (!preauth.authRequired) {
      el.pin.placeholder = 'No PIN required';
    }
  }

  async function login(password, { silent = false } = {}) {
    if (state.loggingIn) return;
    state.loggingIn = true;
    el.loginButton.disabled = true;
    el.loginButton.querySelector('span').textContent = 'Connecting…';
    el.loginError.textContent = '';
    try {
      await fetchJson('/api/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password }),
        silent401: true,
      });
      await enterViewer();
    } catch (err) {
      if (!silent) {
        el.loginError.textContent = err.message;
        el.pin.classList.remove('shake');
        void el.pin.offsetWidth;
        el.pin.classList.add('shake');
        el.pin.select();
      }
    } finally {
      state.loggingIn = false;
      el.loginButton.disabled = false;
      el.loginButton.querySelector('span').textContent = 'Connect';
    }
  }

  // ── Enter viewer ──────────────────────────────────────────────
  async function enterViewer() {
    const info = await fetchJson('/api/info', { silent401: true });
    state.info = info;
    state.controlMode = info.control.mode;
    state.holder = info.control.holder;
    state.settings = { ...info.settings };
    state.encoders = info.encoders || [];
    state.serverBitrate = 0;
    state.activeTransport = null;
    state.live = false;
    state.forcedMse = false;
    state.controlOn = false;
    state.wantControl = false;

    el.loginView.hidden = true;
    el.viewer.hidden = false;
    el.hostName.textContent = info.hostname;
    setStatus('Connecting…');
    updateConn('warn', 'connecting');

    document.title = `${info.hostname} — RustScreen`;
    applyInfo(info);
    syncSettingsUI();
    syncPreferencesUI();

    connectControl();
    startVideo();
    startTimers();
    markActive();
    el.stage.focus({ preventScroll: true });
  }

  function applyInfo(info) {
    el.infoHost.textContent = `${info.hostname} · ${info.os}`;
    el.infoClient.textContent = info.session.clientName;
    el.infoControl.textContent =
      info.control.mode === 'none' ? 'View only' :
      info.control.mode === 'single' ? (info.control.holder ? `${info.control.holder} is controlling` : 'One controller at a time') :
      'All viewers may control';
    el.infoViewers.textContent = `${info.viewers} / ${info.maxViewers}`;
    el.infoVersion.textContent = `v${info.version} · ffmpeg ${info.ffmpegVersion}`;
    el.infoSecurity.textContent = info.tls ? 'TLS encrypted' : 'Unencrypted (HTTP)';
    el.certLink.hidden = !info.tls;

    el.audioToggleRow.hidden = !info.audio.enabled;
    el.btnAudio.hidden = !info.audio.enabled;
    el.audioToggleRow.querySelector('span').textContent = info.audio.device
      ? `Play remote audio (${info.audio.device})`
      : 'Play remote audio';

    const locked = info.control.mode === 'none';
    el.btnApplyStream.disabled = locked;
    el.btnControl.disabled = locked;
    el.btnType.disabled = locked;
    if (locked) {
      el.btnControl.querySelector('span').textContent = 'View only';
      el.btnControl.title = 'Remote control is disabled on this host';
      el.btnType.title = 'Remote input is disabled on this host';
    }
    $$('#settings-panel .range, #source-select, #encoder-select, .preset').forEach((node) => {
      node.disabled = locked;
    });
    el.streamNote.textContent = locked
      ? 'Control is disabled on this host, so stream settings are locked.'
      : 'Changing these restarts the host stream for every viewer.';
    el.streamNote.classList.toggle('warn', locked);

    el.encoderSelect.innerHTML = '';
    for (const encoder of state.encoders) {
      const option = document.createElement('option');
      option.value = encoder;
      option.textContent = encoder;
      el.encoderSelect.appendChild(option);
    }
    if (state.settings.encoder) el.encoderSelect.value = state.settings.encoder;

    paintHud();
    updateControlUI();
  }

  function syncPreferencesUI() {
    el.transportSelect.value = state.preferences.transport;
    el.toggleHud.checked = state.preferences.hud;
    el.toggleFit.checked = state.preferences.fit === 'fit';
    el.hud.hidden = !state.preferences.hud;
    el.stage.classList.toggle('actual', state.preferences.fit === 'actual');
    el.actualHint.hidden = state.preferences.fit !== 'actual';
    el.btnFit.querySelector('span').textContent = state.preferences.fit === 'fit' ? 'Fit' : 'Actual';
  }

  // ── Control channel ───────────────────────────────────────────
  function connectControl() {
    clearTimeout(state.controlRetryTimer);
    if (state.controlWs) {
      try {
        state.controlWs.onclose = null;
        state.controlWs.onerror = null;
        state.controlWs.onmessage = null;
        state.controlWs.close();
      } catch { /* ignore */ }
      state.controlWs = null;
    }
    let ws;
    try {
      ws = new WebSocket(wsUrl('/api/control'));
    } catch {
      scheduleControlReconnect();
      return;
    }
    state.controlWs = ws;
    ws.onopen = () => {
      state.controlReady = true;
      state.controlRetry = 500;
      updateConn('ok', 'live');
      sendPing();
      if (state.wantControl) sendRaw({ t: 'controlRequest' });
    };
    ws.onmessage = (event) => {
      let msg;
      try { msg = JSON.parse(event.data); } catch { return; }
      onControlMessage(msg);
    };
    ws.onclose = () => {
      state.controlReady = false;
      if (state.controlOn) {
        state.controlOn = false;
        updateControlUI();
      }
      updateConn('bad', 'offline');
      scheduleControlReconnect();
    };
    ws.onerror = () => { /* onclose handles it */ };
  }

  function scheduleControlReconnect() {
    if (el.viewer.hidden) return;
    clearTimeout(state.controlRetryTimer);
    const delay = state.controlRetry;
    state.controlRetry = Math.min(Math.round(state.controlRetry * 1.7), 5000);
    state.controlRetryTimer = setTimeout(connectControl, delay);
  }

  function onControlMessage(msg) {
    switch (msg.type) {
      case 'hello':
        state.myName = msg.name || state.myName;
        state.controlMode = msg.controlMode || state.controlMode;
        state.holder = msg.holder || null;
        if (state.info) {
          state.info.control = { mode: state.controlMode, holder: state.holder };
          applyControlInfo();
        }
        if (state.controlMode === 'none') {
          state.wantControl = false;
          state.controlOn = false;
          updateControlUI();
        } else if (state.wantControl) {
          sendRaw({ t: 'controlRequest' });
        }
        break;
      case 'pong': {
        const sent = Number(msg.ts);
        if (Number.isFinite(sent) && sent > 0) state.rtt = Math.max(0, Math.round(performance.now() - sent));
        paintHud();
        break;
      }
      case 'control': {
        state.holder = msg.holder || null;
        if (msg.mode) state.controlMode = msg.mode;
        if (msg.you) {
          state.controlOn = true;
          state.wantControl = false;
        } else if (state.controlMode === 'single' && state.controlOn) {
          state.controlOn = false;
          toast(`${msg.holder || 'Another viewer'} took control`, 'info');
        }
        applyControlInfo();
        updateControlUI();
        break;
      }
      case 'controlAck': {
        state.controlOn = !!msg.granted;
        state.wantControl = false;
        if (msg.granted) toast('You have control. Click the screen, then use your keyboard.', 'ok');
        updateControlUI();
        break;
      }
      case 'controlDenied': {
        state.controlOn = false;
        state.wantControl = false;
        toast(msg.reason || 'Control is not available', 'error');
        updateControlUI();
        break;
      }
      case 'error':
        toast(msg.message || 'Server error', 'error');
        break;
      default:
        break;
    }
  }

  function applyControlInfo() {
    if (!state.info) return;
    state.info.control = { mode: state.controlMode, holder: state.holder };
    el.infoControl.textContent =
      state.controlMode === 'none' ? 'View only' :
      state.controlMode === 'single' ? (state.holder ? `${state.holder} is controlling` : 'One controller at a time') :
      'All viewers may control';
  }

  function sendRaw(message) {
    const ws = state.controlWs;
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    try { ws.send(JSON.stringify(message)); return true; } catch { return false; }
  }

  function sendPing() {
    state.lastPing = Math.round(performance.now());
    sendRaw({ t: 'ping', ts: state.lastPing });
  }

  function canSendControl() {
    return state.controlOn && state.controlReady && !el.viewer.hidden;
  }

  function sendInput(message) {
    if (!canSendControl()) return false;
    return sendRaw(message);
  }

  function enableControl() {
    if (state.controlMode === 'none') {
      toast('The host has disabled remote control.', 'error');
      return;
    }
    if (!state.controlReady) {
      state.wantControl = true;
      toast('Still connecting to the host…', 'info');
      return;
    }
    state.wantControl = true;
    sendRaw({ t: 'controlRequest' });
    if (state.controlMode === 'all') {
      state.controlOn = true;
      updateControlUI();
    }
    el.stage.focus({ preventScroll: true });
  }

  function disableControl() {
    state.wantControl = false;
    const wasOn = state.controlOn;
    state.controlOn = false;
    if (state.controlReady) {
      sendRaw({ t: 'controlRelease' });
      sendRaw({ t: 'releaseAll' });
    }
    releaseLocalKeys();
    updateControlUI();
    if (wasOn) toast('Control released', 'info', 1800);
  }

  function updateControlUI() {
    el.btnControl.classList.toggle('active', state.controlOn);
    el.stage.classList.toggle('controlling', state.controlOn);
    el.controlPill.hidden = !state.controlOn;
    el.audioToggleRow.hidden = !(state.info && state.info.audio.enabled);
    if (state.controlOn) {
      el.controlHint.hidden = true;
    } else {
      updateControlHint();
    }
  }

  function updateControlHint() {
    const eligible =
      state.live &&
      !state.controlOn &&
      state.controlMode !== 'none' &&
      !state.panelOpen &&
      el.typeModal.hidden &&
      state.mouseOverStage;
    el.controlHint.hidden = !eligible;
  }

  function releaseLocalKeys() {
    if (state.keysDown.size && state.controlOn) {
      for (const code of state.keysDown) sendInput({ t: 'key', code, down: false });
    }
    state.keysDown.clear();
    if (state.pressed && state.controlOn) {
      sendInput({ t: 'up', button: buttonName(state.pressed.button) });
    }
    state.pressed = null;
    sendRaw({ t: 'releaseAll' });
  }

  // ── Video transports ──────────────────────────────────────────
  async function detectWebCodecs() {
    if (!('VideoDecoder' in window) || !('EncodedVideoChunk' in window)) return false;
    try {
      const support = await VideoDecoder.isConfigSupported({
        codec: 'avc1.42E01E',
        avc: { format: 'annexb' },
        optimizeForLatency: true,
      });
      return !!support.supported;
    } catch {
      return false;
    }
  }

  function pickTransport() {
    if (state.preferences.transport === 'mse' || state.forcedMse) return 'mse';
    if (state.preferences.transport === 'webcodecs') return 'webcodecs';
    return state.webcodecsSupported ? 'webcodecs' : 'mse';
  }

  function currentFrameCount() {
    // requestVideoFrameCallback (used for MSE) and the WebCodecs output callback
    // both increment framesDecoded. getVideoPlaybackQuality is a fallback for
    // browsers without rVFC.
    if (
      state.activeTransport === 'mse' &&
      typeof el.video.requestVideoFrameCallback !== 'function' &&
      typeof el.video.getVideoPlaybackQuality === 'function'
    ) {
      try { return el.video.getVideoPlaybackQuality().totalVideoFrames; } catch { /* ignore */ }
    }
    return state.framesDecoded;
  }

  function stopVideo() {
    state.videoGen++;
    state.vfcToken++;
    const stopFn = state.videoStopFn;
    state.videoStopFn = null;
    if (stopFn) {
      try { stopFn(); } catch { /* ignore */ }
    }
    if (state.videoAbort) {
      try { state.videoAbort.abort(); } catch { /* ignore */ }
      state.videoAbort = null;
    }
    if (state.videoWs) {
      const ws = state.videoWs;
      state.videoWs = null;
      try {
        ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null;
        ws.close();
      } catch { /* ignore */ }
    }
    if (state.videoDecoder) {
      try { state.videoDecoder.close(); } catch { /* ignore */ }
      state.videoDecoder = null;
    }
    if (state.mseUrl) {
      URL.revokeObjectURL(state.mseUrl);
      state.mseUrl = null;
    }
    state.videoMse = null;
    state.live = false;
    try {
      el.video.pause();
      el.video.removeAttribute('src');
      el.video.load();
    } catch { /* ignore */ }
    el.video.hidden = true;
    el.canvas.hidden = true;
    const ctx = state.ctx;
    if (ctx) { try { ctx.clearRect(0, 0, el.canvas.width, el.canvas.height); } catch { /* ignore */ } }
  }

  async function startVideo() {
    stopVideo();
    state.lastFramesAt = 0;
    state.lastFrames = 0;
    const generation = state.videoGen;
    let attempt = 0;

    while (generation === state.videoGen) {
      const mode = pickTransport();
      state.live = false;
      setStatus('Connecting…');
      updateConn('warn', 'connecting');
      showOverlay('Connecting to the host screen…');
      let err = null;
      try {
        if (mode === 'webcodecs') await runWebCodecs(generation);
        else await runMse(generation);
        if (generation !== state.videoGen) return;
        err = new Error('Stream restarted');
      } catch (error) {
        if (generation !== state.videoGen) return;
        err = error || new Error('Connection failed');
      }

      if (generation !== state.videoGen) return;
      if (err && err.status === 401) { handleUnauthorized(); return; }
      if (err && err.fallback && state.preferences.transport === 'auto' && !state.forcedMse) {
        state.forcedMse = true;
        toast('WebCodecs is unavailable here — switching to Media Source playback.', 'info', 5000);
        attempt = 0;
        continue;
      }

      attempt++;
      setStatus('Reconnecting…', 'error');
      updateConn(attempt > 1 ? 'bad' : 'warn', attempt > 1 ? 'offline' : 'reconnecting');
      showOverlay(
        attempt <= 1 ? 'Reconnecting…' : `${err.message || 'Connection lost'} — retrying…`,
        { spinner: true, retry: attempt >= 2 }
      );
      state.live = false;
      await sleep(Math.min(600 * 1.8 ** Math.min(attempt - 1, 4), 7000));
    }
  }

  function runWebCodecs(generation) {
    return new Promise((resolve, reject) => {
      let ws;
      try {
        ws = new WebSocket(wsUrl('/api/ws'));
      } catch (err) {
        reject(err);
        return;
      }
      ws.binaryType = 'arraybuffer';
      state.videoWs = ws;

      let decoder = null;
      let fps = 30;
      let configured = false;
      let pending = [];
      let settled = false;

      const cleanup = () => {
        if (state.videoWs === ws) state.videoWs = null;
        if (state.videoDecoder === decoder) state.videoDecoder = null;
        try {
          ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null;
          ws.close();
        } catch { /* ignore */ }
        if (decoder) {
          try { decoder.close(); } catch { /* ignore */ }
        }
        decoder = null;
      };
      const fail = (err) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(err);
      };
      const finish = () => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve();
      };
      state.videoStopFn = finish;

      const drawFrame = (frame) => {
        const canvas = el.canvas;
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
          setStreamSize(frame.displayWidth, frame.displayHeight);
        }
        const ctx = state.ctx || (state.ctx = canvas.getContext('2d', { alpha: false, desynchronized: true }));
        if (ctx) ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
        state.framesDecoded++;
        onFirstFrame('webcodecs');
      };

      const feed = (key, ts, data) => {
        if (!configured || !decoder) {
          if (pending.length < 150) pending.push({ key, ts, data });
          return;
        }
        try {
          decoder.decode(new EncodedVideoChunk({
            type: key ? 'key' : 'delta',
            timestamp: ts,
            duration: Math.round(1_000_000 / fps),
            data,
          }));
        } catch (err) {
          fail(Object.assign(new Error(`Decoder rejected a frame: ${err.message}`), { fallback: true }));
        }
      };

      const configure = async (msg) => {
        fps = msg.fps || 30;
        setStreamSize(msg.width, msg.height);
        const codec = msg.codec || 'avc1.42E01E';
        const config = {
          codec,
          optimizeForLatency: true,
          avc: { format: 'annexb' },
          hardwareAcceleration: 'no-preference',
        };
        let support;
        try {
          support = await VideoDecoder.isConfigSupported(config);
        } catch (err) {
          fail(Object.assign(new Error(`This browser cannot decode ${codec}`), { fallback: true }));
          return;
        }
        if (!support || !support.supported) {
          fail(Object.assign(new Error(`This browser cannot decode ${codec}`), { fallback: true }));
          return;
        }
        if (settled) return;
        if (decoder) {
          try { decoder.close(); } catch { /* ignore */ }
          decoder = null;
        }
        decoder = new VideoDecoder({
          output(frame) {
            drawFrame(frame);
            frame.close();
          },
          error(err) {
            fail(Object.assign(new Error(`Video decoder failed: ${err.message}`), { fallback: true }));
          },
        });
        decoder.configure(config);
        configured = true;
        state.videoDecoder = decoder;
        state.activeTransport = 'webcodecs';
        showCanvas();
        paintHud();
        const queued = pending;
        pending = [];
        for (const item of queued) feed(item.key, item.ts, item.data);
      };

      ws.onopen = () => { /* config arrives as the first message */ };
      ws.onerror = () => fail(new Error('Network error while connecting to the stream'));
      ws.onclose = () => fail(new Error('The video connection was closed'));
      ws.onmessage = (event) => {
        if (settled || generation !== state.videoGen) return;
        if (typeof event.data === 'string') {
          let msg;
          try { msg = JSON.parse(event.data); } catch { return; }
          if (msg.type === 'config') configure(msg);
          else if (msg.type === 'reset') fail(new Error(msg.reason || 'Stream restarted'));
          else if (msg.type === 'error') fail(new Error(msg.message || 'Stream error'));
          return;
        }
        const buffer = event.data;
        const bytes = new Uint8Array(buffer);
        if (bytes.length < 17 || bytes[0] !== 0x52 || bytes[1] !== 0x53) return;
        const keyframe = (bytes[3] & 1) === 1;
        const view = new DataView(buffer, 8, 8);
        const ts = Number(view.getBigUint64(0, true));
        feed(keyframe, ts, bytes.slice(16));
      };
    });
  }

  function runMse(generation) {
    return new Promise((resolve, reject) => {
      let settled = false;
      let cleanupFns = [];
      const cleanup = () => {
        for (const fn of cleanupFns) { try { fn(); } catch { /* ignore */ } }
        cleanupFns = [];
        if (state.videoStopFn === finish) state.videoStopFn = null;
      };
      const fail = (err) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(err);
      };
      const finish = () => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve();
      };
      state.videoStopFn = finish;

      (async () => {
        const abort = new AbortController();
        state.videoAbort = abort;
        cleanupFns.push(() => abort.abort());

        const response = await fetch('/api/stream', { cache: 'no-store', signal: abort.signal });
        if (!response.ok) {
          let message = `Stream unavailable (HTTP ${response.status})`;
          try { const body = await response.json(); if (body && body.error) message = body.error; } catch { /* ignore */ }
          const err = new Error(message);
          err.status = response.status;
          throw err;
        }
        if (!response.body) throw new Error('Streaming is not supported by this browser');

        const codec = response.headers.get('x-video-codec') || 'avc1.42E01E';
        const width = Number(response.headers.get('x-video-width')) || 0;
        const height = Number(response.headers.get('x-video-height')) || 0;
        setStreamSize(width, height);

        if (!('MediaSource' in window)) throw new Error('Media Source Extensions are not supported');
        const mime = `video/mp4; codecs="${codec}"`;
        if (!MediaSource.isTypeSupported(mime)) {
          throw new Error(`This browser cannot play ${codec}`);
        }

        const mediaSource = new MediaSource();
        state.videoMse = mediaSource;
        if (state.mseUrl) URL.revokeObjectURL(state.mseUrl);
        state.mseUrl = URL.createObjectURL(mediaSource);
        const video = el.video;
        video.src = state.mseUrl;
        showVideo();
        state.activeTransport = 'mse';
        paintHud();

        await new Promise((resolveOpen, rejectOpen) => {
          const timer = setTimeout(() => rejectOpen(new Error('Timed out starting the video player')), 12000);
          mediaSource.addEventListener('sourceopen', () => { clearTimeout(timer); resolveOpen(); }, { once: true });
          video.addEventListener('error', () => {
            clearTimeout(timer);
            rejectOpen(new Error('The browser rejected the video stream'));
          }, { once: true });
        });
        if (settled) return;

        const sourceBuffer = mediaSource.addSourceBuffer(mime);
        sourceBuffer.mode = 'segments';

        let queue = [];
        let queuedBytes = 0;
        let appending = false;

        const trimLatency = () => {
          if (video.readyState < 2) return;
          const buffered = video.buffered;
          if (!buffered.length) return;
          const end = buffered.end(buffered.length - 1);
          if (end - video.currentTime > 0.9) {
            try { video.currentTime = Math.max(buffered.start(0), end - 0.25); } catch { /* ignore */ }
          }
          // Keep memory bounded on long sessions.
          if (!sourceBuffer.updating && buffered.start(0) < video.currentTime - 6) {
            try {
              sourceBuffer.remove(buffered.start(0), Math.max(buffered.start(0), video.currentTime - 3));
            } catch { /* ignore */ }
          }
        };

        const pump = () => {
          if (settled || appending || queue.length === 0) return;
          if (sourceBuffer.updating) return;
          const chunk = queue.shift();
          queuedBytes -= chunk.byteLength;
          appending = true;
          try {
            sourceBuffer.appendBuffer(chunk);
          } catch (err) {
            appending = false;
            if (err && err.name === 'QuotaExceededError') {
              try {
                const b = video.buffered;
                if (b.length) sourceBuffer.remove(b.start(0), Math.max(b.start(0), video.currentTime - 0.5));
              } catch { /* ignore */ }
              queue.unshift(chunk);
              queuedBytes += chunk.byteLength;
              setTimeout(pump, 250);
              return;
            }
            fail(err);
          }
        };

        const ensurePlaying = () => {
          if (video.paused) video.play().catch(() => { /* user gesture may be required */ });
        };

        sourceBuffer.addEventListener('updateend', () => {
          appending = false;
          trimLatency();
          pump();
          ensurePlaying();
        });
        const trimTimer = setInterval(trimLatency, 900);
        cleanupFns.push(() => clearInterval(trimTimer));

        const token = ++state.vfcToken;
        const countFrame = () => {
          if (settled || token !== state.vfcToken) return;
          state.framesDecoded++;
          onFirstFrame('mse');
          try { video.requestVideoFrameCallback(countFrame); } catch { /* ignore */ }
        };
        if (typeof video.requestVideoFrameCallback === 'function') {
          video.requestVideoFrameCallback(countFrame);
        } else {
          const onTimeUpdate = () => { if (video.currentTime > 0) onFirstFrame('mse'); };
          video.addEventListener('timeupdate', onTimeUpdate);
          cleanupFns.push(() => video.removeEventListener('timeupdate', onTimeUpdate));
        }

        const reader = response.body.getReader();
        state.videoReader = reader;
        cleanupFns.push(() => { try { reader.cancel(); } catch { /* ignore */ } });

        for (;;) {
          const { value, done } = await reader.read();
          if (settled) return;
          if (done) break;
          if (!value || !value.byteLength) continue;
          queue.push(value);
          queuedBytes += value.byteLength;
          if (queuedBytes > 14 * 1024 * 1024) throw new Error('The stream fell too far behind — reconnecting');
          pump();
        }
        throw new Error('The host stopped the stream');
      })().catch((err) => {
        if (generation !== state.videoGen) return;
        fail(err);
      });
    });
  }

  function onFirstFrame(transport) {
    if (!state.live) {
      state.live = true;
      setStatus('Live', 'live');
      updateConn('ok', 'live');
      hideOverlay();
      state.activeTransport = transport;
      paintHud();
      updateControlHint();
    }
  }

  // ── Pointer & keyboard input ──────────────────────────────────
  function pointerNorm(event, target) {
    const rect = target.getBoundingClientRect();
    if (!rect.width || !rect.height) return null;
    const x = (event.clientX - rect.left) / rect.width;
    const y = (event.clientY - rect.top) / rect.height;
    if (x < -0.02 || x > 1.02 || y < -0.02 || y > 1.02) return null;
    return { x: clamp(x, 0, 1), y: clamp(y, 0, 1) };
  }

  function buttonName(button) {
    switch (button) {
      case 1: return 'middle';
      case 2: return 'right';
      case 3: return 'back';
      case 4: return 'forward';
      default: return 'left';
    }
  }

  function flushMove() {
    state.moveRaf = 0;
    const point = state.pendingMove;
    state.pendingMove = null;
    if (!point) return;
    state.lastInput = { x: Number(point.x.toFixed(4)), y: Number(point.y.toFixed(4)), t: 'move' };
    sendInput({ t: 'move', x: point.x, y: point.y });
  }

  function averageTouch() {
    const points = Array.from(state.touches.values());
    if (!points.length) return { x: 0, y: 0 };
    const sum = points.reduce((acc, p) => ({ x: acc.x + p.x, y: acc.y + p.y }), { x: 0, y: 0 });
    return { x: sum.x / points.length, y: sum.y / points.length };
  }

  function onPointerDown(event) {
    markActive();
    if (event.pointerType === 'touch') {
      state.touches.set(event.pointerId, { x: event.clientX, y: event.clientY });
      if (state.touches.size >= 2) {
        // Two fingers scroll instead of dragging the remote pointer.
        if (state.pressed) {
          sendInput({ t: 'up', button: buttonName(state.pressed.button) });
          state.pressed = null;
        }
        state.touchScroll = averageTouch();
        event.preventDefault();
        return;
      }
    }
    if (!state.controlOn) {
      if (state.controlMode !== 'none' && el.typeModal.hidden) enableControl();
      return;
    }
    if (!el.typeModal.hidden) return;
    const point = pointerNorm(event, activeScreen());
    if (!point) return;
    event.preventDefault();
    try { el.stage.setPointerCapture(event.pointerId); } catch { /* ignore */ }
    state.pressed = { button: event.button, pointerId: event.pointerId };
    sendInput({ t: 'down', button: buttonName(event.button), x: point.x, y: point.y });
  }

  function onPointerMove(event) {
    markActive();
    state.mouseOverStage = true;
    updateControlHint();
    if (event.pointerType === 'touch' && state.touches.has(event.pointerId)) {
      state.touches.set(event.pointerId, { x: event.clientX, y: event.clientY });
      if (state.touchScroll && state.touches.size >= 2) {
        const avg = averageTouch();
        state.wheelAcc.x += (state.touchScroll.x - avg.x) * 2.4;
        state.wheelAcc.y += (state.touchScroll.y - avg.y) * 2.4;
        state.touchScroll = avg;
        if (!state.wheelTimer) state.wheelTimer = setTimeout(flushWheel, 60);
        event.preventDefault();
        return;
      }
    }
    const rect = activeScreen().getBoundingClientRect();
    state.lastPointer = {
      clientX: Math.round(event.clientX * 10) / 10,
      clientY: Math.round(event.clientY * 10) / 10,
      rect: { x: rect.x, y: rect.y, w: rect.width, h: rect.height },
      controlOn: state.controlOn,
    };
    if (!state.controlOn) return;
    const point = pointerNorm(event, activeScreen());
    if (!point) return;
    state.pendingMove = point;
    if (!state.moveRaf) state.moveRaf = requestAnimationFrame(flushMove);
  }

  function onPointerUp(event) {
    state.mouseOverStage = true;
    if (event.pointerType === 'touch') {
      state.touches.delete(event.pointerId);
      if (state.touches.size < 2) state.touchScroll = null;
    }
    if (state.pressed && event.pointerId === state.pressed.pointerId) {
      const button = state.pressed.button;
      state.pressed = null;
      sendInput({ t: 'up', button: buttonName(button) });
      try { el.stage.releasePointerCapture(event.pointerId); } catch { /* ignore */ }
    }
    updateControlHint();
  }

  function flushWheel() {
    state.wheelTimer = 0;
    const { x, y } = state.wheelAcc;
    state.wheelAcc = { x: 0, y: 0 };
    if (!x && !y) return;
    const toLines = (value) => {
      if (!value) return 0;
      const lines = Math.round(Math.abs(value) / 100);
      return Math.sign(value) * clamp(lines || 1, 1, 10);
    };
    sendInput({ t: 'wheel', dx: toLines(x), dy: toLines(y) });
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
    if (!state.controlOn || !state.controlReady) return false;
    if (state.panelOpen || !el.typeModal.hidden) return false;
    const active = document.activeElement;
    if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA' || active.tagName === 'SELECT' || active.isContentEditable)) {
      return false;
    }
    return true;
  }

  async function pasteToRemote() {
    try {
      const text = await navigator.clipboard.readText();
      if (!text) { toast('Clipboard is empty', 'info'); return; }
      sendRaw({ t: 'text', text });
      toast('Clipboard sent to the host', 'ok', 2200);
    } catch {
      toast('Clipboard access was blocked. Use “Send text” instead.', 'error');
    }
  }

  function isLocalShortcut(event) {
    if (event.code === 'F5' || (event.ctrlKey && event.code === 'KeyR')) return true;
    if (event.code === 'F11' || event.code === 'F12') return true;
    if (event.ctrlKey && event.shiftKey && (event.code === 'KeyI' || event.code === 'KeyJ' || event.code === 'KeyC')) return true;
    return false;
  }

  function onKeyDown(event) {
    if (!keyboardTargetOk()) return;
    if (event.key === 'Escape') {
      if (document.fullscreenElement) return; // let the browser leave fullscreen
    }
    if (isLocalShortcut(event)) return;
    if (!event.code) return;
    if (event.code === 'KeyV' && (event.ctrlKey || event.metaKey) && !event.shiftKey) {
      event.preventDefault();
      pasteToRemote();
      return;
    }
    if (event.code === 'KeyC' && (event.ctrlKey || event.metaKey) && event.shiftKey) {
      event.preventDefault();
      pasteToRemote();
      return;
    }
    if (event.isComposing) return;
    event.preventDefault();
    sendInput({ t: 'key', code: event.code, down: true });
    state.keysDown.add(event.code);
  }

  function onKeyUp(event) {
    if (!state.keysDown.has(event.code)) return;
    state.keysDown.delete(event.code);
    event.preventDefault();
    sendInput({ t: 'key', code: event.code, down: false });
  }

  function onBlur() {
    if (state.controlOn) releaseLocalKeys();
  }

  // ── Fullscreen / fit / settings drawer ────────────────────────
  function toggleFullscreen() {
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => {});
    } else {
      const target = el.viewer;
      (target.requestFullscreen || target.webkitRequestFullscreen)?.call(target).catch(() => {
        toast('Fullscreen was blocked by the browser', 'error');
      });
    }
  }

  function openPanel() {
    state.panelOpen = true;
    el.panel.hidden = false;
    el.scrim.hidden = false;
    el.viewer.classList.remove('idle');
    if (!state.sources) loadSources();
    updateControlHint();
    markActive();
  }

  function closePanel() {
    state.panelOpen = false;
    el.panel.hidden = true;
    el.scrim.hidden = true;
    markActive();
    updateControlHint();
  }

  function openTypeModal() {
    if (state.controlMode === 'none') {
      toast('The host has disabled remote input.', 'error');
      return;
    }
    el.typeModal.hidden = false;
    el.typeText.value = '';
    setTimeout(() => el.typeText.focus(), 50);
    updateControlHint();
  }

  function closeTypeModal() {
    el.typeModal.hidden = true;
    el.stage.focus({ preventScroll: true });
    updateControlHint();
  }

  async function sendTypedText() {
    const text = el.typeText.value;
    if (!text) { closeTypeModal(); return; }
    if (!state.controlReady) {
      toast('Still connecting to the host…', 'error');
      return;
    }
    sendRaw({ t: 'text', text });
    toast('Text sent', 'ok', 2000);
    closeTypeModal();
  }

  async function pasteIntoModal() {
    try {
      const text = await navigator.clipboard.readText();
      el.typeText.value = text;
    } catch {
      toast('Clipboard access was blocked', 'error');
    }
  }

  // ── Settings ──────────────────────────────────────────────────
  function syncSettingsUI() {
    const settings = state.settings;
    el.fpsRange.value = settings.fps;
    el.bitrateRange.value = settings.bitrateKbps;
    el.scaleRange.value = Math.round(settings.scale * 100);
    updateSliderLabels();
    if (state.encoders.length) {
      el.encoderSelect.value = settings.encoder || state.encoders[0];
    }
    markPreset();
    if (state.sources) {
      if (settings.source && !Array.from(el.sourceSelect.options).some((o) => o.value === settings.source)) {
        const option = document.createElement('option');
        option.value = settings.source;
        option.textContent = `${settings.source} (unavailable)`;
        el.sourceSelect.appendChild(option);
      }
      el.sourceSelect.value = settings.source;
      loadThumbnail(settings.source);
    }
  }

  function updateSliderLabels() {
    el.fpsValue.textContent = `${el.fpsRange.value} fps`;
    el.bitrateValue.textContent = formatBitrate(Number(el.bitrateRange.value));
    el.scaleValue.textContent = `${el.scaleRange.value}%`;
  }

  function markPreset() {
    const fps = Number(el.fpsRange.value);
    const bitrate = Number(el.bitrateRange.value);
    const scale = Number(el.scaleRange.value);
    const presets = {
      smooth: fps === 30 && bitrate === 2500 && scale === 60,
      balanced: fps === 30 && bitrate === 6000 && scale === 100,
      sharp: fps === 60 && bitrate === 12000 && scale === 100,
    };
    $$('.preset', el.presetRow).forEach((button) => {
      button.classList.toggle('active', !!presets[button.dataset.preset]);
    });
  }

  async function loadSources(force = false) {
    if (state.sources && !force) return;
    el.btnRefreshSources.disabled = true;
    el.btnRefreshSources.textContent = 'Loading…';
    try {
      const data = await fetchJson('/api/sources');
      state.sources = data;
      const select = el.sourceSelect;
      select.innerHTML = '';
      const addGroup = (label, items) => {
        if (!items || !items.length) return;
        const group = document.createElement('optgroup');
        group.label = label;
        for (const item of items) {
          const option = document.createElement('option');
          option.value = item.id;
          option.textContent = item.app ? `${item.name} — ${item.app}` : item.name;
          group.appendChild(option);
        }
        select.appendChild(group);
      };
      addGroup('Displays', data.monitors);
      addGroup('Windows', data.windows);
      const wanted = state.settings.source || data.current;
      if (wanted && !Array.from(select.options).some((option) => option.value === wanted)) {
        const option = document.createElement('option');
        option.value = wanted;
        option.textContent = `${wanted} (unavailable)`;
        select.appendChild(option);
      }
      if (wanted) select.value = wanted;
      loadThumbnail(select.value || wanted);
    } catch (err) {
      toast(`Could not list sources: ${err.message}`, 'error');
    } finally {
      el.btnRefreshSources.disabled = false;
      el.btnRefreshSources.textContent = 'Refresh';
    }
  }

  function loadThumbnail(sourceId) {
    if (!sourceId) return;
    el.thumbPlaceholder.hidden = false;
    el.thumbPlaceholder.textContent = 'Loading preview…';
    el.sourceThumb.hidden = true;
    const url = `/api/thumbnail?id=${encodeURIComponent(sourceId)}&w=480&t=${Date.now()}`;
    const img = new Image();
    img.onload = () => {
      el.sourceThumb.src = url;
      el.sourceThumb.hidden = false;
      el.thumbPlaceholder.hidden = true;
    };
    img.onerror = () => {
      el.thumbPlaceholder.textContent = 'Preview unavailable';
    };
    img.src = url;
  }

  async function applyStreamSettings() {
    const patch = {
      source: el.sourceSelect.value || state.settings.source,
      fps: Number(el.fpsRange.value),
      bitrateKbps: Number(el.bitrateRange.value),
      scale: Number(el.scaleRange.value) / 100,
    };
    if (el.encoderSelect.value) patch.encoder = el.encoderSelect.value;
    el.btnApplyStream.disabled = true;
    el.btnApplyStream.textContent = 'Applying…';
    try {
      const data = await fetchJson('/api/settings', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patch),
      });
      state.settings = { ...data.settings };
      state.forcedMse = false;
      toast('Stream settings updated', 'ok');
      syncSettingsUI();
      stopVideo();
      showOverlay('Restarting the stream…');
      setStatus('Restarting…');
      await sleep(300);
      startVideo();
    } catch (err) {
      toast(err.message, 'error', 5000);
    } finally {
      el.btnApplyStream.disabled = state.controlMode === 'none';
      el.btnApplyStream.textContent = 'Apply & restart stream';
    }
  }

  function selectPreset(name) {
    const presets = {
      smooth: { fps: 30, bitrate: 2500, scale: 60 },
      balanced: { fps: 30, bitrate: 6000, scale: 100 },
      sharp: { fps: 60, bitrate: 12000, scale: 100 },
    };
    const preset = presets[name];
    if (!preset) return;
    el.fpsRange.value = preset.fps;
    el.bitrateRange.value = preset.bitrate;
    el.scaleRange.value = preset.scale;
    updateSliderLabels();
    markPreset();
  }

  function setTransport(value) {
    state.preferences.transport = value;
    state.forcedMse = false;
    savePreferences();
    toast('Reconnecting with the new transport…', 'info', 2400);
    stopVideo();
    startVideo();
  }

  // ── Remote audio (optional) ───────────────────────────────────
  function updateAudioUI() {
    el.toggleAudio.checked = state.audioOn;
    el.btnAudio.classList.toggle('active', state.audioOn);
    el.btnAudio.querySelector('span').textContent = state.audioOn ? 'Audio on' : 'Audio';
  }

  async function startAudio() {
    if (state.audioOn || !state.info || !state.info.audio.enabled) return;
    try {
      const response = await fetch('/api/audio', { cache: 'no-store' });
      if (!response.ok) {
        let message = `Audio unavailable (HTTP ${response.status})`;
        try { const body = await response.json(); if (body && body.error) message = body.error; } catch { /* ignore */ }
        throw new Error(message);
      }
      const codec = response.headers.get('x-audio-codec') || 'mp4a.40.2';
      const mime = `audio/mp4; codecs="${codec}"`;
      if (!('MediaSource' in window) || !MediaSource.isTypeSupported(mime)) {
        throw new Error('AAC audio is not supported by this browser');
      }
      const audio = el.audio;
      const mediaSource = new MediaSource();
      const url = URL.createObjectURL(mediaSource);
      const abort = new AbortController();
      state.audio = { abort, url, mediaSource };
      audio.src = url;
      audio.muted = false;
      audio.volume = 0.9;

      await new Promise((resolveOpen, rejectOpen) => {
        const timer = setTimeout(() => rejectOpen(new Error('Timed out starting audio')), 12000);
        mediaSource.addEventListener('sourceopen', () => { clearTimeout(timer); resolveOpen(); }, { once: true });
      });
      const sourceBuffer = mediaSource.addSourceBuffer(mime);
      sourceBuffer.mode = 'segments';
      let appending = false;
      let queue = [];
      const pump = () => {
        if (appending || !queue.length || sourceBuffer.updating) return;
        const chunk = queue.shift();
        appending = true;
        try { sourceBuffer.appendBuffer(chunk); } catch { appending = false; }
      };
      sourceBuffer.addEventListener('updateend', () => { appending = false; pump(); });
      audio.play().catch(() => { toast('Click “Audio” again to allow playback', 'info'); });
      state.audioOn = true;
      updateAudioUI();

      (async () => {
        const reader = response.body.getReader();
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          if (!value || !value.byteLength) continue;
          queue.push(value);
          if (queue.length > 100) queue.shift();
          pump();
        }
      })().catch(() => {}).finally(() => {
        if (state.audioOn) {
          stopAudio();
          toast('Remote audio stopped', 'info');
        }
      });
    } catch (err) {
      stopAudio();
      toast(`Audio unavailable: ${err.message}`, 'error', 5000);
    }
  }

  function stopAudio() {
    state.audioOn = false;
    if (state.audio) {
      try { state.audio.abort.abort(); } catch { /* ignore */ }
      try { URL.revokeObjectURL(state.audio.url); } catch { /* ignore */ }
      state.audio = null;
    }
    const audio = el.audio;
    try {
      audio.pause();
      audio.removeAttribute('src');
      audio.load();
    } catch { /* ignore */ }
    updateAudioUI();
  }

  // ── Timers ────────────────────────────────────────────────────
  function startTimers() {
    stopTimers();
    state.pingTimer = setInterval(() => {
      if (!el.viewer.hidden) sendPing();
    }, 3000);

    state.fpsTimer = setInterval(() => {
      const now = performance.now();
      const total = currentFrameCount();
      if (!state.lastFramesAt) { state.lastFramesAt = now; state.lastFrames = total; return; }
      const dt = (now - state.lastFramesAt) / 1000;
      if (dt < 0.8) return;
      state.clientFps = Math.max(0, Math.round((total - state.lastFrames) / dt));
      state.lastFrames = total;
      state.lastFramesAt = now;
      paintHud();
    }, 500);

    state.statsTimer = setInterval(async () => {
      if (el.viewer.hidden) return;
      try {
        const stats = await fetchJson('/api/stats', { silent401: true });
        state.serverBitrate = stats.bitrateKbps || 0;
        state.holder = stats.control.holder || null;
        if (stats.control.mode) state.controlMode = stats.control.mode;
        applyControlInfo();
        if (state.info) {
          state.info.viewers = stats.viewers;
          el.infoViewers.textContent = `${stats.viewers} / ${stats.maxViewers}`;
        }
        paintHud();
      } catch { /* transient */ }
    }, 3000);
  }

  function stopTimers() {
    clearInterval(state.pingTimer);
    clearInterval(state.fpsTimer);
    clearInterval(state.statsTimer);
    state.pingTimer = state.fpsTimer = state.statsTimer = 0;
  }

  async function logout() {
    try { await fetch('/api/logout', { method: 'POST' }); } catch { /* ignore */ }
    location.reload();
  }

  function teardownAll() {
    stopVideo();
    stopAudio();
    stopTimers();
    clearTimeout(state.controlRetryTimer);
    clearTimeout(state.wheelTimer);
    if (state.controlWs) {
      const ws = state.controlWs;
      state.controlWs = null;
      try {
        ws.onclose = null;
        ws.onmessage = null;
        ws.close();
      } catch { /* ignore */ }
    }
    state.controlReady = false;
    state.controlOn = false;
    state.wantControl = false;
    state.keysDown.clear();
    state.touches.clear();
    state.touchScroll = null;
    state.live = false;
    closePanel();
    if (!el.typeModal.hidden) closeTypeModal();
  }

  // ── Event wiring ──────────────────────────────────────────────
  function wireEvents() {
    // Sign in
    el.loginForm.addEventListener('submit', (event) => {
      event.preventDefault();
      const value = el.pin.value.trim();
      if (!value && state.preauth && state.preauth.authRequired) {
        el.loginError.textContent = 'Enter the PIN shown on the host screen.';
        el.pin.focus();
        return;
      }
      login(value);
    });
    el.pin.addEventListener('input', () => {
      const value = el.pin.value.trim();
      if (/^\d{6}$/.test(value) && !state.loggingIn) login(value);
    });
    el.pin.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') el.pin.blur();
    });

    // Top / bottom controls
    el.btnSettings.addEventListener('click', () => (state.panelOpen ? closePanel() : openPanel()));
    el.btnCloseSettings.addEventListener('click', closePanel);
    el.scrim.addEventListener('click', closePanel);
    el.btnControl.addEventListener('click', () => (state.controlOn ? disableControl() : enableControl()));
    el.controlPill.addEventListener('click', disableControl);
    el.btnType.addEventListener('click', openTypeModal);
    el.btnFullscreen.addEventListener('click', toggleFullscreen);
    el.btnFit.addEventListener('click', () => {
      state.preferences.fit = state.preferences.fit === 'fit' ? 'actual' : 'fit';
      savePreferences();
      syncPreferencesUI();
    });
    el.btnAudio.addEventListener('click', () => (state.audioOn ? stopAudio() : startAudio()));
    el.toggleAudio.addEventListener('change', () => {
      if (el.toggleAudio.checked) startAudio(); else stopAudio();
    });

    document.addEventListener('fullscreenchange', () => {
      el.btnFullscreen.querySelector('span').textContent = document.fullscreenElement ? 'Exit full screen' : 'Full screen';
    });

    // Stage interaction
    el.stage.addEventListener('pointerdown', onPointerDown);
    el.stage.addEventListener('pointermove', onPointerMove);
    el.stage.addEventListener('pointerup', onPointerUp);
    el.stage.addEventListener('pointercancel', (event) => {
      state.touches.delete(event.pointerId);
      state.touchScroll = null;
      if (state.pressed) {
        sendInput({ t: 'up', button: buttonName(state.pressed.button) });
        state.pressed = null;
      }
    });
    el.stage.addEventListener('pointerleave', () => {
      state.mouseOverStage = false;
      updateControlHint();
    });
    el.stage.addEventListener('pointerenter', () => {
      state.mouseOverStage = true;
      updateControlHint();
    });
    el.stage.addEventListener('wheel', onWheel, { passive: false });
    el.stage.addEventListener('contextmenu', (event) => event.preventDefault());
    el.stage.addEventListener('dragstart', (event) => event.preventDefault());

    // Keyboard
    window.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('keyup', onKeyUp, true);
    window.addEventListener('blur', onBlur);
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) onBlur();
    });

    // Overlay retry
    el.overlayRetry.addEventListener('click', () => {
      state.forcedMse = false;
      stopVideo();
      startVideo();
    });

    // Settings panel
    el.presetRow.addEventListener('click', (event) => {
      const button = event.target.closest('.preset');
      if (button) selectPreset(button.dataset.preset);
    });
    el.fpsRange.addEventListener('input', () => { updateSliderLabels(); markPreset(); });
    el.bitrateRange.addEventListener('input', () => { updateSliderLabels(); markPreset(); });
    el.scaleRange.addEventListener('input', () => { updateSliderLabels(); markPreset(); });
    el.btnApplyStream.addEventListener('click', applyStreamSettings);
    el.btnRefreshSources.addEventListener('click', () => loadSources(true));
    el.sourceSelect.addEventListener('change', () => loadThumbnail(el.sourceSelect.value));
    el.transportSelect.addEventListener('change', () => setTransport(el.transportSelect.value));
    el.toggleHud.addEventListener('change', () => {
      state.preferences.hud = el.toggleHud.checked;
      savePreferences();
      paintHud();
    });
    el.toggleFit.addEventListener('change', () => {
      state.preferences.fit = el.toggleFit.checked ? 'fit' : 'actual';
      savePreferences();
      syncPreferencesUI();
    });
    el.btnLogout.addEventListener('click', logout);

    // Text modal
    el.btnSendType.addEventListener('click', sendTypedText);
    el.btnCancelType.addEventListener('click', closeTypeModal);
    el.btnPasteLocal.addEventListener('click', pasteIntoModal);
    el.typeModal.addEventListener('click', (event) => {
      if (event.target === el.typeModal) closeTypeModal();
    });
    el.typeText.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
        event.preventDefault();
        sendTypedText();
      }
      if (event.key === 'Escape') closeTypeModal();
    });

    // Global escape for dialogs
    window.addEventListener('keydown', (event) => {
      if (event.key !== 'Escape') return;
      if (!el.typeModal.hidden) { closeTypeModal(); event.preventDefault(); return; }
      if (state.panelOpen) { closePanel(); event.preventDefault(); }
    });

    // Activity tracking for auto-hiding chrome
    el.viewer.addEventListener('pointermove', markActive);
    el.viewer.addEventListener('pointerdown', markActive);

    window.addEventListener('beforeunload', () => {
      if (state.controlOn) sendRaw({ t: 'releaseAll' });
    });

    // Keep-mse-live: some browsers pause a muted video when hidden
    el.video.addEventListener('pause', () => {
      if (!el.video.hidden && state.activeTransport === 'mse' && state.live) {
        setTimeout(() => { if (el.video.paused) el.video.play().catch(() => {}); }, 250);
      }
    });
  }

  // ── Boot ──────────────────────────────────────────────────────
  // Read-only diagnostics for support and automated tests.
  window.__rustscreen = {
    debug: () => ({
      live: state.live,
      transport: state.activeTransport,
      width: state.streamWidth,
      height: state.streamHeight,
      frames: state.framesDecoded,
      clientFps: state.clientFps,
      serverBitrateKbps: Math.round(state.serverBitrate),
      latencyMs: state.rtt,
      controlOn: state.controlOn,
      controlMode: state.controlMode,
      holder: state.holder,
      keysDown: state.keysDown.size,
      panelOpen: state.panelOpen,
      audioOn: state.audioOn,
      audio: {
        readyState: el.audio.readyState,
        currentTime: Number(el.audio.currentTime.toFixed(3)),
        paused: el.audio.paused,
      },
      lastInput: state.lastInput,
      lastPointer: state.lastPointer,
      video: {
        readyState: el.video.readyState,
        currentTime: Number(el.video.currentTime.toFixed(3)),
        paused: el.video.paused,
        width: el.video.videoWidth,
        height: el.video.videoHeight,
      },
    }),
    prefs: () => ({ ...state.preferences }),
    settings: () => ({ ...state.settings }),
  };

  async function boot() {
    loadPreferences();
    wireEvents();
    state.webcodecsSupported = await detectWebCodecs();

    let preauth = null;
    try {
      preauth = await fetchJson('/api/preauth');
      applyPreauth(preauth);
    } catch (err) {
      el.loginHost.textContent = 'Host unreachable';
      el.loginError.textContent = err.message;
      return;
    }

    let session = null;
    try {
      const response = await fetch('/api/session', { cache: 'no-store' });
      if (response.ok) session = await response.json();
    } catch { /* ignore */ }

    if (session) {
      try {
        await enterViewer();
        return;
      } catch (err) {
        showLogin(err.status === 401 ? 'Your session expired. Enter the PIN again.' : err.message);
        return;
      }
    }

    if (!preauth.authRequired) {
      await login('', { silent: true });
      if (el.viewer.hidden) showLogin('Could not connect automatically. Click Connect.');
      return;
    }
    showLogin();
  }

  boot();
})();
