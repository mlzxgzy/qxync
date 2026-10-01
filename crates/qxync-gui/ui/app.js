/* ============================================================================
 * QSync GUI 前端（vanilla ES2020，无打包器 / 无框架 / 无外部资源）
 *
 * 通道只有一个：window.__TAURI__.core.invoke(cmd, args)
 *   - Tauri v2 命令参数名是 camelCase（Rust 侧 snake_case）：link_id → linkId
 *   - 对象内部字段名保持后端 serde 的 snake_case
 *   - 所有来自 daemon 的字符串一律走 textContent / createElement，绝不拼 innerHTML
 * ==========================================================================*/
(function () {
  'use strict';

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
  var tauri = (typeof window !== 'undefined' && window.__TAURI__) ? window.__TAURI__ : null;
  var invokeFn = (tauri && tauri.core && typeof tauri.core.invoke === 'function')
    ? function (cmd, args) { return tauri.core.invoke(cmd, args); }
    : null;

  // --------------------------------------------------------------- 状态
  var state = {
    tab: 'status',
    files: {
      path: '/home',
      dir: '/home',
      entries: [],
      selected: null,
      loading: false
    },
    mounts: [],
    busy: 0,
    home: '',
    appInfo: null,
    lastStatus: null,
    pollTimer: null,
    logCount: 0
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
    setText('log-count', state.logCount + ' 条');
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

  function openModal(title, message, initial) {
    var box = $('modal');
    if (!box) { return Promise.resolve(null); }
    setText('modal-title', title);
    setText('modal-message', message || '');
    var input = $('modal-input');
    input.value = initial === undefined ? '' : String(initial);
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
      chipD.textContent = 'qxyncd：检测中…';
      return;
    }
    if (!st.running) {
      chipD.textContent = 'qxyncd 未运行';
      chipD.className = 'chip chip-err';
      chipC.textContent = '连接：—';
      chipL.textContent = '登录：—';
      setText('meta-daemon', 'daemon：未运行（socket ' + str(st.socket) + '）');
      setText('meta-conn', '连接：—');
      setText('meta-session', 'session：—');
      return;
    }

    var ping = isObj(st.ping) ? st.ping : null;
    chipD.textContent = 'qxyncd 运行中' + (ping ? ' v' + str(ping.daemon_version) : '');
    chipD.className = 'chip chip-ok';

    var s = isObj(st.status) ? st.status : null;
    var link = (s && isObj(s.link)) ? s.link : null;
    if (link) {
      chipC.textContent = '连接：' + str(link.host) + ':' + str(link.port) + (link.https ? ' (https)' : ' (http)');
      chipC.className = 'chip chip-ok';
    } else {
      chipC.textContent = '连接：无 link 信息';
      chipC.className = 'chip chip-warn';
    }

    var logged = !!(s && s.logged_in);
    var sess = (s && isObj(s.session)) ? s.session : null;
    if (logged) {
      chipL.textContent = '登录：' + str(sess ? sess.sid_masked : '已登录') + (sess && !sess.alive ? '（失活）' : '');
      chipL.className = 'chip ' + (sess && !sess.alive ? 'chip-warn' : 'chip-ok');
    } else {
      chipL.textContent = '登录：未登录';
      chipL.className = 'chip chip-warn';
    }

    setText('meta-daemon', 'daemon：pid ' + str(ping ? ping.pid : '—') + ' · v' + str(ping ? ping.daemon_version : '—') +
      ' · uptime ' + humanDuration(ping ? ping.uptime_secs : 0) + ' · socket ' + str(st.socket));
    setText('meta-conn', '连接：' + (link ? (str(link.user) + '@' + str(link.host) + ':' + str(link.port) +
      (link.ipv4_only ? ' · 仅 IPv4' : '')) : '—'));
    setText('meta-session', 'session：' + (sess ? (str(sess.sid_masked) + ' · ' + (sess.alive ? 'alive' : 'dead')) : '—'));

    if (st.error) {
      chipD.className = 'chip chip-warn';
      chipD.textContent = 'qxyncd 运行中（状态读取有错）';
    }
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

  function addAlert(containerId, text) {
    var box = $(containerId);
    if (!box) { return; }
    box.appendChild(el('div', 'alert', text));
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
    if (emptyId) { show(emptyId, list.length === 0); }

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
    setText('roots-count', roots.length ? '（' + roots.length + '）' : '');
    show('roots-empty', roots.length === 0);
    if (!roots.length) {
      setText('roots-empty', '未读取到远端根（未登录时 daemon 不做可读性探测）。');
    }

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
    show('roots-folders-empty', sf.length === 0);
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
        show('roots-empty', true);
        setText('roots-empty', '读取远端根失败：' + str(r.error));
        show('roots-folders-empty', true);
        var nt = $('roots-note');
        if (nt) { nt.hidden = true; nt.textContent = ''; }
      }
      return r;
    }, function (e) {
      rootsInflight = null;
      if (btn) { btn.disabled = false; }
      throw e;
    });
    return rootsInflight;
  }

  function refreshRootsIfVisible() {
    if (state.tab === 'status') { refreshRoots(); }
  }

  // ============================================================ Tab 2 连接
  function fillConnectForm(info, present) {
    var hint = $('connect-hint');
    if (hint) {
      if (!info) {
        hint.textContent = '无法读取连接配置。';
      } else if (info.exists && isObj(info.link)) {
        hint.textContent = '已加载连接配置：' + str(info.path);
      } else {
        hint.textContent = '尚无连接配置（' + str(info.path) + '），填好后点「保存并登录」。';
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
      mp.value = (str(info.home) || '~') + '/qsync-mnt';
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
        switchTab('connect');
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
    return call('daemon_status', {}).then(function (r) {
      if (r.ok && isObj(r.data)) {
        var s = isObj(r.data.status) ? r.data.status : null;
        state.mounts = (s && s.mounts) ? s.mounts : [];
      }
      renderMountTable('tbl-mounts', 'mounts-empty', 'mounts-count', state.mounts);
      renderMountTable('tbl-status-mounts', 'status-mounts-empty', 'mounts-status-count', state.mounts);
      return state.mounts;
    });
  }

  function refreshMountsIfVisible() {
    if (state.tab === 'mounts') { refreshMounts(); }
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
    if (!requireLogin()) { return Promise.resolve(null); }
    var target = pathNormalize(path === undefined ? state.files.path : path);
    var fpb = $('btn-path-refresh');
    var busy = $('files-busy');
    if (busy) { busy.hidden = false; }
    return ipc({ method: 'ls', path: target }).then(function (r) {
      if (busy) { busy.hidden = true; }
      if (!r.ok) {
        logErr('ls 失败：' + str(r.error));
        state.files.entries = [];
        state.files.selected = null;
        renderFiles();
        setText('files-info', 'ls ' + target + ' 失败：' + str(r.error));
        show('files-empty', true);
        var fpErr = $('fp-path');
        if (fpErr) { fpErr.value = target; }
        return r;
      }
      var d = isObj(r.data) ? r.data : {};
      state.files.dir = str(d.path || target);
      state.files.path = state.files.dir;
      state.files.entries = sortEntries(d.entries);
      state.files.selected = null;
      var fp = $('fp-path');
      if (fp) { fp.value = state.files.dir; }
      renderFiles();
      return r;
    });
  }

  function renderFiles() {
    var tb = $('files-tbody');
    if (!tb) { return; }
    clear(tb);
    var hideExcluded = isOn('fp-hide-excluded', false);
    var entries = state.files.entries || [];
    var shown = 0;

    for (var i = 0; i < entries.length; i++) {
      var e = entries[i];
      if (!isObj(e)) { continue; }
      shown++;
      tb.appendChild(buildFileRow(e, hideExcluded));
    }
    setText('files-info', '共 ' + entries.length + ' 项' + (state.files.dir ? ' · ' + state.files.dir : ''));
    show('files-empty', entries.length === 0);
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
      if (state.tab === 'status' || state.tab === 'sync') {
        renderStatusPanel(st);
      } else if (state.tab === 'mounts') {
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
    }, POLL_MS);
  }
  // 首轮 status 到位前切到某些 tab（用户手快，或 QSYNC_GUI_TAB 指定）时，
  // requireLogin() 还拿不到状态 → refreshFiles 会直接返回空态。status 回来后补一次。
  function refreshCurrentTab() {
    if (state.tab === 'mounts') { refreshMounts(); }
    else if (state.tab === 'connect') { loadConnect(); }
    else if (state.tab === 'status') { refreshRoots(); }
    else if (state.tab === 'files' && !state.files.entries.length) { refreshFiles(state.files.path); }
  }

  // ============================================================ Tab 切换
  function switchTab(name) {
    var prev = state.tab;
    state.tab = name;
    var tabs = document.querySelectorAll('.tab');
    for (var i = 0; i < tabs.length; i++) {
      var t = tabs[i];
      if (t.getAttribute('data-tab') === name) { t.classList.add('active'); }
      else { t.classList.remove('active'); }
    }
    var panels = document.querySelectorAll('.panel');
    for (var j = 0; j < panels.length; j++) {
      var p = panels[j];
      if (p.id === 'panel-' + name) { p.classList.add('active'); }
      else { p.classList.remove('active'); }
    }

    if (name === 'status') {
      renderStatusPanel(state.lastStatus);
      // 切到这个 tab 拉一次远端根（roots 贵，不跟 2s 轮询）；重复点同一 tab 不重复拉
      if (prev !== 'status') { refreshRoots(); }
    }
    if (name === 'sync') { renderStatusPanel(state.lastStatus); }
    if (name === 'mounts') { refreshMounts(); }
    if (name === 'connect') { loadConnect(); }
    if (name === 'files') {
      if (!state.files.entries.length) { refreshFiles(state.files.path); }
      else { renderFiles(); }
    }
  }

  // ============================================================ 日志面板
  function wireLogBox() {
    var head = $('logbox-head');
    var box = $('logbox');
    var toggle = $('log-toggle');
    if (head && box) {
      head.addEventListener('click', function (ev) {
        if (ev.target && ev.target.id === 'btn-log-clear') { return; }
        box.classList.toggle('collapsed');
        if (toggle) { toggle.textContent = box.classList.contains('collapsed') ? '展开' : '收起'; }
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
    var tabs = document.querySelectorAll('.tab');
    for (var i = 0; i < tabs.length; i++) {
      (function (t) {
        t.addEventListener('click', function () {
          switchTab(str(t.getAttribute('data-tab')));
        });
      })(tabs[i]);
    }

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

    // 页面可见性恢复时立刻刷一次
    document.addEventListener('visibilitychange', function () {
      if (!document.hidden) { refreshStatus(true); }
    });
  }

  function on(id, evt, fn) {
    var n = $(id);
    if (n) { n.addEventListener(evt, fn); }
  }

  // ============================================================ 启动
  function boot() {
    wireModal();
    wireLogBox();
    bindEvents();

    if (!invokeFn) {
      var banner = $('env-banner');
      if (banner) { banner.hidden = false; }
      logErr('未在 Tauri 中运行：window.__TAURI__.core.invoke 不存在，界面只做静态展示');
    } else {
      logInfo('QSync GUI 已启动，开始轮询 daemon_status（每 2s）');
    }

    // 先拿 app_info（挂载点默认值 / 路径面板 / HOME）
    call('app_info', {}).then(function (r) {
      if (r.ok) {
        renderAppInfo(isObj(r.data) ? r.data : null);
        // 调试/验收用：环境变量 QSYNC_GUI_TAB=<status|connect|mounts|files|sync>
        // 指定初始 tab（后端 app_info 透传），截图矩阵靠它逐个 tab 出图。
        var want = isObj(r.data) ? str(r.data.initial_tab) : '';
        var names = ['status', 'connect', 'mounts', 'files', 'sync'];
        if (want && names.indexOf(want) >= 0) { switchTab(want); }
      }
    });

    loadConnect();
    refreshStatus(true).then(function () { startPolling(); refreshCurrentTab(); },
                             function () { startPolling(); refreshCurrentTab(); });
    refreshMounts();
    renderFiles();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
