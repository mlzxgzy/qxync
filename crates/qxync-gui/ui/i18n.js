/* ============================================================================
 * qxync GUI —— i18n 文案表（★ M8.6）
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
    'app.title': 'qxync — QNAP 按需同步',
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
    'nav.files_title': '文件（浏览 NAS / 节省空间模式）',
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

    // ---- ★ T3：缓存内容校验
    'cache.integrity': '内容校验（xxhash64）',
    'cache.verify': '校验缓存内容',
    'cache.verify_repair': '校验并修复',

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
    'home.conn_qxync': ' · Qsync {version}',
    'home.tasks_empty': '还没有同步任务。点右上「＋ 添加任务」把本地文件夹与 NAS 文件夹配成一对。',
    'home.task_count': '（{n}）',
    'task.state_no_daemon': 'qxyncd 未运行',
    'task.state_not_logged_in': '未登录 NAS',
    'task.state_no_task': '还没有同步任务',
    'task.state_sync_error': '同步出错：{msg}',
    'task.state_offline': '网络断开 —— 本地改动先攒着，联网后自动同步',
    'task.state_delete_blocked': '有删除被熔断挡住 —— 这轮没有真的删除任何文件',
    'task.state_paused': '已暂停轮询',
    'task.state_no_poll': '尚未跑过同步轮次',
    'task.state_conflicts': '最近一轮有 {n} 个冲突副本',
    'task.state_all_synced': '所有文件均处于最新状态',
    'task.state_paused_task': '已暂停（登记停用，挂载点已卸载）',
    'task.state_not_mounted': '已启用，但当前未挂载',
    'task.pair': '本地 {local}  ⇄  NAS {remote}',
    'task.roots_by_link': '（未单独指定，用家目录）',
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
    // ★ 一对一配对：NAS 文件夹下拉 / 浏览
    'task.nas_home': '家目录（/home）',
    'task.nas_manual': '手动输入 / 其他路径…',
    'task.nas_syncing_folder': 'NAS 同步文件夹',
    'task.act_resume': '继续',
    'task.act_pause': '暂停',
    'task.act_mount': '挂载',
    'task.act_manage': '管理',
    'task.act_open_dir': '打开目录',
    'task.act_sync_now': '立即同步',
    'task.act_add': '＋ 添加任务',
    'task.act_add_pair': '＋ 添加配对文件夹',
    'task.act_settings': '设置',
    'task.act_delete': '删除…',

    // ---- 三态（节省空间模式）
    'state.online': '仅在线',
    'state.dir': '目录',
    'state.dirty': '未上传',
    'state.local': '本地可用',
    'state.always': '始终可用',
    // ★ M15/T8：同步维度（与上面的空间三态正交）
    'state.progress': '同步中 {pct}%',
    'state.outOfSync': '未同步',
    'state.conflicts': '{n} 个冲突待处理',
    'state.syncUnknown': '同步状态未知',
    'files.summary_states': ' · 仅在线 {online} / 本地可用 {local} / 始终可用 {always}',
    'files.summary_sync': ' · 已同步 {in_sync} / 未同步 {out_of_sync}{unknown}',
    'files.summary_transferring': ' · 传输中 {transferring}',
    'files.summary_no_mount': ' · 三态需挂载后才能算（daemon 侧没有该目录的挂载视图）',
    'files.info': '共 {n} 项',
    'files.info_dir': '共 {n} 项 · {dir}',
    'files.not_logged_in': '未登录 —— 先到「设置 → 连接」保存并登录',

    // ---- 通用四态（★ M8.6：加载中 / 读取失败 / 空 / 有数据）
    'list.error': '读取失败：{msg}',
    'list.error_daemon': '读不到数据：daemon 未运行（先在顶部点「启动 daemon」）',

    // ---- 各列表的空态 / 加载态
    'files.loading': '正在读取 NAS 上的目录…',
    'files.empty': '目录为空，或尚未加载。',
    'journal.loading': '正在读取同步日志…',
    'journal.empty': '没有日志。挂载一个同步任务后点「立即同步」，或有实际同步活动时就会出现记录。',
    'journal.cleared': '日志已清空。',
    'journal.empty_filtered': '没有符合条件的日志。挂载一个同步任务后点「立即同步」，或有实际同步活动时就会出现记录。',
    'journal.counts': '共 {total} 条 · ok {ok} / warn {warn} / error {error} / blocked {blocked}',
    'errors.loading': '正在读取错误列表…',
    'errors.empty': '没有失败项 —— 同步、上传、脱水目前都正常。',
    'tasks.loading': '正在读取任务列表…',
    'tasks.empty': '还没有同步任务。点右上「＋ 添加配对文件夹」把本地文件夹与 NAS 文件夹配成一对。',
    'tasks.empty_never': '还没有登记过任务。点右上「＋ 添加配对文件夹」建一个。',
    'tasks.empty_files': '任务目录里没有可用的任务文件。',
    'tasks.bad_file': '⚠️ 任务文件解析失败（已跳过）：{file} —— {err}',
    'decisions.loading': '正在读取冲突待裁决队列…',
    'decisions.empty': '没有待裁决的冲突。',
    'decisions.hint': '只有把某个任务的冲突策略设成 ask（每个文件都问我）时才会有条目。待裁决期间两边都不动（谁的改动都不会被覆盖）；裁决后由下一轮同步执行。已失效的条目会在下一轮同步时自动清掉。',
    // ---- 动作与反馈
    'decisions.act_refresh': '刷新',
    'decisions.act_clear': '清空全部',
    'decisions.act_diff': '对比本地/NAS',
    'decisions.act_diff_busy': '对比中…',
    'decisions.act_diff_aria': '对比 {name} 的本地与 NAS 内容',
    'decisions.act_forget': '忽略这条',
    'decisions.keep_local': '保留本地',
    'decisions.keep_remote': '保留 NAS 上的',
    'decisions.keep_both': '两份都留',
    'decisions.resolve_ok': '已裁决 {path} → {res}（下一轮同步执行）',
    'decisions.resolve_err': '裁决失败：{msg}',
    'decisions.forget_confirm': '忽略这条待裁决？\n\n只从队列里删掉这一条记录，不会动任何文件。\n（下次真的又冲突了，同步引擎会重新列出来。）',
    'decisions.forget_ok': '已忽略 {path}（未动文件）',
    'decisions.forget_err': '忽略失败：{msg}',
    'decisions.clear_confirm': '清空整个待裁决队列？\n\n只清队列记录，不会动任何文件。\n（还没裁决的冲突也会一起清掉；下次真的又冲突了会重新列出来。）',
    'decisions.clear_ok': '已清空待裁决队列（{n} 条，未动文件）',
    'decisions.clear_err': '清空失败：{msg}',
    // ---- 文本对比
    // 语义方向（与 Rust `text_diff` 的实现一致）：
    //   `-` = 只有本地有（removed），`+` = 只有 NAS 有（added）。
    // ★ 旧键 `decisions.diff_legend`（「本地多」）把方向说反了，已删除，不要加回来。
    'decisions.diff_local_only': '本地独有',
    'decisions.diff_nas_only': 'NAS 独有',
    'decisions.diff_local_only_hint': '这些行只有本地有：选「保留 NAS 上的」会丢掉它们',
    'decisions.diff_nas_only_hint': '这些行只有 NAS 上有：选「保留本地」会丢掉它们',
    'decisions.diff_size': '本地 {l} / NAS {r}',
    'decisions.diff_identical': '两边内容逐行相同。',
    // truncated 有**两种来源**（见 decisions.rs）：输出行数超上限被截断、或输入过大走
    // 「有界回退」只对比前 N 行。文案对两者都要成立，不能写成精确行数承诺。
    'decisions.diff_truncated': '差异过多，只显示前 {n} 行（结果已截断）。',
    'decisions.diff_failed': '对比失败：{msg}',
    'decisions.diff_no_data': '没有拿到对比结果。',
    'decisions.diff_bad_shape': '对比结果格式不对（缺少 lines / added / removed 字段），无法显示逐行差异 —— 这通常是 GUI 与 qxyncd 版本不一致，请两边一起升级。',
    'decisions.diff_too_large': '文件 {size}，超出 {limit} 的文本对比上限，不做逐行对比。仍可用上面的「保留本地 / 保留 NAS / 两份都留」裁决。',
    'decisions.diff_binary': '这是二进制文件（含 NUL 字节），不做逐行对比。仍可用上面的「保留本地 / 保留 NAS / 两份都留」裁决。',
    'decisions.diff_is_dir': '这是目录，不做逐行对比。',
    'decisions.diff_no_content': '{side}内容读不到：{detail}',
    'decisions.diff_unknown': '这个文件暂时没法逐行对比。',
    'decisions.side_local': '本地',
    'decisions.side_remote': 'NAS 上的',
    'mounts.loading': '正在读取挂载表…',
    'mounts.empty': '暂无挂载。',
    'roots.loading': '正在读取 NAS 上登记的同步文件夹…',
    'roots.folders_empty': 'NAS 未登记 Qsync 同步文件夹',
    'sync.missing': 'daemon 未上报 sync 状态。',
    // ---- 删除熔断（★ 提示自带出口：文案说清「没删东西」，按钮就在这一行）
    'sync.blocked_alert': '删除被熔断挡住：{reason}（这轮没有真的删除任何文件；确认这些删除确实需要，再放行）',
    'sync.force_deletes': '放行删除',
    'sync.force_deletes_noreason': '（daemon 未给出原因）',
    'sync.force_deletes_confirm': '放行这一轮的删除？\n\n原因：{reason}\n\n放行后会真的执行这批删除（只放行这一轮，下一轮超阈值会再次熔断）。\n如果这些删除不是你预期的 —— 比如 NAS 上的文件夹被误清空 —— 就别放行。',
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

    // ---- ★ M23 Dolphin 集成（user.xdg.tags / Baloo 门禁）
    'adv.dolphin_hint': '两个开关相互独立，可只开一个，默认都关。Tags 列：把图标字符（✓ 已水合 / ☁ 仅在线 / ◐ 部分本地）写进 user.xdg.tags，Dolphin 内置列直接显示，不需要装任何插件。文件角标：需要装 qxync-dolphin-overlay 插件包（KOverlayIconPlugin，只支持 KDE Dolphin），Dolphin 才会在文件图标上画彩色角标（绿对钩 / 蓝云），读的是 user.qxync.overlay —— 装没装、会不会加载，看下面检测表里的「角标插件」一行。两者前置条件相同：挂载点必须已排除出 Baloo 索引（否则索引器会把整个 NAS 拉到本地）。状态都不实时刷新（F5 / 重进目录后更新）。',
    'baloo.title': '需要先排除 Baloo 索引',
    'baloo.why': '这些挂载点还没排除出 Baloo 索引：{mounts}。Baloo 的内容索引器会读文件内容 —— 在 qxync 挂载点上等于把整个 NAS 拉到本地，节省空间模式就废了。点「打开终端」会自动执行排除命令（全程可见，跑完窗口留在原地）。',
    'baloo.open_terminal': '打开终端，自动添加排除',
    'baloo.opened': '已用 {terminal} 打开终端执行排除命令',
    'baloo.after_open': '等终端里的命令跑完后，回到这里再点一次「保存高级设置」（或先点「重新检测 Baloo」核对）。',
    'baloo.open_failed': '打开终端失败：{err}。可以手动执行：{cmd}',
    'baloo.rechecked': 'Baloo 检测已刷新',
    'baloo.enabled': '已安装，索引开启',
    'baloo.disabled': '已安装，索引全局关闭（Tags 列也不会显示）',
    'baloo.not_installed': '未安装（不会有索引风险，但 Dolphin 的 Tags 列也不可用）',
    'baloo.no_mounts': '（还没有挂载点 —— 开关可以先打开，挂载时会再检查）',
    'baloo.mount_indexed': '会被索引（危险）',
    'baloo.mount_excluded': '已排除',
    'baloo.mount_safe': '不会被索引',
    'baloo.tags_on': 'Tags 已生效',
    'baloo.overlay_on': '角标已生效',
    'baloo.tags_note': '在 Dolphin 里右键列头勾「Tags」列即可看到水合状态字符；装了角标插件的话文件图标上还会显示彩色角标。状态变化要 F5 / 重进目录才刷新。',
    // ★ M25：角标插件（qxync-dolphin-overlay）安装检测
    'baloo.plugin_yes': '已安装（{path}）',
    'baloo.plugin_ineffective': '装在 {path}，但 QT_PLUGIN_PATH 未包含该目录 —— Dolphin 不会加载（建议改用系统级安装）',
    'baloo.plugin_no': '未安装 —— 角标开关开了也不会画角标（Arch：装 qxync-dolphin-overlay 包；其它发行版从 GitHub Release 下载 .pkg.tar.zst 用 pacman -U 装，或自行编译 contrib/dolphin/overlay）',
    // ★ NAS 文件夹选择器（一对一配对）
    'nas_picker.title': '选择 NAS 文件夹',
    'nas_picker.hint': '逐层点开目录；「选择这个文件夹」= 把当前目录作为配对的那一个 NAS 目录。',
    'nas_picker.up': '↑ 上级',
    'nas_picker.pick': '选择这个文件夹',
    'nas_picker.loading': '正在读取 NAS 目录…',
    'nas_picker.empty': '这个目录下没有子文件夹。',
    'busy.text': '进行中…',

    // ---- ★ M33：右下角下载进度卡片
    'dl.done': '下载完成',
    'dl.fail': '下载失败',
    'dl.click_close': '点击关闭',

    // ---- 行右键菜单（节省空间模式）
    'menu.pinquery': '🔍 查询 pin 状态',
    'menu.pin': '☑ 始终保留在此设备（Always keep on this device）',
    'menu.unpin': '☐ 取消固定',
    'menu.unpinned': '⊘ 不保留在此设备',
    'menu.excluded': '🚫 排除（不同步）',
    'menu.free': '⤓ 释放空间（Free up space）',
    'menu.get': '下载到本地…',
    'menu.copy': '复制 NAS 路径',
    'menu.rm': '删除（NAS 上的）',

    // ---- ★ M16：多 NAS 连接
    // 顶栏：N/M 聚合（每个 daemon 各自独立，不能混算）
    'top.switch': '当前连接',
    'top.switch_aria': '切换当前连接',
    // ★ M16.1：「当前连接」旁边的新建入口
    'top.add_link_title': '添加新的 NAS 连接',
    'top.add_link_aria': '添加新的 NAS 连接',
    'top.daemon_count': 'daemon {up}/{total} 运行',
    'top.login_count': '已登录 {up}/{total}',
    'top.conn_unconfigured': '连接：未配置',
    'top.daemon_error': 'daemon：状态读取失败',
    'top.meta_error': 'daemon：状态读取失败 —— {msg}',
    'links.unconfigured': '未配置',
    'links.current': '当前',
    'links.count': '共 {n} 个连接',
    'links.loading': '正在读取连接列表…',
    'links.empty': '还没有 NAS 连接。点「＋ 添加 NAS」配置第一台。',
    'links.hint': '每一行是一台 NAS。当前连接决定「文件 / 诊断」页与顶部状态条显示哪个 daemon；同步任务可以分属不同连接，任务卡上会标出来源。',
    'links.refresh': '刷新',
    'links.start_all': '全部启动',
    'links.stop_all': '全部停止',
    'links.add': '＋ 添加 NAS',
    // ★ M16.1：从「主页 / 任务表单」直达新建连接 —— 原来只有设置页最里面那个按钮，
    //   用户盯着「连接」卡片和「NAS 连接」下拉时找不到新建入口。
    'home.add_link': '＋ 添加连接',
    'home.add_link_title': '添加新的 NAS 连接',
    'task.link_new': '＋ 新建连接…',
    'task.link_new_hint': '已跳到「设置 → 连接」：填好新连接并「保存并登录」后，回到「任务」页继续这条文件夹对。',
    'links.daemon_running': 'daemon 运行中',
    'links.daemon_stopped': 'daemon 未运行',
    'links.daemon_down_suffix': '（daemon 未运行）',
    'links.daemon_error': '错误：{msg}',
    'links.login_ok': '已登录',
    'links.login_none': '未登录',
    'links.login_unknown': '登录状态未知',
    'links.cred_present': '凭据已存',
    'links.cred_missing': '无凭据',
    // ★ M17：只标「配没配」，徽标里绝不会出现密钥
    'links.otp_badge': '两步验证',
    'links.link_missing': '无 link 文件',
    'links.task_count': '任务 {n}',
    'links.act_current': '设为当前',
    'links.act_start': '启动',
    'links.act_stop': '停止',
    'links.act_edit': '编辑',
    'links.act_remove': '移除',
    'links.remove_unavailable': '这条连接没有 link 文件（空转 daemon 的合成项），不能移除',
    'links.bad_file': '⚠️ 连接文件解析失败（已跳过）：{path} —— {err}',
    'links.saved': '连接配置已保存',
    'links.stop_confirm': '停止连接「{label}」的 daemon？\n会卸载它的全部挂载点并删除 socket / pid（不删任何文件）。',
    'links.stop_all_confirm': '停止所有连接的 daemon？\n会卸载全部挂载点（不删除任何文件）。',
    'links.confirm_delete': '移除连接「{label}」？\n\n会停止 daemon、卸载挂载点；不会删除本地文件或 NAS 数据。',
    'links.confirm_delete_tasks': '移除连接「{label}」？\n\n该连接还有 {n} 个任务登记：\n{tasks}\n\n确认后会一并删除这些任务登记、停止 daemon、卸载挂载点；\n不会删除本地文件或 NAS 数据。',
    'conn.form_title_new': '添加 NAS 连接',
    'conn.form_title_edit': '编辑连接 · {id}',
    'conn.edit_hint': '正在编辑连接「{id}」（id 只读：换 id 等于换一个连接）。',
    'conn.remove': '移除这个连接',
    'conn.cancel_edit': '取消编辑',
    // ★ M16.1：不带外层括号 —— 它会被塞进「尚无连接配置（{path}）」里，自己带括号会套两层
    'conn.path_pending': '新建连接，保存后写入 links/<id>.json',
    'connect.cred_present': '已保存凭据：{user}@{host}（{path}）',
    'connect.cred_missing': '无凭据（{path}）；保存并登录时需要输入口令。',
    // ★ M17：两步验证（2SV / TOTP）—— 输入框提示与凭据状态后缀
    'connect.otp_hint': '开了两步验证才需要：填 TOTP 密钥会保存下来，之后自动生成验证码；填 6 位验证码只用于本次登录。密钥与口令一样保存在 0600 凭据文件里。',
    'connect.otp_configured': '两步验证：已配置',
    'connect.otp_missing': '两步验证：未配置',
    'home.conn_no_daemon': '连接 {label} 的 daemon 未运行 —— 点顶部「启动 daemon」',
    'home.sync_title': '同步',
    'home.sync_link': '{label}',
    'home.sync_brief': '轮询 {polls} 轮 · 上传 {uploaded} · 删除 {deleted} · 冲突 {conflicts}',
    'home.sync_no_daemon': 'daemon 未运行',
    'home.sync_missing': 'daemon 未上报 sync 状态',
    'home.sync_no_link': '还没有连接配置',
    'home.sync_all_down': '所有连接的 daemon 都未运行 —— 上面的同步状态不可用',
    'home.sync_err_link': '同步错误（{label}）：{msg}',
    'home.sync_blocked_link': '删除被熔断挡住（{label}）：{reason}（这轮没有真的删除任何文件；确认这些删除确实需要，再放行）',
    'home.sync_note_link': '说明（{label}）：{note}',
    'home.offline_link': '离线（{label}）：{since} {state}',
    'home.offline_pending_link': '离线缓冲区（{label}）：待上传 {up} · 待删除 {del} · 待建目录 {mkdir} · 待改名 {rename} · 待改 mtime {setmtime}',
    'task.group_error': '连接 {label} 读取失败：{msg}',
    'task.link_badge': '连接 {label}',
    'task.link_daemon_down': '该连接的 daemon 未运行',
    'task.link_locked': '编辑任务时连接只读：换 NAS = 删除后重建任务（任务 id 不能跨连接复用）',
    'tasks.scope_hint': '任务按连接分组；每张卡片标出来源连接，操作会发到该连接自己的 daemon。',
    'tasks.note_prefix': '说明：',
    'list.conn_error': '连接 {label} 读取失败：{msg}',
    'list.conn_note': '连接 {label} 说明：{note}',
    'list.conn_no_daemon': '未运行 daemon 的连接：{links}（这些连接的数据暂时读不到）',
    'list.no_daemon_all': '所有连接的 daemon 都未运行 —— 先到「设置 → 连接」启动一台（或点顶部「启动 daemon」）',
    'journal.scope_hint': '日志来自所有运行中的连接，每行标出来源连接。',
    'journal.note_prefix': '说明：',
    'errors.scope_hint': '错误来自所有运行中的连接，每条标出来源连接。',
    'errors.no_detail': '（无说明）',
    'errors.copy_path': '复制路径',
    'files.error_daemon_link': '连接 {label} 的 daemon 未运行（先在顶部点「启动 daemon」）',
    'files.link_hint': '文件页只作用于当前连接：{label}',
    'diag.link_hint': '诊断页只显示当前连接的数据：{label}',
    'settings.save_failed': '保存失败',
    'settings.broadcast_fail': '{n} 个连接写失败：{msg}',
    'settings.save_no_daemon': '没有运行中的 daemon：设置没有写入（settings.json 由 daemon 读写，先启动一台连接的 daemon 再保存）',
    'settings.broadcast': '已写入 {ok}/{total} 个运行中的 daemon',
    'decisions.resolved_pending': '（已裁决，待执行）',
    'decisions.pending': '（待裁决）',
    'decisions.clear_partial': '；{n} 个连接失败',

    // ---- ★ 删除 / 移除：可一并删除相关数据（purge）
    // 三层（任务卡 / 连接行 / 设置→高级）共用一套弹窗，只有标题、说明与复选框文案不同。
    // 复选框默认**不勾选** = 保持旧行为（只删登记/连接，本地数据留着）。
    'purge.title': '删除确认',
    'purge.confirm_delete': '删除',
    'purge.confirm_remove': '移除',
    'purge.task_title': '删除任务「{id}」？',
    'purge.task_message': '先卸载挂载点，再删除这条任务登记。NAS 上的文件与挂载点里的文件都不会删。',
    'purge.link_title': '移除连接「{label}」？',
    'purge.link_message': '会停止 daemon、卸载挂载点、删除该连接的 {n} 个任务登记。NAS 上的文件与挂载点里的文件都不会删。',
    'purge.all_title': '删除本机所有 qxync 数据？',
    'purge.all_message': '不可恢复；会先停止所有 daemon。NAS 上的数据与挂载点里的文件不动。',
    'purge.opt_task': '同时删除相关数据（水合缓存 / 同步状态 / 日志）· 约 {size}',
    'purge.opt_link': '同时删除该连接的全部本地数据（缓存 / 同步状态 / 队列 / 日志）· 约 {size}',
    'purge.opt_all': '我了解这会清空所有连接与任务，且不可恢复',
    'purge.opt_hint': '不勾选 = 只删登记 / 连接本身，本地数据（缓存、同步状态、日志等）保留。',
    'purge.optional_mark': '仅勾选后删除',
    'purge.total': '{files} 个文件 · {size}',
    'purge.db_rows': 'baseline {baseline} 条 / 日志 {journal} 条 / 待裁决 {decisions} 条',
    'purge.warn_prefix': '注意：',
    'purge.done_task': '已删除任务 {id}，并清除相关数据（{size} · {files} 个文件）',
    'purge.done_task_kept': '已删除任务 {id}（本地数据保留）',
    'purge.done_link': '已移除连接 {label}，并清除本地数据（{size} · {files} 个文件）',
    'purge.done_all': '已删除本机所有 qxync 数据（{size} · {files} 个文件）',
    // 条目「种类」：未知 kind 回落到后端给的原字符串（绝不显示成 key）
    'purge.kind.task': '任务登记',
    'purge.kind.link': '连接配置',
    'purge.kind.credentials': '凭据',
    'purge.kind.peers': 'LAN 对端记录',
    'purge.kind.cache': '水合缓存',
    'purge.kind.sync': '同步状态',
    'purge.kind.queue': '待裁决队列',
    'purge.kind.log': '日志',
    'purge.kind.mounts': '挂载点',
    'purge.kind.socket': 'socket',
    'purge.kind.pid': 'pid 文件',
    'purge.kind.config': '配置目录',
    'purge.kind.data': '数据目录',
    'purge.kind.state': '状态目录',

    // ---- ★ 设置 → 高级：危险区
    'danger.title': '危险区',
    'danger.hint': '删除本机所有 qxync 数据（连接 / 凭据 / 任务 / 缓存 / 同步状态 / 日志）；NAS 上的数据与挂载点里的文件不动。',
    'danger.purge_all': '删除本机所有 qxync 数据…',
    'danger.opt': '我了解这会清空所有连接与任务，且不可恢复'
  };

  // ---------------------------------------------------------------- en（预留）
  // 空表 = 全部回落到 zh-CN。加词条时**只加需要改的**，不用整份复制：
  //   t() 的回落顺序是 en → zh-CN → key 本身。
  // ★ M17：这里只补了两步验证（2SV）的 4 条 —— 其余键继续回落 zh-CN。
  var EN = {
    'connect.otp_hint': 'Only needed when 2-step verification is enabled: a TOTP secret is saved and used to generate codes later; a 6-digit code is used for this login only. The secret is stored in the 0600 credentials file, same as the password.',
    'connect.otp_configured': '2-step verification: configured',
    'connect.otp_missing': '2-step verification: not configured',
    'links.otp_badge': '2-step'
  };

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

  global.QXNYC_I18N = {
    t: t,
    apply: apply,
    setLocale: setLocale,
    locale: currentLocale,
    stats: stats,
    missingInDOM: missingInDOM,
    tables: TABLES
  };
})(typeof window !== 'undefined' ? window : this);
