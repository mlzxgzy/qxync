#!/usr/bin/env bash
# gui-purge.sh —— ★ 删除 / 移除时「可一并删除相关数据（purge）」的 GUI 无头集成测试
#
# 验的是什么（三层共用一个确认弹窗，复选框默认不勾 = 保持旧行为）：
#   1. 任务卡「删除…」→ 先取 purge_plan，弹出 #purge-modal（条目非空、复选框默认未勾）；
#   2. 取消 → 什么都没删（没有 task_purge、没有 ipc tasks delete）；
#   3. Esc → 同样取消，没有删除调用；
#   4. 不勾选 + 确认 → 走旧路径（ipc_call tasks delete），不调用 task_purge；
#   5. 勾选 + 确认 → 调用 task_purge，args.linkId / args.taskId 正确；
#   6. 连接行「移除」→ 弹窗；勾选确认 → link_delete(deleteTasks=true, purgeData=true)；
#   7. 设置→高级→「删除本机所有 qxync 数据…」→ 弹窗；未勾选时确认按钮 disabled，
#      勾选后可点，点击后 purge_all 被调用；删完没有连接时显示空态（不是白屏/报错）。
#
# 怎么验的：把 crates/qxync-gui/ui 的资源复制进 .local-run 的临时目录，注入
#   * `window.__TAURI__` 打桩：links_status / tasks_config / app_info / ipc_call /
#     purge_plan / task_purge / link_delete / purge_all，调用记进 window.__CALLS__；
#   * 一段 driver 脚本：真实点按钮、勾复选框、发 Escape，读 DOM 与调用记录。
# 然后用 chromium 无头跑 `--dump-dom` 取结果 —— 跑的是**真 app.js**，不是另写一份逻辑。
#
# 依赖：chromium（没有就跳过，退出码 0 并打印 SKIP）。
# 用法：xtask/tests/gui-purge.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UI="$REPO/crates/qxync-gui/ui"
WORK="$REPO/.local-run/purge-gui-test"
OUT="$WORK/dom.html"
PROF="$WORK/profile"

if ! command -v chromium >/dev/null; then
  echo "SKIP：没有 chromium，跳过 purge 无头集成测试"
  exit 0
fi
for f in index.html app.js i18n.js style.css task-defaults.js; do
  [ -f "$UI/$f" ] || { echo "FAIL：缺少 UI 资源 $UI/$f"; exit 1; }
done

rm -rf "$WORK"
mkdir -p "$WORK" "$PROF"
cp "$UI/index.html" "$UI/app.js" "$UI/i18n.js" "$UI/style.css" "$UI/task-defaults.js" "$WORK/"

# ---------------------------------------------------------------- ① 打桩后端
cat >"$WORK/mock-tauri.js" <<'MOCK'
(function () {
  'use strict';
  try { window.localStorage.clear(); } catch (e) { /* file:// 下可能被禁 */ }

  // driver 用它断言「到底调了什么」；purge_all 之后 links_status 返回空列表（模拟删干净）。
  window.__CALLS__ = [];
  var PURGED_ALL = false;

  function rec(cmd, args) {
    var copy = null;
    try { copy = JSON.parse(JSON.stringify(args === undefined ? null : args)); } catch (e) { copy = null; }
    window.__CALLS__.push({ cmd: cmd, args: copy });
  }
  // 注意两种返回形状（与真实命令一致）：
  //   * 普通命令（links_status / app_info / purge_plan / task_purge / link_delete / purge_all）
  //     直接 resolve **数据本身** —— 前端 call() 会把它包成 {ok:true,data}；
  //   * 只有 ipc_call 走 daemon 的协议信封 {v, ok, data}（ipcFor 读的是 res.ok/res.data）。
  function raw(data) { return Promise.resolve(data); }
  function env(data) { return Promise.resolve({ v: 1, ok: true, data: data }); }
  function fail(msg) {
    return Promise.resolve({ ok: false, error: { kind: 'Mock', message: msg } });
  }

  // 磁盘上的任务登记：default 连接有一条 /home 的任务（任务卡上会出现「删除…」）。
  var TASK = {
    task: {
      id: 'home', link_id: 'default', name: 'home', enabled: true,
      mountpoint: '/home/u/qxync/default/home', cache_dir: null, root: '/home',
      read_write: true, cache_mode: 'pagecache', conflict: 'rename_local',
      direction: '2way', space_saving: false, smart_delete: false,
      selective: [], exclude: [], auto_unmount: true
    },
    mounted: true,
    last_error: ''
  };

  function linkInfo(id, host, user) {
    return {
      id: id, host: host, port: 9834, https: true, insecure: false, user: user,
      link_exists: true, credential_present: true, otp: false,
      socket: '/tmp/purge-' + id + '.sock',
      daemon: {
        link_id: id, running: true, socket: '/tmp/purge-' + id + '.sock',
        ping: { pid: 4242, daemon_version: 'mock-purge', uptime_secs: 12 },
        status: {
          logged_in: true, mounts: [], sync: null,
          session: { sid_masked: 'sid-***', alive: true }
        },
        error: null
      }
    };
  }

  /** 三种 scope 的固定计划（形状与后端契约一致）。 */
  function plan(scope) {
    if (scope === 'task') {
      return {
        scope: 'task', target: 'home', link_id: 'default', title: 'task home',
        items: [
          { path: '/home/u/.local/share/qxync/cache/tasks/default/home',
            kind: 'cache', label: '水合缓存', is_dir: true, bytes: 4096, files: 2, optional: true },
          { path: '/home/u/.local/state/qxync/sync/default/home.json',
            kind: 'sync', label: '同步状态', is_dir: false, bytes: 512, files: 1, optional: true },
          { path: '/home/u/.config/qxync/tasks/home.json',
            kind: 'task', label: '任务登记', is_dir: false, bytes: 256, files: 1, optional: false }
        ],
        db_rows: { baseline: 3, journal: 5, decisions: 1 },
        total_bytes: 4864, total_files: 4,
        optional_bytes: 4608, optional_files: 3,
        warnings: [], notes: []
      };
    }
    if (scope === 'link') {
      return {
        scope: 'link', target: 'default', link_id: 'default', title: 'link default',
        items: [
          { path: '/home/u/.config/qxync/tasks/home.json',
            kind: 'task', label: '任务登记 home', is_dir: false, bytes: 256, files: 1, optional: false },
          { path: '/home/u/.config/qxync/links/default.json',
            kind: 'link', label: '连接 default', is_dir: false, bytes: 300, files: 1, optional: false },
          { path: '/home/u/.config/qxync/credentials/default.json',
            kind: 'credentials', label: '凭据 default', is_dir: false, bytes: 120, files: 1, optional: false },
          { path: '/home/u/.local/state/qxync',
            kind: 'state', label: '状态目录', is_dir: true, bytes: 8192, files: 6, optional: true },
          { path: '/tmp/purge-default.sock',
            kind: 'socket', label: 'socket', is_dir: false, bytes: 0, files: 0, optional: true }
        ],
        db_rows: { baseline: 3, journal: 5, decisions: 0 },
        total_bytes: 9068, total_files: 9,
        optional_bytes: 8192, optional_files: 6,
        warnings: ['会一并删除 1 个任务登记'], notes: []
      };
    }
    return {
      scope: 'all', target: 'all', link_id: null, title: 'all',
      items: [
        { path: '/home/u/.config/qxync',
          kind: 'config', label: '配置目录', is_dir: true, bytes: 12288, files: 9, optional: false },
        { path: '/home/u/.local/share/qxync',
          kind: 'data', label: '数据目录', is_dir: true, bytes: 65536, files: 21, optional: false },
        { path: '/home/u/.local/state/qxync',
          kind: 'state', label: '状态目录', is_dir: true, bytes: 8192, files: 4, optional: false }
      ],
      db_rows: null,
      total_bytes: 86016, total_files: 34,
      optional_bytes: 0, optional_files: 0,
      warnings: ['这会停止所有 daemon'], notes: []
    };
  }

  window.__TAURI__ = {
    core: {
      invoke: function (cmd, args) {
        switch (cmd) {
          case 'app_info':
            return raw({
              version: 'mock-purge', home: '/home/u', socket: '/tmp/purge.sock',
              config_dir: '/home/u/.config/qxync',
              data_dir: '/home/u/.local/share/qxync',
              state_dir: '/home/u/.local/state/qxync',
              daemon_bin: null, daemon_running: true,
              links: [
                { id: 'default', host: 'nas-a.local', port: 9834, https: true,
                  insecure: false, user: 'test1', socket: '/tmp/purge-default.sock' },
                { id: 'nas2', host: 'nas-b.local', port: 9834, https: true,
                  insecure: false, user: 'test2', socket: '/tmp/purge-nas2.sock' }
              ],
              links_bad: [], ui_assets: {}, initial_tab: 'tasks'
            });
          case 'links_status':
            if (PURGED_ALL) { return raw({ links: [], bad: [] }); }
            return raw({
              links: [linkInfo('default', 'nas-a.local', 'test1'),
                      linkInfo('nas2', 'nas-b.local', 'test2')],
              bad: []
            });
          case 'tasks_config':
            return raw({ tasks: [], bad_files: [], empty: true, note: '' });
          // 旧后端 / 桩的兜底路径（links_status 拿不到时前端会退回这条），给个单连接形状
          case 'daemon_status':
            return raw({
              link_id: 'default', running: true, socket: '/tmp/purge-default.sock',
              ping: { pid: 4242, daemon_version: 'mock-purge', uptime_secs: 12 },
              status: { logged_in: true, mounts: [], sync: null },
              error: null
            });
          case 'ipc_call': {
            rec('ipc_call', args);
            var req = (args && args.req) || {};
            var method = String(req.method || '');
            var action = String(req.action || '');
            if (method === 'tasks' && action === 'list') {
              var mine = (String(args && args.linkId) === 'default') ? [TASK] : [];
              return env({ tasks: mine, empty: mine.length === 0, note: '' });
            }
            if (method === 'tasks' && action === 'delete') { return env({ deleted: true }); }
            if (method === 'decisions') { return env({ items: [], pending: 0 }); }
            return env({});
          }
          case 'purge_plan': {
            rec('purge_plan', args);
            return raw(plan(String((args && args.scope) || 'all')));
          }
          case 'task_purge':
            rec('task_purge', args);
            return raw({
              purged: true, task_id: String(args && args.taskId),
              deleted_paths: [], bytes: 4608, files: 3,
              db_rows: { baseline: 1, journal: 2, decisions: 0 },
              warnings: [], notes: []
            });
          case 'link_delete':
            rec('link_delete', args);
            return raw({
              deleted: true, needs_confirm: false, tasks_deleted: 1, stopped: true,
              purged: { bytes: 8192, files: 6, deleted_paths: [] }
            });
          case 'purge_all':
            rec('purge_all', args);
            PURGED_ALL = true;
            return raw({
              purged: true, bytes: 86016, files: 34,
              deleted_paths: [], warnings: [], notes: []
            });
          default:
            return fail('mock：未打桩的命令 ' + cmd);
        }
      }
    },
    event: { listen: function () { return Promise.resolve(function () {}); } }
  };
})();
MOCK

# ---------------------------------------------------------------- ② driver：真实操作 DOM
cat >"$WORK/driver.js" <<'DRIVER'
(function () {
  'use strict';
  var lines = [];
  var fails = 0;
  var passes = 0;
  function ok(name, cond, extra) {
    if (cond) { passes += 1; lines.push('PASS ' + name); }
    else { fails += 1; lines.push('FAIL ' + name + (extra ? ' :: ' + extra : '')); }
  }
  function $(id) { return document.getElementById(id); }
  function q(sel) { return document.querySelector(sel); }
  function qa(sel) { return Array.prototype.slice.call(document.querySelectorAll(sel)); }
  function wait(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }
  function waitFor(pred, ms) {
    var started = Date.now();
    return new Promise(function (resolve) {
      (function loop() {
        if (pred()) { resolve(true); return; }
        if (Date.now() - started > ms) { resolve(false); return; }
        setTimeout(loop, 40);
      })();
    });
  }
  function calls() { return window.__CALLS__ || []; }
  function count(cmd) {
    return calls().filter(function (c) { return c.cmd === cmd; }).length;
  }
  function ipcTasksDeletes() {
    return calls().filter(function (c) {
      return c.cmd === 'ipc_call' && c.args && c.args.req
        && String(c.args.req.method) === 'tasks' && String(c.args.req.action) === 'delete';
    });
  }
  function modalOpen() { var m = $('purge-modal'); return !!m && m.hidden === false; }
  function openTaskDialog() {
    var b = q('#tasks-list [data-act="task-delete"]');
    if (!b) { return false; }
    b.click();
    return true;
  }

  (async function () {
    try {
      var ready = await waitFor(function () {
        return qa('#tasks-list .task-card').length >= 1;
      }, 8000);
      ok('打桩后端就绪（任务卡出现）', ready,
        'cards=' + qa('#tasks-list .task-card').length);

      // ---- 1) 任务卡「删除…」→ 弹窗；条目非空、复选框默认未勾 ----
      ok('任务卡有「删除…」按钮', openTaskDialog());
      var shown = await waitFor(modalOpen, 3000);
      ok('任务卡「删除…」打开 #purge-modal', shown);
      ok('#purge-items 非空', $('purge-items').childElementCount > 0,
        'items=' + $('purge-items').childElementCount);
      ok('#purge-opt 默认未勾选', $('purge-opt').checked === false);
      ok('弹窗标题是「删除任务…」',
        String($('purge-title').textContent).indexOf('删除任务') >= 0,
        'title=' + String($('purge-title').textContent));
      ok('optional 项标出「仅勾选后删除」',
        qa('#purge-items .purge-item-mark').length >= 1,
        'marks=' + qa('#purge-items .purge-item-mark').length);
      ok('db_rows / 合计行已显示', $('purge-extra').hidden === false,
        'extra=' + String($('purge-extra').textContent));

      // ---- 2) 取消 → 什么都没删 ----
      $('purge-cancel').click();
      await waitFor(function () { return !modalOpen(); }, 2000);
      ok('取消后弹窗关闭', !modalOpen());
      ok('取消后没有 task_purge', count('task_purge') === 0, 'tp=' + count('task_purge'));
      ok('取消后没有 ipc tasks delete', ipcTasksDeletes().length === 0,
        'del=' + ipcTasksDeletes().length);
      ok('取消流程只发生了 purge_plan', count('purge_plan') >= 1,
        'plan=' + count('purge_plan'));

      // ---- 3) Esc 取消（同样什么都不删） ----
      var beforeTp = count('task_purge');
      var beforeDel = ipcTasksDeletes().length;
      openTaskDialog();
      await waitFor(modalOpen, 2000);
      $('purge-modal').dispatchEvent(new KeyboardEvent('keydown', {
        key: 'Escape', bubbles: true, cancelable: true
      }));
      await waitFor(function () { return !modalOpen(); }, 2000);
      await wait(80);
      ok('Esc 关闭弹窗', !modalOpen());
      ok('Esc 后没有新的删除调用',
        count('task_purge') === beforeTp && ipcTasksDeletes().length === beforeDel,
        'tp=' + count('task_purge') + ' del=' + ipcTasksDeletes().length);

      // ---- 4) 不勾选 + 确认 → 旧路径（只删登记） ----
      openTaskDialog();
      await waitFor(modalOpen, 2000);
      $('purge-ok').click();
      await waitFor(function () { return ipcTasksDeletes().length >= 1; }, 4000);
      ok('不勾选确认 → 走旧路径 ipc tasks delete',
        ipcTasksDeletes().length >= 1, 'del=' + ipcTasksDeletes().length);
      ok('不勾选确认 → 不调用 task_purge', count('task_purge') === 0,
        'tp=' + count('task_purge'));
      var oldCall = ipcTasksDeletes()[0];
      ok('旧路径 args.id = home', !!oldCall && oldCall.args.req.id === 'home',
        JSON.stringify(oldCall && oldCall.args));
      await waitFor(function () { return !modalOpen(); }, 2000);
      ok('旧路径确认后弹窗关闭', !modalOpen());

      // ---- 5) 勾选 + 确认 → task_purge ----
      openTaskDialog();
      await waitFor(modalOpen, 2000);
      $('purge-opt').click();
      ok('勾选后复选框为 true', $('purge-opt').checked === true);
      $('purge-ok').click();
      await waitFor(function () { return count('task_purge') >= 1; }, 4000);
      var tpCall = calls().filter(function (c) { return c.cmd === 'task_purge'; })[0];
      ok('勾选确认 → task_purge 被调用', !!tpCall);
      ok('task_purge args.linkId=default / taskId=home',
        !!tpCall && tpCall.args.linkId === 'default' && tpCall.args.taskId === 'home',
        JSON.stringify(tpCall && tpCall.args));
      await waitFor(function () { return !modalOpen(); }, 2000);
      ok('task_purge 后弹窗关闭', !modalOpen());

      // ---- 6) 连接行「移除」→ 弹窗；勾选确认 → link_delete ----
      q('.rail-item[data-page="settings"]').click();
      q('#sec-tab-connect').click();
      await wait(60);
      var removeBtn = q('#conn-list [data-act="link-remove"]');
      ok('连接行有「移除」按钮', !!removeBtn);
      if (removeBtn) { removeBtn.click(); }
      await waitFor(modalOpen, 3000);
      ok('连接行「移除」打开弹窗（标题含「移除连接」）',
        modalOpen() && String($('purge-title').textContent).indexOf('移除连接') >= 0,
        'title=' + String($('purge-title').textContent));
      ok('移除弹窗条目非空', $('purge-items').childElementCount > 0,
        'items=' + $('purge-items').childElementCount);
      // 6a) 不勾选 → purgeData=false（只删连接 / 任务登记，本地数据保留）
      ok('移除弹窗复选框默认未勾选', $('purge-opt').checked === false);
      $('purge-ok').click();
      await waitFor(function () { return count('link_delete') >= 1; }, 4000);
      var ldKeep = calls().filter(function (c) { return c.cmd === 'link_delete'; })[0];
      ok('不勾选确认 → link_delete args.deleteTasks=true / purgeData=false',
        !!ldKeep && ldKeep.args.deleteTasks === true && ldKeep.args.purgeData === false,
        JSON.stringify(ldKeep && ldKeep.args));
      await waitFor(function () { return !modalOpen(); }, 2000);
      // 6b) 重新打开 + 勾选 → purgeData=true
      removeBtn = q('#conn-list [data-act="link-remove"]');
      if (removeBtn) { removeBtn.click(); }
      await waitFor(modalOpen, 3000);
      $('purge-opt').click();
      $('purge-ok').click();
      await waitFor(function () { return count('link_delete') >= 2; }, 4000);
      var ldCall = calls().filter(function (c) { return c.cmd === 'link_delete'; })[1];
      ok('勾选确认 → link_delete 被调用', !!ldCall);
      ok('link_delete args deleteTasks=true / purgeData=true',
        !!ldCall && ldCall.args.deleteTasks === true && ldCall.args.purgeData === true,
        JSON.stringify(ldCall && ldCall.args));
      await waitFor(function () { return !modalOpen(); }, 2000);

      // ---- 7) 设置→高级→危险区：purge_all（必须勾选才能确认） ----
      q('#sec-tab-advanced').click();
      await wait(60);
      var allBtn = $('btn-purge-all');
      ok('危险区有「删除本机所有 qxync 数据…」按钮', !!allBtn);
      if (allBtn) { allBtn.click(); }
      await waitFor(modalOpen, 3000);
      ok('purge_all 弹窗打开', modalOpen());
      ok('未勾选时确认按钮 disabled', $('purge-ok').disabled === true,
        'disabled=' + $('purge-ok').disabled);
      $('purge-opt').click();
      ok('勾选后确认按钮可点', $('purge-ok').disabled === false,
        'disabled=' + $('purge-ok').disabled);
      $('purge-ok').click();
      await waitFor(function () { return count('purge_all') >= 1; }, 4000);
      ok('purge_all 被调用', count('purge_all') >= 1, 'all=' + count('purge_all'));
      await waitFor(function () { return !modalOpen(); }, 3000);

      // 删完没有连接：应显示空态（不是白屏/报错）
      var emptyOk = await waitFor(function () {
        var n = $('conn-list-empty');
        return !!n && n.hidden === false;
      }, 4000);
      var emptyEl = $('conn-list-empty');
      ok('删完没有连接 → 显示空态',
        emptyOk && emptyEl && emptyEl.getAttribute('data-state') === 'empty',
        'hidden=' + (emptyEl ? emptyEl.hidden : '?') +
        ' state=' + (emptyEl ? emptyEl.getAttribute('data-state') : '?'));
      ok('purge_all 之后界面没有报错（连接列表可重绘）',
        !!$('conn-list') && qa('#conn-list .conn-row').length <= 1,
        'rows=' + qa('#conn-list .conn-row').length);
    } catch (e) {
      fails += 1;
      lines.push('FAIL driver 抛异常 :: ' + (e && e.message ? e.message : e));
    }

    var payload = { pass: passes, fail: fails, lines: lines };
    document.body.setAttribute('data-purge-result', JSON.stringify(payload));
    document.title = fails === 0 ? 'PURGE-PASS' : 'PURGE-FAIL';
    var pre = document.createElement('pre');
    pre.id = 'purge-out';
    pre.textContent = lines.join('\n');
    document.body.appendChild(pre);
  })();
})();
DRIVER

# ---------------------------------------------------------------- ③ 注入 ① ② 并跑无头浏览器
python3 - "$WORK/index.html" "$WORK/mock-tauri.js" "$WORK/driver.js" <<'PY'
import sys
page, mock_path, driver_path = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(page, encoding='utf-8').read()
mock = open(mock_path, encoding='utf-8').read()
driver = open(driver_path, encoding='utf-8').read()
assert '<script src="i18n.js"></script>' in s, 'index.html 里找不到 i18n.js 的 script 标签'
assert '<script src="app.js"></script>' in s, 'index.html 里找不到 app.js 的 script 标签'
s = s.replace('<script src="i18n.js"></script>',
              '<script>\n' + mock + '\n</script>\n<script src="i18n.js"></script>', 1)
s = s.replace('<script src="app.js"></script>',
              '<script src="app.js"></script>\n<script>\n' + driver + '\n</script>', 1)
open(page, 'w', encoding='utf-8').write(s)
PY
rm -f "$WORK/mock-tauri.js" "$WORK/driver.js"

chromium --headless=new --no-sandbox --disable-gpu \
  --user-data-dir="$PROF" --virtual-time-budget=25000 \
  --dump-dom "file://$WORK/index.html" >"$OUT" 2>"$WORK/chromium.err"
if [ ! -s "$OUT" ]; then
  echo "FAIL：chromium --dump-dom 没有输出（见 $WORK/chromium.err）"
  tail -5 "$WORK/chromium.err" 2>/dev/null
  exit 1
fi

# ---------------------------------------------------------------- ④ 取回 driver 的结果
python3 - "$OUT" <<'PY'
import html
import json
import re
import sys

dom = open(sys.argv[1], encoding='utf-8', errors='replace').read()
m = re.search(r'data-purge-result="([^"]*)"', dom)
if not m:
    print('FAIL：页面里没有 data-purge-result（driver 没跑完？）')
    # 给一点上下文，方便排查是 boot 挂了还是断言没跑到
    for line in dom.splitlines():
        if 'purge-out' in line or 'task-card' in line:
            print('DOM> ' + html.unescape(line)[:400])
    sys.exit(1)

payload = json.loads(html.unescape(m.group(1)))
for line in payload.get('lines', []):
    print(line)
print('---')
print('%d passed / %d failed' % (payload.get('pass', 0), payload.get('fail', 0)))
sys.exit(0 if payload.get('fail', 1) == 0 else 1)
PY
RC=$?
echo "（DOM 留档：$OUT；chromium 日志：$WORK/chromium.err）"
exit "$RC"
