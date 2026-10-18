#!/usr/bin/env node
/* ============================================================================
 * M18 任务默认路径 —— 纯函数单测（node 直接跑，不依赖浏览器 / 依赖树）
 *
 *   node xtask/tests/task-defaults.test.mjs
 *
 * 被测文件：crates/qxync-gui/ui/task-defaults.js（浏览器与 node 共用的 UMD 模块）。
 * 这些规则决定了「第二条任务会不会撞 id / 撞挂载点 / 共用缓存」，必须有单测钉住。
 * ==========================================================================*/
import { createRequire } from 'node:module';
import assert from 'node:assert/strict';

const require = createRequire(import.meta.url);
const TD = require('../../crates/qxync-gui/ui/task-defaults.js');

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

// ---------------------------------------------------------------- id 派生
t('idBaseFromRoot(/home) = home', () => assert.equal(TD.idBaseFromRoot('/home'), 'home'));
t('idBaseFromRoot(/home/Public) = public（取最后一段 + 小写）', () =>
  assert.equal(TD.idBaseFromRoot('/home/Public'), 'public'));
t('idBaseFromRoot 允许尾斜杠', () =>
  assert.equal(TD.idBaseFromRoot('/home/Public/'), 'public'));
t('idBaseFromRoot(/home/.Qsync) = qsync', () =>
  assert.equal(TD.idBaseFromRoot('/home/.Qsync'), 'qsync'));
t('idBaseFromRoot(/home/my shared) = my-shared（空格折成 -）', () =>
  assert.equal(TD.idBaseFromRoot('/home/my shared'), 'my-shared'));
t('idBaseFromRoot 中文目录 → task（ASCII 之外无法进 id）', () =>
  assert.equal(TD.idBaseFromRoot('/home/共享目录'), 'task'));
t('idBaseFromRoot(/) = task', () => assert.equal(TD.idBaseFromRoot('/'), 'task'));
t('idBaseFromRoot(空) = task', () => assert.equal(TD.idBaseFromRoot(''), 'task'));
t('idBaseFromRoot 超长名截到 <= 56（给 -2 后缀留位置）', () => {
  const long = '/home/' + 'a'.repeat(80);
  const base = TD.idBaseFromRoot(long);
  assert.ok(base.length <= 56, 'base.length=' + base.length);
  assert.ok(TD.isValidId(base), 'base 仍是合法 id');
});

// ---------------------------------------------------------------- 去重
t('uniqueId 没冲突时原样返回', () => assert.equal(TD.uniqueId('home', {}), 'home'));
t('uniqueId 冲突 → home-2', () => assert.equal(TD.uniqueId('home', { home: true }), 'home-2'));
t('uniqueId 连续冲突 → home-3', () =>
  assert.equal(TD.uniqueId('home', { home: true, 'home-2': true }), 'home-3'));
t('uniqueId 空 base → task', () => assert.equal(TD.uniqueId('', {}), 'task'));
t('uniqueId 永远返回合法 id', () => {
  const taken = {};
  let id = 'home';
  for (let i = 0; i < 5; i += 1) {
    id = TD.uniqueId('home', taken);
    assert.ok(TD.isValidId(id), 'id=' + id);
    taken[id] = true;
  }
});

// ---------------------------------------------------------------- id 合法性与后端对齐
t('isValidId 接受 A-Za-z0-9 . _ -', () => {
  for (const id of ['home', 'home-2', 'a.b_c', 'A1']) {
    assert.equal(TD.isValidId(id), true, id);
  }
});
t('isValidId 拒绝空 / 点 / 斜杠 / 空格 / 中文 / 超长', () => {
  for (const id of ['', '.', '..', 'a/b', 'a b', '共享', 'a'.repeat(65)]) {
    assert.equal(TD.isValidId(id), false, JSON.stringify(id));
  }
});

// ---------------------------------------------------------------- 默认路径
t('mountpoint = HOME/qxync/<连接>/<任务 id>', () =>
  assert.equal(TD.mountpoint('/home/u', 'default', 'home'), '/home/u/qxync/default/home'));
t('mountpoint 多 NAS 连接按连接分目录', () =>
  assert.equal(TD.mountpoint('/home/u', 'nas2', 'home-2'), '/home/u/qxync/nas2/home-2'));
t('mountpoint HOME 未知 → 空串（不瞎猜）', () => assert.equal(TD.mountpoint('', 'default', 'home'), ''));
t('mountpoint 去掉 HOME 尾斜杠', () =>
  assert.equal(TD.mountpoint('/home/u/', 'default', 'home'), '/home/u/qxync/default/home'));

t('cacheDir = data/cache/tasks/<连接>/<任务 id>', () =>
  assert.equal(
    TD.cacheDir('/home/u/.local/share/qxync', 'default', 'home'),
    '/home/u/.local/share/qxync/cache/tasks/default/home',
  ));
t('cacheDir dataDir 未知 → 空串', () =>
  assert.equal(TD.cacheDir('', 'default', 'home'), ''));

t('两条不同 NAS 文件夹派生的 id 不同（不会「都叫 default」）', () => {
  const a = TD.uniqueId(TD.idBaseFromRoot('/home'), {});
  const b = TD.uniqueId(TD.idBaseFromRoot('/Public'), {});
  assert.notEqual(a, b);
  assert.notEqual(
    TD.mountpoint('/home/u', 'default', a),
    TD.mountpoint('/home/u', 'default', b),
  );
  assert.notEqual(
    TD.cacheDir('/home/u/.local/share/qxync', 'default', a),
    TD.cacheDir('/home/u/.local/share/qxync', 'default', b),
  );
});

t('同一 NAS 文件夹在不同连接上的路径不同（用户能一眼分清）', () => {
  const a = TD.mountpoint('/home/u', 'default', 'home');
  const b = TD.mountpoint('/home/u', 'nas2', 'home');
  assert.notEqual(a, b);
});

console.log('---');
console.log(`${passed} passed / ${failed} failed`);
process.exitCode = failed === 0 ? 0 : 1;
