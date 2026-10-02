# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[中文](CHANGELOG.md) · [README](README.en.md) · [Acceptance log](docs/验收记录.md)

## [Unreleased]

**Paired folders are one-to-one now**: one local folder ⇄ one NAS folder, and the mount point
*is* that NAS folder (no extra `home/` level inside it). The NAS side became a dropdown whose
candidates come from the Qsync sync folders registered on the NAS plus the configured NAS
folders; you can still browse level by level or type a path. Destination conflicts are checked
on submit.

### Added

- **"NAS folder" dropdown + "Browse…" picker.** It used to be a textarea (one NAS folder per
  line): you could not see what the NAS actually offers, and nobody should have to type remote
  paths by hand. Now:
  * candidate source 1: Qsync sync folders registered on the NAS
    (`qbox_get_syncing_folder_list&detail=1`), with the share path mapped to the client path
    (`/share/homes/<user>/x` → `/home/x`, backed by a real-machine HAR);
  * candidate source 2: the NAS folders already configured under
    Connection → Advanced → sync scope;
  * "Browse…" walks the tree (reusing the Files page `ls`), and "type manually" is the fallback.
- **GUI static-compliance unit test** (`ui_spec_is_green`, run by `cargo test -p qxync-gui`):
  i18n keys present, no HTML string building, a11y and four-state markers — forgetting an i18n
  key now fails a test.
- **`xtask/tests/pair-1to1.sh` (15 items, no NAS / no FUSE required)**: boots a private daemon
  (fake link, only the `tasks save` path) and verifies the one-to-one rejection of multiple
  roots, the error/warning split for destination conflicts, and that legacy multi-root tasks
  still load.

### Changed

- **A paired folder is a one-to-one mapping.** A task accepts exactly **one** NAS folder.
  Multi-root mounts (`qxync mount --remote /home --remote /Public`, M6) still work but are
  **no longer registered as tasks** — the task layer only expresses "one local folder ⇄ one NAS
  folder". Existing multi-root task files still load, mount and pause (an upgrade never turns a
  task into a broken file); the UI warns about them and one save collapses them to one folder.
- `qxync roots` now prints the **client path** (`client_path`) for each NAS sync folder, falling
  back to the share path with an explicit note when it cannot be mapped.

### Fixed

- **`qbox_get_syncing_folder_list` never parsed the real-machine fields.** The NAS returns
  `name` / `path` / `privilege` (2026-10-02 HAR, `detail=1`), but the parser read the item-level
  `folder` / `permission` — so as soon as the NAS really had a sync folder registered, the UI
  showed "empty name + 0 permission" and mistook "listable" for "not listable". Both spellings
  are accepted now (real-machine fields first), with a HAR-verbatim regression test
  (`syncing_folders_real_machine_response_is_not_lost`).
- **An empty task `roots` did not actually follow the link's `home_root`.** The GUI result rows
  and the `Task` doc comment both said "decided by the link", but the daemon used the
  compile-time constant `/home` (the default in `mount()`); anyone who had changed `home_root`
  was silently mounted at `/home`. It now really uses `link.home_root`.
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

[0.2.3]: https://github.com/mlzxgzy/qxync/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/mlzxgzy/qxync/releases/tag/v0.2.2
[0.1.1]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.1
[0.1.0]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.0
