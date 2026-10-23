#!/usr/bin/env node
/* ============================================================================
 * ★ M33：右下角下载进度卡片 —— 源文本级单测（node 直接跑，不依赖浏览器）
 *
 *   node xtask/tests/m33-dl-toast.test.mjs
 *
 * 被测对象是 crates/qxync-gui/ui/app.js 里的 `makeDownloadProgress` / `removeDlCard`
 * （内联函数，没法 require），从源文本里抠出来在受控环境里执行 —— 与 M26/M28 的
 * 测试同一套路：抠真实函数、注入假时钟/假 DOM，不另写一份逻辑（另写的会漂移）。
 *
 * 覆盖（M26 的时序规则原样保留 + M33 的卡片行为）：
 *   1. 3 秒静默期：静默期内 update 不建卡片（快下载零打扰）。
 *   2. 静默期后第一次 update 建卡片并画进度（百分比/速度/剩余时间）。
 *   3. 10 Hz 节流：100ms 内的第二次画被跳过。
 *   4. 速度滑窗：只统计最近 3 秒的采样。
 *   5. 未知总量（total=0）：进度条挂 indet 类，行文案不带百分比。
 *   6. finish(true)：卡片挂 done 类、文案「下载完成」、2500ms 后自动移除、点击立即关。
 *   7. finish(false)：挂 err 类、带错误文案、6000ms 后移除。
 *   8. 快下载（卡片没建过）finish：什么都不做、不炸。
 *   9. finish 幂等 + finish 后 update 被忽略。
 *  10. removeDlCard：最后一张移除后容器 hidden；空句柄不炸。
 * ==========================================================================*/
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../../crates/qxync-gui/ui/app.js', import.meta.url), 'utf8');

/** 从 app.js 源文本里抠一个函数（function name(...) { ... }），花括号配平。 */
function extractFunction(name) {
  const marker = 'function ' + name + '(';
  const start = src.indexOf(marker);
  assert.ok(start >= 0, `app.js 里找不到函数 ${name}`);
  let depth = 0;
  let i = src.indexOf('{', start);
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

// ---------------------------------------------------------------- 假环境
let clock = 0;                 // 可拨的假时钟：makeDownloadProgress 只用 Date.now()
const FakeDate = { now: () => clock };

/** 一张假卡片句柄 {card, fill, line}：接口与真 DOM 对齐，class 全记进一个 Set。 */
function fakeCard(name) {
  const classes = new Set();
  const mk = (extra) => ({
    classList: {
      add: (c) => classes.add(extra + c),
      remove: (c) => classes.delete(extra + c),
    },
    _classes: classes,
    ...extra ? {} : { onclick: null, title: '' },
    style: {},
  });
  return {
    name,
    card: mk(''),
    fill: mk('f:'),
    line: { textContent: '', title: '' },
  };
}

let built = [];      // buildDlCard 建出的句柄
let removed = [];    // removeDlCard 移除的句柄
let timeouts = [];   // setTimeout 安排 {fn, ms}

const deps = {
  Date: FakeDate,
  buildDlCard: (name) => { const c = fakeCard(name); built.push(c); return c; },
  removeDlCard: (h) => { removed.push(h); },
  setTimeout: (fn, ms) => { timeouts.push({ fn, ms }); return timeouts.length; },
  humanSize: (n) => Math.round(n) + 'B',
  fmtEta: (s) => Math.round(s) + 's',
  str: (v) => String(v ?? ''),
  T: (k) => ({ 'dl.done': '下载完成', 'dl.fail': '下载失败', 'dl.click_close': '点击关闭' }[k] || k),
};

const makeDownloadProgress = new Function(
  ...Object.keys(deps),
  extractFunction('makeDownloadProgress') + '\nreturn makeDownloadProgress;'
)(...Object.values(deps));

// removeDlCard 无外部依赖，直接求值
const removeDlCard = new Function(
  extractFunction('removeDlCard') + '\nreturn removeDlCard;'
)();

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
function reset() { clock = 0; built = []; removed = []; timeouts = []; }

// ---------------------------------------------------------------- ① 3 秒静默期
t('静默期内 update 不建卡片；快下载 finish 也不补建（零打扰）', () => {
  reset();
  const p = makeDownloadProgress('a.bin');
  clock = 500;  p.update(1000, 10000);
  clock = 1500; p.update(3000, 10000);
  clock = 2900; p.update(9000, 10000);
  assert.equal(built.length, 0, '3 秒内不该建卡片');
  p.finish(true);
  assert.equal(built.length, 0, '快下载 finish 也不补建卡片');
  assert.equal(timeouts.length, 0, '没有卡片就没有自动移除');
});

// ---------------------------------------------------------------- ② 静默期后建卡
t('静默期后第一次 update 建卡片并画进度', () => {
  reset();
  const p = makeDownloadProgress('big.bin');
  clock = 1000;  p.update(1000, 10000);   // 静默期内的采样也进滑窗（算速度用）
  clock = 3100;  p.update(5000, 10000);   // 3.1s → 建卡 + 画
  assert.equal(built.length, 1, '3 秒后应建卡片');
  assert.equal(built[0].name, 'big.bin', '卡片标题用文件名');
  const text = built[0].line.textContent;
  assert.ok(text.includes('50.0%'), '百分比应出现: ' + text);
  assert.ok(text.includes('5000B/10000B'), '已下载/总量应出现');
  // 速度 = (5000-1000)B / (3100-1000)ms = 4000/2.1s ≈ 1905 B/s
  assert.ok(text.includes('1905B/s'), '滑窗速度应出现: ' + text);
  assert.ok(text.includes('剩余'), '有速度且未完成时应给剩余时间');
  assert.equal(built[0].fill.style.width, '50.0%', '进度条宽度 = 百分比');
});

// ---------------------------------------------------------------- ③ 10 Hz 节流
t('10 Hz 节流：100ms 内的第二次画被跳过', () => {
  reset();
  const p = makeDownloadProgress('x.bin');
  clock = 4000; p.update(2000, 10000);
  const text1 = built[0].line.textContent;
  clock = 4050; p.update(6000, 10000);   // 距上次仅 50ms → 跳过
  assert.equal(built[0].line.textContent, text1, '50ms 内不该重画');
  clock = 4150; p.update(8000, 10000);   // 距上次 150ms → 重画
  assert.notEqual(built[0].line.textContent, text1, '150ms 后应重画');
  assert.ok(built[0].line.textContent.includes('80.0%'));
});

// ---------------------------------------------------------------- ④ 速度滑窗
t('速度滑窗：只统计最近 3 秒的采样', () => {
  reset();
  const p = makeDownloadProgress('y.bin');
  clock = 1000; p.update(1_000_000, 10_000_000);
  clock = 5000; p.update(5_000_000, 10_000_000);   // 建卡；窗口含 1s 与 5s 两个点
  clock = 9000; p.update(5_000_000, 10_000_000);   // 1s 的点已出窗（8s 前）→ 窗口只剩 5s/9s
  clock = 9150; p.update(5_100_000, 10_000_000);   // 窗口 = [9000, 9150] → 速度 = 100_000/0.15
  assert.ok(built[0].line.textContent.includes('666667B/s'),
    '速度应只算窗口内采样: ' + built[0].line.textContent);
});

// ---------------------------------------------------------------- ⑤ 未知总量
t('未知总量（total=0）：进度条挂 indet，文案不带百分比', () => {
  reset();
  const p = makeDownloadProgress('z.bin');
  clock = 3500; p.update(1234, 0);
  assert.equal(built.length, 1);
  assert.ok(built[0].fill._classes.has('f:indet'), '未知总量应挂 indet 类');
  assert.ok(!built[0].line.textContent.includes('%'), '未知总量不该有百分比');
  assert.ok(built[0].line.textContent.includes('已下载'), '应显示已下载字节数');
  clock = 3700; p.update(2000, 4000);   // 拿到总量 → 摘 indet、画百分比
  assert.ok(!built[0].fill._classes.has('f:indet'), '拿到总量后应摘掉 indet');
  assert.ok(built[0].line.textContent.includes('50.0%'));
});

// ---------------------------------------------------------------- ⑥ finish 成功
t('finish(true)：done 类 + 下载完成 + 2500ms 自动移除 + 点击关闭', () => {
  reset();
  const p = makeDownloadProgress('ok.bin');
  clock = 3500; p.update(4000, 4000);
  const card = built[0];
  clock = 3600; p.finish(true);
  assert.ok(card.card._classes.has('done'), '成功应挂 done 类');
  assert.equal(card.line.textContent, '✅ 下载完成 · 4000B', '文案应为下载完成 + 最终字节数');
  assert.equal(timeouts.length, 1, '应安排一次自动移除');
  assert.equal(timeouts[0].ms, 2500, '成功停留 2.5s');
  assert.equal(typeof card.card.onclick, 'function', '点击关闭应已挂上');
  card.card.onclick();                    // 点击 = 立即移除
  assert.equal(removed.length, 1, '点击应立即移除卡片');
  timeouts[0].fn();                       // 到点再触发（重复移除由真 removeDlCard 兜底幂等）
});

// ---------------------------------------------------------------- ⑦ finish 失败
t('finish(false)：err 类 + 错误文案 + 6000ms 移除', () => {
  reset();
  const p = makeDownloadProgress('bad.bin');
  clock = 3500; p.update(100, 9999);
  const card = built[0];
  clock = 3600; p.finish(false, '连接被重置');
  assert.ok(card.card._classes.has('err'), '失败应挂 err 类');
  assert.ok(card.line.textContent.includes('下载失败'), '文案应为下载失败');
  assert.ok(card.line.textContent.includes('连接被重置'), '应带上错误信息');
  assert.equal(card.line.title, '连接被重置', '长错误靠 title 提示');
  assert.equal(timeouts[0].ms, 6000, '失败停留 6s（够看清错误）');
});

// ---------------------------------------------------------------- ⑧⑨ finish 语义
t('finish 幂等；finish 后 update 被忽略', () => {
  reset();
  const p = makeDownloadProgress('idem.bin');
  clock = 3500; p.update(1000, 2000);
  const card = built[0];
  clock = 3600; p.finish(true);
  clock = 3700; p.finish(false, '再来一次');
  clock = 3800; p.update(1999, 2000);
  assert.equal(timeouts.length, 1, '二次 finish 不该再安排移除');
  assert.equal(card.line.textContent, '✅ 下载完成 · 1000B', '后续 update/finish 不改文案');
  assert.ok(card.card._classes.has('done') && !card.card._classes.has('err'));
});

// ---------------------------------------------------------------- ⑩ removeDlCard 容器
t('removeDlCard：最后一张移除后容器 hidden；空句柄不炸', () => {
  // 最小假 DOM：host 记录 children，removeChild 摘节点
  function hostWith(children) {
    return {
      firstChild: children[0] || null,
      removeChild(c) {
        const i = children.indexOf(c);
        if (i >= 0) children.splice(i, 1);
        this.firstChild = children[0] || null;
      },
      hidden: false,
    };
  }
  const c1 = { tag: 'c1' };
  const c2 = { tag: 'c2' };
  const host = hostWith([c1, c2]);
  c1.parentNode = host;
  c2.parentNode = host;
  removeDlCard({ card: c1 });                     // 移除第一张
  assert.equal(host.hidden, false, '还有一张时容器不隐藏');
  removeDlCard({ card: c2 });                     // 移除最后一张
  assert.equal(host.hidden, true, '最后一张移除后容器隐藏');
  removeDlCard(null);                              // 空句柄不炸
  removeDlCard({ card: {} });                      // 没有 parentNode 不炸
  removeDlCard({ card: { parentNode: null } });    // parentNode 为 null 不炸
});

// ---------------------------------------------------------------- 汇总
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
