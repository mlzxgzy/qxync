#!/usr/bin/env bash
# gui-task-defaults.sh —— ★ M18：新任务表单的默认 id / 本地文件夹 / 水合缓存目录
#
# 验的是什么（用户反馈的原话：不同任务的任务 id、默认文件夹地址、水合目录都默认一样）：
#   1. 打开「＋ 添加配对文件夹」时，id 由 **NAS 文件夹名**派生，且避开已有任务（home → home-2）；
#   2. 本地文件夹 = <HOME>/qxync/<连接 id>/<任务 id>；
#   3. 水合缓存 = <data_dir>/cache/tasks/<连接 id>/<任务 id>；
#   4. 换 NAS 文件夹 → id 与两条路径跟着变；
#   5. 换 NAS 连接 → 路径里的连接段跟着变；
#   6. 手改 id → 两条路径跟着变；手改路径 → 之后换 NAS 文件夹不再覆盖；
#   7. 「重置为默认路径」能恢复自动派生；
#   8. 提交一个已存在的 id → 被拦下（不会静默覆盖另一条任务）。
#
# 怎么验的：把 cranks/qxync-gui/ui 的资源复制进 .local-run 的临时目录，注入
#   * `window.__TAURI__` 打桩（app_info / links_status / tasks_config）；
#   * 一段 driver 脚本：真实点按钮、改 select、派发 input/change、读 DOM。
# 然后用 chromium 无头跑 `--dump-dom` 取结果 —— 跑的是**真 app.js**，不是另写一份逻辑。
#
# 依赖：chromium（没有就跳过，退出码 0 并打印 SKIP）。
# 用法：xtask/tests/gui-task-defaults.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UI="$REPO/crates/qxync-gui/ui"
WORK="$REPO/.local-run/m18-gui-test"
OUT="$WORK/dom.html"
PROF="$WORK/profile"

if ! command -v chromium >/dev/null; then
  echo "SKIP：没有 chromium，跳过 M18 无头集成测试"
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

  // 打桩的「磁盘上的任务登记」：default 连接已经有一条 /home 的任务（id=home）。
  // 新任务表单打开时，默认 id 必须避开它 → home-2。
  var TASKS = {
    default: [{
      task: {
        id: 'home', link_id: 'default', name: 'home', enabled: true,
        mountpoint: '/home/u/qxync/default/home', cache_dir: null, root: '/home',
        read_write: true, cache_mode: 'pagecache', conflict: 'rename_local',
        direction: '2way', space_saving: false, smart_delete: false,
        selective: [], exclude: [], auto_unmount: true
      },
      mounted: false,
      last_error: 'mock：连接 daemon 未运行（磁盘上的登记）'
    }],
    nas2: []
  };

  function linkInfo(id, host, user) {
    return {
      id: id, host: host, port: 9834, https: true, insecure: false, user: user,
      link_exists: true, credential_present: true, otp: false,
      socket: '/tmp/m18-' + id + '.sock',
      daemon: {
        link_id: id, running: false, socket: '/tmp/m18-' + id + '.sock',
        ping: null, status: null, error: null
      }
    };
  }

  window.__TAURI__ = {
    core: {
      invoke: function (cmd, args) {
        switch (cmd) {
          case 'app_info':
            return Promise.resolve({
              version: 'mock-m18', home: '/home/u', socket: '/tmp/m18.sock',
              config_dir: '/home/u/.config/qxync',
              data_dir: '/home/u/.local/share/qxync',
              state_dir: '/home/u/.local/state/qxync',
              daemon_bin: null, daemon_running: false,
              links: [
                { id: 'default', host: 'nas-a.local', port: 9834, https: true,
                  insecure: false, user: 'test1', socket: '/tmp/m18-default.sock' },
                { id: 'nas2', host: 'nas-b.local', port: 9834, https: true,
                  insecure: false, user: 'test2', socket: '/tmp/m18-nas2.sock' }
              ],
              links_bad: [], ui_assets: {}, initial_tab: 'tasks'
            });
          case 'links_status':
            return Promise.resolve({
              links: [linkInfo('default', 'nas-a.local', 'test1'),
                      linkInfo('nas2', 'nas-b.local', 'test2')],
              bad: []
            });
          case 'tasks_config':
            return Promise.resolve({
              tasks: TASKS[String(args && args.linkId)] || [],
              bad_files: [], empty: false, note: ''
            });
          default:
            return Promise.resolve({
              ok: false,
              error: { kind: 'Mock', message: 'mock：未打桩的命令 ' + cmd }
            });
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
  function val(id) { var n = $(id); return n ? String(n.value) : ''; }
  function fire(node, type) { node.dispatchEvent(new Event(type, { bubbles: true })); }
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
  /** 往 NAS 文件夹下拉里塞一个候选并选中它（候选本来自 roots，打桩环境直接塞）。 */
  function pickNasFolder(path) {
    var sel = $('t-nas-folder');
    var o = document.createElement('option');
    o.value = path;
    o.textContent = path;
    sel.appendChild(o);
    sel.value = path;
    fire(sel, 'change');
  }

  (async function () {
    try {
      var linksReady = await waitFor(function () {
        return document.querySelectorAll('#conn-select option').length >= 2;
      }, 8000);
      ok('打桩后端就绪（连接下拉 2 项）', linksReady,
        'options=' + document.querySelectorAll('#conn-select option').length);

      var tasksReady = await waitFor(function () {
        return document.querySelectorAll('#tasks-list .task-card').length >= 1;
      }, 8000);
      ok('已有任务登记已灌进任务页（default/home）', tasksReady,
        'cards=' + document.querySelectorAll('#tasks-list .task-card').length);

      // ---- 1) 打开新任务表单：id 必须避开已有 home ----
      $('btn-tasks-add').click();
      await waitFor(function () { return $(('task-form-card')).hidden === false; }, 2000);
      ok('新任务 id 唯一化：已有 home → home-2', val('t-id') === 'home-2', 'got=' + val('t-id'));
      ok('默认本地文件夹 = <HOME>/qxync/<连接>/<id>',
        val('t-mountpoint') === '/home/u/qxync/default/home-2', 'got=' + val('t-mountpoint'));
      ok('默认水合缓存 = <data>/cache/tasks/<连接>/<id>',
        val('t-cache-dir') === '/home/u/.local/share/qxync/cache/tasks/default/home-2',
        'got=' + val('t-cache-dir'));

      // ---- 2) 换 NAS 文件夹 → id 与两条路径都跟着变 ----
      pickNasFolder('/home/Public');
      await new Promise(function (r) { setTimeout(r, 30); });
      ok('换 NAS 文件夹 → id 取文件夹名（public）', val('t-id') === 'public', 'got=' + val('t-id'));
      ok('换 NAS 文件夹 → 本地文件夹跟着变',
        val('t-mountpoint') === '/home/u/qxync/default/public', 'got=' + val('t-mountpoint'));
      ok('换 NAS 文件夹 → 水合缓存跟着变',
        val('t-cache-dir') === '/home/u/.local/share/qxync/cache/tasks/default/public',
        'got=' + val('t-cache-dir'));

      // ---- 3) 换 NAS 连接 → 路径的连接段跟着变 ----
      var lsel = $('t-link');
      lsel.value = 'nas2';
      fire(lsel, 'change');
      await new Promise(function (r) { setTimeout(r, 30); });
      ok('换 NAS 连接 → 本地文件夹连接段变 nas2',
        val('t-mountpoint') === '/home/u/qxync/nas2/public', 'got=' + val('t-mountpoint'));
      ok('换 NAS 连接 → 水合缓存连接段变 nas2',
        val('t-cache-dir') === '/home/u/.local/share/qxync/cache/tasks/nas2/public',
        'got=' + val('t-cache-dir'));

      // ---- 4) 手改 id → 两条路径跟着走 ----
      var idEl = $('t-id');
      idEl.value = 'myid';
      fire(idEl, 'input');
      await new Promise(function (r) { setTimeout(r, 20); });
      ok('手改 id → 本地文件夹跟随', val('t-mountpoint') === '/home/u/qxync/nas2/myid',
        'got=' + val('t-mountpoint'));
      ok('手改 id → 水合缓存跟随',
        val('t-cache-dir') === '/home/u/.local/share/qxync/cache/tasks/nas2/myid',
        'got=' + val('t-cache-dir'));

      // ---- 5) 手改路径后，换 NAS 文件夹不再覆盖它 ----
      var mpEl = $('t-mountpoint');
      mpEl.value = '/home/u/custom-mnt';
      fire(mpEl, 'input');
      pickNasFolder('/home/Public');
      await new Promise(function (r) { setTimeout(r, 20); });
      ok('手改过的本地文件夹不被自动覆盖', val('t-mountpoint') === '/home/u/custom-mnt',
        'got=' + val('t-mountpoint'));

      // ---- 6) 「重置为默认路径」恢复自动派生 ----
      $('btn-task-defaults').click();
      await new Promise(function (r) { setTimeout(r, 20); });
      ok('重置为默认路径（id 回到 public / 路径回到连接 nas2）',
        val('t-id') === 'public' && val('t-mountpoint') === '/home/u/qxync/nas2/public'
        && val('t-cache-dir') === '/home/u/.local/share/qxync/cache/tasks/nas2/public',
        'id=' + val('t-id') + ' mp=' + val('t-mountpoint') + ' cache=' + val('t-cache-dir'));

      // ---- 7) 提交已有 id：拦下，不静默覆盖 ----
      idEl.value = 'home';
      fire(idEl, 'input');
      $('form-task').dispatchEvent(new Event('submit', { cancelable: true }));
      await new Promise(function (r) { setTimeout(r, 50); });
      var resultText = String(($('task-result') || {}).textContent || '');
      ok('提交已存在的 id 被拦下（不覆盖另一条任务）',
        resultText.indexOf('已经被另一条任务用了') >= 0, 'task-result=' + resultText.slice(0, 160));

      // ---- 8) 非法 id 提交：也拦下 ----
      idEl.value = 'a/b';
      fire(idEl, 'input');
      $('form-task').dispatchEvent(new Event('submit', { cancelable: true }));
      await new Promise(function (r) { setTimeout(r, 50); });
      resultText = String(($('task-result') || {}).textContent || '');
      ok('非法 id（含 /）被拦下', resultText.indexOf('任务 id 不合法') >= 0,
        'task-result=' + resultText.slice(0, 160));
    } catch (e) {
      fails += 1;
      lines.push('FAIL driver 抛异常 :: ' + (e && e.message ? e.message : e));
    }

    var payload = { pass: passes, fail: fails, lines: lines };
    document.body.setAttribute('data-m18-result', JSON.stringify(payload));
    document.title = fails === 0 ? 'M18-PASS' : 'M18-FAIL';
    var pre = document.createElement('pre');
    pre.id = 'm18-out';
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
  --user-data-dir="$PROF" --virtual-time-budget=20000 \
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
m = re.search(r'data-m18-result="([^"]*)"', dom)
if not m:
    print('FAIL：页面里没有 data-m18-result（driver 没跑完？）')
    # 给一点上下文，方便排查是 boot 挂了还是断言没跑到
    for line in dom.splitlines():
        if 'm18-out' in line or 'task-form-card' in line:
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
