/* ============================================================================
 * qxync GUI 前端（vanilla ES2020，无打包器 / 无框架 / 无外部资源）
 *
 * 通道只有一个：window.__TAURI__.core.invoke(cmd, args)
 *   - Tauri v2 命令参数名是 camelCase（Rust 侧 snake_case）：link_id → linkId
 *   - 对象内部字段名保持后端 serde 的 snake_case
 *   - 所有来自 daemon 的字符串一律走 textContent / createElement，绝不拼 innerHTML
 * ==========================================================================*/
(function () {
  'use strict';

  // --------------------------------------------------------------- i18n（★ M8.6）
  // 文案表在 ui/i18n.js：zh-CN 是真值，en 预留、查不到就整条回落到 zh-CN。
  // 表没加载（旧包 / 被单测直接 eval）时退化成「返回 key」，界面不至于抛错。
  var I18N = (typeof window !== 'undefined' && window.QXNYC_I18N) ? window.QXNYC_I18N : null;
  function T(key, vars) {
    if (I18N && typeof I18N.t === 'function') { return I18N.t(key, vars); }
    return String(key);
  }

  // --------------------------------------------------------------- 常量
  var LOG_MAX = 200;              // 操作日志最多保留条数
  var POLL_MS = 2000;             // daemon_status 轮询间隔
  var PIN_STATES = ['unspecified', 'pinned', 'unpinned', 'excluded'];
  var PIN_LABEL = {
    unspecified: '未指定',
    pinned: '已固定',
    unpinned: '已取消固定',
    excluded: '已排除'
  };

  // ★ M8.1：一级目的地（左侧图标栏）。顺序 = 界面顺序。
  var PAGES = ['home', 'tasks', 'files', 'journal', 'errors', 'settings', 'diag'];
  var PAGE_TITLE = {
    home: T('page.home'),
    tasks: T('page.tasks'),
    files: T('page.files'),
    journal: T('page.journal'),
    errors: T('page.errors'),
    settings: T('page.settings'),
    diag: T('page.diag')
  };
  // 诊断页内的子 tab
  var DIAG_PANELS = ['status', 'mounts', 'sync'];
  // ★ M8.4：设置页内的分区（对齐 Qsync 的四个 tab + 连接/筛选/LAN/关于）
  var SETTINGS_SECS = ['connect', 'proxy', 'sync', 'personal', 'advanced', 'free', 'lan', 'about'];
  var SEC_TITLE = {
    connect: T('sec.connect'),
    proxy: T('sec.proxy'),
    sync: T('sec.sync'),
    personal: T('sec.personal'),
    advanced: T('sec.advanced'),
    free: T('sec.free'),
    lan: T('sec.lan'),
    about: T('sec.about')
  };
  // ★ 兼容：M8.1 之前 QXNYC_GUI_TAB 用的是这 5 个值，必须继续可用
  var LEGACY_TABS = {
    status: { page: 'diag', diag: 'status' },
    mounts: { page: 'diag', diag: 'mounts' },
    sync: { page: 'diag', diag: 'sync' },
    connect: { page: 'settings' },
    files: { page: 'files' }
  };
  var tauri = (typeof window !== 'undefined' && window.__TAURI__) ? window.__TAURI__ : null;
  var invokeFn = (tauri && tauri.core && typeof tauri.core.invoke === 'function')
    ? function (cmd, args) { return tauri.core.invoke(cmd, args); }
    : null;

  // --------------------------------------------------------------- 状态
  var state = {
    page: 'home',      // 一级目的地（PAGES）
    diag: 'status',    // 诊断页内的子 tab（DIAG_PANELS）
    files: {
      path: '/home',
      dir: '/home',
      entries: [],
      selected: null,
      loading: false,
      error: ''          // ★ M8.6：读取失败时的可见原因
    },
    mounts: [],
    tasks: null,        // ★ M8.2：tasks 请求的最近一次结果
    journal: null,      // ★ M8.3：journal 请求的最近一次结果
    errors: null,       // ★ M8.3：错误列表（journal level=error）
    busy: 0,
    home: '',
    appInfo: null,
    lastStatus: null,
    pollTimer: null,
    logCount: 0,
    // ★ M8.4
    sec: 'connect',       // 设置页内的分区
    settings: null,       // settings 请求的最近一次结果
    link: null,           // link_read 的最近一次结果（LAN / 关于 要用）
    space: null,          // space 请求的最近一次结果
    fileStates: {},       // 文件名 → 三态（file_states 请求）
    fsSummary: null,      // file_states 的汇总（online/local/always）
    m84Info: null,        // GUI 侧 M8.4 能力（托盘/插件）
    // ★ M8.6：各列表的四态（loading / error）—— 首轮不再冒充「不可用」
    journalLoading: false, journalError: '',
    errorsLoading: false, errorsError: '',
    tasksLoading: false, tasksError: '',
    decisionsLoading: false, decisionsError: '',
    mountsLoading: false, mountsError: '',
    rootsLoaded: false,
    lanLoading: false,
    spaceLoading: false,
    settingsError: '',
    rowMenuTrigger: null, // 右键菜单的焦点来处（关闭时还回去）
    lastErrorKey: '',     // 已通知过的错误条目（去重）
    errorTick: 0,         // 轮询计数器（每 3 次查一遍错误列表）
    rowMenu: null,        // 右键菜单当前作用的条目
    decisions: null,      // 冲突待裁决队列
    lastErrorId: 0,       // 已通知过的最大 journal id（桌面通知去重）
    trayAction: ''        // 最近一次托盘动作（自检/排查用）
  };

  // ============================================================ 小工具
  function $(id) {
    return document.getElementById(id);
  }

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) { n.className = cls; }
    if (text !== undefined && text !== null) { n.textContent = String(text); }
    return n;
  }

  function clear(node) {
    if (!node) { return; }
    while (node.firstChild) { node.removeChild(node.firstChild); }
  }

  function setText(id, text) {
    var n = $(id);
    if (n) { n.textContent = (text === undefined || text === null) ? '—' : String(text); }
  }

  function show(id, visible) {
    var n = $(id);
    if (n) { n.hidden = !visible; }
  }

  /**
   * ★ M8.6：列表四态的统一入口 —— loading / error / empty / ready。
   *
   * 为什么要它：M4 踩坑 #2 的同类问题在 M8 的新页面上又冒出来过一次 ——
   * 首轮数据还没回来时，界面写的是「不可用（daemon 未运行？）」，
   * 明明只是**还没加载完**，却长得像错误。现在四态各有各的文案，
   * 并把相位写到 `data-state` 上（`ready` 时移除），验收矩阵可据此断言。
   */
  function setListState(hintId, phase, text) {
    var n = $(hintId);
    if (!n) { return; }
    if (phase === 'ready') {
      n.hidden = true;
      n.removeAttribute('data-state');
      n.classList.remove('empty-err');
      return;
    }
    n.hidden = false;
    n.setAttribute('data-state', phase);
    if (phase === 'error') { n.classList.add('empty-err'); }
    else { n.classList.remove('empty-err'); }
    if (text !== undefined && text !== null) { n.textContent = String(text); }
  }

  /** 容器在读取中的 aria-busy（读屏能播报，验收也能断言）。 */
  function setBusyState(id, on) {
    var n = $(id);
    if (n) {
      if (on) { n.setAttribute('aria-busy', 'true'); }
      else { n.setAttribute('aria-busy', 'false'); }
    }
  }

  function isFn(v) { return typeof v === 'function'; }

  function isObj(v) { return v !== null && typeof v === 'object'; }

  function num(v, dflt) {
    var n = typeof v === 'number' ? v : parseFloat(v);
    return (typeof n === 'number' && isFinite(n)) ? n : (dflt === undefined ? 0 : dflt);
  }

  function str(v) {
    return (v === undefined || v === null) ? '' : String(v);
  }

  /** 有值才取，否则 undefined（用于「不给 = 查询」这类可选参数）。 */
  function opt(v) {
    if (v === undefined || v === null || v === '') { return undefined; }
    return v;
  }

  function bool(v) { return v === true; }

  // -------------------------------------------------------- 格式化
  function humanSize(n) {
    var v = num(n, 0);
    if (v === 0) { return '0 B'; }
    var neg = v < 0;
    if (neg) { v = -v; }
    var units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
    var i = 0;
    while (v >= 1024 && i < units.length - 1) {
      v = v / 1024;
      i++;
    }
    var out = (i === 0) ? (Math.round(v) + ' ' + units[0]) : (v.toFixed(v < 10 ? 2 : 1) + ' ' + units[i]);
    return (neg ? '-' : '') + out;
  }

  function pad2(n) {
    n = num(n, 0);
    return (n < 10 ? '0' : '') + n;
  }

  function clock(ts) {
    var d = isObj(ts) || ts instanceof Date ? new Date(ts) : new Date();
    return pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
  }

  function humanDuration(secs) {
    var s = Math.max(0, Math.round(num(secs, 0)));
    if (s < 60) { return s + ' 秒'; }
    var m = Math.floor(s / 60);
    if (m < 60) { return m + ' 分 ' + (s % 60) + ' 秒'; }
    var h = Math.floor(m / 60);
    if (h < 24) { return h + ' 小时 ' + (m % 60) + ' 分'; }
    var d = Math.floor(h / 24);
    return d + ' 天 ' + (h % 24) + ' 小时';
  }

  /** epoch 秒/毫秒 → 本地时间字符串。 */
  function humanTimeFromEpoch(epoch) {
    var e = num(epoch, 0);
    if (!e) { return ''; }
    if (e < 1e12) { e = e * 1000; }   // 秒 → 毫秒
    var d = new Date(e);
    if (isNaN(d.getTime())) { return String(epoch); }
    return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate()) + ' ' +
      pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
  }

  function entryTime(e) {
    if (!isObj(e)) { return '—'; }
    if (e.mt) { return String(e.mt); }
    var t = humanTimeFromEpoch(e.epochmt);
    return t || '—';
  }

  // -------------------------------------------------------- 路径工具
  function pathJoin(dir, name) {
    var d = str(dir);
    if (!d) { d = '/'; }
    if (d.charAt(d.length - 1) === '/') { return d + str(name); }
    return d + '/' + str(name);
  }

  function pathParent(p) {
    var s = str(p).replace(/\/+$/, '');
    if (!s || s === '/') { return '/'; }
    if (s.charAt(0) !== '/') { s = '/' + s; }
    var i = s.lastIndexOf('/');
    if (i <= 0) { return '/'; }
    return s.slice(0, i);
  }

  function pathBase(p) {
    var s = str(p).replace(/\/+$/, '');
    if (!s || s === '/') { return '/'; }
    var i = s.lastIndexOf('/');
    return i < 0 ? s : s.slice(i + 1);
  }

  function pathNormalize(p) {
    var s = str(p).trim();
    if (!s) { return '/'; }
    if (s.charAt(0) !== '/') { s = '/' + s; }
    var parts = s.split('/');
    var out = [];
    for (var i = 0; i < parts.length; i++) {
      var seg = parts[i];
      if (!seg || seg === '.') { continue; }
      if (seg === '..') { out.pop(); continue; }
      out.push(seg);
    }
    return '/' + out.join('/');
  }

  function isOn(checkboxId, dflt) {
    var n = $(checkboxId);
    if (!n) { return !!dflt; }
    return n.checked === true;
  }

  /**
   * ★ M6：把 textarea 里的「每行一个远端根」解析成字符串数组。
   * 去掉空行与首尾空白；空数组 = 只同步家目录（老行为）。
   */
  function parseRootLines(textareaId) {
    var n = $(textareaId);
    if (!n) { return []; }
    var raw = str(n.value).split(/\r?\n/);
    var out = [];
    for (var i = 0; i < raw.length; i++) {
      var line = raw[i].trim();
      if (!line) { continue; }
      if (out.indexOf(line) < 0) { out.push(line); }
    }
    return out;
  }

  // -------------------------------------------------------- 参数摘要
  function argSummary(cmd, args) {
    var a = isObj(args) ? args : {};
    if (cmd === 'ipc_call') {
      var req = isObj(a.req) ? a.req : {};
      var s = str(req.method);
      var extra = [];
      if (req.path !== undefined) { extra.push(str(req.path)); }
      if (req.dir !== undefined) { extra.push(str(req.dir)); }
      if (req.name !== undefined) { extra.push(str(req.name)); }
      if (req.parent !== undefined) { extra.push(str(req.parent)); }
      if (req.mountpoint !== undefined) { extra.push(str(req.mountpoint)); }
      if (req.dest !== undefined) { extra.push('→ ' + str(req.dest)); }
      if (req.state !== undefined) { extra.push('state=' + str(req.state)); }
      if (req.once !== undefined) { extra.push('once=' + str(req.once)); }
      if (req.force_deletes !== undefined) { extra.push('force_deletes=' + str(req.force_deletes)); }
      if (req.interval_secs !== undefined) { extra.push('interval=' + str(req.interval_secs)); }
      if (req.all !== undefined) { extra.push('all=' + str(req.all)); }
      if (req.dry_run !== undefined) { extra.push('dry_run=' + str(req.dry_run)); }
      if (req.cache_limit !== undefined) { extra.push('limit=' + str(req.cache_limit)); }
      if (req.idle_secs !== undefined) { extra.push('idle=' + str(req.idle_secs)); }
      return s + (extra.length ? ' ' + extra.join(' ') : '');
    }
    if (cmd === 'link_read' || cmd === 'daemon_start') {
      return 'linkId=' + str(a.linkId === undefined ? 'default' : a.linkId);
    }
    if (cmd === 'link_save' || cmd === 'credential_save' || cmd === 'login_flow') {
      var inp = isObj(a.input) ? a.input : {};
      // 注意：绝不记录 password
      return 'host=' + str(inp.host) + ' user=' + str(inp.user) + ' port=' + str(inp.port) +
        (inp.password ? ' password=***' : '');
    }
    return '';
  }

  function shortErr(err) {
    var msg = '';
    if (typeof err === 'string') { msg = err; }
    else if (isObj(err)) { msg = str(err.message || err.error || err.kind); }
    else { msg = str(err); }
    if (msg.length > 300) { msg = msg.slice(0, 300) + '…'; }
    return msg;
  }

  // ============================================================ 操作日志
  function logLine(kind, text) {
    var body = $('log-body');
    if (!body) { return; }
    var line = el('div', 'logline ' + kind);
    line.appendChild(el('span', 't', clock()));
    line.appendChild(el('span', 'c', text));

    body.insertBefore(line, body.firstChild);
    state.logCount++;
    while (body.childNodes.length > LOG_MAX) {
      body.removeChild(body.lastChild);
    }
    setText('log-count', T('log.count', { n: state.logCount }));
  }

  function logOk(text) { logLine('ok', text); }
  function logErr(text) { logLine('err', text); }
  function logInfo(text) { logLine('info', text); }

  /** 把错误详情渲染成可选中的等宽块，方便自动化验收抓取。 */
  function renderError(boxId, title, message) {
    var box = $(boxId);
    if (!box) { return; }
    clear(box);
    box.hidden = false;
    box.className = 'result err';
    box.appendChild(el('div', 'result-title', title));
    var pre = el('pre', 'mono err-pre', message);
    box.appendChild(pre);
  }

  // ============================================================ IPC 调用
  /**
   * 统一调用入口：记日志 + 返回 {ok, data|error}
   * 失败分两类：invoke reject（字符串）与信封 {ok:false, error:{kind,message}}
   */
  function call(cmd, args) {
    var summary = argSummary(cmd, args);
    var label = cmd + (summary ? ' ' + summary : '');
    if (!invokeFn) {
      logErr(label + ' → 未在 Tauri 中运行，无法调用');
      return Promise.resolve({ ok: false, error: '未在 Tauri 中运行（window.__TAURI__ 不存在）' });
    }
    return invokeFn(cmd, args).then(function (res) {
      if (isObj(res) && res.ok === false) {
        var m = (isObj(res.error) ? str(res.error.message) : str(res.error)) || '未知错误';
        var k = isObj(res.error) ? str(res.error.kind) : '';
        logErr(label + ' → error' + (k ? '[' + k + ']' : '') + ' ' + shortErr(m));
        return { ok: false, error: m, kind: k, data: res.data, raw: res };
      }
      logOk(label + ' → ok');
      return { ok: true, data: res, raw: res };
    }, function (err) {
      var m = shortErr(err);
      logErr(label + ' → 调用失败 ' + m);
      return { ok: false, error: m || '调用失败' };
    });
  }

  /** ipc_call 的语义化包装：req 直接是 {v:1, method:...} 那一层。 */
  function ipc(req) {
    if (isObj(req) && req.v === undefined) { req.v = 1; }
    return call('ipc_call', { req: req }).then(function (r) {
      var res = r.raw;
      if (!r.ok) { return { ok: false, error: r.error, kind: r.kind }; }
      if (isObj(res) && res.ok === true) {
        return { ok: true, data: res.data, raw: res };
      }
      var msg = (isObj(res) && isObj(res.error)) ? str(res.error.message) : '调用失败';
      return { ok: false, error: msg, raw: res };
    });
  }

  // ============================================================ 忙碌标记
  function setBusy(on, ids, btnIds) {
    state.busy += on ? 1 : -1;
    if (state.busy < 0) { state.busy = 0; }
    var i;
    for (i = 0; i < (ids || []).length; i++) {
      var n = $(ids[i]);
      if (n) { n.hidden = !on; }
    }
    for (i = 0; i < (btnIds || []).length; i++) {
      var b = $(btnIds[i]);
      if (b) { b.disabled = !!on; }
    }
  }

  // 长任务统一包一层：禁用按钮 + 显示「进行中…」
  function withBusy(opts, fn) {
    var o = opts || {};
    setBusy(true, o.spinners || [], o.buttons || []);
    return fn().then(function (r) {
      setBusy(false, o.spinners || [], o.buttons || []);
      return r;
    }, function (e) {
      setBusy(false, o.spinners || [], o.buttons || []);
      throw e;
    });
  }

  function requireDaemon() {
    var st = state.lastStatus;
    if (!st || !st.running) {
      logInfo('qxyncd 未运行，操作已取消（先点「启动 daemon」）');
      return false;
    }
    return true;
  }

  function requireLogin() {
    var st = state.lastStatus;
    if (!st || !st.running) {
      logInfo('qxyncd 未运行，请先启动 daemon');
      return false;
    }
    var status = st.status;
    if (!status || !status.logged_in) {
      logInfo('当前未登录，请到「连接 / 登录」页登录');
      return false;
    }
    return true;
  }

  // ============================================================ 弹窗
  var modalResolve = null;
  var modalLastFocus = null;   // ★ M8.6：打开前的焦点，关闭时还回去

  function openModal(title, message, initial) {
    var box = $('modal');
    if (!box) { return Promise.resolve(null); }
    setText('modal-title', title);
    setText('modal-message', message || '');
    var input = $('modal-input');
    input.value = initial === undefined ? '' : String(initial);
    modalLastFocus = document.activeElement;
    box.hidden = false;
    input.focus();
    input.select();
    return new Promise(function (resolve) {
      modalResolve = resolve;
    });
  }

  function closeModal(value) {
    var box = $('modal');
    if (box) { box.hidden = true; }
    if (modalLastFocus && isFn(modalLastFocus.focus)) { modalLastFocus.focus(); }
    modalLastFocus = null;
    var r = modalResolve;
    modalResolve = null;
    if (isFn(r)) { r(value); }
  }

  function wireModal() {
    var ok = $('modal-ok');
    var cancel = $('modal-cancel');
    var input = $('modal-input');
    var box = $('modal');
    if (ok) { ok.addEventListener('click', function () { closeModal(input.value); }); }
    if (cancel) { cancel.addEventListener('click', function () { closeModal(null); }); }
    if (input) {
      input.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter') { ev.preventDefault(); closeModal(input.value); }
        if (ev.key === 'Escape') { ev.preventDefault(); closeModal(null); }
      });
    }
    if (box) {
      box.addEventListener('click', function (ev) {
        if (ev.target === box) { closeModal(null); }
      });
      // ★ M8.6：aria-modal 的弹窗必须把 Tab 关在里面（焦点陷阱），Escape 关闭。
      box.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape') { ev.preventDefault(); closeModal(null); return; }
        if (ev.key !== 'Tab') { return; }
        var f = box.querySelectorAll('input, select, textarea, button, [href], [tabindex]:not([tabindex="-1"])');
        if (!f.length) { return; }
        var first = f[0];
        var last = f[f.length - 1];
        if (ev.shiftKey && document.activeElement === first) { ev.preventDefault(); last.focus(); }
        else if (!ev.shiftKey && document.activeElement === last) { ev.preventDefault(); first.focus(); }
      });
    }
  }

  // ============================================================ KV 渲染
  function kvRow(dl, key, value, cls) {
    if (!dl) { return; }
    dl.appendChild(el('dt', null, key));
    var dd = el('dd', cls ? cls : null);
    if (value instanceof Node) {
      dd.appendChild(value);
    } else {
      dd.textContent = (value === undefined || value === null || value === '') ? '—' : String(value);
    }
    dl.appendChild(dd);
  }

  function kvText(dl, key, value) { kvRow(dl, key, value); }

  function kvBool(dl, key, on, onText, offText) {
    kvRow(dl, key, on ? (onText || '是') : (offText || '否'), on ? 'val-ok' : 'val-dim');
  }

  function optText(v) {
    if (v === undefined || v === null || v === '') { return ''; }
    return String(v);
  }

  // ============================================================ 顶部状态条
  function renderTop(st) {
    var chipD = $('chip-daemon');
    var chipC = $('chip-conn');
    var chipL = $('chip-login');
    if (!chipD) { return; }

    chipD.className = 'chip chip-unknown';
    chipC.className = 'chip chip-unknown';
    chipL.className = 'chip chip-unknown';

    if (!st) {
      chipD.textContent = T('top.daemon_checking');
      return;
    }
    if (!st.running) {
      chipD.textContent = T('top.daemon_down');
      chipD.className = 'chip chip-err';
      chipC.textContent = T('top.conn_dash');
      chipL.textContent = T('top.login_dash');
      setText('meta-daemon', T('top.daemon_meta_down', { socket: str(st.socket) }));
      setText('meta-conn', T('top.meta_dash'));
      setText('meta-session', T('top.session_dash'));
      return;
    }

    var ping = isObj(st.ping) ? st.ping : null;
    chipD.textContent = T('top.daemon_up') + (ping ? ' v' + str(ping.daemon_version) : '');
    chipD.className = 'chip chip-ok';

    var s = isObj(st.status) ? st.status : null;
    var link = (s && isObj(s.link)) ? s.link : null;
    if (link) {
      chipC.textContent = T('top.conn', {
        value: str(link.host) + ':' + str(link.port) + (link.https ? ' (https)' : ' (http)')
      });
      chipC.className = 'chip chip-ok';
    } else {
      chipC.textContent = T('top.conn_none');
      chipC.className = 'chip chip-warn';
    }

    var logged = !!(s && s.logged_in);
    var sess = (s && isObj(s.session)) ? s.session : null;
    if (logged) {
      chipL.textContent = T('top.login', {
        value: str(sess ? sess.sid_masked : T('top.login_ok')) + (sess && !sess.alive ? T('top.login_alive_dead') : '')
      });
      chipL.className = 'chip ' + (sess && !sess.alive ? 'chip-warn' : 'chip-ok');
    } else {
      chipL.textContent = T('top.login_none');
      chipL.className = 'chip chip-warn';
    }

    setText('meta-daemon', T('top.daemon_meta', {
      pid: str(ping ? ping.pid : '—'), version: str(ping ? ping.daemon_version : '—'),
      uptime: humanDuration(ping ? ping.uptime_secs : 0), socket: str(st.socket)
    }));
    setText('meta-conn', link
      ? T('top.conn_meta', { user: str(link.user), host: str(link.host), port: str(link.port) }) +
        (link.ipv4_only ? T('top.conn_ipv4') : '')
      : T('top.meta_dash'));
    setText('meta-session', sess
      ? T('top.session_meta', { sid: str(sess.sid_masked), state: (sess.alive ? 'alive' : 'dead') })
      : T('top.session_dash'));

    if (st.error) {
      chipD.className = 'chip chip-warn';
      chipD.textContent = T('top.daemon_up_err');
    }
  }

  // ============================================================ 主页
  /**
   * 把一个「同步任务」的状态压成一句话 + 一个状态标记（对齐 Qsync 的任务卡片语言）。
   * 现阶段的「任务」= 一个挂载点（M8.2 才会引入真正的 Task 持久化）。
   * ⚠️ 只做**诚实**的归纳：拿不到证据就不说「已同步」。
   */
  function taskState(st, sy, hasMounts) {
    if (!st || !st.running) {
      return { mark: '⏸', tone: 'warn', text: T('task.state_no_daemon') };
    }
    var s = isObj(st.status) ? st.status : null;
    if (!s || !s.logged_in) {
      return { mark: '⚠️', tone: 'warn', text: T('task.state_not_logged_in') };
    }
    if (!hasMounts) {
      return { mark: '⏸', tone: 'warn', text: T('task.state_no_task') };
    }
    if (sy) {
      if (sy.last_error) {
        return { mark: '⚠️', tone: 'err', text: T('task.state_sync_error', { msg: String(sy.last_error) }) };
      }
      if (sy.delete_block_reason) {
        return { mark: '⚠️', tone: 'warn', text: T('task.state_delete_blocked') };
      }
      if (!sy.enabled) {
        return { mark: '⏸', tone: 'warn', text: T('task.state_paused') };
      }
      if (!num(sy.polls, 0)) {
        return { mark: '⏳', tone: 'warn', text: T('task.state_no_poll') };
      }
      if (num(sy.conflicts, 0) > 0) {
        return { mark: '⚠️', tone: 'warn', text: T('task.state_conflicts', { n: num(sy.conflicts, 0) }) };
      }
    }
    return { mark: '✅', tone: 'ok', text: T('task.state_all_synced') };
  }

  function renderHome(st) {
    if (!$('home-conn-text')) { return; }

    var s = (st && isObj(st.status)) ? st.status : null;
    var link = (s && isObj(s.link)) ? s.link : null;
    var dot = $('home-dot');
    var txt = $('home-conn-text');

    if (dot) { dot.className = 'conn-dot'; }
    if (!st || !st.running) {
      if (dot) { dot.className = 'conn-dot err'; }
      if (txt) { txt.textContent = T('home.conn_down'); }
    } else if (!link) {
      if (dot) { dot.className = 'conn-dot warn'; }
      if (txt) { txt.textContent = T('home.conn_no_link'); }
    } else {
      var logged = !!(s && s.logged_in);
      if (dot) { dot.className = 'conn-dot ' + (logged ? 'ok' : 'warn'); }
      if (txt) {
        txt.textContent = T('home.conn_ok', {
          user: str(link.user), host: str(link.host), port: str(link.port),
          state: (logged ? T('home.conn_connected') : T('home.conn_disconnected'))
        }) + (s && s.server && s.server.qsync_version
          ? T('home.conn_qxync', { version: str(s.server.qsync_version) }) : '');
      }
    }

    var mounts = (s && s.mounts && s.mounts.length) ? s.mounts : (state.mounts || []);
    var sy = (s && isObj(s.sync)) ? s.sync : null;
    // ★ M8.2：有任务登记就按任务展示（每张卡 = 一个同步任务），否则退回挂载点
    var taskInfos = (state.tasks && state.tasks.tasks) ? state.tasks.tasks : null;
    renderHomeTasks(st, sy, mounts, taskInfos);
    renderHomeSync(sy);
  }

  /** ★ M8.2：把「任务登记」与「挂载点」统一成一种卡片视图。 */
  function taskViews(mounts, taskInfos) {
    var out = [];
    if (taskInfos && taskInfos.length) {
      for (var i = 0; i < taskInfos.length; i++) {
        if (isObj(taskInfos[i]) && isObj(taskInfos[i].task)) {
          out.push({ task: taskInfos[i].task, info: taskInfos[i], mounted: taskInfos[i].mounted === true });
        }
      }
      return out;
    }
    // 没有任务登记 → 退回「一个挂载点 = 一个同步任务」（M8.1 的行为）
    var list = (mounts || []).filter(isObj);
    for (var k = 0; k < list.length; k++) {
      out.push({ mount: list[k], mounted: true });
    }
    return out;
  }

  function taskRoots(v) {
    if (v.task) { return (v.task.roots || []).filter(function (x) { return str(x); }); }
    var m = v.mount || {};
    return (m.roots && m.roots.length) ? m.roots : (str(m.remote) ? [str(m.remote)] : []);
  }

  function renderHomeTasks(st, sy, mounts, taskInfos) {
    var box = $('home-tasks');
    if (!box) { return; }
    clear(box);
    var views = taskViews(mounts, taskInfos);
    setText('home-task-count', views.length ? T('home.task_count', { n: views.length }) : '');
    setListState('home-tasks-empty', views.length ? 'ready' : 'empty', T('home.tasks_empty'));

    var ts = taskState(st, sy, views.length > 0);
    for (var i = 0; i < views.length; i++) {
      box.appendChild(buildTaskCard(views[i], ts, sy, false));
    }
  }

  /**
   * 一张任务卡。`full=true` 时带完整操作（任务页用），否则只给「管理 + 立即同步」（主页用）。
   */
  function buildTaskCard(v, ts, sy, full) {
    var t = v.task, m = v.mount || {};
    var card = el('div', 'task-card');
    var mark = ts.mark;
    if (t) {
      if (!t.enabled) { mark = '⏸'; }
      else if (!v.mounted) { mark = '⚠️'; }
    }
    card.appendChild(el('div', 'task-mark', mark));

    var body = el('div', 'task-body');
    var stateText = ts.text;
    if (t) {
      if (!t.enabled) { stateText = T('task.state_paused_task'); }
      else if (!v.mounted) { stateText = T('task.state_not_mounted'); }
    }
    body.appendChild(el('div', 'task-state', stateText));

    var roots = taskRoots(v);
    var mp = t ? str(t.mountpoint) : str(m.mountpoint);
    body.appendChild(el('div', 'task-meta', T('task.pair', {
      local: mp || T('task.not_set'),
      remote: roots.length ? roots.join(', ') : T('task.roots_by_link')
    })));

    var sub = [];
    if (t) {
      sub.push(t.read_write ? T('task.meta_read_write') : T('task.meta_read_only'));
      sub.push(T('task.meta_cache', { mode: str(t.cache_mode) }));
      if (t.direction && t.direction !== '2way') { sub.push(T('task.meta_direction', { dir: str(t.direction) })); }
      if (t.space_saving) { sub.push(T('task.meta_space_saving')); }
      if (t.smart_delete) { sub.push(T('task.meta_smart_delete')); }
      if ((t.exclude || []).length) { sub.push(T('task.meta_exclude', { n: t.exclude.length })); }
      // ★ M8.4：冲突策略一眼可见（ask 的要显眼 —— 会攒待裁决队列）
      sub.push(T('task.meta_conflict', { policy: conflictLabel(str(t.conflict) || 'rename_local') }));
    } else {
      sub.push(m.readonly ? T('task.meta_read_only') : T('task.meta_read_write'));
    }
    if (sy && num(sy.last_poll_age_secs, 0) > 0) {
      sub.push(T('task.meta_last_poll', { dur: humanDuration(sy.last_poll_age_secs) }));
    }
    body.appendChild(el('div', 'task-meta', sub.join(' · ')));

    var badgeRow = el('div', 'task-meta');
    badgeRow.appendChild(el('span', 'badge badge-sync', T('task.badge_sync')));
    badgeRow.appendChild(document.createTextNode(' '));
    if (t && !t.enabled) {
      badgeRow.appendChild(el('span', 'badge badge-off', T('task.badge_paused')));
    } else if (t && !v.mounted) {
      badgeRow.appendChild(el('span', 'badge badge-warn', T('task.badge_unmounted')));
    } else {
      badgeRow.appendChild(el('span', 'badge badge-on', T('task.badge_mounted')));
    }
    if (roots.length > 1) {
      badgeRow.appendChild(document.createTextNode(' '));
      badgeRow.appendChild(el('span', 'badge', T('task.badge_multi_root')));
    }
    body.appendChild(badgeRow);
    card.appendChild(body);

    var act = el('div', 'task-actions');

    if (full && t) {
      if (!t.enabled) {
        var bResume = el('button', 'mini primary', T('task.act_resume'));
        bResume.type = 'button';
        bResume.addEventListener('click', function () { doTaskAction('resume', t.id, '继续', bResume); });
        act.appendChild(bResume);
      } else if (v.mounted) {
        var bPause = el('button', 'mini', T('task.act_pause'));
        bPause.type = 'button';
        bPause.addEventListener('click', function () { doTaskAction('pause', t.id, '暂停', bPause); });
        act.appendChild(bPause);
      } else {
        var bMount = el('button', 'mini primary', T('task.act_mount'));
        bMount.type = 'button';
        bMount.addEventListener('click', function () { doTaskAction('mount', t.id, '挂载', bMount); });
        act.appendChild(bMount);
      }
    }

    if (full && t) {
      var bEdit = el('button', 'mini', T('task.act_settings'));
      bEdit.type = 'button';
      bEdit.addEventListener('click', function () { editTask(t); });
      act.appendChild(bEdit);
    }

    var bOpen = el('button', 'mini', T('task.act_manage'));
    bOpen.type = 'button';
    bOpen.addEventListener('click', function () { switchPage('diag', 'mounts'); });
    act.appendChild(bOpen);

    var bSync = el('button', 'mini primary', T('task.act_sync_now'));
    bSync.type = 'button';
    bSync.addEventListener('click', function () { doSync({ once: true }, [], '立即同步'); });
    act.appendChild(bSync);

    if (full && t) {
      var bDel = el('button', 'mini danger', T('task.act_delete'));
      bDel.type = 'button';
      bDel.addEventListener('click', function () {
        if (!window.confirm('删除任务登记？\n' + t.id + '\n\n只删 ~/.config/qxync/tasks/' + t.id +
          '.json，**不动挂载点里的文件，也不动 NAS 上的数据**。')) { return; }
        doTaskAction('delete', t.id, '删除登记', bDel);
      });
      act.appendChild(bDel);
    }

    card.appendChild(act);
    return card;
  }

  // ============================================================ 更新中心 / 错误列表（M8.3）
  var KIND_LABEL = {
    remote_change: '远端改动',
    upload: '上传',
    download: '下载',
    conflict: '冲突副本',
    delete: '删除',
    dehydrate: '释放空间',
    scan: '对账'
  };

  function kindLabel(k) {
    var s = str(k);
    return KIND_LABEL[s] ? (KIND_LABEL[s] + '（' + s + '）') : s;
  }

  function statusMark(st) {
    if (st === 'error') { return '❌'; }
    if (st === 'blocked') { return '⛔'; }
    return '✅';
  }

  function journalQuery(extra) {
    var req = { method: 'journal' };
    var q = $('j-query');
    var lvl = $('j-level');
    var lim = $('j-limit');
    if (q && str(q.value).trim()) { req.query = str(q.value).trim(); }
    if (lvl && str(lvl.value) && str(lvl.value) !== 'all') { req.level = str(lvl.value); }
    if (lim && num(lim.value, 0) > 0) { req.limit = num(lim.value, 200); }
    if (isObj(extra)) {
      for (var k in extra) {
        if (Object.prototype.hasOwnProperty.call(extra, k)) { req[k] = extra[k]; }
      }
    }
    return req;
  }

  function refreshJournal(extra) {
    state.journalLoading = true;
    state.journalError = '';
    renderJournalPage();
    return withBusy({ buttons: ['btn-journal-refresh'] }, function () {
      return ipc(journalQuery(extra));
    }).then(function (r) {
      state.journalLoading = false;
      if (!r.ok) {
        logErr('journal 查询失败：' + str(r.error));
        state.journalError = str(r.error);
        renderJournalPage();
        return r;
      }
      state.journal = isObj(r.data) ? r.data : null;
      renderJournalPage();
      return r;
    });
  }

  function renderJournalPage() {
    var tb = $('journal-tbody');
    if (!tb) { return; }
    clear(tb);
    setBusyState('journal-tbody', state.journalLoading === true);
    var d = state.journal;
    if (state.journalError) {
      setText('journal-count', '');
      setListState('journal-empty', 'error', T('list.error', { msg: state.journalError }));
      show('journal-note', false);
      return;
    }
    if (!d) {
      setText('journal-count', '');
      setListState('journal-empty', 'loading', T('journal.loading'));
      show('journal-note', false);
      return;
    }
    var list = (d.entries || []).filter(isObj);
    var counts = isObj(d.counts) ? d.counts : {};
    setText('journal-count', T('journal.counts', {
      total: num(d.total, 0), ok: num(counts.ok, 0),
      error: num(counts.error, 0), blocked: num(counts.blocked, 0)
    }));
    if (list.length === 0) {
      setListState('journal-empty', 'empty', d.cleared ? T('journal.cleared') : T('journal.empty_filtered'));
    } else {
      setListState('journal-empty', 'ready');
    }

    for (var i = 0; i < list.length; i++) {
      (function (e) {
        var tr = el('tr');
        tr.appendChild(el('td', 'col-time mono', humanTimeFromEpoch(e.ts)));
        var tdKind = el('td', 'col-kind');
        tdKind.appendChild(el('span', null, statusMark(str(e.status)) + ' '));
        tdKind.appendChild(el('span', 'badge', kindLabel(e.kind)));
        tr.appendChild(tdKind);
        tr.appendChild(el('td', 'mono', str(e.path) || '—'));
        tr.appendChild(el('td', null, str(e.detail) || '—'));
        tr.appendChild(el('td', 'col-size mono', num(e.bytes, 0) > 0 ? humanSize(e.bytes) : ''));
        tb.appendChild(tr);
      })(list[i]);
    }

    var note = $('journal-note');
    if (note) {
      note.hidden = !d.note;
      note.textContent = d.note ? ('说明：' + String(d.note)) : '';
    }
  }

  /** 错误列表 = journal 里 status=error 的视图（复用同一份数据）。 */
  function refreshErrors() {
    state.errorsLoading = true;
    state.errorsError = '';
    renderErrorsPage();
    return ipc({ method: 'journal', level: 'error', limit: 500 }).then(function (r) {
      state.errorsLoading = false;
      if (!r.ok) {
        logErr('错误列表读取失败：' + str(r.error));
        state.errorsError = str(r.error);
        renderErrorsPage();
        return r;
      }
      state.errors = isObj(r.data) ? r.data : null;
      renderErrorsPage();
      return r;
    });
  }

  function renderErrorsPage() {
    var box = $('errors-list');
    if (!box) { return; }
    clear(box);
    setBusyState('errors-list', state.errorsLoading === true);
    var d = state.errors;
    if (state.errorsError) {
      setText('errors-count', '');
      setListState('errors-empty', 'error', T('list.error', { msg: state.errorsError }));
      return;
    }
    if (!d) {
      setText('errors-count', '');
      setListState('errors-empty', 'loading', T('errors.loading'));
      return;
    }
    var list = (d.entries || []).filter(isObj);
    setText('errors-count', list.length ? '（' + list.length + '）' : '');
    setListState('errors-empty', list.length ? 'ready' : 'empty', T('errors.empty'));

    for (var i = 0; i < list.length; i++) {
      (function (e) {
        var row = el('div', 'alert alert-err');
        row.appendChild(el('span', 'mono', humanTimeFromEpoch(e.ts) + '  '));
        row.appendChild(el('span', 'badge badge-err', kindLabel(e.kind)));
        row.appendChild(document.createTextNode('  ' + (str(e.detail) || '（无说明）')));
        if (str(e.path)) {
          row.appendChild(document.createTextNode('  '));
          row.appendChild(el('span', 'mono', str(e.path)));
          var b = el('button', 'mini', '复制路径');
          b.type = 'button';
          b.addEventListener('click', function () { copyText(str(e.path)); });
          row.appendChild(document.createTextNode(' '));
          row.appendChild(b);
        }
        box.appendChild(row);
      })(list[i]);
    }
  }

  function copyText(t) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(t).then(function () { logOk('已复制：' + t); },
                                            function () { logInfo('复制失败：' + t); });
    } else {
      logInfo('当前环境不支持剪贴板，路径在此：' + t);
    }
  }

  function doJournalClear() {
    if (!window.confirm('清空同步日志？\n\n只删日志本身，**不动同步状态**（游标 / baseline / pin / 上传队列都不受影响），也不动 NAS 上的数据。')) {
      return Promise.resolve(null);
    }
    return refreshJournal({ clear: true }).then(function (r) {
      if (r && r.ok) { logOk('同步日志已清空'); }
      return r;
    });
  }

  // ============================================================ 任务页（M8.2）
  function refreshTasks() {
    state.tasksLoading = true;
    state.tasksError = '';
    renderTasksPage();
    return ipc({ method: 'tasks', action: 'list' }).then(function (r) {
      state.tasksLoading = false;
      // 任务列表回来后顺手刷一下冲突策略汇总与待裁决队列（M8.4）
      window.setTimeout(function () { renderConflictKv(); }, 0);
      refreshDecisions();
      if (!r.ok) {
        logErr('tasks list 失败：' + str(r.error));
        state.tasksError = str(r.error);
        renderTasksPage();
        return r;
      }
      state.tasks = isObj(r.data) ? r.data : null;
      renderTasksPage();
      return r;
    });
  }

  function renderTasksPage() {
    var box = $('tasks-list');
    if (!box) { return; }
    clear(box);
    clear($('tasks-bad'));
    setBusyState('tasks-list', state.tasksLoading === true);
    var d = state.tasks;
    if (state.tasksError) {
      setText('tasks-count', '');
      setListState('tasks-empty', 'error', T('list.error', { msg: state.tasksError }));
      show('tasks-note', false);
      return;
    }
    if (!d) {
      setText('tasks-count', '');
      setListState('tasks-empty', 'loading', T('tasks.loading'));
      show('tasks-note', false);
      return;
    }
    var infos = (d.tasks || []).filter(isObj);
    var sy = (state.lastStatus && isObj(state.lastStatus.status)) ? state.lastStatus.status.sync : null;
    var ts = taskState(state.lastStatus, sy, infos.length > 0);

    setText('tasks-count', infos.length ? '（' + infos.length + '）' : '');
    if (infos.length === 0) {
      setListState('tasks-empty', 'empty', d.empty ? T('tasks.empty_never') : T('tasks.empty_files'));
    } else {
      setListState('tasks-empty', 'ready');
    }
    for (var i = 0; i < infos.length; i++) {
      box.appendChild(buildTaskCard(
        { task: infos[i].task, info: infos[i], mounted: infos[i].mounted === true }, ts, sy, true));
    }

    var note = $('tasks-note');
    if (note) {
      note.hidden = !d.note;
      note.textContent = d.note ? ('说明：' + String(d.note)) : '';
    }
    var badBox = $('tasks-bad');
    var bad = (d.bad_files || []);
    // 空的时候必须隐藏：`.plain-list:empty::after` 会渲染一个「(无)」，
    // 放在「解析失败清单」下面会被误读成「有失败项」。
    if (badBox) { badBox.hidden = bad.length === 0; }
    for (var k = 0; k < bad.length; k++) {
      var pair = bad[k];
      badBox.appendChild(el('li', null,
        T('tasks.bad_file', { file: str(pair[0]), err: str(pair[1]) })));
    }
  }

  function doTaskAction(action, id, label, btn) {
    return withBusy({ buttons: btn ? [btn.id] : [] }, function () {
      return ipc({ method: 'tasks', action: action, id: id });
    }).then(function (r) {
      if (r.ok) {
        logOk(label + ' ' + id + ' 完成');
      } else {
        logErr(label + ' ' + id + ' 失败：' + str(r.error));
      }
      return refreshTasks().then(function () { return refreshStatus(true); }).then(function () { return r; });
    });
  }

  function openTaskForm() {
    var card = $('task-form-card');
    if (!card) { return; }
    var mp = $('t-mountpoint');
    if (mp && !mp.value) {
      mp.value = (str(state.home) || '~') + '/qxync-mnt';
    }
    card.hidden = false;
    card.scrollIntoView({ block: 'nearest' });
    var idEl = $('t-id');
    if (idEl) { idEl.focus(); }
  }

  /** ★ M8.4：把已有任务填进「文件夹对设置」表单（冲突策略就在这里面改）。 */
  function editTask(t) {
    var card = $('task-form-card');
    if (!card || !t) { return; }
    var set = function (id, v) { var n = $(id); if (n) { n.value = v; } };
    var chk = function (id, v) { var n = $(id); if (n) { n.checked = !!v; } };
    set('t-id', str(t.id));
    set('t-mountpoint', str(t.mountpoint));
    set('t-roots', (t.roots && t.roots.length) ? t.roots.join('\n') : '');
    set('t-cache-dir', t.cache_dir ? str(t.cache_dir) : '');
    set('t-cache-mode', str(t.cache_mode) || 'pagecache');
    set('t-conflict', str(t.conflict) || 'rename_local');
    set('t-direction', str(t.direction) || '2way');
    chk('t-space-saving', t.space_saving === true);
    chk('t-smart-delete', t.smart_delete === true);
    chk('t-read-write', t.read_write === true);
    chk('t-do-mount', t.enabled !== false);
    setText('task-form-title', '文件夹对设置 · ' + str(t.id));
    card.hidden = false;
    card.scrollIntoView({ block: 'nearest' });
  }

  function closeTaskForm() {
    var card = $('task-form-card');
    if (card) { card.hidden = true; }
    setText('task-form-title', '文件夹对设置');
    show('task-result', false);
  }

  function doTaskSave() {
    var id = str($('t-id') ? $('t-id').value : '').trim() || 'default';
    var mp = str($('t-mountpoint') ? $('t-mountpoint').value : '').trim();
    if (!mp) {
      renderError('task-result', '参数错误', '本地文件夹（挂载点）不能为空');
      return Promise.resolve(null);
    }
    var roots = parseRootLines('t-roots');
    var cdir = str($('t-cache-dir') ? $('t-cache-dir').value : '').trim();
    var task = {
      id: id,
      name: id,
      enabled: true,
      mountpoint: mp,
      cache_dir: cdir ? cdir : null,
      roots: roots,
      read_write: isOn('t-read-write', false),
      cache_mode: str($('t-cache-mode') ? $('t-cache-mode').value : 'pagecache') || 'pagecache',
      auto_unmount: true,
      // ★ M8.4：冲突策略 + 方向 + 节省空间（后端会 normalize：space_saving 为真时
      //   强制 smart_delete=false，这是 Qsync 原文语义）
      conflict: str($('t-conflict') ? $('t-conflict').value : 'rename_local') || 'rename_local',
      direction: str($('t-direction') ? $('t-direction').value : '2way') || '2way',
      space_saving: isOn('t-space-saving', false),
      smart_delete: isOn('t-smart-delete', false)
    };
    return withBusy({ spinners: ['task-busy'], buttons: ['btn-task-save'] }, function () {
      return ipc({ method: 'tasks', action: 'save', task: task });
    }).then(function (r) {
      if (!r.ok) {
        renderError('task-result', '保存任务失败', str(r.error));
        return r;
      }
      if (!isOn('t-do-mount', true)) {
        renderResult('task-result', true, '任务已保存（未挂载）', [
          ['id', id], ['本地', mp],
          ['NAS', roots.length ? roots.join(', ') : '（由 link 决定）'],
          ['缓存目录', cdir || '（默认 ~/.local/share/qxync/cache）'],
          ['冲突策略', conflictLabel(task.conflict)]
        ]);
        refreshTasks();
        return r;
      }
      return withBusy({ spinners: ['task-busy'] }, function () {
        return ipc({ method: 'tasks', action: 'resume', id: id });
      }).then(function (r2) {
        var okMount = r2.ok;
        renderResult('task-result', okMount, okMount ? '任务已保存并挂载' : '任务已保存，但挂载失败', [
          ['id', id],
          ['本地', mp],
          ['NAS', roots.length ? roots.join(', ') : '（由 link 决定）'],
          ['缓存目录', cdir || '（默认 ~/.local/share/qxync/cache）'],
          ['冲突策略', conflictLabel(task.conflict)],
          ['挂载', okMount ? '成功' : str(r2.error)]
        ]);
        refreshTasks();
        refreshStatus(true);
        return r2;
      });
    });
  }

  function renderHomeSync(sy) {
    var dl = $('home-sync-kv');
    if (!dl) { return; }
    clear(dl);
    clearAlerts('home-alerts');
    if (!sy) {
      kvText(dl, '同步', '不可用（daemon 未运行或未上报）');
      return;
    }
    kvText(dl, '轮询', sy.enabled ? (num(sy.interval_secs, 0) + ' 秒') : '已暂停',
      sy.enabled ? 'val-ok' : 'val-warn');
    kvText(dl, '轮次', num(sy.polls, 0));
    kvText(dl, '上次轮询', num(sy.last_poll_age_secs, 0) > 0
      ? humanDuration(sy.last_poll_age_secs) + '前' : '未轮询');
    kvText(dl, '刷新 / 上传 / 删除',
      num(sy.refreshed, 0) + ' / ' + num(sy.uploaded, 0) + ' / ' + num(sy.deleted, 0));
    kvText(dl, '冲突副本', num(sy.conflicts, 0), num(sy.conflicts, 0) > 0 ? 'val-warn' : null);
    kvText(dl, 'baseline 条目', num(sy.baseline_entries, 0));

    if (sy.last_error) { addAlert('home-alerts', '同步错误：' + String(sy.last_error)); }
    if (sy.delete_block_reason) {
      addAlert('home-alerts', '删除被熔断挡住：' + String(sy.delete_block_reason) +
        '（到「诊断 → 同步 / 缓存」可强制放行一轮）');
    }
    if (sy.note) { addAlert('home-alerts', '说明：' + String(sy.note), 'warn'); }
  }

  // ============================================================ Tab 1 状态
  function renderStatusPanel(st) {
    var srv = $('srv-kv');
    if (!srv) { return; }
    var s = (st && isObj(st.status)) ? st.status : null;

    // --- 服务端
    clear(srv);
    var srvWarn = $('srv-warn');
    if (srvWarn) { srvWarn.hidden = true; srvWarn.textContent = ''; }

    if (!st || !st.running) {
      show('sync-missing', true);
      kvText(srv, '状态', 'qxyncd 未运行：请点顶部「启动 daemon」，或到「连接 / 登录」页保存配置后启动。');
      ['sess-kv', 'cursors-kv', 'hydro-kv', 'uploads-kv', 'cache-kv', 'cache-blocked-kv', 'sync-summary-kv', 'sync-note']
        .forEach(function (id) { clear($(id)); });
      show('uploads-active', false);
      show('sync-note', false);
      setText('sync-summary-kv', '');
      renderMountTable('tbl-status-mounts', 'status-mounts-empty', 'mounts-status-count', []);
      setCacheBar('cache-bar-fill', 'cache-bar-label', null);
      clearAlerts('sync-alerts');
      return;
    }

    var server = (s && isObj(s.server)) ? s.server : null;
    if (server) {
      kvText(srv, 'Qsync 版本', optText(server.qsync_version));
      kvText(srv, 'QPKG 版本', optText(server.qpkg_version));
      kvText(srv, 'Build', optText(server.build));
      kvBool(srv, 'qbox CGI', server.qbox_cgi === true, '可用', '不可用');
      kvBool(srv, 'FCGI', server.fcgi === true, '可用', '不可用');
      if (server.busy_reason) {
        kvText(srv, 'busy_reason', String(server.busy_reason));
        if (srvWarn) {
          srvWarn.hidden = false;
          srvWarn.textContent = '⚠ NAS 处于忙状态：' + String(server.busy_reason);
        }
      }
    } else {
      kvText(srv, '状态', '未登录 / 服务端信息不可用');
    }
    if (st.error) {
      if (srvWarn) {
        srvWarn.hidden = false;
        srvWarn.textContent = '⚠ daemon 状态读取错误：' + String(st.error);
      }
    }

    // --- 会话
    var sess = $('sess-kv');
    clear(sess);
    kvText(sess, 'daemon 版本', optText(s.daemon && s.daemon.version));
    kvText(sess, 'pid', optText(s.daemon && s.daemon.pid));
    kvText(sess, 'uptime', humanDuration(s.daemon ? s.daemon.uptime_secs : (st.ping ? st.ping.uptime_secs : 0)));
    kvText(sess, 'socket', optText(s.daemon && s.daemon.socket));
    kvBool(sess, 'logged_in', s.logged_in === true, '已登录', '未登录');
    if (isObj(s.session)) {
      kvText(sess, 'sid', optText(s.session.sid_masked));
      kvBool(sess, 'session alive', s.session.alive === true, '存活', '失活');
    } else {
      kvText(sess, 'session', '—');
    }
    if (isObj(s.link)) {
      kvText(sess, 'link', str(s.link.id) + ' · ' + str(s.link.user) + '@' + str(s.link.host) + ':' + str(s.link.port));
    }

    // --- 游标
    var cur = $('cursors-kv');
    clear(cur);
    if (isObj(s.cursors)) {
      kvText(cur, 'max_log', s.cursors.max_log);
      kvText(cur, 'global_notify', s.cursors.global_notify);
      kvText(cur, 'sync_signal', s.cursors.sync_signal);
    } else {
      kvText(cur, 'cursors', '不可用');
    }

    // --- 水合
    var hydro = $('hydro-kv');
    clear(hydro);
    var h = isObj(s.hydro) ? s.hydro : { count: 0, bytes: 0 };
    kvText(hydro, '水合次数', num(h.count, 0));
    kvText(hydro, '水合字节', humanSize(h.bytes) + '（' + num(h.bytes, 0) + ' B）');

    // --- 上传队列
    var up = $('uploads-kv');
    clear(up);
    var u = isObj(s.uploads) ? s.uploads : null;
    if (u) {
      show('uploads-active', u.active === true);
      kvText(up, 'pending', num(u.pending, 0));
      kvText(up, 'active', num(u.active ? 1 : 0, 0) + (u.active ? '（有作业在途）' : ''));
      kvText(up, 'done', num(u.done, 0));
      kvText(up, 'failed', num(u.failed, 0), num(u.failed, 0) > 0 ? 'val-err' : null);
      kvText(up, 'retries', num(u.retries, 0), num(u.retries, 0) > 0 ? 'val-warn' : null);
      kvText(up, 'bytes', humanSize(u.bytes));
    } else {
      show('uploads-active', false);
      kvText(up, 'uploads', '—');
    }

    // --- 缓存 / 脱水
    var cache = $('cache-kv');
    var blocked = $('cache-blocked-kv');
    clear(cache);
    clear(blocked);
    var c = isObj(s.cache) ? s.cache : null;
    if (c) {
      kvText(cache, 'cache_mode', optText(c.mode));
      kvText(cache, 'used_bytes', humanSize(c.used_bytes));
      kvText(cache, '文件数', num(c.hydrated_files, 0) + ' / ' + num(c.total_files, 0) + ' 已水合');
      kvText(cache, 'limit', (c.limit_bytes === null || c.limit_bytes === undefined)
        ? '未设限额' : humanSize(c.limit_bytes));
      kvText(cache, 'idle_secs', num(c.idle_secs, 0) === 0 ? '未启用定时脱水' : humanDuration(c.idle_secs));
      kvText(cache, 'dehydrated_total', num(c.dehydrated_total, 0));
      kvText(cache, 'freed_total', humanSize(c.freed_total_bytes));
      kvText(cache, 'last_sweep_age', num(c.last_sweep_age_secs, 0) === 0 ? '未扫描' : humanDuration(c.last_sweep_age_secs) + '前');
      if (c.last_error) {
        kvText(cache, 'last_error', String(c.last_error), 'val-err');
      }
      kvText(blocked, 'blocked 合计', num(c.blocked_dirty, 0) + num(c.blocked_pinned, 0) + num(c.blocked_open, 0) +
        num(c.blocked_mapped, 0) + num(c.blocked_inflight, 0));
      kvText(blocked, 'dirty', num(c.blocked_dirty, 0));
      kvText(blocked, 'pinned', num(c.blocked_pinned, 0));
      kvText(blocked, 'open', num(c.blocked_open, 0));
      kvText(blocked, 'mapped', num(c.blocked_mapped, 0));
      kvText(blocked, 'inflight', num(c.blocked_inflight, 0));
    } else {
      kvText(cache, 'cache', '—');
      kvText(blocked, 'blocked', '—');
    }
    setCacheBar('cache-bar-fill', 'cache-bar-label', c);

    // --- 挂载
    var mounts = (s && s.mounts && s.mounts.length) ? s.mounts : state.mounts;
    renderMountTable('tbl-status-mounts', 'status-mounts-empty', 'mounts-status-count', mounts);

    // --- 同步摘要
    var sum = $('sync-summary-kv');
    clear(sum);
    clearAlerts('sync-alerts');
    var sy = isObj(s.sync) ? s.sync : null;
    show('sync-missing', !sy);
    if (sy) {
      kvText(sum, 'enabled', sy.enabled ? '是' : '否', sy.enabled ? 'val-ok' : 'val-warn');
      kvText(sum, 'interval_secs', num(sy.interval_secs, 0) === 0 ? '暂停轮询' : num(sy.interval_secs, 0) + ' 秒');
      kvText(sum, 'polls', num(sy.polls, 0));
      kvText(sum, 'last_poll_age', num(sy.last_poll_age_secs, 0) === 0 ? '未轮询' : humanDuration(sy.last_poll_age_secs) + '前');
      kvText(sum, 'refreshed', num(sy.refreshed, 0));
      kvText(sum, 'conflicts', num(sy.conflicts, 0), num(sy.conflicts, 0) > 0 ? 'val-warn' : null);
      kvText(sum, 'uploaded', num(sy.uploaded, 0));
      kvText(sum, 'deleted', num(sy.deleted, 0));
      kvText(sum, 'deletes_blocked', num(sy.deletes_blocked, 0), num(sy.deletes_blocked, 0) > 0 ? 'val-err' : null);
      kvText(sum, 'events', num(sy.events, 0));
      kvText(sum, 'baseline_entries', num(sy.baseline_entries, 0));
      if (sy.last_error) {
        addAlert('sync-alerts', '同步错误：' + String(sy.last_error));
      }
      if (sy.delete_block_reason) {
        addAlert('sync-alerts', '删除被熔断挡住：' + String(sy.delete_block_reason) + '（可用「强制放行删除」放行一轮）');
      }
      var note = $('sync-note');
      if (note) {
        note.hidden = !sy.note;
        note.textContent = sy.note ? ('说明：' + String(sy.note)) : '';
      }
    } else {
      var note2 = $('sync-note');
      if (note2) { note2.hidden = true; note2.textContent = ''; }
    }

    renderSyncPanel(s, sy);
  }

  function setCacheBar(fillId, labelId, c) {
    var fill = $(fillId);
    var label = $(labelId);
    if (!fill || !label) { return; }
    if (!c) {
      fill.style.width = '0%';
      fill.className = 'bar-fill';
      label.textContent = '缓存数据不可用';
      return;
    }
    var used = num(c.used_bytes, 0);
    var limit = (c.limit_bytes === null || c.limit_bytes === undefined) ? null : num(c.limit_bytes, 0);
    if (!limit || limit <= 0) {
      fill.style.width = '100%';
      fill.className = 'bar-fill';
      fill.style.opacity = '0.25';
      label.textContent = '未设限额：已用 ' + humanSize(used);
      return;
    }
    fill.style.opacity = '1';
    var pct = Math.min(100, (used / limit) * 100);
    fill.style.width = (isFinite(pct) ? pct.toFixed(1) : '0') + '%';
    fill.className = 'bar-fill' + (pct >= 90 ? ' err' : (pct >= 70 ? ' warn' : ''));
    label.textContent = humanSize(used) + ' / ' + humanSize(limit) + '（' + pct.toFixed(1) + '%）';
  }

  // severity: 'error'（默认，红，真失败）| 'warn'（黄，需注意但不致命）。
  // ★ 只有真失败才配红色；引擎的 note（如 qbox_get_sync_log 的 -17「区间内没有事件」）
  //   是提示性诊断信息，归 'warn'，否则主页会常驻一个红框。
  function addAlert(containerId, text, severity) {
    var box = $(containerId);
    if (!box) { return; }
    var cls = (severity === 'warn') ? 'alert alert-warn' : 'alert';
    box.appendChild(el('div', cls, text));
  }

  function clearAlerts(containerId) {
    clear($(containerId));
  }

  // ============================================================ 挂载表格
  function renderMountTable(tbodyTableId, emptyId, countId, mounts) {
    var tbl = $(tbodyTableId);
    if (!tbl) { return; }
    var tb = tbl.querySelector('tbody');
    if (!tb) { return; }
    clear(tb);
    var list = (mounts || []).filter(isObj);
    if (countId) { setText(countId, list.length ? '（' + list.length + '）' : ''); }
    if (emptyId) {
      if (state.mountsError) {
        setListState(emptyId, 'error', T('list.error', { msg: state.mountsError }));
      } else if (state.mountsLoading && !list.length) {
        setListState(emptyId, 'loading', T('mounts.loading'));
      } else if (list.length) {
        setListState(emptyId, 'ready');
      } else {
        setListState(emptyId, 'empty', T('mounts.empty'));
      }
    }
    setBusyState(tbodyTableId, state.mountsLoading === true);

    for (var i = 0; i < list.length; i++) {
      (function (m) {
        var tr = el('tr');
        tr.appendChild(el('td', 'mono', str(m.mountpoint)));
        // ★ M6：多根时显示全部根 + 徽章；老响应没有 roots 字段则退回 remote
        var roots = (m.roots && m.roots.length) ? m.roots : (str(m.remote) ? [str(m.remote)] : []);
        var tdRemote = el('td', 'mono', roots.join(', '));
        if (roots.length > 1) {
          tdRemote.appendChild(document.createTextNode(' '));
          tdRemote.appendChild(el('span', 'badge', '多根'));
        }
        tr.appendChild(tdRemote);
        var mode = el('td');
        mode.appendChild(el('span', 'badge ' + (m.readonly ? 'badge-off' : 'badge-on'), m.readonly ? '只读' : '读写'));
        tr.appendChild(mode);
        var act = el('td');
        var btn = el('button', 'mini danger', '卸载');
        btn.addEventListener('click', function () { doUmount(str(m.mountpoint), btn); });
        act.appendChild(btn);
        tr.appendChild(act);
        tb.appendChild(tr);
      })(list[i]);
    }
  }

  function doUmount(mountpoint, btn) {
    if (!requireDaemon()) { return; }
    if (!window.confirm('确定卸载挂载点？\n' + mountpoint)) { return; }
    withBusy({ buttons: btn ? [btn.id] : [] }, function () {
      return ipc({ method: 'umount', mountpoint: mountpoint });
    }).then(function (r) {
      if (r.ok) {
        logOk('umount ok: ' + mountpoint);
        refreshMountsIfVisible();
        refreshStatus(true);
        refreshRoots();
      } else {
        logErr('umount 失败: ' + str(r.error));
      }
    });
  }

  // ====================================================== ★ M6 远端根面板
  // roots 会真的去列 NAS 目录（贵），所以不跟着 2s 轮询刷：
  // 只在「切到状态 tab」+「点刷新」+「挂载/卸载成功」时各拉一次。
  var rootsInflight = null;

  function renderRoots(d) {
    var data = isObj(d) ? d : {};
    var list = $('roots-list');
    var folders = $('roots-folders');
    if (!list) { return; }
    clear(list);
    clear(folders);
    clear($('roots-summary-kv'));

    var roots = (data.roots || []).filter(isObj);
    state.rootsLoaded = true;
    setBusyState('roots-list', false);
    setText('roots-count', roots.length ? '（' + roots.length + '）' : '');
    if (roots.length) { setListState('roots-empty', 'ready'); }
    else { setListState('roots-empty', 'empty', T('roots.unreadable')); }

    var kv = $('roots-summary-kv');
    kvText(kv, 'home_root', optText(data.home_root) || '—');
    var cfg = (data.configured || []);
    kvText(kv, 'configured', cfg.length ? cfg.join(', ') : '（未配置 roots，仅家目录）');

    for (var i = 0; i < roots.length; i++) {
      var r = roots[i];
      var readable = r.readable === true;
      var writable = r.writable === true;
      var li = el('li', 'root-row');
      // ✅/❌ 按「可读」判定：不可读时下面是哪一步失败的看得见
      li.appendChild(el('span', 'root-mark', readable ? '✅' : '❌'));
      li.appendChild(el('span', 'root-path mono', str(r.remote)));
      li.appendChild(el('span', 'root-view', '视图名 ' + (str(r.view_name) || '直通')));
      li.appendChild(el('span', 'badge ' + (writable ? 'badge-on' : 'badge-off'), writable ? '可写' : '只读'));
      li.appendChild(el('span', 'badge ' + (readable ? 'badge-on' : 'badge-err'), readable ? '可读' : '不可读'));
      if (!readable && r.note) {
        li.appendChild(el('span', 'root-note-sub', String(r.note)));
      }
      list.appendChild(li);
    }

    var note = $('roots-note');
    if (note) {
      note.hidden = !data.note;
      note.textContent = data.note ? ('说明：' + String(data.note)) : '';
    }

    var sf = (data.syncing_folders || []).filter(isObj);
    if (sf.length) { setListState('roots-folders-empty', 'ready'); }
    else { setListState('roots-folders-empty', 'empty', T('roots.folders_empty')); }
    for (var j = 0; j < sf.length; j++) {
      var f = sf[j];
      var txt = str(f.folder) + ' · permission=' + str(f.permission) +
        (f.read_deletable === true ? ' · 可删' : '');
      folders.appendChild(el('li', null, txt));
    }
  }

  function refreshRoots() {
    if (rootsInflight) { return rootsInflight; }
    var btn = $('btn-roots-refresh');
    if (btn) { btn.disabled = true; }
    // ★ M8.6：首轮还没有数据时先给「读取中」，别让空态冒充结论
    if (!state.rootsLoaded) {
      setListState('roots-empty', 'loading', T('roots.loading'));
      setBusyState('roots-list', true);
    }
    rootsInflight = ipc({ method: 'roots' }).then(function (r) {
      rootsInflight = null;
      if (btn) { btn.disabled = false; }
      if (r.ok) {
        renderRoots(r.data);
      } else {
        // 失败也要给出可见反馈（错误详情在底部操作日志）
        clear($('roots-list'));
        clear($('roots-folders'));
        clear($('roots-summary-kv'));
        setText('roots-count', '');
        setListState('roots-empty', 'error', T('list.error', { msg: str(r.error) }));
        setListState('roots-folders-empty', 'ready');
        setBusyState('roots-list', false);
        var nt = $('roots-note');
        if (nt) { nt.hidden = true; nt.textContent = ''; }
      }
      return r;
    }, function (e) {
      rootsInflight = null;
      if (btn) { btn.disabled = false; }
      setBusyState('roots-list', false);
      throw e;
    });
    return rootsInflight;
  }

  function refreshRootsIfVisible() {
    if (state.page === 'diag' && state.diag === 'status') { refreshRoots(); }
  }

  // ============================================================ Tab 2 连接
  function fillConnectForm(info, present) {
    var hint = $('connect-hint');
    if (hint) {
      if (!info) {
        hint.textContent = T('connect.failed');
      } else if (info.exists && isObj(info.link)) {
        hint.textContent = T('connect.loaded', { path: str(info.path) });
      } else {
        hint.textContent = T('connect.none', { path: str(info.path) });
      }
    }
    setText('link-path-hint', info ? ('link 文件：' + str(info.path)) : '—');

    var link = (info && isObj(info.link)) ? info.link : null;
    if (link) {
      var host = $('f-host'); if (host) { host.value = str(link.host); }
      var port = $('f-port'); if (port) { port.value = str(link.port); }
      var user = $('f-user'); if (user) { user.value = str(link.user); }
      var hr = $('f-home-root'); if (hr) { hr.value = str(link.home_root); }
      var ht = $('f-https'); if (ht) { ht.checked = link.https !== false; }
      var ins = $('f-insecure'); if (ins) { ins.checked = link.insecure === true; }
      var v4 = $('f-ipv4-only'); if (v4) { v4.checked = link.ipv4_only === true; }
    }
    // ★ M6：roots 有值就每行一个；为空就留空（placeholder 提示当前家目录）
    var rootsBox = $('f-roots');
    if (rootsBox) {
      var rlist = (link && link.roots && link.roots.length) ? link.roots : [];
      var lines = [];
      for (var ri = 0; ri < rlist.length; ri++) { lines.push(str(rlist[ri])); }
      rootsBox.value = lines.join('\n');
      rootsBox.placeholder = str(link && link.home_root) || '/home';
    }
    var pw = $('f-password');
    if (pw) { pw.value = ''; }

    var cred = $('cred-hint');
    if (cred) {
      if (present && present.present) {
        cred.textContent = '已保存凭据：' + str(present.user) + '@' + str(present.host) + '   （' + str(present.path) + '）';
      } else {
        cred.textContent = '无凭据' + (present ? '（' + str(present.path) + '）' : '') + '；保存并登录时需要输入口令。';
      }
    }
  }

  function renderAppInfo(info) {
    state.appInfo = info;
    var dl = $('paths-kv');
    if (!dl) { return; }
    clear(dl);
    if (!info) {
      kvText(dl, 'app_info', '读取失败');
      return;
    }
    state.home = str(info.home);
    kvText(dl, 'GUI 版本', str(info.version));
    kvText(dl, 'HOME', str(info.home));
    kvText(dl, 'socket', str(info.socket));
    kvText(dl, 'config_dir', str(info.config_dir));
    kvText(dl, 'data_dir', str(info.data_dir));
    kvText(dl, 'state_dir', str(info.state_dir));
    kvText(dl, 'daemon_bin', info.daemon_bin ? str(info.daemon_bin) : '未找到（先 cargo build --workspace）',
      info.daemon_bin ? null : 'val-warn');
    kvBool(dl, 'daemon_running', info.daemon_running === true, '运行中', '未运行');

    // 挂载点默认值 & 下载默认目录
    var mp = $('m-mountpoint');
    if (mp && !mp.value) {
      mp.value = (str(info.home) || '~') + '/qxync-mnt';
    }
  }

  function buildLinkInput(withPassword) {
    var host = str($('f-host') ? $('f-host').value : '').trim();
    var user = str($('f-user') ? $('f-user').value : '').trim();
    if (!host || !user) {
      return { error: 'host / user 不能为空' };
    }
    var input = {
      host: host,
      port: num($('f-port') ? $('f-port').value : 9834, 9834),
      https: isOn('f-https', true),
      insecure: isOn('f-insecure', false),
      user: user,
      home_root: str($('f-home-root') ? $('f-home-root').value : '/home').trim() || '/home',
      ipv4_only: isOn('f-ipv4-only', false),
      // ★ M6：远端根；空数组也要传（表示清空，回到「只同步家目录」）
      roots: parseRootLines('f-roots')
    };
    if (withPassword) {
      input.password = str($('f-password') ? $('f-password').value : '');
    }
    return { input: input };
  }

  function loadConnect() {
    return Promise.all([
      call('link_read', { linkId: 'default' }),
      call('credential_present', {}),
      call('app_info', {})
    ]).then(function (rs) {
      var link = rs[0].ok ? rs[0].data : null;
      var cred = rs[1].ok ? rs[1].data : null;
      var info = rs[2].ok ? rs[2].data : null;
      renderAppInfo(info);
      fillConnectForm(link, cred);
    });
  }

  function doLinkSave() {
    var built = buildLinkInput(false);
    if (built.error) {
      renderError('login-result', '参数错误', built.error);
      return Promise.resolve({ ok: false, error: built.error });
    }
    return call('link_save', { input: built.input }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        renderResult('login-result', true, '连接配置已保存', [
          ['文件', str(d.path)],
          ['host:port', str(d.link && d.link.host) + ':' + str(d.link && d.link.port)],
          ['user', str(d.link && d.link.user)],
          ['https', d.link && d.link.https ? '是' : '否'],
          ['home_root', str(d.link && d.link.home_root)],
          ['roots', (d.link && d.link.roots && d.link.roots.length)
            ? d.link.roots.join(', ') : '（无，只同步家目录）']
        ]);
      } else {
        renderError('login-result', '保存连接配置失败', str(r.error));
      }
      return r;
    });
  }

  function doLogin() {
    var built = buildLinkInput(true);
    if (built.error) {
      renderError('login-result', '参数错误', built.error);
      return Promise.resolve({ ok: false, error: built.error });
    }
    if (!built.input.password) {
      renderError('login-result', '缺少口令', '请输入口令后再「保存并登录」（口令只用于写入凭据，不会回传）。');
      var pw = $('f-password');
      if (pw) { pw.focus(); }
      return Promise.resolve({ ok: false, error: '缺少口令' });
    }
    return withBusy({
      spinners: [],
      buttons: ['btn-login', 'btn-link-save']
    }, function () {
      return call('login_flow', { input: built.input });
    }).then(function (r) {
      if (!r.ok) {
        renderError('login-result', '登录流程失败（invoke 被拒）', str(r.error));
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      var login = isObj(d.login) ? d.login : null;
      var rows = [
        ['link 文件', str(d.link_path)],
        ['凭据文件', str(d.credential_path)],
        ['roots', built.input.roots.length ? built.input.roots.join(', ') : '（无，只同步家目录）'],
        ['daemon 已重启', d.restarted ? '是' : '否']
      ];
      if (login && isObj(login.data)) {
        rows.push(['sid', str(login.data.sid_masked)]);
        rows.push(['user', str(login.data.user)]);
        rows.push(['uid', optText(login.data.uid)]);
      }
      if (d.error) { rows.push(['错误', str(d.error)]); }
      if (login && isObj(login.error)) {
        rows.push(['登录错误', str(login.error.kind) + ': ' + str(login.error.message)]);
      }
      var ok = d.ok === true;
      renderResult('login-result', ok, ok ? '登录成功' : '登录失败', rows);
      if (ok) {
        loadConnect();
        refreshStatus(true);
        refreshRootsIfVisible();
      }
      return r;
    });
  }

  function renderResult(boxId, ok, title, rows) {
    var box = $(boxId);
    if (!box) { return; }
    clear(box);
    box.hidden = false;
    box.className = 'result ' + (ok ? 'ok' : 'err');
    box.appendChild(el('div', 'result-title', title));
    var dl = el('dl', 'kv');
    for (var i = 0; i < (rows || []).length; i++) {
      kvText(dl, rows[i][0], rows[i][1]);
    }
    box.appendChild(dl);
  }

  function doDaemonStart() {
    return withBusy({ buttons: ['btn-daemon-start', 'btn-connect-start'] }, function () {
      return call('daemon_start', { linkId: 'default' });
    }).then(function (r) {
      if (!r.ok) { return r; }
      var d = isObj(r.data) ? r.data : {};
      if (d.error) {
        logErr('daemon_start: ' + str(d.error));
      } else if (d.already) {
        logInfo('qxyncd 已在运行');
      } else {
        logOk('qxyncd 已启动' + (d.bin ? '：' + str(d.bin) : ''));
      }
      refreshStatus(true);
      return r;
    });
  }

  function doDaemonStop() {
    if (!window.confirm('确定停止 qxyncd？\n会卸载全部挂载点并删除 socket / pid。')) {
      return Promise.resolve(null);
    }
    return withBusy({ buttons: ['btn-daemon-stop', 'btn-connect-stop'] }, function () {
      return call('daemon_stop', {});
    }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        if (d.already) { logInfo('qxyncd 本来就没在运行'); }
        else if (d.stopped) { logOk('qxyncd 已停止'); }
        else { logErr('qxyncd 未能在超时内退出'); }
      }
      refreshStatus(true);
      return r;
    });
  }

  /** 顶部「立即登录」：用已保存的凭据 + 账密登录（不碰表单里的口令）。 */
  function doQuickLogin() {
    return call('credential_present', {}).then(function (r) {
      var d = r.ok ? r.data : null;
      if (!d || !d.present) {
        logInfo('没有已保存的凭据：请到「连接 / 登录」页填写口令并「保存并登录」');
        switchPage('settings');
        var pw = $('f-password');
        if (pw) { pw.focus(); }
        return null;
      }
      return call('daemon_start', { linkId: 'default' }).then(function () {
        return withBusy({ buttons: ['btn-quick-login'] }, function () {
          // 不带 user/password：daemon 自己读 credentials.json
          return ipc({ method: 'login' });
        });
      }).then(function (lr) {
        if (lr && lr.ok) {
          logOk('登录成功：' + str(lr.data && lr.data.sid_masked) + ' user=' + str(lr.data && lr.data.user));
        } else {
          logErr('登录失败：' + str(lr && lr.error));
        }
        refreshStatus(true);
        return lr;
      });
    });
  }

  // ============================================================ Tab 3 挂载
  function refreshMounts() {
    state.mountsLoading = true;
    return call('daemon_status', {}).then(function (r) {
      state.mountsLoading = false;
      if (r.ok && isObj(r.data)) {
        state.mountsError = '';
        var s = isObj(r.data.status) ? r.data.status : null;
        state.mounts = (s && s.mounts) ? s.mounts : [];
      } else {
        state.mountsError = str(r.error) || T('list.error_daemon');
      }
      renderMountTable('tbl-mounts', 'mounts-empty', 'mounts-count', state.mounts);
      renderMountTable('tbl-status-mounts', 'status-mounts-empty', 'mounts-status-count', state.mounts);
      return state.mounts;
    });
  }

  function refreshMountsIfVisible() {
    if (state.page === 'diag' && state.diag === 'mounts') { refreshMounts(); }
  }

  function doMount() {
    if (!requireLogin()) { return Promise.resolve(null); }
    var mp = str($('m-mountpoint') ? $('m-mountpoint').value : '').trim();
    // ★ M6：远端根每行一个；第一个是 remote（兼容字段）
    var roots = parseRootLines('m-remote');
    if (!mp) {
      renderError('mount-result', '参数错误', '挂载点不能为空');
      return Promise.resolve(null);
    }
    if (!roots.length) {
      renderError('mount-result', '参数错误', '远端根至少填一个（每行一个，默认 /home）');
      return Promise.resolve(null);
    }
    var req = {
      method: 'mount',
      mountpoint: mp,
      remote: roots[0],
      roots: roots,
      threads: num($('m-threads') ? $('m-threads').value : 4, 4),
      hydrate_timeout_secs: num($('m-hydrate-timeout') ? $('m-hydrate-timeout').value : 600, 600),
      auto_unmount: isOn('m-auto-unmount', true),
      read_write: isOn('m-read-write', false),
      delete_limit: num($('m-delete-limit') ? $('m-delete-limit').value : 100, 100),
      cache_mode: str($('m-cache-mode') ? $('m-cache-mode').value : 'pagecache') || 'pagecache'
    };
    return withBusy({
      spinners: ['mount-busy'],
      buttons: ['btn-mount']
    }, function () {
      return ipc(req);
    }).then(function (r) {
      if (r.ok) {
        renderResult('mount-result', true, '挂载成功', [
          ['挂载点', mp],
          ['远端根', roots.join(', ') + (roots.length > 1 ? '（多根）' : '')],
          ['模式', req.read_write ? '读写' : '只读'],
          ['cache_mode', req.cache_mode]
        ]);
        refreshMounts();
        refreshStatus(true);
        refreshRoots();
      } else {
        renderError('mount-result', '挂载失败', str(r.error));
      }
      return r;
    });
  }

  // ============================================================ Tab 4 文件
  var PIN_SELECT_CLASS = 'pin-select';

  function sortEntries(entries) {
    var arr = (entries || []).slice();
    arr.sort(function (a, b) {
      var af = a && a.isfolder === true;
      var bf = b && b.isfolder === true;
      if (af !== bf) { return af ? -1 : 1; }
      var an = str(a && a.filename);
      var bn = str(b && b.filename);
      try {
        return an.localeCompare(bn, 'zh-Hans-CN');
      } catch (e) {
        return an < bn ? -1 : (an > bn ? 1 : 0);
      }
    });
    return arr;
  }

  function refreshFiles(path) {
    var target = pathNormalize(path === undefined ? state.files.path : path);
    // ★ M8.6：先区分「daemon 没跑」/「没登录」/「真的读失败」，别把三种情况揉成一句空态。
    // 但**首轮 status 还没回来**不算错误 —— 那是「读取中」，不是「不可用」（M4 踩坑 #2 的同类）。
    if (!requireDaemon() || !requireLogin()) {
      state.files.loading = false;
      state.files.error = '';
      if (state.lastStatus && !state.lastStatus.running) {
        state.files.error = T('list.error_daemon');
      } else if (state.lastStatus && state.lastStatus.running) {
        state.files.error = T('files.not_logged_in');
      } else {
        state.files.loading = true;   // 状态未到：如实显示「读取中」
      }
      renderFiles();
      return Promise.resolve(null);
    }
    var fpb = $('btn-path-refresh');
    var busy = $('files-busy');
    if (busy) { busy.hidden = false; }
    state.files.loading = true;
    state.files.error = '';
    if (!state.files.entries.length) { renderFiles(); }
    return ipc({ method: 'ls', path: target }).then(function (r) {
      if (busy) { busy.hidden = true; }
      state.files.loading = false;
      if (!r.ok) {
        logErr('ls 失败：' + str(r.error));
        state.files.entries = [];
        state.files.selected = null;
        state.files.error = str(r.error);
        var fpErr = $('fp-path');
        if (fpErr) { fpErr.value = target; }
        renderFiles();
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      state.files.error = '';
      state.files.dir = str(d.path || target);
      state.files.path = state.files.dir;
      state.files.entries = sortEntries(d.entries);
      state.files.selected = null;
      var fp = $('fp-path');
      if (fp) { fp.value = state.files.dir; }
      renderFiles();
      // ★ M8.4：顺带取一次三态（仅在线 / 本地可用 / 始终可用）；失败不影响列表
      refreshFileStates();
      return r;
    });
  }

  // ------------------------------------------------------------ ★ M8.4 文件三态 + 右键菜单

  /** 取当前目录每个条目的三态。daemon 未挂载该目录时回空表（界面显示 —）。 */
  function refreshFileStates() {
    var dir = state.files.dir || state.files.path;
    if (!dir) { return Promise.resolve(null); }
    return ipc({ method: 'file_states', path: dir }).then(function (r) {
      var map = {};
      if (r.ok && isObj(r.data) && r.data.entries) {
        var es = r.data.entries;
        for (var i = 0; i < es.length; i++) {
          if (es[i] && es[i].name) { map[str(es[i].name)] = es[i]; }
        }
        state.fsSummary = {
          online: num(r.data.online, 0),
          local: num(r.data.local, 0),
          always: num(r.data.always, 0),
          mountpoint: r.data.mountpoint,
          note: r.data.note
        };
      } else {
        state.fsSummary = null;
      }
      state.fileStates = map;
      renderFiles();
      return r;
    }, function () { return { ok: false }; });
  }

  /** pin / 脱水之后稍等一会儿再刷新三态（daemon 侧状态要有落点）。 */
  function refreshFileStatesSoon() {
    window.setTimeout(function () {
      if (state.page === 'files') { refreshFileStates(); }
    }, 500);
  }

  function openRowMenu(ev, e, full) {
    var menu = $('row-menu');
    if (!menu) { return; }
    state.rowMenu = { entry: e, path: full };
    // ★ M8.6：键盘（Shift+F10 / 菜单键）也能开；关闭时把焦点还给来处
    state.rowMenuTrigger = document.activeElement;
    setText('row-menu-title', full);
    menu.hidden = false;
    var w = menu.offsetWidth || 300;
    var h = menu.offsetHeight || 250;
    var cx = num(ev && ev.clientX, 0);
    var cy = num(ev && ev.clientY, 0);
    var rect = (ev && ev.target && ev.target.getBoundingClientRect) ? ev.target.getBoundingClientRect() : null;
    if (!cx && !cy && rect) { cx = rect.left + 8; cy = rect.bottom + 2; }
    var x = Math.min(cx, Math.max(4, window.innerWidth - w - 8));
    var y = Math.min(cy, Math.max(4, window.innerHeight - h - 8));
    menu.style.left = Math.max(4, x) + 'px';
    menu.style.top = Math.max(4, y) + 'px';
    var first = menu.querySelector('.ctx-item');
    if (first && isFn(first.focus)) { first.focus(); }
  }

  /** `restoreFocus=true`（键盘路径）时把焦点还给触发元素；鼠标点空白处关就不还，免得抢焦点。 */
  function closeRowMenu(restoreFocus) {
    var m = $('row-menu');
    if (m) { m.hidden = true; }
    state.rowMenu = null;
    if (restoreFocus === true && state.rowMenuTrigger && isFn(state.rowMenuTrigger.focus)) {
      state.rowMenuTrigger.focus();
    }
    state.rowMenuTrigger = null;
  }

  function wireRowMenu() {
    var menu = $('row-menu');
    if (!menu) { return; }
    var items = menu.querySelectorAll('.ctx-item');
    for (var i = 0; i < items.length; i++) {
      (function (b) {
        b.addEventListener('click', function () {
          doRowAction(str(b.getAttribute('data-act')));
        });
      })(items[i]);
    }
    // ★ M8.6：菜单内部的方向键导航（↑↓/Home/End），Escape/Tab 关闭并把焦点还给来处
    menu.addEventListener('keydown', function (ev) {
      var items = menu.querySelectorAll('.ctx-item');
      var cur = -1;
      for (var i = 0; i < items.length; i++) {
        if (items[i] === document.activeElement) { cur = i; }
      }
      var next = -1;
      if (ev.key === 'ArrowDown') { next = cur < 0 ? 0 : (cur + 1) % items.length; }
      else if (ev.key === 'ArrowUp') { next = cur < 0 ? items.length - 1 : (cur - 1 + items.length) % items.length; }
      else if (ev.key === 'Home') { next = 0; }
      else if (ev.key === 'End') { next = items.length - 1; }
      else if (ev.key === 'Escape' || ev.key === 'Tab') { ev.preventDefault(); closeRowMenu(true); return; }
      if (next < 0) { return; }
      ev.preventDefault();
      items[next].focus();
    });
    document.addEventListener('click', function (ev) {
      var m = $('row-menu');
      if (m && !m.hidden && !m.contains(ev.target)) { closeRowMenu(false); }
    });
    document.addEventListener('keydown', function (ev) {
      var m = $('row-menu');
      if (ev.key === 'Escape' && m && !m.hidden) { closeRowMenu(true); }
    });
    window.addEventListener('blur', function () { closeRowMenu(false); });
  }

  function doRowAction(act) {
    var cur = state.rowMenu;
    if (!cur) { return; }
    var e = cur.entry;
    var full = cur.path;
    closeRowMenu(false);
    if (act === 'pin') {
      setPinState(full, 'pinned', null, null);
      refreshFileStatesSoon();
    } else if (act === 'unpin') {
      setPinState(full, 'unspecified', null, null);
      refreshFileStatesSoon();
    } else if (act === 'free') {
      doDehydratePath(full, null).then(refreshFileStatesSoon);
    } else if (act === 'get') {
      doDownload(e, null);
    } else if (act === 'copy') {
      copyText(full);
    } else if (act === 'rm') {
      doRemovePath(full);
    }
  }

  function doRemovePath(full) {
    if (!requireDaemon()) { return; }
    if (!window.confirm('确定删除远端条目？\n' + full)) { return; }
    withBusy({ spinners: ['files-busy'] }, function () {
      return ipc({ method: 'rm', dir: state.files.dir, name: pathBase(full) });
    }).then(function (r) {
      if (r.ok) {
        logOk('已删除：' + full);
        refreshFiles(state.files.dir);
      } else {
        logErr('rm 失败：' + str(r.error));
      }
      return r;
    });
  }

  function renderFiles() {
    var tb = $('files-tbody');
    if (!tb) { return; }
    clear(tb);
    var hideExcluded = isOn('fp-hide-excluded', false);
    var entries = state.files.entries || [];
    setBusyState('files-tbody', state.files.loading === true);

    for (var i = 0; i < entries.length; i++) {
      var e = entries[i];
      if (!isObj(e)) { continue; }
      tb.appendChild(buildFileRow(e, hideExcluded));
    }
    // ★ M8.6：读取失败 → 明确报错；读取中 → 不把「共 0 项」当结论
    if (state.files.error) {
      setText('files-info', T('list.error', { msg: state.files.error }));
      setListState('files-empty', 'error', T('list.error', { msg: state.files.error }));
      return;
    }
    if (state.files.loading && !entries.length) {
      setText('files-info', T('files.loading'));
      setListState('files-empty', 'loading', T('files.loading'));
      return;
    }
    var info = state.files.dir
      ? T('files.info_dir', { n: entries.length, dir: state.files.dir })
      : T('files.info', { n: entries.length });
    if (state.fsSummary) {
      info += T('files.summary_states', {
        online: num(state.fsSummary.online, 0),
        local: num(state.fsSummary.local, 0),
        always: num(state.fsSummary.always, 0)
      });
    } else if (state.files.dir) {
      info += T('files.summary_no_mount');
    }
    setText('files-info', info);
    if (entries.length) { setListState('files-empty', 'ready'); }
    else { setListState('files-empty', 'empty', T('files.empty')); }
  }

  function buildFileRow(e, hideExcluded) {
    var tr = el('tr');
    var full = pathJoin(state.files.dir, e.filename);

    // 选择
    var tdSel = el('td', 'col-sel');
    var cb = el('input');
    cb.type = 'checkbox';
    cb.checked = state.files.selected === full;
    cb.addEventListener('change', function () {
      state.files.selected = cb.checked ? full : null;
      renderFiles();
    });
    tdSel.appendChild(cb);
    tr.appendChild(tdSel);

    // 类型
    var isDir = e.isfolder === true;
    tr.appendChild(el('td', 'col-type', isDir ? '📁' : '📄'));

    // 文件名
    var tdName = el('td', 'name');
    if (isDir) {
      var btn = el('button', 'name-link', str(e.filename));
      btn.type = 'button';
      btn.addEventListener('click', function () {
        state.files.selected = null;
        refreshFiles(pathJoin(state.files.dir, e.filename));
      });
      tdName.appendChild(btn);
    } else {
      tdName.appendChild(el('span', 'name-file', str(e.filename)));
    }
    if (e.exist === false) {
      tdName.appendChild(document.createTextNode(' '));
      tdName.appendChild(el('span', 'badge badge-warn', '占位'));
    }
    if (e.privilege) {
      tdName.appendChild(document.createTextNode(' '));
      tdName.appendChild(el('span', 'badge', str(e.privilege)));
    }
    tr.appendChild(tdName);

    // 大小
    tr.appendChild(el('td', 'col-size', isDir ? '—' : humanSize(e.filesize)));

    // 时间
    tr.appendChild(el('td', 'col-time mono', entryTime(e)));

    // have_child
    tr.appendChild(el('td', 'col-child', e.have_child === true ? '✔' : ''));

    // ★ M8.4：状态列（节省空间模式三态）
    var fs = state.fileStates[str(e.filename)];
    var tdState = el('td', 'col-state');
    if (isDir) {
      tdState.appendChild(el('span', 'badge', T('state.dir')));
    } else if (fs && str(fs.state) === 'always') {
      tdState.appendChild(el('span', 'badge badge-always', T('state.always')));
    } else if (fs && str(fs.state) === 'local') {
      tdState.appendChild(el('span', 'badge badge-local', T('state.local')));
    } else if (fs && str(fs.state) === 'online') {
      tdState.appendChild(el('span', 'badge badge-online', T('state.online')));
    } else {
      tdState.appendChild(el('span', 'badge', '—'));
    }
    if (fs && !isDir && num(fs.hydrated_bytes, 0) > 0) {
      tdState.appendChild(document.createTextNode(' '));
      tdState.appendChild(el('span', 'task-meta', humanSize(num(fs.hydrated_bytes, 0))));
    }
    if (fs && fs.dirty === true) {
      tdState.appendChild(document.createTextNode(' '));
      tdState.appendChild(el('span', 'badge badge-warn', T('state.dirty')));
    }
    tr.appendChild(tdState);

    // 右键菜单（对齐 Qsync 的「节省空间模式 ▸ …」）
    tr.addEventListener('contextmenu', function (ev) {
      ev.preventDefault();
      openRowMenu(ev, e, full);
    });
    // ★ M8.6：键盘打开同一份菜单（Shift+F10 或菜单键）—— 事件从行内的 checkbox/按钮冒泡上来
    tr.addEventListener('keydown', function (ev) {
      if (ev.key === 'ContextMenu' || (ev.shiftKey && ev.key === 'F10')) {
        ev.preventDefault();
        openRowMenu(ev, e, full);
      }
    });

    // pin 操作
    var tdPin = el('td', 'col-pin');
    var sel = el('select', PIN_SELECT_CLASS);
    var opt0 = el('option', null, '（未知）');
    opt0.value = '';
    sel.appendChild(opt0);
    for (var i = 0; i < PIN_STATES.length; i++) {
      var o = el('option', null, PIN_STATES[i] + ' · ' + PIN_LABEL[PIN_STATES[i]]);
      o.value = PIN_STATES[i];
      sel.appendChild(o);
    }
    sel.value = '';
    tdPin.appendChild(sel);
    tdPin.appendChild(document.createTextNode(' '));

    var btnQuery = el('button', 'mini', '查 pin');
    btnQuery.type = 'button';
    btnQuery.addEventListener('click', function () {
      setPinState(full, null, sel, btnQuery);
    });
    tdPin.appendChild(btnQuery);
    tdPin.appendChild(document.createTextNode(' '));

    var btnSet = el('button', 'mini', '设 pin');
    btnSet.type = 'button';
    btnSet.addEventListener('click', function () {
      if (!sel.value) {
        logInfo('请先选择 pin 状态（unspecified / pinned / unpinned / excluded）');
        return;
      }
      setPinState(full, sel.value, sel, btnSet);
    });
    tdPin.appendChild(btnSet);

    // 文件额外操作：下载 / 脱水
    if (!isDir) {
      tdPin.appendChild(document.createTextNode(' '));
      var btnGet = el('button', 'mini', '下载');
      btnGet.type = 'button';
      btnGet.addEventListener('click', function () { doDownload(e, btnGet); });
      tdPin.appendChild(btnGet);

      tdPin.appendChild(document.createTextNode(' '));
      var btnDehy = el('button', 'mini', '脱水');
      btnDehy.type = 'button';
      btnDehy.addEventListener('click', function () { doDehydratePath(full, btnDehy); });
      tdPin.appendChild(btnDehy);
    } else {
      tdPin.appendChild(document.createTextNode(' '));
      var btnDehyDir = el('button', 'mini', '脱水');
      btnDehyDir.type = 'button';
      btnDehyDir.addEventListener('click', function () { doDehydratePath(full, btnDehyDir); });
      tdPin.appendChild(btnDehyDir);
    }

    tr.appendChild(tdPin);
    return tr;
  }

  function setPinState(path, pinState, selectEl, btn) {
    if (!requireDaemon()) { return; }
    var req = { method: 'pin', path: path };
    if (pinState) { req.state = pinState; }
    withBusy({ buttons: btn ? [btn.id] : [] }, function () {
      return ipc(req);
    }).then(function (r) {
      if (!r.ok) {
        logErr('pin ' + (pinState ? '设置' : '查询') + '失败：' + str(r.error));
        return;
      }
      var d = isObj(r.data) ? r.data : {};
      var got = str(d.pin);
      if (got && selectEl) { selectEl.value = got; }
      logOk('pin ' + (pinState ? '设置' : '查询') + '：' + path + ' → ' + (got || '—'));
      if (r.raw && r.raw.data !== undefined) {
        // pin 查询结果就是 {path, pin}
      }
    });
  }

  function doDownload(e, btn) {
    if (!requireDaemon()) { return; }
    var base = state.home || '';
    var guess = base ? pathJoin(base, e.filename) : e.filename;
    openModal('下载到本地', '远端：' + pathJoin(state.files.dir, e.filename) + '（大文件可能较久，daemon 超时 600s）', guess)
      .then(function (dest) {
        if (!dest) { return null; }
        var target = str(dest).trim();
        if (!target) { return null; }
        return withBusy({
          spinners: ['files-busy'],
          buttons: btn ? [btn.id] : []
        }, function () {
          return ipc({ method: 'get', dir: state.files.dir, name: str(e.filename), dest: target });
        }).then(function (r) {
          if (r.ok) {
            var d = isObj(r.data) ? r.data : {};
            logOk('下载完成：' + str(d.dest) + '（' + humanSize(d.bytes) + '）');
          } else {
            logErr('下载失败：' + str(r.error));
          }
          return r;
        });
      });
  }

  function doDehydratePath(path, btn) {
    if (!requireDaemon()) { return; }
    withBusy({
      spinners: ['dehy-busy', 'files-busy'],
      buttons: btn ? [btn.id] : []
    }, function () {
      return ipc({ method: 'dehydrate', path: path });
    }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        logOk('脱水 ' + path + '：' + num(d.dehydrated, 0) + ' 项，释放 ' + humanSize(d.freed_bytes) +
          '（当前已用 ' + humanSize(d.used_bytes) + '）');
      } else {
        logErr('脱水失败：' + str(r.error));
      }
      return r;
    });
  }

  function doMkdir() {
    if (!requireLogin()) { return; }
    openModal('新建目录', '父目录：' + state.files.dir, '')
      .then(function (name) {
        var n = str(name).trim();
        if (!n) { return null; }
        if (n.indexOf('/') >= 0) {
          logErr('目录名不能包含 /');
          return null;
        }
        return withBusy({ spinners: ['files-busy'], buttons: ['btn-mkdir'] }, function () {
          return ipc({ method: 'mkdir', parent: state.files.dir, name: n });
        }).then(function (r) {
          if (r.ok) {
            logOk('已创建目录：' + pathJoin(state.files.dir, n));
            refreshFiles(state.files.dir);
          } else {
            logErr('mkdir 失败：' + str(r.error));
          }
          return r;
        });
      });
  }

  function doRm() {
    if (!requireLogin()) { return; }
    var full = state.files.selected;
    if (!full) {
      logInfo('未选中任何条目：先勾选左侧「选」列');
      return;
    }
    var name = pathBase(full);
    if (!window.confirm('确定删除远端条目？\n' + full)) { return; }
    withBusy({ spinners: ['files-busy'], buttons: ['btn-rm'] }, function () {
      return ipc({ method: 'rm', dir: state.files.dir, name: name });
    }).then(function (r) {
      if (r.ok) {
        logOk('已删除：' + full);
        state.files.selected = null;
        refreshFiles(state.files.dir);
      } else {
        logErr('rm 失败：' + str(r.error));
      }
      return r;
    });
  }

  // ============================================================ Tab 5 同步
  function renderSyncPanel(s, sy) {
    var dl = $('sync-full-kv');
    if (!dl) { return; }
    clear(dl);
    if (!sy) {
      kvText(dl, 'sync', '不可用（daemon 未运行或未上报）');
      clear($('sync-cursors-kv'));
      clear($('sync-devices'));
      clearAlerts('sync2-alerts');
      return;
    }
    kvText(dl, 'enabled', sy.enabled ? '是' : '否', sy.enabled ? 'val-ok' : 'val-warn');
    kvText(dl, 'interval_secs', num(sy.interval_secs, 0) === 0 ? '0（暂停轮询）' : num(sy.interval_secs, 0));
    kvText(dl, 'polls', num(sy.polls, 0));
    kvText(dl, 'last_poll_age_secs', num(sy.last_poll_age_secs, 0));
    kvText(dl, 'baseline_entries', num(sy.baseline_entries, 0));
    kvText(dl, 'refreshed', num(sy.refreshed, 0));
    kvText(dl, 'conflicts', num(sy.conflicts, 0), num(sy.conflicts, 0) > 0 ? 'val-warn' : null);
    kvText(dl, 'uploaded', num(sy.uploaded, 0));
    kvText(dl, 'deleted', num(sy.deleted, 0));
    kvText(dl, 'deletes_blocked', num(sy.deletes_blocked, 0), num(sy.deletes_blocked, 0) > 0 ? 'val-err' : null);
    kvText(dl, 'events', num(sy.events, 0));
    kvText(dl, 'devices', (sy.devices || []).length);
    kvText(dl, 'last_error', sy.last_error ? String(sy.last_error) : '—', sy.last_error ? 'val-err' : null);
    kvText(dl, 'delete_block_reason', sy.delete_block_reason ? String(sy.delete_block_reason) : '—',
      sy.delete_block_reason ? 'val-err' : null);
    kvText(dl, 'note', sy.note ? String(sy.note) : '—');

    var cur = $('sync-cursors-kv');
    clear(cur);
    if (isObj(sy.cursors)) {
      kvText(cur, 'config', num(sy.cursors.config, 0));
      kvText(cur, 'notify', num(sy.cursors.notify, 0));
      kvText(cur, 'global_notify', num(sy.cursors.global_notify, 0));
      kvText(cur, 'max_log_seen', num(sy.cursors.max_log_seen, 0));
      kvText(cur, 'log_missing_count', num(sy.cursors.log_missing_count, 0),
        num(sy.cursors.log_missing_count, 0) > 0 ? 'val-warn' : null);
    }

    var dev = $('sync-devices');
    clear(dev);
    var list = (sy.devices || []);
    for (var i = 0; i < list.length; i++) {
      dev.appendChild(el('li', null, str(list[i])));
    }

    clearAlerts('sync2-alerts');
    if (sy.last_error) { addAlert('sync2-alerts', '同步错误：' + String(sy.last_error)); }
    if (sy.delete_block_reason) {
      addAlert('sync2-alerts', '删除熔断：' + String(sy.delete_block_reason));
    }

    var cdl = $('cache-full-kv');
    clear(cdl);
    var c = (s && isObj(s.cache)) ? s.cache : null;
    if (!c) {
      kvText(cdl, 'cache', '不可用');
    } else {
      kvText(cdl, 'mode', str(c.mode));
      kvText(cdl, 'used_bytes', humanSize(c.used_bytes) + '（' + num(c.used_bytes, 0) + ' B）');
      kvText(cdl, 'total_files', num(c.total_files, 0));
      kvText(cdl, 'hydrated_files', num(c.hydrated_files, 0));
      kvText(cdl, 'limit_bytes', (c.limit_bytes === null || c.limit_bytes === undefined) ? '未设限额' : humanSize(c.limit_bytes));
      kvText(cdl, 'idle_secs', num(c.idle_secs, 0));
      kvText(cdl, 'dehydrated_total', num(c.dehydrated_total, 0));
      kvText(cdl, 'freed_total_bytes', humanSize(c.freed_total_bytes));
      kvText(cdl, 'last_sweep_age_secs', num(c.last_sweep_age_secs, 0));
      kvText(cdl, 'blocked_dirty', num(c.blocked_dirty, 0));
      kvText(cdl, 'blocked_pinned', num(c.blocked_pinned, 0));
      kvText(cdl, 'blocked_open', num(c.blocked_open, 0));
      kvText(cdl, 'blocked_mapped', num(c.blocked_mapped, 0));
      kvText(cdl, 'blocked_inflight', num(c.blocked_inflight, 0));
      kvText(cdl, 'last_error', c.last_error ? String(c.last_error) : '—', c.last_error ? 'val-err' : null);
    }
    setCacheBar('cache2-bar-fill', 'cache2-bar-label', c);
  }

  function doSync(req, btnIds, label) {
    if (!requireDaemon()) { return Promise.resolve(null); }
    var r0 = { method: 'sync' };
    if (isObj(req)) {
      for (var k in req) {
        if (Object.prototype.hasOwnProperty.call(req, k)) { r0[k] = req[k]; }
      }
    }
    return withBusy({ spinners: ['sync-busy'], buttons: btnIds || [] }, function () {
      return ipc(r0);
    }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        logOk((label || 'sync') + '完成：refreshed=' + num(d.refreshed, 0) + ' conflicts=' + num(d.conflicts, 0) +
          ' uploaded=' + num(d.uploaded, 0) + ' deleted=' + num(d.deleted, 0) +
          ' blocked=' + num(d.deletes_blocked, 0));
      } else {
        logErr((label || 'sync') + '失败：' + str(r.error));
      }
      refreshStatus(true);
      return r;
    });
  }

  function doDehydrate(req, label) {
    if (!requireDaemon()) { return Promise.resolve(null); }
    var r0 = { method: 'dehydrate' };
    if (isObj(req)) {
      for (var k in req) {
        if (Object.prototype.hasOwnProperty.call(req, k)) { r0[k] = req[k]; }
      }
    }
    return withBusy({
      spinners: ['dehy-busy'],
      buttons: ['btn-dehy-dry', 'btn-dehy-all', 'btn-dehy-limit', 'btn-dehy-idle']
    }, function () {
      return ipc(r0);
    }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        var targets = (d.targets || []).length;
        var blocked = (d.blocked || []).length;
        renderResult('dehy-result', true, (label || '脱水') + (d.dry_run ? '（预演，未真正删除）' : ''), [
          ['脱水项', num(d.dehydrated, 0)],
          ['释放字节', humanSize(d.freed_bytes)],
          ['当前已用', humanSize(d.used_bytes)],
          ['限额', (d.limit_bytes === null || d.limit_bytes === undefined) ? '未设限额' : humanSize(d.limit_bytes)],
          ['targets', targets + ' 项'],
          ['被挡下', blocked + ' 项']
        ]);
        var list = (d.blocked || []);
        var box = $('dehy-result');
        if (box && list.length) {
          var ul = el('ul', 'plain-list mono');
          for (var i = 0; i < list.length && i < 20; i++) {
            var pair = list[i];
            var t = (pair && pair.length >= 2) ? (str(pair[0]) + ' —— ' + str(pair[1])) : str(pair);
            ul.appendChild(el('li', null, t));
          }
          box.appendChild(ul);
        }
      } else {
        renderError('dehy-result', (label || '脱水') + '失败', str(r.error));
      }
      refreshStatus(true);
      return r;
    });
  }

  // ============================================================ 状态轮询
  var inflight = null;   // 进行中的 daemon_status Promise（避免重叠请求）

  function refreshStatus(force) {
    if (inflight) { return inflight; }
    inflight = call('daemon_status', {}).then(function (r) {
      inflight = null;
      if (!r.ok) {
        renderTop(null);
        return r;
      }
      var st = isObj(r.data) ? r.data : null;
      state.lastStatus = st;
      renderTop(st);
      var s = (st && isObj(st.status)) ? st.status : null;
      if (s && s.mounts) { state.mounts = s.mounts; }
      if (state.page === 'home') {
        renderHome(st);
      } else if (state.page === 'tasks') {
        renderTasksPage();
      } else if (state.page === 'diag' && (state.diag === 'status' || state.diag === 'sync')) {
        renderStatusPanel(st);
      } else if (state.page === 'diag' && state.diag === 'mounts') {
        renderMountTable('tbl-mounts', 'mounts-empty', 'mounts-count', state.mounts);
      }
      return r;
    }, function (e) {
      inflight = null;
      throw e;
    });
    return inflight;
  }

  function startPolling() {
    if (state.pollTimer) { return; }
    state.pollTimer = window.setInterval(function () {
      if (document.hidden) { return; }
      if (inflight) { return; }
      refreshStatus(true);
      // ★ M8.4：每 3 个 tick（≈6s）看一次错误列表，有新错误就发桌面通知
      state.errorTick = (state.errorTick || 0) + 1;
      if (state.errorTick % 3 === 0) { checkErrorsForNotify(); }
    }, POLL_MS);
  }
  // 首轮 status 到位前切到某些页（用户手快，或 QXNYC_GUI_TAB 指定）时，
  // requireLogin() 还拿不到状态 → refreshFiles 会直接返回空态。status 回来后补一次。
  function refreshCurrentPage() {
    if (state.page === 'home') {
      renderHome(state.lastStatus);
      refreshMounts();
      refreshTasks();
    } else if (state.page === 'tasks') {
      refreshTasks();
    } else if (state.page === 'files' && !state.files.entries.length) {
      refreshFiles(state.files.path);
    } else if (state.page === 'settings') {
      loadConnect();
      refreshSettings();
    } else if (state.page === 'diag') {
      if (state.diag === 'mounts') { refreshMounts(); }
      else if (state.diag === 'status') { refreshRoots(); }
    }
  }

  // ============================================================ ★ M8.4 设置中心
  // 设置本体在 daemon 侧（settings.json）：GUI 只是编辑器 —— 读回来填表、改完整体回写，
  // 于是「GUI 与 CLI 改的是同一份设置」，不存在两套真值。

  /** 与 Rust 侧 `Settings::default()` 对齐的前端兜底（daemon 读不到时用）。 */
  function settingsDefaults() {
    return {
      version: 1,
      proxy: { mode: 'auto', server: '', port: null, auth: false, user: '', password: '' },
      launch_at_startup: false,
      desktop_notifications: true,
      debug_log: false,
      language: '',
      region: '',
      free_space: { auto: false, mode: 'below_pct', below_pct: 10, every_hours: 24 },
      close_to_tray: true
    };
  }

  /** 当前设置的**深拷贝**（改完整体回写，不污染 state）。 */
  function currentSettings() {
    var d = state.settings;
    if (d && isObj(d.settings)) {
      return JSON.parse(JSON.stringify(d.settings));
    }
    return settingsDefaults();
  }

  function proxyModeLabel(m) {
    if (m === 'none') { return 'No proxy（无代理）'; }
    if (m === 'manual') { return 'Manual（手动）'; }
    return 'Auto-detect（自动检测）';
  }

  function refreshSettings() {
    return ipc({ method: 'settings' }).then(function (r) {
      if (r.ok) {
        state.settings = isObj(r.data) ? r.data : null;
        state.settingsError = '';
      } else {
        logErr('读取设置失败：' + str(r.error));
        state.settingsError = str(r.error);
      }
      renderSettings();
      return r;
    });
  }

  function renderSettings() {
    var d = state.settings;
    var s = currentSettings();
    var proxy = isObj(s.proxy) ? s.proxy : settingsDefaults().proxy;
    var free = isObj(s.free_space) ? s.free_space : settingsDefaults().free_space;

    // ---- 代理
    var mode = $('p-mode');
    if (mode) { mode.value = str(proxy.mode) || 'auto'; }
    var server = $('p-server'); if (server) { server.value = str(proxy.server); }
    var port = $('p-port');
    if (port) { port.value = (proxy.port === null || proxy.port === undefined) ? '' : str(proxy.port); }
    var auth = $('p-auth'); if (auth) { auth.checked = proxy.auth === true; }
    var puser = $('p-user'); if (puser) { puser.value = str(proxy.user); }
    var ppw = $('p-password'); if (ppw) { ppw.value = ''; }
    toggleProxyFields();

    setText('proxy-mode-badge', proxyModeLabel(str(proxy.mode)));
    var env = (d && isObj(d.proxy_env)) ? d.proxy_env : {};
    var envDl = $('proxy-env-kv');
    if (envDl) {
      clear(envDl);
      var keys = Object.keys(env);
      if (!keys.length) {
        kvText(envDl, '环境变量', '（无 http_proxy / https_proxy / all_proxy）');
      } else {
        for (var i = 0; i < keys.length; i++) {
          kvText(envDl, keys[i], str(env[keys[i]]));
        }
      }
      kvText(envDl, '实际代理 URL', (d && d.proxy_url) ? str(d.proxy_url) : '（不走手动代理）');
    }
    setText('proxy-hint', state.settingsError
      ? T('settings.error', { msg: state.settingsError })
      : (d
        ? ('设置文件：' + str(d.path) + (d.saved ? '（已保存）' : '') + (d.note ? ' · ' + str(d.note) : ''))
        : T('settings.loading')));

    // ---- 个人
    var su = $('s-startup'); if (su) { su.checked = s.launch_at_startup === true; }
    var lang = $('s-lang'); if (lang) { lang.value = str(s.language); }
    var region = $('s-region'); if (region) { region.value = str(s.region); }
    var ct = $('s-close-tray'); if (ct) { ct.checked = s.close_to_tray !== false; }
    setText('autostart-hint', d
      ? ('autostart 桌面项：' + str(d.autostart_path) + '（' + (d.autostart_present ? '存在' : '不存在') + '）')
      : '—');

    // ---- 高级
    var dl = $('s-debug-log'); if (dl) { dl.checked = s.debug_log === true; }
    var nt = $('s-notifications'); if (nt) { nt.checked = s.desktop_notifications !== false; }

    // ---- 释放空间
    var fa = $('fs-auto'); if (fa) { fa.checked = free.auto === true; }
    var mp = $('fs-mode-pct'); if (mp) { mp.checked = str(free.mode) !== 'frequency'; }
    var mf = $('fs-mode-freq'); if (mf) { mf.checked = str(free.mode) === 'frequency'; }
    var bp = $('fs-below-pct'); if (bp) { bp.value = str(num(free.below_pct, 10)); }
    var eh = $('fs-every-hours'); if (eh) { eh.value = str(num(free.every_hours, 24)); }

    // ---- 冲突策略（全局只是"显示 + 每个任务在哪改"）
    renderConflictKv();

    // ---- 关于
    renderAbout();
  }

  /** 代理表单的联动（手动模式才显示服务器/认证；勾了认证才显示用户名口令）。 */
  function toggleProxyFields() {
    var mode = $('p-mode') ? str($('p-mode').value) : 'auto';
    show('p-manual-wrap', mode === 'manual');
    show('p-cred-wrap', mode === 'manual' && isOn('p-auth', false));
    show('p-port', mode === 'manual');
  }

  /** 把某个分区的表单值写回一份设置对象（不动别的分区）。 */
  function collectProxy(s) {
    var mode = $('p-mode') ? str($('p-mode').value) : 'auto';
    var oldPw = (s.proxy && isObj(s.proxy)) ? str(s.proxy.password) : '';
    var typedPw = str($('p-password') ? $('p-password').value : '');
    var p = {
      mode: mode,
      server: str($('p-server') ? $('p-server').value : '').trim(),
      port: null,
      auth: isOn('p-auth', false),
      user: str($('p-user') ? $('p-user').value : '').trim(),
      // 留空 = 不改动已存口令（否则每次保存都要重打一遍）
      password: typedPw ? typedPw : oldPw
    };
    var portV = str($('p-port') ? $('p-port').value : '').trim();
    if (portV) { p.port = num(portV, 0) || null; }
    s.proxy = p;
    return s;
  }

  function collectPersonal(s) {
    s.launch_at_startup = isOn('s-startup', false);
    s.language = str($('s-lang') ? $('s-lang').value : '').trim();
    s.region = str($('s-region') ? $('s-region').value : '').trim();
    s.close_to_tray = isOn('s-close-tray', true);
    return s;
  }

  function collectAdvanced(s) {
    s.debug_log = isOn('s-debug-log', false);
    s.desktop_notifications = isOn('s-notifications', true);
    return s;
  }

  function collectFree(s) {
    s.free_space = {
      auto: isOn('fs-auto', false),
      mode: isOn('fs-mode-freq', false) ? 'frequency' : 'below_pct',
      below_pct: num($('fs-below-pct') ? $('fs-below-pct').value : 10, 10),
      every_hours: num($('fs-every-hours') ? $('fs-every-hours').value : 24, 24)
    };
    return s;
  }

  /** 整体回写设置（带 autostart_exe：开机自启要写 GUI 自己的绝对路径）。 */
  function saveSettings(s, resultBox, okTitle, rows) {
    var done = function (exe) {
      var req = { method: 'settings_save', settings: s };
      if (exe) { req.autostart_exe = exe; }
      return withBusy({
        buttons: ['btn-proxy-save', 'btn-personal-save', 'btn-advanced-save', 'btn-free-save']
      }, function () {
        return ipc(req);
      }).then(function (r) {
        if (!r.ok) {
          renderError(resultBox, '保存失败', str(r.error));
          return r;
        }
        state.settings = isObj(r.data) ? r.data : null;
        renderSettings();
        var extra = rows || [];
        var out = [['设置文件', str(isObj(r.data) ? r.data.path : '')]].concat(extra);
        renderResult(resultBox, true, okTitle, out);
        logOk(okTitle);
        return r;
      });
    };
    // GUI 可执行文件路径：给 daemon 写 autostart 的 Exec=（拿不到就让 daemon 自己找同目录）
    return call('app_exe_path', {}).then(function (r) {
      var exe = (r.ok && isObj(r.data) && r.data.path) ? str(r.data.path) : '';
      return done(exe);
    }, function () { return done(''); });
  }

  function doProxySave() {
    return saveSettings(collectProxy(currentSettings()), 'proxy-result', '代理设置已保存', [
      ['模式', proxyModeLabel(str($('p-mode') ? $('p-mode').value : 'auto'))],
      ['提示', '手动代理对**已建立**的连接要等下次挂载才生效']
    ]);
  }

  function doPersonalSave() {
    return saveSettings(collectPersonal(currentSettings()), 'personal-result', '个人设置已保存', []);
  }

  function doAdvancedSave() {
    return saveSettings(collectAdvanced(currentSettings()), 'advanced-result', '高级设置已保存', []);
  }

  function doFreeSave() {
    return saveSettings(collectFree(currentSettings()), 'free-result', '释放空间设置已保存', []).then(function (r) {
      refreshSpace();
      return r;
    });
  }

  // ------------------------------------------------------------ 释放空间

  function refreshSpace() {
    state.spaceLoading = true;
    return ipc({ method: 'space' }).then(function (r) {
      state.spaceLoading = false;
      if (!r.ok) {
        logErr('读取释放空间状态失败：' + str(r.error));
        setText('space-reason', '读取失败：' + str(r.error));
        return r;
      }
      state.space = isObj(r.data) ? r.data : null;
      renderSpace();
      return r;
    });
  }

  function renderSpace() {
    var d = state.space;
    var dl = $('space-kv');
    if (!dl) { return; }
    clear(dl);
    if (!d) {
      kvText(dl, '状态', state.spaceLoading ? T('space.loading') : T('space.not_read'));
      return;
    }
    var total = num(d.fs_total, 0);
    var avail = num(d.fs_avail, 0);
    kvText(dl, '量空间', str(d.fs_path));
    kvText(dl, '文件系统', humanSize(total) + '（可用 ' + humanSize(avail) + '，' + num(d.fs_avail_pct, 0) + '%）');
    kvText(dl, '缓存占用', humanSize(num(d.cache_used_bytes, 0)));
    kvText(dl, '本轮判定', (d.would_run ? '会触发' : '不触发') + ' —— ' + str(d.reason));
    if (num(d.last_run_unix, 0) > 0) {
      kvText(dl, '上次触发', clock(num(d.last_run_unix, 0) * 1000));
    }
    if (d.injected === true) {
      kvRow(dl, '⚠ 注入', '正在使用 QXNYC_TEST_FAST_STATVFS 注入值（验收模式）', 'val-warn');
    }
    setText('space-used-label', '缓存 ' + humanSize(num(d.cache_used_bytes, 0)));

    // 进度条按「可用百分比」画：越窄越危险
    var pct = num(d.fs_avail_pct, 100);
    var fill = $('space-bar-fill');
    if (fill) { fill.style.width = Math.max(0, Math.min(100, pct)) + '%'; }
    setText('space-bar-label', '可用 ' + pct + '% · 缓存 ' + humanSize(num(d.cache_used_bytes, 0)));
    setText('space-reason', '判定依据：' + str(d.reason) + (d.note ? '　|　' + str(d.note) : ''));

    clearAlerts('space-blocked');
    if (isObj(d) && d.blocked && d.blocked.length) {
      addAlert('space-blocked', '被安全检查链挡下 ' + d.blocked.length + ' 项（例：' +
        str(d.blocked[0] && d.blocked[0][0]) + ' — ' + str(d.blocked[0] && d.blocked[0][1]) + '）');
    }
  }

  function doFreeNow() {
    return withBusy({ spinners: ['files-busy'], buttons: ['btn-free-now'] }, function () {
      return ipc({ method: 'space', now: true });
    }).then(function (r) {
      if (!r.ok) {
        renderError('free-result', '立即释放空间失败', str(r.error));
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      state.space = d;
      renderSpace();
      renderResult('free-result', true, '立即释放空间完成', [
        ['脱水', str(num(d.dehydrated, 0)) + ' 个'],
        ['释放', humanSize(num(d.freed_bytes, 0))],
        ['被挡下', str((d.blocked || []).length) + ' 个（安全检查链）'],
        ['缓存现在', humanSize(num(d.cache_used_bytes, 0))]
      ]);
      return r;
    });
  }

  // ------------------------------------------------------------ 筛选器（link.exclude）

  function refreshFilters() {
    return call('link_read', { linkId: 'default' }).then(function (r) {
      if (!r.ok || !isObj(r.data)) {
        setText('flt-note', '读取连接配置失败：' + str(r.error));
        return r;
      }
      state.link = isObj(r.data.link) ? r.data.link : null;
      var link = state.link;
      var lines = (link && link.exclude && link.exclude.length) ? link.exclude : [];
      var ta = $('flt-lines');
      if (ta) { ta.value = lines.join('\n'); }
      var ft = $('flt-filter-temp');
      if (ft) { ft.checked = !(link && link.filter_temp === false); }
      return refreshRulesPreview();
    });
  }

  function refreshRulesPreview() {
    return ipc({ method: 'rules' }).then(function (r) {
      var note = $('flt-note');
      if (!note) { return r; }
      if (!r.ok) {
        note.hidden = false;
        note.textContent = '规则读取失败（daemon 未运行？）：' + str(r.error);
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      var parts = [
        '生效 ' + ((d.patterns || []).length) + ' 条规则（挂载点里会被剪掉）',
        '临时文件过滤 ' + (d.filter_temp ? '开' : '关')
      ];
      if (d.bad && d.bad.length) { parts.push('⚠ 解析不了：' + d.bad.join(', ')); }
      note.hidden = false;
      note.textContent = parts.join(' · ');
      return r;
    });
  }

  function doFiltersSave() {
    var raw = str($('flt-lines') ? $('flt-lines').value : '');
    var lines = [];
    var arr = raw.split('\n');
    for (var i = 0; i < arr.length; i++) {
      var v = arr[i].trim();
      if (v) { lines.push(v); }
    }
    if (!lines.length) {
      // ⚠ link_save 的语义是「空数组 = 保留旧值」，所以清空要用一条注释行表达
      //   （规则解析器忽略 `#` 开头的行，效果等价于「没有任何排除规则」）。
      lines = ['# 已清空（GUI）'];
    }
    var link = state.link || {};
    if (!link.host || !link.user) {
      renderError('flt-match-result', '无法保存筛选器', '连接配置里缺少 host/user：先在「连接」分区保存一次连接配置');
      return Promise.resolve(null);
    }
    var input = {
      id: link.id || 'default',
      host: str(link.host),
      port: num(link.port, 9834),
      https: link.https !== false,
      insecure: link.insecure === true,
      user: str(link.user),
      home_root: str(link.home_root) || '/home',
      roots: (link.roots && link.roots.length) ? link.roots : [],
      ipv4_only: link.ipv4_only === true,
      exclude: lines,
      filter_temp: isOn('flt-filter-temp', true)
    };
    return withBusy({ buttons: ['btn-flt-save'] }, function () {
      return call('link_save', { input: input });
    }).then(function (r) {
      if (!r.ok) {
        renderError('flt-match-result', '保存筛选器失败', str(r.error));
        return r;
      }
      renderResult('flt-match-result', true, '筛选器已保存', [
        ['规则', lines.join(' , ')],
        ['文件', str(isObj(r.data) ? r.data.path : '')],
        ['生效时机', '要在**已挂载**的任务上生效，需要重启 daemon（规则在挂载时注入 FUSE 与同步引擎）']
      ]);
      refreshFilters();
      return r;
    });
  }

  function doMatchPreview() {
    var p = str($('flt-match') ? $('flt-match').value : '').trim();
    if (!p) {
      renderError('flt-match-result', '参数错误', '请输入一条远端绝对路径，例如 /home/qxync-test/secret.bin');
      return Promise.resolve(null);
    }
    return ipc({ method: 'rules', match_path: p }).then(function (r) {
      if (!r.ok) {
        renderError('flt-match-result', '判定失败', str(r.error));
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      var verdict = '可见（会同步）';
      if (d.match_hidden === true) {
        verdict = (d.match_reason === 'temp') ? '隐藏（临时文件规则）' : '隐藏（命中排除规则）';
      } else if (d.match_root === null || d.match_root === undefined) {
        verdict = '不在任何配置的远端根之内';
      }
      renderResult('flt-match-result', true, '判定：' + verdict, [
        ['路径', p],
        ['归属根', d.match_root ? str(d.match_root) : '（不在任何根内）'],
        ['根相对路径', d.match_rel ? str(d.match_rel) : '—'],
        ['原因', str(d.match_reason) || '—']
      ]);
      return r;
    });
  }

  // ------------------------------------------------------------ 冲突策略 / 待裁决

  function conflictLabel(v) {
    if (v === 'ask') { return '每个文件都问我'; }
    if (v === 'rename_remote') { return '重命名 NAS 上的文件'; }
    if (v === 'replace_remote') { return '用本地文件替换 NAS 上的文件（⚠ 会丢远端改动）'; }
    if (v === 'replace_local') { return '用 NAS 上的文件替换本地文件（⚠ 会丢本地改动）'; }
    return '重命名本地文件（默认，双方都不丢）';
  }

  function taskList() {
    var d = state.tasks;
    var out = [];
    if (d && d.tasks && d.tasks.length) {
      for (var i = 0; i < d.tasks.length; i++) {
        var t = d.tasks[i] && d.tasks[i].task ? d.tasks[i].task : null;
        if (t) { out.push(t); }
      }
    }
    return out;
  }

  function renderConflictKv() {
    var dl = $('conflict-kv');
    if (!dl) { return; }
    clear(dl);
    var ts = taskList();
    if (!ts.length) {
      kvText(dl, '任务', '（还没有任务；策略在任务的「文件夹对设置」里选）');
      return;
    }
    for (var i = 0; i < ts.length; i++) {
      kvText(dl, str(ts[i].id), conflictLabel(str(ts[i].conflict) || 'rename_local'));
    }
  }

  function refreshDecisions() {
    state.decisionsLoading = true;
    state.decisionsError = '';
    renderDecisions();
    return ipc({ method: 'decisions', action: 'list' }).then(function (r) {
      state.decisionsLoading = false;
      if (!r.ok) {
        logErr('读取冲突待裁决队列失败：' + str(r.error));
        state.decisionsError = str(r.error);
        renderDecisions();
        return r;
      }
      state.decisions = isObj(r.data) ? r.data : null;
      renderDecisions();
      return r;
    });
  }

  function renderDecisions() {
    var box = $('decisions-list');
    if (!box) { return; }
    var d = state.decisions;
    var items = (d && d.decisions) ? d.decisions : [];
    clear(box);
    setBusyState('decisions-list', state.decisionsLoading === true);
    setText('decisions-count', items.length ? (items.length + ' 条（待裁决 ' + num(d.pending, 0) + '）') : '');
    if (state.decisionsError) {
      setListState('decisions-empty', 'error', T('list.error', { msg: state.decisionsError }));
    } else if (!d) {
      setListState('decisions-empty', 'loading', T('decisions.loading'));
    } else if (items.length === 0) {
      setListState('decisions-empty', 'empty', T('decisions.empty'));
    } else {
      setListState('decisions-empty', 'ready');
    }
    var note = $('decisions-note');
    if (note) {
      note.hidden = !(d && d.note);
      note.textContent = d && d.note ? str(d.note) : '';
    }
    for (var i = 0; i < items.length; i++) {
      box.appendChild(buildDecisionCard(items[i]));
    }
  }

  function buildDecisionCard(x) {
    var card = el('div', 'task-card');
    var body = el('div', 'task-body');
    body.appendChild(el('div', 'task-state', pathBase(str(x.path)) + (x.resolution ? '（已裁决，待执行）' : '（待裁决）')));
    body.appendChild(el('div', 'task-meta', '远端 ' + str(x.path)));
    body.appendChild(el('div', 'task-meta',
      '本地 ' + humanSize(num(x.local_size, 0)) + ' / 远端 ' + humanSize(num(x.remote_size, 0)) +
      ' · 发现于 ' + clock(num(x.created_unix, 0) * 1000)));
    card.appendChild(body);

    var acts = el('div', 'task-actions');
    var mk = function (label, res, cls) {
      var b = el('button', 'mini' + (cls ? ' ' + cls : ''), label);
      b.type = 'button';
      b.addEventListener('click', function () { doDecisionResolve(str(x.id), res, b); });
      return b;
    };
    acts.appendChild(mk('保留本地', 'keep_local', 'primary'));
    acts.appendChild(mk('保留 NAS 上的', 'keep_remote'));
    acts.appendChild(mk('两份都留', 'keep_both'));
    card.appendChild(acts);
    return card;
  }

  function doDecisionResolve(id, res, btn) {
    return withBusy({ buttons: btn ? [btn.id] : [] }, function () {
      return ipc({ method: 'decisions', action: 'resolve', id: id, resolution: res });
    }).then(function (r) {
      if (!r.ok) {
        logErr('裁决失败：' + str(r.error));
        return r;
      }
      logOk('已裁决 ' + id + ' → ' + res + '（下一轮同步执行）');
      refreshDecisions();
      return r;
    });
  }

  // ------------------------------------------------------------ LAN 加速

  function refreshLan() {
    state.lanLoading = true;
    return ipc({ method: 'peer', action: 'status' }).then(function (r) {
      if (!r.ok) {
        state.lanLoading = false;
        logErr('读取 LAN 状态失败：' + str(r.error));
        return r;
      }
      renderLanStatus(isObj(r.data) ? r.data : null);
      return Promise.all([
        ipc({ method: 'peer', action: 'list' }),
        ipc({ method: 'peer', action: 'events', limit: 10 })
      ]).then(function (rs) {
        state.lanLoading = false;
        renderLanDevices(rs[0].ok ? rs[0].data : null);
        renderLanEvents(rs[1].ok ? rs[1].data : null);
        return r;
      });
    });
  }

  function renderLanStatus(d) {
    var dl = $('lan-status-kv');
    if (!dl) { return; }
    clear(dl);
    if (!d) {
      kvText(dl, '状态', 'daemon 未运行或未响应');
      return;
    }
    kvBool(dl, '启用', d.enabled === true, '监听中', '未监听');
    kvText(dl, '绑定地址', d.listen ? str(d.listen) : '（未监听）');
    kvText(dl, '身份名', str(d.identity));
    kvText(dl, '配对码', d.pairing_code ? str(d.pairing_code) : '（没有开启配对）');
    kvText(dl, '已配对设备', str((d.devices || []).length) + ' 台');
    kvText(dl, '事件', '发出 ' + num(d.events_out, 0) + ' / 收到 ' + num(d.events_in, 0) +
      ' / 拒绝 ' + num(d.rejected, 0));
    kvText(dl, 'LAN 命中', num(d.lan_hits, 0) + ' 次，' + humanSize(num(d.lan_bytes, 0)));
    var link = state.link;
    var listen = $('lan-listen');
    if (listen && link) { listen.value = link.peer_listen ? str(link.peer_listen) : ''; }
    var name = $('lan-name');
    if (name && link) { name.value = link.peer_name ? str(link.peer_name) : ''; }
  }

  function renderLanDevices(d) {
    var ul = $('lan-devices');
    if (!ul) { return; }
    clear(ul);
    var devs = (d && d.devices) ? d.devices : [];
    if (devs.length) { setListState('lan-devices-empty', 'ready'); }
    else if (state.lanLoading) { setListState('lan-devices-empty', 'loading', T('lan.devices_loading')); }
    else { setListState('lan-devices-empty', 'empty', T('lan.devices_empty')); }
    for (var i = 0; i < devs.length; i++) {
      ul.appendChild(el('li', null, str(devs[i].name) + '  ' + str(devs[i].addr) + '  ' + str(devs[i].token_masked)));
    }
  }

  function renderLanEvents(d) {
    var ul = $('lan-events');
    if (!ul) { return; }
    clear(ul);
    var evs = (d && d.events) ? d.events : [];
    if (evs.length) { setListState('lan-events-empty', 'ready'); }
    else if (state.lanLoading) { setListState('lan-events-empty', 'loading', T('lan.events_loading')); }
    else { setListState('lan-events-empty', 'empty', T('lan.events_empty')); }
    for (var i = 0; i < evs.length; i++) {
      ul.appendChild(el('li', null, clock(num(evs[i].ts, 0) * 1000) + '  ' + str(evs[i].kind) + '  ' + str(evs[i].path)));
    }
  }

  function doLanSave() {
    var link = state.link;
    if (!link || !link.host || !link.user) {
      renderError('lan-result', '无法保存', '先到「连接」分区保存一次连接配置');
      return Promise.resolve(null);
    }
    var listen = str($('lan-listen') ? $('lan-listen').value : '').trim();
    var name = str($('lan-name') ? $('lan-name').value : '').trim();
    var input = {
      id: link.id || 'default',
      host: str(link.host),
      port: num(link.port, 9834),
      https: link.https !== false,
      insecure: link.insecure === true,
      user: str(link.user),
      home_root: str(link.home_root) || '/home',
      roots: (link.roots && link.roots.length) ? link.roots : [],
      ipv4_only: link.ipv4_only === true,
      // 空串 = 关闭监听（daemon 侧 `peer_listen` 去空白后为空就不监听）
      peer_listen: listen,
      peer_name: name
    };
    return withBusy({ buttons: ['btn-lan-save'] }, function () {
      return call('link_save', { input: input });
    }).then(function (r) {
      if (!r.ok) {
        renderError('lan-result', '保存 LAN 设置失败', str(r.error));
        return r;
      }
      renderResult('lan-result', true, 'LAN 设置已保存', [
        ['监听', listen || '（关闭）'],
        ['设备名', name || '（主机名）'],
        ['生效', '需要重启 daemon']
      ]);
      refreshFilters();
      return r;
    });
  }

  function doLanPair() {
    var addr = str($('lan-pair-addr') ? $('lan-pair-addr').value : '').trim();
    var code = str($('lan-pair-code') ? $('lan-pair-code').value : '').trim();
    if (!addr || !code) {
      renderError('lan-result', '参数错误', '需要「地址 + 配对码」两样：先在对方机器上跑 qxync peer status 拿配对码');
      return Promise.resolve(null);
    }
    return withBusy({ buttons: ['btn-lan-pair'] }, function () {
      return ipc({ method: 'peer', action: 'pair', addr: addr, code: code });
    }).then(function (r) {
      if (!r.ok) {
        renderError('lan-result', '配对失败', str(r.error));
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      renderResult('lan-result', true, '配对成功', [
        ['设备名', str(d.paired_name)],
        ['地址', str(d.paired_addr)],
        ['令牌', str(d.paired_token_masked)]
      ]);
      refreshLan();
      return r;
    });
  }

  // ------------------------------------------------------------ 关于 / 快捷出口

  function renderAbout() {
    var dl = $('about-kv');
    if (!dl) { return; }
    clear(dl);
    var info = state.appInfo;
    kvText(dl, 'qxync', info ? str(info.version) : '—');
    kvText(dl, '说明', 'FUSE 按需同步 + qxyncd 常驻守护；对齐 Qsync Client 6.1 的界面与术语');
    kvText(dl, 'socket', info ? str(info.socket) : '—');
    kvText(dl, '配置目录', info ? str(info.config_dir) : '—');
    kvText(dl, '数据目录', info ? str(info.data_dir) : '—');
    kvText(dl, '状态目录', info ? str(info.state_dir) : '—');
    var st = state.lastStatus;
    var s = (st && isObj(st.status)) ? st.status : null;
    if (s && isObj(s.link)) {
      kvText(dl, 'NAS', str(s.link.host) + ':' + str(s.link.port));
    }
    kvBool(dl, 'daemon', !!(s || (info && info.daemon_running)),
      '运行中', '未运行');
    var note = $('about-m84');
    if (note) {
      if (!state.m84Info) {
        note.textContent = '托盘/通知/选择器（M8.4）：GUI 侧命令未就绪时这里显示 —';
      } else {
        // ★ 「建起来」≠「有人画」：这里如实区分三态，别让用户以为托盘一定可见
        var m = state.m84Info;
        var tray;
        if (m.tray_visible === true) { tray = '可见（面板会画）'; }
        else if (m.tray_probed !== true) { tray = '探测中…'; }
        else if (m.tray_created === true) { tray = '不可见（名字注册了但没有面板宿主）'; }
        else { tray = '不可用（没有 StatusNotifierWatcher）'; }
        note.textContent = '托盘：' + tray +
          (m.tray_reason ? '　' + str(m.tray_reason) : '') +
          '　| 插件：' + ((m.plugins || []).join(' / ') || '—') +
          '　| 关窗：' + (m.close_to_tray ? '收进托盘' : '直接关闭') +
          '　| 托盘事件：' + (state.trayAction ? state.trayAction : '（还没触发过）');
      }
    }
  }

  function m84InfoRefresh() {
    return call('m84_info', {}).then(function (r) {
      state.m84Info = (r.ok && isObj(r.data)) ? r.data : null;
      renderAbout();
      return r;
    }, function () { return { ok: false }; });
  }

  /** 打开一个路径/URL（走 GUI 的 opener 命令；未就绪时只记日志，不假装成功）。 */
  function doOpen(what, target) {
    var cmd = (what === 'url') ? 'open_url' : 'open_path';
    var args = (what === 'url') ? { url: target } : { path: target };
    return call(cmd, args).then(function (r) {
      if (r.ok && isObj(r.data) && r.data.ok === false) {
        logErr('打开失败：' + str(r.data.error));
      } else if (!r.ok) {
        logErr('打开 ' + target + ' 失败：' + str(r.error));
      } else {
        logOk('已请求打开：' + target);
      }
      return r;
    });
  }

  function fileStationUrl() {
    var st = state.lastStatus;
    var s = (st && isObj(st.status)) ? st.status : null;
    var link = (s && isObj(s.link)) ? s.link : (state.link || null);
    if (!link || !link.host) { return ''; }
    var scheme = link.https === false ? 'http' : 'https';
    return scheme + '://' + str(link.host) + ':' + str(link.port) + '/cgi-bin/filemanager/index.html';
  }

  // ------------------------------------------------------------ 桌面通知

  /** 发一条系统通知（尊重设置里的「显示桌面通知」；GUI 侧命令未就绪时静默降级）。 */
  function notifyUser(title, body) {
    return call('notify_show', { title: title, body: body });
  }

  /**
   * 轮询里顺带看一下「错误列表」有没有新条目 —— 有新错误就发一条桌面通知。
   * 刻意**不通知成功项**：Qsync 的「每个活动都通知」在 Linux 桌面上是噪声。
   */
  function checkErrorsForNotify() {
    return ipc({ method: 'journal', level: 'error', limit: 1 }).then(function (r) {
      if (!r.ok) { return r; }
      var d = isObj(r.data) ? r.data : {};
      var es = d.entries || [];
      if (!es.length) { return r; }
      var e = es[0];
      var key = str(e.ts) + '|' + str(e.path) + '|' + str(e.detail);
      if (state.lastErrorKey === key) { return r; }
      state.lastErrorKey = key;
      if (state.settings && isObj(state.settings.settings) &&
          state.settings.settings.desktop_notifications === false) {
        return r;   // 用户关掉了通知
      }
      notifyUser('qxync 同步出错', str(e.path) + ' —— ' + str(e.detail));
      return r;
    }, function () { return { ok: false }; });
  }

  // ============================================================ 页面切换
  /** 旧值 → 新目的地（`QXNYC_GUI_TAB` 的历史值必须继续可用）。 */
  function resolvePage(name) {
    if (LEGACY_TABS[name]) { return LEGACY_TABS[name]; }
    // `diag:<status|mounts|sync>` —— 给验收矩阵用的「直达诊断子页」写法
    if (name.indexOf('diag:') === 0) {
      var sub = name.slice(5);
      if (DIAG_PANELS.indexOf(sub) >= 0) { return { page: 'diag', diag: sub }; }
    }
    // ★ M8.4：`settings:<connect|proxy|sync|personal|advanced|free|lan|about>`
    //   —— 一个目的地里的八个分区，验收矩阵逐个出图
    if (name.indexOf('settings:') === 0) {
      var sec = name.slice(9);
      if (SETTINGS_SECS.indexOf(sec) >= 0) { return { page: 'settings', sec: sec }; }
    }
    if (PAGES.indexOf(name) >= 0) { return { page: name, diag: null }; }
    return null;
  }

  /** 切一级目的地；`diagPanel` 只在 page=diag 时有意义，`sec` 只在 page=settings 时有意义。 */
  function switchPage(name, diagPanel, sec) {
    var r = resolvePage(name) || { page: 'home', diag: null };
    name = r.page;
    if (r.diag) { diagPanel = r.diag; }
    if (r.sec) { sec = r.sec; }

    state.page = name;
    if (diagPanel && DIAG_PANELS.indexOf(diagPanel) >= 0) { state.diag = diagPanel; }

    var items = document.querySelectorAll('.rail-item');
    for (var i = 0; i < items.length; i++) {
      var it = items[i];
      if (it.getAttribute('data-page') === name) {
        it.classList.add('active');
        it.setAttribute('aria-current', 'page');     // ★ M8.6：读屏能播报「当前页」
      } else {
        it.classList.remove('active');
        it.removeAttribute('aria-current');
      }
    }

    var pages = document.querySelectorAll('.page');
    for (var j = 0; j < pages.length; j++) {
      var p = pages[j];
      if (p.id === 'page-' + name) { p.classList.add('active'); }
      else { p.classList.remove('active'); }
    }
    setText('page-title', PAGE_TITLE[name] || name);

    if (name === 'home') {
      renderHome(state.lastStatus);
      refreshMounts();
      refreshTasks();
    } else if (name === 'tasks') {
      refreshTasks();
      refreshDecisions();
    } else if (name === 'journal') {
      refreshJournal();
    } else if (name === 'errors') {
      refreshErrors();
    } else if (name === 'files') {
      if (!state.files.entries.length) { refreshFiles(state.files.path); }
      else { renderFiles(); refreshFileStates(); }
    } else if (name === 'settings') {
      switchSettingsSec(sec || state.sec);
      loadConnect();
      refreshSettings();
      if (state.sec === 'free') { refreshSpace(); }
      if (state.sec === 'lan') { refreshLan(); }
    } else if (name === 'diag') {
      switchDiag(state.diag);
    }
  }

  /** ★ M8.4：切设置页内的分区。 */
  function switchSettingsSec(name) {
    if (SETTINGS_SECS.indexOf(name) < 0) { name = 'connect'; }
    state.sec = name;
    var tabs = document.querySelectorAll('#settings-tabs .tab');
    for (var i = 0; i < tabs.length; i++) {
      var t = tabs[i];
      if (t.getAttribute('data-sec') === name) {
        t.classList.add('active');
        t.setAttribute('aria-selected', 'true');     // ★ M8.6
      } else {
        t.classList.remove('active');
        t.setAttribute('aria-selected', 'false');
      }
    }
    var secs = document.querySelectorAll('#page-settings .settings-sec');
    for (var j = 0; j < secs.length; j++) {
      var s = secs[j];
      if (s.id === 'sec-' + name) { s.classList.add('active'); }
      else { s.classList.remove('active'); }
    }
    setText('page-title', T('page.settings_sec', { sec: SEC_TITLE[name] || name }));
    if (name === 'free') { refreshSpace(); }
    if (name === 'lan') { refreshLan(); }
    if (name === 'about') { renderAbout(); }
    if (name === 'sync') { refreshFilters(); }
  }

  /** 切诊断页内的子 tab（状态 / 挂载 / 同步缓存）。 */
  function switchDiag(name) {
    if (DIAG_PANELS.indexOf(name) < 0) { name = 'status'; }
    var prev = state.diag;
    state.diag = name;

    var tabs = document.querySelectorAll('#diag-tabs .tab');
    for (var i = 0; i < tabs.length; i++) {
      var t = tabs[i];
      if (t.getAttribute('data-panel') === name) {
        t.classList.add('active');
        t.setAttribute('aria-selected', 'true');     // ★ M8.6
      } else {
        t.classList.remove('active');
        t.setAttribute('aria-selected', 'false');
      }
    }

    var panels = document.querySelectorAll('#page-diag .panel');
    for (var j = 0; j < panels.length; j++) {
      var p = panels[j];
      if (p.id === 'panel-' + name) { p.classList.add('active'); }
      else { p.classList.remove('active'); }
    }

    if (name === 'status' || name === 'sync') {
      renderStatusPanel(state.lastStatus);
      // 切到「状态」拉一次远端根（roots 贵，不跟 2s 轮询）；重复点同一子页不重复拉
      if (name === 'status' && prev !== 'status') { refreshRoots(); }
    }
    if (name === 'mounts') { refreshMounts(); }
  }

  // ============================================================ 日志面板
  function wireLogBox() {
    var head = $('logbox-head');
    var box = $('logbox');
    var toggle = $('log-toggle');
    if (head && box) {
      var flip = function () {
        box.classList.toggle('collapsed');
        var collapsed = box.classList.contains('collapsed');
        if (toggle) { toggle.textContent = collapsed ? T('log.expand') : T('log.collapse'); }
        // ★ M8.6：折叠头是 div，补上 role=button 的展开状态
        head.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
      };
      head.addEventListener('click', function (ev) {
        if (ev.target && ev.target.id === 'btn-log-clear') { return; }
        flip();
      });
      // ★ M8.6：Enter / Space 也能折叠（鼠标之外的第二条路）
      head.addEventListener('keydown', function (ev) {
        if (ev.target !== head) { return; }
        if (ev.key === 'Enter' || ev.key === ' ' || ev.key === 'Spacebar') {
          ev.preventDefault();
          flip();
        }
      });
    }
    var clearBtn = $('btn-log-clear');
    if (clearBtn) {
      clearBtn.addEventListener('click', function (ev) {
        ev.stopPropagation();
        clear($('log-body'));
        state.logCount = 0;
        setText('log-count', '0 条');
      });
    }
  }

  // ============================================================ 事件绑定
  function bindEvents() {
    // 左侧图标栏（一级目的地）
    // ★ M8.6：图标栏除了可点，还要能用键盘走 —— ↑↓/←→ 移焦点，Home/End 到首尾，
    //   Enter/Space 由 button 原生触发（不需要手写）。
    var rail = document.querySelectorAll('.rail-item');
    for (var i = 0; i < rail.length; i++) {
      (function (b, idx) {
        b.addEventListener('click', function () {
          switchPage(str(b.getAttribute('data-page')));
        });
        b.addEventListener('keydown', function (ev) {
          var list = document.querySelectorAll('.rail-item');
          var next = -1;
          if (ev.key === 'ArrowDown' || ev.key === 'ArrowRight') { next = (idx + 1) % list.length; }
          else if (ev.key === 'ArrowUp' || ev.key === 'ArrowLeft') { next = (idx - 1 + list.length) % list.length; }
          else if (ev.key === 'Home') { next = 0; }
          else if (ev.key === 'End') { next = list.length - 1; }
          if (next < 0) { return; }
          ev.preventDefault();
          if (list[next] && isFn(list[next].focus)) { list[next].focus(); }
        });
      })(rail[i], i);
    }

    // 诊断页内的子 tab
    var dtabs = document.querySelectorAll('#diag-tabs .tab');
    for (var k = 0; k < dtabs.length; k++) {
      (function (t) {
        t.addEventListener('click', function () {
          switchDiag(str(t.getAttribute('data-panel')));
        });
      })(dtabs[k]);
    }

    // ★ M8.6：设置分区 / 诊断子页的 tablist 支持 ←→（Home/End）切换
    wireTabKeys('#settings-tabs', function (t) { switchSettingsSec(str(t.getAttribute('data-sec'))); });
    wireTabKeys('#diag-tabs', function (t) { switchDiag(str(t.getAttribute('data-panel'))); });

    // ★ M8.3 更新中心 / 错误列表
    on('btn-journal-refresh', 'click', function () { refreshJournal(); });
    on('btn-journal-clear', 'click', function () { doJournalClear(); });
    on('btn-errors-refresh', 'click', function () { refreshErrors(); });
    var jq = $('j-query');
    if (jq) {
      jq.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter') { ev.preventDefault(); refreshJournal(); }
      });
    }
    var jl = $('j-level');
    if (jl) { jl.addEventListener('change', function () { refreshJournal(); }); }

    // ★ M8.2 任务页
    on('btn-tasks-refresh', 'click', function () { refreshTasks(); });
    on('btn-tasks-add', 'click', function () { openTaskForm(); });
    on('btn-task-cancel', 'click', function () { closeTaskForm(); });
    var ft = $('form-task');
    if (ft) {
      ft.addEventListener('submit', function (ev) {
        ev.preventDefault();
        doTaskSave();
      });
    }

    // 主页快捷动作
    on('btn-home-settings', 'click', function () { switchPage('settings'); });
    on('btn-home-add-task', 'click', function () { switchPage('diag', 'mounts'); });
    on('btn-home-refresh', 'click', function () { refreshStatus(true); refreshMounts(); });
    on('btn-home-sync-now', 'click', function () {
      doSync({ once: true }, ['btn-home-sync-now'], '立即同步');
    });

    // 顶部动作
    on('btn-daemon-start', 'click', function () { doDaemonStart(); });
    on('btn-daemon-stop', 'click', function () { doDaemonStop(); });
    on('btn-quick-login', 'click', function () { doQuickLogin(); });

    // 连接页
    var f = $('form-connect');
    if (f) {
      f.addEventListener('submit', function (ev) {
        ev.preventDefault();
        doLinkSave();
      });
    }
    on('btn-login', 'click', function () { doLogin(); });
    on('btn-connect-start', 'click', function () { doDaemonStart(); });
    on('btn-connect-stop', 'click', function () { doDaemonStop(); });
    on('btn-app-info-refresh', 'click', function () { loadConnect(); });

    // 挂载页
    var fm = $('form-mount');
    if (fm) {
      fm.addEventListener('submit', function (ev) {
        ev.preventDefault();
        doMount();
      });
    }
    on('btn-mounts-refresh', 'click', function () { refreshMounts(); });
    on('btn-status-mounts-refresh', 'click', function () { refreshMounts(); });

    // 远端根面板
    on('btn-roots-refresh', 'click', function () { refreshRoots(); });

    // 文件页
    var fp = $('form-path');
    if (fp) {
      fp.addEventListener('submit', function (ev) {
        ev.preventDefault();
        refreshFiles(str($('fp-path') ? $('fp-path').value : '/home'));
      });
    }
    on('btn-path-up', 'click', function () { refreshFiles(pathParent(state.files.dir || state.files.path)); });
    on('btn-path-refresh', 'click', function () { refreshFiles(state.files.dir || state.files.path); });
    on('fp-hide-excluded', 'change', function () { renderFiles(); });
    on('btn-mkdir', 'click', function () { doMkdir(); });
    on('btn-rm', 'click', function () { doRm(); });

    // 同步页
    on('btn-sync-once', 'click', function () { doSync({ once: true }, ['btn-sync-once'], '立即同步'); });
    on('btn-sync-force', 'click', function () {
      doSync({ once: true, force_deletes: true }, ['btn-sync-force'], '强制放行删除');
    });
    on('btn-sync-pause', 'click', function () { doSync({ interval_secs: 0 }, ['btn-sync-pause'], '暂停轮询'); });
    on('btn-sync-interval', 'click', function () {
      var v = num($('sy-interval') ? $('sy-interval').value : 60, 60);
      if (v <= 0) { logErr('轮询间隔必须 > 0（暂停请用「暂停轮询」按钮）'); return; }
      doSync({ interval_secs: v }, ['btn-sync-interval'], '设置轮询间隔 ' + v + 's');
    });

    on('btn-dehy-dry', 'click', function () {
      doDehydrate({ dry_run: true, all: true }, '脱水预演');
    });
    on('btn-dehy-all', 'click', function () {
      doDehydrate({ dry_run: false, all: true, force: false }, '全部脱水');
    });
    on('btn-dehy-limit', 'click', function () {
      var lim = str($('dehy-limit') ? $('dehy-limit').value : '').trim();
      if (!lim) { logErr('请输入限额，例如 512M / 2G / 25%'); return; }
      doDehydrate({ cache_limit: lim }, '按限额脱水 ' + lim);
    });
    on('btn-dehy-idle', 'click', function () {
      var idle = num($('dehy-idle') ? $('dehy-idle').value : 0, 0);
      doDehydrate({ idle_secs: idle }, '释放闲置 ≥ ' + idle + 's');
    });

    // ★ M8.4：设置中心
    var stabs = document.querySelectorAll('#settings-tabs .tab');
    for (var si = 0; si < stabs.length; si++) {
      (function (t) {
        t.addEventListener('click', function () {
          switchSettingsSec(str(t.getAttribute('data-sec')));
        });
      })(stabs[si]);
    }
    on('btn-settings-refresh', 'click', function () { refreshSettings(); });
    on('btn-proxy-save', 'click', function () { doProxySave(); });
    on('btn-personal-save', 'click', function () { doPersonalSave(); });
    on('btn-advanced-save', 'click', function () { doAdvancedSave(); });
    on('btn-free-save', 'click', function () { doFreeSave(); });
    on('btn-free-now', 'click', function () { doFreeNow(); });
    on('btn-space-refresh', 'click', function () { refreshSpace(); });
    on('btn-flt-save', 'click', function () { doFiltersSave(); });
    on('btn-flt-reload', 'click', function () { refreshFilters(); });
    on('btn-flt-match', 'click', function () { doMatchPreview(); });
    on('btn-lan-save', 'click', function () { doLanSave(); });
    on('btn-lan-status', 'click', function () { refreshLan(); });
    on('btn-lan-pair', 'click', function () { doLanPair(); });
    on('btn-about-refresh', 'click', function () { loadConnect(); m84InfoRefresh(); renderAbout(); });
    on('btn-notify-test', 'click', function () {
      notifyUser('qxync 测试通知', '如果你看到这条，说明桌面通知链路是通的。').then(function (r) {
        if (r.ok && isObj(r.data) && r.data.ok === false) {
          renderError('advanced-result', '通知未发出', str(r.data.error || r.data.reason));
        } else if (r.ok) {
          renderResult('advanced-result', true, '已请求发送测试通知', [
            ['说明', '如果系统没弹窗，检查通知守护进程与「免打扰」设置']
          ]);
        } else {
          renderError('advanced-result', '通知命令不可用', str(r.error));
        }
      });
    });
    on('btn-open-logdir', 'click', function () {
      var info = state.appInfo;
      // daemon/GUI 的日志在状态目录的 log/ 下
      doOpen('path', info ? (str(info.state_dir) + '/log') : '');
    });
    on('btn-open-config', 'click', function () {
      var info = state.appInfo;
      doOpen('path', info ? str(info.config_dir) : '');
    });
    on('btn-open-station', 'click', function () {
      var u = fileStationUrl();
      if (!u) { renderError('about-result', '打不开', '还没有连接配置（先在「连接」分区保存）'); return; }
      doOpen('url', u);
      renderResult('about-result', true, '已请求打开 File Station', [
        ['URL', u],
        ['说明', 'File Station 的 Web 路径在不同 QTS 版本上略有差异，打不开就手动访问 NAS 首页']
      ]);
    });
    var unsupported = {
      'btn-unsupported-thumb': '创建缩略图属于 NAS 侧媒体索引优化，需要引入图像依赖与额外通道 —— 本客户端不做。',
      'btn-unsupported-backup': '备份任务是 Qsync 6.0 的旗舰功能，但要一整套单向引擎 + 调度器；qxync 的定位是 Linux 按需同步，备份请用 HBS 3 / rsync / restic。',
      'btn-unsupported-version': '「以前版本 / 还原」依赖 NAS 版本控制，本机实测 versioning_support 恒为 0 —— 已决策不做。'
    };
    Object.keys(unsupported).forEach(function (id) {
      on(id, 'click', function () {
        var note = $('unsupported-note');
        if (note) { note.hidden = false; note.textContent = unsupported[id]; }
      });
    });
    on('btn-decisions-refresh', 'click', function () { refreshDecisions(); });

    // 页面可见性恢复时立刻刷一次
    document.addEventListener('visibilitychange', function () {
      if (!document.hidden) { refreshStatus(true); }
    });
  }

  /**
   * ★ M8.4：托盘菜单动作。
   * Rust 侧只负责 emit（`tray://action`），真正的动作在前端做 —— 前端持有 IPC 与界面状态。
   */
  function wireTray() {
    if (!tauri || !tauri.event || !isFn(tauri.event.listen)) {
      return;   // 老版本/非 Tauri 环境：托盘事件不可用（不影响其它功能）
    }
    tauri.event.listen('tray://action', function (ev) {
      var action = str(ev && ev.payload);
      state.trayAction = action;
      logInfo('托盘动作：' + action);
      if (action === 'open') {
        call('window_show', {});
        switchPage('home');
      } else if (action === 'sync') {
        doSync({ once: true }, [], '托盘：立即与 NAS 同步');
      } else if (action === 'pause') {
        doSync({ interval_secs: 0 }, [], '托盘：暂停轮询');
      }
      renderAbout();
    });
  }

  function on(id, evt, fn) {
    var n = $(id);
    if (n) { n.addEventListener(evt, fn); }
  }

  /**
   * ★ M8.6：给 `role="tablist"` 的分段控件接方向键。
   * 只移动焦点 + 立即切换（跟随焦点），符合 APG 的 tabs 键盘交互；Tab 键仍可离开控件。
   */
  function wireTabKeys(containerSel, onPick) {
    var box = document.querySelector(containerSel);
    if (!box) { return; }
    box.addEventListener('keydown', function (ev) {
      var tabs = box.querySelectorAll('.tab');
      var cur = -1;
      for (var i = 0; i < tabs.length; i++) {
        if (tabs[i] === document.activeElement) { cur = i; }
      }
      if (cur < 0) { return; }
      var next = -1;
      if (ev.key === 'ArrowRight' || ev.key === 'ArrowDown') { next = (cur + 1) % tabs.length; }
      else if (ev.key === 'ArrowLeft' || ev.key === 'ArrowUp') { next = (cur - 1 + tabs.length) % tabs.length; }
      else if (ev.key === 'Home') { next = 0; }
      else if (ev.key === 'End') { next = tabs.length - 1; }
      if (next < 0) { return; }
      ev.preventDefault();
      if (isFn(tabs[next].focus)) { tabs[next].focus(); }
      if (isFn(onPick)) { onPick(tabs[next]); }
    });
  }

  // ============================================================ 启动
  function boot() {
    // ★ M8.6：先把静态文案套一遍（表在 i18n.js；DOM 里挂错 key 会在下面报出来）
    if (I18N && isFn(I18N.apply)) {
      I18N.apply(document);
      document.title = T('app.title');
    }
    wireModal();
    wireLogBox();
    wireRowMenu();
    wireTray();
    bindEvents();

    if (!invokeFn) {
      var banner = $('env-banner');
      if (banner) { banner.hidden = false; }
      logErr('未在 Tauri 中运行：window.__TAURI__.core.invoke 不存在，界面只做静态展示');
    } else {
      logInfo('qxync GUI 已启动，开始轮询 daemon_status（每 2s）');
    }

    // ★ M8.6：i18n 一致性 —— DOM 里挂了 data-i18n 但表里没有的 key 直接报出来（拼错立刻可见）
    if (I18N && isFn(I18N.missingInDOM)) {
      var miss = I18N.missingInDOM(document);
      if (miss.length) {
        logErr('i18n：DOM 里有 ' + miss.length + ' 个 key 不在文案表里 —— ' + miss.join(', '));
      } else if (isFn(I18N.stats)) {
        logInfo('i18n：' + I18N.locale() + ' 文案表已应用（' + I18N.stats().zh_keys + ' 条，en 预留）');
      }
    }

    // 先拿 app_info（挂载点默认值 / 路径面板 / HOME）
    call('app_info', {}).then(function (r) {
      if (r.ok) {
        renderAppInfo(isObj(r.data) ? r.data : null);
        // 调试/验收用：环境变量 QXNYC_GUI_TAB 指定初始目的地
        //   新值：home|tasks|files|journal|errors|settings|diag
        //   旧值（M8.1 之前，必须继续可用）：status|mounts|sync→诊断；connect→设置；files
        // 后端 app_info 原样透传，截图矩阵靠它逐个目的地出图。
        var want = isObj(r.data) ? str(r.data.initial_tab) : '';
        if (want && resolvePage(want)) { switchPage(want); }
      }
    });

    loadConnect();
    refreshSettings();
    m84InfoRefresh();
    refreshDecisions();
    refreshStatus(true).then(function () { startPolling(); refreshCurrentPage(); },
                             function () { startPolling(); refreshCurrentPage(); });
    refreshMounts();
    refreshTasks();
    renderFiles();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
