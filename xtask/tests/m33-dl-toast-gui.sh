#!/usr/bin/env bash
# m33-dl-toast-gui.sh —— ★ M33：右下角下载进度卡片的无头浏览器集成测试
#
# 验的是什么（用户反馈的原话：「下载大文件……右下角并没有弹出下载进度提示框」）：
#   1. 「下载到本地」超过 3 秒（静默期）后，右下角浮出一张进度卡片；
#   2. 卡片标题 = 文件名，进度行带百分比/速度，进度条宽度在走；
#   3. 卡片容器 fixed 定位在视口右下角（right/bottom 16px）；
#   4. 下载完成 → 卡片挂 done、文案「下载完成」，2.5s 后自动消失；
#   5. 静默期内的快下载不建卡片（零打扰）。
#
# 怎么验的：把 crates/qxync-gui/ui 的资源复制进 .local-run 临时目录，注入
#   * `window.__TAURI__` 打桩（app_info / daemon_status / ipc_call(ls) /
#     ipc_call_with_progress —— 后者用 setTimeout 按 250ms 一档喂进度行，
#     模拟一次 4.5 秒的慢下载）；
#   * 一段 driver 脚本：真实走「文件页 → 行右键 → 下载到本地… → 弹窗确定」
#     的用户路径，然后读 DOM 断言卡片。
# 用 chromium 无头 + --virtual-time-budget 跑（虚拟时间把 3 秒静默期快进掉），
# 跑的是**真 app.js**，不是另写一份逻辑。
#
# 依赖：chromium（没有就跳过，退出码 0 并打印 SKIP）。
# 用法：xtask/tests/m33-dl-toast-gui.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UI="$REPO/crates/qxync-gui/ui"
WORK="$REPO/.local-run/m33-gui-test"
OUT="$WORK/dom.html"
PROF="$WORK/profile"

if ! command -v chromium >/dev/null; then
  echo "SKIP：没有 chromium，跳过 M33 无头集成测试"
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

  var progressCbs = [];   // progress://download 的监听者（ipcForWithProgress 注册）

  function emitProgress(written, total) {
    progressCbs.forEach(function (cb) {
      try { cb({ payload: { written: written, total: total } }); } catch (e) { /* 桩 */ }
    });
  }

  window.__TAURI__ = {
    core: {
      invoke: function (cmd, args) {
        switch (cmd) {
          case 'app_info':
            return Promise.resolve({
              version: 'mock-m33', home: '/home/u', socket: '/tmp/m33.sock',
              config_dir: '/home/u/.config/qxync',
              data_dir: '/home/u/.local/share/qxync',
              state_dir: '/home/u/.local/state/qxync',
              daemon_bin: null, daemon_running: true,
              links: [{ id: 'default', host: 'nas-a.local', port: 9834, https: true,
                        insecure: false, user: 'test1', socket: '/tmp/m33-default.sock' }],
              links_bad: [], ui_assets: {}, initial_tab: 'files'
            });
          case 'links_status':
            return Promise.resolve({
              links: [{
                id: 'default', host: 'nas-a.local', port: 9834, https: true,
                insecure: false, user: 'test1', link_exists: true,
                credential_present: true, otp: false, socket: '/tmp/m33-default.sock',
                daemon: { link_id: 'default', running: true, socket: '/tmp/m33-default.sock',
                          ping: { pong: true, daemon_version: 'mock', pid: 1, uptime_secs: 9 },
                          status: { logged_in: true, mounts: [], cursors: null, hydro: {},
                                    uploads: null, deletes: null, sync: null, cache: null },
                          error: null }
              }],
              bad: []
            });
          case 'daemon_status':
            return Promise.resolve({
              link_id: 'default', running: true, socket: '/tmp/m33-default.sock',
              ping: { pong: true, daemon_version: 'mock', pid: 1, uptime_secs: 9 },
              status: { logged_in: true, mounts: [], cursors: null, hydro: {},
                        uploads: null, deletes: null, sync: null, cache: null },
              error: null
            });
          case 'ipc_call': {
            var req = (args && args.req) || {};
            if (req.method === 'ls') {
              return Promise.resolve({
                ok: true,
                data: {
                  path: '/home',
                  entries: [
                    { filename: 'big.bin', filesize: 4194304, isfolder: false,
                      epochmt: 1790000000, have_child: false, exist: true },
                    { filename: 'tiny.txt', filesize: 12, isfolder: false,
                      epochmt: 1790000001, have_child: false, exist: true }
                  ]
                }
              });
            }
            return Promise.resolve({ ok: false, error: { kind: 'Mock', message: 'mock：未打桩 ' + req.method } });
          }
          case 'ipc_call_with_progress': {
            // ★ 模拟一次 4.5 秒的慢下载：每 250ms 喂一档进度（共 18 档），
            //   3 秒静默期过后前端才会建卡片；喂完发最终响应。
            var total = 4194304;
            var step = total / 18;
            var i = 0;
            return new Promise(function (resolve) {
              (function tick() {
                if (i < 18) {
                  i += 1;
                  emitProgress(Math.round(step * i), total);
                  setTimeout(tick, 250);
                } else {
                  resolve({ ok: true, data: { bytes: total, dest: '/home/u/big.bin' } });
                }
              })();
            });
          }
          default:
            return Promise.resolve({ ok: false, error: { kind: 'Mock', message: 'mock：未打桩的命令 ' + cmd } });
        }
      }
    },
    event: {
      listen: function (name, cb) {
        if (name === 'progress://download') { progressCbs.push(cb); }
        return Promise.resolve(function () {
          var i = progressCbs.indexOf(cb);
          if (i >= 0) { progressCbs.splice(i, 1); }
        });
      }
    }
  };
})();
MOCK

# ---------------------------------------------------------------- ② driver：真实走用户路径
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
  function firstRow() { return document.querySelector('#files-tbody tr'); }

  (async function () {
    try {
      // ---- 0) 文件页就绪（boot → refreshStatus → refreshCurrentPage → ls）----
      var rowsReady = await waitFor(function () { return !!firstRow(); }, 8000);
      ok('文件页渲染出文件行（打桩 ls 生效）', rowsReady);

      // ---- 1) 行右键 → 菜单 → 「下载到本地…」----
      var tr = firstRow();
      tr.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
      var menuShown = await waitFor(function () {
        var m = $('row-menu'); return m && m.hidden === false;
      }, 2000);
      ok('右键菜单弹出', menuShown);
      var getItem = document.querySelector('#row-menu .ctx-item[data-act="get"]');
      ok('菜单里有「下载到本地…」', !!getItem);
      getItem.click();

      // ---- 2) 目的地弹窗：确认默认值 → 确定 ----
      var modalShown = await waitFor(function () {
        var m = $('modal'); return m && m.hidden === false;
      }, 2000);
      ok('下载目的地弹窗弹出', modalShown);
      var guess = String(($('modal-input') || {}).value || '');
      ok('弹窗预填 HOME 下的同名文件', guess.indexOf('/home/u/big.bin') === 0, 'got=' + guess);
      $('modal-ok').click();

      // ---- 3) 静默期后：右下角浮出进度卡片 ----
      var card = await waitFor(function () { return !!document.querySelector('.dl-card'); }, 8000);
      ok('超过 3 秒后浮出下载进度卡片', card);
      if (card) {
        var title = document.querySelector('.dl-card .dl-title');
        ok('卡片标题 = 文件名', title && title.textContent === 'big.bin',
          'got=' + (title ? title.textContent : '(null)'));
        var line = document.querySelector('.dl-card .dl-line');
        ok('进度行带百分比', line && line.textContent.indexOf('%') >= 0,
          'got=' + (line ? line.textContent : '(null)'));
        ok('进度行带速度', line && line.textContent.indexOf('/s') >= 0);
        var fill = document.querySelector('.dl-card .dl-fill');
        var w = fill ? parseFloat(fill.style.width) : 0;
        ok('进度条宽度在走（>0%）', w > 0, 'width=' + (fill ? fill.style.width : '(null)'));

        // ---- 4) 「右下角」：容器 fixed + 贴视口右下 ----
        var host = $('dl-toasts');
        var cs = getComputedStyle(host);
        var rect = host.getBoundingClientRect();
        var vw = window.innerWidth, vh = window.innerHeight;
        ok('容器 fixed 定位', cs.position === 'fixed', 'position=' + cs.position);
        ok('容器贴视口右下角', rect.right > vw * 0.7 && rect.bottom > vh * 0.7,
          'rect=' + Math.round(rect.right) + ',' + Math.round(rect.bottom) + ' vw=' + vw + ' vh=' + vh);
        ok('容器不挡下层交互（pointer-events:none）', cs.pointerEvents === 'none',
          'pointerEvents=' + cs.pointerEvents);
      }

      // ---- 5) 下载完成 → done 态 → 自动消失 ----
      var done = await waitFor(function () {
        return !!document.querySelector('.dl-card.done');
      }, 8000);
      ok('完成后卡片挂 done 类', done);
      if (done) {
        var dl = document.querySelector('.dl-card.done .dl-line');
        ok('完成文案带「下载完成」', dl && dl.textContent.indexOf('下载完成') >= 0,
          'got=' + (dl ? dl.textContent : '(null)'));
      }
      var gone = await waitFor(function () {
        return !document.querySelector('.dl-card');
      }, 8000);
      ok('2.5 秒后卡片自动移除', gone);
      var hostAfter = $('dl-toasts');
      ok('最后一张移除后容器隐藏', hostAfter && hostAfter.hidden === true);
    } catch (e) {
      fails += 1;
      lines.push('FAIL driver 抛异常 :: ' + (e && e.message ? e.message : e));
    }

    document.title = fails === 0 ? 'M33-PASS' : 'M33-FAIL';
    var pre = document.createElement('pre');
    pre.id = 'm33-out';
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
  --user-data-dir="$PROF" --virtual-time-budget=30000 \
  --dump-dom "file://$WORK/index.html" >"$OUT" 2>"$WORK/chromium.err"
if [ ! -s "$OUT" ]; then
  echo "FAIL：chromium 没产出 DOM（看 $WORK/chromium.err）"
  cat "$WORK/chromium.err" | head -20
  exit 1
fi

# ---------------------------------------------------------------- ④ 从 DOM 里取 driver 的结论
python3 - "$OUT" <<'PY'
import re, sys
dom = open(sys.argv[1], encoding='utf-8', errors='replace').read()
m = re.search(r'<pre id="m33-out">(.*?)</pre>', dom, re.S)
if not m:
    print('FAIL：DOM 里没有 #m33-out（driver 可能没跑完）')
    sys.exit(1)
import html
text = html.unescape(m.group(1))
print(text)
fails = len(re.findall(r'^FAIL ', text, re.M))
passes = len(re.findall(r'^PASS ', text, re.M))
print(f'\n{passes} passed, {fails} failed')
sys.exit(1 if fails else 0)
PY
