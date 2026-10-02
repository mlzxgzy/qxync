# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[中文](CHANGELOG.md) · [README](README.en.md) · [Acceptance log](docs/验收记录.md)

## [Unreleased]

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

[Unreleased]: https://github.com/mlzxgzy/qxync/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.1
[0.1.0]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.0
