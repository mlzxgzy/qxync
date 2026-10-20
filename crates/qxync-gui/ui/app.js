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

  // ★ M16.1：任务表单「NAS 连接」下拉里的伪选项 —— 选中它 = 去新建连接（不是一条真连接）。
  var NEW_LINK_OPTION = '__new__';

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

  // ★ M18：任务默认路径的纯函数（任务 id / 本地文件夹 / 水合缓存）。
  //   单独放在 ui/task-defaults.js：能 node 单测，也能被无头浏览器集成测试复用。
  //   资源没加载（旧包）时退化成 null —— 表单只是不再自动派生，不会抛错。
  var TD = (typeof window !== 'undefined' && window.QXNYC_TASK_DEFAULTS)
    ? window.QXNYC_TASK_DEFAULTS : null;

  // --------------------------------------------------------------- ★ M16 多连接状态
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
    mounts: [],          // 当前连接的挂载表（诊断 / 主页退化展示用）
    busy: 0,
    home: '',
    appInfo: null,
    pollTimer: null,
    logCount: 0,
    // ★ M8.4
    sec: 'connect',       // 设置页内的分区
    settings: null,       // settings 请求的最近一次结果（全局，所有连接共享一份文件）
    link: null,           // link_read 的最近一次结果（**当前连接**的 LAN / 筛选器）
    space: null,          // space 请求的最近一次结果（当前连接）
    fileStates: {},       // 文件名 → 三态（file_states 请求，当前连接）
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
    lastErrorKey: '',     // 已通知过的错误条目（去重，按「连接:条目」）
    errorTick: 0,         // 轮询计数器（每 3 次查一遍错误列表）
    rowMenu: null,        // 右键菜单当前作用的条目
    trayAction: '',       // 最近一次托盘动作（自检/排查用）

    // ★ M16：多 NAS 连接。
    //   `links` 是 links_status 的权威列表（含「没有 link 文件但 default daemon 在跑」
    //   的合成项）；`statuses` 按 id 存每个 daemon 的状态；其余按连接分桶的字段只存
    //   「最近一次 fanout 的结果」，页面上再合并。
    links: [],            // [{id, host, port, https, insecure, user, link_exists,
                          //   credential_present, socket, daemon}]
    linksBad: [],         // [{path, error}]：解析失败、被跳过的 link 文件
    linksLoading: true,   // 首轮 links_status 没回来前，连接列表显示「读取中」而不是「没有连接」
    linksError: '',
    linksLoaded: false,   // 首轮 links_status 是否已回来（顶部 chip 的「检测中」）
    statuses: {},         // linkId -> DaemonStatus
    currentLink: '',      // 当前连接 id（持久化到 localStorage 的 qxync.currentLink）
    editingLinkId: '',    // 连接表单：'' = 新建
    creatingLink: false,  // ★ M16.1：'' 有两种含义（「展示当前连接」/「正在新建」），用它区分
    taskFormLink: '',     // 任务表单：这条任务属于哪个连接
    taskFormEditing: false,
    // ★ M18：新任务表单里哪些字段还是「自动派生」的。
    //   用户一旦手改某个字段，就把对应标志置 false —— 之后换 NAS 文件夹 / 换连接
    //   都不会再把用户的输入覆盖掉（点「重置为默认路径」可全部恢复 true）。
    taskFormAuto: { id: true, mountpoint: true, cache: true },
    tasksByLink: {},      // linkId -> {data, error}
    journalByLink: {},    // linkId -> {data, error}
    errorsByLink: {},     // linkId -> {data, error}
    decisionsByLink: {},  // linkId -> {data, error}
    currentChanged: false,// 本轮 links_status 是否切换了当前连接
    linksSig: ''          // 连接列表的渲染签名（没变就不重画，别打断键盘焦点）
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

  /**
   * 时间戳 → `HH:MM:SS`（只用于「刚刚发生的日志/事件」；要日期请用 humanTimeFromEpoch）。
   *
   * ★ 以前这里写成 `isObj(ts) || ts instanceof Date ? new Date(ts) : new Date()`：
   *   **数字时间戳不是对象**，于是传进来的 epoch 被丢掉、恒显示「现在」——
   *   冲突卡片曾因此把每条的「发现于」都显示成当前时刻。
   */
  function clock(ts) {
    var d;
    if (ts instanceof Date) { d = ts; }
    else if (typeof ts === 'number' && isFinite(ts)) { d = new Date(ts); }
    else { d = new Date(); }
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
      // 注意：绝不记录 password，也**绝不记录两步验证密钥/验证码**（只记「填了」）。
      return 'host=' + str(inp.host) + ' user=' + str(inp.user) + ' port=' + str(inp.port) +
        (inp.password ? ' password=***' : '') +
        (inp.otp_secret ? ' otp=***' : '');
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

  /** ipc_call 的语义化包装：**显式指定连接**（新代码一律用它）。 */
  function ipcFor(linkId, req) {
    var id = str(linkId) || 'default';
    if (isObj(req) && req.v === undefined) { req.v = 1; }
    return call('ipc_call', { linkId: id, req: req }).then(function (r) {
      var res = r.raw;
      if (!r.ok) { return { ok: false, error: r.error, kind: r.kind, linkId: id }; }
      if (isObj(res) && res.ok === true) {
        return { ok: true, data: res.data, raw: res, linkId: id };
      }
      var msg = (isObj(res) && isObj(res.error)) ? str(res.error.message) : '调用失败';
      return { ok: false, error: msg, raw: res, linkId: id };
    });
  }

  /** 当前连接的 ipc —— 「文件 / 诊断 / 当前连接的同步动作」都走它。 */
  function ipc(req) { return ipcFor(currentId(), req); }

  /** 并行 fanout 同一个请求到多个连接：返回 [{id, link, running, r}]（顺序与入参一致）。 */
  function fanout(req, targets) {
    var ts = targets || fanoutTargets();
    var jobs = [];
    for (var i = 0; i < ts.length; i++) {
      (function (t) {
        jobs.push(ipcFor(t.id, req).then(function (r) {
          return { id: t.id, link: t.link, running: t.running, r: r };
        }, function (e) {
          return { id: t.id, link: t.link, running: t.running, r: { ok: false, error: str(e) } };
        }));
      })(ts[i]);
    }
    return Promise.all(jobs);
  }
  // ============================================================ 忙碌标记
  /**
   * 按钮引用解析：既接受 index.html 里静态按钮的 **id 字符串**，也接受**元素本身**。
   *
   * ★ 为什么必须支持元素：卡片里的按钮是 `el('button')` 动态建的、**没有 id**。
   *   以前调用方写 `withBusy({ buttons: btn ? [btn.id] : [] })` → `btn.id === ''`
   *   → `getElementById('')` 恒 null → 「飞行中禁用」完全没生效，连点就会发重复请求
   *   （裁决/忽略这种会改队列的操作尤其不能重复发）。
   */
  function busyButton(ref) {
    if (!ref) { return null; }
    if (typeof ref === 'string') { return $(ref); }
    return (typeof ref === 'object' && ref.nodeType === 1) ? ref : null;
  }

  function setBusy(on, ids, btnIds) {
    state.busy += on ? 1 : -1;
    if (state.busy < 0) { state.busy = 0; }
    var i;
    for (i = 0; i < (ids || []).length; i++) {
      var n = $(ids[i]);
      if (n) { n.hidden = !on; }
    }
    for (i = 0; i < (btnIds || []).length; i++) {
      var b = busyButton(btnIds[i]);
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

  /** 当前连接（或指定连接）的 daemon 是否在跑。 */
  function requireDaemon(linkId) {
    var id = str(linkId) || currentId();
    var st = statusOf(id);
    if (!st || !st.running) {
      logInfo('连接 ' + linkLabelById(id) + ' 的 qxyncd 未运行，操作已取消（先点「启动 daemon」）');
      return false;
    }
    return true;
  }

  function requireLogin(linkId) {
    var id = str(linkId) || currentId();
    var st = statusOf(id);
    if (!st || !st.running) {
      logInfo('连接 ' + linkLabelById(id) + ' 的 qxyncd 未运行，请先启动 daemon');
      return false;
    }
    var status = st.status;
    if (!status || !status.logged_in) {
      logInfo('连接 ' + linkLabelById(id) + ' 当前未登录，请到「设置 → 连接」登录');
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
    // ★ M23：确认模式可能改过输入框可见性 / OK 文案 —— 关闭时一律还原，
    //   下一个 openModal 拿到的还是干净的输入弹窗。
    var input = $('modal-input');
    if (input) { input.hidden = false; }
    var okBtn = $('modal-ok');
    if (okBtn) { okBtn.textContent = T('modal.ok'); }
    if (modalLastFocus && isFn(modalLastFocus.focus)) { modalLastFocus.focus(); }
    modalLastFocus = null;
    var r = modalResolve;
    modalResolve = null;
    if (isFn(r)) { r(value); }
  }

  /**
   * ★ M23：确认弹窗（复用输入弹窗的 DOM，但**藏掉输入框**、可自定义 OK 文案）。
   * 返回 Promise<boolean>：OK=true / 取消=false。closeModal 负责还原现场。
   */
  function openConfirm(title, message, okLabel) {
    var box = $('modal');
    if (!box) { return Promise.resolve(false); }
    var p = openModal(title, message, '');
    var input = $('modal-input');
    if (input) {
      input.hidden = true;
      // wireModal 里 Enter 提交的是 input.value —— 藏掉后按 Enter 也应该等于 OK
    }
    var okBtn = $('modal-ok');
    if (okBtn) { okBtn.textContent = okLabel || T('modal.ok'); }
    if (okBtn) { okBtn.focus(); }
    return p.then(function (v) { return v !== null; });
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

  // ==================================================== ★ purge（删除可带相关数据）
  // 三层（任务卡 / 连接行 / 设置→高级）共用同一个弹窗：后端先给一份 purge 计划
  // （`purge_plan`），前端把「会删什么」摊开，复选框**默认不勾选** = 保持旧行为。
  // 条目一律用 createElement + textContent 构造（路径用 mono 样式），不拼 HTML。
  //
  // 弹出的是独立弹窗（#purge-modal），**不复用** #modal：后者是「一个输入框」的形状，
  // 而且 focus/Enter 的语义是「返回输入值」，与这里的「确认 + 一个复选框」完全不同。
  var purgeResolve = null;
  var purgeLastFocus = null;
  var purgeOptRequired = false;

  /** 计划条目里的 kind → 文案 key。未知 kind 回落到后端给的原字符串（绝不显示成 key）。 */
  var PURGE_KIND_KEYS = {
    task: 'purge.kind.task',
    link: 'purge.kind.link',
    credentials: 'purge.kind.credentials',
    peers: 'purge.kind.peers',
    cache: 'purge.kind.cache',
    sync: 'purge.kind.sync',
    queue: 'purge.kind.queue',
    log: 'purge.kind.log',
    mounts: 'purge.kind.mounts',
    socket: 'purge.kind.socket',
    pid: 'purge.kind.pid',
    config: 'purge.kind.config',
    data: 'purge.kind.data',
    state: 'purge.kind.state'
  };

  function purgeKindLabel(kind) {
    var k = str(kind);
    var key = PURGE_KIND_KEYS[k];
    if (!key) { return k; }
    var v = T(key);
    return (v === key) ? k : v;
  }

  function purgeSizeText(bytes, files) {
    var b = num(bytes, 0);
    var f = num(files, 0);
    if (f > 0) { return T('purge.total', { size: humanSize(b), files: f }); }
    return humanSize(b);
  }

  /** 计划条目列表：label · 种类徽标 · 路径（mono）· 大小；optional 的标「仅勾选后删除」。 */
  function fillPurgeItems(items) {
    var ul = $('purge-items');
    if (!ul) { return; }
    clear(ul);
    var list = Array.isArray(items) ? items : [];
    for (var i = 0; i < list.length; i++) {
      var it = list[i];
      if (!isObj(it)) { continue; }
      var optional = it.optional === true;
      var kindText = str(it.kind) ? purgeKindLabel(it.kind) : '';
      var labelText = str(it.label) || kindText;
      var li = el('li', 'purge-item' + (optional ? ' purge-item-opt' : ''));
      li.appendChild(el('span', 'purge-item-label', labelText));
      // 后端 label 一般就是 kind 的中文名：相同就不再挂一个重复徽标。
      if (kindText && kindText !== labelText) {
        li.appendChild(document.createTextNode(' '));
        li.appendChild(el('span', 'badge', kindText));
      }
      if (str(it.path)) {
        li.appendChild(el('span', 'purge-item-sep', ' · '));
        li.appendChild(el('span', 'mono purge-item-path', str(it.path)));
      }
      if (num(it.bytes, 0) > 0 || num(it.files, 0) > 0) {
        li.appendChild(el('span', 'purge-item-sep', ' · '));
        li.appendChild(el('span', 'purge-item-size', purgeSizeText(it.bytes, it.files)));
      }
      if (optional) {
        li.appendChild(document.createTextNode(' '));
        li.appendChild(el('span', 'badge badge-warn purge-item-mark', T('purge.optional_mark')));
      }
      ul.appendChild(li);
    }
  }

  /** 「合计 + db_rows」一行：baseline / 日志 / 待裁决 与总量（都为空就不显示）。 */
  function purgeExtraText(o) {
    var parts = [];
    var db = isObj(o.dbRows) ? o.dbRows : null;
    if (db) {
      var b = num(db.baseline, 0);
      var j = num(db.journal, 0);
      var d = num(db.decisions, 0);
      if (b + j + d > 0) {
        parts.push(T('purge.db_rows', { baseline: b, journal: j, decisions: d }));
      }
    }
    if (num(o.totalBytes, 0) > 0 || num(o.totalFiles, 0) > 0) {
      parts.push(T('purge.total', { size: humanSize(o.totalBytes), files: num(o.totalFiles, 0) }));
    }
    return parts.join(' · ');
  }

  /** 后端的 warnings：一条一个黄色 alert（文字一律 textContent）。 */
  function fillPurgeWarnings(list) {
    var box = $('purge-warn');
    if (!box) { return; }
    clear(box);
    var ws = Array.isArray(list) ? list : [];
    for (var i = 0; i < ws.length; i++) {
      box.appendChild(el('div', 'alert alert-warn', T('purge.warn_prefix') + str(ws[i])));
    }
  }

  /**
   * 打开删除确认弹窗 → Promise<{confirmed, purge}>。
   *
   *   confirmed=false：取消 / Esc / 点遮罩（什么都不删）
   *   purge=true     ：勾了复选框（调用方走「连本地数据一起删」的新命令）
   *   optRequired    ：危险操作（purge_all）必须勾选，未勾选时确认按钮 disabled。
   */
  function openPurgeDialog(opts) {
    var box = $('purge-modal');
    if (!box) { return Promise.resolve({ confirmed: false, purge: false }); }
    var o = isObj(opts) ? opts : {};
    // 防御：同一时刻只允许一个 purge 弹窗（连点/键盘重复触发）。旧的按「取消」收掉，
    // 否则它的 Promise 永远不会 resolve，调用方就一直挂着。
    if (isFn(purgeResolve)) {
      var stale = purgeResolve;
      purgeResolve = null;
      stale({ confirmed: false, purge: false });
    }
    setText('purge-title', str(o.title));
    setText('purge-message', str(o.message));
    fillPurgeItems(o.items);
    fillPurgeWarnings(o.warnings);

    var extra = purgeExtraText(o);
    var extraEl = $('purge-extra');
    if (extraEl) {
      extraEl.hidden = !extra;
      extraEl.textContent = extra;
    }
    var optLabel = $('purge-opt-label');
    if (optLabel) { optLabel.textContent = str(o.optLabel); }
    var optHint = $('purge-opt-hint');
    if (optHint) {
      optHint.hidden = !str(o.optHint);
      optHint.textContent = str(o.optHint);
    }
    var opt = $('purge-opt');
    if (opt) { opt.checked = false; }
    purgeOptRequired = o.optRequired === true;
    var ok = $('purge-ok');
    if (ok) {
      ok.textContent = str(o.confirmText) || T('purge.confirm_delete');
      ok.disabled = purgeOptRequired;
    }

    purgeLastFocus = document.activeElement;
    box.hidden = false;
    if (opt) { opt.focus(); }
    else {
      var cancelBtn = $('purge-cancel');
      if (cancelBtn) { cancelBtn.focus(); }
    }
    return new Promise(function (resolve) {
      purgeResolve = resolve;
    });
  }

  function closePurgeDialog(confirmed) {
    var box = $('purge-modal');
    var opt = $('purge-opt');
    var optOn = !!(opt && opt.checked === true);
    // 必须勾选却没勾：把「确认」当取消处理（按钮本来 disabled，这里再兜一层）
    var yes = (confirmed === true) && (!purgeOptRequired || optOn);
    if (box) { box.hidden = true; }
    if (purgeLastFocus && isFn(purgeLastFocus.focus)) { purgeLastFocus.focus(); }
    purgeLastFocus = null;
    var r = purgeResolve;
    purgeResolve = null;
    if (isFn(r)) { r({ confirmed: yes, purge: yes && optOn }); }
  }

  function wirePurgeModal() {
    var box = $('purge-modal');
    var ok = $('purge-ok');
    var cancel = $('purge-cancel');
    var opt = $('purge-opt');
    if (ok) { ok.addEventListener('click', function () { closePurgeDialog(true); }); }
    if (cancel) { cancel.addEventListener('click', function () { closePurgeDialog(false); }); }
    if (opt) {
      opt.addEventListener('change', function () {
        var b = $('purge-ok');
        if (b) { b.disabled = purgeOptRequired ? (opt.checked !== true) : false; }
      });
    }
    if (box) {
      // 点遮罩 = 取消（焦点在弹窗内，Esc / Tab 与 #modal 同一套处理）
      box.addEventListener('click', function (ev) {
        if (ev.target === box) { closePurgeDialog(false); }
      });
      box.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape') { ev.preventDefault(); closePurgeDialog(false); return; }
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

  // ★ M22：补上第 4 参（cls）—— 之前 kvText 把调用方传的 val-dim / val-ok /
  // val-warn / val-err 静默丢掉，诊断页这些行的颜色标注从来没生效过。
  function kvText(dl, key, value, cls) { kvRow(dl, key, value, cls); }

  function kvBool(dl, key, on, onText, offText) {
    kvRow(dl, key, on ? (onText || '是') : (offText || '否'), on ? 'val-ok' : 'val-dim');
  }

  function optText(v) {
    if (v === undefined || v === null || v === '') { return ''; }
    return String(v);
  }

  // ==================================================== ★ M16 多连接：取数 / 命名
  //
  // 约定（贯穿整个前端）：
  //   * 只要请求是「按连接」的（文件 / 诊断 / 该连接的同步动作），一律走
  //     `ipcFor(linkId, req)`；`ipc(req)` 只是「当前连接」的简写。
  //   * 跨连接的展示（任务 / 更新 / 错误 / 待裁决）一律 `fanout`，并把来源连接
  //     标在条目上；**绝不把 A 的错误算到 B 头上**。
  var CURRENT_LINK_KEY = 'qxync.currentLink';

  function linksList() { return state.links || []; }

  function linkById(id) {
    var want = str(id);
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      if (ls[i] && str(ls[i].id) === want) { return ls[i]; }
    }
    return null;
  }

  function statusOf(linkId) { return state.statuses[str(linkId)] || null; }

  function currentStatus() { return statusOf(state.currentLink); }

  /** 连接的可读名：`user@host:port`；没有 link 文件（空转 daemon / 未配置）说「未配置」。 */
  function linkLabel(link) {
    if (!link) { return T('links.unconfigured'); }
    if (link.link_exists === false || (!str(link.host) && !str(link.user))) {
      return T('links.unconfigured');
    }
    return str(link.user) + '@' + str(link.host) + ':' + str(link.port);
  }

  /** 卡片 / 条目上的连接徽标（带 id：同一台 NAS 上可能存在多个账号的连接）。 */
  function linkBadge(link) {
    if (!link) { return T('links.unconfigured'); }
    return str(link.id) + ' · ' + linkLabel(link);
  }

  function linkLabelById(id) {
    var l = linkById(id);
    return l ? linkLabel(l) : str(id);
  }

  function currentLinkEntry() { return linkById(state.currentLink); }

  function linkDaemonRunning(link) { return !!(link && link.daemon && link.daemon.running); }

  /** 当前连接的 id（给 ipc_call / link_* 用）：空列表时退回 'default'，与后端默认一致。 */
  function currentId() { return state.currentLink || 'default'; }

  function loadSavedLink() {
    try { return str(window.localStorage.getItem(CURRENT_LINK_KEY)); } catch (e) { return ''; }
  }

  function saveCurrentLink(id) {
    try { window.localStorage.setItem(CURRENT_LINK_KEY, str(id)); } catch (e) { /* 隐私模式忽略 */ }
  }

  /** 让 currentLink 落在一个真实存在的连接上：优先持久化值，失效则回退第一个。 */
  function ensureCurrentLink() {
    var prev = state.currentLink;
    var saved = prev || loadSavedLink();
    if (saved && linkById(saved)) {
      state.currentLink = str(saved);
    } else {
      var ls = linksList();
      state.currentLink = ls.length ? str(ls[0].id) : '';
    }
    saveCurrentLink(state.currentLink);
    return state.currentLink !== prev;
  }

  /** 切连接时要清掉的「属于上一个连接」的缓存（不清会串台）。 */
  function resetLinkScopedState() {
    state.mounts = [];
    state.link = null;
    state.space = null;
    state.fileStates = {};
    state.fsSummary = null;
    state.rootsLoaded = false;
    state.files = { path: '/home', dir: '/home', entries: [], selected: null, loading: false, error: '' };
    var fp = $('fp-path');
    if (fp) { fp.value = '/home'; }
  }

  /** 切换当前连接：换掉所有「按连接」的缓存，并重拉当前页。 */
  function setCurrentLink(id) {
    var link = linkById(id);
    if (!link) { return Promise.resolve(null); }
    if (str(id) === state.currentLink) { renderConnSelect(); return Promise.resolve(null); }
    state.currentLink = str(id);
    saveCurrentLink(state.currentLink);
    resetLinkScopedState();
    renderTop();
    renderConnSelect();
    renderConnList();
    renderFiles();
    if (state.page === 'settings') { loadConnect(); refreshFilters(); refreshLan(); }
    logInfo('切换当前连接：' + linkBadge(link));
    // 让 refreshStatus 统一重拉当前页（切连接 = 所有按连接的数据都过期了）
    state.currentChanged = true;
    return refreshStatus(true);
  }

  /** 全局 fanout 的目标：所有**运行中**的连接；一个都没跑时仍对当前连接试一次
   *  （这样旧后端 / 离线夹具 / 「所有 daemon 都停了」时也能拿到 IPC 层的准确错误，
   *   而不是整页假装「没有数据」）。 */
  function runningTargets() {
    var ls = linksList();
    var out = [];
    for (var i = 0; i < ls.length; i++) {
      if (linkDaemonRunning(ls[i])) {
        out.push({ id: str(ls[i].id), link: ls[i], running: true });
      }
    }
    return out;
  }

  function fanoutTargets() {
    var out = runningTargets();
    if (out.length) { return out; }
    var id = currentId();
    return [{ id: id, link: linkById(id), running: false }];
  }

  /** daemon 没在跑的连接（给列表页显示「连接 X 的 daemon 未运行」）。 */
  function stoppedLinks() {
    var ls = linksList();
    var out = [];
    for (var i = 0; i < ls.length; i++) {
      if (!linkDaemonRunning(ls[i])) { out.push(ls[i]); }
    }
    return out;
  }

  /** 所有配置连接的 daemon 都没跑？（整页失败时换成这句人话，而不是 socket 报错原文） */
  function noDaemonRunning() {
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      if (linkDaemonRunning(ls[i])) { return false; }
    }
    return true;
  }

  /** `stoppedLinks()` → 一句可见说明（列表页顶部用）。 */
  function stoppedLinksNote() {
    var stopped = stoppedLinks();
    if (!stopped.length) { return ''; }
    var names = [];
    for (var i = 0; i < stopped.length; i++) { names.push(linkLabel(stopped[i])); }
    return T('list.conn_no_daemon', { links: names.join('、') });
  }

  // ============================================================ 顶部状态条（多连接聚合）
  function renderTop() {
    var chipD = $('chip-daemon');
    var chipC = $('chip-conn');
    var chipL = $('chip-login');
    if (!chipD) { return; }

    if (!state.linksLoaded) {
      // 首轮状态还没回来：别把「检测中」显示成「未运行」
      chipD.textContent = state.linksError ? T('top.daemon_error') : T('top.daemon_checking');
      chipD.className = 'chip ' + (state.linksError ? 'chip-err' : 'chip-unknown');
      chipC.textContent = T('top.conn_dash');
      chipC.className = 'chip chip-unknown';
      chipL.textContent = T('top.login_dash');
      chipL.className = 'chip chip-unknown';
      setText('meta-daemon', state.linksError
        ? T('top.meta_error', { msg: state.linksError })
        : T('top.daemon_meta_down', { socket: '—' }));
      setText('meta-conn', T('top.meta_dash'));
      setText('meta-session', T('top.session_dash'));
      return;
    }

    var ls = linksList();
    var total = ls.length;
    var up = 0;
    var logged = 0;
    for (var i = 0; i < ls.length; i++) {
      var d = (ls[i] && isObj(ls[i].daemon)) ? ls[i].daemon : null;
      if (d && d.running) {
        up++;
        if (isObj(d.status) && d.status.logged_in === true) { logged++; }
      }
    }

    // daemon chip：N/M 运行
    chipD.textContent = total ? T('top.daemon_count', { up: up, total: total }) : T('top.daemon_down');
    chipD.className = 'chip ' + (total === 0 ? 'chip-err' : (up === 0 ? 'chip-err' : (up === total ? 'chip-ok' : 'chip-warn')));

    // 连接 chip：当前连接
    var cur = currentLinkEntry();
    if (!total) {
      chipC.textContent = T('top.conn_dash');
      chipC.className = 'chip chip-unknown';
    } else if (!cur || (cur.link_exists === false && !str(cur.host))) {
      chipC.textContent = T('top.conn_unconfigured');
      chipC.className = 'chip chip-warn';
    } else {
      chipC.textContent = T('top.conn', { value: linkLabel(cur) });
      chipC.className = 'chip chip-ok';
    }

    // login chip：已登录 N/M
    if (!total) {
      chipL.textContent = T('top.login_dash');
      chipL.className = 'chip chip-unknown';
    } else {
      chipL.textContent = T('top.login_count', { up: logged, total: total });
      chipL.className = 'chip ' + (logged === total ? 'chip-ok' : 'chip-warn');
    }

    // meta：**当前连接**的 daemon / socket / session（按连接分开显示，不混算）
    var st = currentStatus();
    var ping = (st && isObj(st.ping)) ? st.ping : null;
    if (!st || !st.running) {
      setText('meta-daemon', T('top.daemon_meta_down', { socket: str(st ? st.socket : (cur ? cur.socket : '')) }));
      setText('meta-conn', cur ? T('top.conn_meta', { user: str(cur.user), host: str(cur.host), port: str(cur.port) }) : T('top.meta_dash'));
      setText('meta-session', T('top.session_dash'));
      return;
    }
    setText('meta-daemon', T('top.daemon_meta', {
      pid: str(ping ? ping.pid : '—'), version: str(ping ? ping.daemon_version : '—'),
      uptime: humanDuration(ping ? ping.uptime_secs : 0), socket: str(st.socket)
    }));
    setText('meta-conn', cur
      ? T('top.conn_meta', { user: str(cur.user), host: str(cur.host), port: str(cur.port) })
      : T('top.meta_dash'));
    var s = isObj(st.status) ? st.status : null;
    var sess = (s && isObj(s.session)) ? s.session : null;
    setText('meta-session', sess
      ? T('top.session_meta', {
          sid: str(sess.sid_masked),
          // ★ M22：alive=false 且 alive_known=false 是「不知道」（离线未探测），
          // 不是「失活」；alive=true 只可能来自真探测。
          state: (sess.alive ? 'alive' : (sess.alive_known === false ? 'unknown' : 'dead'))
        })
      : T('top.session_dash'));
  }
  // ============================================================ 主页 / 任务卡片（★ M16 按连接聚合）
  /**
   * 把一个「同步任务」的状态压成一句话 + 一个状态标记（对齐 Qsync 的任务卡片语言）。
   * ⚠️ 只做**诚实**的归纳：拿不到证据就不说「已同步」。
   *
   * ★ M16：`st` / `sy` 必须是**该任务所属连接**的状态，不能拿当前连接的去套。
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
        return { mark: '⚠️', tone: 'warn', text: T('task.state_delete_blocked'), blocked: true };
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

  /** 某个连接上的挂载点（优先用该连接 status 里的 mounts；当前连接再看 state.mounts）。 */
  function mountsForLink(id) {
    var st = statusOf(id);
    var s = (st && isObj(st.status)) ? st.status : null;
    if (s && Array.isArray(s.mounts)) { return s.mounts; }
    return (str(id) === state.currentLink) ? (state.mounts || []) : [];
  }

  /**
   * ★ M16：把「任务登记」按**来源连接**分组。
   *
   * 每组 = 一个连接：`{link, id, views, error}`，`views` 里每项都带 `linkId`。
   * 任务登记为空时退回该连接的挂载点（M8.1 的展示），**同样带连接标记** ——
   * 这样主页不会出现「一张没有归属的卡片」。
   */
  function taskGroups() {
    var out = [];
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var link = ls[i];
      var id = str(link.id);
      var entry = state.tasksByLink[id] || null;
      var data = (entry && entry.data) ? entry.data : null;
      var err = (entry && entry.error) ? entry.error : '';
      var infos = (data && data.tasks) ? data.tasks.filter(isObj) : [];
      var views = [];
      for (var j = 0; j < infos.length; j++) {
        if (isObj(infos[j].task)) {
          views.push({ task: infos[j].task, info: infos[j], mounted: infos[j].mounted === true, linkId: id });
        }
      }
      if (!views.length && !err) {
        var mounts = mountsForLink(id);
        for (var k = 0; k < mounts.length; k++) {
          if (isObj(mounts[k])) { views.push({ mount: mounts[k], mounted: true, linkId: id }); }
        }
      }
      out.push({ link: link, id: id, views: views, error: err });
    }
    return out;
  }

  /** 这个任务 / 挂载对应的**那一个** NAS 文件夹（一对一）。空 = 没指定（用家目录 /home）。 */
  function taskRoot(v) {
    if (v.task) { return str(v.task.root); }
    var m = v.mount || {};
    return str(m.remote);
  }

  /** 分组标题：连接徽标 + daemon / 登录状态（每组一眼看清属于谁）。 */
  function buildTaskGroupHead(group) {
    var head = el('div', 'task-group-head');
    head.appendChild(el('span', 'task-group-label mono', linkBadge(group.link)));
    var d = group.link && isObj(group.link.daemon) ? group.link.daemon : null;
    var s = (d && isObj(d.status)) ? d.status : null;
    var state0 = (!d || !d.running) ? T('links.daemon_stopped')
      : ((s && s.logged_in) ? T('links.login_ok') : T('links.login_none'));
    head.appendChild(el('span', 'badge ' + ((d && d.running) ? (s && s.logged_in ? 'badge-on' : 'badge-warn') : 'badge-off'), state0));
    if (group.views.length) {
      head.appendChild(el('span', 'task-group-count', T('home.task_count', { n: group.views.length })));
    }
    return head;
  }

  function renderHome() {
    var cst = currentStatus();
    var cs = (cst && isObj(cst.status)) ? cst.status : null;
    var cur = currentLinkEntry();
    var dot = $('home-dot');
    var txt = $('home-conn-text');
    if (dot) { dot.className = 'conn-dot'; }

    if (!linksList().length) {
      if (dot) { dot.className = 'conn-dot err'; }
      if (txt) { txt.textContent = T('home.conn_down'); }
    } else if (!cur || (cur.link_exists === false && !str(cur.host))) {
      if (dot) { dot.className = 'conn-dot warn'; }
      if (txt) { txt.textContent = T('home.conn_no_link'); }
    } else if (!cst || !cst.running) {
      if (dot) { dot.className = 'conn-dot warn'; }
      if (txt) { txt.textContent = T('home.conn_no_daemon', { label: linkLabel(cur) }); }
    } else {
      var logged = !!(cs && cs.logged_in);
      var link = (cs && isObj(cs.link)) ? cs.link : cur;
      if (dot) { dot.className = 'conn-dot ' + (logged ? 'ok' : 'warn'); }
      if (txt) {
        txt.textContent = T('home.conn_ok', {
          user: str(link.user), host: str(link.host), port: str(link.port),
          state: (logged ? T('home.conn_connected') : T('home.conn_disconnected'))
        }) + (cs && cs.server && cs.server.qsync_version
          ? T('home.conn_qxync', { version: str(cs.server.qsync_version) }) : '');
      }
    }

    renderHomeTasks();
    renderHomeSync();
  }

  function renderHomeTasks() {
    var box = $('home-tasks');
    if (!box) { return; }
    clear(box);
    var groups = taskGroups();
    var total = 0;
    for (var gi = 0; gi < groups.length; gi++) { total += groups[gi].views.length; }
    setText('home-task-count', total ? T('home.task_count', { n: total }) : '');
    setListState('home-tasks-empty', total ? 'ready' : 'empty', T('home.tasks_empty'));
    if (!total) { return; }

    var stoppedNote = stoppedLinksNote();
    if (stoppedNote) { box.appendChild(el('p', 'note', stoppedNote)); }

    for (var g = 0; g < groups.length; g++) {
      var group = groups[g];
      if (!group.views.length && !group.error) { continue; }   // 没任务也没挂载的连接不占版面
      box.appendChild(buildTaskGroupHead(group));
      if (group.error) {
        box.appendChild(el('div', 'task-group-empty', T('task.group_error', {
          label: linkLabel(group.link), msg: group.error
        })));
        continue;
      }
      for (var i = 0; i < group.views.length; i++) {
        box.appendChild(buildTaskCard(group.views[i], false));
      }
    }
  }

  /**
   * 一张任务卡。`full=true` 时带完整操作（任务页用），否则只给「管理 + 立即同步」（主页用）。
   * ★ M16：卡片右上带**来源连接**徽标；所有操作按 `v.linkId` 发到对应 daemon。
   */
  function buildTaskCard(v, full) {
    var t = v.task, m = v.mount || {};
    var linkId = str(v.linkId) || currentId();
    var link = linkById(linkId);
    var st = statusOf(linkId);
    var s = (st && isObj(st.status)) ? st.status : null;
    var sy = (s && isObj(s.sync)) ? s.sync : null;
    var ts = taskState(st, sy, v.mounted === true || !!(t || m.mountpoint));
    var card = el('div', 'task-card');
    var mark = ts.mark;
    if (t) {
      if (!t.enabled) { mark = '⏸'; }
      else if (!v.mounted) { mark = '⚠️'; }
    }
    card.appendChild(el('div', 'task-mark', mark));

    var body = el('div', 'task-body');
    var stateText = ts.text;
    // 卡片自己的登记状态会盖掉全局同步状态（暂停 / 未挂载更"紧急"）——
    // 覆盖生效时下面的「放行删除」也要一起让位，否则会出现「状态说已暂停、
    // 却挂着个放行删除按钮」的矛盾画面。
    var blocked = ts.blocked === true;
    if (t) {
      if (!t.enabled) { stateText = T('task.state_paused_task'); blocked = false; }
      else if (!v.mounted) { stateText = T('task.state_not_mounted'); blocked = false; }
    }
    body.appendChild(el('div', 'task-state', stateText));

    // ★ M16：来源连接（徽标 + 人话名）。同一 NAS 多账号时 id 是唯一能区分的。
    var linkRow = el('div', 'task-link-row');
    linkRow.appendChild(el('span', 'badge badge-link', T('task.link_badge', { label: linkBadge(link) })));
    if (!linkDaemonRunning(link)) {
      linkRow.appendChild(document.createTextNode(' '));
      linkRow.appendChild(el('span', 'badge badge-off', T('task.link_daemon_down')));
    }
    body.appendChild(linkRow);

    var root = taskRoot(v);
    var mp = t ? str(t.mountpoint) : str(m.mountpoint);
    body.appendChild(el('div', 'task-meta', T('task.pair', {
      local: mp || T('task.not_set'),
      remote: root || T('task.roots_by_link')
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
    body.appendChild(badgeRow);
    card.appendChild(body);

    var act = el('div', 'task-actions');

    if (full && t) {
      if (!t.enabled) {
        var bResume = el('button', 'mini primary', T('task.act_resume'));
        bResume.type = 'button';
        bResume.addEventListener('click', function () { doTaskAction('resume', t.id, '继续', bResume, linkId); });
        act.appendChild(bResume);
      } else if (v.mounted) {
        var bPause = el('button', 'mini', T('task.act_pause'));
        bPause.type = 'button';
        bPause.addEventListener('click', function () { doTaskAction('pause', t.id, '暂停', bPause, linkId); });
        act.appendChild(bPause);
      } else {
        var bMount = el('button', 'mini primary', T('task.act_mount'));
        bMount.type = 'button';
        bMount.addEventListener('click', function () { doTaskAction('mount', t.id, '挂载', bMount, linkId); });
        act.appendChild(bMount);
      }
    }

    if (full && t) {
      var bEdit = el('button', 'mini', T('task.act_settings'));
      bEdit.type = 'button';
      bEdit.addEventListener('click', function () { editTask(t, linkId); });
      act.appendChild(bEdit);
    }

    var bOpen = el('button', 'mini', T('task.act_manage'));
    bOpen.type = 'button';
    bOpen.addEventListener('click', function () { switchPage('diag', 'mounts'); });
    act.appendChild(bOpen);

    // ★ M10.7：「打开目录」—— 用系统里**用户自己配的默认目录工具**打开本地挂载点
    // （走 `open_path` 命令，见 commands.rs：优先 `xdg-mime`/`gio` 查到的默认程序）。
    // 开的是**本地**那一侧（NAS 侧在网络上，没有可交给本地目录工具的路径）。
    var bDir = el('button', 'mini', T('task.act_open_dir'));
    bDir.type = 'button';
    bDir.setAttribute('title', mp || T('task.not_set'));
    if (!mp) {
      // 没有本地挂载点就没法开：禁用并说清原因，别让按钮点了没反应。
      bDir.disabled = true;
      bDir.setAttribute('aria-disabled', 'true');
    } else {
      bDir.addEventListener('click', function () { openTaskDir(mp, bDir); });
    }
    act.appendChild(bDir);

    var bSync = el('button', 'mini primary', T('task.act_sync_now'));
    bSync.type = 'button';
    bSync.addEventListener('click', function () { doSync({ once: true }, [], '立即同步', linkId); });
    act.appendChild(bSync);

    if (full && t) {
      var bDel = el('button', 'mini danger', T('task.act_delete'));
      bDel.type = 'button';
      bDel.setAttribute('data-act', 'task-delete');
      bDel.addEventListener('click', function () { doTaskDelete(t, linkId, bDel); });
      act.appendChild(bDel);
    }

    // ★ 只有状态行真的在讲「删除被熔断」时才给这个按钮：它是这条提示的**出口**，
    //   放在别的状态下就是个语义错位的危险按钮（而它确实会真删东西）。
    if (blocked) {
      var bForce = el('button', 'mini danger', T('sync.force_deletes'));
      bForce.type = 'button';
      bForce.addEventListener('click', function () { doForceDeletes(bForce, linkId); });
      act.appendChild(bForce);
    }

    card.appendChild(act);
    return card;
  }

  // ============================================================ 更新中心 / 错误列表（M8.3）
  //
  // ★ M16：这两页是**跨连接**的 —— 对每个运行中的 daemon 各发一次 journal 请求，
  //   合并后排序，并在每一行标出来源连接（同一个 ts 的两条不会再互相顶掉）。
  var KIND_LABEL = {
    remote_change: 'NAS 上的改动',
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

  /** 合并各连接的 journal 条目：[{e, linkId}]，按时间倒序。 */
  function mergedJournalEntries() {
    var out = [];
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var id = str(ls[i].id);
      var bucket = state.journalByLink[id];
      if (!bucket || !bucket.data) { continue; }
      var entries = (bucket.data.entries || []).filter(isObj);
      for (var j = 0; j < entries.length; j++) {
        out.push({ e: entries[j], linkId: id });
      }
    }
    out.sort(function (a, b) { return num(b.e.ts, 0) - num(a.e.ts, 0); });
    return out;
  }

  /** 逐连接失败 / daemon 没跑 → 一行说明（不冒充整页失败）。 */
  function fanoutNotes(map) {
    var notes = [];
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var id = str(ls[i].id);
      var bucket = map[id];
      if (bucket && bucket.error) {
        notes.push(T('list.conn_error', { label: linkLabelById(id), msg: bucket.error }));
      }
    }
    var stopped = stoppedLinksNote();
    if (stopped) { notes.push(stopped); }
    return notes;
  }

  function refreshJournal(extra) {
    state.journalLoading = true;
    state.journalError = '';
    renderJournalPage();
    var targets = fanoutTargets();
    return withBusy({ buttons: ['btn-journal-refresh'] }, function () {
      return fanout(journalQuery(extra), targets);
    }).then(function (rs) {
      state.journalLoading = false;
      state.journalByLink = {};
      var errors = 0;
      var firstErr = '';
      for (var i = 0; i < rs.length; i++) {
        var id = str(rs[i].id);
        if (rs[i].r.ok) {
          state.journalByLink[id] = { data: isObj(rs[i].r.data) ? rs[i].r.data : null };
        } else {
          state.journalByLink[id] = { error: str(rs[i].r.error) };
          errors++;
          if (!firstErr) { firstErr = str(rs[i].r.error); }
        }
      }
      // 所有目标都失败才算「整页失败」；部分失败只出提示
      state.journalError = (errors > 0 && errors === rs.length)
        ? (noDaemonRunning() ? T('list.no_daemon_all') : firstErr) : '';
      if (state.journalError) { logErr('journal 查询失败：' + state.journalError); }
      renderJournalPage();
      return { ok: errors === 0, errors: errors };
    });
  }

  function renderJournalPage() {
    var tb = $('journal-tbody');
    if (!tb) { return; }
    clear(tb);
    setBusyState('journal-tbody', state.journalLoading === true);
    if (state.journalError) {
      setText('journal-count', '');
      setListState('journal-empty', 'error', T('list.error', { msg: state.journalError }));
      show('journal-note', false);
      return;
    }
    if (state.journalLoading && !Object.keys(state.journalByLink).length) {
      setText('journal-count', '');
      setListState('journal-empty', 'loading', T('journal.loading'));
      show('journal-note', false);
      return;
    }
    var list = mergedJournalEntries();
    var ls = linksList();
    if (!ls.length) {
      setText('journal-count', '');
      setListState('journal-empty', 'loading', T('journal.loading'));
      return;
    }
    var total = 0, ok = 0, err = 0, blocked = 0;
    var cleared = true;
    var cleanDirs = true;
    for (var bi = 0; bi < ls.length; bi++) {
      var bucket = state.journalByLink[str(ls[bi].id)];
      if (!bucket || !bucket.data) { cleared = false; cleanDirs = false; continue; }
      total += num(bucket.data.total, 0);
      var c = isObj(bucket.data.counts) ? bucket.data.counts : {};
      ok += num(c.ok, 0);
      err += num(c.error, 0);
      blocked += num(c.blocked, 0);
      if (!bucket.data.cleared) { cleared = false; }
      if ((bucket.data.entries || []).length) { cleanDirs = false; }
    }
    setText('journal-count', T('journal.counts', { total: total, ok: ok, error: err, blocked: blocked }));
    if (list.length === 0) {
      setListState('journal-empty', 'empty', (cleared && cleanDirs) ? T('journal.cleared') : T('journal.empty_filtered'));
    } else {
      setListState('journal-empty', 'ready');
    }

    for (var i = 0; i < list.length; i++) {
      (function (item) {
        var e = item.e;
        var tr = el('tr');
        tr.appendChild(el('td', 'col-time mono', humanTimeFromEpoch(e.ts)));
        // ★ M16：来源连接（同一日志表里合并了多个 daemon 的记录）
        var tdLink = el('td', 'col-link');
        tdLink.appendChild(el('span', 'badge badge-link', linkLabelById(item.linkId)));
        tr.appendChild(tdLink);
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

    var notes = fanoutNotes(state.journalByLink);
    var daemonNotes = [];
    for (var k = 0; k < ls.length; k++) {
      var b2 = state.journalByLink[str(ls[k].id)];
      if (b2 && b2.data && b2.data.note) {
        daemonNotes.push(T('list.conn_note', { label: linkLabelById(str(ls[k].id)), note: str(b2.data.note) }));
      }
    }
    var all = notes.concat(daemonNotes);
    var note = $('journal-note');
    if (note) {
      note.hidden = !all.length;
      note.textContent = all.length ? (T('journal.note_prefix') + all.join('；')) : '';
    }
  }

  /** 错误列表 = journal 里 status=error 的视图（复用同一份数据；同样跨连接 fanout）。 */
  function refreshErrors() {
    state.errorsLoading = true;
    state.errorsError = '';
    renderErrorsPage();
    var targets = fanoutTargets();
    return fanout({ method: 'journal', level: 'error', limit: 500 }, targets).then(function (rs) {
      state.errorsLoading = false;
      state.errorsByLink = {};
      var errors = 0;
      var firstErr = '';
      for (var i = 0; i < rs.length; i++) {
        var id = str(rs[i].id);
        if (rs[i].r.ok) {
          state.errorsByLink[id] = { data: isObj(rs[i].r.data) ? rs[i].r.data : null };
        } else {
          state.errorsByLink[id] = { error: str(rs[i].r.error) };
          errors++;
          if (!firstErr) { firstErr = str(rs[i].r.error); }
        }
      }
      state.errorsError = (errors > 0 && errors === rs.length)
        ? (noDaemonRunning() ? T('list.no_daemon_all') : firstErr) : '';
      if (state.errorsError) { logErr('错误列表读取失败：' + state.errorsError); }
      renderErrorsPage();
      return { ok: errors === 0, errors: errors };
    });
  }

  function mergedErrorEntries() {
    var out = [];
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var id = str(ls[i].id);
      var bucket = state.errorsByLink[id];
      if (!bucket || !bucket.data) { continue; }
      var entries = (bucket.data.entries || []).filter(isObj);
      for (var j = 0; j < entries.length; j++) {
        out.push({ e: entries[j], linkId: id });
      }
    }
    out.sort(function (a, b) { return num(b.e.ts, 0) - num(a.e.ts, 0); });
    return out;
  }

  function renderErrorsPage() {
    var box = $('errors-list');
    if (!box) { return; }
    clear(box);
    setBusyState('errors-list', state.errorsLoading === true);
    if (state.errorsError) {
      setText('errors-count', '');
      setListState('errors-empty', 'error', T('list.error', { msg: state.errorsError }));
      return;
    }
    if (state.errorsLoading && !Object.keys(state.errorsByLink).length) {
      setText('errors-count', '');
      setListState('errors-empty', 'loading', T('errors.loading'));
      return;
    }
    var list = mergedErrorEntries();
    setText('errors-count', list.length ? '（' + list.length + '）' : '');
    setListState('errors-empty', list.length ? 'ready' : 'empty', T('errors.empty'));

    for (var i = 0; i < list.length; i++) {
      (function (item) {
        var e = item.e;
        var row = el('div', 'alert alert-err');
        row.appendChild(el('span', 'mono', humanTimeFromEpoch(e.ts) + '  '));
        // ★ M16：来源连接 —— 同一份列表里 A 的失败不能被当成 B 的
        row.appendChild(el('span', 'badge badge-link', linkLabelById(item.linkId)));
        row.appendChild(document.createTextNode(' '));
        row.appendChild(el('span', 'badge badge-err', kindLabel(e.kind)));
        row.appendChild(document.createTextNode('  ' + (str(e.detail) || T('errors.no_detail'))));
        if (str(e.path)) {
          row.appendChild(document.createTextNode('  '));
          row.appendChild(el('span', 'mono', str(e.path)));
          var b = el('button', 'mini', T('errors.copy_path'));
          b.type = 'button';
          b.addEventListener('click', function () { copyText(str(e.path)); });
          row.appendChild(document.createTextNode(' '));
          row.appendChild(b);
        }
        box.appendChild(row);
      })(list[i]);
    }

    var notes = fanoutNotes(state.errorsByLink);
    var hint = $('errors-links-hint');
    if (hint) {
      hint.hidden = !notes.length;
      hint.textContent = notes.join('；');
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
    if (!window.confirm('清空同步日志？\n\n只删日志本身，**不动同步状态**（游标 / baseline / pin / 上传队列都不受影响），也不动 NAS 上的数据。\n\n注意：所有连接的日志都会被清空。')) {
      return Promise.resolve(null);
    }
    var targets = fanoutTargets();
    return withBusy({ buttons: ['btn-journal-clear'] }, function () {
      return fanout({ method: 'journal', clear: true }, targets);
    }).then(function (rs) {
      var ok = 0;
      for (var i = 0; i < rs.length; i++) { if (rs[i].r.ok) { ok++; } }
      logOk('同步日志已清空（' + ok + '/' + rs.length + ' 个连接）');
      return refreshJournal();
    });
  }

  // ============================================================ 任务页（M8.2）
  //
  // ★ M16：任务按连接分布在各 daemon 上，`tasks list` 只回本连接的 → 这里 fanout 合并，
  //   每张卡片标出来源连接，操作再按卡片的 linkId 发回对应 daemon。
  function refreshTasks() {
    state.tasksLoading = true;
    state.tasksError = '';
    renderTasksPage();
    // 运行中的连接走 daemon（有挂载状态）；停跑的连接走 tasks_config（只读磁盘登记），
    // 这样主页/任务页不会因为「daemon 没起」就把任务藏起来。
    var stopped = stoppedLinks().filter(function (l) {
      return l.link_exists !== false && str(l.id);
    });
    var offline = stopped.length
      ? Promise.all(stopped.map(function (l) {
          return call('tasks_config', { linkId: str(l.id) }).then(function (r) {
            return { id: str(l.id), r: r };
          });
        }))
      : Promise.resolve([]);
    return Promise.all([fanout({ method: 'tasks', action: 'list' }, runningTargets()), offline]).then(function (both) {
      var rs = both[0].slice();
      var have = {};
      for (var hi = 0; hi < rs.length; hi++) { have[str(rs[hi].id)] = true; }
      var extra = both[1] || [];
      for (var ei = 0; ei < extra.length; ei++) {
        if (!have[str(extra[ei].id)]) { rs.push(extra[ei]); }
      }
      state.tasksLoading = false;
      state.tasksByLink = {};
      var errors = 0;
      var firstErr = '';
      for (var i = 0; i < rs.length; i++) {
        var id = str(rs[i].id);
        if (rs[i].r.ok) {
          state.tasksByLink[id] = { data: isObj(rs[i].r.data) ? rs[i].r.data : null };
        } else {
          state.tasksByLink[id] = { error: str(rs[i].r.error) };
          errors++;
          if (!firstErr) { firstErr = str(rs[i].r.error); }
          logErr('tasks list 失败（' + linkLabelById(id) + '）：' + str(rs[i].r.error));
        }
      }
      state.tasksError = (errors > 0 && errors === rs.length)
        ? (noDaemonRunning() ? T('list.no_daemon_all') : firstErr) : '';
      // 任务列表回来后顺手刷一下冲突策略汇总与待裁决队列（M8.4）
      window.setTimeout(function () { renderConflictKv(); }, 0);
      refreshDecisions();
      renderTasksPage();
      return { ok: errors === 0, errors: errors };
    });
  }

  function renderTasksPage() {
    var box = $('tasks-list');
    if (!box) { return; }
    clear(box);
    clear($('tasks-bad'));
    setBusyState('tasks-list', state.tasksLoading === true);
    if (state.tasksError) {
      setText('tasks-count', '');
      setListState('tasks-empty', 'error', T('list.error', { msg: state.tasksError }));
      show('tasks-note', false);
      return;
    }
    if (state.tasksLoading && !Object.keys(state.tasksByLink).length) {
      setText('tasks-count', '');
      setListState('tasks-empty', 'loading', T('tasks.loading'));
      show('tasks-note', false);
      return;
    }
    var groups = taskGroups();
    var ls = linksList();
    var total = 0;
    for (var gi = 0; gi < groups.length; gi++) { total += groups[gi].views.length; }
    setText('tasks-count', total ? '（' + total + '）' : '');
    if (total === 0) {
      var anyData = false;
      for (var ai = 0; ai < ls.length; ai++) {
        if (state.tasksByLink[str(ls[ai].id)]) { anyData = true; }
      }
      var never = true;
      for (var di = 0; di < ls.length; di++) {
        var b = state.tasksByLink[str(ls[di].id)];
        if (b && b.data && !b.data.empty) { never = false; }
      }
      setListState('tasks-empty', 'empty', (anyData && !never) ? T('tasks.empty_files') : T('tasks.empty_never'));
    } else {
      setListState('tasks-empty', 'ready');
    }

    var stoppedNote = stoppedLinksNote();
    if (stoppedNote) { box.appendChild(el('p', 'note', stoppedNote)); }

    for (var g = 0; g < groups.length; g++) {
      var group = groups[g];
      if (!group.views.length && !group.error) { continue; }
      box.appendChild(buildTaskGroupHead(group));
      if (group.error) {
        box.appendChild(el('div', 'task-group-empty', T('task.group_error', {
          label: linkLabel(group.link), msg: group.error
        })));
        continue;
      }
      for (var i = 0; i < group.views.length; i++) {
        box.appendChild(buildTaskCard(group.views[i], true));
      }
    }

    // 逐连接的附注 + 解析失败清单（同样标出连接）
    var notes = [];
    var badBox = $('tasks-bad');
    var badCount = 0;
    for (var li = 0; li < ls.length; li++) {
      var b2 = state.tasksByLink[str(ls[li].id)];
      if (b2 && b2.data && b2.data.note) {
        notes.push(T('list.conn_note', { label: linkLabelById(str(ls[li].id)), note: str(b2.data.note) }));
      }
      if (b2 && b2.data) {
        var bad = b2.data.bad_files || [];
        for (var k = 0; k < bad.length; k++) {
          var pair = bad[k];
          badBox.appendChild(el('li', null, T('tasks.bad_file', {
            file: str(pair[0]), err: str(pair[1])
          }) + '（' + linkLabelById(str(ls[li].id)) + '）'));
          badCount++;
        }
      }
    }
    if (badBox) { badBox.hidden = badCount === 0; }
    var note = $('tasks-note');
    if (note) {
      note.hidden = !notes.length;
      note.textContent = notes.length ? (T('tasks.note_prefix') + notes.join('；')) : '';
    }
  }

  /** 按**任务所属连接**执行 tasks 动作（暂停 / 继续 / 挂载 / 删除登记）。 */
  function doTaskAction(action, id, label, btn, linkId) {
    var lid = str(linkId) || currentId();
    return withBusy({ buttons: btn ? [btn] : [] }, function () {
      return ipcFor(lid, { method: 'tasks', action: action, id: id });
    }).then(function (r) {
      if (r.ok) {
        logOk(label + ' ' + id + ' 完成（' + linkLabelById(lid) + '）');
      } else {
        logErr(label + ' ' + id + ' 失败（' + linkLabelById(lid) + '）：' + str(r.error));
      }
      return refreshTasks().then(function () { return refreshStatus(true); }).then(function () { return r; });
    });
  }

  /**
   * ★ 删除任务：先问后端「删它会连带哪些本地数据」（`purge_plan`），再弹自定义弹窗。
   *
   *   * 复选框**不勾** → 旧路径：`tasks delete` 只删任务登记（挂载点里的文件 / NAS 数据不动）；
   *   * 勾了 → `task_purge`：任务登记 + 水合缓存 / 同步状态 / 日志一起删。
   *
   * 取消 / Esc / 点遮罩 → 什么都不发（连 purge_plan 之外没有请求）。
   */
  function doTaskDelete(t, linkId, btn) {
    var lid = str(linkId) || currentId();
    var id = str(t && t.id);
    if (!id) { return Promise.resolve(null); }
    return withBusy({ buttons: btn ? [btn] : [] }, function () {
      return call('purge_plan', { scope: 'task', linkId: lid, taskId: id });
    }).then(function (r) {
      if (!r.ok) {
        logErr('读取删除计划失败（' + linkLabelById(lid) + '）：' + str(r.error));
        return null;
      }
      var plan = isObj(r.data) ? r.data : {};
      return openPurgeDialog({
        title: T('purge.task_title', { id: id }),
        message: T('purge.task_message'),
        items: plan.items,
        dbRows: plan.db_rows,
        totalBytes: plan.total_bytes,
        totalFiles: plan.total_files,
        warnings: plan.warnings,
        optLabel: T('purge.opt_task', { size: humanSize(plan.optional_bytes) }),
        optHint: T('purge.opt_hint'),
        confirmText: T('purge.confirm_delete')
      }).then(function (ans) {
        if (!ans.confirmed) {
          logInfo('已取消删除任务 ' + id);
          return null;
        }
        // 旧行为：只删任务登记（保持与以前完全一致，包括日志文案与刷新）
        if (!ans.purge) {
          return doTaskAction('delete', id, '删除登记', btn, lid);
        }
        return withBusy({ buttons: btn ? [btn] : [] }, function () {
          return call('task_purge', { linkId: lid, taskId: id });
        }).then(function (r2) {
          if (!r2.ok) {
            logErr('删除任务 ' + id + ' 失败（' + linkLabelById(lid) + '）：' + str(r2.error));
            return r2;
          }
          var d = isObj(r2.data) ? r2.data : {};
          logOk(T('purge.done_task', {
            id: id, size: humanSize(d.bytes), files: num(d.files, 0)
          }));
          return refreshTasks().then(function () { return refreshStatus(true); }).then(function () { return r2; });
        });
      });
    });
  }

  /**
   * ★ M10.7：用系统默认目录工具打开一个任务的本地挂载点。
   *
   * 用 `doOpen('path', …)`（→ `open_path` 命令）而不是在 JS 里发起请求：命令里已经处理了
   * 「查 `xdg-mime`/`gio` 默认目录工具 → 起不来回落插件 opener → 把实际走的哪条路回报给前端」。
   *
   * ⚠️ 挂载点**没挂上**时目录可能根本不存在（比如 NAS 掉了、任务被停用后自动卸载）：
   * 这时如实报「路径不存在」，而不是去 `mkdir` 造一个空目录 —— 造出来会让用户以为
   * 文件真的在本地，恰好掩盖掉最该看见的故障。
   */
  function openTaskDir(mountpoint, btn) {
    // 连点两下会开两个目录窗口，这里直接挡掉第二次。
    if (btn && btn.disabled) { return Promise.resolve({ ok: false, error: '已在打开中' }); }
    if (btn) { btn.disabled = true; }
    return doOpen('path', mountpoint).then(function (r) {
      var d = (r && r.ok && isObj(r.data)) ? r.data : null;
      // `via` 如实反映走了哪条路：查到的默认目录程序（xdg-mime / gio）还是回落；
      // `program` 是**真正被执行的程序名**（不是桌面项 ID —— 那个往往不是可执行文件）。
      if (d && d.ok === true && d.via) {
        logOk('已用 ' + str(d.via) + (d.program ? '（' + str(d.program) + '）' : '') +
          ' 打开 ' + mountpoint);
      }
      return r;
    }, function (e) {
      return { ok: false, error: e };
    }).then(function (r) {
      // 无论成败都恢复按钮：目录工具是**脱离**本进程起的，调用本身很快返回，
      // 一直禁着会让用户以为按钮坏了。
      if (btn) { btn.disabled = false; btn.removeAttribute('aria-disabled'); }
      return r;
    });
  }

  // ==================================================== ★ 一对一配对：NAS 文件夹选择
  //
  // 用户反馈（2026-10-02）：NAS 目录原来是个「每行一个」的 textarea，能填多个 →
  // 挂载点被当成虚拟根，顶层多出一层目录名（本地 `/home/user/qsync` 里又出现 `home/`）。
  // Qsync 的「配对文件夹」是**一对一**的，所以这里改成单选：
  //   * 候选来自所选连接的 daemon `roots`（NAS 上登记的 Qsync 同步文件夹 + 家目录）；
  //   * 选不到就「浏览…」逐层挑（复用文件页的 `ls`），或选「手动输入」；
  //   * 保存时只写一个根 —— 多根会被 daemon 的 `tasks save` 直接拒掉。
  //
  // ★ M16：候选**按连接缓存** —— 编辑不同连接的任务时不能拿上一个连接的 NAS 目录列表。

  var NAS_MANUAL = '__manual__';
  var nasCache = {};   // linkId -> { loaded, inflight, candidates }

  function nasFor(linkId) {
    var id = str(linkId) || currentId();
    if (!nasCache[id]) { nasCache[id] = { loaded: false, inflight: null, candidates: [] }; }
    return nasCache[id];
  }

  /** 任务表单当前选中的连接 id。 */
  function taskFormLinkId() {
    return state.taskFormLink || currentId();
  }

  // ---------------------------------------------------- ★ M18 任务默认路径
  // 背景（用户反馈）：新任务表单里「任务 id=default、本地文件夹=~/qxync-mnt、
  // 水合缓存=按连接的缓存根」三处默认值对所有任务都一样 —— 第二条任务一提交就
  // 要么撞 id（tasks/<id>.json 是唯一文件名）、要么撞挂载点，只能手工改三处。
  // 现在按「连接 + NAS 文件夹」派生，天然唯一；规则在 ui/task-defaults.js（纯函数）。

  /** 已经用掉的任务 id。id 是**全局**文件名（`tasks/<id>.json`），跨连接也不能重复。 */
  function takenTaskIds() {
    var out = {};
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var entry = state.tasksByLink[str(ls[i].id)];
      var infos = (entry && entry.data && Array.isArray(entry.data.tasks)) ? entry.data.tasks : [];
      for (var j = 0; j < infos.length; j++) {
        if (isObj(infos[j]) && isObj(infos[j].task)) { out[str(infos[j].task.id)] = true; }
      }
    }
    return out;
  }

  /** 表单里 NAS 一侧当前选中的路径（下拉 / 手动输入两种模式都算）。 */
  function taskFormNasPath() {
    return nasSelectedPath() || '/home';
  }

  /**
   * 按当前「连接 + NAS 文件夹」刷新自动派生的字段（编辑已有任务时什么都不做）。
   *
   * 只覆盖 `state.taskFormAuto` 里仍为 true 的字段：用户改过的 id / 路径不会再被
   * 换 NAS 文件夹或换连接冲掉。
   */
  function applyTaskDefaults() {
    if (!TD || state.taskFormEditing) { return; }
    var linkId = taskFormLinkId();
    var auto = state.taskFormAuto || { id: true, mountpoint: true, cache: true };
    var idEl = $('t-id');
    var mpEl = $('t-mountpoint');
    var cEl = $('t-cache-dir');
    if (auto.id && idEl) {
      idEl.value = TD.uniqueId(TD.idBaseFromRoot(taskFormNasPath()), takenTaskIds());
    }
    // 下面两处跟着「当前 id」走：id 被用户清空时先不动，免得打字中途把路径改花
    var id = str(idEl ? idEl.value : '').trim();
    if (!id) { return; }
    var info = isObj(state.appInfo) ? state.appInfo : {};
    if (auto.mountpoint && mpEl) { mpEl.value = TD.mountpoint(str(state.home), linkId, id); }
    if (auto.cache && cEl) { cEl.value = TD.cacheDir(str(info.data_dir), linkId, id); }
  }

  /** 「重置为默认路径」：把三个字段都交回自动派生（用户手改后想恢复时用）。 */
  function resetTaskDefaults() {
    state.taskFormAuto = { id: true, mountpoint: true, cache: true };
    applyTaskDefaults();
    var idEl = $('t-id');
    if (idEl) { idEl.focus(); }
  }

  /** 用户手动改了 id：之后不再自动覆盖它，但挂载点 / 缓存（若仍自动）继续跟着 id 走。 */
  function onTaskIdEdited() {
    state.taskFormAuto.id = false;
    var idEl = $('t-id');
    var id = str(idEl ? idEl.value : '').trim();
    if (!id) { return; }
    var info = isObj(state.appInfo) ? state.appInfo : {};
    if (state.taskFormAuto.mountpoint) {
      var mpEl = $('t-mountpoint');
      if (mpEl && TD) { mpEl.value = TD.mountpoint(str(state.home), taskFormLinkId(), id); }
    }
    if (state.taskFormAuto.cache) {
      var cEl = $('t-cache-dir');
      if (cEl && TD) { cEl.value = TD.cacheDir(str(info.data_dir), taskFormLinkId(), id); }
    }
  }

  /**
   * 从一份 `roots` 响应里提取候选项，存进**指定连接**的缓存。
   *
   * 诊断页「NAS 同步文件夹」卡片每次刷新都会把同一份响应喂进来（不额外请求），
   * 同时也缓存下来给任务表单用 —— 于是那边刷新之后，表单里的候选也是新的。
   */
  function applyNasCandidates(data, linkId) {
    var id = str(linkId) || currentId();
    var d = isObj(data) ? data : {};
    var out = [];
    var seen = {};
    var push = function (path, label) {
      var p = str(path).trim();
      if (!p || seen[p]) { return; }
      seen[p] = true;
      out.push({ path: p, label: label });
    };
    var sf = (d.syncing_folders || []).filter(isObj);
    for (var i = 0; i < sf.length; i++) {
      // ★ client_path 是映射好的**客户端路径**（/share/homes/<user>/x → /home/x）；
      //   映射不出来就不当候选（宁可不给，也不猜一个挂载不了的 NAS 路径）。
      var cp = str(sf[i].client_path).trim();
      if (!cp) { continue; }
      push(cp, T('task.nas_syncing_folder') + '：' + (str(sf[i].folder) || cp) + '（' + cp + '）');
    }
    var nas = nasFor(id);
    nas.candidates = out;
    nas.loaded = true;
    // 如果任务表单正停在同一个连接上，顺手重画候选（保留用户当前选择 / 手动模式）
    if (state.taskFormLink === id) {
      var sel = $('t-nas-folder');
      renderNasFolderSelect(nasSelectCurrent(), !!sel && sel.value === NAS_MANUAL);
    }
  }

  /** 拿候选：已经有就直接重画（不重复请求 NAS），没有才走一次该连接的 `roots`。 */
  function loadNasCandidates() {
    var id = taskFormLinkId();
    var nas = nasFor(id);
    if (nas.inflight) { return nas.inflight; }
    if (nas.loaded) {
      var sel0 = $('t-nas-folder');
      renderNasFolderSelect(nasSelectCurrent(), !!sel0 && sel0.value === NAS_MANUAL);
      return Promise.resolve(null);
    }
    nas.inflight = ipcFor(id, { method: 'roots' }).then(function (r) {
      nas.inflight = null;
      if (r.ok) { applyNasCandidates(r.data, id); } else { nas.candidates = []; }
      // 候选是异步到的：**保留用户当前选中的值/模式**（可能是正在编辑的任务根）
      var sel = $('t-nas-folder');
      renderNasFolderSelect(nasSelectCurrent(), !!sel && sel.value === NAS_MANUAL);
      return r;
    }, function (e) {
      nas.inflight = null;
      throw e;
    });
    return nas.inflight;
  }

  function nasSelectCurrent() {
    var sel = $('t-nas-folder');
    if (!sel) { return ''; }
    if (sel.value === NAS_MANUAL) {
      return str($('t-nas-manual') ? $('t-nas-manual').value : '').trim();
    }
    return str(sel.value).trim();
  }

  /** 重建下拉：家目录 + 候选项 + 当前值（不在候选里也要能选出来）+ 手动输入。 */
  function renderNasFolderSelect(current, manualMode) {
    var sel = $('t-nas-folder');
    if (!sel) { return; }
    var cur = str(current).trim();
    var list = nasFor(taskFormLinkId()).candidates;
    clear(sel);
    var add = function (value, label) {
      var o = el('option', null, label);
      o.value = value;
      sel.appendChild(o);
    };
    add('/home', T('task.nas_home'));
    var known = cur === '/home';
    for (var i = 0; i < list.length; i++) {
      var c = list[i];
      if (c.path === '/home') { continue; }
      if (c.path === cur) { known = true; }
      add(c.path, c.label);
    }
    if (cur && !known) { add(cur, cur); }
    add(NAS_MANUAL, T('task.nas_manual'));
    if (manualMode) {
      // 候选是异步到的：用户正停在「手动输入」上，就别把他弹回列表
      sel.value = NAS_MANUAL;
    } else {
      sel.value = cur || '/home';
      if (str(sel.value) !== (cur || '/home')) { sel.value = '/home'; }
    }
    syncNasManual();
  }

  function syncNasManual() {
    var sel = $('t-nas-folder');
    var wrap = $('t-nas-manual-wrap');
    if (!sel || !wrap) { return; }
    var manual = sel.value === NAS_MANUAL;
    wrap.hidden = !manual;
    if (manual) {
      var inp = $('t-nas-manual');
      if (inp) { inp.focus(); }
    }
  }

  function nasSelectedPath() {
    var sel = $('t-nas-folder');
    if (!sel) { return ''; }
    if (sel.value === NAS_MANUAL) {
      return str($('t-nas-manual') ? $('t-nas-manual').value : '').trim();
    }
    return str(sel.value).trim();
  }

  /** 表单填值入口（打开新表单 / 编辑已有任务都走这里）。
   *  ★ M18：新任务模式下，换 NAS 文件夹会重算 id / 本地路径（编辑模式不动）。 */
  function setNasFolder(path) {
    var p = str(path).trim();
    var inp = $('t-nas-manual');
    if (inp) { inp.value = p; }
    renderNasFolderSelect(p);
    applyTaskDefaults();
  }

  // ---------------------------------------------------- ★ NAS 文件夹浏览器（逐层挑）
  // ★ M16：浏览动作属于**任务表单所选连接**，不是「当前连接」。
  var nasPicker = { dir: '', inflight: null };

  function openNasPicker() {
    var box = $('nas-picker');
    if (!box) { return; }
    box.hidden = false;
    // 从「当前选中的 NAS 路径」开始浏览；手动输入模式下就是那一格里填的路径
    loadNasPickerDir(nasSelectedPath() || '/home');
  }

  function closeNasPicker() {
    var box = $('nas-picker');
    if (box) { box.hidden = true; }
  }

  function loadNasPickerDir(path) {
    var p = str(path).trim() || '/home';
    var busy = $('nas-picker-busy');
    if (busy) { busy.hidden = false; }
    nasPicker.dir = p;
    setText('nas-picker-path', p);
    clear($('nas-picker-list'));
    setListState('nas-picker-empty', 'loading', T('nas_picker.loading'));
    nasPicker.inflight = ipcFor(taskFormLinkId(), { method: 'ls', path: p }).then(function (r) {
      nasPicker.inflight = null;
      var b = $('nas-picker-busy');
      if (b) { b.hidden = true; }
      if (!r.ok) {
        setListState('nas-picker-empty', 'error', T('list.error', { msg: str(r.error) }));
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      nasPicker.dir = str(d.path || p);
      setText('nas-picker-path', nasPicker.dir);
      var dirs = (d.entries || []).filter(function (e) { return isObj(e) && e.isfolder === true; });
      dirs.sort(function (a, b2) { return str(a.filename).localeCompare(str(b2.filename)); });
      renderNasPickerList(dirs);
      return r;
    });
    return nasPicker.inflight;
  }

  function renderNasPickerList(dirs) {
    var ul = $('nas-picker-list');
    if (!ul) { return; }
    clear(ul);
    if (!dirs.length) {
      setListState('nas-picker-empty', 'empty', T('nas_picker.empty'));
      return;
    }
    setListState('nas-picker-empty', 'ready');
    for (var i = 0; i < dirs.length; i++) {
      (function (name) {
        var full = pathJoin(nasPicker.dir, name);
        var li = el('li', null, '📁 ' + name);
        li.tabIndex = 0;
        li.setAttribute('role', 'button');
        var go = function () { loadNasPickerDir(full); };
        li.addEventListener('click', go);
        li.addEventListener('keydown', function (ev) {
          if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); go(); }
        });
        ul.appendChild(li);
      })(str(dirs[i].filename));
    }
  }

  function pickNasFolder() {
    var p = nasPicker.dir;
    closeNasPicker();
    if (p) { setNasFolder(p); }
  }

  /** ★ M16：任务表单的「NAS 连接」下拉（新建时可改；编辑时只读）。 */
  /** 换连接后候选列表要重画（保留当前 NAS 路径作为「不在候选里」的一项）。 */
  function nasSelectedRefresh() {
    var sel = $('t-nas-folder');
    renderNasFolderSelect(nasSelectCurrent(), !!sel && sel.value === NAS_MANUAL);
  }

  function renderTaskLinkSelect() {
    var sel = $('t-link');
    if (!sel) { return; }
    clear(sel);
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var o = el('option', null, linkBadge(ls[i]));
      o.value = str(ls[i].id);
      sel.appendChild(o);
    }
    // ★ M16.1：新建任务时想连一台还没配的 NAS，就选这一项（它本身不是连接，
    //   选中后跳到「设置 → 连接」新建；编辑态下拉是只读的，不挂这一项）。
    if (state.taskFormEditing !== true) {
      var no = el('option', null, T('task.link_new'));
      no.value = NEW_LINK_OPTION;
      sel.appendChild(no);
    }
    var want = state.taskFormLink || (ls.length ? str(ls[0].id) : '');
    if (want && linkById(want)) { sel.value = want; }
    sel.disabled = state.taskFormEditing === true;
    var hint = $('t-link-hint');
    if (hint) {
      hint.hidden = !state.taskFormEditing;
      hint.textContent = state.taskFormEditing ? T('task.link_locked') : '';
    }
  }

  function resetTaskForm() {
    var set = function (id, v) { var n = $(id); if (n) { n.value = v; } };
    var chk = function (id, v) { var n = $(id); if (n) { n.checked = !!v; } };
    state.taskFormEditing = false;
    // ★ M18：三个默认值全部交回自动派生（按「连接 + NAS 文件夹」生成，见 applyTaskDefaults）
    state.taskFormAuto = { id: true, mountpoint: true, cache: true };
    state.taskFormLink = state.currentLink || (linksList().length ? str(linksList()[0].id) : '');
    set('t-id', '');
    set('t-mountpoint', '');
    set('t-cache-dir', '');
    set('t-cache-mode', 'pagecache');
    set('t-conflict', 'rename_local');
    set('t-direction', '2way');
    chk('t-space-saving', false);
    chk('t-smart-delete', false);
    chk('t-read-write', false);
    chk('t-do-mount', true);
    renderTaskLinkSelect();
    // setNasFolder → applyTaskDefaults：按当前 NAS 文件夹（默认 /home）填唯一默认值
    setNasFolder('/home');
    setText('task-form-title', '文件夹对设置');
    // ★ M18：编辑任务时不做派生，所以「默认值说明 + 重置按钮」只在新建时显示
    show('t-defaults-hint', true);
  }

  function openTaskForm() {
    var card = $('task-form-card');
    if (!card) { return; }
    resetTaskForm();
    card.hidden = false;
    card.scrollIntoView({ block: 'nearest' });
    loadNasCandidates();
    // ★ M18：焦点默认落在 NAS 文件夹（先选它，id/路径都会跟着变），而不是让用户
    //   先看见一个「看起来要手填」的 id 输入框。
    var nasSel = $('t-nas-folder');
    if (nasSel) { nasSel.focus(); }
  }

  /** ★ M8.4：把已有任务填进「文件夹对设置」表单（冲突策略就在这里面改）。
   *  ★ M16：`linkId` = 这条任务登记的连接；编辑时连接只读（换 NAS = 重建任务）。 */
  function editTask(t, linkId) {
    var card = $('task-form-card');
    if (!card || !t) { return; }
    var set = function (id, v) { var n = $(id); if (n) { n.value = v; } };
    var chk = function (id, v) { var n = $(id); if (n) { n.checked = !!v; } };
    state.taskFormEditing = true;
    // ★ M18：编辑已有任务 = 所有字段都是用户/磁盘上的既有值，一律不自动派生。
    state.taskFormAuto = { id: false, mountpoint: false, cache: false };
    state.taskFormLink = str(linkId) || str(t.link_id) || currentId();
    set('t-id', str(t.id));
    set('t-mountpoint', str(t.mountpoint));
    set('t-cache-dir', t.cache_dir ? str(t.cache_dir) : '');
    set('t-cache-mode', str(t.cache_mode) || 'pagecache');
    set('t-conflict', str(t.conflict) || 'rename_local');
    set('t-direction', str(t.direction) || '2way');
    chk('t-space-saving', t.space_saving === true);
    chk('t-smart-delete', t.smart_delete === true);
    chk('t-read-write', t.read_write === true);
    chk('t-do-mount', t.enabled !== false);
    renderTaskLinkSelect();
    // ★ 一对一：NAS 侧就一个文件夹（没写 = 用默认的家目录 /home）
    setNasFolder(str(t.root));
    setText('task-form-title', '文件夹对设置 · ' + str(t.id));
    // ★ M18：编辑时字段都是磁盘上的既有值 —— 隐藏「默认值说明 + 重置按钮」
    show('t-defaults-hint', false);
    card.hidden = false;
    card.scrollIntoView({ block: 'nearest' });
    loadNasCandidates();
  }

  function closeTaskForm() {
    var card = $('task-form-card');
    if (card) { card.hidden = true; }
    state.taskFormEditing = false;
    state.taskFormLink = '';
    setText('task-form-title', '文件夹对设置');
    show('task-result', false);
  }

  function doTaskSave() {
    // ★ M18：id 不再默默回落成 "default" —— 那正是「第二条任务把第一条覆盖掉」的来源。
    //   id 是全局唯一文件名（tasks/<id>.json），空 / 非法字符 / 重名都在提交前说清楚。
    var id = str($('t-id') ? $('t-id').value : '').trim();
    if (!id) {
      renderError('task-result', '参数错误',
        '任务 id 不能为空：它是任务登记的文件名（如 home / public / home-2）');
      return Promise.resolve(null);
    }
    if (TD && !TD.isValidId(id)) {
      renderError('task-result', '参数错误',
        '任务 id 不合法：' + id + '（只允许 A-Za-z0-9 . _ -，最长 64 字符，且不能是 . 或 ..）');
      return Promise.resolve(null);
    }
    if (!state.taskFormEditing && takenTaskIds()[id]) {
      renderError('task-result', '参数错误',
        '任务 id「' + id + '」已经被另一条任务用了（同一份任务登记里 id 是唯一文件名）：'
        + '换个 id，点「重置为默认路径」会自动加 -2 后缀');
      return Promise.resolve(null);
    }
    var mp = str($('t-mountpoint') ? $('t-mountpoint').value : '').trim();
    if (!mp) {
      renderError('task-result', '参数错误', '本地文件夹（挂载点）不能为空');
      return Promise.resolve(null);
    }
    if (mp.charAt(0) !== '/') {
      renderError('task-result', '参数错误',
        '本地文件夹必须是绝对路径（以 / 开头）：' + mp);
      return Promise.resolve(null);
    }
    // ★ 一对一：NAS 侧**一个**目录（下拉 / 浏览 / 手输，三选一）
    var nasPath = nasSelectedPath();
    if (!nasPath) {
      renderError('task-result', '参数错误',
        'NAS 文件夹不能为空：一个配对文件夹 = 一个本地文件夹 + 一个 NAS 文件夹');
      return Promise.resolve(null);
    }
    var linkId = taskFormLinkId();
    if (!linkById(linkId)) {
      renderError('task-result', '参数错误', '没有可用的 NAS 连接：先到「设置 → 连接」添加一台 NAS');
      return Promise.resolve(null);
    }
    var cdir = str($('t-cache-dir') ? $('t-cache-dir').value : '').trim();
    var task = {
      id: id,
      name: id,
      enabled: true,
      mountpoint: mp,
      cache_dir: cdir ? cdir : null,
      root: nasPath,
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
    var rows = [
      ['id', id], ['本地', mp],
      ['NAS 连接', linkBadge(linkById(linkId))],
      ['NAS（一对一）', nasPath],
      ['缓存目录', cdir || '（留空 = 按连接共享，实际在 <data_dir>/cache/<连接命名空间>）'],
      ['冲突策略', conflictLabel(task.conflict)]
    ];
    return withBusy({ spinners: ['task-busy'], buttons: ['btn-task-save'] }, function () {
      return ipcFor(linkId, { method: 'tasks', action: 'save', task: task });
    }).then(function (r) {
      if (!r.ok) {
        // daemon 的「目的地冲突 / 一对一 / 跨连接覆盖」错误原样展示（它已经是人话了）
        renderError('task-result', '保存任务失败（' + linkLabelById(linkId) + '）', str(r.error));
        return r;
      }
      // ★ daemon 的 NAS 侧提示（同一 NAS 目录被别的任务用了）—— 只提示，不拦保存
      var warns = (isObj(r.data) && r.data.warnings) ? r.data.warnings : [];
      for (var wi = 0; wi < warns.length; wi++) {
        if (str(warns[wi])) { rows.push(['⚠️ 提示', str(warns[wi])]); }
      }
      if (!isOn('t-do-mount', true)) {
        renderResult('task-result', true, '任务已保存（未挂载）', rows);
        refreshTasks();
        return r;
      }
      return withBusy({ spinners: ['task-busy'] }, function () {
        return ipcFor(linkId, { method: 'tasks', action: 'resume', id: id });
      }).then(function (r2) {
        var okMount = r2.ok;
        renderResult('task-result', okMount, okMount ? '任务已保存并挂载' : '任务已保存，但挂载失败',
          rows.concat([['挂载', okMount ? '成功' : str(r2.error)]]));
        refreshTasks();
        refreshStatus(true);
        return r2;
      });
    });
  }

  /** 「最近一次同步」按**连接**分行汇总：A 的错误不会被算到 B 头上。 */
  function renderHomeSync() {
    var dl = $('home-sync-kv');
    if (!dl) { return; }
    clear(dl);
    clearAlerts('home-alerts');
    var ls = linksList();
    if (!ls.length) {
      kvText(dl, T('home.sync_title'), T('home.sync_no_link'));
      return;
    }
    var anyRunning = false;
    for (var i = 0; i < ls.length; i++) {
      (function (link) {
        var id = str(link.id);
        var st = statusOf(id);
        var s = (st && isObj(st.status)) ? st.status : null;
        var sy = (s && isObj(s.sync)) ? s.sync : null;
        var label = linkLabel(link);
        if (!st || !st.running) {
          kvText(dl, T('home.sync_link', { label: label }), T('home.sync_no_daemon'), 'val-dim');
          return;
        }
        anyRunning = true;
        // ★ M20：离线横幅必须**先于** sync 判空 —— 离线时 sync 信息也可能缺，
        // 但用户最想知道的是「我的改动还在不在、什么时候能同步」。
        var conn = (s && isObj(s.connectivity)) ? s.connectivity : null;
        if (conn && conn.known === true && conn.online === false) {
          var off = (s && isObj(s.offline)) ? s.offline : null;
          var pend = off
            ? (num(off.pending_upload, 0) + num(off.pending_delete, 0) + num(off.pending_mkdir, 0)
              + num(off.pending_rename, 0) + num(off.pending_set_mtime, 0))
            : 0;
          var durSecs = conn.since_unix ? Math.max(0, Math.floor(Date.now() / 1000) - num(conn.since_unix, 0)) : 0;
          var sinceTxt = durSecs > 0 ? '已离线 ' + humanDuration(durSecs) : '';
          addAlert('home-alerts', T('home.offline_link', {
            label: label,
            since: sinceTxt,
            state: pend > 0
              ? '本地改动已先攒着（' + pend + ' 项），联网后自动同步'
              : '已下载的内容仍可正常读写'
          }), 'warn');
          if (off) {
            addAlert('home-alerts', T('home.offline_pending_link', {
              label: label,
              up: num(off.pending_upload, 0),
              del: num(off.pending_delete, 0),
              mkdir: num(off.pending_mkdir, 0),
              rename: num(off.pending_rename, 0),
              setmtime: num(off.pending_set_mtime, 0)
            }), 'warn');
          }
        }
        if (!sy) {
          kvText(dl, T('home.sync_link', { label: label }), T('home.sync_missing'), 'val-dim');
          return;
        }
        kvText(dl, T('home.sync_link', { label: label }), T('home.sync_brief', {
          polls: num(sy.polls, 0),
          uploaded: num(sy.uploaded, 0),
          deleted: num(sy.deleted, 0),
          conflicts: num(sy.conflicts, 0)
        }), (sy.last_error || sy.delete_block_reason) ? 'val-warn' : null);
        if (sy.last_error) {
          addAlert('home-alerts', T('home.sync_err_link', {
            label: label, msg: String(sy.last_error)
          }));
        }
        if (sy.delete_block_reason) {
          addAlert('home-alerts', T('home.sync_blocked_link', {
            label: label, reason: String(sy.delete_block_reason)
          }), null, {
            label: T('sync.force_deletes'),
            onClick: function (b) { doForceDeletes(b, id); }
          });
        }
        if (sy.note) {
          addAlert('home-alerts', T('home.sync_note_link', { label: label, note: String(sy.note) }), 'warn');
        }
      })(ls[i]);
    }
    if (!anyRunning) { addAlert('home-alerts', T('home.sync_all_down'), 'warn'); }
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
      kvText(srv, '状态', '连接 ' + linkLabelById(currentId()) +
        ' 的 qxyncd 未运行：请点顶部「启动 daemon」，或到「设置 → 连接」保存配置后启动。');
      ['sess-kv', 'cursors-kv', 'hydro-kv', 'transfers-kv', 'uploads-kv', 'cache-kv', 'cache-blocked-kv', 'sync-summary-kv', 'sync-note']
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
      // ★ M22：离线时 daemon 不发 check_alive —— alive=false 且 alive_known=false
      // 是「不知道」，显示「未知」而不是「失活」（后者会误导用户去重登，其实只是网断了）。
      if (!s.session.alive && s.session.alive_known === false) {
        kvText(sess, 'session alive', '未知（离线未探测）', 'val-dim');
      } else {
        kvBool(sess, 'session alive', s.session.alive === true, '存活', '失活');
      }
    } else {
      kvText(sess, 'session', '—');
    }
    if (isObj(s.link)) {
      kvText(sess, 'link', str(s.link.id) + ' · ' + str(s.link.user) + '@' + str(s.link.host) + ':' + str(s.link.port));
    }
    // ★ M20：连通性 + 离线缓冲区（在线且无积压时 daemon 不给 `offline`）
    var conn = isObj(s.connectivity) ? s.connectivity : null;
    if (conn) {
      if (conn.known !== true) {
        kvText(sess, '连通性', '未检测（还没发过网络请求）', 'val-dim');
      } else if (conn.online !== false) {
        kvText(sess, '连通性', '在线', 'val-ok');
      } else {
        var offSecs = conn.since_unix ? Math.max(0, Math.floor(Date.now() / 1000) - num(conn.since_unix, 0)) : 0;
        kvText(sess, '连通性',
          '离线' + (offSecs > 0 ? '（已 ' + humanDuration(offSecs) + '）' : '')
          + (conn.last_error ? '：' + String(conn.last_error) : ''), 'val-warn');
      }
    }
    var offbuf = isObj(s.offline) ? s.offline : null;
    if (offbuf) {
      kvText(sess, '离线缓冲区',
        '待上传 ' + num(offbuf.pending_upload, 0)
        + ' · 待删除 ' + num(offbuf.pending_delete, 0)
        + ' · 待建目录 ' + num(offbuf.pending_mkdir, 0)
        + ' · 待改名 ' + num(offbuf.pending_rename, 0)
        + ' · 待改 mtime ' + num(offbuf.pending_set_mtime, 0),
        offbuf.replaying ? 'val-warn' : null);
      if (offbuf.replaying) {
        kvText(sess, '结构操作', '待回放 / 正在回放（联网后按顺序推回 NAS；期间上传与删除冻结）', 'val-warn');
      }
      if (offbuf.last_note) {
        kvText(sess, '离线原因', String(offbuf.last_note), 'val-warn');
      }
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

    // --- ★ T8：传输中（下载 + 上传）。`undefined/null` = 此刻没有在途作业。
    //   与上面的累计水合计数是**两个口径**：那个是历史总量，这个是此刻在传的。
    var tr = $('transfers-kv');
    clear(tr);
    if (isObj(s.transfers)) {
      var t = s.transfers;
      kvText(tr, '在传文件', num(t.active, 0) + '（下载 ' + num(t.downloading, 0)
        + ' · 上传 ' + num(t.uploading, 0) + '）');
      kvText(tr, '在传字节', humanSize(num(t.done_bytes, 0)) + ' / ' + humanSize(num(t.total_bytes, 0)));
    } else {
      kvText(tr, '传输中', '无');
    }

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
      // ★ `deletes_blocked` 是**累计**计数（真机到过 692742），不是「当前被挡了多少」。
      //   「当前有没有被挡」只看 `delete_block_reason`（有值 = 这一轮有删除被熔断）；
      //   累计值配红字会让人以为现在还在熔断，所以这里不上色。
      kvText(sum, '删除被挡（累计）', num(sy.deletes_blocked, 0));
      kvText(sum, 'events', num(sy.events, 0));
      kvText(sum, 'baseline_entries', num(sy.baseline_entries, 0));
      if (sy.last_error) {
        addAlert('sync-alerts', '同步错误：' + String(sy.last_error));
      }
      if (sy.delete_block_reason) {
        addAlert('sync-alerts', T('sync.blocked_alert', { reason: String(sy.delete_block_reason) }),
          null, { label: T('sync.force_deletes'), onClick: doForceDeletes });
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
  //
  // ★ action（可选 {label, onClick}）：告警行右侧挂一个按钮。
  //   用户反馈（2026-10-04）：「有删除被熔断挡住（待确认）」这条提示**没地方处理**——
  //   它只说「到诊断→同步/缓存可放行」，而那里的按钮叫「强制放行删除」，两边对不上，
  //   看到的人根本不知道该点哪个。**凡是「看到了还得做点什么」的状态，就把按钮挂在
  //   这条提示自己身上**，别让用户自己去找。
  function addAlert(containerId, text, severity, action) {
    var box = $(containerId);
    if (!box) { return; }
    var cls = (severity === 'warn') ? 'alert alert-warn' : 'alert';
    var row = el('div', cls);
    row.appendChild(el('span', 'alert-text', text));
    if (isObj(action) && action.label && typeof action.onClick === 'function') {
      row.className += ' alert-act';
      var b = el('button', 'mini danger', action.label);
      b.type = 'button';
      b.addEventListener('click', function () { action.onClick(b); });
      row.appendChild(b);
    }
    box.appendChild(row);
  }

  /**
   * 「放行删除」的唯一实现 —— 告警行内按钮、任务卡按钮、诊断页按钮都走这里。
   *
   * 删除不可逆，所以：① 把 daemon 给的**原始原因**摆出来让人自己判断
   * （「一次对账要删除 812 项，超过上限 50」比「确认放行？」有信息量得多）；
   * ② 一定要二次确认；③ 按钮在飞行中禁用，防连点。
   * 只放行**这一轮**（daemon 侧 `force_deletes` 用完即置回 false）。
   */
  function doForceDeletes(btn, linkId) {
    var lid = str(linkId) || currentId();
    if (!requireDaemon(lid)) { return Promise.resolve(null); }
    var st = statusOf(lid);
    var sy = (st && isObj(st.status) && isObj(st.status.sync)) ? st.status.sync : null;
    var reason = (sy && sy.delete_block_reason) ? String(sy.delete_block_reason) : '';
    var n = num(sy && sy.deletes_blocked, 0);
    if (!window.confirm(T('sync.force_deletes_confirm', {
      reason: reason || T('sync.force_deletes_noreason'),
      n: n
    }))) {
      return Promise.resolve(null);
    }
    if (btn) { btn.disabled = true; }
    return doSync({ once: true, force_deletes: true }, [], T('sync.force_deletes'), lid).then(function (r) {
      if (btn) { btn.disabled = false; }
      return r;
    }, function (e) {
      if (btn) { btn.disabled = false; }
      throw e;
    });
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
        var tdRemote = el('td', 'mono', str(m.remote));
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
    withBusy({ buttons: btn ? [btn] : [] }, function () {
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

  // ====================================================== NAS 目录面板
  //
  // 一对一是删除过的：这里不再有「配置的多个根」要列，只剩
  // 「家目录根 + NAS 上登记的 Qsync 同步文件夹」——也就是配对时能选的 NAS 文件夹。
  // 这个请求**不重**（不再逐根列举目录），可以跟着刷新走。
  var rootsInflight = null;

  function renderRoots(d) {
    var data = isObj(d) ? d : {};
    var folders = $('roots-folders');
    if (!folders) { return; }
    // ★ 同一份响应顺手喂给「配对文件夹」的 NAS 候选（不额外请求 NAS）：
    //   在诊断页点过刷新之后，任务表单里的下拉也是新的。
    // ★ M16：候选按连接缓存（诊断页只展示当前连接，所以直接用 currentLink）
    applyNasCandidates(data, state.currentLink);
    clear(folders);
    clear($('roots-summary-kv'));

    var sf = (data.syncing_folders || []).filter(isObj);
    state.rootsLoaded = true;
    setText('roots-count', sf.length ? '（' + sf.length + '）' : '');
    setBusyState('roots-folders', false);

    var kv = $('roots-summary-kv');
    kvText(kv, 'NAS 登记的同步文件夹', sf.length ? (sf.length + ' 个（配对时可下拉选）') : '（无）');

    var note = $('roots-note');
    if (note) {
      note.hidden = !data.note;
      note.textContent = data.note ? ('说明：' + String(data.note)) : '';
    }

    if (sf.length) { setListState('roots-folders-empty', 'ready'); }
    else { setListState('roots-folders-empty', 'empty', T('roots.folders_empty')); }
    for (var j = 0; j < sf.length; j++) {
      var f = sf[j];
      // ★ 真机字段是 name/path/privilege；`client_path` 是映射好的客户端路径（/home/…）。
      //   没有 client_path 说明映射不出来（别把它当成能挂的目录）。
      var usable = str(f.client_path).trim();
      var raw = str(f.path).trim() || str(f.realpath).trim();
      var txt = str(f.folder) + ' · ' +
        (usable ? usable : '（不可映射：' + (raw || '?') + '）') +
        ' · 权限=' + str(f.permission) +
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
      setListState('roots-folders-empty', 'loading', T('roots.loading'));
      setBusyState('roots-folders', true);
    }
    rootsInflight = ipc({ method: 'roots' }).then(function (r) {
      rootsInflight = null;
      if (btn) { btn.disabled = false; }
      if (r.ok) {
        renderRoots(r.data);
      } else {
        // 失败也要给出可见反馈（错误详情在底部操作日志）
        clear($('roots-folders'));
        clear($('roots-summary-kv'));
        setText('roots-count', '');
        setListState('roots-folders-empty', 'error', T('list.error', { msg: str(r.error) }));
        setBusyState('roots-folders', false);
        var nt = $('roots-note');
        if (nt) { nt.hidden = true; nt.textContent = ''; }
      }
      return r;
    }, function (e) {
      rootsInflight = null;
      if (btn) { btn.disabled = false; }
      setBusyState('roots-folders', false);
      throw e;
    });
    return rootsInflight;
  }

  function refreshRootsIfVisible() {
    if (state.page === 'diag' && state.diag === 'status') { refreshRoots(); }
  }

  // ============================================================ Tab 2 连接（★ M16 多连接）
  //
  // 三个数据源分工：
  //   * `links_status` —— 连接列表 + 每个 daemon 的运行/登录/凭据状态（每 2s 轮询）；
  //   * `link_read`    —— 单个连接的**完整**配置（含 GUI 不编辑的 M7 字段，只读）；
  //   * `credential_present` —— 只回「有没有凭据」，绝不回口令。
  // 表单沿用旧的 f-* / btn-* id（截图矩阵与既有操作习惯不变），新增一个隐藏的编辑 id 状态。

  function setFormValue(id, v) { var n = $(id); if (n) { n.value = v; } }
  function setFormChecked(id, v) { var n = $(id); if (n) { n.checked = !!v; } }

  /** 凭据提示：有没有凭据 + ★ M17 两步验证（2SV）是否已配置。
   *  只按 `credential_present` 回的布尔渲染 —— 密钥本身永远不会到前端。 */
  function credHintText(present) {
    var txt = (present && present.present)
      ? T('connect.cred_present', {
        user: str(present.user), host: str(present.host), path: str(present.path)
      })
      : T('connect.cred_missing', { path: str(present ? present.path : '') });
    txt += ' · ' + (present && present.otp === true
      ? T('connect.otp_configured')
      : T('connect.otp_missing'));
    return txt;
  }

  /** 把 link_read 的结果填进「连接配置」表单（新建时 info.link 为空 = 全清空）。 */
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
    var editing = str(state.editingLinkId);
    setFormValue('f-link-id', editing);
    setFormValue('f-host', link ? str(link.host) : '');
    setFormValue('f-port', link ? str(link.port) : '');
    setFormValue('f-user', link ? str(link.user) : '');
    setFormChecked('f-https', link ? link.https !== false : true);
    setFormChecked('f-insecure', link ? link.insecure === true : false);
    setFormChecked('f-ipv4-only', link ? link.ipv4_only === true : false);
    var pw = $('f-password');
    if (pw) { pw.value = ''; }
    // ★ M17：两步验证密钥**绝不回显**（和口令一样：只写不读）。但只在「表单换到了
    //   另一条连接 / 进入新建态」时清空 —— 不能每次填表都清：切页/保存后/当前连接被
    //   外部改掉时都会重新 fillConnectForm，无脑清空会在用户正输入密钥时把它抹掉。
    var formTarget = (link
      ? str(link.id) + '|' + str(link.host) + '|' + str(link.user)
      : 'new') + '|' + editing;
    if (formTarget !== state.otpFormTarget) {
      state.otpFormTarget = formTarget;
      var otpEl = $('f-otp-secret');
      if (otpEl) { otpEl.value = ''; }
    }

    // 表单标题 / 编辑提示 / 编辑态按钮
    setText('conn-form-title', editing
      ? T('conn.form_title_edit', { id: editing })
      : T('conn.form_title_new'));
    var eHint = $('conn-edit-hint');
    if (eHint) {
      eHint.hidden = !editing;
      eHint.textContent = editing
        ? T('conn.edit_hint', { id: editing })
        : '';
    }
    show('conn-edit-actions', !!editing);

    var cred = $('cred-hint');
    if (cred) { cred.textContent = credHintText(present); }
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
    kvText(dl, '连接数', ((info.links || []).length) + ' 个'
      + (((info.links_bad || []).length) ? '（解析失败 ' + (info.links_bad || []).length + ' 个）' : ''));
    kvText(dl, 'daemon_bin', info.daemon_bin ? str(info.daemon_bin) : '未找到（先 cargo build --workspace）',
      info.daemon_bin ? null : 'val-warn');
    kvBool(dl, 'daemon_running', info.daemon_running === true, '运行中', '未运行');

    // 挂载点默认值 & 下载默认目录
    var mp = $('m-mountpoint');
    if (mp && !mp.value) {
      mp.value = (str(info.home) || '~') + '/qxync-mnt';
    }
  }

  // ------------------------------------------------------------ ★ M16：连接列表
  /** 连接列表的「渲染签名」：只有当连接集合 / 运行 / 登录 / 凭据 / 当前连接变化时才重画。
   *  轮询每 2s 一次，无脑重建 <select> 和行内按钮会打断正在用键盘操作的用户。 */
  function linkSig() {
    var parts = [str(state.currentLink)];
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var l = ls[i];
      var d = isObj(l.daemon) ? l.daemon : {};
      var st = isObj(d.status) ? d.status : null;
      var tb = state.tasksByLink[str(l.id)];
      var taskCount = (tb && tb.data && Array.isArray(tb.data.tasks)) ? tb.data.tasks.length : -1;
      parts.push([
        str(l.id),
        str(l.user), str(l.host), str(l.port),
        l.link_exists === false ? 0 : 1,
        l.credential_present ? 1 : 0,
        // ★ M17：2SV 是否已配置会影响连接行的徽标，必须进签名，否则轮询不会重画。
        l.otp === true ? 1 : 0,
        d.running ? 1 : 0,
        st && st.logged_in ? 1 : 0,
        taskCount,
        str(d.error || ''),
        str(d.socket || '')
      ].join(':'));
    }
    return parts.join('|');
  }

  function renderConnSelect() {
    var sel = $('conn-select');
    if (!sel) { return; }
    var cur = state.currentLink;
    clear(sel);
    var ls = linksList();
    if (!ls.length) {
      var o0 = el('option', null, T('links.unconfigured'));
      o0.value = '';
      sel.appendChild(o0);
      sel.disabled = true;
      return;
    }
    sel.disabled = false;
    for (var i = 0; i < ls.length; i++) {
      var l = ls[i];
      var label = str(l.id) + ' · ' + linkLabel(l);
      if (!linkDaemonRunning(l)) { label += T('links.daemon_down_suffix'); }
      var o = el('option', null, label);
      o.value = str(l.id);
      sel.appendChild(o);
    }
    sel.value = cur;
    if (sel.value !== cur) { sel.value = str(ls[0].id); }
  }

  /** 连接列表的一行：id · user@host:port + daemon/登录/凭据徽标 + 行内操作。 */
  function buildConnRow(link) {
    var id = str(link.id);
    var d = (link && isObj(link.daemon)) ? link.daemon : null;
    var st = (d && isObj(d.status)) ? d.status : null;
    var running = !!(d && d.running);
    var row = el('li', 'conn-row' + (id === state.currentLink ? ' current' : ''));
    var main = el('div', 'conn-main');

    var title = el('div', 'conn-title');
    title.appendChild(el('span', 'conn-id mono', id));
    title.appendChild(document.createTextNode(' '));
    title.appendChild(el('span', 'conn-label mono', linkLabel(link)));
    if (id === state.currentLink) {
      title.appendChild(document.createTextNode(' '));
      title.appendChild(el('span', 'badge badge-on', T('links.current')));
    }
    main.appendChild(title);

    var badges = el('div', 'conn-badges');
    badges.appendChild(el('span', 'badge ' + (running ? 'badge-on' : 'badge-off'),
      running ? T('links.daemon_running') : T('links.daemon_stopped')));
    if (!running) {
      badges.appendChild(el('span', 'badge', T('links.login_unknown')));
    } else if (st && st.logged_in) {
      badges.appendChild(el('span', 'badge badge-on', T('links.login_ok')));
    } else {
      badges.appendChild(el('span', 'badge badge-warn', T('links.login_none')));
    }
    badges.appendChild(el('span', 'badge ' + (link.credential_present ? 'badge-on' : 'badge-off'),
      link.credential_present ? T('links.cred_present') : T('links.cred_missing')));
    // ★ M17：只标「配没配」，徽标里绝不出现密钥。
    if (link.otp === true) {
      badges.appendChild(el('span', 'badge badge-on', T('links.otp_badge')));
    }
    if (link.link_exists === false) {
      badges.appendChild(el('span', 'badge badge-warn', T('links.link_missing')));
    }
    // 任务数（可选）：列表已 fanout 过 tasks 才显示，否则不冒充 0
    var tb = state.tasksByLink[id];
    if (tb && tb.data && Array.isArray(tb.data.tasks)) {
      badges.appendChild(el('span', 'badge', T('links.task_count', { n: tb.data.tasks.length })));
    }
    main.appendChild(badges);

    var meta = [];
    if (d && d.socket) { meta.push(str(d.socket)); }
    if (st && isObj(st.session) && st.session.sid_masked) { meta.push(str(st.session.sid_masked)); }
    if (d && d.error) { meta.push(T('links.daemon_error', { msg: str(d.error) })); }
    main.appendChild(el('div', 'conn-meta mono', meta.join(' · ')));
    row.appendChild(main);

    var act = el('div', 'conn-actions');
    // 设为当前
    var bCur = el('button', 'mini' + (id === state.currentLink ? '' : ' primary'),
      id === state.currentLink ? T('links.current') : T('links.act_current'));
    bCur.type = 'button';
    bCur.disabled = id === state.currentLink;
    bCur.addEventListener('click', function () { setCurrentLink(id); });
    act.appendChild(bCur);
    // 启动 / 停止
    var bRun = el('button', 'mini', running ? T('links.act_stop') : T('links.act_start'));
    bRun.type = 'button';
    bRun.addEventListener('click', function () {
      if (running) { doDaemonStop(id, bRun); } else { doDaemonStart(id, bRun); }
    });
    act.appendChild(bRun);
    // 编辑
    var bEdit = el('button', 'mini', T('links.act_edit'));
    bEdit.type = 'button';
    bEdit.addEventListener('click', function () { editLink(id); });
    act.appendChild(bEdit);
    // 移除（没有 link 文件的合成项不可移除：删不了不存在的东西）
    var bDel = el('button', 'mini danger', T('links.act_remove'));
    bDel.type = 'button';
    bDel.setAttribute('data-act', 'link-remove');
    if (link.link_exists === false) {
      bDel.disabled = true;
      bDel.setAttribute('aria-disabled', 'true');
      bDel.title = T('links.remove_unavailable');
    } else {
      bDel.addEventListener('click', function () { doLinkDelete(id, bDel); });
    }
    act.appendChild(bDel);
    row.appendChild(act);
    return row;
  }

  function renderLinksBad() {
    var ul = $('links-bad');
    if (!ul) { return; }
    clear(ul);
    var bad = state.linksBad || [];
    ul.hidden = bad.length === 0;
    for (var i = 0; i < bad.length; i++) {
      ul.appendChild(el('li', null, T('links.bad_file', {
        path: str(bad[i].path), err: str(bad[i].error)
      })));
    }
  }

  function renderConnList() {
    var ul = $('conn-list');
    if (!ul) { return; }
    clear(ul);
    setBusyState('conn-list', state.linksLoading === true);
    var ls = linksList();
    // ★ purge_all 之后一条真实连接都不剩：后端/前端都会合成一个「未配置」占位项
    //   （让「启动 / 添加」仍有落点），这里据此显示空态文案，而不是假装列表里还有东西。
    var realCount = 0;
    for (var ri = 0; ri < ls.length; ri++) {
      if (ls[ri] && ls[ri].link_exists !== false) { realCount++; }
    }
    setText('links-count', realCount ? T('links.count', { n: realCount }) : '');
    if (state.linksError) {
      setListState('conn-list-empty', 'error', T('list.error', { msg: state.linksError }));
    } else if (state.linksLoading && !ls.length) {
      setListState('conn-list-empty', 'loading', T('links.loading'));
    } else if (!realCount) {
      setListState('conn-list-empty', 'empty', T('links.empty'));
    } else {
      setListState('conn-list-empty', 'ready');
    }
    for (var i = 0; i < ls.length; i++) {
      ul.appendChild(buildConnRow(ls[i]));
    }
    renderLinksBad();
  }

  function refreshLinks() {
    state.linksLoading = true;
    renderConnList();
    return refreshStatus(true).then(function () {
      state.linksLoading = false;
      renderConnList();
      return { ok: true };
    }, function (e) {
      state.linksLoading = false;
      state.linksError = str(e);
      renderConnList();
      return { ok: false, error: str(e) };
    });
  }

  /** 读当前连接的配置（表单用）+ app_info。正在编辑别的连接时不覆盖表单。 */
  function loadConnect() {
    var id = currentId();
    return Promise.all([
      call('link_read', { linkId: id }),
      call('credential_present', { linkId: id }),
      call('app_info', {})
    ]).then(function (rs) {
      var link = rs[0].ok ? rs[0].data : null;
      var cred = rs[1].ok ? rs[1].data : null;
      var info = rs[2].ok ? rs[2].data : null;
      renderAppInfo(info);
      if (str(state.editingLinkId) === '') {
        // ★ M16.1：点了「新建连接」时，这次异步回包（读的是**当前连接**）不能把
        //   已经清空的表单又填回去 —— 否则「新建」表单里会带着上一台 NAS 的地址。
        if (state.creatingLink === true) {
          fillConnectForm({ exists: false, path: T('conn.path_pending'), link: null }, cred);
        } else {
          fillConnectForm(link, cred);
        }
      } else if (str(state.editingLinkId) === id) {
        fillConnectForm(link, cred);   // 编辑的就是当前连接：读回来的就是它
      } else {
        // 正在编辑另一条连接：只更新凭据提示，别把表单冲掉
        var c = $('cred-hint');
        if (c) { c.textContent = credHintText(cred); }
      }
      renderConnList();
      renderConnSelect();
      return rs;
    });
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
      ipv4_only: isOn('f-ipv4-only', false)
    };
    // ★ 编辑态才带 id：新建交给后端自动生成（无连接 → default，否则 nas2/nas3…）
    if (state.editingLinkId) { input.id = str(state.editingLinkId); }
    if (withPassword) {
      input.password = str($('f-password') ? $('f-password').value : '');
    }
    // ★ M17：两步验证（2SV）字段。「保存连接」与「保存并登录」两条路径都带上；
    //   值为空 = 后端保留该连接已保存的密钥（编辑时不填 = 不改）。
    //   注意：它属于凭据文件，link_save 会忽略它，只有 credential_save / login_flow 用。
    var otpEl = $('f-otp-secret');
    var otpVal = otpEl ? str(otpEl.value).trim() : '';
    if (otpVal) { input.otp_secret = otpVal; }
    return { input: input };
  }

  function editLink(id) {
    var lid = str(id);
    return call('link_read', { linkId: lid }).then(function (r) {
      if (!r.ok || !isObj(r.data)) {
        renderError('login-result', '读取连接配置失败', str(r.error));
        return r;
      }
      return call('credential_present', { linkId: lid }).then(function (c) {
        state.editingLinkId = lid;
        state.creatingLink = false;   // ★ M16.1：编辑已有连接 = 退出「新建」态
        fillConnectForm(r.data, c.ok ? c.data : null);
        // ★ M17：重新进入编辑态 = 重新开始，清掉可能残留的密钥/验证码输入。
        clearOtpField();
        var pw = $('f-password');
        if (pw) { pw.focus(); }
        return r;
      });
    });
  }

  /** 「＋ 添加 NAS」：清空表单回到新建态（不动已保存的连接）。 */
  function newLink() {
    state.editingLinkId = '';
    state.creatingLink = true;
    fillConnectForm({ exists: false, path: T('conn.path_pending'), link: null }, null);
    // ★ M17：新建 = 空白表单，清掉残留的两步验证输入。
    clearOtpField();
    // ★ M16.1：表单在「连接配置」卡片里，点完按钮如果不把卡片滚进视野、也不给一句日志，
    //   看上去就是「点了没反应」（不少人就是这么以为界面上没有新建入口的）。
    var card = $('link-form-card');
    if (card && typeof card.scrollIntoView === 'function') {
      card.scrollIntoView({ block: 'start', behavior: 'smooth' });
    }
    var host = $('f-host');
    if (host) { host.focus(); }
    logInfo('已打开「添加 NAS 连接」表单：填完点「保存并登录」');
    return Promise.resolve(null);
  }

  /** ★ M16.1：统一的「新建连接」入口（主页 / 顶栏 / 任务表单都走这里）：
   *  切到「设置 → 连接」再把新建表单滚进来。 */
  function openNewLink(hint) {
    switchPage('settings', null, 'connect');
    newLink();
    if (hint) { logInfo(hint); }
    return Promise.resolve(null);
  }

  function cancelLinkEdit() {
    state.editingLinkId = '';
    state.creatingLink = false;
    show('login-result', false);
    return loadConnect();
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

  /** ★ M17：把「两步验证」结果行追加到结果表（只在后端明确给了信息时）。
   *  `otp_saved` = 本次保存了密钥；`otp_note` = 例如「一次性验证码不会保存」。
   *  **绝不显示密钥/验证码本身**（后端也不会回传）。 */
  function appendOtpRows(rows, d) {
    if (!d) { return; }
    if (d.otp_saved === true) {
      rows.push(['两步验证', '密钥已保存（之后自动生成验证码）']);
    }
    if (d.otp_note) {
      rows.push(['两步验证', str(d.otp_note)]);
    }
  }

  /** ★ M17：清空两步验证输入框 —— 密钥只写不读，保存/登录完不在屏幕上留。 */
  function clearOtpField() {
    var n = $('f-otp-secret');
    if (n) { n.value = ''; }
  }

  /** 保存连接配置（不登录）。新连接保存后自动切为当前连接。 */
  function doLinkSave() {
    var built = buildLinkInput(false);
    if (built.error) {
      renderError('login-result', '参数错误', built.error);
      return Promise.resolve({ ok: false, error: built.error });
    }
    return call('link_save', { input: built.input }).then(function (r) {
      if (!r.ok) {
        renderError('login-result', '保存连接配置失败', str(r.error));
        logErr('link_save 失败：' + str(r.error));
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      var savedId = str(d.link && d.link.id);
      state.editingLinkId = savedId;
      state.creatingLink = false;   // ★ M16.1：保存成功 → 表单切回「编辑这条连接」
      var rows = [
        ['连接 id', savedId],
        ['文件', str(d.path)],
        ['NAS', str(d.link && d.link.user) + '@' + str(d.link && d.link.host) + ':' + str(d.link && d.link.port)],
        ['https', d.link && d.link.https ? '是' : '否']
      ];
      // ★ M17：`link_save` 只写 link 文件，不写凭据（凭据是「保存并登录」的职责）。
      //   所以用户在这张表单里填了密钥/验证码时**不能静默丢掉输入**：明确告知
      //   「未保存」，并保留输入框内容，让用户能接着点「保存并登录」。
      var pendingOtp = !!str(built.input.otp_secret);
      if (pendingOtp) {
        rows.push(['两步验证', '未保存 —— 点「保存并登录」才会写入凭据']);
        // ★ M17(a)：保存成功后 `loadConnect()` 会重填表单（新建连接尤其会从「新建态」
        //   切到「编辑态」）。预先把「保存后的表单目标」记进 state.otpFormTarget，
        //   免得 fillConnectForm 把刚保留下来的两步验证输入清掉。
        var savedLink = isObj(d.link) ? d.link : null;
        state.otpFormTarget = (savedLink
          ? str(savedLink.id || savedId) + '|' + str(savedLink.host) + '|' + str(savedLink.user)
          : 'new') + '|' + savedId;
      } else {
        // ★ M17：如果后端顺手保存了两步验证密钥（或明确说「验证码不保存」），
        //   如实显示 —— 但这只是提示，**密钥本身不会出现在任何返回值里**。
        appendOtpRows(rows, d);
      }
      renderResult('login-result', true, T('links.saved'), rows);
      logOk('连接配置已保存：' + savedId + (pendingOtp ? '（两步验证输入未保存，保留在输入框）' : ''));
      // ★ M17：没填两步验证时字段用完就清（绝不回显已保存内容）；
      //   填了则**保留**，否则用户接下来点「保存并登录」会发现输入没了。
      if (!pendingOtp) { clearOtpField(); }
      return refreshStatus(true).then(function () {
        if (savedId && savedId !== state.currentLink) { return setCurrentLink(savedId); }
        return null;
      }).then(function () { return loadConnect().then(function () { return r; }); });
    });
  }

  /** 保存 + 写凭据 + （必要时）重启该连接的 daemon + 登录。**只动这一条连接**。 */
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
    var wantId = str(built.input.id || '');
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
        ['daemon 已重启', d.restarted ? '是' : '否']
      ];
      if (login && isObj(login.data)) {
        rows.push(['sid', str(login.data.sid_masked)]);
        rows.push(['user', str(login.data.user)]);
        rows.push(['uid', optText(login.data.uid)]);
        // ★ M17：这次登录是否走了两步验证。`two_step=true` = NAS 要求 2SV 且
        //   验证码通过了；false = 账号没开 2SV（或本次没用上）。
        rows.push(['两步验证', login.data.two_step === true
          ? '已通过（TOTP）'
          : '未启用 / 本次未使用']);
      }
      // ★ M17：本次是否保存了密钥 / 填的是不是一次性验证码（密钥本身不回传）。
      appendOtpRows(rows, d);
      if (d.error) { rows.push(['错误', str(d.error)]); }
      if (login && isObj(login.error)) {
        rows.push(['登录错误', str(login.error.kind) + ': ' + str(login.error.message)]);
      }
      var ok = d.ok === true;
      renderResult('login-result', ok, ok ? '登录成功' : '登录失败', rows);
      if (ok) {
        logOk('登录成功（' + (wantId || '新连接') + '）');
        // ★ M17：密钥/验证码用完就清（绝不回显；失败时也清，避免显眼看得到的过期码）。
        clearOtpField();
        return adoptLink(wantId, built.input).then(function () { return loadConnect(); }).then(function () { return r; });
      }
      clearOtpField();
      return refreshStatus(true).then(function () { return r; });
    });
  }

  /** 登录/保存后把「刚配置的连接」设为当前：优先用已知 id，否则按 host/port/user 找回。 */
  function adoptLink(knownId, input) {
    state.creatingLink = false;   // ★ M16.1：登录/保存成功后表单不再是「新建」态
    return refreshStatus(true).then(function () {
      var id = str(knownId);
      if (id && linkById(id)) {
        state.editingLinkId = id;
        return setCurrentLink(id);
      }
      var ls = linksList();
      for (var i = 0; i < ls.length; i++) {
        var l = ls[i];
        if (str(l.host).toLowerCase() === str(input.host).toLowerCase()
          && num(l.port, 0) === num(input.port, 0)
          && str(l.user) === str(input.user)) {
          state.editingLinkId = str(l.id);
          return setCurrentLink(str(l.id));
        }
      }
      return null;
    });
  }

  /** 顶栏 / 连接页的「启动 daemon」——只启动 `linkId`（默认当前连接）。 */
  function doDaemonStart(linkId, btn) {
    var id = str(linkId) || currentId();
    var buttons = btn ? [btn] : ['btn-daemon-start', 'btn-connect-start'];
    return withBusy({ buttons: buttons }, function () {
      return call('daemon_start', { linkId: id });
    }).then(function (r) {
      if (!r.ok) {
        logErr('启动 daemon 失败（' + linkLabelById(id) + '）：' + str(r.error));
      } else {
        var d = isObj(r.data) ? r.data : {};
        if (d.error) {
          logErr('daemon_start（' + linkLabelById(id) + '）: ' + str(d.error));
        } else if (d.already) {
          logInfo('连接 ' + linkLabelById(id) + ' 的 qxyncd 已在运行');
        } else {
          logOk('连接 ' + linkLabelById(id) + ' 的 qxyncd 已启动' + (d.bin ? '：' + str(d.bin) : ''));
        }
      }
      return refreshStatus(true).then(function () { return r; });
    });
  }

  function doDaemonStop(linkId, btn) {
    var id = str(linkId) || currentId();
    if (!window.confirm(T('links.stop_confirm', { label: linkLabelById(id) }))) {
      return Promise.resolve(null);
    }
    var buttons = btn ? [btn] : ['btn-daemon-stop', 'btn-connect-stop'];
    return withBusy({ buttons: buttons }, function () {
      return call('daemon_stop', { linkId: id });
    }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        if (d.already) { logInfo('连接 ' + linkLabelById(id) + ' 的 qxyncd 本来就没在运行'); }
        else if (d.stopped) { logOk('连接 ' + linkLabelById(id) + ' 的 qxyncd 已停止'); }
        else { logErr('连接 ' + linkLabelById(id) + ' 的 qxyncd 未能在超时内退出'); }
      } else {
        logErr('停止 daemon 失败（' + linkLabelById(id) + '）：' + str(r.error));
      }
      return refreshStatus(true).then(function () { return r; });
    });
  }

  function doLinksStartAll() {
    return withBusy({ buttons: ['btn-links-start-all'] }, function () {
      return call('daemon_start_all', {});
    }).then(function (r) {
      if (!r.ok) {
        logErr('全部启动失败：' + str(r.error));
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      var ls = (d.links || []);
      var okCount = 0;
      for (var i = 0; i < ls.length; i++) {
        var res = isObj(ls[i].result) ? ls[i].result : {};
        if (res.error) {
          logErr('启动 ' + str(ls[i].id) + ' 失败：' + str(res.error));
        } else {
          okCount++;
        }
      }
      logOk('全部启动完成：' + okCount + '/' + ls.length + ' 个连接已在运行');
      return refreshStatus(true).then(function () { return r; });
    });
  }

  function doLinksStopAll() {
    if (!window.confirm(T('links.stop_all_confirm'))) { return Promise.resolve(null); }
    return withBusy({ buttons: ['btn-links-stop-all'] }, function () {
      return call('daemon_stop_all', {});
    }).then(function (r) {
      if (!r.ok) { logErr('全部停止失败：' + str(r.error)); }
      else {
        // ★ 某条连接被占用时会**单独跳过**（不影响其它连接），逐条把「谁在占用」报出来。
        var d = isObj(r.data) ? r.data : {};
        var ls = Array.isArray(d.links) ? d.links : [];
        var bad = 0;
        for (var i = 0; i < ls.length; i++) {
          if (ls[i].error) {
            bad++;
            logErr('停止 ' + str(ls[i].id) + ' 失败：' + str(ls[i].error));
          }
        }
        if (!bad) { logOk('全部连接的 daemon 已停止'); }
        else if (bad < ls.length) { logOk('其余 ' + (ls.length - bad) + ' 个连接的 daemon 已停止'); }
      }
      return refreshStatus(true).then(function () { return r; });
    });
  }

  /**
   * ★ 移除连接：先取 `purge_plan`（会删哪些任务登记 / 本地数据、各多大），再弹自定义弹窗。
   *
   *   * 复选框**不勾** → `link_delete(deleteTasks:true, purgeData:false)`：停 daemon、
   *     卸载挂载点、删任务登记与 link/凭据，**缓存 / 同步状态 / 队列 / 日志留着**；
   *   * 勾了 → `purgeData:true`：上面这些 + 该连接的全部本地数据一起删。
   *
   * 旧的「先探测 needs_confirm 再 window.confirm」两段式**已由这个计划弹窗取代**
   * （计划里本来就列了任务登记；后端仍然保留 needs_confirm 兜底，这里不再依赖它）。
   */
  function doLinkDelete(linkId, btn) {
    var id = str(linkId);
    var link = linkById(id);
    if (!link) { return Promise.resolve(null); }
    var label = linkLabel(link);
    return withBusy({ buttons: btn ? [btn] : [] }, function () {
      return call('purge_plan', { scope: 'link', linkId: id });
    }).then(function (r) {
      if (!r.ok) {
        renderError('login-result', '读取删除计划失败', str(r.error));
        logErr('读取移除连接计划失败（' + label + '）：' + str(r.error));
        return null;
      }
      var plan = isObj(r.data) ? r.data : {};
      var items = Array.isArray(plan.items) ? plan.items : [];
      var taskCount = 0;
      for (var i = 0; i < items.length; i++) {
        if (isObj(items[i]) && str(items[i].kind) === 'task') { taskCount++; }
      }
      return openPurgeDialog({
        title: T('purge.link_title', { label: label }),
        message: T('purge.link_message', { label: label, n: taskCount }),
        items: plan.items,
        dbRows: plan.db_rows,
        totalBytes: plan.total_bytes,
        totalFiles: plan.total_files,
        warnings: plan.warnings,
        optLabel: T('purge.opt_link', { size: humanSize(plan.optional_bytes) }),
        optHint: T('purge.opt_hint'),
        confirmText: T('purge.confirm_remove')
      }).then(function (ans) {
        if (!ans.confirmed) {
          logInfo('已取消移除连接 ' + id);
          return null;
        }
        return withBusy({ buttons: btn ? [btn] : [] }, function () {
          return call('link_delete', {
            linkId: id, deleteTasks: true, purgeData: ans.purge === true
          });
        }).then(function (r2) {
          return afterLinkRemoved(id, r2);
        });
      });
    });
  }

  function afterLinkRemoved(id, r) {
    if (!r || !r.ok) {
      if (r) { renderError('login-result', '移除连接失败', str(r.error)); logErr('移除连接失败：' + str(r.error)); }
      return r;
    }
    var d = isObj(r.data) ? r.data : {};
    var p = isObj(d.purged) ? d.purged : null;
    var tasksDeleted = num(d.tasks_deleted, 0);
    if (p && p.error) {
      logErr('清除连接 ' + id + ' 的本地数据失败：' + str(p.error));
    }
    var msg;
    if (p) {
      msg = T('purge.done_link', {
        label: id, size: humanSize(p.bytes), files: num(p.files, 0)
      }) + '（删除任务登记 ' + tasksDeleted + ' 个）';
    } else {
      msg = '已移除连接 ' + id + '（删除任务登记 ' + tasksDeleted + ' 个；本地数据保留）';
    }
    logOk(msg + '；本地文件 / NAS 数据未动');
    if (str(state.editingLinkId) === id) { state.editingLinkId = ''; }
    state.creatingLink = false;   // ★ M16.1：移除后回到「展示当前连接」而不是停在新建态
    // 当前连接被删掉后，refreshStatus 里的 ensureCurrentLink 会自动切到第一个剩余连接
    return refreshStatus(true).then(function () {
      return loadConnect().then(function () { return refreshCurrentPage(); });
    }).then(function () { return r; });
  }

  /**
   * ★ 设置 → 高级 → 危险区：删除本机所有 qxync 数据。
   *
   * 先看计划（各根目录 + 大小）→ 弹窗（必须勾选「我了解…」才能确认）→ `purge_all`。
   * 成功后把前端状态清零（连接 / 任务 / 缓存），并清掉 localStorage 里的当前连接；
   * 然后按**空连接**重新渲染 —— 删完没有连接时应显示空态，而不是白屏或报错。
   */
  function doPurgeAll(btn) {
    return withBusy({ buttons: btn ? [btn] : ['btn-purge-all'] }, function () {
      return call('purge_plan', { scope: 'all' });
    }).then(function (r) {
      if (!r.ok) {
        renderError('advanced-result', '读取删除计划失败', str(r.error));
        logErr('读取删除计划失败：' + str(r.error));
        return null;
      }
      var plan = isObj(r.data) ? r.data : {};
      return openPurgeDialog({
        title: T('purge.all_title'),
        message: T('purge.all_message'),
        items: plan.items,
        dbRows: plan.db_rows,
        totalBytes: plan.total_bytes,
        totalFiles: plan.total_files,
        warnings: plan.warnings,
        optLabel: T('purge.opt_all'),
        optHint: T('purge.opt_hint'),
        optRequired: true,
        confirmText: T('purge.confirm_delete')
      }).then(function (ans) {
        if (!ans.confirmed) {
          logInfo('已取消删除本机全部 qxync 数据');
          return null;
        }
        return withBusy({ buttons: btn ? [btn] : ['btn-purge-all'] }, function () {
          return call('purge_all', {});
        }).then(function (r2) {
          if (!r2.ok) {
            renderError('advanced-result', '删除失败', str(r2.error));
            logErr('删除本机全部 qxync 数据失败：' + str(r2.error));
            return r2;
          }
          var d = isObj(r2.data) ? r2.data : {};
          var warnings = Array.isArray(d.warnings) ? d.warnings : [];
          for (var wi = 0; wi < warnings.length; wi++) {
            logErr('purge_all 警告：' + str(warnings[wi]));
          }
          logOk(T('purge.done_all', { size: humanSize(d.bytes), files: num(d.files, 0) }));
          // 前端状态清零：连接 / 任务 / 各按连接分桶的缓存全部作废
          state.links = [];
          state.linksBad = [];
          state.linksError = '';
          state.linksLoaded = true;
          state.linksSig = '';
          state.currentLink = '';
          state.statuses = {};
          state.tasksByLink = {};
          state.journalByLink = {};
          state.errorsByLink = {};
          state.decisionsByLink = {};
          state.editingLinkId = '';
          state.creatingLink = false;
          try { window.localStorage.removeItem(CURRENT_LINK_KEY); } catch (e) { /* 隐私模式忽略 */ }
          renderTop();
          renderConnSelect();
          renderConnList();
          renderScopeHints();
          return refreshStatus(true).then(function () {
            return loadConnect();
          }).then(function () {
            return refreshCurrentPage();
          }).then(function () {
            // 删完没有真实连接：明确显示空态（合成项还在，但它是「未配置」占位）
            var ls = linksList();
            var real = 0;
            for (var ri = 0; ri < ls.length; ri++) {
              if (ls[ri] && ls[ri].link_exists !== false) { real++; }
            }
            if (!real) { setListState('conn-list-empty', 'empty', T('links.empty')); }
            return r2;
          });
        });
      });
    });
  }

  /** 顶部「立即登录」：用**当前连接**已保存的凭据 + 账密登录（不碰表单里的口令）。 */
  function doQuickLogin() {
    var id = currentId();
    return call('credential_present', { linkId: id }).then(function (r) {
      var d = r.ok ? r.data : null;
      if (!d || !d.present) {
        logInfo('连接 ' + linkLabelById(id) + ' 没有已保存的凭据：请到「设置 → 连接」填写口令并「保存并登录」');
        switchPage('settings');
        state.editingLinkId = str(id);
        editLink(id).then(function () {
          var pw = $('f-password');
          if (pw) { pw.focus(); }
        });
        return null;
      }
      return call('daemon_start', { linkId: id }).then(function () {
        return withBusy({ buttons: ['btn-quick-login'] }, function () {
          // 不带 user/password：daemon 自己读该连接的 credentials 文件
          return ipcFor(id, { method: 'login' });
        });
      }).then(function (lr) {
        if (lr && lr.ok) {
          logOk('登录成功（' + linkLabelById(id) + '）：' + str(lr.data && lr.data.sid_masked) +
            ' user=' + str(lr.data && lr.data.user));
        } else {
          logErr('登录失败（' + linkLabelById(id) + '）：' + str(lr && lr.error));
        }
        refreshStatus(true);
        return lr;
      });
    });
  }

  // ============================================================ Tab 3 挂载
  function refreshMounts() {
    state.mountsLoading = true;
    var id = currentId();
    return call('daemon_status', { linkId: id }).then(function (r) {
      state.mountsLoading = false;
      if (r.ok && isObj(r.data)) {
        state.mountsError = '';
        // 顺手更新该连接的状态缓存：顶部聚合 / 任务卡不必等下一轮轮询
        state.statuses[id] = r.data;
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
    // 一对一：一个挂载点 = 一个 NAS 目录
    var remote = str($('m-remote') ? $('m-remote').value : '/home').trim();
    if (!mp) {
      renderError('mount-result', '参数错误', '挂载点不能为空');
      return Promise.resolve(null);
    }
    if (!remote) {
      renderError('mount-result', '参数错误', 'NAS 目录不能为空（默认 /home）');
      return Promise.resolve(null);
    }
    var req = {
      method: 'mount',
      mountpoint: mp,
      remote: remote,
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
          ['NAS 目录', remote],
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
    var cst = currentStatus();
    if (!requireDaemon() || !requireLogin()) {
      state.files.loading = false;
      state.files.error = '';
      if (cst && !cst.running) {
        state.files.error = T('files.error_daemon_link', { label: linkLabelById(currentId()) });
      } else if (cst && cst.running) {
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
          // ★ T8：同步维度汇总（老 daemon 不给这几个字段 → num() 取默认 0）
          in_sync: num(r.data.in_sync, 0),
          out_of_sync: num(r.data.out_of_sync, 0),
          unknown: num(r.data.unknown, 0),
          transferring: num(r.data.transferring, 0),
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

  function openRowMenu(ev, e, full, selectEl) {
    var menu = $('row-menu');
    if (!menu) { return; }
    // selectEl 是该行的 pin 下拉：菜单里改完 pin 把它回填上，
    // 否则用户「设了 pinned」却还看着下拉显示「（未知）」。
    state.rowMenu = { entry: e, path: full, selectEl: selectEl || null };
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
    if (act === 'pinquery') {
      // ★ 查询（不设置）：结果回填到该行的 pin 下拉上，不传 state 就是查询
      var sel = cur.selectEl || null;
      if (sel) { sel.value = ''; }
      setPinState(full, null, sel, null);
      refreshFileStatesSoon();
    } else if (act === 'pin') {
      setPinState(full, 'pinned', cur.selectEl || null, null);
      refreshFileStatesSoon();
    } else if (act === 'unpin') {
      setPinState(full, 'unspecified', cur.selectEl || null, null);
      refreshFileStatesSoon();
    } else if (act === 'unpinned') {
      setPinState(full, 'unpinned', cur.selectEl || null, null);
      refreshFileStatesSoon();
    } else if (act === 'excluded') {
      setPinState(full, 'excluded', cur.selectEl || null, null);
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
    if (!window.confirm('确定删除 NAS 上的条目？\n' + full)) { return; }
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
      // ★ M15/T8：同步维度另起一句（与空间三态正交，不能合成一个数）
      info += T('files.summary_sync', {
        in_sync: num(state.fsSummary.in_sync, 0),
        out_of_sync: num(state.fsSummary.out_of_sync, 0),
        unknown: num(state.fsSummary.unknown, 0) > 0
          ? T('state.syncUnknown') + ' ' + num(state.fsSummary.unknown, 0)
          : ''
      });
      if (num(state.fsSummary.transferring, 0) > 0) {
        info += T('files.summary_transferring', {
          transferring: num(state.fsSummary.transferring, 0)
        });
      }
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

    // 类型（have_child 合并在这里：目录名后加 ▾ 表示有子目录）
    var isDir = e.isfolder === true;
    var tdType = el('td', 'col-type');
    tdType.appendChild(document.createTextNode(isDir ? '📁' : '📄'));
    if (isDir && e.have_child === true) {
      var sub = el('span', 'has-child', '▾');
      sub.title = '有子目录';
      tdType.appendChild(sub);
    }
    tr.appendChild(tdType);

    // 文件名
    var tdName = el('td', 'name');
    var dispName = str(e.filename);
    tdName.title = dispName;          // ★ 省略号显示时靠title 给全名
    if (isDir) {
      var btn = el('button', 'name-link', dispName);
      btn.type = 'button';
      btn.addEventListener('click', function () {
        state.files.selected = null;
        refreshFiles(pathJoin(state.files.dir, e.filename));
      });
      tdName.appendChild(btn);
    } else {
      tdName.appendChild(el('span', 'name-file', dispName));
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
    // ★ M15/T8：同步维度（与上面的空间三态**正交** ——「仅在线 + 已同步」很正常）。
    //   progress 有值 → 正在传（百分比）；in_sync === false/null → 未同步/未知。
    if (fs && !isDir) {
      var prog = fs.progress;
      if (isObj(prog) && num(prog[1], 0) > 0 && num(prog[0], 0) < num(prog[1], 0)) {
        var pct = Math.floor(num(prog[0], 0) * 100 / num(prog[1], 0));
        tdState.appendChild(document.createTextNode(' '));
        tdState.appendChild(el('span', 'badge badge-syncing', T('state.progress', { pct: pct })));
      } else if (fs.in_sync === false) {
        tdState.appendChild(document.createTextNode(' '));
        tdState.appendChild(el(
          'span',
          'badge badge-warn',
          num(fs.conflicts, 0) > 0
            ? T('state.conflicts', { n: num(fs.conflicts, 0) })
            : T('state.outOfSync')));
      } else if (fs.in_sync === null || fs.in_sync === undefined) {
        tdState.appendChild(document.createTextNode(' '));
        tdState.appendChild(el('span', 'badge', T('state.syncUnknown')));
      }
    }
    if (fs && fs.dirty === true) {
      tdState.appendChild(document.createTextNode(' '));
      tdState.appendChild(el('span', 'badge badge-warn', T('state.dirty')));
    }
    tr.appendChild(tdState);

    // ★ pin 下拉先建出来：右键菜单 / 键盘菜单 /⋯ 按钮都要用它回填 pin 状态
    var sel = el('select', PIN_SELECT_CLASS);

    // 右键菜单（对齐 Qsync 的「节省空间模式 ▸ …」）
    tr.addEventListener('contextmenu', function (ev) {
      ev.preventDefault();
      openRowMenu(ev, e, full, sel);
    });
    // ★ M8.6：键盘打开同一份菜单（Shift+F10 或菜单键）—— 事件从行内的 checkbox/按钮冒泡上来
    tr.addEventListener('keydown', function (ev) {
      if (ev.key === 'ContextMenu' || (ev.shiftKey && ev.key === 'F10')) {
        ev.preventDefault();
        openRowMenu(ev, e, full, sel);
      }
    });

    // ★ pin 操作列 —— 从「下拉 + 4 个按钮」收成「下拉 + ⋯ 菜单」。
    //   原因：那 4 个按钮（查 pin / 设 pin / 下载 / 脱水）每一个都在行右键菜单里有
    //   等价项（查 pin 原本缺，已补），而它们把这一列撑到 ~370px —— 整张表因此
    //   溢出容器，最右列被**静默裁掉**，用户得横向拖才能看到。查询/设置/下载/
    //   脱水的文案与说明在菜单里反而更完整，所以收编过去不丢功能。
    var tdPin = el('td', 'col-pin');
    var opt0 = el('option', null, '（未知）');
    opt0.value = '';
    sel.appendChild(opt0);
    for (var i = 0; i < PIN_STATES.length; i++) {
      // 选项文字只留英文 key —— 中文说明挪到菜单项里，下拉框窄一点才放得下
      var o = el('option', null, PIN_STATES[i]);
      o.value = PIN_STATES[i];
      o.title = PIN_LABEL[PIN_STATES[i]];
      sel.appendChild(o);
    }
    sel.value = '';
    sel.title = '选中后从⋯ 菜单里设 pin';
    tdPin.appendChild(sel);

    var btnMore = el('button', 'mini row-more', '⋯');
    btnMore.type = 'button';
    btnMore.title = '这一行的操作（也可用右键 / Shift+F10）';
    btnMore.addEventListener('click', function (ev) {
      ev.preventDefault();
      ev.stopPropagation();          // 别让行自己再开一次
      openRowMenu(ev, e, full, sel);
    });
    tdPin.appendChild(document.createTextNode(' '));
    tdPin.appendChild(btnMore);

    tr.appendChild(tdPin);
    return tr;
  }

  function setPinState(path, pinState, selectEl, btn) {
    if (!requireDaemon()) { return; }
    var req = { method: 'pin', path: path };
    if (pinState) { req.state = pinState; }
    withBusy({ buttons: btn ? [btn] : [] }, function () {
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
    openModal('下载到本地', 'NAS：' + pathJoin(state.files.dir, e.filename) + '（大文件可能较久，daemon 超时 600s）', guess)
      .then(function (dest) {
        if (!dest) { return null; }
        var target = str(dest).trim();
        if (!target) { return null; }
        return withBusy({
          spinners: ['files-busy'],
          buttons: btn ? [btn] : []
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

  // 单文件/单目录的「脱水」= 用户明确点名要释放这个路径。
  // ★ 必须带force：daemon.rs 的 `let recent = if opts.force { 0 } else { 300 }`
  //   会给不带 force 的请求套上 300s「刚访问过」保护窗口 —— 而「刚下载 /
  //   刚读完就点脱水」恰恰是最想释放空间的场景，不该被自己的保护窗口挡住
  //   （等价于 CLI 的 `dehydrate --path X --force`）。
  //   注意只跳「刚访问过」这一项，dirty / 待上传 / 打开的 fd / mmap /
  //   在途 / pinned 这些安全检查一条都没松。
  function doDehydratePath(path, btn) {
    if (!requireDaemon()) { return; }
    withBusy({
      spinners: ['dehy-busy', 'files-busy'],
      buttons: btn ? [btn] : []
    }, function () {
      return ipc({ method: 'dehydrate', path: path, force: true });
    }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        var done = num(d.dehydrated, 0);
        var reasons = dehydrateReasons(d.blocked);
        var line = '脱水 ' + path + '：' + done + ' 项，释放 ' + humanSize(d.freed_bytes) +
          '（当前已用 ' + humanSize(d.used_bytes) + '）';
        if (reasons) { line += '｜被挡下：' + reasons; }
        // ★ 0 项一定要说清为什么：只打「0 项，释放 0 B」等于静默失败
        //   （用户无从判断是保护窗口、是 pinned，还是这个路径压根不在挂载范围内）。
        if (done > 0) {
          logOk(line);
        } else if (reasons) {
          logErr(line);
        } else {
          logInfo(line + '（没有可脱水的候选：该路径可能不在任何挂载点的远端根下）');
        }
      } else {
        logErr('脱水失败：' + str(r.error));
      }
      return r;
    });
  }

  /** 把 `blocked: [(路径, 原因), …]` 压成一行人话（最多列 3 条，其余折成计数）。 */
  function dehydrateReasons(blocked) {
    if (!isObj(blocked) || !blocked.length) { return ''; }
    var out = [];
    for (var i = 0; i < blocked.length; i++) {
      var pair = blocked[i];
      var why = (isObj(pair) && pair.length !== undefined) ? str(pair[1]) : '';
      if (!why && isObj(pair) && pair.reason !== undefined) { why = str(pair.reason); }
      if (why) { out.push(why); }
    }
    if (!out.length) { return ''; }
    var uniq = [];
    for (var j = 0; j < out.length; j++) {
      if (uniq.indexOf(out[j]) < 0) { uniq.push(out[j]); }
    }
    var head = uniq.slice(0, 3).join('、');
    return uniq.length > 3 ? head+ ' 等 ' + uniq.length + ' 类' : head;
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
    if (!window.confirm('确定删除 NAS 上的条目？\n' + full)) { return; }
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
    // ★ 累计值（不是「当前」）：当前是否被熔断只看下面的 delete_block_reason。
    kvText(dl, '删除被挡（累计）', num(sy.deletes_blocked, 0));
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
      addAlert('sync2-alerts', T('sync.blocked_alert', { reason: String(sy.delete_block_reason) }),
        null, { label: T('sync.force_deletes'), onClick: doForceDeletes });
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

  /** 同步动作（★ M16：默认当前连接；任务卡 / 放行删除显式传该任务所属连接）。 */
  function doSync(req, btnIds, label, linkId) {
    var lid = str(linkId) || currentId();
    if (!requireDaemon(lid)) { return Promise.resolve(null); }
    var r0 = { method: 'sync' };
    if (isObj(req)) {
      for (var k in req) {
        if (Object.prototype.hasOwnProperty.call(req, k)) { r0[k] = req[k]; }
      }
    }
    return withBusy({ spinners: ['sync-busy'], buttons: btnIds || [] }, function () {
      return ipcFor(lid, r0);
    }).then(function (r) {
      if (r.ok) {
        var d = isObj(r.data) ? r.data : {};
        logOk((label || 'sync') + '完成（' + linkLabelById(lid) + '）：refreshed=' + num(d.refreshed, 0) +
          ' conflicts=' + num(d.conflicts, 0) + ' uploaded=' + num(d.uploaded, 0) +
          ' deleted=' + num(d.deleted, 0) + ' blocked=' + num(d.deletes_blocked, 0));
      } else {
        logErr((label || 'sync') + '失败（' + linkLabelById(lid) + '）：' + str(r.error));
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

  // ★ T3：缓存内容校验（**纯本地** —— 不查 daemon 是否在跑，也不经 IPC）。
  //
  // 走 Tauri 命令 `cache_verify` 而不是 IPC：校验只读本地缓存文件与
  // `.qxstate`/`.qxsum`，「数据出问题的时候往往正是网络不方便的时候」，
  // 不该要求挂载还活着。
  function doVerify(repair) {
    return withBusy({
      spinners: ['verify-busy'],
      buttons: ['btn-verify', 'btn-verify-repair']
    }, function () {
      return call('cache_verify', { repair: !!repair });
    }).then(function (r) {
      if (!r || r.ok === false) {
        renderError('verify-result', '校验失败', (r && str(r.error)) || '未知错误');
        return r;
      }
      var d = (r.data && isObj(r.data)) ? r.data : r;
      if (d.skipped) {
        renderResult('verify-result', true, '校验', [
          ['状态', str(d.reason || '缓存目录不存在')]
        ]);
        return r;
      }
      var clean = d.clean === true;
      var label = repair ? '校验并修复' : '校验';
      var rows = [
        ['结论', clean ? '全部通过' : (num(d.bad_files, 0) + ' 个文件有问题')],
        ['扫过文件', num(d.files_scanned, 0)],
        ['校验字节', humanSize(d.bytes_verified)],
        ['耗时', num(d.elapsed_ms, 0) + ' ms']
      ];
      if (repair) {
        rows.push(['修复区间', num(d.repaired_chunks, 0)]);
        rows.push(['整文件哈希更新', num(d.whole_hashes_written, 0)]);
      }
      // 修完之后不算「失败」：坏区间已经退回按需水合，缓存回到了诚实的状态。
      // 显示红色只会让用户以为修复没生效，反而再去点一次。
      var title = clean ? label + '（通过）'
        : (repair ? label + '（已把坏区间退回按需水合）' : label + '（发现损坏）');
      renderResult('verify-result', clean || repair, title, rows);

      // 有问题的文件逐个列出，精确到**文件 + 区间** —— 这是用户能自己处理的粒度
      var list = (d.files || []).filter(function (f) {
        return isObj(f) && ((f.bad_chunks && f.bad_chunks.length) || f.stale_state || f.missing_sums);
      });
      var box = $('verify-result');
      if (box && list.length) {
        var ul = el('ul', 'plain-list mono');
        for (var i = 0; i < list.length && i < 30; i++) {
          var f = list[i];
          var why = [];
          if (f.stale_state) { why.push('位图不可信(' + str(f.stale_state) + ')'); }
          if (f.missing_sums) { why.push('缺校验和表'); }
          if (f.bad_chunks && f.bad_chunks.length) {
            var ids = f.bad_chunks.slice(0, 8).map(function (n) { return '#' + n; }).join(' ');
            why.push(f.bad_chunks.length + ' 个区间损坏: ' + ids +
              (f.bad_chunks.length > 8 ? ' …' : ''));
          }
          var li = el('li', null, str(f.cache) + ' —— ' + why.join('；'));
          ul.appendChild(li);
        }
        if (list.length > 30) {
          ul.appendChild(el('li', null, '…还有 ' + (list.length - 30) + ' 个（用 qxync verify --json 看全部）'));
        }
        box.appendChild(ul);
      }
      return r;
    });
  }

  // ============================================================ 状态轮询（★ M16 多连接）
  var linksInflight = null;   // 进行中的 links_status Promise（避免重叠请求）

  /** 把 links_status 的结果灌进 state；返回是否切换了当前连接。 */
  function applyLinksStatus(d) {
    var ls = (d.links || []).filter(isObj);
    state.linksBad = (d.bad || []).filter(isObj);
    if (!ls.length) {
      // 没有任何 link 文件：合成一个 default 占位项，让「启动 / 添加」仍然有落点。
      // （后端只在 default daemon 在跑时才合成；这里补上「没跑」的情况，UI 才有东西可点。）
      ls.push({
        id: 'default', host: '', port: 0, https: true, insecure: false, user: '',
        link_exists: false, credential_present: false, socket: '',
        daemon: { link_id: 'default', running: false, socket: '', ping: null, status: null, error: null }
      });
    }
    state.links = ls;
    state.linksError = '';
    state.linksLoaded = true;
    state.statuses = {};
    for (var i = 0; i < ls.length; i++) {
      state.statuses[str(ls[i].id)] = isObj(ls[i].daemon) ? ls[i].daemon : null;
    }
    return ensureCurrentLink();
  }

  /** 兼容旧后端 / 离线夹具：没有 links_status 时退回单连接 daemon_status。 */
  function applyLegacyStatus(r) {
    var st = (r && r.ok && isObj(r.data)) ? r.data : null;
    var id = str(st && st.link_id) || 'default';
    state.links = [{
      id: id, host: '', port: 0, https: true, insecure: false, user: '',
      link_exists: false, credential_present: false, socket: st ? str(st.socket) : '',
      daemon: st, synthetic: true
    }];
    state.linksBad = [];
    state.statuses = {};
    state.statuses[id] = st;
    state.currentLink = id;
    state.linksError = '';
    state.linksLoaded = true;
  }

  function fetchLinksStatus() {
    return call('links_status', {}).then(function (r) {
      if (r.ok && isObj(r.data) && Array.isArray(r.data.links)) {
        var changed = applyLinksStatus(r.data);
        if (changed) { resetLinkScopedState(); }
        return r;
      }
      // 旧后端 / 桩：退回单连接 daemon_status（default）
      return call('daemon_status', {}).then(function (r2) {
        applyLegacyStatus(r2);
        resetLinkScopedState();
        return r2;
      });
    });
  }

  function refreshStatus(force) {
    if (linksInflight) { return linksInflight; }
    linksInflight = fetchLinksStatus().then(function () {
      linksInflight = null;
      state.linksLoading = false;
      var changed = state.currentChanged === true;
      state.currentChanged = false;
      var sig = linkSig();
      var connUiChanged = (sig !== state.linksSig);
      state.linksSig = sig;
      renderTop();
      if (connUiChanged) {
        renderConnSelect();
        renderConnList();
      }
      renderScopeHints();
      if (changed) {
        // 当前连接被外部改掉/删掉了：按连接的缓存已经清空，要重新拉当前页
        refreshCurrentPage();
      } else {
        renderCurrentPage();
      }
      return { ok: true };
    }, function (e) {
      linksInflight = null;
      state.linksLoading = false;
      state.linksError = str(e);
      renderTop();
      renderConnSelect();
      renderConnList();
      return { ok: false, error: str(e) };
    });
    return linksInflight;
  }

  /** 「这一页只作用于当前连接」的可见说明（文件 / 诊断页顶部）。 */
  function renderScopeHints() {
    var cur = currentLinkEntry();
    var label = cur ? linkLabel(cur) : T('links.unconfigured');
    setText('diag-link-hint', T('diag.link_hint', { label: label }));
    setText('files-link-hint', T('files.link_hint', { label: label }));
    var jh = $('journal-link-hint');
    if (jh) { jh.textContent = T('journal.scope_hint'); }
    var eh = $('errors-link-hint');
    if (eh) { eh.textContent = T('errors.scope_hint'); }
    var th = $('tasks-links-hint');
    if (th) { th.textContent = T('tasks.scope_hint'); }
  }

  /** 轮询 / 切连接后只**重画**当前页（不发新请求，避免每 2s 打一遍 NAS）。 */
  function renderCurrentPage() {
    if (state.page === 'home') { renderHome(); }
    else if (state.page === 'tasks') { renderTasksPage(); }
    else if (state.page === 'settings') {
      if (state.sec === 'connect') { renderConnList(); renderConnSelect(); }
      else if (state.sec === 'free') { renderSpace(); }
    } else if (state.page === 'diag') {
      if (state.diag === 'status' || state.diag === 'sync') { renderStatusPanel(currentStatus()); }
      else if (state.diag === 'mounts') {
        renderMountTable('tbl-mounts', 'mounts-empty', 'mounts-count', state.mounts);
        renderMountTable('tbl-status-mounts', 'status-mounts-empty', 'mounts-status-count', state.mounts);
      }
    }
  }

  /** 切页 / 切连接后**按需重拉**当前页的数据。 */
  function refreshCurrentPage() {
    if (state.page === 'home') {
      renderHome();
      refreshMounts();
      refreshTasks();
    } else if (state.page === 'tasks') {
      refreshTasks();
      refreshDecisions();
    } else if (state.page === 'journal') {
      refreshJournal();
    } else if (state.page === 'errors') {
      refreshErrors();
    } else if (state.page === 'files') {
      refreshFiles(state.files.path);
    } else if (state.page === 'settings') {
      loadConnect();
      refreshSettings();
      if (state.sec === 'free') { refreshSpace(); }
      if (state.sec === 'lan') { refreshLan(); }
      if (state.sec === 'sync') { refreshFilters(); }
    } else if (state.page === 'diag') {
      if (state.diag === 'mounts') { refreshMounts(); }
      else if (state.diag === 'status') { refreshRoots(); }
    }
    return Promise.resolve(null);
  }

  function startPolling() {
    if (state.pollTimer) { return; }
    state.pollTimer = window.setInterval(function () {
      if (document.hidden) { return; }
      if (linksInflight) { return; }
      refreshStatus(true);
      // ★ M8.4：每 3 个 tick（≈6s）看一次错误列表，有新错误就发桌面通知
      state.errorTick = (state.errorTick || 0) + 1;
      if (state.errorTick % 3 === 0) { checkErrorsForNotify(); }
    }, POLL_MS);
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

  /** 全局设置读哪台 daemon：当前连接在跑就用它；否则用第一台运行中的；都没有就试当前
   *  （拿 IPC 层的准确错误，而不是假装「没有设置」）。 */
  function settingsLinkId() {
    var cur = currentId();
    var st = statusOf(cur);
    if (st && st.running) { return cur; }
    var ts = fanoutTargets();
    return (ts.length && ts[0].running) ? ts[0].id : cur;
  }

  function refreshSettings() {
    return ipcFor(settingsLinkId(), { method: 'settings' }).then(function (r) {
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
    // ★ M23：Dolphin 集成开关 + Baloo 检测结果（设置页打开时顺手刷一次）
    var dt = $('s-dolphin-tags'); if (dt) { dt.checked = s.dolphin_tags === true; }
    refreshBalooStatus();

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
    // ★ M23：Dolphin 集成开关（保存前的 Baloo 门禁在 doAdvancedSave 里做）
    s.dolphin_tags = isOn('s-dolphin-tags', false);
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

  /** 整体回写设置（带 autostart_exe：开机自启要写 GUI 自己的绝对路径）。
   *
   *  ★ M16：settings.json 是所有连接共用的**全局**设置，但每个 daemon 各有一份内存态 ——
   *  保存时必须广播给**所有运行中的** daemon（Promise.all），失败逐条提示；
   *  一台都没跑时只留文件，下次 daemon 起来自己会读。 */
  function saveSettings(s, resultBox, okTitle, rows) {
    var done = function (exe) {
      var req = { method: 'settings_save', settings: s };
      if (exe) { req.autostart_exe = exe; }
      var targets = fanoutTargets();
      return withBusy({
        buttons: ['btn-proxy-save', 'btn-personal-save', 'btn-advanced-save', 'btn-free-save']
      }, function () {
        return fanout(req, targets);
      }).then(function (rs) {
        var okCount = 0;
        var fails = [];
        var savedData = null;
        for (var i = 0; i < rs.length; i++) {
          if (rs[i].r && rs[i].r.ok) {
            okCount++;
            if (!savedData) { savedData = isObj(rs[i].r.data) ? rs[i].r.data : null; }
          } else {
            fails.push({ id: rs[i].id, msg: str(rs[i].r && rs[i].r.error) });
            logErr('设置保存失败（' + linkLabelById(rs[i].id) + '）：' + str(rs[i].r && rs[i].r.error));
          }
        }
        if (savedData) { state.settings = savedData; }
        renderSettings();
        if (!okCount) {
          // 一台都没写成功：如实报错（不要显示「已保存」）
          renderError(resultBox, T('settings.save_failed'),
            fails.length ? T('settings.broadcast_fail', { n: fails.length, msg: fails[0].msg })
                         : T('settings.save_no_daemon'));
          return { ok: false, errors: fails };
        }
        var extra = (rows || []).slice();
        extra.push(['广播', T('settings.broadcast', { ok: okCount, total: rs.length })]);
        if (fails.length) {
          extra.push(['失败', fails.map(function (f) {
            return linkLabelById(f.id) + ': ' + f.msg;
          }).join('；')]);
        }
        renderResult(resultBox, fails.length === 0, okTitle, extra);
        logOk(okTitle + '（' + okCount + '/' + rs.length + ' 个运行中的 daemon）');
        return { ok: fails.length === 0, errors: fails };
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
    var s = collectAdvanced(currentSettings());
    // ★ M23：勾了 Dolphin 集成 → 保存前先做 Baloo 预检。
    //   daemon 的 settings_save 里有同样的门禁（CLI / 竞态兜底），这里做是为了
    //   能在**保存失败之前**给出「弹终端一键排除」的引导，而不是一句报错。
    if (s.dolphin_tags === true) {
      return ipc({ method: 'baloo_status' }).then(function (r) {
        if (!r.ok) {
          // 检测本身失败（daemon 没跑 / 老版本 daemon 不认识这个方法）：
          // 不拦保存 —— 让 daemon 侧的门禁（或它的缺席）自己说话。
          logInfo('Baloo 检测不可用（' + str(r.error) + '），直接保存');
          return saveSettings(s, 'advanced-result', '高级设置已保存', []);
        }
        var d = isObj(r.data) ? r.data : {};
        if (d.can_enable !== false) { return doAdvancedSaveNow(s); }
        var mounts = [];
        for (var i = 0; i < (d.mounts || []).length; i++) {
          var m = d.mounts[i];
          if (m && m.indexed === true) { mounts.push(str(m.mountpoint)); }
        }
        return promptBalooExclude(mounts).then(function (fixed) {
          if (!fixed) { return { ok: false, canceled: true }; }
          // 终端跑完了：重检一次，通过才保存（没通过就再来一轮提示）
          return doAdvancedSave();
        });
      });
    }
    return doAdvancedSaveNow(s);
  }

  function doAdvancedSaveNow(s) {
    return saveSettings(s, 'advanced-result', '高级设置已保存', []);
  }

  /**
   * ★ M23：Baloo 未排除 → 弹确认框，OK = 打开终端自动执行排除命令。
   * 返回 Promise<boolean>：true = 终端已弹出（用户跑完命令，可以重试保存）。
   */
  function promptBalooExclude(mounts) {
    var list = (mounts || []).join('、') || '（无）';
    return openConfirm(
      T('baloo.title'),
      T('baloo.why', { mounts: list }),
      T('baloo.open_terminal')
    ).then(function (yes) {
      if (!yes) { return false; }
      return call('open_baloo_exclude', { mountpoints: mounts || [] }).then(function (r) {
        var d = (r && isObj(r.data)) ? r.data : {};
        if (r && r.ok && d.ok) {
          logOk(T('baloo.opened', { terminal: str(d.terminal) }));
          logInfo(T('baloo.after_open'));
          return true;
        }
        logErr(T('baloo.open_failed', {
          err: str(d.error || (r && r.error) || '未知错误'),
          cmd: str(d.command || '')
        }));
        return false;
      }, function (e) {
        logErr(T('baloo.open_failed', { err: str(e), cmd: '' }));
        return false;
      });
    });
  }

  /** ★ M23：拉一次 Baloo 检测，画进高级分区的 kv 表。 */
  function refreshBalooStatus() {
    var dl = $('dolphin-kv');
    if (!dl) { return Promise.resolve(); }
    return ipc({ method: 'baloo_status' }).then(function (r) {
      clear(dl);
      if (!r.ok) {
        kvText(dl, 'Baloo 检测', '不可用（' + str(r.error) + '）');
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      var installed = !!d.balooctl;
      kvText(dl, 'Baloo', installed
        ? (d.indexing_enabled === false ? T('baloo.disabled') : T('baloo.enabled'))
        : T('baloo.not_installed'));
      var ms = d.mounts || [];
      if (!ms.length) {
        kvText(dl, '挂载点', T('baloo.no_mounts'));
      } else {
        for (var i = 0; i < ms.length; i++) {
          var m = ms[i];
          var label = m.indexed === true ? '⚠️ ' + T('baloo.mount_indexed')
            : (m.excluded === true ? '✅ ' + T('baloo.mount_excluded')
            : '✅ ' + T('baloo.mount_safe'));
          kvText(dl, str(m.mountpoint), label + (m.tags_active === true ? ' · ' + T('baloo.tags_on') : ''));
        }
      }
      if (installed && d.indexing_enabled !== false && d.dolphin_tags === true) {
        kvText(dl, '提示', T('baloo.tags_note'));
      }
      return r;
    });
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
      // ★ 用带日期的格式化：只显示 HH:MM:SS 会让人分不清是今天还是上周跑的。
      kvText(dl, '上次触发', humanTimeFromEpoch(num(d.last_run_unix, 0)));
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
    // ★ M16：筛选器挂在**当前连接**的 link 文件上（不是全局）
    return call('link_read', { linkId: currentId() }).then(function (r) {
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
    // ★ M16：写回**当前连接**的 link 文件（id 显式传当前连接）
    var link = state.link || currentLinkEntry() || {};
    if (!link.host || !link.user) {
      renderError('flt-match-result', '无法保存筛选器',
        '当前连接缺少 host/user：先在「连接」分区保存一次连接配置');
      return Promise.resolve(null);
    }
    var input = {
      id: currentId(),
      host: str(link.host),
      port: num(link.port, 9834),
      https: link.https !== false,
      insecure: link.insecure === true,
      user: str(link.user),
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
      renderError('flt-match-result', '参数错误', '请输入一条 NAS 绝对路径，例如 /home/qxync-test/secret.bin');
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
        verdict = '不在任何要同步的 NAS 目录之内';
      }
      renderResult('flt-match-result', true, '判定：' + verdict, [
        ['路径', p],
        ['属于哪个目录', d.match_root ? str(d.match_root) : '（不在任何目录内）'],
        ['目录内相对路径', d.match_rel ? str(d.match_rel) : '—'],
        ['原因', str(d.match_reason) || '—']
      ]);
      return r;
    });
  }

  // ------------------------------------------------------------ 冲突策略 / 待裁决

  function conflictLabel(v) {
    if (v === 'ask') { return '每个文件都问我'; }
    if (v === 'rename_remote') { return '重命名 NAS 上的文件'; }
    if (v === 'replace_remote') { return '用本地文件替换 NAS 上的文件（⚠ 会丢 NAS 上的改动）'; }
    if (v === 'replace_local') { return '用 NAS 上的文件替换本地文件（⚠ 会丢本地改动）'; }
    return '重命名本地文件（默认，双方都不丢）';
  }

  /** 所有连接的任务登记（带来源连接）：设置页的冲突策略汇总用。 */
  function taskList() {
    var out = [];
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var id = str(ls[i].id);
      var b = state.tasksByLink[id];
      var infos = (b && b.data && b.data.tasks) ? b.data.tasks : [];
      for (var j = 0; j < infos.length; j++) {
        var t = infos[j] && infos[j].task ? infos[j].task : null;
        if (t) { out.push({ task: t, linkId: id }); }
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
      kvText(dl, str(ts[i].task.id) + ' · ' + linkLabelById(ts[i].linkId),
        conflictLabel(str(ts[i].task.conflict) || 'rename_local'));
    }
  }

  // ------------------------------------------------------------ ★ M16：冲突待裁决（跨连接）
  //
  // 每个 daemon 有自己的队列；这里 fanout 合并，卡片标出来源连接，裁决 / 忽略 / 对比 /
  // 清空都按**条目来源连接**路由 —— A 的裁决绝不能发到 B 的 daemon 上。

  function mergedDecisions() {
    var out = [];
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) {
      var id = str(ls[i].id);
      var b = state.decisionsByLink[id];
      if (!b || !b.data) { continue; }
      var items = (b.data.decisions || []).filter(isObj);
      for (var j = 0; j < items.length; j++) {
        out.push({ x: items[j], linkId: id });
      }
    }
    out.sort(function (a, b2) {
      return num(b2.x.created_unix, 0) - num(a.x.created_unix, 0);
    });
    return out;
  }

  function refreshDecisions() {
    state.decisionsLoading = true;
    state.decisionsError = '';
    renderDecisions();
    return fanout({ method: 'decisions', action: 'list' }).then(function (rs) {
      state.decisionsLoading = false;
      state.decisionsByLink = {};
      var errors = 0;
      var firstErr = '';
      for (var i = 0; i < rs.length; i++) {
        var id = str(rs[i].id);
        if (rs[i].r.ok) {
          state.decisionsByLink[id] = { data: isObj(rs[i].r.data) ? rs[i].r.data : null };
        } else {
          state.decisionsByLink[id] = { error: str(rs[i].r.error) };
          errors++;
          if (!firstErr) { firstErr = str(rs[i].r.error); }
          logErr('读取冲突待裁决队列失败（' + linkLabelById(id) + '）：' + str(rs[i].r.error));
        }
      }
      state.decisionsError = (errors > 0 && errors === rs.length)
        ? (noDaemonRunning() ? T('list.no_daemon_all') : firstErr) : '';
      renderDecisions();
      return { ok: errors === 0, errors: errors };
    });
  }

  function renderDecisions() {
    var box = $('decisions-list');
    if (!box) { return; }
    var items = mergedDecisions();
    clear(box);
    setBusyState('decisions-list', state.decisionsLoading === true);
    setText('decisions-count', items.length ? T('home.task_count', { n: items.length }) : '');
    if (state.decisionsError) {
      setListState('decisions-empty', 'error', T('list.error', { msg: state.decisionsError }));
    } else if (state.decisionsLoading && !Object.keys(state.decisionsByLink).length) {
      setListState('decisions-empty', 'loading', T('decisions.loading'));
    } else if (items.length === 0) {
      setListState('decisions-empty', 'empty', T('decisions.empty'));
    } else {
      setListState('decisions-empty', 'ready');
    }

    // 逐连接附注（部分失败 / 某连接 daemon 未跑 / 各 daemon 的 note）
    var notes = fanoutNotes(state.decisionsByLink);
    var ls = linksList();
    for (var k = 0; k < ls.length; k++) {
      var b = state.decisionsByLink[str(ls[k].id)];
      if (b && b.data && b.data.note) {
        notes.push(T('list.conn_note', { label: linkLabelById(str(ls[k].id)), note: str(b.data.note) }));
      }
    }
    var note = $('decisions-note');
    if (note) {
      note.hidden = !notes.length;
      note.textContent = notes.join('；');
    }
    for (var i = 0; i < items.length; i++) {
      box.appendChild(buildDecisionCard(items[i].x, items[i].linkId));
    }
  }

  function buildDecisionCard(x, linkId) {
    var lid = str(linkId) || currentId();
    var card = el('div', 'task-card dec-card');
    var body = el('div', 'task-body');
    body.appendChild(el('div', 'task-state',
      pathBase(str(x.path)) + (x.resolution ? T('decisions.resolved_pending') : T('decisions.pending'))));
    var linkRow = el('div', 'task-meta');
    linkRow.appendChild(el('span', 'badge badge-link', T('task.link_badge', { label: linkBadge(linkById(lid)) })));
    body.appendChild(linkRow);
    body.appendChild(el('div', 'task-meta', 'NAS ' + str(x.path)));
    body.appendChild(el('div', 'task-meta',
      '本地 ' + humanSize(num(x.local_size, 0)) + ' / NAS ' + humanSize(num(x.remote_size, 0)) +
      ' · 发现于 ' + (humanTimeFromEpoch(num(x.created_unix, 0)) || '—')));
    card.appendChild(body);

    var acts = el('div', 'task-actions');
    var mk = function (label, res, cls) {
      var b = el('button', 'mini' + (cls ? ' ' + cls : ''), label);
      b.type = 'button';
      b.addEventListener('click', function () { doDecisionResolve(x, res, b, lid); });
      return b;
    };
    // ★ 决定「保留哪边」之前得先能**看见差在哪** —— 没有内容对比就只能靠猜。
    var cmp = el('button', 'mini', T('decisions.act_diff'));
    cmp.type = 'button';
    cmp.setAttribute('aria-label', T('decisions.act_diff_aria', { name: pathBase(str(x.path)) }));
    cmp.addEventListener('click', function () { toggleDecisionDiff(x, card, cmp, lid); });
    acts.appendChild(cmp);
    acts.appendChild(mk(T('decisions.keep_local'), 'keep_local', 'primary'));
    acts.appendChild(mk(T('decisions.keep_remote'), 'keep_remote'));
    acts.appendChild(mk(T('decisions.keep_both'), 'keep_both'));
    // ★ 出口：这条我不想管 → 只删队列行，不动文件。
    var fgt = el('button', 'mini', T('decisions.act_forget'));
    fgt.type = 'button';
    fgt.addEventListener('click', function () { doDecisionForget(x, fgt, lid); });
    acts.appendChild(fgt);
    card.appendChild(acts);
    return card;
  }

  function doDecisionResolve(x, res, btn, linkId) {
    var lid = str(linkId) || currentId();
    var id = str(x.id);
    return withBusy({ buttons: btn ? [btn] : [] }, function () {
      return ipcFor(lid, { method: 'decisions', action: 'resolve', id: id, resolution: res });
    }).then(function (r) {
      if (!r.ok) {
        logErr(T('decisions.resolve_err', { msg: str(r.error) }));
        return r;
      }
      // 用**路径**而不是内部 id 记日志：id 是路径压平后的内部键，对用户没有意义。
      logOk(T('decisions.resolve_ok', { path: str(x.path), res: resolutionLabel(res) }) +
        '（' + linkLabelById(lid) + '）');
      refreshDecisions();
      return r;
    });
  }

  /// 「忽略这条」：只删队列行，**不动任何文件**。给用户一个明确的出口 ——
  /// 否则已裁决的条目只能干等下一轮同步，看着像卡住。
  function doDecisionForget(x, btn, linkId) {
    var lid = str(linkId) || currentId();
    if (!confirm(T('decisions.forget_confirm', { path: str(x.path) }))) { return Promise.resolve(); }
    return withBusy({ buttons: btn ? [btn] : [] }, function () {
      return ipcFor(lid, { method: 'decisions', action: 'forget', id: str(x.id) });
    }).then(function (r) {
      if (!r.ok) {
        logErr(T('decisions.forget_err', { msg: str(r.error) }));
        return r;
      }
      logOk(T('decisions.forget_ok', { path: str(x.path) }));
      refreshDecisions();
      return r;
    });
  }

  /** 清空**所有连接**的待裁决队列（只删队列记录，不动文件）。 */
  function doDecisionsClear(btn) {
    if (!confirm(T('decisions.clear_confirm'))) { return Promise.resolve(); }
    var targets = fanoutTargets();
    return withBusy({ buttons: btn ? [btn] : [] }, function () {
      return fanout({ method: 'decisions', action: 'clear' }, targets);
    }).then(function (rs) {
      var n = 0;
      var failed = 0;
      for (var i = 0; i < rs.length; i++) {
        if (rs[i].r.ok) { n += isObj(rs[i].r.data) ? num(rs[i].r.data.removed, 0) : 0; }
        else { failed++; logErr(T('decisions.clear_err', { msg: str(rs[i].r.error) })); }
      }
      logOk(T('decisions.clear_ok', { n: n }) + (failed ? T('decisions.clear_partial', { n: failed }) : ''));
      refreshDecisions();
      return rs;
    });
  }

  /// 展开 / 收起某一条的本地↔NAS 文本对比。
  ///
  /// ★ 竞态：请求飞行中再点会**再发一次请求**（第一问没回来时 `.dec-diff` 还不存在，
  ///   收起分支接不住），回包后可能 append 出第二个面板。所以飞行中禁用按钮（点击
  ///   不再触发），回包时先清掉旧面板再挂新的；列表若在飞行中被刷新（卡片已被重画、
  ///   脱离文档），就别往死节点里塞东西。
  function toggleDecisionDiff(x, card, btn, linkId) {
    var lid = str(linkId) || currentId();
    var open = card.querySelector('.dec-diff');
    if (open) {
      open.remove();
      btn.disabled = false;
      btn.removeAttribute('aria-busy');
      btn.textContent = T('decisions.act_diff');
      return Promise.resolve();
    }
    if (btn.disabled) { return Promise.resolve(); }   // 飞行中：忽略重复点击
    btn.disabled = true;
    btn.setAttribute('aria-busy', 'true');
    btn.textContent = T('decisions.act_diff_busy');
    var settle = function () {
      btn.disabled = false;
      btn.removeAttribute('aria-busy');
      btn.textContent = T('decisions.act_diff');
    };
    return ipcFor(lid, { method: 'decisions', action: 'diff', id: str(x.id) }).then(function (r) {      card.appendChild(buildDiffView(isObj(r.data) ? r.data : null));
      return r;
    }, function (e) {
      settle();
      throw e;
    });
  }

  /** 一行 diff 的 wire 形状（`DiffLine`）：`kind` 是 `' '`/`'+'`/`'-'` 单字符，两侧行号是数字，`text` 是字符串。 */
  function isDiffLine(l) {
    if (!isObj(l)) { return false; }
    var k = str(l.kind);
    return (k === ' ' || k === '+' || k === '-') &&
      typeof l.local_no === 'number' && isFinite(l.local_no) &&
      typeof l.remote_no === 'number' && isFinite(l.remote_no) &&
      typeof l.text === 'string';
  }

  /// 渲染 diff 面板。**`outcome.status` 不是 `diff` 时必须显示原因** ——
  /// 静默显示「无差异」会让人以为两边一样，那是危险的假象。
  ///
  /// ★ wire 形状（`crates/qxync-core/src/decisions.rs` 的 `#[serde(tag = "status")]`
  ///   newtype 变体）：`DiffResult` 的字段与 `status` **平级**，不嵌套在 `diff` 下：
  ///   `{"status":"diff","lines":[…],"added":N,"removed":M,"truncated":bool}`。
  ///   读错一层时旧代码有 `{}` 兜底 → 恒显「+0/-0 / 两边内容逐行相同」。
  ///   所以这里在 `status=diff` 之后**还要验形状**；缺字段就显示错误，
  ///   **绝不落成「逐行相同」**（没比过和比出零差异在界面上必须长得不一样）。
  ///
  /// 语义方向（与 `text_diff`/`removed`/`added` 的实际实现一致）：
  ///   `-` 行 = 只有本地有（`local_no>0`、`remote_no=0`），`removed` = 本地独有行数；
  ///   `+` 行 = 只有 NAS 有（`local_no=0`、`remote_no>0`），`added` = NAS 独有行数。
  function buildDiffView(d) {
    var box = el('div', 'dec-diff');
    if (!isObj(d)) {
      box.appendChild(buildDiffSkip(T('decisions.diff_no_data'), true));
      return box;
    }
    var o = isObj(d.outcome) ? d.outcome : null;
    if (!o) {
      box.appendChild(buildDiffSkip(T('decisions.diff_no_data'), true));
      return box;
    }
    var status = str(o.status);
    if (status === 'skipped') {
      box.appendChild(buildDiffSkip(diffSkipText(o), false));
      return box;
    }
    if (status !== 'diff') {
      // 既不是 diff 也不是 skipped：形状不认识。显示成错误说明（不是「无差异」）。
      box.appendChild(buildDiffSkip(T('decisions.diff_no_data'), true));
      return box;
    }

    // ★ 平级字段。缺任何一项都按「结果不可信」处理。
    var lines = Array.isArray(o.lines) ? o.lines : null;
    var added = (typeof o.added === 'number' && isFinite(o.added)) ? o.added : null;
    var removed = (typeof o.removed === 'number' && isFinite(o.removed)) ? o.removed : null;
    var linesOk = lines !== null && lines.every(isDiffLine);
    if (!linesOk || added === null || removed === null) {
      box.appendChild(buildDiffSkip(T('decisions.diff_bad_shape'), true));
      return box;
    }
    // 「统计说有差异、却一行都画不出来」是自相矛盾的结果 —— 同样不能当「逐行相同」。
    if (lines.length === 0 && (added > 0 || removed > 0)) {
      box.appendChild(buildDiffSkip(T('decisions.diff_bad_shape'), true));
      return box;
    }
    var truncated = o.truncated === true;

    var head = el('div', 'dec-diff-head');
    var stat = el('span', 'dec-diff-stat');
    // 顺序与颜色跟语义对齐：本地独有 = `-`（红），NAS 独有 = `+`（绿）。
    var del = el('span', 'del', '-' + removed);
    del.title = T('decisions.diff_local_only_hint');
    var add = el('span', 'add', '+' + added);
    add.title = T('decisions.diff_nas_only_hint');
    stat.appendChild(document.createTextNode(T('decisions.diff_local_only') + ' '));
    stat.appendChild(del);
    stat.appendChild(document.createTextNode(' / ' + T('decisions.diff_nas_only') + ' '));
    stat.appendChild(add);
    head.appendChild(stat);
    head.appendChild(el('span', null, T('decisions.diff_size', {
      l: humanSize(num(d.local_size, 0)),
      r: humanSize(num(d.remote_size, 0))
    })));
    box.appendChild(head);

    var body = el('div', 'dec-diff-body');
    if (lines.length === 0) {
      // 只有形状校验通过、且统计也确实是 0 差异时才敢这么说。
      body.appendChild(el('div', 'dec-diff-foot', T('decisions.diff_identical')));
    }
    for (var i = 0; i < lines.length; i++) {
      body.appendChild(buildDiffRow(lines[i]));
    }
    box.appendChild(body);
    if (truncated) {
      box.appendChild(el('div', 'dec-diff-foot', T('decisions.diff_truncated', { n: num(lines.length, 0) })));
    }
    return box;
  }

  function buildDiffRow(l) {
    var x = isObj(l) ? l : {};
    var kind = str(x.kind) === '+' ? 'add' : (str(x.kind) === '-' ? 'del' : 'eq');
    var row = el('div', 'dec-diff-row ' + kind);
    var lno = num(x.local_no, 0);
    var rno = num(x.remote_no, 0);
    row.appendChild(el('span', 'no', lno === 0 ? '' : String(lno)));
    row.appendChild(el('span', 'no', rno === 0 ? '' : String(rno)));
    row.appendChild(el('span', 'tx', str(x.text)));
    return row;
  }

  function buildDiffSkip(msg, isErr) {
    return el('div', 'dec-diff-skip' + (isErr ? ' err' : ''), msg);
  }

  /// 把 `DiffSkip` 的结构化原因翻成人话。
  function diffSkipText(o) {
    var reason = str(o.reason);
    if (reason === 'too_large') {
      return T('decisions.diff_too_large', {
        size: humanSize(num(o.size, 0)),
        limit: humanSize(num(o.limit, 0))
      });
    }
    if (reason === 'binary') { return T('decisions.diff_binary'); }
    if (reason === 'is_dir') { return T('decisions.diff_is_dir'); }
    if (reason === 'no_content') {
      // 「哪一侧读不到 + 人话原因」。
      //
      // ★ 前缀判重：`DiffSkip::NoContent.detail` 可能**已经自带**同义前缀 ——
      //   ① 旧 daemon 对本地读失败写的是 `detail: "本地内容读不到：{e}"`；
      //   ② fuse 的 WouldBlock 人话原因本身就以「本地内容还没水合完整（…）」开头。
      //   这时再套一层 `{side}内容读不到：{detail}` 会显示
      //   「本地内容读不到：本地内容读不到：…」。daemon 侧后续会去掉自己那层前缀，
      //   所以这里做**双向兼容**：有前缀就原样用，没有才补。
      //   `which` 只可能是 local / remote（daemon 赋值）；`unknown` 是 DiffData 的默认值，
      //   到了这里也不能瞎猜成 NAS（CLI 侧同样是「两侧之一」口径）。
      var isLocal = str(o.which) === 'local';
      var isRemote = str(o.which) === 'remote';
      var which = isLocal ? T('decisions.side_local')
        : (isRemote ? T('decisions.side_remote') : '两侧之一');
      var detail = str(o.detail).trim();
      if (detail.indexOf('内容读不到') >= 0 || detail.indexOf(which) === 0) {
        return detail;   // 已自带侧别/前缀，别重复
      }
      return T('decisions.diff_no_content', { side: which, detail: detail });
    }
    return T('decisions.diff_unknown');
  }

  function resolutionLabel(v) {
    if (v === 'keep_local') { return T('decisions.keep_local'); }
    if (v === 'keep_remote') { return T('decisions.keep_remote'); }
    if (v === 'keep_both') { return T('decisions.keep_both'); }
    return str(v);
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
      // ts 是 unix 秒；用带日期的格式化（同一个 clock(n) 曾把数字时间戳丢掉、恒显现在）。
      ul.appendChild(el('li', null, humanTimeFromEpoch(num(evs[i].ts, 0)) + '  ' +
        str(evs[i].kind) + '  ' + str(evs[i].path)));
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
      // ★ M16：LAN 对等配置写在**当前连接**的 link 文件里
      id: currentId(),
      host: str(link.host),
      port: num(link.port, 9834),
      https: link.https !== false,
      insecure: link.insecure === true,
      user: str(link.user),
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
    // ★ M16：关于页显示「当前连接」；daemon 运行数按全部连接聚合。
    var cur = currentLinkEntry();
    if (cur) {
      kvText(dl, '当前连接', linkBadge(cur));
      kvText(dl, 'NAS', cur.link_exists === false ? T('links.unconfigured')
        : str(cur.user) + '@' + str(cur.host) + ':' + str(cur.port));
    }
    var running = 0;
    var ls = linksList();
    for (var i = 0; i < ls.length; i++) { if (linkDaemonRunning(ls[i])) { running++; } }
    kvText(dl, 'daemon', running + ' / ' + ls.length + ' 运行中');
    kvBool(dl, '当前连接的 daemon', !!(currentStatus() && currentStatus().running),
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
    // ★ M16：File Station 打的是**当前连接**那台 NAS；daemon status 里的 link 最准，
    // 没有就退回 link 文件（state.link / links_status 里的条目）。
    var st = currentStatus();
    var s = (st && isObj(st.status)) ? st.status : null;
    var link = (s && isObj(s.link)) ? s.link : (state.link || currentLinkEntry() || null);
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
    // ★ M16：错误来自**哪个连接**必须一起看：去重键 = `linkId + ':' + 条目 id`
    //   （条目没有 id 时退回 ts|path|detail，语义等价）。
    return fanout({ method: 'journal', level: 'error', limit: 1 }, fanoutTargets()).then(function (rs) {
      var best = null;
      for (var i = 0; i < rs.length; i++) {
        if (!rs[i].r.ok) { continue; }
        var d = isObj(rs[i].r.data) ? rs[i].r.data : {};
        var es = (d.entries || []).filter(isObj);
        if (!es.length) { continue; }
        var e = es[0];
        if (!best || num(e.ts, 0) > num(best.e.ts, 0)) { best = { id: str(rs[i].id), e: e }; }
      }
      if (!best) { return { ok: true, entries: 0 }; }
      var e = best.e;
      var entryId = (e.id !== undefined && e.id !== null && str(e.id) !== '')
        ? str(e.id)
        : (str(e.ts) + '|' + str(e.path) + '|' + str(e.detail));
      var key = best.id + ':' + entryId;
      if (state.lastErrorKey === key) { return { ok: true, entries: 1 }; }
      state.lastErrorKey = key;
      if (state.settings && isObj(state.settings.settings) &&
          state.settings.settings.desktop_notifications === false) {
        return { ok: true, entries: 1 };   // 用户关掉了通知
      }
      notifyUser('qxync 同步出错（' + linkLabelById(best.id) + '）',
        str(e.path) + ' —— ' + str(e.detail));
      return { ok: true, entries: 1 };
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

    // ★ M16：切页只决定「去哪」，取数统一走 refreshCurrentPage（它知道每页要拉什么）。
    if (name === 'settings') {
      switchSettingsSec(sec || state.sec);
    } else if (name === 'diag') {
      switchDiag(state.diag);
      return;   // switchDiag 自己会拉当前子页的数据
    }
    refreshCurrentPage();
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
    if (name === 'connect') { loadConnect(); renderConnList(); }
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
      renderStatusPanel(currentStatus());
      renderScopeHints();
      // 切到「状态」拉一次 NAS 目录（家目录 + 登记的同步文件夹）；重复点同一子页不重复拉
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
    // ★ 一对一配对：NAS 文件夹下拉 / 浏览 / 选择器
    on('btn-nas-browse', 'click', function () { openNasPicker(); });
    on('btn-nas-up', 'click', function () { loadNasPickerDir(pathParent(nasPicker.dir || '/home')); });
    on('btn-nas-pick', 'click', function () { pickNasFolder(); });
    on('btn-nas-cancel', 'click', function () { closeNasPicker(); });
    // ★ M16：任务表单的「NAS 连接」——换连接 = 换 daemon 取候选 + 换保存落点
    on('t-link', 'change', function (ev) {
      if (state.taskFormEditing) { return; }   // 编辑时只读
      var v = str(ev.target && ev.target.value);
      if (v === NEW_LINK_OPTION) {
        // ★ M16.1：选中「＋ 新建连接…」不是换连接，而是去设置页新建；
        //   先把下拉恢复到真实连接上，否则下次渲染会停在一个不存在的连接 id 上。
        renderTaskLinkSelect();
        openNewLink(T('task.link_new_hint'));
        return;
      }
      state.taskFormLink = v || state.taskFormLink;
      renderTaskLinkSelect();
      nasSelectedRefresh();
      loadNasCandidates();
      // ★ M18：换连接 = 路径里的连接段、id 去重集合都可能变 → 重算自动派生字段
      applyTaskDefaults();
    });
    var nasSel = $('t-nas-folder');
    if (nasSel) {
      // ★ M18：换 NAS 文件夹（先选它）→ 任务 id 取文件夹名、两条路径跟着 id 走
      nasSel.addEventListener('change', function () { syncNasManual(); applyTaskDefaults(); });
    }
    // ★ M18：手动输入 NAS 路径同样是「选 NAS 文件夹」的一种（改动在 blur/回车时生效）
    var nasManual = $('t-nas-manual');
    if (nasManual) { nasManual.addEventListener('change', function () { applyTaskDefaults(); }); }
    // ★ M18：用户手改了哪个字段，就停止自动派生那个字段（id 改动仍会带动两条路径）
    var tidEl = $('t-id');
    if (tidEl) { tidEl.addEventListener('input', function () { onTaskIdEdited(); }); }
    var tmpEl = $('t-mountpoint');
    if (tmpEl) { tmpEl.addEventListener('input', function () { state.taskFormAuto.mountpoint = false; }); }
    var tcdEl = $('t-cache-dir');
    if (tcdEl) { tcdEl.addEventListener('input', function () { state.taskFormAuto.cache = false; }); }
    on('btn-task-defaults', 'click', function () { resetTaskDefaults(); });
    var nasBox = $('nas-picker');
    if (nasBox) {
      // 点遮罩 / Esc 关掉（列表项可聚焦，所以 Esc 在键盘浏览时也能用）
      nasBox.addEventListener('click', function (ev) {
        if (ev.target === nasBox) { closeNasPicker(); }
      });
      nasBox.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape') { ev.preventDefault(); closeNasPicker(); }
      });
    }
    var ft = $('form-task');
    if (ft) {
      ft.addEventListener('submit', function (ev) {
        ev.preventDefault();
        doTaskSave();
      });
    }

    // 主页快捷动作
    // ★ M16.1：「连接设置」必须落到「设置 → 连接」这一分区，不能沿用「上次看过的设置分区」
    //   （否则上次停在「关于」，点它会落到关于页，连接列表根本不出现）。
    on('btn-home-settings', 'click', function () { switchPage('settings', null, 'connect'); });
    // ★ M16.1：主页连接卡片上的「＋ 添加连接」——这里是用户最容易找「加连接」的地方。
    on('btn-home-add-link', 'click', function () { openNewLink(); });
    // ★ 主页「＋ 添加任务」= 任务页的「＋ 添加配对文件夹」：切到任务页并直接摊开文件夹对表单。
    //   （别再去诊断 → 挂载：那条路是「先挂载、再登记任务」的专家流程；
    //   任务页的表单提交走 tasks/save + tasks/resume，登记和挂载一步到位。）
    on('btn-home-add-task', 'click', function () { switchPage('tasks'); openTaskForm(); });
    on('btn-home-refresh', 'click', function () { refreshStatus(true); refreshMounts(); });
    on('btn-home-sync-now', 'click', function () {
      doSync({ once: true }, ['btn-home-sync-now'], '立即同步');
    });

    // 顶部动作（★ M16：作用在**当前连接**上）
    on('btn-daemon-start', 'click', function () { doDaemonStart(); });
    on('btn-daemon-stop', 'click', function () { doDaemonStop(); });
    on('btn-quick-login', 'click', function () { doQuickLogin(); });

    // ★ M16：顶部「当前连接」选择器（键盘/读屏都能用；select 自带 label）
    on('conn-select', 'change', function (ev) {
      var v = str(ev.target && ev.target.value);
      if (v) { setCurrentLink(v); }
    });
    // ★ M16.1：顶栏「＋」= 新建连接（就贴在「当前连接」旁边，看着下拉就能找到）
    on('btn-top-add-link', 'click', function () { openNewLink(); });

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
    // ★ M16：连接列表的工具栏
    on('btn-link-add', 'click', function () { newLink(); });
    on('btn-links-refresh', 'click', function () { refreshLinks(); });
    on('btn-links-start-all', 'click', function () { doLinksStartAll(); });
    on('btn-links-stop-all', 'click', function () { doLinksStopAll(); });
    on('btn-link-cancel-edit', 'click', function () { cancelLinkEdit(); });
    on('btn-link-delete', 'click', function (ev) {
      var id = str(state.editingLinkId);
      if (!id) { logInfo('没有正在编辑的连接'); return; }
      doLinkDelete(id, ev && ev.currentTarget);
    });

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

    // NAS 同步文件夹面板
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
    on('btn-sync-force', 'click', function () { doForceDeletes($('btn-sync-force')); });
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
    on('btn-verify', 'click', function () { doVerify(false); });
    on('btn-verify-repair', 'click', function () { doVerify(true); });

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
    // ★ M23：手动重检 Baloo（在终端里跑完排除命令后点它，kv 表会更新）
    on('btn-baloo-recheck', 'click', function () {
      refreshBalooStatus().then(function (r) {
        if (r && r.ok) { logOk(T('baloo.rechecked')); }
      });
    });
    // ★ 设置 → 高级 → 危险区：删除本机所有 qxync 数据（先看计划，必须勾选才能确认）
    on('btn-purge-all', 'click', function (ev) { doPurgeAll(ev && ev.currentTarget); });
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
    on('btn-decisions-clear', 'click', function (ev) {
      doDecisionsClear(ev && ev.currentTarget);
    });

    // 页面可见性恢复时立刻刷一次
    document.addEventListener('visibilitychange', function () {
      if (!document.hidden) { refreshStatus(true); }
    });
  }

  /**
   * ★ M8.4：托盘菜单动作。
   * Rust 侧只负责 emit（`tray://action`），真正的动作在前端做 —— 前端持有 IPC 与界面状态。
   */
  /** ★ M16：托盘动作（立即同步 / 暂停）要遍历**所有运行中的**连接。 */
  /**
   * ★ M8.4：托盘菜单动作。
   * Rust 侧只负责 emit（`tray://action`），真正的动作在前端做 —— 前端持有 IPC 与界面状态。
   * ★ M16：同步 / 暂停要**遍历所有运行中的连接**（托盘没有「当前连接」的概念）。
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
        trayFanout({ once: true }, '托盘：立即与 NAS 同步');
      } else if (action === 'pause') {
        trayFanout({ interval_secs: 0 }, '托盘：暂停轮询');
      }
      renderAbout();
    });
  }

  function trayFanout(req, label) {
    var targets = fanoutTargets();
    if (!targets.length || !targets[0].running) {
      logInfo(label + '：没有运行中的 daemon');
      return Promise.resolve(null);
    }
    // req 只是 sync 的参数（once / interval_secs），method 必须在这里补上
    var r0 = { method: 'sync' };
    if (isObj(req)) {
      for (var k in req) {
        if (Object.prototype.hasOwnProperty.call(req, k)) { r0[k] = req[k]; }
      }
    }
    return fanout(r0, targets).then(function (rs) {
      var ok = 0;
      for (var i = 0; i < rs.length; i++) {
        if (rs[i].r.ok) { ok++; }
        else { logErr(label + '（' + linkLabelById(rs[i].id) + '）失败：' + str(rs[i].r.error)); }
      }
      logOk(label + '：' + ok + '/' + rs.length + ' 个连接已完成');
      refreshStatus(true);
      return rs;
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
    wirePurgeModal();
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

    // ★ M16：先把持久化的「当前连接」读出来，再发任何按连接的请求
    state.currentLink = loadSavedLink();
    renderConnSelect();
    renderConnList();
    renderTop();
    renderScopeHints();

    loadConnect();
    refreshSettings();
    m84InfoRefresh();
    refreshDecisions();
    // 首轮状态 → 再按当前页重拉（顺序很重要：当前连接校验过、状态到齐才发按连接的请求）
    refreshStatus(true).then(function () {
      startPolling();
      return refreshCurrentPage();
    }, function () {
      startPolling();
      return refreshCurrentPage();
    });
    renderFiles();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
