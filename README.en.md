# qxync — Qsync for Linux (with on-demand sync)

[![CI](https://github.com/mlzxgzy/qxync/actions/workflows/ci.yml/badge.svg)](https://github.com/mlzxgzy/qxync/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mlzxgzy/qxync?label=release)](https://github.com/mlzxgzy/qxync/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform](https://img.shields.io/badge/platform-Linux-lightgrey.svg)](#install)

[中文](README.md) · **English** · [Download](https://github.com/mlzxgzy/qxync/releases) · [Changelog](CHANGELOG.en.md) · [Disclaimer](DISCLAIMER.md)

A **third-party QNAP Qsync client for Linux**, written in Rust + FUSE, whose core is
**on-demand sync**: a file on the NAS is merely a "placeholder" locally — `ls -l` shows its
real size with **zero download**, and only the range you actually read gets fetched;
anything you don't want to keep can be **dehydrated** back into a placeholder, so local disk
usage stays under your control at all times.

Installing it gives you three programs: `qxync` (command line), `qxyncd` (long-running daemon)
and `qxync-gui` (desktop GUI).

> ⚠️ **Only for QNAP NAS devices that you own or are explicitly authorized to use.**
> This project is **in no way affiliated with QNAP Systems, Inc.** and does not distribute any
> QNAP binary or installer — see the **[Disclaimer](DISCLAIMER.md)**.
> It **reads and writes real files on your NAS**, so try it on a test directory before trusting
> it with important data.

## Features

- **On-demand sync**: placeholders + on-demand 128 KiB range downloads; `head -c 100 big.bin`
  fetches only the range it needs, not the whole file
- **Cache first**: content already on disk is reused as-is; directory listings come from a local
  snapshot, so `ls` does not wait on the network every time
- **Mounts survive session expiry**: an expired login is renewed automatically and the operation is
  retried in place — no manual remount
- **Offline usable**: cached content stays readable and writable while the network is down; local
  changes (including `mkdir`, rename, delete and `touch`) go into a durable buffer and are replayed
  in order once the NAS is reachable again — instead of being dropped after 10 seconds of retries
- **Read-write mount**: with `--rw`, local changes are pushed back through an upload queue;
  writes are read-modify-write, so un-fetched ranges are never uploaded as zeros
- **Free up space**: automatic dehydration by idle time / cache quota, or manually with
  `qxync dehydrate`
- **Conflict handling**: when both sides changed, a conflicted copy is created; you can choose
  per file to keep the local version, the NAS version, or both
- **Selective sync**: gitignore-flavoured `exclude` rules + built-in temporary-file filtering
- **Shared folders**: the home directory and Qsync sync folders registered on the NAS can both be
  mounted; whether they are writable is decided by the NAS
- **Multiple NAS boxes**: one independent daemon per connection; mount and sync several NAS
  devices at once
- **Two-step verification**: supports TOTP (authenticator apps) in QNAP's 2SV
- **LAN direct**: qxync↔qxync transfers directly on the same LAN, with silent fallback to the NAS
- **Desktop GUI**: an overview, task list, file browser, update center, error list, settings and
  diagnostics, plus tray, notifications and autostart

## Screenshots

> The screenshots below show a **redacted** UI (real NAS addresses and local paths have been painted over).

| Home | Files |
|---|---|
| ![Home](docs/images/gui-home.png) | ![Files](docs/images/gui-files.png) |

| File Update Center | Settings · Proxy |
|---|---|
| ![Updates](docs/images/gui-journal.png) | ![Proxy](docs/images/gui-settings-proxy.png) |

## Install

Download a prebuilt artifact from [Releases](https://github.com/mlzxgzy/qxync/releases), or build
from source. The prebuilt artifacts are **x86_64 Linux** and require **glibc ≥ 2.35**; each release
ships a `SHA256SUMS` file for verification. On the NAS side, **Qsync** (the QPKG) must be installed
and enabled, and the Qsync service must be turned on in QTS (default port `9834`).

### Arch Linux

```bash
# download qxync-bin-<version>-1-x86_64.pkg.tar.zst from the Release
sudo pacman -U qxync-bin-<version>-1-x86_64.pkg.tar.zst
```

The package contains the three programs, a desktop entry, icons and the systemd user units.
The repo also carries an AUR package definition ([`packaging/arch/`](packaging/arch/)): under
`packaging/arch/`, run `updpkgsums` first to fill in the checksums of the Release artifacts, then
`makepkg -si` (see the README in that directory).

### Debian / Ubuntu

The Release ships a single `.deb` that works on **both Debian and Ubuntu** (amd64):

| Download this | Supported distributions |
|---|---|
| `qxync_<version>-1_amd64.deb` | Debian 12 / 13, Ubuntu 22.04 / 24.04 / 26.04 |

```bash
sudo apt install ./qxync_<version>-1_amd64.deb
```

The package is built from the same tarball as the other Release artifacts (the three binaries plus
the desktop entry / icons / systemd user units / documentation). It is built in an `ubuntu:22.04`
container; `Depends` is measured with `dpkg-shlibdeps` against the three binaries and then gets a
hand-added `fuse3`. `apt install ./<file>.deb` pulls in the dependencies from your configured apt
sources; if you prefer `dpkg -i`, follow it with `sudo apt-get -f install` to install whatever is
missing. Every dependency is a standard package in the official Debian / Ubuntu archives (GTK 3,
WebKitGTK 4.1, libsoup 3, FUSE 3, …), so **the common Chinese mirrors (Tsinghua, Aliyun, …) carry
them** — our packaging and CI install checks actually run against the Tsinghua mirror.

CI install-tests it in clean `debian:12` / `debian:13` / `ubuntu:22.04` / `ubuntu:24.04` /
`ubuntu:26.04` containers (`qxync --version` correct, `ldd` shows no `not found`,
`qxync-gui --self-test` prints JSON; newer releases renamed GTK 3 to `t64` packages such as
`libgtk-3-0t64`, which carry `Provides: libgtk-3-0 (= version)`, so dependencies with a version
floor are still satisfied). The package ships the systemd user units, so you can enable them right
after installing:

```bash
systemctl --user enable --now qxyncd
```

Like the other Release artifacts, the binaries are **intentionally not stripped** (so user-reported
stack traces stay readable); the installed size is about 190 MB. See
[`packaging/debian/README.md`](packaging/debian/README.md) for the dependency list, packaging
details and the expected lintian warnings.

**Upgrading from the old packages**: if you previously installed
`qxync_<old-version>-1~deb12_amd64.deb` / `qxync_<old-version>-1~ubuntu22.04_amd64.deb`, just
`sudo apt install ./qxync_<version>-1_amd64.deb` — by dpkg version ordering `<version>-1` is greater
than `<old-version>-1~deb12` (a tilde sorts before the empty string), so apt upgrades normally; to check
first, run `dpkg --compare-versions <version>-1 gt <old-version>-1~deb12 && echo newer`.

### Other x86_64 Linux

```bash
# download qxync-<version>-x86_64-unknown-linux-gnu.tar.gz from the Release
tar xzf qxync-<version>-x86_64-unknown-linux-gnu.tar.gz
cd qxync-<version>-x86_64-unknown-linux-gnu
install -Dm755 qxync qxyncd qxync-gui ~/.local/bin/
```

The tarball also contains `qxync.desktop`, icons and the systemd units. Note that the unit's
`ExecStart` assumes the binaries live in `/usr/bin` (the distro-package layout); if you installed
them into `~/.local/bin`, either change `ExecStart` to the real path or skip the unit and use
`qxync daemon start`. Keep the three programs in the same directory: the GUI first looks for
`qxyncd` next to itself and only then falls back to `PATH`.

Runtime requirements:

- **Mounting** needs FUSE 3: `/dev/fuse` + `fusermount3` (install the `fuse3` package)
- **The GUI** needs the WebKitGTK 4.1 and GTK 3 runtimes
  (`libwebkit2gtk-4.1-0` + `libgtk-3-0` on Debian/Ubuntu)

### Build from source

Requires **Rust 1.90+** and FUSE 3; building the GUI additionally needs the WebKitGTK 4.1 / GTK 3
development packages (`libwebkit2gtk-4.1-dev` + `libgtk-3-dev` on Debian/Ubuntu).

```bash
git clone https://github.com/mlzxgzy/qxync.git
cd qxync
cargo build --release --workspace
# binaries: target/release/{qxync,qxyncd,qxync-gui}
```

The frontend is the dependency-free static set under `crates/qxync-gui/ui/` — **no npm or bundler
is needed**; after editing it, rebuild with `cargo build` (the assets are embedded at compile
time). For an AppImage, use the Tauri CLI — `tauri.conf.json` already configures the targets. To
build a Debian / Ubuntu `.deb` locally, use the debian packaging scripts in the repo — see
[`packaging/debian/README.md`](packaging/debian/README.md).

## Quick start

### 1. Log in to your NAS

```bash
QXNYC_PASSWORD='<password>' qxync login --host <NAS-address> --user <user> --insecure
```

- `--insecure` = accept the **self-signed certificate** served by QTS; drop it if your NAS uses a
  trusted certificate.
- The port defaults to `9834` (use `--port` if you changed it in QTS).
- Pass the password via `QXNYC_PASSWORD` to keep it out of your shell history; credentials are
  stored in `~/.config/qxync/credentials.json` (mode `0600`; non-`default` connections use
  `~/.config/qxync/links/<id>.credentials.json`).
- If the account has **two-step verification** enabled, add `--otp-secret '<TOTP secret>'`
  (stored; the daemon then computes codes on every re-login — recommended) or
  `--otp-code <6-digit code>` (this login only, not stored).

### 2. Start the daemon

If you installed a distro package (Arch / Debian / Ubuntu, or the binaries are in `/usr/bin` and
you installed the systemd unit yourself), the user unit is recommended:

```bash
systemctl --user enable --now qxyncd
journalctl --user -u qxyncd -f        # logs
sudo loginctl enable-linger "$USER"   # optional: keep it running without an active login
```

Without systemd, use the CLI:

```bash
qxync daemon start
```

> Do not use both: once the systemd unit is in use, stop managing the daemon with
> `qxync daemon start` / `stop`.

Several NAS boxes: each connection gets its own daemon. For connections other than `default`, use
the template unit `qxyncd@<link_id>` (`<link_id>` corresponds to
`~/.config/qxync/links/<id>.json`, e.g. `systemctl --user enable --now qxyncd@nas2`), or run
`qxync daemon start --all`. `qxync links` lists all connections with their daemon / login state.

### 3. Mount

```bash
mkdir -p ~/qxync-mnt
qxync mount ~/qxync-mnt --remote /home          # read-only (default)
# for write-back, use this instead (pick one):
qxync mount ~/qxync-mnt --remote /home --rw     # read-write
```

The mount point *is* that NAS folder's content; **one mount point = one NAS folder**
(for several folders, create several mount points / tasks).

```bash
ls -l ~/qxync-mnt/qxync-test              # real size, nothing downloaded yet
cat ~/qxync-mnt/qxync-test/hello.txt      # the first read hydrates on demand, only the ranges needed
getfattr -n user.qxync.state ~/qxync-mnt/qxync-test/hello.txt   # placeholder / partial / hydrated
qxync dehydrate --path /home/qxync-test/big.bin                 # dehydrate, reclaim local space
qxync umount ~/qxync-mnt
```

### 4. Desktop GUI

```bash
qxync-gui
```

On first launch, fill in the NAS address, account and password under **设置 → 连接**
(*Settings → Connections*) and log in, then click **启动 daemon** (*Start daemon*) in the top bar.
If systemd already started the daemon, you can skip that last step.
The GUI currently ships Chinese UI strings only — see "Known limitations".

### 5. Common commands

| Command | What it does |
|---|---|
| `qxync status` | session, server, cursors, hydration stats and mounts |
| `qxync ls /home` | list a NAS directory |
| `qxync mount ~/qxync-mnt --remote /home --rw` | mount (`--rw` for read-write) |
| `qxync umount ~/qxync-mnt` | unmount |
| `qxync sync --once` | run one change-discovery round now |
| `qxync dehydrate --path <remote-path>` | dehydrate, free local space |
| `qxync space --now` | free up space now using the current policy |
| `qxync task list` / `task pause <id>` / `task resume <id>` | list / pause / resume sync tasks |
| `qxync journal --level error` | sync log / error list |
| `qxync conflicts --resolve <id> --as keep_local\|keep_remote\|keep_both` | resolve a conflict |
| `qxync file-states <remote-dir>` | file states: online-only / locally available / always available |
| `qxync settings` / `settings --set k=v` | view / change settings (proxy, automatic space freeing, …) |
| `qxync links` | list all NAS connections (with their daemon / login state) |
| `qxync peer status\|pair\|ping\|events\|fetch` | LAN peering (enable `peer_listen` in the connection config first) |
| `qxync purge --task <id>` | delete a task and its local data (preview by default; add `--yes` to execute) |

All subcommands are listed in `qxync --help`; the GUI and the CLI share the same daemon.

## Upgrading

After replacing the binaries, **restart the daemon** (a running process does not update itself):

```bash
systemctl --user restart qxyncd       # with the systemd unit
# without systemd:
qxync daemon stop && qxync daemon start
```

Before stopping, `qxync daemon stop` checks whether the mount point is still in use (a file
inside it is open, or a process `cd`'d into it). If so it lists the `pid / program / path`
and **refuses to stop** until those programs are closed; add `--force` to stop anyway (open
files under the mount point stop working immediately). `qxync umount` uses the same check.

Breaking changes when upgrading from 0.1.x (named `qsync`) or 0.2.x (multi-root mounts) are in the
[CHANGELOG](CHANGELOG.en.md).

## Known limitations

- **Linux only**: depends on FUSE and `/proc` semantics; no Windows / macOS support.
- **Limited real-hardware verification**: fully verified only on
  **QNAP TS-464C / QTS 5.2.9 / Qsync QPKG 5.0.0.7**; other models / firmware may behave differently.
- **Local-account login only**: no QID / myQNAPcloud cloud-account login.
- **One mount point = one NAS folder**: use several tasks for several folders.
- **Multiple NAS boxes**: do not bidirectionally sync the same remote directory through two
  connections to the same NAS (each connection has its own baseline, so they read each other's
  changes as their own).
- **Two-step verification covers TOTP only** (authenticator apps): security questions, email codes
  and QR-code / device approval are not implemented; the TOTP secret is stored in plaintext in the
  `0600` credentials file, same as the password (keyring support is future work).
- **The tray icon needs an SNI-capable desktop environment**: Plasma, Xfce (statusnotifier plugin),
  waybar, polybar, LXQt, Cinnamon and GNOME + AppIndicator all work; IceWM / Fluxbox / Openbox+tray /
  bare GNOME / old Xfce·MATE panels (XEmbed only) do not show a tray icon (install
  [`snixembed`](https://sr.ht/~steef/snixembed/) to bridge an old-style tray). When the tray is not
  visible, closing the window really closes it — it is never hidden in a tray nobody draws.
- **The GUI is currently Chinese-only**: the `en` string table covers only a few
  two-step-verification messages; every other UI string is Chinese.
- **No trash bin on the mount point**: moving files to the trash is rejected, so deleting really
  deletes (the remote delete is pushed by a background queue and guarded by a bulk-delete circuit
  breaker).
- **No interoperability with the official client**: its binary WebSocket channel was not recovered;
  LAN acceleration is qxync's own protocol and silently falls back to the NAS.

## Security & privacy

- **No telemetry is collected or reported**; qxync connects only to the NAS you configure and
  (optionally) LAN peers.
- **It does not write the NAS device list, does no device registration, and modifies no NAS-side
  configuration.**
- Credentials live in `~/.config/qxync/credentials.json` (`0600`; non-`default` connections use
  `links/<id>.credentials.json`); the IPC socket directory is `0700`.
  Config, state store, cache and logs live under `~/.config/qxync`, `~/.local/share/qxync` and
  `~/.local/state/qxync`; `qxync purge --all` removes all local data (it never touches the NAS or
  files inside the mount point).
- LAN peering is **plaintext TCP on the local network** — enable it only on a trusted network.
- If you find a security problem, please **do not open a public Issue** — see
  [SECURITY.md](SECURITY.md).

## Contributing

Bug reports, measured protocol corrections, code PRs and UI translations are all welcome — see
[CONTRIBUTING.md](CONTRIBUTING.md). Before you start, read the
[two hard rules](CONTRIBUTING.md#4-the-two-hard-rules-reviewers-check-these) there: they are about
data integrity, and no change may break them.

## License

The **original code** of this project is released under **MIT OR Apache-2.0**, at your option:
[LICENSE-MIT](LICENSE-MIT) / [LICENSE-APACHE](LICENSE-APACHE).

That license covers only the original parts of this repository; QNAP, Qsync, myQNAPcloud, QID and
other names and marks are trademarks or registered trademarks of QNAP Systems, Inc. — see
[DISCLAIMER.md](DISCLAIMER.md) for details.

Design and acceptance records live in [`docs/`](docs/).
