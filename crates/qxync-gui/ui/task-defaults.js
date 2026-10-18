/* ============================================================================
 * qxync GUI —— 新建任务的默认 id / 本地文件夹 / 水合缓存目录（★ M18）
 *
 * 为什么单独一个文件（而不塞进 app.js）：
 *   这三条规则是**纯函数**，不碰 DOM、不碰 IPC —— 独立出来就能用 node 直接单测
 *   （xtask/tests/task-defaults.test.mjs），也能被无头浏览器集成测试复用。
 *
 * 规则（对应 docs/M18-任务默认路径.md）：
 *   * 任务 id   = NAS 文件夹名（`/home` → `home`，`/home/Public` → `public`），
 *                 与已有任务重名时自动加 `-2` / `-3`…（后端限制 id ≤ 64 字符）；
 *   * 本地文件夹 = `<HOME>/qxync/<连接 id>/<任务 id>`；
 *   * 水合缓存   = `<data_dir>/cache/tasks/<连接 id>/<任务 id>`（**父目录**）。
 *
 * 为什么要带上「连接 id」：任务 id 是全局唯一的文件名（`tasks/<id>.json`），
 * 但本地路径没有全局唯一约束 —— 多 NAS 连接时 `~/qxync/default/home` 与
 * `~/qxync/nas2/home` 一眼能看出是哪个账号的哪条任务。
 *
 * 为什么缓存要按任务隔离：缓存**父目录**默认是按连接共享的（`cache/<连接命名空间>`），
 * 多条任务共用一份水合缓存；按任务派生后每条任务可以单独清理、单独设限额，
 * 代价是同一批远端文件被不同任务水合时会各存一份（极少见，且只影响可再生的缓存）。
 * ==========================================================================*/
(function (global, factory) {
  'use strict';
  var api = factory();
  // node 单测（xtask/tests/task-defaults.test.mjs）可以直接 require；
  // 浏览器里连 `file://` 打开也能用（没有 module 对象，走 global 分支）。
  if (typeof module === 'object' && module && module.exports) { module.exports = api; }
  if (global) { global.QXNYC_TASK_DEFAULTS = api; }
})(
  typeof window !== 'undefined' ? window
    : (typeof globalThis !== 'undefined' ? globalThis : null),
  function () {
    'use strict';

    // 与后端 `Task::is_valid_id` / `LinkConfig::is_valid_id` 对齐
    var ID_MAX = 64;
    // 给 `-2` / `-999` 这类去重后缀留位置
    var BASE_MAX = 56;
    var FALLBACK = 'task';

    function str(v) { return (v === undefined || v === null) ? '' : String(v); }

    /** 任务 / 连接 id 是否合法（后端同样规则：A-Za-z0-9 . _ -，1~64 字符，不能是 . / ..）。 */
    function isValidId(id) {
      var s = str(id);
      return s.length > 0 && s.length <= ID_MAX && s !== '.' && s !== '..'
        && /^[A-Za-z0-9._-]+$/.test(s);
    }

    /** 把任意一段文字压成 id 片段：只留 ASCII 字母数字与 . _ -，其余折成 '-'。 */
    function idPart(s) {
      var out = str(s).toLowerCase()
        .replace(/[^a-z0-9._-]+/g, '-')
        .replace(/-+/g, '-')
        .replace(/^[-._]+/, '')
        .replace(/[-._]+$/, '');
      return out;
    }

    /** NAS 路径 → 任务 id 基名：`/home` → `home`；`/home/Public` → `public`；`/` → `task`。 */
    function idBaseFromRoot(root) {
      var p = str(root).trim().replace(/\/+$/, '');
      var i = p.lastIndexOf('/');
      var seg = i >= 0 ? p.slice(i + 1) : p;
      var base = idPart(seg) || FALLBACK;
      if (base.length > BASE_MAX) { base = base.slice(0, BASE_MAX).replace(/[-._]+$/, ''); }
      return base || FALLBACK;
    }

    /** 在一批已用 id 里挑一个不冲突的：`base` / `base-2` / `base-3` … */
    function uniqueId(base, taken) {
      var b = str(base) || FALLBACK;
      var used = taken || {};
      var candidate = b;
      var n = 2;
      while (used[candidate] || !isValidId(candidate)) {
        candidate = b + '-' + n;
        n += 1;
        if (n > 10000) { break; } // 病态输入兜底：正常永远走不到
      }
      return candidate;
    }

    /** 路径里的一段：非法字符折成 '-'，空则回落到 `task`。 */
    function seg(v) {
      return idPart(v) || FALLBACK;
    }

    /** 默认本地挂载点：`<HOME>/qxync/<连接 id>/<任务 id>`；HOME 未知时返回空串（别猜）。 */
    function mountpoint(home, linkId, taskId) {
      var h = str(home).replace(/\/+$/, '');
      if (!h) { return ''; }
      return h + '/qxync/' + seg(linkId) + '/' + seg(taskId);
    }

    /**
     * 默认水合缓存**父目录**：`<data_dir>/cache/tasks/<连接 id>/<任务 id>`。
     *
     * ⚠️ 这是父目录：daemon 收到显式的 `cache_dir` 后还会再拼一层「连接命名空间」
     * （`config::link_namespace`：`default` → NAS 主机名；其它 → `#<id>@<主机名>`），
     * 所以真实的缓存文件在 `<这里>/<命名空间>/` 下。
     */
    function cacheDir(dataDir, linkId, taskId) {
      var d = str(dataDir).replace(/\/+$/, '');
      if (!d) { return ''; }
      return d + '/cache/tasks/' + seg(linkId) + '/' + seg(taskId);
    }

    return {
      isValidId: isValidId,
      idPart: idPart,
      idBaseFromRoot: idBaseFromRoot,
      uniqueId: uniqueId,
      mountpoint: mountpoint,
      cacheDir: cacheDir
    };
  }
);
