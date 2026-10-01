/* ============================================================================
 * QSync GUI —— i18n 文案表（★ M8.6）
 *
 * 目标（对应 docs/M8-向Qsync-Client-6靠拢.md §M8.6 交付物 ④）：
 *   * **把 zh-CN 文案集中到一张表**（不引入任何 i18n 框架 / 不做 ICU 复数规则）；
 *   * **预留 en**：`en` 表现在是空表 —— 查不到就整条回落到 zh-CN（见 t() 的三级回落）；
 *   * 静态标记用 `data-i18n` / `data-i18n-title` / `data-i18n-placeholder` /
 *     `data-i18n-aria-label` 挂 key，`apply()` 在启动时统一套用；
 *   * JS 侧动态文案走 `T(key, vars)`；`{name}` 形式做插值（只支持具名占位，够用）。
 *
 * 范围（有意划定，避免 i18n 成本失控 —— 方案 §8 风险 10）：
 *   * **进表**：页面/分区/导航名、状态文案、空态/加载态/错误态文案、按钮与徽章、
 *     三态术语（仅在线 / 本地可用 / 始终可用）等**面向用户**的文案；
 *   * **不进表**：`logErr` / `logInfo` 之类的**诊断日志**（面向开发者，且带着命令行/字段名），
 *     以及 `index.html` 里带内联 `<code>` 的**混合标记段落**（保持原样，不与 HTML 结构耦合）。
 *
 * 一致性由两道检查守住：
 *   1. 启动时 `apply()` + `checkDOM()`：DOM 里挂了 data-i18n 但表里没有的 key 会写进操作日志；
 *   2. 编译期自检 `qxync-gui --self-test` 的 `ui_spec`（见 crates/qxync-gui/src/lib.rs）：
 *      扫 app.js 里所有 `T('key')` 用法，key 不在表里 → `ui_spec.ok=false`。
 * ==========================================================================*/
(function (global) {
  'use strict';

  // ---------------------------------------------------------------- zh-CN
  var ZH = {
    // ---- 应用外壳
    'app.title': 'QSync — QNAP 按需同步',
    'app.skip': '跳到主内容',
    'nav.aria': '主导航',
    'nav.home': '主页',
    'nav.tasks': '任务',
    'nav.files': '文件',
    'nav.journal': '更新',
    'nav.errors': '错误',
    'nav.settings': '设置',
    'nav.diag': '诊断',
    'nav.home_title': '主页（任务状态）',
    'nav.tasks_title': '任务（同步任务列表）',
    'nav.files_title': '文件（远端浏览 / 节省空间模式）',
    'nav.journal_title': '文件更新中心（同步日志）',
    'nav.errors_title': '错误列表（失败项）',
    'nav.settings_title': '设置（连接 / 同步 / 缓存 / 网络 / 通知）',
    'nav.diag_title': '诊断（专家模式：游标 / baseline / 水合 / 脱水）',
    'page.home': '主页',
    'page.tasks': '任务',
    'page.files': '文件',
    'page.journal': '文件更新中心',
    'page.errors': '错误列表',
    'page.settings': '设置',
    'page.diag': '诊断',
    'page.settings_sec': '设置 · {sec}',

    // ---- 诊断子页
    'diag.status': '状态 / 进度',
    'diag.mounts': '挂载',
    'diag.sync': '同步 / 缓存',
    'diag.tabs_aria': '诊断子页',

    // ---- 设置分区
    'sec.connect': '连接',
    'sec.proxy': '代理',
    'sec.sync': '同步与筛选',
    'sec.personal': '个人',
    'sec.advanced': '高级',
    'sec.free': '释放空间',
    'sec.lan': 'LAN 加速',
    'sec.about': '关于',
    'sec.tabs_aria': '设置分区',

    // ---- 顶栏 / 状态条
    'top.daemon_checking': 'qxyncd：检测中…',
    'top.daemon_down': 'qxyncd 未运行',
    'top.daemon_up': 'qxyncd 运行中',
    'top.daemon_up_err': 'qxyncd 运行中（状态读取有错）',
    'top.conn': '连接：{value}',
    'top.conn_none': '连接：无 link 信息',
    'top.conn_dash': '连接：—',
    'top.login': '登录：{value}',
    'top.login_none': '登录：未登录',
    'top.login_dash': '登录：—',
    'top.login_alive_dead': '（失活）',
    'top.login_ok': '已登录',
    'top.daemon_meta': 'daemon：pid {pid} · v{version} · uptime {uptime} · socket {socket}',
    'top.daemon_meta_down': 'daemon：未运行（socket {socket}）',
    'top.conn_meta': '连接：{user}@{host}:{port}',
    'top.conn_ipv4': ' · 仅 IPv4',
    'top.session_meta': 'session：{sid} · {state}',
    'top.meta_dash': '连接：—',
    'top.session_dash': 'session：—',

    // ---- 主页 / 任务卡
    'home.conn_down': 'qxyncd 未运行 —— 点顶部「启动 daemon」',
    'home.conn_no_link': 'daemon 在跑，但没有连接配置',
    'home.conn_ok': '{user}@{host}:{port} · {state}',
    'home.conn_connected': '已连接',
    'home.conn_disconnected': '未登录',
    'home.conn_qsync': ' · Qsync {version}',
    'home.tasks_empty': '还没有同步任务。点右上「＋ 添加任务」把本地文件夹与 NAS 文件夹配成一对。',
    'home.task_count': '（{n}）',
    'task.state_no_daemon': 'qxyncd 未运行',
    'task.state_not_logged_in': '未登录 NAS',
    'task.state_no_task': '还没有同步任务',
    'task.state_sync_error': '同步出错：{msg}',
    'task.state_delete_blocked': '有删除被熔断挡住（待确认）',
    'task.state_paused': '已暂停轮询',
    'task.state_no_poll': '尚未跑过同步轮次',
    'task.state_conflicts': '最近一轮有 {n} 个冲突副本',
    'task.state_all_synced': '所有文件均处于最新状态',
    'task.state_paused_task': '已暂停（登记停用，挂载点已卸载）',
    'task.state_not_mounted': '已启用，但当前未挂载',
    'task.pair': '本地 {local}  ⇄  NAS {remote}',
    'task.roots_by_link': '（由 link home_root 决定）',
    'task.not_set': '（未设）',
    'task.meta_read_write': '读写',
    'task.meta_read_only': '只读',
    'task.meta_cache': '缓存 {mode}',
    'task.meta_direction': '方向 {dir}',
    'task.meta_space_saving': '节省空间模式',
    'task.meta_smart_delete': '智能删除',
    'task.meta_exclude': '排除规则 {n}',
    'task.meta_conflict': '冲突 {policy}',
    'task.meta_last_poll': '上次轮询 {dur}前',
    'task.badge_sync': 'Sync',
    'task.badge_paused': '已暂停',
    'task.badge_unmounted': '未挂载',
    'task.badge_mounted': '已挂载',
    'task.badge_multi_root': '多根',
    'task.act_resume': '继续',
    'task.act_pause': '暂停',
    'task.act_mount': '挂载',
    'task.act_manage': '管理',
    'task.act_sync_now': '立即同步',
    'task.act_add': '＋ 添加任务',
    'task.act_add_pair': '＋ 添加配对文件夹',
    'task.act_settings': '设置',
    'task.act_delete': '删除登记',

    // ---- 三态（节省空间模式）
    'state.online': '仅在线',
    'state.dir': '目录',
    'state.dirty': '未上传',
    'state.local': '本地可用',
    'state.always': '始终可用',
    'files.summary_states': ' · 仅在线 {online} / 本地可用 {local} / 始终可用 {always}',
    'files.summary_no_mount': ' · 三态需挂载后才能算（daemon 侧没有该目录的挂载视图）',
    'files.info': '共 {n} 项',
    'files.info_dir': '共 {n} 项 · {dir}',
    'files.not_logged_in': '未登录 —— 先到「设置 → 连接」保存并登录',

    // ---- 通用四态（★ M8.6：加载中 / 读取失败 / 空 / 有数据）
    'list.error': '读取失败：{msg}',
    'list.error_daemon': '读不到数据：daemon 未运行（先在顶部点「启动 daemon」）',

    // ---- 各列表的空态 / 加载态
    'files.loading': '正在读取远端目录…',
    'files.empty': '目录为空，或尚未加载。',
    'journal.loading': '正在读取同步日志…',
    'journal.empty': '没有日志。挂载一个同步任务后点「立即同步」，或有实际同步活动时就会出现记录。',
    'journal.cleared': '日志已清空。',
    'journal.empty_filtered': '没有符合条件的日志。挂载一个同步任务后点「立即同步」，或有实际同步活动时就会出现记录。',
    'journal.counts': '共 {total} 条 · ok {ok} / error {error} / blocked {blocked}',
    'errors.loading': '正在读取错误列表…',
    'errors.empty': '没有失败项 —— 同步、上传、脱水目前都正常。',
    'tasks.loading': '正在读取任务列表…',
    'tasks.empty': '还没有同步任务。点右上「＋ 添加配对文件夹」把本地文件夹与 NAS 文件夹配成一对。',
    'tasks.empty_never': '还没有登记过任务。点右上「＋ 添加配对文件夹」建一个。',
    'tasks.empty_files': '任务目录里没有可用的任务文件。',
    'tasks.bad_file': '⚠️ 任务文件解析失败（已跳过）：{file} —— {err}',
    'decisions.loading': '正在读取冲突待裁决队列…',
    'decisions.empty': '没有待裁决的冲突。',
    'mounts.loading': '正在读取挂载表…',
    'mounts.empty': '暂无挂载。',
    'roots.loading': '正在读取远端根…',
    'roots.empty': '尚未读取远端根（点右上「刷新」）。',
    'roots.unreadable': '未读取到远端根（未登录时 daemon 不做可读性探测）。',
    'roots.folders_empty': 'NAS 未登记 Qsync 同步文件夹',
    'sync.missing': 'daemon 未上报 sync 状态。',
    'lan.devices_loading': '正在读取已配对设备…',
    'lan.devices_empty': '还没有配对的设备。',
    'lan.events_loading': '正在读取对端事件…',
    'lan.events_empty': '还没有收到对端事件。',
    'space.not_read': '尚未读取',
    'space.loading': '正在读取空间用量…',
    'settings.loading': '正在读取设置…',
    'settings.error': '设置读取失败：{msg}',
    'connect.loaded': '已加载连接配置：{path}',
    'connect.none': '尚无连接配置（{path}），填好后点「保存并登录」。',
    'connect.failed': '无法读取连接配置。',

    // ---- 日志面板 / 弹窗
    'log.title': '操作日志',
    'log.collapse': '收起',
    'log.expand': '展开',
    'log.clear': '清空',
    'log.count': '{n} 条',
    'modal.title': '输入',
    'modal.ok': '确定',
    'modal.cancel': '取消',
    'busy.text': '进行中…',

    // ---- 行右键菜单（节省空间模式）
    'menu.pin': '☑ 始终保留在此设备（Always keep on this device）',
    'menu.unpin': '☐ 取消固定',
    'menu.free': '⤓ 释放空间（Free up space）',
    'menu.get': '下载到本地…',
    'menu.copy': '复制远端路径',
    'menu.rm': '删除（远端）'
  };

  // ---------------------------------------------------------------- en（预留）
  // 空表 = 全部回落到 zh-CN。加词条时**只加需要改的**，不用整份复制：
  //   t() 的回落顺序是 en → zh-CN → key 本身。
  var EN = {};

  var TABLES = { 'zh-CN': ZH, en: EN };
  var FALLBACK = 'zh-CN';
  var locale = FALLBACK;
  var misses = {};

  function table(loc) {
    return TABLES[loc] || null;
  }

  /** 取一条文案：`{name}` 做具名插值；查不到就回落到 zh-CN，再不行返回 key（并记账）。 */
  function t(key, vars) {
    var out = null;
    var loc = table(locale);
    if (loc && typeof loc[key] === 'string') { out = loc[key]; }
    var fb = table(FALLBACK);
    if (out === null && fb && typeof fb[key] === 'string') { out = fb[key]; }
    if (out === null) {
      misses[String(key)] = true;
      out = String(key);
    }
    if (vars && typeof vars === 'object') {
      out = out.replace(/\{(\w+)\}/g, function (m, name) {
        return (vars[name] === undefined || vars[name] === null) ? m : String(vars[name]);
      });
    }
    return out;
  }

  function setLocale(loc) {
    locale = table(loc) ? loc : FALLBACK;
    return locale;
  }

  function currentLocale() { return locale; }

  /**
   * 把 DOM 里挂了 data-i18n* 的静态文案套一遍。
   * 属性前缀 `data-i18n` 支持的后缀：无（textContent）/ `-title` / `-placeholder` / `-aria-label`
   */
  function apply(root) {
    var doc = root || global.document;
    if (!doc || !doc.querySelectorAll) { return 0; }
    var n = 0;
    var pairs = [
      ['data-i18n', 'text'],
      ['data-i18n-title', 'title'],
      ['data-i18n-placeholder', 'placeholder'],
      ['data-i18n-aria-label', 'aria-label']
    ];
    for (var p = 0; p < pairs.length; p++) {
      var attr = pairs[p][0];
      var kind = pairs[p][1];
      var nodes = doc.querySelectorAll('[' + attr + ']');
      for (var i = 0; i < nodes.length; i++) {
        var node = nodes[i];
        var key = node.getAttribute(attr);
        if (!key) { continue; }
        var text = t(key);
        if (kind === 'text') { node.textContent = text; }
        else { node.setAttribute(kind, text); }
        n++;
      }
    }
    return n;
  }

  /** DOM 里挂了 data-i18n 但表里没有的 key（启动时记一次，便于发现拼错的 key）。 */
  function missingInDOM(root) {
    var doc = root || global.document;
    var out = [];
    if (!doc || !doc.querySelectorAll) { return out; }
    var attrs = ['data-i18n', 'data-i18n-title', 'data-i18n-placeholder', 'data-i18n-aria-label'];
    var fb = table(FALLBACK) || {};
    for (var a = 0; a < attrs.length; a++) {
      var nodes = doc.querySelectorAll('[' + attrs[a] + ']');
      for (var i = 0; i < nodes.length; i++) {
        var key = nodes[i].getAttribute(attrs[a]);
        if (key && typeof fb[key] !== 'string' && out.indexOf(key) < 0) { out.push(key); }
      }
    }
    return out;
  }

  function stats() {
    return {
      locale: locale,
      fallback: FALLBACK,
      zh_keys: Object.keys(ZH).length,
      en_keys: Object.keys(EN).length,
      dom_keys: missingInDOM().length,
      missing_dom: missingInDOM(),
      missing_used: Object.keys(misses)
    };
  }

  global.QSYNC_I18N = {
    t: t,
    apply: apply,
    setLocale: setLocale,
    locale: currentLocale,
    stats: stats,
    missingInDOM: missingInDOM,
    tables: TABLES
  };
})(typeof window !== 'undefined' ? window : this);
