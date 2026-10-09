<!-- SPDX-License-Identifier: GPL-3.0-only -->

# The station package and installer

A station is one Debian package, `coaxhub-stationd`, installed by a short script ([installer/station.sh](../installer/station.sh)). The package holds stationd, a private copy of radiod and both services, so there is nothing else to install. This document covers how the package is built, what it contains, why each systemd setting is there, and how the package and the installer are tested.

## 1. Targets and versions

| | bookworm (Debian 12) | trixie (Debian 13) |
|---|---|---|
| **amd64** | `coaxhub-stationd_<v>-<r>~deb12_amd64.deb` | `coaxhub-stationd_<v>-<r>~deb13_amd64.deb` |
| **arm64** | `coaxhub-stationd_<v>-<r>~deb12_arm64.deb` | `coaxhub-stationd_<v>-<r>~deb13_arm64.deb` |

- `<v>` is stationd's crate version and `<r>` is `PACKAGE_REVISION` (default 1). The `~debN` suffix orders an upgrade across suites: `0.1.0-1~deb12` < `0.1.0-1~deb13` < `0.1.0-1`.
- Raspberry Pi OS is Debian: its suite is the Debian codename, and the 64-bit image is arm64. The Pi 3B+ (Cortex-A53) is the oldest target.
- Each stationd release pins exactly one ka9q-radio commit, the one its TLV table was generated from: `384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd`.

## 2. Building

```sh
COAXHUB_DOMAIN=coaxhub.test COAXHUB_ROOT_CA=/path/to/root.pem packaging/build-all.sh
COAXHUB_DOMAIN=coaxhub.test COAXHUB_ROOT_CA=/path/to/root.pem packaging/build-deb.sh --suite trixie --arch arm64
```

| Variable | |
|---|---|
| `COAXHUB_DOMAIN` | required: the domain built into stationd; enrollment goes to `enroll.<domain>` |
| `COAXHUB_ROOT_CA` | required: the PEM file of the CoaxHub root built into stationd, the only trust anchor for enrollment. The script refuses a file with a private key, and copies only this file into the build. |
| `PACKAGE_REVISION` | Debian revision, default 1 |
| `RUST_VERSION` | the rustup toolchain of the build image, default the channel in `rust-toolchain.toml` |
| `RADIOD_ARCHOPTS` | overrides radiod's `ARCHOPTS` (below) |
| `DOCKER_PROGRESS` | `docker build --progress`, default `auto` |

A package you build with your own domain and root enrolls only against that service. Release builds use the production domain and the root committed under `packaging/roots/` ([releasing.md](releasing.md)).

Output, in `target/packages/` (the layout an APT repository is built from):

```
bookworm/coaxhub-stationd_0.1.0-1~deb12_amd64.deb
bookworm/coaxhub-stationd_0.1.0-1~deb12_arm64.deb
bookworm/ka9q-radio-384b269a.tar.gz     the corresponding source of the bundled radiod
trixie/coaxhub-stationd_0.1.0-1~deb13_amd64.deb
trixie/coaxhub-stationd_0.1.0-1~deb13_arm64.deb
trixie/ka9q-radio-384b269a.tar.gz       the same file
```

### How

`build-deb.sh` needs only `git` and Docker with BuildKit (`docker build` with buildx). Everything else happens in `debian:<suite>` containers on the Docker daemon, driven by [packaging/deb/Dockerfile](../packaging/deb/Dockerfile) with the repository root as the build context. Nothing is bind-mounted and the result comes back with `--output type=local`, so it works the same on a CI runner, on a workstation and with a remote Docker daemon.

1. **The radiod source.** The pinned commit is fetched once (`git fetch --depth 1`) into `target/packaging/ka9q-radio.git`, and `git archive | gzip -n` makes `ka9q-radio-384b269a.tar.gz`. radiod is built **from that tarball**, so the published source is exactly what was compiled.
2. **Stage `toolchain`** (cached as an image layer per suite): the native and `aarch64-linux-gnu` C toolchains; the `-dev` libraries radiod links, for amd64 and, through multiarch, arm64 (`libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libusb-1.0-0-dev librtlsdr-dev`); `protobuf-compiler` for station-proto; `dpkg-dev`; Rust from rustup with both targets. The build needs no arm64 emulation: arm64 is cross-compiled.
3. **Stage `radiod`**: `make radiod rtlsdr.so` with every hardware driver off except `ENABLE_RTLSDR=1`.
   - `ARCHOPTS` is always passed. Upstream's `-march=native` would tie the binary to the build machine. The defaults are `-march=x86-64-v2 -mtune=generic` for amd64 (any x86-64 CPU since about 2009) and `-march=armv8-a -mtune=cortex-a53` for arm64 (every 64-bit Pi).
   - `config_paths.h` is written by the Dockerfile instead of upstream's Makefile rule, which asks git (the tarball has no `.git`). It holds the same commit, date and summary, and the package's private paths: `PKGLIBDIR`, `PKGDATADIR` and `CONFDIR` are `/usr/lib/coaxhub/radiod` (so a `/etc/radio` from another radiod install never overrides our presets), and `STATEDIR` is `/var/lib/coaxhub-radiod` (FFTW wisdom).
   - Upstream's Makefile calls a bare `ar`; the build puts the target's `ar` first on `PATH`.
   - radiod and `rtlsdr.so` are stripped.
4. **Stage `stationd`**: `cargo build --release --locked -p stationd --features packaged --target <triple>` with `COAXHUB_DOMAIN` and the root certificate (a named build context holding only `root.pem`). The cargo registry and the target directory are BuildKit cache mounts, so a rebuild compiles only what changed. `cargo tree` lists the linked crates for the copyright file.
5. **Stage `package`**: [packaging/deb/assemble.sh](../packaging/deb/assemble.sh) lays out the file tree, runs `dpkg-shlibdeps` for the target architecture on the three ELF files, writes `control`, `conffiles` and `md5sums`, sets every mtime to the last commit's time, and runs `dpkg-deb --root-owner-group -Zxz`.

Build times on a 16-thread x86-64 development machine:

| | first build | stationd changed |
|---|---|---|
| bookworm amd64 | about 75 s (toolchain image about 40 s, stationd 37 s) | 12 s |
| bookworm arm64 (toolchain cached) | 33 s | 10 s |
| trixie amd64 | 87 s | 11 s |
| trixie arm64 (toolchain cached) | 55 s | 10 s |
| `build-all.sh` | about 4 min | about 45 s |

Package sizes: 2.4 MB (amd64), 2.2 MB (arm64); installed about 7.6 MB.

If `docker build` fails with "error getting credentials", the credential helper named in `~/.docker/config.json` is not reachable. Point Docker at an empty config for the build: `mkdir -p /tmp/docker-config && echo '{}' >/tmp/docker-config/config.json && export DOCKER_CONFIG=/tmp/docker-config`.

## 3. Contents

| Path | Contents |
|---|---|
| `/usr/bin/stationd` | stationd built with `--features packaged`: the domain and root are compiled in; `[backend]`, `[endpoints]`, `[credentials]` and `state_dir` are refused |
| `/usr/lib/coaxhub/radiod/` | `radiod`, `rtlsdr.so`, `presets.conf`. Private, not on `PATH`. |
| `/usr/share/coaxhub/radiod-templates/` | `rtlsdr.conf`, `rtlsdr-hf.conf` (the development-only `siggen` is left out) |
| `/etc/coaxhub/stationd.toml` | conffile: station protocol §7's defaults (`[measurements] enabled = true` among them) and `[sdr] profile = "rtlsdr"` |
| `/usr/lib/systemd/system/` | `stationd.service`, `coaxhub-radiod.service` |
| `/usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules` | RTL-SDR device nodes for the `coaxhub-radiod` group only |
| `/usr/lib/modprobe.d/coaxhub-stationd.conf` | blacklists `dvb_usb_rtl28xxu` |
| `/usr/share/doc/coaxhub-stationd/copyright` | DEP-5: GPL-3.0-only; the ka9q-radio commit, its tarball and SHA-256, the `ARCHOPTS`; the Rust crates linked into stationd with their licences |

- Units, udev rules and modprobe files are under `/usr/lib` on both suites. Both are merged-`/usr` systems, where `/lib` is `/usr/lib`, and trixie's dpkg expects packages to ship files under `/usr` only.
- `Depends` comes from `dpkg-shlibdeps` (libc6, libfftw3-single3, libopus0, libiniparser1 or libiniparser4, libbsd0, librtlsdr0, libgcc-s1), plus `adduser` and `iproute2`. `Recommends: systemd-timesyncd | time-daemon`.

**Users and directories** (`postinst`):

| | owner | mode | |
|---|---|---|---|
| user `coaxhub` | | | stationd; system user, no shell, home `/var/lib/coaxhub` |
| user `coaxhub-radiod` | | | radiod; system user, no shell |
| `/var/lib/coaxhub` | root | 0755 | |
| `/var/lib/coaxhub/stationd` | coaxhub | 0700 | the station key, certificate, CA bundle, assignment (station protocol §7) |
| `/var/lib/coaxhub-radiod` | coaxhub-radiod | 0755 | radiod's `STATEDIR` (FFTW wisdom) |
| `/run/coaxhub/stationd` | coaxhub | 0755 | `status.json` (systemd `RuntimeDirectory`) |
| `/run/coaxhub-radiod` | coaxhub-radiod | 0755 | the rendered `radiod.conf` (systemd `RuntimeDirectory`) |

These are the paths stationd's packaged flavour uses (`crates/stationd/src/flavour.rs`).

## 4. The units

### `coaxhub-radiod.service`

| Setting | Why |
|---|---|
| `User=coaxhub-radiod` | radiod gets the SDR and nothing else; it never sees the station key |
| `RuntimeDirectory=coaxhub-radiod`, `StateDirectory=coaxhub-radiod` | `/run/coaxhub-radiod/radiod.conf` and FFTW wisdom, writable under `ProtectSystem=strict` |
| `ExecStartPre=+ip link set dev lo multicast on` | radiod sends with `IP_MULTICAST_IF=lo`; a stock `lo` lacks the MULTICAST flag and radiod would exit ([radiod notes §2](radiod-notes.md)). `+` runs only this command as root. |
| `ExecStartPre=/usr/bin/stationd backend-config --out /run/coaxhub-radiod/radiod.conf` | radiod's config is rendered from the owner's profile at every start; it is never edited |
| `ExecStart=/usr/lib/coaxhub/radiod/radiod /run/coaxhub-radiod/radiod.conf` | |
| `AmbientCapabilities=CAP_SYS_NICE`, `CapabilityBoundingSet=CAP_SYS_NICE` | realtime scheduling for the DSP threads, and no other capability |
| `IPAddressAllow=localhost multicast`, `IPAddressDeny=any` | radiod talks only to stationd, over multicast with TTL 0 on loopback |
| `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK` | libusb uses netlink for hotplug |
| `DevicePolicy=closed`, `DeviceAllow=char-usb_device rw` | USB device nodes only; the udev rule decides which ones the group may open |
| `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `RestrictSUIDSGID`, `LockPersonality`, `SystemCallArchitectures=native` | standard hardening radiod does not need to relax. Not set: `RestrictRealtime` (radiod needs it), `PrivateDevices` (it needs `/dev/bus/usb`), `MemoryDenyWriteExecute` (untested with its drivers). |
| `Restart=on-failure` | a radiod crash does not drop the tunnel; stationd reports the backend down and recovers |

`systemd-analyze security` rates it 3.4–3.6 ("OK").

### `stationd.service`

| Setting | Why |
|---|---|
| `User=coaxhub` | holds the station key; no device access, no capabilities |
| `Wants=`/`After=coaxhub-radiod.service` | radiod first; stationd copes with radiod starting later or restarting |
| `Wants=`/`After=network-online.target time-sync.target systemd-time-wait-sync.service` | a Pi has no clock battery: TLS and enrollment fail until NTP has set the clock. `postinst` enables `systemd-time-wait-sync.service` (on trixie it comes with `systemd-timesyncd`, which the package recommends), and `Wants=` starts it with stationd in any case. If NTP never syncs, stationd does not start. |
| `RuntimeDirectory=coaxhub/stationd` | `status.json` for `stationd status` |
| `Restart=on-failure`, `RestartPreventExitStatus=3` | exit 3 means not enrolled or revoked: restarting cannot help (station protocol §2.2) |
| `CapabilityBoundingSet=` (empty), `AmbientCapabilities=` | stationd never needs privileges |
| `ProtectSystem=strict`, `ReadWritePaths=/var/lib/coaxhub` | writes only its state directory (renewal) and runtime directory |
| `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK` | the tunnel, radiod's multicast, DNS |
| `PrivateDevices`, `PrivateTmp`, `ProtectHome`, `NoNewPrivileges`, `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`, `LockPersonality`, `MemoryDenyWriteExecute`, `SystemCallArchitectures=native`, `SystemCallFilter=@system-service ~@privileged @resources` | stationd is plain Rust and Tokio: no devices, no JIT, no privileged system calls |

`systemd-analyze security` rates it 1.5–1.7 ("OK").

Neither unit is enabled or started by the package: a fresh station is not enrolled, and stationd would exit 3. The installer (or the owner) enrolls it, then runs `systemctl enable --now coaxhub-radiod.service stationd.service`.

### udev and the DVB driver

- `72-coaxhub-rtlsdr.rules` gives RTL-SDR nodes (`0bda:2838`, `0bda:2832`) `root:coaxhub-radiod 0660` and removes the `uaccess` tag. It runs after `60-librtlsdr0.rules` (which would set `plugdev`) and `70-uaccess.rules` (which tags software radios for the desktop user), and before `73-seat-late.rules` applies that tag. The assignments use `:=` so later rules cannot widen them.
- The kernel's DVB-T driver `dvb_usb_rtl28xxu` claims RTL-SDR dongles. The blacklist applies from the next boot; the installer unloads the module if it is loaded, and `postinst` says so otherwise.

## 5. Maintainer scripts

| | |
|---|---|
| `postinst configure` | creates the users and directories (idempotent; existing credentials are never touched), enables `systemd-time-wait-sync.service` (and remembers that it did), reloads udev rules and re-triggers RTL-SDR devices, `daemon-reload`. On an **upgrade** it runs `systemctl try-restart` on both units, so only running units restart. |
| `prerm remove` | stops both units. `prerm upgrade` leaves them running until the new `postinst` restarts them. |
| `postrm remove` | `daemon-reload`. The configuration, the enrollment and the users stay, so reinstalling needs no new enrollment. |
| `postrm purge` | removes the units' enablement links, disables `systemd-time-wait-sync` if the package enabled it, removes `/etc/coaxhub`, `/var/lib/coaxhub` (the station key), `/var/lib/coaxhub-radiod`, the runtime directories, and both users and groups. |

Every script works without systemd running or installed (containers), and without udev.

## 6. The installer

`installer/station.sh` is a POSIX `sh` template (checked with `dash` and `shellcheck`). Whoever publishes it at `get.<domain>/station` replaces exactly two placeholders with `sed`:

| Placeholder | |
|---|---|
| `@COAXHUB_APT_URL@` | the repository, e.g. `http://apt.coaxhub.test` in a test, `https://apt.<domain>` in production |
| `@COAXHUB_KEY_FINGERPRINT@` | the repository signing key's fingerprint, 40 hex digits |

The script refuses to run while either is unreplaced, or when the URL or fingerprint is malformed. [releasing.md](releasing.md) says who fills them in.

What it does, in order:
1. **Checks**: root; `ID` or `ID_LIKE` is `debian`; `VERSION_CODENAME` is bookworm or trixie; `dpkg --print-architecture` is amd64 or arm64 (a 32-bit Raspberry Pi OS gets a hint to install the 64-bit one).
2. **SDR**: looks for `0bda:2838` or `0bda:2832` under `/sys/bus/usb/devices` and asks for the profile (1: `rtlsdr`, the default; 2: `rtlsdr-hf`). Without an SDR it asks whether to continue. All questions are read from `/dev/tty`, because stdin is the script itself under `curl | sudo sh`; `main` redirects its own stdin from `/dev/null`, so no command it runs can swallow the rest of the script. Without a terminal, `--profile` and `--yes` answer them.
3. **Repository key**: installs `gpg` and `curl` if missing, downloads `<APT_URL>/coaxhub-archive-keyring.asc` into a temporary directory and checks, with `gpg --show-keys --with-colons` in a throwaway `GNUPGHOME`, that it holds **exactly one** primary key whose fingerprint is the pinned one (an extra key appended to the file is refused). Only then does it write the dearmored keyring to `/usr/share/keyrings/coaxhub-archive-keyring.gpg` and the deb822 source `/etc/apt/sources.list.d/coaxhub.sources` with `Signed-By:` that keyring alone. A mismatch stops before anything is written. The initial download is protected by HTTPS in production; everything after it by the pinned key.
4. **Package**: `apt-get update`, `apt-get install coaxhub-stationd` with `--force-confold`, so an owner's edited `stationd.toml` is kept on upgrades. It unloads `dvb_usb_rtl28xxu` if loaded.
5. **Profile**: sets `[sdr] profile` in `/etc/coaxhub/stationd.toml` with `awk`, keeping every other line, then runs `stationd check-config`.
6. **Clock**: waits up to 2 minutes for `timedatectl show -p NTPSynchronized --value` to say `yes`. Without `timedatectl` it stops unless `--skip-time-sync` is given (containers without systemd).
7. **Enrollment**: skipped when `stationd status` says the station is enrolled, unless `--reenroll` (which passes `--force`). stationd runs as `coaxhub` (`runuser`), so it writes its own files. With a terminal, stationd's standard input **is** the terminal: stationd turns echo off, then prompts, reads the token and zeroes it after use. The token never passes through the shell, an argument, the environment or a log. stationd exits 4 when the token is refused, malformed or empty; the installer then asks again, at most 3 times. With `--token-file`, the file is stationd's standard input. Any other failure is reported with stationd's message and the installer exits 1. The installer saves the terminal's settings first and restores them on exit, so a Ctrl-C at the hidden prompt never leaves echo off.
8. **Services**: `systemctl enable` and `restart` of both units (a restart, so a changed profile takes effect), unless `--no-start`, then `stationd status`.

Running it again upgrades the package, keeps the enrollment and applies a new `--profile`.

`COAXHUB_INSTALL_USB_DEVICES` replaces `/sys/bus/usb/devices` for SDR detection; it exists for the tests (`sudo` drops it anyway).

## 7. Tests

```sh
packaging/test/test-packages.sh [--quick]     # the packages; --quick: bookworm only
packaging/test/test-installer.sh [--quick]    # the installer
```

Both need the four packages in `target/packages` and a Docker daemon, and nothing else: the tests copy files into `debian:<suite>` containers with `docker cp`, bind no host ports and mount nothing. Inside, checks print TAP-like `ok N - …` lines.

**`test-packages.sh`**, per suite:
- **Artefacts** (both architectures, not installed, so arm64 is checked on an amd64 host): control fields, the `~debN` version, `Depends`, the file list, root ownership, no PEM file shipped, the source tarball next to the packages with the SHA-256 the copyright file names, and `file` on stationd, radiod and `rtlsdr.so`: ELF 64-bit, `x86-64` or `ARM aarch64`, stripped.
- **Install, amd64**, twice: on the bare image (no systemd) and with systemd installed. In a clean container: `apt-get install ./coaxhub-stationd_*.deb` resolves every dependency; `dpkg --verify`; users, groups, modes and paths as in §3; radiod not on `PATH`; the conffile; the copyright file; `stationd --version`; `check-config` says "not enrolled yet" (exit 0), `status` and `run` exit 3; `backend-config` renders both profiles with the fixed groups and TTL 0, also as `coaxhub-radiod` into `/run/coaxhub-radiod`; the packaged build refuses `siggen` and `[backend]`; `radiod -V` reports the pinned commit; every shared library resolves; radiod, run on the rendered config without an SDR, loads its presets and driver from `/usr/lib/coaxhub/radiod` and stops at the missing device. With systemd: `systemd-analyze verify` and `security` (exposure ≤ 2.5 and ≤ 4.5), `systemd-time-wait-sync` enabled. Then a reinstall keeps credentials, modes and an edited `stationd.toml`; `remove` keeps configuration, credentials and users; `purge` leaves no file, user, group, enablement link or `/etc` mention behind and disables `systemd-time-wait-sync` again.

**`test-installer.sh`**, per suite: a signing key generated in the test, a repository built with `apt-ftparchive` (`Release`, `InRelease`, `Release.gpg`, amd64 and arm64) and served by `python3 -m http.server` on `127.0.0.1:28080` inside the container. The installer is filled in with `sed`, as its publisher does.
- Refusals, each leaving no keyring, source or package behind: the unfilled template; `--token` on the command line; not root; an unknown profile; Debian bullseye, Ubuntu, Fedora; armhf and i386 (a `dpkg` shim); no SDR without a terminal or `--yes`; a wrong fingerprint; a key file with a second key appended.
- The happy path up to enrollment, with the script piped into `sh -s --` as `curl | sudo sh` does (`--token-file`, `--skip-time-sync`, `--no-start`, a fake RTL-SDR): SDR detected, keyring and source written, package installed, profile written once with the rest of the file kept, and enrollment failing cleanly (no control plane) with stationd's reason and exit 1; the token appears nowhere in the output.
- On a pseudo-terminal, as a person runs it (`in-container/interactive.py`): the profile question, a refused token asking again ("2 of 3"), three refused tokens stopping, "Continue anyway?" answered no; no token is ever echoed.
- Re-runs: keyring and source unchanged, one source file, a new profile applied; a malformed token file; with fake credentials (openssl), a re-run skips enrollment, waits for NTP through a `timedatectl` shim and exits 0, and stops when `timedatectl` is missing; `--reenroll` enrolls again, and a failed attempt keeps the old credentials.

Each suite's installer test takes about 2.5 minutes and each package test about 1 minute, mostly `apt-get`.

**Not covered by the container tests**: the units running under a real systemd (the sandbox, `IPAddressDeny`, `DevicePolicy`, the `+` `ExecStartPre`) and radiod with a real RTL-SDR. A container that boots systemd needs `--privileged`, which would share the host's kernel modules and udev with the test; a run on a real Raspberry Pi covers these instead.
