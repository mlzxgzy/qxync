# Changelog

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

[中文](CHANGELOG.md) · [README](README.en.md) · [Acceptance log](docs/验收记录.md)

## [Unreleased]

### Fixed

- **`qxync daemon stop` looked like it could not stop when the mount point was in use**:
  stopping first drains the upload / delete queues and then unmounts (each queue may wait
  up to 120 s), but the old CLI only watched the socket, printed "stopped" after 10 s and
  left the process running; when `fusermount -u` hit "a process is using it", it only
  returned `Device or resource busy`, so the user could not tell *who* was holding it.
  On a real machine (2026-10-19 12:48:40 → 12:50:40) one pending upload (`HTML 文件.html`)
  made `daemon stop` hang for the full 120 s while the CLI had already reported success.
  Now:
  * before stopping, scan `/proc` (process cwd / root / open files / mmap, plus any
    sub-mounts under the mount point); if anything is in use, **list pid / program / path
    and refuse to stop** until those programs are closed;
    `qxync daemon stop --force` still forces it (open files under the mount point stop
    working immediately);
  * wait for the **process to actually exit** (not just for the socket), and say what it
    is waiting for while the queues drain;
  * `qxync umount` uses the same check: while in use it reports *who* instead of a bare
    `Device or resource busy`; the daemon also logs the holders when an unmount fails.

## [0.6.6] - 2026-10-19

### Fixed

- **The event fast path broke entirely when the view root is a subdirectory of home
  (every event returned `status=5`)**: with a task pointing at a folder inside the home
  directory (`root=/home/.Qsync` on a real machine, i.e. NAS `~/.Qsync`), `map_event_path`
  stripped `/share/homes/<user>` from the real path and then appended the remainder
  (`/.Qsync/…`) to the view root `/home/.Qsync` again → `/home/.Qsync/.Qsync/…`
  (`.Qsync` twice). Every event's `stat` therefore returned `status=5 (no such path / no
  permission)`: the fast path did nothing (all remote changes had to wait for a baseline
  reconcile round) and the "recent errors" list drowned in the same message (real-machine
  log: the 2026-10-18 17:31 round **pulled 87,580 events and skipped 8,017**; that day's
  daemon log has **79,670 `stat` failures for events, 100% of them with the doubled
  `/home/.Qsync/.Qsync/...` prefix** (79,667 `status=5`, 3 network errors). Event paths now
  go through `client_path_from_share` into the client
  namespace (always rooted at `/home`) and are then narrowed to the view root — events
  outside this view return `None` and are left to the reconciler instead of being turned
  into non-existent paths. Also: `stat` 4/5/6 on an event is no longer reported as a hard
  error (those codes also mean "permission denied / session lost", so they never were
  errors in themselves).
- **Phantom conflict copies: an upload of our own that landed mid-round was judged against
  the round-start baseline snapshot**: `poll_once` clones the baseline at the start of a
  round and uses it for the whole round (tens of seconds); the upload-success hook, however,
  advances the authoritative baseline *during* that round and calls `clear_dirty`. So when
  judging a file the user had just saved: the snapshot still had the old signature, the
  remote already had our own freshly uploaded version and the local node was clean →
  `decide` took the `(dirty=false, remote≠base)` branch and, because the local signature ≠
  the stale base, returned **Conflict** - and the then-effective conflict policy produced a
  copy that was **byte-for-byte identical** to the original (2026-10-19 08:06:
  `防火墙配置 (conflicted copy from <hostname> 2026-10-19).txt`, 12,218 = 12,218 bytes). The round
  now also clears a "hook advances seen this round" table, and every decision (reconcile /
  events / conflict re-judgement) uses "snapshot ∪ this round's hook advances" as the base;
  multiple uploads within one round are matched as a **list** (a directory listing can fall
  between two of them); when the round's listing happened before the upload landed (remote
  still matches the snapshot), the snapshot wins so local metadata is not refreshed back to
  the old version. Two more fixes came out of review: (1) `persist` runs several times per
  round (once per event batch plus once at the end) - it no longer clears `hook_touched`
  after the first merge, otherwise the second full-replace persist wiped the hook's advance
  on disk and the same phantom conflict reappeared one round later; (2) the "remote is older
  than this round's last advance" shortcut now only applies when the local node is clean -
  with unuploaded local changes it falls back to the normal decision, otherwise a
  freshly-written-but-old-mtime remote (slow clock / `rsync -t` / backup restore) would be
  overwritten by the local copy, losing data. Covered by a regression test reproducing the
  real timeline (old logic always says Conflict, new logic says Noop), a repeated-persist
  regression, an end-to-end test through `reconcile_view`, and a round-lifecycle test.
- **A conflict policy changed on the task page never reached the mounted view, and was
  overwritten by `mounts.json` after a restart**: the sync engine dispatches conflicts from
  `MountEntry.conflict` (the view does not read the task file), while `mounts.json` records
  "what was actually mounted" and wins on restore. On a real machine this meant the task said
  `ask` while the engine kept using `rename_local` - so a conflict that should have queued
  for a decision produced a conflict copy instead. Now: (1) saving a task pushes the new
  policy to the mounted view with the same mountpoint and persists it into the record;
  (2) on startup restore, a task with the same mountpoint wins for the conflict policy (all
  other mount parameters still follow the record). Skipped events are now also counted by
  reason and written into the `remote_change` journal detail (delete event without path /
  own-device echo / other account / hidden by rules / not visible remotely / mapping failed)
  so "the event fast path died" does not silently disappear again.

## [0.6.5] - 2026-10-18

### Added

- **Debian / Ubuntu package**: the release now ships `qxync_<version>-1_amd64.deb`, **one
  package for Debian 12 / 13 and Ubuntu 22.04 / 24.04 / 26.04** (amd64). Its three binaries
  are byte-for-byte the ones in this release's tarball (repackaged, not rebuilt); `Depends`
  comes from `dpkg-shlibdeps` run against the binaries plus a hand-added `fuse3`
  (11 entries, all standard packages from the official repositories). The release pipeline
  installs the package with `apt install` in clean Debian 12 / 13 and Ubuntu 22.04 / 24.04 /
  26.04 containers and only publishes when every assertion passes.
- **Packaging scripts**: `xtask/release/package-deb.sh` (builds the `.deb` from the release
  tarball inside an `ubuntu:22.04` / `debian:12` baseline container) and
  `xtask/release/apt-mirror.sh` (switches apt to a mainland-China mirror inside containers,
  Tsinghua by default). Maintainer notes: `packaging/debian/README.md`.
- **`docs/协议要点.md`** (Chinese): the protocol-level pitfalls that used to live in the
  README now live in a dedicated developer document; entry numbers are unchanged so existing
  code comments still match.

### Changed

- **README rewritten for users** (Chinese / English kept in sync): what it is, features,
  screenshots, installation (Arch, Debian/Ubuntu, tarball, from source), signing in, the
  long-running daemon, multiple NAS devices, mounting and dehydration, common commands,
  upgrading, known limitations, security & privacy, contributing. Maintainer-facing
  architecture notes, implementation details and the acceptance matrix moved to `docs/` and
  `CONTRIBUTING.md`.
- **Release pipeline**: 7 release assets instead of 6 (the shared `.deb` is new). Two new
  steps after the Arch package check: build the `.deb`, then install it in a five-distro
  clean-container matrix; the `.deb` is also listed in `SHA256SUMS`.

### Notes

- **No sync-logic changes in this release**: `git diff v0.6.4..v0.6.5 -- crates/` is empty;
  0.6.5 changes no data formats, configuration keys or CLI options. Upgrading from 0.6.4 is
  optional.

## [0.6.4] - 2026-10-18

### Fixed

- **Deletion verdicts were postponed forever once the directory count exceeded the
  per-poll limit**: the daemon kept repeating "228 directories exceed the per-poll limit of
  200… skipping the deletion verdict this round" for 87 consecutive rounds. The cause: each
  reconciliation listed only the first 200 directories in sorted order, so the trailing ones
  were **never** reached; the safety valve "incomplete remote view → never delete locals by
  absence" therefore stayed on permanently, and remote deletions (plus upload/refresh
  decisions for those directories) never ran. Data safety was intact, but the delete half of
  sync stalled. Now reconciliation uses a **rotating scan window + per-directory verdicts**:
  - when the directory count exceeds the limit, each round resumes after the directory where
    the previous round stopped and wraps around (the remote root is always listed); the cursor
    is stored in the `meta` table of `sync.db`, so `qxync sync --once` keeps rotating across
    processes;
  - "incomplete view" is narrowed from the whole view to **per directory**: only candidates
    whose parent directory was actually listed this round (a successful list, or 4/5/6
    confirmed by `stat` with `exist=0`) may be decided. Candidates under directories that were
    not listed are deferred (never deleted by absence), while the other fully-listed
    directories in the same view are **not held back**;
  - rename pairing likewise only uses removed-side candidates whose parent was listed this
    round — "not seen" is never paired into a rename;
  - coverage: unit tests for the rotation window plus a no-real-NAS mock `reconcile_view`
    regression over two rounds (228 directories with one remote deletion in each of d000 and
    d210: round 1 deletes only inside its window, round 2 rotates to the trailing directories,
    nothing outside the window is deleted, and the two windows cover every directory), plus a
    "one failing directory only defers itself" test.
- **`status 4/5` is no longer treated as "the remote deleted it"**: 4/5 are also the
  permission-denied / session-expired codes (on real hardware `/Download` returns 5 and
  `/homes` returns 4 even though they exist). The old code treated 4/5 as a definite
  "not found", so a permission/ACL change or a mid-round sid expiry would turn every
  candidate under that directory into `DeleteLocal` (local node + cache + baseline removed)
  without triggering a re-login. Now a 4/5/6 listing is **confirmed with `stat` first**: only
  `exist=0` allows the "remote deleted" verdict; if `stat` says it still exists the
  **directory entry itself** is inserted into the remote view (so a parent listing that
  omitted it cannot get the whole subtree + cache deleted) while its contents are not decided
  this round; if `stat` also fails the directory is presence-unknown and neither it nor its
  contents are decided. A successful `stat` proves the session is alive, so `auth_failed` is
  **not** set (otherwise a permanently ACL-hidden directory would re-login and warn every
  round); only a failing-`stat` auth error or a root 4/5 does. The mount root is the
  exception: it cannot vanish, so 4/5/6 there is always treated as a
  permission/session/unmount problem (no `stat`, no deletion).
  Regression: `listing_status_4_or_5_never_deletes_without_stat_confirmation`,
  `listing_root_status_4_or_5_defers_instead_of_deleting`,
  `directory_missing_from_parent_listing_but_stat_exists_is_not_deleted`,
  `stat_failure_keeps_directory_and_subtree`.
- **HTTP-layer failures are no longer read as "empty directory / missing path"**:
  `get_list` / `stat` now check the HTTP status first — a reverse-proxy 502/504 JSON error
  body (`{"error":…}`) used to pass `Listing`'s all-defaults deserialization and be read as
  "0 entries", which the reconciler then used to delete locals. Even with HTTP 200, a JSON
  body that does not look like a listing (none of `status`/`datas`/`total`/`real_total`/
  `exist`) is now a parse failure. `list_limit` also no longer treats a missing `total` as
  `total=0` and stops after the first page (directories with >200 entries were silently
  truncated); without a trustworthy `total` it converges through pagination instead.

## [0.6.3] - 2026-10-18

### Added

- **Delete related data (M19)**: previously "delete task registration" and "remove connection"
  deliberately left every byte of data behind — hydration cache, `sync/<ns>/sync.db`
  (baseline / journal / decisions), `upload-queue/<ns>/` and logs stayed on disk with no way
  out of the UI. Three scopes can now clean them up:
  - **task**: registration + that task's hydration cache + `baseline` / `journal` / `decisions`
    rows under its NAS folder subtree (unmounts first; per-connection queues / pins / nodes are
    left alone);
  - **connection**: task registrations + link / credentials / peers + `sync/<ns>` +
    `cache/<ns>` + `upload-queue/<ns>` + mount records + that connection's logs + socket / pid;
  - **everything**: the config / data / state roots (factory reset), stopping all daemons first.
  - **No scope ever deletes NAS data or files inside the mountpoint**; shared caches / shared
    NAS folders are skipped with a warning instead of guessed; symlinks are removed as links only.
  - GUI: task-card "Delete…", connection-row "Remove", Settings → Advanced → **Danger zone**
    (must tick the acknowledgement); all three share one confirmation dialog that first lists
    what would be deleted and how big it is, with the data checkbox **off by default**.
  - CLI: `qxync purge --task <id>` / `--connection` / `--all`, **preview by default**,
    `--yes` to execute, `--json` for scripts.
  - New `qxync-gui --self-test-purge`, `xtask/tests/m19-purge.sh` (isolated-XDG e2e, wired into
    CI) and core unit tests; `link_delete` gained an optional `purgeData` argument and behaves
    exactly as before when omitted.
  - Design notes and boundaries: [M19-删除相关数据.md](docs/M19-删除相关数据.md) (Chinese).

- **New-task default paths are derived from the connection and the task id (M18)**: previously
  the task id, local folder and hydration cache dir all had the *same* default for every task
  (`default` / `~/qxync-mnt` / the per-connection cache root), so the second task always collided
  on the id or the mountpoint and you had to edit three fields by hand. Now:
  - the task id is taken from the NAS folder name (`/home` → `home`, suffixed `-2` on collision);
  - the local folder is `~/qxync/<connection id>/<task id>`;
  - the hydration cache is `<data_dir>/cache/tasks/<connection id>/<task id>` (a **parent**
    directory; the daemon still appends the M16 per-connection namespace on disk);
  - the form asks for the NAS folder first; changing it (or the connection) re-derives the rest,
    **fields you edited by hand are never overwritten** (use "reset to default paths" to restore),
    and editing an existing task derives nothing;
  - saving is blocked when the id is empty, illegal, or already used by another task, so a new
    task can no longer silently overwrite one;
  - no existing task is moved, and the task file format gains **no new fields** (downgrading is
    compatible). See [M18-任务默认路径.md](docs/M18-任务默认路径.md) (Chinese).

### Fixed

- **Creating a new task could silently overwrite another one**: `tasks/<id>.json` is a globally
  unique file name, while the GUI's new-task form always defaulted the id to `default` — creating
  a second task on the same connection replaced the first task's record while the mount table
  still used the old task's root (record and mount disagree, invisibly). The GUI now rejects a
  duplicate id before saving (editing an existing task is unaffected).

## [0.6.2] - 2026-10-18

### Added

- **Two-step verification (2SV / TOTP) support (M17)**: once an account enabled two-step
  verification in QTS, qxync **could never log in** — the *first* `authLogin.cgi` form decides
  whether a sid is issued, and qxync only sent `user + pwd(base64)`, so the server answered
  `authPassed=0 / need_2sv=1` and every later endpoint had no sid. The real-NAS conclusion
  (2026-10-18): **add `security_code=<6-digit TOTP>` to that same form and the response is
  `authPassed=1` + `authSid`**; nothing else changes. The implementation follows the QTS Web UI's
  two-step flow: **the first request is byte-for-byte identical to v0.6.1**, and the second one
  (carrying `security_code`) is only sent when the NAS answers `need_2sv=1` — so "accounts
  without 2SV are unchanged" is structural rather than relying on the server ignoring unknown
  fields. Implementation:
  - a pure-Rust built-in TOTP (RFC 6238: SHA-1 / 30 s / 6 digits; Base32 secrets; a pasted
    `otpauth://` URI works too) with **no new dependencies**;
  - the credentials file gains an optional `otp_secret` (same file as the password, `0600`,
    one per connection; old files need **zero migration** — a missing field means "2SV not
    configured");
  - CLI globals `--otp-secret` (or `QXNYC_OTP_SECRET`, saved with the credentials) and
    `--otp-code` (or `QXNYC_OTP_CODE`, one-shot and never saved);
  - the daemon **computes a fresh code on every re-login** (only the secret is stored, codes are
    never cached), so long-running mounts do not drop when a code expires; `Request::Login`
    accepts a one-shot `otp`;
  - the GUI's Settings → Connections gains a "two-step verification (optional)" field (a secret is
    saved; a 6-digit code is one-shot); editing a connection shows "configured", the login result
    shows whether 2SV passed, and the secret is **never echoed back**;
  - design and protocol evidence: [M17-两步验证.md](docs/M17-两步验证.md).

### Fixed

- **Login errors for 2SV accounts are no longer misleading**: an `authPassed=0` used to always
  append "check the password encoding: it must be base64", which is wrong when two-step
  verification is enabled. The two branches are now distinct: "the account needs 2SV but no
  second factor was supplied" → tell the user about `--otp-code` / `--otp-secret` (and where to
  find them in the GUI); "supplied but rejected" → mention that the code may have expired
  (30-second window) or that the local clock may be skewed. Neither the message nor the logs
  ever contain the secret or the code.

### Changed

- **Accounts without 2SV are byte-for-byte unchanged**: the login body carries no
  `security_code` and the credentials file gains no field.
- Old credentials files stay readable (no `otp_secret` = not configured); an old version reading
  a new file just ignores the extra field.
- `IPC_VERSION` is **not bumped**: `Request::Login.otp` and `LoginData.two_step` both use
  `serde(default)`, so old and new processes can run side by side (same reasoning as M16).
- Rolling back to v0.6.1: 2SV accounts stop logging in again; everything else is unaffected.

### Known limitations (not fixed in this batch)

- **TOTP only**; QNAP's security questions / email codes / QR-code approval / device approval are
  **not implemented**.
- Depends on the local clock; `otp_secret` is plaintext in a `0600` file just like the password
  (keyring support is future work).
- Not measured on the protocol side: `dont_verify_2sv_again=1` ("remember this device") and
  account lockout / rate limiting after **repeated** wrong codes (only a single wrong code was
  tested: rejected, no lockout).

## [0.6.1] - 2026-10-18

### Added

- **"Add connection" is no longer one hidden button**: the "＋" next to the current-connection
  picker in the top bar, "＋ 添加连接" on the home connection card, and "＋ 新建连接…" at the
  end of the task form's "NAS connection" dropdown all jump straight to the new-connection
  form under Settings → Connections (switch to that section, scroll the form into view,
  focus "host / NAS address", and leave a line in the log). Previously the only entry was
  "＋ 添加 NAS" in the Settings → Connections card; neither the home page nor the task page
  had one, and the task form's dropdown lists **existing** connections only — so "connect a
  NAS that is not configured yet" had nowhere to click. See [M16 §7.1](docs/M16-多NAS连接.md).

### Fixed

- **Renaming/deleting no longer "drains the entire upload queue first"** (saving a large
  file / text in an editor drops from 1–2 s back to tens of milliseconds). Previously FUSE
  `rename`/`unlink` called `drain()` and waited for the **whole** upload queue to empty (up
  to 60 s / 30 s) before talking to the server whenever the target had local changes; an
  editor's atomic save (KIO/Kate: write `.part` → rename) hits exactly that path and had to
  wait for `.part`, swap files and every other upload in the same directory (measured
  1.5–1.9 s per save). Now:
  - only jobs on the **relevant paths** are frozen and awaited; queued jobs are retargeted to
    the new name (`UploadQueue::retarget_pending_under`, state-store primary key rekeyed)
    instead of uploading the old name first;
  - nodes gain `remote_known` (whether the remote actually has this copy): renaming/deleting
    a locally created file (KIO's `.part`, temp files) **never hits the server** — it is
    handled purely locally and the upload lands under the final name; when the remote has an
    older copy, behaviour is unchanged (rename on the remote first, then update the local
    view; on failure EIO and the queue stays intact);
  - the upload queue gains a 250 ms enqueue debounce: consecutive writes to the same path
    collapse into one upload, so an editor's temp-file-then-rename does not push 0-byte or
    intermediate versions to the NAS;
  - deleting a file that was never uploaded (closing a document deletes its swap file) no
    longer "uploads first, then deletes" either.
  - Regression: 6 new unit tests (path-level wait / retarget / debounce / pause-resume /
    `remote_known` lifecycle); `cargo test --workspace` is green.
- **The new-connection form is no longer filled in by the current connection's async reply**:
  after "＋ 添加 NAS" cleared the form, the `loadConnect()` reply (which reads the **current**
  connection) put the previous NAS's host / path back into the form — the title said "Add NAS
  connection" while the address belonged to someone else, and saving it would have created a
  wrong connection. A `creatingLink` flag now distinguishes "showing the current connection"
  from "creating a new one"; in the creating state that reply leaves the form alone.
- **"Connection settings" on the home page no longer lands on the last settings tab**: if you
  last looked at "About", clicking it dropped you on the About page and the connection list
  never showed. It is now pinned to "Connections".

## [0.6.0] - 2026-10-18

### Added

- **Multiple NAS connections (M16)**: one `qxyncd` process per connection, so several NAS
  boxes can be connected at the same time and each connection can host several tasks
  (one local folder ⇄ one NAS folder). Highlights:
  - each connection gets its **own** socket / pid / log / state store / cache / upload queue;
    **stopping one connection leaves the others untouched**; the `default` connection keeps
    the legacy paths and **upgrades with zero migration**;
  - data is namespaced per connection: `default` uses `<host>` (the legacy directory names),
    every other connection uses `#<id>@<host>` (same rule for `sync/`, `cache/`,
    `upload-queue/` and `mounts/`); non-default credentials live in
    `links/<id>.credentials.json`;
  - tasks carry an optional `link_id`; ownership of legacy tasks is **deterministic**
    (`default` first, otherwise the lexicographically smallest connection); reusing a task id
    across connections is **refused**; local mount-point conflicts are **refused across
    connections**, NAS-root overlaps only warn within the same connection;
  - migration: `default` only moves the upload queue; for non-default connections the legacy
    owner moves sync / cache / queue / mounts / credentials (sync / queue failures **refuse to
    start**, cache failures degrade with a warning);
  - CLI: `qxync links [--json]` (all connections plus each daemon / credential state), a global
    `--link` routing flag, and `qxync daemon start|stop|status|reload --all`;
  - GUI: connection list (add / edit / remove), a "current connection" switcher, aggregated
    tasks, the files page following the current connection, settings broadcast to all
    connections, and tray "start all / stop all";
  - removing a connection **only deletes registrations** (link / credentials / peers / task
    records) — **never** local files, NAS data, cache or the state store; a duplicate
    `(host, port, user)` is refused;
  - new `qxync-gui --self-test-multi` self-check (two fake connections, two independent
    sockets, task isolation, cross-connection overwrite refused, everything stopped cleanly,
    all exit codes 0); see [M16-多NAS连接.md](docs/M16-多NAS连接.md).
- **The conflict decision queue is now something you can actually inspect and act on.**
  Under the "ask me for every file" policy the queue only had three resolve buttons
  (keep local / keep NAS / keep both): you could not *see* what differed, and there was no
  way to say "I don't care about this one".
  - **Compare local vs NAS** in the GUI and the CLI: a line-level text diff
    (`-` = exists only locally, `+` = exists only on the NAS). Oversized (512 KiB) files,
    binaries, directories and unreadable sides now show an explicit reason — the UI never
    silently claims "no differences".
  - **Ignore this one** (queue row only; no file is touched), plus **Clear all** in the GUI
    and `--clear` in the CLI.
  - **Queue sweep in the engine**: rows whose remote file is gone, whose task is gone,
    whose policy is no longer `ask`, or whose conflict resolved itself are dropped
    automatically with a human-readable reason (queue rows only).
- **The sync engine no longer manages recycle-bin subtrees**: `@Recycle`, `.Trash` and
  `.Trash-<uid>` are excluded from reconciliation, baseline, rename pairing, event
  handling, the decision queue and the upload queue (shared segment rules live in
  `qxync-core::trash`, matching FUSE's existing `deny_trash`). Real-device evidence:
  1425 of 1991 baseline entries were under `/home/@Recycle` and 13 under `.Trash-1000`
  (FDO trashinfo had even been synced back to the NAS); QTS purging its recycle bin
  produced mass phantom deletions.

### Fixed

- **The GUI said "some deletes are blocked by the circuit breaker" but gave you no way to
  act on it** (user report). This was a state with a pointer but no destination: the task
  card read "有删除被熔断挡住（待确认）" and the home page added "到「诊断 → 同步 / 缓存」
  可强制放行一轮" — but the only button over there is called "**强制放行删除**", which
  corresponds to nothing in the message. Readers had no way to know which one to click and
  ended up searching the whole UI.
  1. `addAlert()` takes an optional `action` argument, so an alert row can carry its own
     button. **Any state you must still act on after seeing it now carries its own exit**,
     instead of sending you hunting.
  2. All three alerts (home "last sync", diagnostics → status, diagnostics → sync/cache)
     embed a "放行删除" button.
  3. The task card's action row grows a "放行删除" button while the breaker is tripped —
     and only while the state line is actually talking about the breaker. When the card's
     own "paused / not mounted" state overrides the global one, the button steps aside too,
     so you never see "state says paused" next to a release-deletes button (a button that
     really does delete things).
  4. The vague "待确认" is gone, replaced by the plain fact "**这轮没有真的删除任何文件**".
     What the breaker holds back is a deletion that was *about to run*; the thing users
     actually fear — files already gone — never happens, and the UI never said so.
  5. All three entry points now funnel into one `doForceDeletes()`: it asks for
     confirmation and **shows the daemon's original reason in the prompt** ("一次对账要
     删除 812 项，超过上限 50" carries far more information than "确认放行？"), plus a
     warning not to release if the remote was emptied by accident. Deletion is
     irreversible; this step cannot be skipped.
- **The conflict diff panel always showed "local +0 / -0 / identical"** (a dangerous
  illusion): serde flattens `DiffOutcome` fields next to `status` on the wire, but the
  frontend read a nested `o.diff`, and its `{}` fallback hid the mistake — "never compared"
  and "zero differences" looked exactly the same. Now it reads the flat shape, validates
  every line, and shows an error instead of ever claiming "identical".
- **Diff direction was inverted** (GUI legend and CLI): in the real `text_diff`, `-` = local
  only (`removed`) and `+` = NAS only (`added`). An inverted legend makes users pick the
  wrong "keep" side, which cannot be undone. Core docs, the wire-contract fixture, the GUI
  legend and the CLI output are now aligned (pinned by tests using the production function).
- **"Discovered at" always showed the current time**: `clock()` dropped numeric timestamps
  and fell back to `new Date()`; the same bug affected "last trigger" and LAN event times.
- **Busy/disabled state for dynamic buttons never worked**: `withBusy` looked buttons up by
  `id`, but card buttons are created dynamically without ids, so repeated clicks sent
  duplicate resolve/forget/compare requests. It now accepts element references; the diff
  toggle is disabled while a request is in flight.
- **Diagnostics displayed cumulative counters as "current"**: `deletes_blocked` is
  cumulative (692 742 on the real device); it is now labelled "(cumulative)" and no longer
  styled as an error. "Currently tripped" is driven only by `delete_block_reason`.
- **Saving the connection page silently wiped selective-sync / LAN-peer settings**:
  `link_save` persisted `LinkInfo::link()` directly while the form only submits
  host/port/https/insecure/user/ipv4, resetting `exclude` / `filter_temp` / `peer_*`.
  It now uses `link_with_existing()`, like `login_flow` already did.
- **`login_flow` ignored `already` on the restart path**: when another process had already
  started the daemon (`{started:false, already:true}`), it reported failure and skipped login.
- **`LinkInfo` lacked `insecure`**: the GUI could not tell that "skip TLS verification" had
  changed, so the daemon kept the old setting. The field is added and compared in
  `same_link`.
- **The same 526 blocked deletions produced a WARN + journal entry every 30 s for 13 hours**
  (3023 of 3046 journal rows were the same sentence; the 10 000-row cap was being flushed
  within days): blocked rounds are now de-duplicated by a "count + reason" signature
  (unchanged signature drops to debug); `ask` conflicts are counted/logged only on first
  enqueue (it used to claim "generated N conflict copies" every round while generating
  none); the "engine alive" scan entry is rate-limited to 600 s.
- **Stale `last_error` / breaker alerts never cleared after recovery**: live evidence — the
  `status=3` error had recovered at 08:30 but was still red on the home page in the
  afternoon. A single `absorb_round()` now clears `last_error` on a *completed* round with
  zero errors and clears the breaker alert when `deletes_blocked == 0`; rounds that return
  early (session expired / server busy) only update the current error and never clear
  historical alarms. Dehydration errors got the same treatment.
- **Expired sessions waited up to 120 s for the keepalive**: `poll_once` returned `Ok` with
  the `qbox_get_max_log` failure only in `errors`, so the poller's `is_auth` re-login branch
  was unreachable. `SyncReport::auth_failed` (status 3 is recognised on the max_log endpoint
  only, with real-device evidence) makes the poller re-login immediately.
- **The decision queue was silently wiped when no mount view existed**: right after daemon
  start / while unmounted / when restore failed, `views` is empty and the old sweep treated
  every row as "task deleted + remote gone", destroying the user's decisions. Added
  `DecisionFacts::observed` (never judge from placeholder signatures) plus an on-disk task
  check to tell "task deleted" from "temporarily unmounted"; unreadable config is kept.
  A red-team review found the other half: `observed` only proves the **remote** side was
  stat'ed; after a daemon restart the local node table is still empty (`local_sig` returns a
  placeholder `MISSING`), so "already resolved, waiting for execution" rows would be judged
  "conflict resolved itself" and dropped. Added `DecisionFacts::local_observed`; rule 4 now
  only concludes "resolved" when both sides were really observed.
- **A failed directory listing was treated as "remote deleted" and could delete local
  nodes/cache** (data safety): with an incomplete `remote_map`, the old logic picked
  `DeleteLocal`. Any non-404 listing failure now defers deletion for that view with an
  explanation; a complete listing on the next round resumes it. The same class of escape
  found in review is covered too: when the directory count exceeds `max_dirs_per_poll`
  (default 200) only the first 200 are scanned and the rest were treated as remotely
  deleted — **no error, no failure counter, the easiest one to miss**. "Directory scan was
  truncated" now counts as an incomplete remote view and defers deletion as well.
- **Baseline deletions and renames never reached disk** (pre-existing; the root cause of
  "recycle-bin purge comes back next round"): `persist()` only merged newer signatures, so
  deletions were resurrected from the in-memory snapshot. Now a round without concurrent
  hook writes replaces the whole baseline (deletions/renames really land); otherwise it
  falls back to per-entry merging. This also affected rule-exclusion purges, executed
  deletions and rename migrations. Since "replace the whole baseline" requires one round at
  a time (the old pure-merge write was naturally concurrency-safe), `run_sync_once` is now
  serialised with a `State`-level async mutex: a manual `sync --once` / GUI "Sync now"
  queues behind the background round instead of overwriting its persisted result.
- **Text diff could OOM / hang**: the LCS DP table was quadratic in line count — 512 KiB of
  one-character lines is 260k x 260k x 4 B ≈ 275 GB worst case. A bounded fallback now
  diffs the first 2000 lines of each side (worst case 16 MiB, `truncated=true`, and it never
  falsely reports "identical").
- **Diff edge cases**: empty remote files no longer send a suspicious `bytes=0--1` range;
  oversize reports the *real* size (FUSE node metadata first, not the truncated read length).
- **`download_range`'s "HTTP 200 + JSON error body" guard was dead code**: it searched for
  the 8-byte `"status"` with a 9-byte window, so the condition was always false and the
  protection never worked. It now recognises the server error envelope precisely without
  misclassifying user JSON files.
- **`download_range` range edges**: `start > end` makes the server ignore Range and return
  the whole file (which FUSE would write into one chunk cache); an ignored Range was not
  detected either. Both are explicit errors now.
- **Upload streaming swallowed read errors as EOF**: `reader.read(...).await.ok()?` turned a
  local IO error into end-of-stream, sending a short body for the declared Content-Length —
  at best losing the root cause, at worst leaving a truncated file on the server.
- **Session keepalive treated "server not ready" as "session dead"**: `status=8` (Qsync
  Central is initializing) should simply wait; the old code re-logged in every round and
  printed the wrong diagnosis.
- **Two real LAN-peer bugs**: `local_of` used `trim_start_matches` and stripped the prefix
  repeatedly (`/home/home/x` → `x`), so a peer could read a *different* file; `path_allowed`
  failed open when roots were empty. Now single `strip_prefix` and fail-closed.
- **`read_cached` could treat the zero bytes of a partially hydrated file as local content**
  (fake diff, potentially a wrong resolve): it now verifies that every required chunk is
  hydrated (in-memory bitmap / read-only `.qxstate` with size/mtime/file_id checks) and
  returns an explicit reason instead of fabricated data.
- **CLI `conflicts` silently preferred one flag**: `--clear`/`--diff`/`--forget`/`--resolve`
  given together silently picked one (users assumed all ran). Now clap rejects conflicting
  combinations (exit 2) and `--as` requires `--resolve`.
- **CLI cumulative counters are labelled**: "deletes blocked", upload/delete queue
  done/failed/retries, hydration stats now say "(cumulative, since daemon start)"; the
  duplicated "content unreadable:" prefix is fixed.

### Changed

- With the `ask` conflict policy, `conflicts` now counts **files waiting for a decision**
  (first enqueue only) instead of accumulating per round; `deletes_blocked` remains
  cumulative (now labelled).
- Recycle-bin exclusion is a **hard rule**: even a user `!@Recycle` re-include rule cannot
  bring them back into sync (consistent with FUSE's `deny_trash`).

### Known limitations (not fixed in this batch)

- Diff depends on FUSE nodes: after a daemon restart, comparing a path the user has not
  touched in the mount yet yields "this process has not materialised the file" (needs a
  FUSE-side materialisation API).
- When the differing region exceeds 2000 lines per side the bounded fallback kicks in: the
  footer explicitly says "result truncated", but the `added/removed` counts only cover the
  compared prefix (the numbers are understated for extremely large diffs).
- The FUSE mount still lists recycle-bin directories (the engine no longer manages them;
  whether to hide them is a product decision).
- `.Trash-<uid>` only matches the local uid; a different NAS-side uid would not be excluded
  (they match on the real device today).
- Continuously repeating sync errors still produce one WARN per round (long outages still
  grow the log; blocked rounds are de-duplicated).
- GUI: the delete queue (`StatusData.deletes`) is not displayed; file-state/LAN query
  failures degrade silently; no single-instance guard; `pick_with` has no timeout;
  `link_read` / `credential_present` swallow errors.
- Protocol: `download_to_file` does not yet reuse the "200 + JSON error body" guard;
  HTTP 401/403 are classified as Transport (no re-login; the real device uses 200+status 4/5);
  no upload cancellation; LAN pairing rate limiting is global and peers may register any
  addr (low risk).
- `status=3` is only recognised as a session loss on `qbox_get_max_log` (insufficient
  evidence elsewhere).

## [0.5.1] - 2026-10-04

**Fixes two GUI problems that looked fine but were not actually working**: single-file
dehydration always returned zero, and the rightmost column of the file table was
invisible.

### Fixed

- **Clicking "dehydrate" on a single row in the GUI file page always returned
  "0 items, 0 B freed"** — not a single file was released. Two layers of root cause:
  1. `doDehydratePath` only sent `{method:'dehydrate', path}`, without `force`. The
     daemon's `let recent = if opts.force { 0 } else { 300 }` therefore applied the
     300s "recently accessed" protection window to it — yet "I just downloaded / just
     finished reading, now free the space" is precisely the case where releasing is
     wanted most; you had to wait five minutes to dehydrate your own read. The CLI has
     had `--force` all along (the M3 doc calls it "the existing switch for manual
     dehydration", §5), but this GUI path had no equivalent, so the test matrices all
     pass `--force` and the gap was never exercised. It now always sends `force: true`;
     **only the "recently accessed" check is skipped** — pin / unuploaded changes /
     open fds / mmap / in-flight / upload queue are all still enforced.
  2. The log swallowed the reason: it printed only `dehydrated`/`freed_bytes`/
     `used_bytes` and never the `blocked` list that the response already carried
     (`DehydrateData.blocked`). There was no way to tell whether the file was blocked
     by the protection window, blocked because it is pinned, or simply not under any
     mount point's remote root (the latter also returns all zeros). It now appends
     "｜blocked: <reason>" and picks one of three log levels: success → `logOk`,
     0 items with reasons → `logErr`, 0 items without reasons → `logInfo`. The new
     `dehydrateReasons()` handles both `blocked` shapes, de-duplicates reasons, and
     folds anything past three into "and N more" (directory-level dehydration produces
     hundreds of entries).
- **The operation log's status colors were dead**: `.logline.ok/.err/.info` declared
  their colors on `.res`, but the spans `logLine` appends are `.t` and `.c` — there
  has never been a `.res` element in the DOM, so all three states rendered in the same
  gray and "failed" / "blocked" were visually indistinguishable. Now matched against
  `.c` (higher specificity, so it overrides the earlier `--fg-dim`), plus
  `flex: 1 1 auto; min-width: 0` on `.c` so long text wraps properly.
- **The GUI file page table did not fit — the whole rightmost "pin / 操作" column was
  cut off**: at the default 1200px window you had to drag horizontally to reach the end
  of a row, and the overflow **produced no scrollbar at all** (the card clipped it, so
  content vanished silently). Three causes stacked up, none of them "the window is too
  small":
  1. The pin column rendered, **on every row**, a `<select>` plus four buttons
     (query pin / set pin / download / dehydrate) — roughly 370px for that one column —
     while **every one of those four already had an equivalent in the row context menu**.
     Pure redundancy.
  2. The table had no horizontal scroll container: `#main` does have `overflow:auto`, but
     the table is constrained to the card width, so the excess simply disappeared.
  3. `have_child` had its own column, yet QNAP returns 0 for directories almost always —
     so the column was permanently empty, wasting ~80px and squeezing the name column.

  Fix: the pin column becomes "narrow select + ⋯ button", with all four operations moved
  into the row context menu (plus three that were missing: query pin state / set unpinned
  / exclude); the table is wrapped in a `.tbl-scroll{overflow-x:auto}` safety net;
  `.tbl-files` switches to `table-layout: fixed` with explicit column widths, the filename
  column is the only flexible one, and long names ellipsize with the full name in `title`;
  `have_child` folded into the type column (a ▾ after directories) so no information is
  lost and a whole column is saved. Measured horizontal overflow while narrowing the
  content area: 1136→0 / 1000→0 / 900→0 / 800→0, 700→66, 600→166 (only then does it
  degrade to a scrollbar) — **zero overflow at the 960px minimum window**.
  `openRowMenu` gained a 4th parameter carrying that row's select into the menu state, so
  changing pin now writes back to it instead of leaving "unknown" on screen.

### Not done

- **The "dehydrate all" button keeps `force: false`**: in a bulk operation the files
  just accessed are exactly the ones worth keeping locally, which is a different
  semantic from the single-file button. It already renders the full `blocked` list in
  the panel, so observability is fine there.

## [0.5.0] - 2026-10-04

**The sync engine no longer takes the mount point down with it, and remote renames no longer
sprout phantom conflict copies**: this release separates "sync" from "mount" in terms of
lifecycle, and gives every file a locally-stable identity.

That closes out T1–T9 of the M15 list (T4 was cancelled after verification — see below).

### Added

- **`systemctl --user reload qxyncd` (SIGHUP): reload sync without interrupting the mount
  point for a single second.** New `qxync daemon reload` takes the same path. The
  implementation hands the sync engine a *generation token*; old tasks retire at their next
  checkpoint — no `abort()`, so a task parked on network I/O never holds up the shutdown.
  ★ **This is the path that actually works for "restart without losing the mount"**:
  systemd's `restart` is "stop → wait for the old process to exit → start", so a
  never-exiting guardian gets SIGKILLed after `TimeoutStopSec` and the mount dies anyway.
  That is systemd's semantics, not something qxync can work around.
- **SIGTERM keeps mounts**: IPC `stop` / `SIGINT` still unmount everything (the user really
  did say "shut it down"), but SIGTERM now only stops the sync tasks, writes state to disk,
  and switches into mount-guardian mode. The guardian has four exit conditions (IPC stop /
  SIGINT / a second SIGTERM / empty mount table) plus an optional `--mount-hold-secs`, so it
  cannot wedge systemd.
- **Automatic cleanup of zombie mounts**: after a real crash or an OOM kill, the mount point
  stays in `/proc/mounts` but returns `ENOTCONN`, and `cd`ing in says "Transport endpoint is
  not connected". The daemon now probes the mount points **it recorded** early at startup
  and lazy-unmounts (`fusermount3 -uz`) only the ones confirmed dead. Safety boundaries: only
  paths from `mounts.json` (never a full-disk scan), subtype must be `qxync`, `-uz` rather
  than `-u`, and a failure only warns — it never blocks startup.
- **Stable file identity `file_id` (16 bytes, built locally)**: the server's `DirEntry` has no
  id field, so identity is derived from `(mtime, size)` (directories get their own scheme,
  and it **deliberately excludes the filename** — including it would change identity on
  rename). New `nodes` table (schema v3 → v4); deletions clear the index.
  - **Reconciliation uses it to recognise remote renames** → the local node is renamed and
    the baseline moves with it, so **no conflict copy is generated**. This was the root
    cause of the most annoying phantom copy in 0.4.x.
  - **A queued upload whose file is renamed remotely** now lands under the new name instead
    of creating a ghost file.
  - **Pairing is conservative**: unknown identity, mismatched type, or one identity mapping
    to several paths (ambiguous) are all refused, falling back to the original add/delete
    decision. Better one conflict too many than a mis-pairing or lost data.
- **`qxync file-states <dir>`**: per-file sync status. `in_sync` is a **derived value**
  (aggregating `dirty` / queued uploads / pending conflict decisions) — no new state table.
- **Transfer progress**: download reports after every received chunk (128 KiB); upload
  advances using the "bytes actually sent" counter introduced back in T2. Progress reporting
  is **read-only** and **never propagates** into the read or upload path (callback panics are
  isolated with `catch_unwind`).

### Fixed

- **`rename_remote` failed to migrate the identity index** (caught and fixed during this
  release's own review): the remote-rename path migrated the local node but left the row in
  the `nodes` table behind. The same `file_id` then appeared twice → lookups by identity
  permanently reported "ambiguous" → **the upload queue's identity-based retargeting was dead
  for good**, and every remote rename leaked one index row. Fixed, with a regression test
  that actually catches it (verified by mutation testing: remove the fix and the test fails).
- **`progress_percent` integer overflow**: with `done > total`, `saturating_mul(100)` produced
  `18446744073709551615`. Now uses a `u128` intermediate and clamps to 100.
- `Store`'s SQLite connection is now an `Arc<Mutex<Connection>>`: the FUSE side also needs a
  `Store` (for the identity index), and it must be the **same connection** rather than two
  connections fighting over the write lock.

### Changed

- **The default "restart without losing the mount" path moves from `systemctl restart` to
  `systemctl reload`. The two are not interchangeable — don't mix them up**: `restart` still
  interrupts the mount briefly (recovered by T5's automatic remount, typically within 3
  seconds); use `reload` when you need *zero* interruption. The systemd unit's comment was
  corrected accordingly — the old line "qxyncd unmounts all FUSE mount points on SIGTERM" is
  no longer accurate as of 0.5.0.
- To stop the guardian process use `qxync daemon stop` (goes through IPC, really unmounts) or
  `qxync umount` (unmounts the mount point only).

### Not doing

- **T4 chunked/resumable upload — cancelled after verification.** The original premise was
  wrong: `qsyncsrv.cgi` implements chunked upload thoroughly (5 funcs, complete parameter
  tables). The real obstacle is that **the chunked channel only serves Qbox space**
  (`upload_root_dir` recognises the `/remote:` prefix), while `upload.php`'s `dest_path` uses
  ordinary home-directory paths — **the two path spaces are disjoint**, so we cannot reach it.
  The probe is kept at `xtask/probe/upload_chunk_probe.py`. It also corrects a subtle trap:
  that CGI returns **two JSON documents**, and the leading `"status": 0` is a framework
  header — scraping only the first one makes every func look like it did nothing.

### Verification status

- `cargo check --workspace --all-targets`: zero errors, zero warnings.
- `cargo test --workspace --lib --bins --tests`: **290 passed / 0 failed** (baseline 265 +
  25 new in this batch). All 19 `#[ignore]`d tests live in `qxync-proto-test/` and need a real
  NAS; this batch added no new ignores.
- ⚠️ **This machine has no `/dev/fuse`**, so every acceptance item that requires actually
  mounting something (`ls` during reload, self-healing after `kill -9`, whether the FUSE
  session truly survives SIGHUP) is covered **by logic-level unit tests only — no real-machine
  verification**. The T6/T7/T9 sections mark each such item "unverified".

## [0.4.3] - 2026-10-04

**No trash bin on the mount point, and deletes no longer block**: files landing in a trash
bin and long waits on delete came from two different places; both are addressed here.

### Fixed

- **The mount point no longer hosts a trash bin**. The trash bin was never qxync's — there
  is no trash logic in the code at all (KDE Dolphin follows the FDO per-volume trash
  protocol, creates `.Trash-$UID/` *inside the mount point*, and its contents end up on
  the NAS). FUSE write entry points now reject trash names (`.Trash` / `.Trash-<uid>` /
  `@Recycle`) with `EPERM`: **the trash directory cannot be created, so KIO cannot enable
  the volume trash and falls back to its built-in direct delete**. The guard covers
  `mkdir` / `create` / `rename` / `unlink` / `rmdir`, and matching is **exact per path
  segment** — so ordinary names like `.recent`, `.trash` or `a.Trash-1000.txt` are not
  affected. (KIO 6.30 removed the `DisabledFor` key, so there is no configuration-level
  switch; this has to be enforced at the filesystem layer.)
- **`rm -rf` parent/child delete conflict**: the kernel hands both `rmdir(dir)` and
  `unlink(dir/child)` to the delete queue at once. Once the parent is gone, deleting the
  child necessarily fails with "not found", burning retries and inflating the failure
  count. Children whose parent is deleted **in the same batch** are now dropped before
  the request is sent, with prefix matching done on **whole path segments** (so a deleted
  `victim` does not affect `victim2.txt`).
- **Delete retry counts were being reset**: retries used to overwrite each item's counter
  with the batch maximum, so an item that had already failed 4 times was knocked back to
  1 and could never reach the give-up threshold. Counts now increment per item and each
  item decides independently whether to give up.

### Added

- **A delete queue** (`qxync-fuse/src/delete.rs`): `unlink`/`rmdir` now **return
  immediately**, and the remote delete is pushed by a background worker that coalesces
  items into batches. Previously this was `rt.block_on(delete_entry(..))` — unlink waited
  for the NAS to answer, so deleting N files meant N serial round trips.
  - **Same-directory items are merged into one request**: `file_total` on
    `qsyncsrv.cgi?func=delete` was always meant to be "number of items in this batch"
    (`stat`/`set_mtime` hardcode it to `1` only because those handle one file per call).
    Items are now grouped by directory, capped at 50 per batch, with a 200 ms coalescing
    window.
  - **Crash recovery**: jobs are persisted to a `deletes` state table (the same
    `queue.db` as the upload queue) with identical semantics — **the table *is* the set of
    unfinished jobs**, and they are re-queued on restart.
  - `status` gains a "delete queue" line; it is drained for up to 120 s on unmount.
  - **Matches official semantics**: the official Qsync client's delete (Smart Delete) is
    *"deleted files on the local device are retained on the NAS"* — locally effective
    immediately, advanced asynchronously server-side. This queue follows that shape.

### Safety

- **The delete circuit breaker still runs before enqueueing**: `DeleteGuard`
  (100 items / 60 s) is unchanged and still returns `EACCES` past the threshold, so a
  large accidental delete cannot slip into the queue just because deletes became async.
- The trade-off of async deletion is "local is authoritative": enqueue returns
  immediately, so if the remote delete fails the local file is already gone. Failures that
  exhaust retries are reflected in the `status` failure count and the daemon log, so they
  can be cleaned up manually.

## [0.4.2] - 2026-10-04

**Two saves in a row no longer spawn a bogus conflict copy, and saving is faster**: this release
fixes a false conflict a user can hit on the very first try (saving the same file twice produces an
extra conflict copy), and cuts the wait for one save from **growing linearly with file size** to
roughly 1/8 of it.

### Fixed

- **Why the conflict copy appeared (not "two saves should merge" — the baseline was
  never advanced)**: the baseline records the **remote** signature as of the last
  successful sync, and previously only the sync engine advanced it, when a poll
  round observed the remote. Our own successful upload only cleared `dirty` in the
  success hook, leaving the baseline alone. That left a window of up to **30s**
  (one poll interval) between "upload landed" and "next poll", in which local ==
  remote == the new version while the baseline still held the old signature.
  `decide` then saw "remote changed + local not dirty" and, because the local
  signature differed from the baseline, concluded **Conflict** — so **saving twice
  in a row grew a conflict copy after the second save settled**. The baseline is now
  advanced to the signature just written the moment the upload succeeds
  (`note_uploaded`), the window is gone, and each save is its own increment.
  Genuine simultaneous edits are still detected as conflicts (the `decide` table is
  unchanged).
- **A reverse trap in the fix above, fixed together**: a poll round "clones a
  baseline snapshot → runs long network IO → writes it back", and it used to write
  back that **stale snapshot wholesale**, wiping signatures the hook had advanced
  while the poll was running — undoing the very fix above. Persistence now
  **merges**: only entries strictly newer than the in-memory ones are accepted
  (larger mtime wins); on an mtime tie the in-memory entry is kept (two saves
  within the same epoch second are common).
- **Slow saves: before writing, every missing chunk of the file has to be fetched,
  and they were fetched one at a time**. Read-modify-write requires the whole file
  locally first (otherwise unfetched ranges read as 0 and the full-file upload
  zeroes the remote), so the cost of one save was *all* missing chunks × RTT —
  **growing linearly with file size** (128 KiB chunks: 8 round-trips for 1 MB,
  32 for 4 MB). The chunks are independent, so they are now fetched with a
  fan-out of 8, turning O(chunks×RTT) into O(⌈chunks/8⌉×RTT) — roughly 8× for
  1 MB. Tune with `QXYNC_HYDRATE_FANOUT` (1–64, default 8; 1 restores serial
  fetching). Correctness is unchanged: each chunk has its own dedup lock and
  content is persisted before the bitmap, so no half-written chunk can ever be
  marked ready.
- Also fixed a race in the upload callback: the byte count used for stats was read
  from disk **after** the upload, so with two rapid saves the job may already have
  been dequeued and the read picked up the **next** version's size — and that
  number is what advances the baseline, parking it on a signature the remote does
  not have. The callback now receives the byte count **actually sent**
  (`upload_one`'s return value). Also, if the local file changed again while the
  upload was in flight, the baseline is deliberately **not** advanced; the next
  version advances it when it lands.
- **Opening a folder with the desktop ID reported by `xdg-mime` always fails**: `xdg-mime` returns a
  **desktop entry ID** such as `org.kde.dolphin.desktop`, but the program that actually runs is the
  one named in its `Exec=` line (`dolphin`) — `org.kde.dolphin` does not exist in `PATH` at all.
  Measured on the development machine: spawning the ID directly gives **127 "No such file or
  directory"**, while the `Exec`-resolved program starts normally.
- **A failing `setsid` made "Open folder" unusable**: `setsid()` returns `EPERM` in a process that
  **is already a process-group leader**, and a forked child can still be one. Treating that as fatal
  made the button **impossible to press** in such environments. Detaching is now **best-effort**: if
  it fails the program is still launched (it just shares the GUI's session, matching what the opener
  plugin does).

### Added

- `QXYNC_HYDRATE_FANOUT`: chunk fetch fan-out (1–64, default 8).
- **"Open folder" button on task cards**: every task card (home and tasks page) now has an
  "Open folder" button that opens that task's **local mount point** with the **default file manager
  the user configured for directories**. It queries the user's configured default directory handler
  with `xdg-mime query default inode/directory`, locates the matching `.desktop` per the XDG spec,
  reads its `Exec=` to get the **actual executable name**, and spawns it detached (`setsid`) with
  the path. When no handler is configured or it fails to start, it falls back to the previous plugin
  opener (`xdg-open` first) and reports which route was taken (`via`), so a dead button is
  diagnosable instead of silent.
- When a task has no local mount point the button is **disabled** with a reason instead of doing
  nothing; when the mount point is missing (NAS offline, auto-unmounted after the task was disabled)
  the error is reported honestly — it never `mkdir`s an empty directory, which would make the user
  think the files are really local and hide exactly the failure they need to see.

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

[0.6.4]: https://github.com/mlzxgzy/qxync/compare/v0.6.3...v0.6.4
[0.6.3]: https://github.com/mlzxgzy/qxync/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/mlzxgzy/qxync/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/mlzxgzy/qxync/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/mlzxgzy/qxync/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/mlzxgzy/qxync/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/mlzxgzy/qxync/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/mlzxgzy/qxync/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/mlzxgzy/qxync/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/mlzxgzy/qxync/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/mlzxgzy/qxync/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mlzxgzy/qxync/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/mlzxgzy/qxync/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/mlzxgzy/qxync/releases/tag/v0.2.2
[0.1.1]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.1
[0.1.0]: https://github.com/mlzxgzy/qxync/releases/tag/v0.1.0
