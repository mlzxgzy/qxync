# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[中文](CHANGELOG.md) · [README](README.en.md) · [Acceptance log](docs/验收记录.md)

## [0.4.0] - 2026-10-03

**Cache first + local mapping**: content already cached on disk is **adopted as-is** after a node is
recreated (the hydration bitmap is persisted next to it), so a cache-hit `cat` makes no NAS round
trip; `ls`/`lookup` are served from a **local snapshot of the NAS file list** (refreshed by the
change poll) instead of waiting on the network; when a remote file changes, a **hydrated** local file
keeps serving its old content while the new version is fetched in the background and **atomically
swapped in** (until the swap, `cat` returns the complete old version and `stat` reports its size),
while a **dehydrated** file only gets refreshed metadata.
See [`docs/M9-缓存优先与映射.md`](docs/M9-缓存优先与映射.md) for the design and the on-device numbers.

### Fixed

- **Content already cached on disk was treated as "not cached"**: the range bitmap (`chunks_done`)
  lived only in memory, so after a daemon restart — or whenever a node was re-`lookup`ed —
  `cache_file_for` reset it to all `false`, and `cat` re-fetched every range from the NAS even though
  the bytes were right there. Measured on a real NAS with a 4 MiB file after a daemon restart:
  **23.75 s → 0.115 s**.
- **Every `ls` was a NAS round trip** (about 0.3–1 s) because `readdir`/`lookup` had no local
  listing. Measured: second `ls` in the same session **0.50 s → 0.001 s**; second `ls` after a
  restart **0.32 s → 0.001 s**.
- **A remote change dropped the local cache immediately**: `apply_remote_meta` discarded the content,
  so the next `read()` had to download the whole file again (blocking inside the read path). Hydrated
  files now keep the old content readable while the new version is fetched in the background, and the
  baseline is only advanced once the refresh lands — so a local edit based on the old version is
  reported as a conflict instead of as "local overwrites remote".

### Added

- **Persisted hydration bitmap** `<cache>.qxstate` (magic/version/chunk_size/size/mtime/chunk count +
  bit set): content is written before the bitmap, the bitmap goes through `.tmp` + `rename`, and
  `release()` writes it once more with the final size/mtime. Dehydration, invalidation, remote
  deletion, local unlink and renames all keep content and bitmap in sync.
- **Directory-listing snapshot** (the "mapping"): the daemon's poll pushes each listed directory to the
  mount view (`apply_listing` / `drop_listing`) and `readdir`/`lookup` are served from it; a stale
  snapshot is only refreshed in the background; local `create`/`mkdir`/`unlink`/`rename` update the
  snapshot immediately (no ghost nodes, no invisible new files).
- **Background content refresh**: the new version is downloaded into `<cache>.refresh` and then
  `rename`d into place, switching `attr`/range bitmap/state file in one step, followed by a kernel
  page-cache invalidation (the daemon injects `Notifier::inval_inode`); after 3 consecutive failures
  it falls back to on-demand hydration.
- `sync --once` and the journal now report "hydrated locally → updating content in the background".

### Tests

- `qxync-fuse` unit tests **27 → 34**: bitmap round-trip and corruption rejection, adopting cached
  content after a node is recreated, snapshot-served `readdir`/`lookup` (with `nas.invalid` as the NAS,
  so **any** network access necessarily fails), the remote-change semantics for hydrated files,
  metadata consistency of the atomic swap, aborting when the node changed, and the fallback after
  repeated refresh failures.

**Session hot-update (M10)**: after the sid expires, a mount point no longer stays `EIO` until it is
remounted — the sid is hot-swappable, and on an auth failure a mount asks the daemon to log in again,
takes the new sid and **retries in place**; a successful login **pushes the new sid to every mount of
the same account** (including the upload queue of read-write mounts), with a periodic keepalive as a
safety net. See [`docs/M10-会话热更新.md`](docs/M10-会话热更新.md).

### Fixed (session hot-update)

- **An expired sid meant a permanently `EIO` mount**: `Client.sid` was a plain field that was copied
  into the FUSE client at mount time and never refreshed; "re-login on auth failure" existed only in
  the IPC macro, so neither mounts nor the sync engine used it. On-device acceptance: after `logout`,
  a cold `ls` on the mount succeeds in 0.50 s and `cat` returns 1024 bytes, with the log showing
  "mount session expired → session hot-updated to 1 mount → login ok".
- **Change polling failed on every round once the session died**: `map_err` flattened server status
  4/5 into `ErrorKind::Status`, so callers could not tell "session expired" from other statuses; 4/5
  now map to `ErrorKind::Auth` and the poller re-logs in.

### Added (session hot-update)

- `Client`'s sid is now `Arc<RwLock<..>>` with `set_sid(&self)` / `clear_sid()`, hot-swappable across
  `Arc<Client>` handles; `qxync_core::Error::is_auth()` is the single "session expired" predicate used
  by client, daemon and FUSE.
- FUSE gets a `SidRefresher`: `readdir` / `lookup` / range hydration / `mkdir` / `unlink` /
  `set_mtime` / `rename` re-login on an auth failure and **retry exactly once**.
- Session broker: FUSE threads request a new sid synchronously while the daemon performs logins
  serially (deduplicated within 3 s).
- Session keepalive: probes every `QXNYC_SESSION_KEEPALIVE` seconds (default 120, `0` disables),
  re-logging in and pushing to mounts when the sid is gone; a failed probe (network blip) waits for
  the next round instead of hammering the login endpoint.
- A successful `login_internal` pushes the new sid to mounts of the **same account** (an account
  switch only logs a warning, to avoid mixing trees); `logout` also clears the mounts' sid.

## [0.4.1] - 2026-10-03

**Readable server errors + explicit readiness state**: the `msg` the server always sent but we
discarded is now surfaced (`Qsync Central is initializing. Please wait a few minutes and try
again.`), and the four readiness fields plus rate-limit info that `qbox_get_max_log` already
carries are parsed and wired up.

### Fixed

- **The server said why, users only saw "unknown status code"**: `Error::Status` now carries
  `msg` and renders it (`— server msg: …`). `get_list` / `stat` / `qbox_get_max_log` failure
  responses now reach the user with the server's own wording.
- **"Server not ready" was treated as an ordinary error**: reverse engineering confirmed status
  `8` is the only business status on the `qbox_*` private path (msg:
  `Qsync Central is initializing…`). `Error::is_server_busy()` now recognises it and classifies
  it as **"wait a bit" rather than "failure"** — it does not count as a sync error and does not
  trigger a re-login (re-logging in is useless; only waiting for `qsyncsrv_metad` / `qsyncsrvd`
  to become ready helps). The check keys off the `msg` text rather than a hard-coded number, so
  it survives a future server-side code change.
- **Pointless full re-scan while the NAS is migrating / recovering / restoring a backup**: during
  those states `max_log` can dip briefly, which `should_reset` used to read as "cursor went
  backwards" and trigger a full re-scan. The engine now short-circuits on `is_migrating` /
  `is_recovering` / `is_backuping_restoring` / `is_booting` and skips the round.

### Added

- `qbox_get_max_log` now parses `is_booting` / `is_migrating` / `is_recovering` /
  `is_backuping_restoring` plus `server_limit` / `cgi_number`, with `MaxLog::busy_reason()` and
  `advised_interval_secs()` helpers.
- **Batch size is now clamped by the server-reported `server_limit`** (measured 256 on-device)
  instead of a hard-coded 200. A hard-coded value gets truncated by the server on models with a
  lower cap, which means "looks like we drained the log" when we did not. When
  `sync_signal == 2` (slowdown), the server's `slowdown_seconds` becomes a backoff hint.
- `qxync status` reports the rate-limit, slowdown and readiness state.
- `Client::qsync_probe()` / `raw_get()`: issue an arbitrary CGI call with raw key/value pairs and
  no parsing or status judgement, for on-device probes (product code should use the typed
  wrappers).

### Measured findings (overriding an earlier reverse-engineering conclusion)

- **`get_meta` / `get_meta_profile` are not usable**; the metadata path for on-demand falls back
  to recursive `get_list`. Measured on a live NAS (QPKG 5.0.0.7 build 20260723): `get_meta` on
  `qsyncsrv.cgi` returns **HTTP 500** (an Apache HTML error page) for all 16 parameter forms
  tried; `get_meta_profile` always returns `status: 19` with no data fields. On
  `filemanager/utilRequest.cgi` both return `status: 20` — and that endpoint returns `status: 20`
  for a **made-up** func too, so they do not exist in the File Station namespace either.
  The 500 is a CGI crash, not a wrong parameter name: a made-up func on the same endpoint returns
  an empty `200`, and `get_tree` / `get_list` work, so dispatch is fine. See
  [`docs/get_meta-实测证伪.md`](docs/get_meta-实测证伪.md).
- Measured `get_list` recursion cost: 11 requests / 17 entries, ~157–430 ms per round trip —
  **latency is dominated by round-trip count**, so the way to speed it up is fewer round trips
  (caching / mapping), not a different endpoint.
- On-demand sync is **not** ruled out: the `{share}/.qsync/meta/` layer generated by
  `qsyncsrv_metad` still exists; it simply has no usable CGI surface.

### Docs

- Added [`docs/get_meta-实测证伪.md`](docs/get_meta-实测证伪.md): the on-device falsification of
  `get_meta`, its evidence, and a regression guard.
- Sanitisation: a real NAS hostname left in a committed document now points at the local
  (uncommitted) test-environment doc instead.

## [0.3.0] - 2026-10-02

**One-to-many removed entirely; one-to-one only**: one mount point = one NAS folder, and the mount
point *is* that folder's content (pair `/home` and you see the home directory directly — no extra
`home/` level). The NAS side became a dropdown (candidates = the Qsync sync folders registered on
the NAS + the home directory); you can still browse level by level or type a path. Destination
conflicts are checked on submit. **The whole "Advanced" section on the Connection page is gone**:
neither `roots` nor `home_root` exists any more. **No compatibility is kept.**

### Removed (breaking)

- **The one-to-many (multi-root) capability is gone**, including every entry point it had:
  * `roots` **and `home_root`** in the link config (the whole "Advanced" section on the Connection
    page is gone; the form is host / port / user / password / https / insecure / ipv4-only). The
    home directory is fixed to `/home` in the Qsync protocol (`qxync_core::HOME_ROOT`), not a
    config field;
  * `qxync mount --remote A --remote B` — `--remote` (and `--root`) can now be given only once;
  * the FUSE **virtual root** (`ViewLayout::Multi` / `RootSpec` / `QxyncFs::new_multi` /
    `multi_root` branches), plus `MountInfo.roots` and `RootsData.configured` / `roots`;
  * `Task.roots: Vec<String>` → `root: Option<String>` (one NAS folder per task).
- **Legacy multi-root task files**: none are preserved. A file with a single `roots` entry is
  migrated to `root` automatically; several entries are reported as a `bad_file` in the task list
  ("old multi-root format — create one task per NAS folder"). The sync scope is **never silently
  narrowed**.
- `xtask/tests/m6-matrix.sh` (29 multi-root items) went away with the feature;
  `docs/M6-多根与共享文件夹.md` is marked historical (the still-valid finding "shared folders are
  readable but not writable" lives on in README §pitfalls 30–33).

### Added

- **"NAS folder" dropdown + "Browse…" picker.** It used to be a textarea (one NAS folder per
  line): you could not see what the NAS actually offers, and nobody should have to type remote
  paths by hand. Now:
  * candidates: Qsync sync folders registered on the NAS
    (`qbox_get_syncing_folder_list&detail=1`), with the share path mapped to the client path
    (`/share/homes/<user>/x` → `/home/x`, backed by a real-machine HAR) + the home directory;
  * "Browse…" walks the tree (reusing the Files page `ls`), and "type manually" is the fallback.
- **GUI static-compliance unit test** (`ui_spec_is_green`, run by `cargo test -p qxync-gui`):
  i18n keys present, no HTML string building, a11y and four-state markers — forgetting an i18n
  key now fails a test.
- **`xtask/tests/pair-1to1.sh` (17 items, no NAS / no FUSE required)**: boots a private daemon
  (fake link, only the `tasks save` path) and verifies one-to-one registration, that a repeated
  `--root` is rejected, the error/warning split for destination conflicts, and that a legacy file
  with one `roots` entry migrates while several entries error out.

### Changed

- **The client no longer pre-judges writability**: the old guard cut everything that was not the
  home directory down to read-only, which was the wrong test — writability is decided by the NAS
  (whether the folder is registered as a Qsync sync folder, see `qbox_get_syncing_folder_list`),
  and registered folders outside home are writable too. Ticking "read-write" now mounts
  read-write; if the server refuses (`status:20`) the upload queue and the Updates / Errors page
  report it verbatim. The GUI dropdown marks "NAS sync folder" candidates and the hint spells out
  this boundary.
- `qxync roots` now lists only "home directory + sync folders registered on the NAS" (no more
  per-root readability probing, no more "configured roots"); each NAS sync folder line prints the
  **client path** (`client_path`), falling back to the share path with an explicit note.

### Fixed

- **`qbox_get_syncing_folder_list` never parsed the real-machine fields.** The NAS returns
  `name` / `path` / `privilege` (2026-10-02 HAR, `detail=1`), but the parser read the item-level
  `folder` / `permission` — so as soon as the NAS really had a sync folder registered, the UI
  showed "empty name + 0 permission" and mistook "listable" for "not listable". Both spellings
  are accepted now (real-machine fields first), with a HAR-verbatim regression test
  (`syncing_folders_real_machine_response_is_not_lost`).
- **The "no NAS folder given" default is now consistent.** The GUI result rows and the `Task` doc
  comment both said "decided by the link's `home_root`", but the daemon used the compile-time
  constant `/home` (the default in `mount()`) — anyone who had changed `home_root` was silently
  mounted at `/home`. Both sides now go through `Task::effective_root()` = the protocol constant
  `/home` (and the `home_root` field itself is gone).
- **Destination conflicts are checked on submit**: a local folder that duplicates or nests inside
  another task's folder is **rejected** with the conflicting task named (nested mounts hide each
  other); the same NAS folder used by another task is a **warning** (read-only mounts of one NAS
  folder are legitimate — `m82-matrix.sh` relies on it for t1/t2), and when both tasks are
  read-write the warning says so explicitly.

## [0.2.3] - 2026-10-02

**The UI is readable now**: the jargon is gone (no more "remote root" on screen — it is called
a **NAS folder** throughout), the two connection fields that ordinary users never touch moved
into a collapsed **Advanced** section, and the home page's "Add task" button no longer sends you
to Diagnostics → Mounts. No breaking changes; config and protocol are untouched — just upgrade.

### Added

- **"Advanced: sync scope" collapsible on the connection page** (the `roots` + `home_root`
  fields). Ordinary accounts never need to fill these in (the default is "home folder only"),
  yet they used to sit naked in the form where users could neither understand nor safely leave
  them alone. They now live in a native `<details>` that is collapsed by default; the input ids
  are unchanged and hidden inputs still round-trip through `link_read` / `link_save`. Expanding
  it shows two plain-language explanations that also name the raw config fields
  (`roots` / `home_root`).

### Fixed

- **The home page's "＋ Add task" button now really adds a task.** Its click handler was still
  the pre-M8.2 form (`switchPage('diag', 'mounts')`) — back then task registration did not exist
  in the GUI and the mount page was the only place to send you. After M8.2 added the Tasks page
  and `openTaskForm()`, and M8.4 folded conflict policy / direction / space-saving into that same
  form, the home shortcut was never updated, so it took a different path from the Tasks page's own
  `#btn-tasks-add`: users who read "add task" and expected to pair a local folder with a NAS
  folder were shown mountpoint / remote root / thread-count instead. Both now take the identical
  path (`switchPage('tasks')` + `openTaskForm()`). The task card's **Manage** button still opens
  Diagnostics → Mounts, which is the right place to inspect or unmount an existing mount.
- **The home page no longer paints the engine's "note" as an error.** When
  `qbox_get_sync_log` keeps returning `status:-17` (this account has never registered a sync
  folder, so there are no events in range), the engine in
  `crates/qxync-daemon/src/sync.rs` already classifies it as `report.note(...)`, not
  `report.error(...)`. But the GUI home page rendered `sy.note` through the same `addAlert()`,
  which had no severity parameter and hardcoded `div.alert`, whose colours are fixed to
  `--err-soft` / `--err` — so a perfectly normal hint sat permanently on the home page as a red
  box, and since the counter increments every round it never went away. The same `note` renders
  as a neutral `<p class="note">` on the Status page, so the red was clearly unintended.
  `addAlert()` now takes a `severity` argument (default `'error'`, real errors stay red) and
  `note` passes `'warn'`, which applies `.alert-warn` (`--warn-soft` / `--warn`).

### Changed

- **UI terminology unified**: "remote root" → **"NAS folder"**, "multi-root" → "multiple
  folders", "view name X" → "called X inside the mountpoint", "owning root" / "root-relative
  path" → "which folder" / "path inside the folder". Places that said just "remote" (the Files
  page title, the context menu's "copy remote path", the conflict policy text) now say "NAS" too
  — the old copy mixed "NAS" and "remote" on the same screen. The raw config field names
  (`roots` / `home_root`) are kept in the hints and in the diagnostics panel's key/value labels
  so you can still match them against the config file or `--json` output; `docs/`, the config
  fields and the CLI keep the technical term.
- The daemon's `roots` note was reworded to match (it also shows up in `qxync roots`; the m6
  matrix only asserts it is non-empty).
- Version → 0.2.3; release notes at `docs/发布说明-v0.2.3.md`.

## [0.2.2] - 2026-10-02

**The daemon now ships a systemd user unit**, plus a fix for a bug that left FUSE mounts behind
on `systemctl stop`.

### Added

- **systemd user unit `qxyncd.service`** (`packaging/systemd/qxyncd.service`, installed into
  `/usr/lib/systemd/user/`): `systemctl --user enable --now qxyncd` means "start at login and
  start now", and `Restart=on-failure` brings it back after a crash; use
  `sudo loginctl enable-linger "$USER"` to keep it running without an active login.
  It is deliberately a **user** unit rather than a system one: qxyncd is *one user's* sync client
  (it uses `$HOME` / `$XDG_RUNTIME_DIR`, and its FUSE mounts belong to that user), so the package
  will **not** enable it — a `qxync-bin.install` hook prints how to enable it and warns not to mix
  `qxync daemon start/stop` with it.
- **The Arch package and the release tarball both carry the unit**; CI's package content check
  now requires it too.

### Fixed

- **`qxyncd` now handles SIGTERM.** That is what `systemctl --user stop/restart` (and
  systemd-logind on logout) sends; previously it was unhandled, so the default action **killed the
  process outright** — leaving FUSE mounts in `/proc/mounts` and the socket/pid files behind. Now
  SIGTERM takes the same graceful path as SIGINT and the IPC `shutdown` request (unmount
  everything → remove socket/pid), and the unit sets `TimeoutStopSec=30` to give that time.
  If registering the handler fails it does **not** panic (a daemonised panic would vanish into
  `/dev/null`) — it logs a warning and simply loses that exit path.

### Changed

- Version → 0.2.2; release notes renamed to `docs/发布说明-v0.2.2.md`.

## [0.2.1] - 2026-10-02

> **This version was never released either** (no tag). The first actual release is **0.2.2**.

**The daemon can now keep running with no NAS configured at all.** `qxyncd` used to require a
link config (`~/.config/qxync/links/<id>.json`) at startup and **exit outright** when it was
missing — so "not logged in / nothing configured yet" meant "the daemon is unusable", which
directly contradicts running it permanently in the background. The GUI even pointed you at
`qxync daemon start`, which failed again.

### Changed

- **New "idle standby" state**: with no connection configured the daemon still starts and stays
  up, serving only `ping` / `status` / `shutdown` plus settings read/write (**plain local files**).
  `status` reports `link: null`, which the GUI already renders as "not configured"
  (`top.conn_none` / `home.conn_no_link`). Requests that need the NAS are **explicitly rejected**
  ("还没有配置 NAS 连接：先 `qxync login`…") instead of being sent to an empty host.
- **Saving a connection takes effect automatically — no restart, no extra command**: the idle
  daemon checks the link file every 2 seconds and, when it appears, switches to sync mode
  **in place** (same process, same pid, no re-exec) and starts syncing.
- **`qxync daemon start` and the GUI's "start daemon" no longer refuse to start without a link
  config**; when it starts unconfigured the CLI says so ("idle standby") and the GUI home page
  shows "not configured".
- **Settings stay readable and writable while idle**: the GUI's settings page (which hosts the
  login form) renders from it and used to show an error first. The `settings_save` disk logic is
  now a shared helper so the idle and sync paths can't drift apart.

### Tests

- Added a **NAS-free regression test** `idle_daemon_serves_without_any_link_config`: it really
  spawns `qxyncd` and really speaks the unix socket, checking "no link still means
  ping/status/settings/clean shutdown" and "NAS-dependent requests are rejected with a readable
  reason". This one runs in CI (the existing case needs real hardware and stays `#[ignore]`d).

## [0.2.0] - 2026-10-02

> **This version was never released on its own** (no tag was pushed). The first actual release is
> **0.2.2**, which contains all of the rename work below — the section is kept because the rename
> deserves its own entry.

**The rename release: the command line is now `qxync`, not `qsync`.** The old name fought the
**official QNAP Qsync client** for the same `/usr/bin/qsync`, and `~/.config/qsync`, the
`QSYNC_*` environment variables and the `user.qsync.*` xattrs all came from the same place.
This release moves every identifier that is **ours** under `qxync`; **QNAP's product names and
the NAS protocol surface are untouched** (the boundary is spelled out below).

### Changed (breaking)

- **CLI binary `qsync` → `qxync`**: `/usr/bin/qsync` no longer exists, so it can't collide with
  the official Qsync client in `PATH`. Subcommands, flags and output are unchanged.
- **Config / data / state directories `…/qsync` → `…/qxync`**: `~/.config/qxync`,
  `~/.local/share/qxync`, `~/.local/state/qxync`. **Migrated automatically on first run**, and no
  case loses data: old directory only → rename (falling back to copy + delete across filesystems);
  **both present** (e.g. a leftover `qxync/` prototype directory from 0.1.0) → **fill the gaps
  only**, moving in entries the new directory lacks and **never overwriting what is already
  there**; new directory only → use it. Credentials and the state database come along, so no new
  `login` is needed.
- **Environment variable prefix `QSYNC_` → `QXNYC_`**: `QXNYC_HOST` / `QXNYC_USER` /
  `QXNYC_PASSWORD` / `QXNYC_SOCKET` plus every `QXNYC_*` tuning and acceptance switch
  (formerly `QSYNC_*`). The old names are no longer read.
- **FUSE xattrs `user.qsync.*` → `user.qxync.*`**: `state` / `pin` / `remote` / `vsize` /
  `chunks`; scripts using `getfattr -n user.qsync.state` must be updated.
- **Download temp suffix `*.qsync-part` → `*.qxync-part`** (`*.qsync-tmp` → `*.qxync-tmp`), and
  the built-in temp-file filter list follows.
- **GUI display name `QSync` → `qxync`**: `productName`, window title, tray tooltip,
  notification titles and the autostart entry's `Name=` are all unified; the Tauri `identifier`
  becomes `org.qxync.qxync-gui` accordingly.
- **Logs and runtime names**: `qsync-gui.log` → `qxync-gui.log`, tray id `qsync-tray` →
  `qxync-tray`, IPC socket directory `$XDG_RUNTIME_DIR/qxync/qxyncd.sock`.
- **Autostart entry** `autostart/qsync.desktop` → `qxync.desktop`: toggling "launch at startup"
  also removes the old one (its `Exec=` actually still worked — only `Name=` was stale), and
  `autostart_present()` counts the old file as active so the UI doesn't lie.

### Where the line is drawn (what did **not** change)

Only **our own** identifiers were renamed; QNAP's product names and the NAS protocol surface are
preserved verbatim: `cgi-bin/qsync/qsyncsrv.cgi` / `qsyncsrv_login.cgi` / `upload.php`, the
`qsync_version` / `Qsync_qpkg_version` / `Qsync_client_version` response fields, `client_app=Qsync`
in the login body, `Qsync QPKG` / `Qsync Client 6` / `.Qsync`, the error codes
`WFM_QSYNC_DISABLED` / `QFILE_ERROR_QSYNC_QPKG_NOT_EXIST`, the NAS setting
`QSYNC_FOLDERPAIR_USE_SPACE_SAVING`, the Windows client registry keys `QSYNC_PROCESSED_MAX_*`,
the probes' `QSYNC` constant and the myQNAPcloud path prefix `qsync/`. The `.desktop`
`GenericName=QNAP Qsync client` and the crates.io `qsync` keyword stay too — they describe what
this interoperates with, not what this project is called.

### Fixed

- **`qxyncd` startup failures are no longer silent.** The daemon daemonizes by default (after
  `fork`, fds 0/1/2 all point at `/dev/null`), so a fatal startup error used to go only to
  `/dev/null` through anyhow — leaving `qxync daemon start` to report nothing but "socket not
  ready", with no way to find out why (this is exactly what bit us here). Three fixes:
  ① the daemon logs its startup failure into `<state>/log/qxyncd.log*`;
  ② `qxync daemon start` brings the log tail back to the foreground when the socket never comes up;
  ③ the CLI checks the link config **before** spawning and, when it is missing, says so and prints
  the `login` command (the GUI's `daemon_start` already had this pre-flight check; the CLI now
  matches it).
- **The GUI's empty-state hint now names the step that is actually missing**: when it cannot reach
  the daemon it no longer always says "run `qxync daemon start`" — with no connection configured at
  all it instead says to save one in "Settings → Connection" (or run `qxync --host … login`).
  On a fresh install the old hint just sent people into the same wall again.
- The daemon's "create config/data/log directories" and "create / restrict the socket directory"
  steps now carry anyhow context, so the log shows exactly where it stopped.

### Other

- **User agent and login body**: `client_agent` went from the hardcoded `QSyncLinux/0.1` to
  `qxync/<real version>` (`CARGO_PKG_VERSION`), so it can't drift again.
- **Frozen historical records keep the old name**: `docs/发布说明-v0.1.0.md`,
  `docs/发布说明-v0.1.1.md`, `docs/发布清单-v0.1.0.md` and the 0.1.x entries below document
  what was **actually published** at the time and are not rewritten; the release checklist gained
  a note saying to translate the old names when following it. Likewise, **external links to QNAP**
  (tutorials / announcements / product pages) and filenames outside this repository are untouched.
- Added unit tests for `adopt_legacy_dir` and the legacy-autostart cleanup (full rename / fill gaps
  without overwriting / re-runnable / old desktop entry removed).
- Quick upgrade table in the README ("Upgrading from 0.1.x").

## [0.1.1] - 2026-10-02

**Only how qxync reaches users changed — client behaviour is identical to v0.1.0**: there are
**no Rust code changes** between `v0.1.0` and `v0.1.1` (`git diff --stat v0.1.0 HEAD -- '*.rs'`
is empty).

### Added

- **Automated release artifacts**: pushing a `v*` tag (or manually running the `Release`
  workflow) builds and publishes the three binaries `qsync` / `qxyncd` / `qxync-gui` — one
  `qxync-<version>-x86_64-unknown-linux-gnu.tar.gz` (desktop entry and icons included), the
  three binaries on their own, and `SHA256SUMS`, all attached to the release for that tag. A
  **version consistency guard** (tag vs. `Cargo.toml` vs. `tauri.conf.json` vs. the AUR
  `PKGBUILD` `pkgver`) blocks publishing on drift, and `xtask/release/package-linux.sh` can be
  re-run locally; the tarball is reproducible (zeroed owner/group, fixed order, gzip without a
  timestamp).
- **The Arch package ships with the release too**: `qxync-bin-<version>-1-x86_64.pkg.tar.zst` is
  built by CI with the **real `makepkg`** inside an `archlinux:base-devel` container, out of the
  very same tarball — the three binaries inside are **byte-for-byte the release assets**, and
  `sudo pacman -U` installs it.
- **Arch Linux package (AUR `qxync-bin`)**: `packaging/arch/` ships an AUR package definition
  that pulls the prebuilt tarball from the release and installs `qsync` / `qxyncd` / `qxync-gui`
  into `/usr/bin`, with a desktop entry and icons. `makepkg -si` installs it locally; once it is
  on the AUR it is `yay -S qxync-bin`. The package is **intentionally not stripped** (same as the
  release artifacts, so backtraces stay readable) and installs ~190 MB.

## [0.1.0] - 2026-10-02

**First public release.** This is not a "it launches" milestone: every milestone went through a full
acceptance run against real hardware. The latest full run is **491 checks passing**
(`fuse 68 · gui 148 · m5 30 · m6 29 · m7 60 · m82 37 · m83 27 · m84 92`); details in
[`docs/验收记录.md`](docs/验收记录.md).

Verified on **QNAP TS-464C / QTS 5.2.9 / Qsync QPKG 5.0.0.7 (build 20260723)**.

### Added

- **Protocol client and CLI (M0)**: login / list / stat / download / upload / mkdir all verified
  against a real NAS; the `qsync` binary exposes
  `login · status · ls · stat · get · put · mkdir`. Protocol findings come from static
  reverse engineering of Qsync for Windows v6.1.0.0831, each finding re-verified against real hardware.
- **Read-only FUSE + on-demand hydration (M1)**: `ls -l` shows real sizes with zero download; data
  is fetched on the first `read()`; `user.qsync.*` xattrs expose placeholder state; an
  incomplete read returns `EIO` — never a short read.
- **Daemon and local IPC (M1.5)**: the `qxyncd` binary becomes the **only** process holding the
  FUSE mount and the NAS session; unix socket with one JSON object per line; `qsync` auto-routes to
  it whenever the socket is reachable.
- **128 KiB range hydration (M2a)**: `head -c 100 big.bin` downloads a single range instead of the
  whole file.
- **Write path (M2b)**: `--rw` mounts; upload queue with dirty-marker crash recovery; mandatory
  **read-modify-write** before upload so never-fetched ranges are never uploaded as zeros.
- **Change discovery (M2c)**: three-cursor polling plus baseline three-way reconciliation (events
  are only a fast path); **conflicted copies** when both sides changed; a **circuit breaker** for
  mass remote deletions.
- **Dehydration / free up space (M3)**: a full safety chain (pins, pending uploads, open fds,
  mmap via `/proc/*/maps`, in-flight hydration, recent access) before any local content is cleared;
  the `inval_inode` ordering rule; idle/limit LRU; `--cache-mode pagecache|direct`.
- **Desktop GUI (M4)**: a Tauri 2 app whose frontend is a **dependency-free static trio**
  (no npm): login config / mount management / status & progress / pin management.
- **SQLite state store + delta (M5)**: `sync.db` holds cursors / baseline / pins / upload queue,
  committed in a **single transaction**; legacy JSON state auto-migrates and is archived; librsync
  native-format sign/delta/patch plus capability gating.
- **Multi-root / shared folders (M6)**: `roots` in the link config; each root appears as a
  top-level name in the mountpoint; the home root is writable, non-home roots return `EROFS` at the
  FUSE layer.
- **Selective sync (M7)**: a gitignore-flavoured `exclude` rule engine (anchoring / `*.iso` at any
  depth / subtree pruning / negative patterns / `**`) plus built-in temp-file filtering, wired
  through FUSE, the sync engine and dehydration candidates.
- **LAN direct (M7)**: a qxync-to-qxync peer protocol — pairing, event fast path, local range
  transfer; not listening by default, and any failure/mismatch/timeout silently falls back to the NAS.
- **Sync tasks (M8.2)**: `tasks/<id>.json` persistent registry, per-task pause/resume,
  `qxyncd --restore-tasks` on restart (off by default).
- **Sync journal (M8.3)**: `sync.db` schema v3 adds a `journal` table with background batch inserts
  and rotation, driving the GUI's "file update centre / error list".
- **Settings centre (M8.4)**: three proxy modes (Auto-detect / No proxy / Manual, wired into
  reqwest and verified with a real `CONNECT`), autostart, desktop notifications, a ksni tray with a
  "is it actually visible" probe, `statvfs`-driven automatic free-up-space (reusing the dehydration
  safety chain), five conflict strategies, and the three file states.
- **Polish and accessibility (M8.6)**: design tokens (light/dark), keyboard accessibility and focus
  rings, an audit of the empty/error/loading states across the UI, an i18n message table
  (zh-CN 163 entries + a reserved `en` table), and the `ui_spec` static self-check in
  `qxync-gui --self-test`.

### Fixed

- A failed mount no longer panics the daemon's IPC worker: dropping `QxyncFs`'s tokio `Runtime`
  inside an async context triggered
  `Cannot drop a runtime in a context where blocking is not allowed`. It now uses a `FsRuntime`
  that picks its shutdown path by context (`shutdown_background()` under tokio), with a permanent
  regression test, `dropping_fs_inside_async_context_does_not_panic`.

### Security

- **The repository contains no real hostnames, accounts, local paths or device fingerprints** —
  everything was sanitized before release; the mapping and verification method are in
  [`docs/发布清单-v0.1.0.md`](docs/发布清单-v0.1.0.md).
- The repository **distributes no** QNAP binaries, installers or decompilation output; third-party
  credentials in the report are masked.
- Credentials file is `0600`; IPC socket directory `0700` / socket `0600`.
- **No telemetry of any kind**; nothing is contacted other than the NAS you configured and an
  optional LAN peer.

### Documentation

- Bilingual `README.md` / `README.en.md`.
- Added [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md) and this changelog
  (in both languages).
- Acceptance results were split out of the README into [`docs/验收记录.md`](docs/验收记录.md).
- Dual-licensed under **MIT OR Apache-2.0** (`LICENSE-MIT` / `LICENSE-APACHE`).

[0.4.1]: https://github.com/mlzxgzy/qxync/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/mlzxgzy/qxync/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mlzxgzy/qxync/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/mlzxgzy/qxync/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/mlzxgzy/qxync/releases/tag/v0.2.2
[0.1.1]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.1
[0.1.0]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.0
