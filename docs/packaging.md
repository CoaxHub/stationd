<!-- SPDX-License-Identifier: GPL-3.0-only -->

# The station package and installer

A station is one package, `coaxhub-stationd` (a `.deb`, built per supported system), installed by a short script ([installer/station.sh](../installer/station.sh)). The package holds stationd, a private copy of radiod with its own librtlsdr, `coaxhub-helper` (the small root helper through which stationd starts the receivers) and their systemd units, so there is nothing else to install. It holds no settings: the receivers are set up in the web app and the hub sends them to stationd ([architecture](architecture.md#hardware-and-receivers)). This document covers how the package is built, what it contains, why each systemd setting is there, and how the package and the installer are tested.

## 1. Targets and versions

| Suite | System | Package version suffix | Packages |
|---|---|---|---|
| `bookworm` | Debian 12; Raspberry Pi OS 12 | `~deb12` | `coaxhub-stationd_<v>-<r>~deb12_{amd64,arm64}.deb` |
| `trixie` | Debian 13; Raspberry Pi OS 13 | `~deb13` | `…~deb13_{amd64,arm64}.deb` |
| `jammy` | Ubuntu 22.04 LTS | `~ubuntu22.04` | `…~ubuntu22.04_{amd64,arm64}.deb` |
| `noble` | Ubuntu 24.04 LTS | `~ubuntu24.04` | `…~ubuntu24.04_{amd64,arm64}.deb` |
| `resolute` | Ubuntu 26.04 LTS | `~ubuntu26.04` | `…~ubuntu26.04_{amd64,arm64}.deb` |

Five suites on `amd64` and `arm64` make ten packages per release. The suites, their images, suffixes and paths are in one file, [packaging/suites.sh](../packaging/suites.sh), which the build and the tests read.

- `<v>` is stationd's crate version and `<r>` is `PACKAGE_REVISION` (default 1). The suffix orders a release upgrade within a family: `0.1.0-1~deb12` < `0.1.0-1~deb13` < `0.1.0-1`, and `~ubuntu22.04` < `~ubuntu24.04` < `~ubuntu26.04`.
- Raspberry Pi OS is Debian: its suite is the Debian codename (`ID=raspbian` on the 32-bit image, `ID=debian` on the 64-bit one), and the 64-bit image is arm64. The Pi 3B (Cortex-A53) is the oldest target.
- **Only maintained releases.** A release is a target while its distribution publishes security updates for it under standard support. For Debian that is the security team or Debian LTS; Freexian's paid ELTS doesn't count. For Ubuntu LTS it is the main archive's five years; ESM and Ubuntu Pro don't count, for the same reason. A new Debian stable or Ubuntu LTS is added in the first stationd release after it comes out. When a release's support ends, the next stationd release drops it: `build-all.sh` stops building it, the installer refuses it (§6), and packages already in the repository stay but get no new versions. Nothing is built for a release past its end, even for an existing station whose owner would rather not reinstall.
- Today: bookworm (Debian LTS until June 2028), trixie, and Ubuntu jammy (standard support until May 2027), noble (May 2029) and resolute (May 2031). bullseye's LTS ended on 2026-08-31 and Ubuntu 20.04's standard support in May 2025, so both are refused; Ubuntu's non-LTS releases are never targets.
- **Derivatives are refused.** Linux Mint, Pop!_OS, Zorin and other systems that name Ubuntu or Debian only in `ID_LIKE` or `UBUNTU_CODENAME` are not supported: only `ID=ubuntu` and `ID=debian` or `raspbian` with a supported codename are. They are not tested, and their kernel, udev and systemd may differ from the suite's.
- Each stationd release pins exactly one ka9q-radio commit, the one its TLV table was generated from: `384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd`.
- It also pins one osmocom librtlsdr commit, the same on every suite and architecture: tag v2.0.2, `619ac3186ea0ffc092615e1f59f7397e5e6f668c` (§3, [the bundled librtlsdr](#the-bundled-librtlsdr)).

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
bookworm/rtl-sdr-619ac318.tar.gz        the corresponding source of the bundled librtlsdr
trixie/coaxhub-stationd_0.1.0-1~deb13_amd64.deb
trixie/coaxhub-stationd_0.1.0-1~deb13_arm64.deb
trixie/ka9q-radio-384b269a.tar.gz       the same files
trixie/rtl-sdr-619ac318.tar.gz
```

### How

`build-deb.sh` needs only `git` and Docker with BuildKit (`docker build` with buildx). Everything else happens in containers of the suite's own system (`debian:bookworm`, `ubuntu:22.04` and so on) on the Docker daemon, driven by [packaging/deb/Dockerfile](../packaging/deb/Dockerfile) with the repository root as the build context. Nothing is bind-mounted and the result comes back with `--output type=local`, so it works the same on a CI runner, on a workstation and with a remote Docker daemon.

1. **The radiod and librtlsdr sources.** Each pinned commit is fetched once (`git fetch --depth 1`) into `target/packaging/ka9q-radio.git` and `target/packaging/rtl-sdr.git`, and `git archive | gzip -n` makes `ka9q-radio-384b269a.tar.gz` and `rtl-sdr-619ac318.tar.gz`, the same bytes on every run. radiod and librtlsdr are built **from those tarballs**, each a named build context (`ka9q`, `rtlsdr`), so the published source is exactly what was compiled: for radiod, the tarball with our patches, which `build-deb.sh` copies next to it as `ka9q-radio-384b269a-<name>.patch`.
2. **Stage `toolchain`** (the suite's image, `BASE`; cached as an image layer per suite): the native and `aarch64-linux-gnu` C toolchains; the `-dev` libraries radiod and librtlsdr link, for amd64 and, through multiarch, arm64 (`libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libusb-1.0-0-dev`; the suite's `librtlsdr-dev` is not used); `cmake`; `protobuf-compiler` for station-proto; `dpkg-dev`; Rust from rustup with both targets. The build needs no arm64 emulation: arm64 is cross-compiled.
   On **Ubuntu**, `archive.ubuntu.com` has no arm64, so the stage limits the image's own sources to `amd64` (the one-line `sources.list` of jammy and the deb822 `ubuntu.sources` of noble and resolute) and adds `ports.ubuntu.com/ubuntu-ports` for `arm64` before `dpkg --add-architecture arm64`. The same `-dev` packages and the same cross toolchain serve every suite (gcc 11 on jammy, 13 on noble). jammy's protoc is 3.12.4, which takes proto3 `optional` fields only with `--experimental_allow_proto3_optional`: where protoc is older than 3.15, a wrapper adds the flag (newer ones accept and ignore it). The runtime libraries `dpkg-shlibdeps` finds (libiniparser, libfftw3-single3, libopus0, libbsd0, libusb-1.0-0) are all in `main` on the Ubuntu suites, never `universe`, and the tests install the package with only `main` and `restricted` enabled; the suites' own librtlsdr is never used (ADR-0050).
3. **Stage `librtlsdr`** ([the bundled librtlsdr](#the-bundled-librtlsdr)): checks the tarball's SHA-256 against the one `build-deb.sh` computed (the one the copyright file names), then builds only the shared library with CMake (`-DCMAKE_BUILD_TYPE=Release`, no udev rule, no zero-copy, and `DETACH_KERNEL_DRIVER` on as in Debian's build; for arm64 the cross compiler and the arm64 `pkg-config` directory). The library, `librtlsdr.so.0` (its SONAME), is stripped, and its two public headers go to radiod's stage.
4. **Stage `radiod`**: applies our patches, `packaging/patches/ka9q-radio/*.patch` in order, to the unpacked tarball (`patch -p1`; one that doesn't apply fails the build), then `make radiod`, then `make rtlsdr.so` against the private librtlsdr (its headers, and `-Wl,--enable-new-dtags,-rpath,/usr/lib/coaxhub/radiod`, so `rtlsdr.so` has `RUNPATH` `/usr/lib/coaxhub/radiod` and `NEEDED` `librtlsdr.so.0`; the stage checks both), with every hardware driver off except `ENABLE_RTLSDR=1`.
   - `ARCHOPTS` is always passed. Upstream's `-march=native` would tie the binary to the build machine. The defaults are `-march=x86-64-v2 -mtune=generic` for amd64 (any x86-64 CPU since about 2009) and `-march=armv8-a -mtune=cortex-a53` for arm64 (every 64-bit Pi).
   - `config_paths.h` is written by the Dockerfile instead of upstream's Makefile rule, which asks git (the tarball has no `.git`). It holds the same commit, date and summary, the branch ` (coaxhub-stationd, patches 0001,…)` that names the patches applied (`radiod -V` and radiod's log show it), and the package's private paths: `PKGLIBDIR`, `PKGDATADIR` and `CONFDIR` are `/usr/lib/coaxhub/radiod` (so a `/etc/radio` from another radiod install never overrides our presets), and `STATEDIR` is `/var/lib/coaxhub-radiod` (FFTW wisdom).
   - Upstream's Makefile calls a bare `ar`; the build puts the target's `ar` first on `PATH`.
   - radiod and `rtlsdr.so` are stripped.
5. **Stage `stationd`**: `cargo build --release --locked -p stationd --features packaged --target <triple>` with `COAXHUB_DOMAIN` and the root certificate (a named build context holding only `root.pem`), and `cargo build --release --locked -p coaxhub-helper --target <triple>` for the helper. The cargo registry and the target directory are BuildKit cache mounts, so a rebuild compiles only what changed. `cargo tree` lists the linked crates for the copyright file.
6. **Stage `package`**: [packaging/deb/assemble.sh](../packaging/deb/assemble.sh) lays out the file tree (for the suite: where the units go, and the `preinst` filled in with the system the package is for), runs `dpkg-shlibdeps` for the target architecture on the five ELF files (stationd, `coaxhub-helper`, radiod, `rtlsdr.so`, `librtlsdr.so.0`), checks that `Depends` names libusb and no librtlsdr, writes `control` and `md5sums` (the package has no conffile), sets every mtime to the last commit's time, and runs `dpkg-deb --root-owner-group -Zxz`.

The `ARCHES` variable of `build-all.sh` (default `amd64 arm64`) limits the architectures built.

Build times on a 16-thread x86-64 development machine:

| | first build | stationd changed |
|---|---|---|
| bookworm amd64 | about 75 s (toolchain image about 40 s, stationd 37 s) | 12 s |
| bookworm arm64 (toolchain cached) | 33 s | 10 s |
| trixie amd64 | 87 s | 11 s |
| trixie arm64 (toolchain cached) | 55 s | 10 s |
| an Ubuntu suite, amd64 then arm64 | about 2.5 min for the pair (the toolchain image with both architectures' libraries is most of it) | 20 s |
| `build-all.sh`, ten packages | about 15 min | about 2 min |

Package sizes: 2.5 MB (amd64), 2.2 MB (arm64); installed about 7.9 MB.

If `docker build` fails with "error getting credentials", the credential helper named in `~/.docker/config.json` is not reachable. Point Docker at an empty config for the build: `mkdir -p /tmp/docker-config && echo '{}' >/tmp/docker-config/config.json && export DOCKER_CONFIG=/tmp/docker-config`.

## 3. Contents

| Path | Contents |
|---|---|
| `/usr/bin/stationd` | stationd built with `--features packaged`: the domain and root are compiled in, the SDR table, the profiles and the capacity claims are fixed, and it reads no configuration file (no `--config`, no `check-config`) |
| `/usr/lib/coaxhub/coaxhub-helper` | the root helper (§4): started by `coaxhub-helper@.service` for each connection on its socket. Private, not on `PATH`. |
| `/usr/lib/coaxhub/radiod/` | `radiod`, `rtlsdr.so`, `librtlsdr.so.0` (below), `presets.conf`. Private, not on `PATH`. |
| `/usr/share/coaxhub/radiod-templates/` | `rtlsdr.conf`, `rtlsdr-hf.conf`, `rtlsdr-v4.conf` (the development-only `siggen` is left out) |
| `/usr/lib/systemd/system/` | `stationd.service`, `coaxhub-radiod@.service` (the template unit: one instance per receiver), `coaxhub-helper.socket` and `coaxhub-helper@.service` (the helper, §4) |
| `/usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules` | RTL-SDR device nodes for the `coaxhub-radiod` group only |
| `/usr/lib/modprobe.d/coaxhub-stationd.conf` | blacklists `dvb_usb_rtl28xxu` |
| `/usr/share/doc/coaxhub-stationd/copyright` | DEP-5: GPL-3.0-only; the ka9q-radio commit, its tarball and SHA-256, our patches to it each with its SHA-256, the `ARCHOPTS`; librtlsdr's version, commit, tarball and SHA-256, its copyright holders and GPL-2.0-or-later; the Rust crates linked into stationd with their licences |

- Units, udev rules and modprobe files are under `/usr/lib` on every suite but jammy, where they are under `/lib` (the table shows `/usr/lib`; jammy's package ships the same files at `/lib/systemd/system/…`, `/lib/udev/rules.d/…` and `/lib/modprobe.d/…`). jammy's own systemd package installs under `/lib`, its kmod 29 reads `/lib/modprobe.d` and not `/usr/lib/modprobe.d`, and on a jammy that is not merged-`/usr` (one upgraded from an old release) the two directories are different, so the DVB blacklist would be missed. On a merged system `/lib` is `/usr/lib`; trixie's dpkg expects packages to ship under `/usr`, so the Debian suites and noble and resolute keep `/usr/lib`. `/usr/lib/coaxhub/` and `/usr/bin/stationd` are the same everywhere.
- **`Pre-Depends: base-files (>= …)`**, the first `base-files` version of the target release (`12`, `13`, `12ubuntu1`, `13ubuntu1`, `14ubuntu1`): during an in-place release upgrade `apt` unpacks `base-files` first, so `/etc/os-release` already names the new release when the `preinst` reads it (§5).
- `Depends` comes from `dpkg-shlibdeps` (libc6, libfftw3-single3, libopus0, libiniparser1 or libiniparser4, libbsd0, libusb-1.0-0, libgcc-s1), plus `adduser`, `iproute2` and `dbus | dbus-system-bus`: `stationd` reads the receivers' units' state with `systemctl show`, which as a non-root user talks to systemd over the system bus (§4). The package depends on no polkit and ships no polkit rule: starting a unit goes through `coaxhub-helper` instead. No librtlsdr package is a dependency, and one installed on the system is never used. `Recommends: systemd-timesyncd | time-daemon`.

### The bundled librtlsdr

radiod's RTL-SDR driver, `rtlsdr.so`, links osmocom librtlsdr. The package builds its own, v2.0.2 (commit `619ac3186ea0ffc092615e1f59f7397e5e6f668c` of `https://gitea.osmocom.org/sdr/rtl-sdr.git`, mirrored at `https://github.com/osmocom/rtl-sdr`), on every suite and architecture, and installs it as `/usr/lib/coaxhub/radiod/librtlsdr.so.0`. `rtlsdr.so` finds it there through its `RUNPATH`, never through the system's library path.

- **Why.** The RTL-SDR Blog V4 (an R828D tuner with a built-in HF upconverter that the library switches by GPIO) needs librtlsdr 2.0.0 or newer. bookworm ships 0.6.0, with which a V4 receives no signals or the wrong frequencies; trixie ships 2.0.2. One library on every suite is one code path to test, and a V3 or any other RTL-SDR works the same with it on both suites.
- **HF on a V4.** The library switches the upconverter in below 28.8 MHz, so a V4 tunes to the HF frequency itself and its tuner's gain is in the signal path; direct sampling receives nothing on it. ka9q-radio's `rtlsdr.c` passes the frequency through and sets direct sampling only when the config asks for it, so the `rtlsdr-v4` profile is the V4's whole support. A V4 identifies itself by its USB strings (manufacturer `RTLSDRBlog`, product `Blog V4`) and has the usual IDs, `0bda:2838`.
- **Corresponding source.** `rtl-sdr-619ac318.tar.gz`, next to the packages and in the release, with its SHA-256 in the copyright file. Its licence is GPL-2.0-or-later, which combines with radiod's GPL-3.
- **Updates.** The pin moves with osmocom's releases, security fixes in particular, as radiod's does ([releasing.md](releasing.md)).
- **What the package does not take from it**: its utilities (`rtl_sdr`, `rtl_tcp` and the rest) and its udev rule. The package's own udev rule and DVB blacklist (§4) stand on their own; the library's kernel-driver detaching, built in as Debian builds it, frees a dongle the DVB driver still holds before the reboot that applies the blacklist.

**Users and directories** (`postinst`):

| | owner | mode | |
|---|---|---|---|
| user `coaxhub` | | | stationd; system user, no shell, home `/var/lib/coaxhub` |
| user `coaxhub-radiod` | | | radiod; system user, no shell |
| `/var/lib/coaxhub` | root | 0755 | |
| `/var/lib/coaxhub/stationd` | coaxhub | 0700 | the station key, certificate, CA bundle, assignment (station protocol §7) |
| `/var/lib/coaxhub/receivers` | coaxhub (group coaxhub-radiod) | 2750 | `receivers.json`, the applied receiver set (0640, group `coaxhub-radiod`, so each instance can render its config from it; station protocol §7). The directory is set-group-ID: stationd writes the file as `coaxhub` and can't chown it (`stationd.service` filters `@privileged`), so the file takes the directory's group. Empty on a new station. |
| `/var/lib/coaxhub-radiod` | coaxhub-radiod | 0755 | radiod's `STATEDIR` (FFTW wisdom) |
| `/run/coaxhub/stationd` | coaxhub | 0755 | `status.json` (systemd `RuntimeDirectory`) |
| `/run/coaxhub-radiod/<receiver_id>` | coaxhub-radiod | 0755 | one receiver's rendered `radiod.conf` (systemd `RuntimeDirectory=coaxhub-radiod/%i`) |

These are the paths stationd's packaged flavour uses (`crates/stationd/src/flavour.rs`).

## 4. The units

### `coaxhub-radiod@.service`

A template unit: `coaxhub-radiod@<receiver_id>.service` is the radiod of one receiver, and `%i` is the receiver's ID. `stationd` starts, restarts and stops the instances through `coaxhub-helper` ([below](#how-stationd-starts-the-receivers)); nothing enables them.

| Setting | Why |
|---|---|
| `User=coaxhub-radiod` | radiod gets the SDRs and nothing else; it never sees the station key |
| `RuntimeDirectory=coaxhub-radiod/%i`, `StateDirectory=coaxhub-radiod` | `/run/coaxhub-radiod/%i/radiod.conf` for this instance and FFTW wisdom, writable under `ProtectSystem=strict` |
| `ExecStartPre=+ip link set dev lo multicast on` | radiod sends with `IP_MULTICAST_IF=lo`; a stock `lo` lacks the MULTICAST flag and radiod would exit ([radiod notes §2](radiod-notes.md)). `+` runs only this command as root. |
| `ExecCondition=/usr/bin/stationd backend-config --receiver %i --check` | a receiver that is not in `receivers.json`, or whose config cannot be rendered, is not started and not retried (a skipped condition is not a failure, so `Restart=on-failure` leaves it); the unit is skipped (`inactive`, with `Skipped due to 'exec-condition'` and the renderer's reason in the journal), which stationd reports as `its unit stopped`. It is what stops a request for an id the station does not have from failing the renderer every 5 s for ever (§4, the helper). |
| `ExecStartPre=/usr/bin/stationd backend-config --receiver %i` | radiod's config is rendered from the template of the receiver's profile and `receivers.json` at every start, with the SDR's serial, the centre, the settings and the multicast groups of the receiver's slot, into `/run/coaxhub-radiod/%i/radiod.conf`; it is never edited |
| `ExecStart=/usr/lib/coaxhub/radiod/radiod /run/coaxhub-radiod/%i/radiod.conf` | |
| `AmbientCapabilities=CAP_SYS_NICE`, `CapabilityBoundingSet=CAP_SYS_NICE` | realtime scheduling for the DSP threads, and no other capability |
| `IPAddressAllow=localhost multicast`, `IPAddressDeny=any` | radiod talks only to stationd, over multicast with TTL 0 on loopback |
| `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK` | libusb uses netlink for hotplug |
| `DevicePolicy=closed`, `DeviceAllow=char-usb_device rw` | USB device nodes only; the udev rule decides which ones the group may open |
| `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `RestrictSUIDSGID`, `LockPersonality`, `SystemCallArchitectures=native` | standard hardening radiod does not need to relax. Not set: `RestrictRealtime` (radiod needs it), `PrivateDevices` (it needs `/dev/bus/usb`), `MemoryDenyWriteExecute` (untested with its drivers). |
| `Restart=on-failure` | a radiod crash does not drop the tunnel or the station's other receivers; stationd reports that receiver's backend down and recovers |

`systemd-analyze security` rates it 3.4–3.6 ("OK").

### `stationd.service`

| Setting | Why |
|---|---|
| `User=coaxhub` | holds the station key; no device access, no capabilities |
| `Wants=`/`After=network-online.target time-sync.target systemd-time-wait-sync.service` | a Pi has no clock battery: TLS and enrollment fail until NTP has set the clock. `postinst` enables `systemd-time-wait-sync.service` (on trixie it comes with `systemd-timesyncd`, which the package recommends), and `Wants=` starts it with stationd in any case. If NTP never syncs, stationd does not start. |
| `RuntimeDirectory=coaxhub/stationd` | `status.json` for `stationd status` |
| `Restart=on-failure`, `RestartPreventExitStatus=3` | exit 3 means not enrolled or revoked: restarting cannot help (station protocol §2.2) |
| `CapabilityBoundingSet=` (empty), `AmbientCapabilities=` | stationd never needs privileges |
| `ProtectSystem=strict`, `ReadWritePaths=/var/lib/coaxhub` | writes only its state directory (renewal) and runtime directory |
| `Wants=`/`After=coaxhub-helper.socket` | the socket stationd asks to start the receivers' units ([below](#how-stationd-starts-the-receivers)). `Wants=`, not `Requires=`: when the helper cannot run, stationd still runs (the tunnel, the reports, `doctor`) and says why no receiver starts. Restarting stationd restarts a failed socket. |
| `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK` | the tunnel, radiod's multicast, DNS; `AF_UNIX` is the helper's socket (`/run/coaxhub-helper.sock`) and the system bus socket (`systemctl show` reads the receivers' state through it); connecting to either needs no capability |
| `PrivateDevices`, `PrivateTmp`, `ProtectHome`, `NoNewPrivileges`, `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`, `LockPersonality`, `MemoryDenyWriteExecute`, `SystemCallArchitectures=native`, `SystemCallFilter=@system-service ~@privileged @resources` | stationd is plain Rust and Tokio: no devices, no JIT, no privileged system calls |

`systemd-analyze security` rates it 1.5–1.7 ("OK").

None of these units is enabled or started by the package: a fresh station is not enrolled, and stationd would exit 3. The installer (or the owner) enrolls it, then runs `systemctl enable --now stationd.service`. **That is the only unit anyone enables.** The `coaxhub-radiod@` instances have no `[Install]` section and are never enabled; `stationd` starts them. Nor has `coaxhub-helper.socket`: `stationd.service` wants it.

### `coaxhub-helper.socket` and `coaxhub-helper@.service`

`stationd` runs as `coaxhub` with no capability, and systemd lets only root, or polkit's say-so, start a unit. The package uses no polkit: `coaxhub-helper` is a small root program that does exactly what the polkit rule it replaces allowed, and nothing else. It is the station's one privileged door: a later privileged operation is a new verb of this helper, added by its own decision, never a second helper.

`coaxhub-helper.socket` listens on `/run/coaxhub-helper.sock`, owner `root`, group `coaxhub`, mode 0660, with `Accept=yes`, `MaxConnections=4` (at most four connections served at once; `stationd` asks one thing at a time) and `TriggerLimitIntervalSec=2s`, `TriggerLimitBurst=200` (systemd's own defaults, set explicitly: a socket that gets more than 200 connections in 2 s fails until `stationd.service` is restarted, which wants it again), and has no `[Install]` section. Each connection starts one `coaxhub-helper@<n>-<pid>-<uid>.service`, which serves it and exits.

| Setting of `coaxhub-helper@.service` | Why |
|---|---|
| `ExecStart=/usr/lib/coaxhub/coaxhub-helper`, `StandardInput=socket`, `StandardOutput=socket`, `StandardError=journal`, `RuntimeMaxSec=15` | the connection is its standard input and output; every request goes to the journal; a hung instance is stopped |
| runs as root, `CapabilityBoundingSet=` (empty), `NoNewPrivileges=yes` | starting a unit takes root; the empty bounding set drops what root is not needed for. This narrows what a hijacked helper could do by other means. It is **not** the boundary: root on systemd's private socket can ask systemd to run anything, so what keeps the helper to three verbs is the parser (one short line, canonical ids, a fixed `systemctl` argument list, the peer's UID checked first), which is small, has no `unsafe` code and is fuzzed. |
| `ProtectSystem=strict` (no `ReadWritePaths`), `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `InaccessiblePaths=-/etc/shadow -/etc/gshadow -/var/lib/coaxhub -/var/lib/coaxhub-radiod` | the helper writes no file and has no use for the station's state or the shadow files |
| `PrivateNetwork=yes`, `RestrictAddressFamilies=AF_UNIX`, `IPAddressDeny=any` | the connection (passed in) and systemd's socket (a path); no network |
| `ProtectProc=invisible`, `ProcSubset=pid` | no view of other processes |
| `CollectMode=inactive-or-failed` | an instance that `RuntimeMaxSec` kills does not stay loaded as failed |
| `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`, `LockPersonality`, `MemoryDenyWriteExecute`, `SystemCallArchitectures=native`, `SystemCallFilter=@system-service ~@privileged @resources`, `UMask=0077` | the same hardening as `stationd.service` |

`systemd-analyze security` rates it 0.7–1.5 ("OK", by suite) (for an instance such as `coaxhub-helper@0-1.service`).

**The protocol** (`crates/coaxhub-helper/src/protocol.rs`): one request line, ASCII, at most 64 bytes with its newline, read within 2 s: `<verb> <id>\n`.

- The verbs are `start`, `restart`, `stop` (each with an id) and `ping` (without one). `<id>` is the receiver's number in canonical decimal, `[1-9][0-9]*`, at most `4294967295`. Canonical matters: `coaxhub-radiod@011` would be a different instance from `@11`.
- Exactly one space and nothing after the id; no carriage return, no tab, no byte outside printable ASCII. Anything else is `error bad request`, and nothing runs. The helper never echoes a request in the reply or the journal.
- **Who may ask.** The socket's owner and mode are the first gate. The second: the peer's UID, from `SO_PEERCRED`, must be `coaxhub`'s (read from `/etc/passwd`) or 0 (`sudo stationd doctor`). Another peer gets `error not permitted`, and the helper reads nothing from it.
- **What it does.** `ping` does nothing. `start` and `restart` first count the *other* `coaxhub-radiod@*` units that are running or starting (active, activating, which includes waiting to restart, or deactivating: `systemctl list-units`, so from systemd itself, never from a file `stationd` writes); with 16 or more (a station has at most 16 receivers) the answer is `error too many receivers: 16 other coaxhub-radiod units are running (at most 16)` and nothing runs. A failed unit runs nothing and is not counted: `stop` does not clear a failed state, so counting it could fill the limit with units nobody can remove. `stop` is never refused. Then it runs `/usr/bin/systemctl --no-block <verb> coaxhub-radiod@<id>.service` (`/bin/systemctl` on a system that is not merged-`/usr`), with an empty environment. Each `systemctl` call is killed after 5 s: with the 2 s to read the request, a request takes 12 s at the most, and `RuntimeMaxSec=15` ends the instance in any case. `stationd` waits 20 s for the answer, longer than that, so it never reports a receiver `failed` while the helper is still carrying out the request.
- **The reply** is `ok\n`, or `error <detail>\n`: the detail is `systemctl`'s stderr as one line of at most 256 bytes, in the form `systemctl --no-block start coaxhub-radiod@3.service: ...`. It is what reaches the receiver's state in stationd and the owner on the station's page.
- **The journal** gets one line per connection: the peer's UID and the outcome (`start 11: ok`, `refused: bad request (unknown verb)`).

The helper writes no file and reads none that `stationd` can touch (it reads `/etc/passwd` for the `coaxhub` UID; if `coaxhub` is not in it, only root may ask): root never parses what the unprivileged side writes (`receivers.json` is rendered, as `coaxhub-radiod`, by the unit's own `ExecStartPre`).

**What bounds the number of units.** The ids are the hub's, any `u32`, so the helper cannot know which are real; it bounds how many units can exist at once (the count above), and the template unit bounds what one unit does: `ExecCondition=/usr/bin/stationd backend-config --receiver %i --check` runs the unprivileged renderer first, and when the receiver is not in `receivers.json` or its config cannot be rendered the unit does not start and is **not retried** (a skipped condition is not a failure, so `Restart=on-failure` leaves it). Without it a request for an id the station does not have would fail the renderer every 5 s for ever, in an instance per id. The template has no `StartLimit` settings on purpose: `stationd` does not restart a failed receiver by itself (its monitor only follows systemd's own restarts), so a start limit that gave up on a radiod whose SDR was busy for a minute would leave the receiver failed until the next change from the hub. Recovery therefore stays as it was: a radiod that exits is restarted by systemd every 5 s, and `stationd` stops it (`NO_DEVICE`) when its SDR is unplugged and starts it, through the helper, when the SDR is plugged in; a refused config is tried again the next time the hub changes that receiver, since `stationd` restarts the receivers it changes.

### How stationd starts the receivers

- At boot, `stationd` reads `receivers.json` and starts `coaxhub-radiod@<receiver_id>.service` for each receiver in it whose SDR is plugged in, before it connects to the hub. A receiver therefore runs while the tunnel is down. Afterwards it starts, restarts and stops instances as `ConfigureReceivers` changes the set, stops one when its SDR is unplugged, and starts it when the SDR is plugged in.
- It does so by sending `start`, `restart` or `stop` and the receiver's number to `coaxhub-helper` (above). Reading state needs no authorization: `systemctl show` for the unit's state and the unit's cgroup for its CPU time are as before.
- The helper's answer reaches `receiver_state` as before. When it cannot be reached the receiver is `failed` with one of: `cannot reach coaxhub-helper (/run/coaxhub-helper.sock): <error>; run sudo stationd doctor`, `coaxhub-helper closed the connection without an answer`, `coaxhub-helper timed out`. `stationd doctor` has a `helper` check: it sends `ping` and passes on `ok`; otherwise it fails with the cause, the state of `coaxhub-helper.socket` and the fix, `sudo systemctl restart stationd`.
- `stationd` never edits a unit or a running radiod's config. A change to a receiver restarts its instance, and `ExecStartPre` renders the config again from `receivers.json`.

### udev and the DVB driver

- `72-coaxhub-rtlsdr.rules` gives RTL-SDR nodes (`0bda:2838`, `0bda:2832`, the Blog V4 included) `root:coaxhub-radiod 0660` and removes the `uaccess` tag. The package needs no rule from the system's librtlsdr packages; when one is installed anyway (bookworm's `60-librtlsdr0.rules` sets `plugdev`), this rule runs after it, and after `70-uaccess.rules` (which tags software radios for the desktop user), and before `73-seat-late.rules` applies that tag. The assignments use `:=` so later rules cannot widen them.
- The kernel's DVB-T driver `dvb_usb_rtl28xxu` claims RTL-SDR dongles. The package's own blacklist, `/usr/lib/modprobe.d/coaxhub-stationd.conf` (`/lib/modprobe.d/…` on jammy), applies from the next boot; the installer unloads the module if it is loaded, and `postinst` says so otherwise.

## 5. Maintainer scripts

| | |
|---|---|
| `preinst install`, `preinst upgrade` | the package refuses a system it was not built for, before anything is unpacked. Each package carries its own system, written in when it is built (`ID` and `VERSION_CODENAME`): the `noble` package accepts `ID=ubuntu` with `VERSION_CODENAME=noble` and nothing else; a Debian package accepts `ID=debian` or `raspbian` with its suite's codename. Anything else (another suite, a release past its support, a non-LTS Ubuntu, a derivative, a system with no `/etc/os-release`) fails with a message that names the system found (`PRETTY_NAME`) and the one the package is for, lists the supported systems and says what to do (use the installer, which picks the package, or upgrade the system); nothing is installed, and a refused upgrade leaves the installed version in place. It is the installer's refusal (§6) for what the installer does not see: a repository added by hand, an edited installer, `dpkg -i` (the Ubuntu 22.04 owner's case: the bookworm package on jammy). `remove`, `purge` and the `abort-*` calls never check. `dpkg --force-all` is the owner's decision about their own computer and is not supported. |
| `postinst configure` | creates the users and directories (idempotent; existing credentials are never touched), enables `systemd-time-wait-sync.service` (and remembers that it did), reloads udev rules and re-triggers RTL-SDR devices, `daemon-reload`. On an **upgrade** it makes sure 0.2.x's polkit rule file is gone, restarts `coaxhub-helper.socket` if it is active (a changed socket unit, such as its mode or group, applies now and not at the next boot), starts it when `stationd.service` runs (0.2.x did not know it), and runs `systemctl try-restart` on `stationd.service` and on the `coaxhub-radiod@*.service` instances, so only running units restart: each running receiver restarts once. |
| `prerm remove` | stops `stationd.service`, every `coaxhub-radiod@*.service` instance, then `coaxhub-helper.socket` and its connections. `prerm upgrade` leaves them running until the new `postinst` restarts them. |
| `postrm remove` | `daemon-reload`. The enrollment, the applied receiver set and the users stay, so reinstalling needs no new enrollment and the receivers start again. |
| `postrm purge` | removes `stationd.service`'s enablement link (nothing else is enabled: not the helper's socket), disables `systemd-time-wait-sync` if the package enabled it, removes `/var/lib/coaxhub` (the station key and `receivers/`), `/var/lib/coaxhub-radiod`, the runtime directories, and both users and groups. |

Every script works without systemd running or installed (containers), and without udev.

## 6. The installer

`installer/station.sh` is a POSIX `sh` template (checked with `dash` and `shellcheck`). Whoever publishes it at `get.<domain>/station` replaces exactly two placeholders with `sed`:

| Placeholder | |
|---|---|
| `@COAXHUB_APT_URL@` | the repository, e.g. `http://apt.coaxhub.test` in a test, `https://apt.<domain>` in production |
| `@COAXHUB_KEY_FINGERPRINT@` | the repository signing key's fingerprint, 40 hex digits |

The script refuses to run while either is unreplaced, or when the URL or fingerprint is malformed. [releasing.md](releasing.md) says who fills them in.

What it does, in order:
1. **Checks**: root; the system, by `ID` and `VERSION_CODENAME` exactly as the package's `preinst` does: `ubuntu` with `jammy`, `noble` or `resolute`, or `debian` or `raspbian` with `bookworm` or `trixie`; `dpkg --print-architecture` is amd64 or arm64 (a 32-bit Raspberry Pi OS gets a hint to install the 64-bit one). Anything else is refused before anything is written, and the refusal **names the system** by `PRETTY_NAME` and **lists what is supported**: "Ubuntu 23.10 is not supported: CoaxHub stations run on Ubuntu 22.04, 24.04 or 26.04 LTS, or Debian or Raspberry Pi OS 12 or 13 (64-bit)". Derivatives (Linux Mint, Pop!_OS and others that name Ubuntu or Debian only in `ID_LIKE`) are refused; the installer never says "Debian jammy". The repository source it writes is `Suites: <codename>`.
2. **SDRs**: it asks nothing about them and has no `--profile` option; the owner chooses each receiver's model and band in the web app, from the list the station reports ([architecture](architecture.md#hardware-and-receivers)). As information only, it lists the SDRs it sees under `/sys/bus/usb/devices` (`0bda:2838` and `0bda:2832`, with the USB `product` string), and says so when there are none; the install continues either way, since a station can be installed before its SDR arrives. Its one question is the enrollment token (step 6), read from `/dev/tty`, because stdin is the script itself under `curl | sudo sh`; `main` redirects its own stdin from `/dev/null`, so no command it runs can swallow the rest of the script. Without a terminal, `--token-file` supplies the token and `--yes` takes the default of any confirmation the installer might ask.
3. **Repository key**: installs `gpg` and `curl` if missing, downloads `<APT_URL>/coaxhub-archive-keyring.asc` into a temporary directory and checks, with `gpg --show-keys --with-colons` in a throwaway `GNUPGHOME`, that it holds **exactly one** primary key whose fingerprint is the pinned one (an extra key appended to the file is refused). Only then does it write the dearmored keyring to `/usr/share/keyrings/coaxhub-archive-keyring.gpg` and the deb822 source `/etc/apt/sources.list.d/coaxhub.sources` with `Signed-By:` that keyring alone. A mismatch stops before anything is written. The initial download is protected by HTTPS in production; everything after it by the pinned key.
4. **Package**: `apt-get update`, `apt-get install coaxhub-stationd`, with `NEEDRESTART_SUSPEND=1` in the environment: Ubuntu's `needrestart` would otherwise stop `curl | sudo sh` with a dialog, and the package restarts its own units itself. The package has no conffile, so there is nothing for an upgrade to ask about. It unloads `dvb_usb_rtl28xxu` if loaded.
5. **Clock**: waits up to 2 minutes for `timedatectl show -p NTPSynchronized --value` to say `yes`. Without `timedatectl` it stops unless `--skip-time-sync` is given (containers without systemd).
6. **Enrollment**: skipped when `stationd status` says the station is enrolled, unless `--reenroll` (which passes `--force`). stationd runs as `coaxhub` (`runuser`), so it writes its own files. With a terminal, stationd's standard input **is** the terminal: stationd turns echo off, then prompts, reads the token and zeroes it after use. The token never passes through the shell, an argument, the environment or a log. stationd exits 4 when the token is refused, malformed or empty; the installer then asks again, at most 3 times. With `--token-file`, the file is stationd's standard input. Any other failure is reported with stationd's message and the installer exits 1. The installer saves the terminal's settings first and restores them on exit, so a Ctrl-C at the hidden prompt never leaves echo off.
7. **Start**: `systemctl enable` and `restart` of `stationd.service` alone, unless `--no-start`, then `stationd status`. The installer enables and starts no radiod: the station connects, reports its SDRs, and the hub starts the receivers the owner set up (on a station with receivers already applied, `stationd` starts them as it starts).

Running it again upgrades the package and keeps the enrollment and the receivers.

`COAXHUB_INSTALL_USB_DEVICES` replaces `/sys/bus/usb/devices` for listing the SDRs; it exists for the tests (`sudo` drops it anyway).

## 7. Tests

```sh
packaging/test/test-packages.sh [--quick]     # the packages; --quick: bookworm only
packaging/test/test-installer.sh [--quick]    # the installer
packaging/test/test-systemd.sh [--quick]      # the package under a real systemd
```

Each runs every suite (`bookworm`, `trixie`, `jammy`, `noble`, `resolute`; the containers are of the suite's own system) unless given `--suite <name>` for one (CI runs one job per suite) or `--quick` (bookworm only). All need the packages in `target/packages` (`packaging/build-all.sh`; `test-systemd.sh` the amd64 ones) and a Docker daemon, and nothing else: the tests copy files into `debian:<suite>` containers with `docker cp`, bind no host ports and mount nothing. Inside, checks print TAP-like `ok N - …` lines.

**`test-packages.sh`**, per suite:
- **Artefacts** (both architectures, not installed, so arm64 is checked on an amd64 host): control fields, the suite's version suffix (`~deb12`, `~ubuntu22.04`, …), `Depends` (libusb and dbus, and no librtlsdr and no polkit), the file list, under the suite's unit directory (`/usr/lib/…`, `/lib/…` on jammy) (the template unit `coaxhub-radiod@.service`, the helper `coaxhub-helper` and its socket and service units in; no polkit rule, no `coaxhub-radiod.service`, no `/etc/coaxhub`), `Depends` with `dbus` and without polkit, no conffile, root ownership, no PEM file shipped, both source tarballs and radiod's patches next to the packages with the SHA-256s the copyright file names; `readelf -d`: `rtlsdr.so` needs `librtlsdr.so.0` with `RUNPATH` `/usr/lib/coaxhub/radiod`, and `librtlsdr.so.0` has that SONAME, needs libusb and knows the Blog V4; and `file` on stationd, `coaxhub-helper`, radiod, `rtlsdr.so` and `librtlsdr.so.0`: ELF 64-bit, `x86-64` or `ARM aarch64`, stripped.
- **Install, amd64**, twice: on the bare image (no systemd) and with systemd installed. In a clean container: `apt-get install ./coaxhub-stationd_*.deb` resolves every dependency and installs no librtlsdr package; `dpkg --verify`; users, groups, modes and paths as in §3; radiod not on `PATH`; `/var/lib/coaxhub/receivers` empty with its owner and set-group-ID mode; the copyright file; `stationd --version`; the packaged build takes no `--config` and has no `check-config` (clap's exit 2); `doctor` reports the station is not enrolled and that there is no RTL-SDR on USB (it names the IDs it looked for, `0bda:2838` and `0bda:2832`); `status` and `run` exit 3; polkit is not installed with the package and no rule is shipped; the helper's units are checked as text (the socket's path, owner `root`, group `coaxhub`, mode 0660, `Accept=yes`, no `[Install]`; the service's empty capability bounding set, `NoNewPrivileges`, `ProtectSystem=strict`, `AF_UNIX` only, a run time limit), and the helper started from a shell says it runs from its socket and exits 1; with a `receivers.json` the test writes as `coaxhub`, as stationd does (three receivers on the three profiles, in slots that are not 0, mode 0640; the file takes the group `coaxhub-radiod` from the directory, and `coaxhub-radiod` reads it), `backend-config --receiver ID` renders each receiver's profile with the serial, the centre and its slot's groups (`239.77.<slot>.6` and `239.77.<slot>.4`) and TTL 0 (`rtlsdr-v4` without direct sampling and with the tuner's gain, or the receiver's own gain), also as `coaxhub-radiod` into `/run/coaxhub-radiod/ID`; it refuses, with exit 2, an ID that is not in `receivers.json`, receiver 0, a `siggen` profile (the packaged build has none) and a `receivers.json` with a key stationd's schema doesn't have; `radiod -V` reports the pinned commit and names our patches; every shared library resolves, and `ldd` resolves `rtlsdr.so`'s librtlsdr to the package's; radiod, run on a rendered config without an SDR, loads its presets and driver from `/usr/lib/coaxhub/radiod` and stops at the missing device. With systemd: `systemd-analyze verify` on every unit (the template units through an instance, `coaxhub-radiod@1.service` and `coaxhub-helper@0-1.service`) and `security` (exposure ≤ 2.5, ≤ 4.5 and ≤ 3.0 for stationd, radiod and the helper), `stationd.service` wanting the helper's socket, `systemd-time-wait-sync` enabled. On Ubuntu the package is installed with only `main` and `restricted` enabled, which proves that its runtime libraries are in `main`. `systemd-analyze verify` must print nothing about our units (a directive jammy's systemd 249 does not know shows up there; verify itself exits 0), and `security`'s score needs `--offline`, which 249 lacks, so on jammy it is checked under `test-systemd.sh`. Then a reinstall keeps credentials, modes and `receivers.json`; `remove` keeps credentials, the receiver set and users and takes the helper away; `purge` leaves no file, user, group or enablement link behind and disables `systemd-time-wait-sync` again.

- **The package refuses a system it was not built for** (`refusal.sh`, a clean container of the suite). A package for another system is refused by `dpkg -i`, naming the package's system and this one by its `PRETTY_NAME`, listing what is supported, saying nothing was installed, and leaving no file, user or package. It needs only this suite's own packages (CI builds one suite per job): `refusal.sh` makes it from the suite's own `.deb` (`dpkg-deb -R`, the three lines the build filled into the `preinst` (`OS_IDS`, `CODENAME`, `TARGET_NAME`) set to another suite's, `dpkg-deb -b`), keeping this suite's `Pre-Depends`, which is met, so that the real `preinst` text the build produced is what refuses (bookworm's system on jammy, the jammy owner's case, among the pairs). When the other suite's real package is in `target/packages` too (a full local build: an older release's, or another family's with a lower floor, bookworm's on jammy, jammy's on noble and bookworm, noble's on resolute, bookworm's on trixie, jammy's on bookworm) it is tried as well. The suite's own package is refused when `/etc/os-release` is changed to a derivative (`ID=linuxmint` with this suite's ID in `ID_LIKE`), to a release past its support or a non-LTS one (`xenial`, `bullseye`, `mantic`), to Fedora, or removed; and installs on Raspberry Pi OS (`ID=raspbian`) for the Debian suites and is refused there for the Ubuntu suites. An upgrade on a system that does not match is refused and leaves the installed version in place, and `remove` and `purge` are never blocked.

**`test-installer.sh`**, per suite (the repository, `apt-ftparchive` and the gpg key are built in a container of that suite): a signing key generated in the test, a repository built with `apt-ftparchive` (`Release`, `InRelease`, `Release.gpg`, amd64 and arm64) and served by `python3 -m http.server` on `127.0.0.1:28080` inside the container. The installer is filled in with `sed`, as its publisher does.
- Refusals, each leaving no keyring, source or package behind: the unfilled template; `--token` on the command line; an unknown option (`--profile` among them); not root; Debian 11, Ubuntu 23.10, Ubuntu 20.04, Linux Mint, Pop!_OS, Fedora, a Debian that claims an Ubuntu codename (never named "Debian jammy") and an Ubuntu that claims a Debian one, each refusal naming the system and listing the supported ones; armhf and i386 (a `dpkg` shim); a wrong fingerprint; a key file with a second key appended.
- The happy path up to enrollment, with the script piped into `sh -s --` as `curl | sudo sh` does (`--token-file`, `--skip-time-sync`, `--no-start`, a fake RTL-SDR): the SDR listed as information, keyring and source written, package installed, and enrollment failing cleanly (no control plane) with stationd's reason and exit 1; the token appears nowhere in the output. No file is written under `/etc/coaxhub` and no `receivers.json`.
- Raspberry Pi OS (`ID=raspbian`) on the Debian suites goes through to enrollment and installs the package; on the Ubuntu suites the installer refuses it. On Ubuntu a shim in the place of `needrestart`'s apt hook records `NEEDRESTART_SUSPEND` after the installer's `apt-get`: it is always set, so no dialog stops `curl | sudo sh`.
- With no SDR at all (an empty sysfs), on a terminal and without one: the installer says it sees none and carries on, with no question.
- On a pseudo-terminal, as a person runs it (`in-container/interactive.py`): no profile question and no SDR question; a refused token asking again ("2 of 3"), three refused tokens stopping; no token is ever echoed.
- An RTL-SDR Blog V4 (a fake sysfs device with product `Blog V4` and manufacturer `RTLSDRBlog`): listed by its product string, with no question on a terminal or without one.
- Re-runs: keyring and source unchanged, one source file; a malformed token file; with fake credentials (openssl), a re-run skips enrollment, waits for NTP through a `timedatectl` shim and exits 0, and stops when `timedatectl` is missing; `--reenroll` enrolls again, and a failed attempt keeps the old credentials.

Each suite's installer test takes about 2.5 minutes and each package test about 1 minute, mostly `apt-get`.

**`test-systemd.sh`**, per suite, amd64: the package in a container (the suite's own image, built once) whose init is a real systemd. The recipe uses no `--privileged` and shares no host device or udev: `--cgroupns=private --cap-add SYS_ADMIN --security-opt apparmor=unconfined -e container=docker`, tmpfs on `/run`, `/run/lock` and `/tmp`, and an entrypoint that remounts `/sys/fs/cgroup` read-write and execs systemd. The AppArmor option is what lets that remount through on Ubuntu hosts and GitHub's runners, whose `docker-default` profile denies mounts; elsewhere it changes nothing. The image (built per suite) has systemd, dbus and the test tools, **no polkit**, and no `/usr/sbin/policy-rc.d` (which would stop the package from starting anything). A stub radiod (`sleep infinity`, through a drop-in of the receiver template) stands in for radiod: the point is that the unit starts, as `coaxhub-radiod`, under the template's sandbox.
- **Fresh install.** `apt-get install` pulls no polkit; `stationd.service` is not enabled, and nothing enables the helper's socket; the units load, verify and score as above; modprobe blacklists the DVB driver. Then, **inside `stationd.service`'s own sandbox** (a drop-in swaps its command for a script, so the user is `coaxhub`, with no capability, `NoNewPrivs` and its seccomp filter; its `Wants=coaxhub-helper.socket` starts the socket): the same sandbox cannot start a unit by itself (`Access denied`: no polkit, no rule) but reads a unit's state; `ping` and `start 11` answer `ok` and receiver 11 runs as `coaxhub-radiod`; a second `start` leaves it alone; `restart` gives it a new invocation; `stop` stops it and leaves the others.
- **`stationd` by itself.** With throwaway credentials (openssl) and two fake RTL-SDRs in a tmpfs over `/sys/bus/usb/devices` (which `CAP_SYS_ADMIN` allows), the real `stationd.service` is started with the unit exactly as shipped: it pulls in the helper's socket, starts both applied receivers at boot through the helper, as the `coaxhub` user, with no hub to reach (the journal shows `start 31: ok` from `coaxhub`'s UID), and `doctor` sees both.
- **The number of units.** Sixteen stub receivers run; a seventeenth `start` and a `restart` of a new id are refused with the reason and start nothing; `restart` of one of the sixteen, `stop`, and a `start` once there is room all work; the journal records the refusal; a unit whose radiod fails is failed, `stop` does not clear it, and it does not count against the sixteen.
- **Hostile requests.** From the same sandbox: `enable 11`, `start 0`, `start 011`, `start 11 12`, `start coaxhub-radiod@11`, `start 4294967296`, `start -1`, a NUL byte, non-UTF-8, CRLF, an upper-case verb, `daemon-reload`, `ping 1`, a 1 MiB line, and a line with no newline that is left hanging: each is `error bad request`, and the list of `coaxhub-radiod@*` units is the same before and after. The journal names each instance with the peer's UID, records `start 11: ok` and refusals by reason, and never holds a request's bytes.
- **Who may connect.** A user outside the `coaxhub` group gets `Permission denied` at `connect`; a user put in the group by the test but with another UID connects and gets `error not permitted` (the peer check) and starts nothing; root and `coaxhub` may `ping`.
- **doctor.** `stationd doctor` passes `helper`; as `nobody` it warns; with the socket stopped it fails, naming the socket unit's state and `sudo systemctl restart stationd`.
- **A hostile `receivers.json`.** With the template as shipped, `Restart=on-failure` and `RestartSec=5` included (a drop-in replaces only `+ip link` and radiod): an id the file lacks, a key outside the schema, the development `siggen` profile and a file that is not JSON each make the unit's `ExecCondition` (the unprivileged renderer, run as `coaxhub-radiod`) refuse it; the unit is skipped and stays inactive (the journal says `Skipped due to 'exec-condition'`), its `ExecStartPre` never runs (no config is written), and 12 s later (two restart delays) it was skipped once and only once, `NRestarts` is 0 and no refused unit is counted as busy. A valid receiver's condition and renderer succeed. Root, in the helper, reads none of it.
- **Remove and purge.** `remove` stops the socket and the receivers and removes the helper and its units; `purge` leaves no helper unit, socket or enablement link.
- **The jammy owner** (jammy only). The published 0.2.3 package for bookworm is installed on Ubuntu 22.04 the way the altered installer did (its `libfftw3-single3 (>= 3.3.10)` dependency forced: jammy has 3.3.8, the same soname; polkit 0.105 comes with it). The reported bug is reproduced: `coaxhub` starting a receiver's unit gets "Interactive authentication required" (polkit 0.105 reads `.pkla` files and ignores the `.rules` file), and the old stationd, enrolled with throwaway credentials and two fake SDRs, starts no receiver. Then the installer, filled in for a local repository that has the jammy package, is run: it succeeds, writes `Suites: jammy`, upgrades to `…~ubuntu22.04`, the rule file is gone, the helper's socket is active, and both receivers are started by stationd through the helper. The DVB blacklist is found by jammy's kmod.
- **Upgrade from 0.2.3** (bookworm). The published `coaxhub-stationd_0.2.3-1~deb12_amd64.deb` is downloaded once from the CoaxHub APT repository into `target/packages-previous/` and checked against its SHA-256. It is installed (it brings polkit); `coaxhub` starts a receiver through its polkit rule; two receivers run. Then the new package is installed over it (`--reinstall`: a package built from the same tree has the same version number until a release bumps it, and dpkg still runs the new `postinst` as for an upgrade): the rule file is gone, polkit stays until `autoremove`, `coaxhub` can no longer start a unit by itself, `coaxhub-helper.socket` is active and `root:coaxhub 0660`, `stationd.service` and each running receiver were restarted exactly once (one new invocation, not two), and the helper starts, restarts and stops other receivers for `coaxhub`. A second install over it (now an upgrade from a version that has the helper) restarts the socket, which is active again with the same owner and mode.

**Not covered by the container tests**: the `+` `ExecStartPre` of the receiver template (the container has no `CAP_NET_ADMIN`), several radiod instances on one host with a real SDR, an SDR unplugged and plugged back in (the sysfs is a fake), and radiod with real RTL-SDRs; a run on a real Raspberry Pi covers these.
