#!/usr/bin/env bash
# gui-i18n.sh —— ★ M50：GUI 英文界面 / 语言切换的无头验收
#
# 验的是什么（用户可感知的两件事）：
#   1. 系统语言是英文时，整个界面**真的**是英文：<html lang="en">、标题、
#      所有 data-i18n 文案与属性、以及 app.js 动态渲染的文案都没有中文；
#      （诊断日志 logErr/logInfo 仍中文是本轮已知边界，检查时排除 #logbox）
#   2. 系统语言是中文时回落 zh-CN：<html lang="zh-CN">、标题/选项是中文；
#   3. 「设置 → 个人 → 语言」的选项里真的有 English（不是只有跟随系统/中文）；
#   4. i18n 表自洽：QXNYC_I18N.missingInDOM() 为空（没有拼错/漏译的 key）。
#
# 怎么验的：把 crates/qxync-gui/ui 的资源复制到 .local-run 的临时目录，注入
#   * `window.__TAURI__` 打桩（所有 invoke 返回 mock 错误，不连 daemon）；
#   * navigator.languages 打桩（模拟系统语言 en-US / zh-CN）；
#   * 一段 probe 脚本（在 load 后把 QXNYC_I18N 的 locale / missingInDOM / stats 写进 DOM）；
# 然后用 chromium 无头跑 `--dump-dom` —— 跑的是**真 app.js + 真 i18n.js**。
#
# 依赖：chromium（没有就 SKIP，退出码 0）、python3、node。
# 用法：xtask/tests/gui-i18n.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UI="$REPO/crates/qxync-gui/ui"
WORK="$REPO/.local-run/m50-gui-i18n"
FAIL=0

if ! command -v chromium >/dev/null; then
  echo "SKIP：没有 chromium，跳过 M50 无头 i18n 测试"
  exit 0
fi
for f in index.html app.js i18n.js style.css task-defaults.js; do
  [ -f "$UI/$f" ] || { echo "FAIL：缺少 UI 资源 $UI/$f"; exit 1; }
done
if ! command -v node >/dev/null; then
  echo "FAIL：缺少 node（用于 --check）"; exit 1
fi
node --check "$UI/app.js" || { echo "FAIL：app.js 语法错误"; exit 1; }
node --check "$UI/i18n.js" || { echo "FAIL：i18n.js 语法错误"; exit 1; }

rm -rf "$WORK"

# ---------------------------------------------------------------- 单个语言场景
# $1 = en|zh；$2 = 期望的 document.lang；$3 = 期望标题里的片段
run_case() {
  local mode="$1" want_lang="$2" want_title="$3"
  local dir="$WORK/$mode"
  mkdir -p "$dir/profile"
  cp "$UI/index.html" "$UI/app.js" "$UI/i18n.js" "$UI/style.css" "$UI/task-defaults.js" "$dir/"

  if [ "$mode" = "en" ]; then
    local langs="['en-US', 'en']" lang="'en-US'"
  else
    local langs="['zh-CN', 'zh']" lang="'zh-CN'"
  fi
  cat >"$dir/mock-tauri.js" <<MOCK
(function () {
  'use strict';
  Object.defineProperty(navigator, 'languages', { configurable: true, get: function () { return $langs; } });
  Object.defineProperty(navigator, 'language', { configurable: true, get: function () { return $lang; } });
  try { window.localStorage.clear(); } catch (e) {}
  window.__TAURI__ = { core: { invoke: function () { return Promise.resolve({ ok: false, error: 'mock-no-daemon', data: null }); } } };
})();
MOCK
  # probe：load 后把 i18n 自检结果写到 DOM（chromium --dump-dom 才读得到）
  cat >"$dir/probe.js" <<'PROBE'
(function () {
  'use strict';
  window.addEventListener('load', function () {
    setTimeout(function () {
      var pre = document.createElement('pre');
      pre.id = 'i18n-probe';
      var I = window.QXNYC_I18N || null;
      pre.textContent = JSON.stringify(I ? {
        locale: I.locale(),
        lang: document.documentElement.lang,
        missing_dom: I.missingInDOM(),
        zh_keys: I.stats().zh_keys,
        en_keys: I.stats().en_keys
      } : { missing: 'QXNYC_I18N' });
      document.body.appendChild(pre);
    }, 300);
  });
})();
PROBE
  python3 - "$dir/index.html" <<'PY'
import sys
from pathlib import Path
p = Path(sys.argv[1])
s = p.read_text(encoding='utf-8')
i = s.find('<script')
s = s[:i] + '<script src="mock-tauri.js"></script>\n  ' + s[i:]
s = s.replace('</body>', '<script src="probe.js"></script></body>')
p.write_text(s, encoding='utf-8')
PY

  timeout 120 chromium --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --user-data-dir="$dir/profile" --virtual-time-budget=6000 \
    --dump-dom "file://$dir/index.html" >"$dir/dom.html" 2>"$dir/err.log" || true
  [ -s "$dir/dom.html" ] || { echo "FAIL[$mode]：chromium 没有输出 DOM（见 $dir/err.log）"; FAIL=1; return; }

  python3 - "$dir/dom.html" "$mode" "$want_lang" "$want_title" <<'PY' || FAIL=1
import json
import re
import sys

dom, mode, want_lang, want_title = sys.argv[1:5]
src = open(dom, encoding='utf-8').read()
errs = []

def ok(cond, msg):
    if not cond:
        errs.append(msg)

# ---- probe（i18n 运行时自检）
m = re.search(r'<pre id="i18n-probe">(.*?)</pre>', src, re.S)
ok(m is not None, '缺少 #i18n-probe（probe.js 没跑起来）')
if m:
    probe = json.loads(m.group(1))
    ok(probe.get('lang') == want_lang,
       'document.lang=%r，期望 %r' % (probe.get('lang'), want_lang))
    ok(probe.get('locale') == want_lang,
       'QXNYC_I18N.locale()=%r，期望 %r' % (probe.get('locale'), want_lang))
    ok(not probe.get('missing_dom'),
       'DOM 里有表里没有的 key：%r' % probe.get('missing_dom'))
    ok(probe.get('zh_keys', 0) >= 500, 'zh 键数异常：%r' % probe.get('zh_keys'))
    ok(probe.get('en_keys') == probe.get('zh_keys'),
       'EN 未与 ZH 同键数：zh=%r en=%r' % (probe.get('zh_keys'), probe.get('en_keys')))

# ---- 标题
mt = re.search(r'<title[^>]*>(.*?)</title>', src, re.S)
title = mt.group(1) if mt else ''
ok(want_title in title, '标题=%r，期望包含 %r' % (title, want_title))

# ---- 语言下拉里必须有 English 选项（否则设置里的「语言」还是摆设）
sel = re.search(r'<select id="s-lang">.*?</select>', src, re.S)
ok(sel is not None, '找不到 #s-lang')
if sel:
    ok('value="en"' in sel.group(0), '#s-lang 里没有 value="en" 选项')
    ok('data-i18n="lang.follow_system"' in sel.group(0),
       '#s-lang 的「跟随系统」没挂 data-i18n')

# ---- 英文模式：去掉脚本/样式/注释/#logbox（诊断日志本轮仍中文）后不许有中文
if mode == 'en':
    body = re.sub(r'<script\b.*?</script>', '', src, flags=re.S | re.I)
    body = re.sub(r'<style\b.*?</style>', '', body, flags=re.S | re.I)
    body = re.sub(r'<!--.*?-->', '', body, flags=re.S)
    i = body.find('id="logbox"')
    if i >= 0:
        j = body.rfind('<', 0, i)
        tag = re.match(r'<([a-zA-Z][\w-]*)', body[j:]).group(1)
        depth = 0
        for mt2 in re.finditer(r'<%s\b[^>]*>|</%s>' % (tag, tag), body):
            if mt2.start() < j:
                continue
            if mt2.group(0).startswith('</'):
                depth -= 1
                if depth == 0:
                    body = body[:j] + body[mt2.end():]
                    break
            else:
                depth += 1
    text = re.sub(r'<[^>]+>', ' ', body)
    cjk = re.findall(r'[\u4e00-\u9fff]+', text)
    ok(not cjk, '英文界面仍有中文文本（%d 处）：%r' % (len(cjk), cjk[:8]))
    # ★ M50：全角标点也只属于中文排版（英文界面该用 ASCII 的 , ; : 与圆括号）
    punct = re.findall(r'[：，。；！？（）“”‘’、　]', text)
    ok(not punct, '英文界面仍有全角标点（%d 处）：%r' % (len(punct), punct[:8]))
    bad_attr = re.findall(
        r'\s(?:title|placeholder|aria-label|alt)="([^"]*[\u4e00-\u9fff][^"]*)"', body)
    ok(not bad_attr, '英文界面仍有中文属性：%r' % bad_attr[:5])
    ok(not re.search(r'[\u4e00-\u9fff]', title), '英文界面的 <title> 仍是中文：%r' % title)

if errs:
    print('FAIL[%s]' % mode)
    for e in errs:
        print('  - ' + e)
    sys.exit(1)
print('PASS[%s]  lang=%s  title=%r' % (mode, want_lang, title))
PY
}

run_case en en "QNAP on-demand sync"
run_case zh zh-CN "按需同步"

echo
if [ "$FAIL" -ne 0 ]; then
  echo "M50 GUI i18n 无头测试：FAIL（DOM 见 $WORK）"
  exit 1
fi
echo "M50 GUI i18n 无头测试：PASS（en/zh 两种语言各一次；诊断日志 #logbox 不在英文检查范围）"
