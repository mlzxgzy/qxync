#!/usr/bin/env node
/* ============================================================================
 * ★ M28：托盘状态聚合 + 网络错误识别 —— 源文本级单测（node 直接跑，不依赖浏览器）
 *
 *   node xtask/tests/m28-tray-status.test.mjs
 *
 * 被测对象是 crates/qxync-gui/ui/app.js 里的两段**内联**逻辑（不是 UMD 模块，没法
 * require），所以这里从源文本里把函数抠出来在受控环境里执行 —— 与 M26 下载进度的
 * 测试同一套路：抠真实函数，不另写一份逻辑（另写的会漂移）。
 *
 * 覆盖：
 *   1. isNetworkErrorEntry：新 daemon 的 warn/kind=network、老 daemon 的文案兜底、
 *      普通错误绝不误判（误判 = 普通错误被静默吞掉）。
 *   2. updateTrayStatus 的聚合判据：红（探活失败）> 黄（非网络错误/熔断）> 绿；
 *      没有 daemon 在跑时不下发；老 daemon 缺 connectivity 按「正常」处理。
 *   3. checkConflictsForNotify 只认「产生了冲突」的条目（队列清理不算新冲突）。
 * ==========================================================================*/
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../../crates/qxync-gui/ui/app.js', import.meta.url), 'utf8');

/** 从 app.js 源文本里抠一个函数体（function name(...) { ... }），在沙箱里求值。 */
function extractFunction(name) {
  const marker = 'function ' + name + '(';
  const start = src.indexOf(marker);
  assert.ok(start >= 0, `app.js 里找不到函数 ${name}`);
  let depth = 0;
  let i = src.indexOf('{', start);
  const bodyStart = i;
  for (; i < src.length; i += 1) {
    if (src[i] === '{') depth += 1;
    else if (src[i] === '}') {
      depth -= 1;
      if (depth === 0) break;
    }
  }
  assert.ok(depth === 0, `函数 ${name} 的花括号不平衡`);
  return src.slice(start, i + 1);
}

let passed = 0;
let failed = 0;
function t(name, fn) {
  try {
    fn();
    passed += 1;
    console.log('PASS ' + name);
  } catch (e) {
    failed += 1;
    console.error('FAIL ' + name + ' :: ' + e.message);
  }
}

// ---------------------------------------------------------------- ① isNetworkErrorEntry
const isNetworkErrorEntry = new Function(
  'isObj', 'str',
  extractFunction('isNetworkErrorEntry') + '\nreturn isNetworkErrorEntry;'
)(v => v !== null && typeof v === 'object', v => String(v ?? ''));

t('新 daemon：status=warn → 网络错误（不弹窗/不进错误列表）', () => {
  assert.equal(isNetworkErrorEntry({ status: 'warn', kind: 'network', detail: '网络不可用：…' }), true);
});
t('新 daemon：kind=network（哪怕 status 拼错）→ 网络错误', () => {
  assert.equal(isNetworkErrorEntry({ status: 'error', kind: 'network', detail: 'x' }), true);
});
t('老 daemon：文案以「网络不可用」开头 → 兜底识别', () => {
  assert.equal(isNetworkErrorEntry({ status: 'error', kind: 'sync', detail: '网络不可用：探测 NAS 失败，本轮轮询中止' }), true);
});
t('老 daemon：文案以「网络错误」开头 → 兜底识别', () => {
  assert.equal(isNetworkErrorEntry({ status: 'error', kind: 'sync', detail: '网络错误: get_list …: error sending request' }), true);
});
t('普通错误绝不误判（误判 = 静默吞掉真错误）', () => {
  assert.equal(isNetworkErrorEntry({ status: 'error', kind: 'sync', detail: 'qbox_get_max_log 失败: status=3' }), false);
  assert.equal(isNetworkErrorEntry({ status: 'error', kind: 'upload', detail: '上传失败：连接重置' }), false);
  assert.equal(isNetworkErrorEntry({ status: 'blocked', kind: 'dehydrate', detail: '被 pin 挡下' }), false);
});
t('null / 非对象 → false（不炸）', () => {
  assert.equal(isNetworkErrorEntry(null), false);
  assert.equal(isNetworkErrorEntry(undefined), false);
  assert.equal(isNetworkErrorEntry('网络不可用'), false);
});

// ---------------------------------------------------------------- ② 托盘聚合判据
// updateTrayStatus 依赖一串 app.js 内部助手；这里按真实判据重建等价桩，
// 并把**源文本里的判据顺序**断言住（红 > 黄 > 绿、无 daemon 不下发）。
t('源文本：红判据是 connectivity.known && !online（探活结论，不是 last_error）', () => {
  assert.match(src, /conn\.known === true && conn\.online === false/, '红点必须读探活结论');
});
t('源文本：离线时 last_error 不再计黄（同一件事不重复表达）', () => {
  const seg = src.slice(src.indexOf('function updateTrayStatus'), src.indexOf('function renderScopeHints'));
  assert.match(seg, /已由红点表达/, '离线分支的注释要在');
  // offline.push 之后必须 continue，不能落进 warning（正则容忍换行/空白差异）
  assert.match(seg, /offline\.push\([\s\S]*?\);\s*continue;/, 'offline 命中后要 continue，不重复计黄');
});
t('源文本：没有任何 daemon 在跑时不下发（不误报）', () => {
  const seg = src.slice(src.indexOf('function updateTrayStatus'), src.indexOf('function renderScopeHints'));
  assert.match(seg, /if \(!targets\.length\) \{ return; \}/, '无运行 daemon 必须直接 return');
});
t('源文本：三态下发顺序 offline > warning > ok', () => {
  const seg = src.slice(src.indexOf('function updateTrayStatus'), src.indexOf('function renderScopeHints'));
  const iOff = seg.indexOf("status: 'offline'");
  const iWarn = seg.indexOf("status: 'warning'");
  const iOk = seg.indexOf("status: 'ok'");
  assert.ok(iOff >= 0 && iWarn >= 0 && iOk >= 0, '三个状态都要下发');
  assert.ok(iOff < iWarn && iWarn < iOk, '判据顺序必须是 红 > 黄 > 绿');
});
t('源文本：老 daemon 缺 connectivity 按正常处理（缺字段 ≠ 离线）', () => {
  const seg = src.slice(src.indexOf('function updateTrayStatus'), src.indexOf('function renderScopeHints'));
  assert.match(seg, /conn && conn\.known === true/, '判据必须带 known 检查（M20 同一原则）');
});

// ---------------------------------------------------------------- ③ 冲突通知只认「新冲突」
t('源文本：冲突通知只认 kind=conflict 且 status=ok 的条目', () => {
  const seg = src.slice(src.indexOf('function checkConflictsForNotify'), src.indexOf('// ============================================================ 页面切换'));
  assert.match(seg, /str\(e\.kind\) !== 'conflict' \|\| str\(e\.status\) !== 'ok'/, 'kind/status 双重过滤');
  assert.match(seg, /indexOf\('双方都改'\) !== 0/, '只认「双方都改」开头的新冲突，队列清理不弹');
});
t('源文本：错误通知跳过网络条目（老 daemon 兜底）', () => {
  const seg = src.slice(src.indexOf('function checkErrorsForNotify'), src.indexOf('function isNetworkErrorEntry'));
  assert.match(seg, /isNetworkErrorEntry\(e\)/, '错误通知必须先过网络识别');
});
t('源文本：冲突通知尊重「桌面通知」设置', () => {
  const seg = src.slice(src.indexOf('function checkConflictsForNotify'), seg_end());
  function seg_end() { return src.indexOf('// ============================================================ 页面切换'); }
  assert.match(seg, /desktop_notifications === false/, '关掉通知后不能弹');
});

console.log('---');
console.log(`${passed} passed / ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
