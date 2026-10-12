<!-- SPDX-License-Identifier: GPL-3.0-only -->

# The station package and installer

A station is one Debian package, `coaxhub-stationd`, installed by a short script ([installer/station.sh](../installer/station.sh)). The package holds stationd, a private copy of radiod with its own librtlsdr, and their systemd units, so there is nothing else to install. It holds no settings: the receivers are set up in the web app and the hub sends them to stationd ([architecture](architecture.md#hardware-and-receivers)). This document covers how the package is built, what it contains, why each systemd setting is there, and how the package and the installer are tested.

## 1. Targets and versions

| | bookworm (Debian 12) | trixie (Debian 13) |
|---|---|---|
| **amd64** | `coaxhub-stationd_<v>-<r>~deb12_amd64.deb` | `coaxhub-stationd_<v>-<r>~deb13_amd64.deb` |
| **arm64** | `coaxhub-stationd_<v>-<r>~deb12_arm64.deb` | `coaxhub-stationd_<v>-<r>~deb13_arm64.deb` |

- `<v>` is stationd's crate version and `<r>` is `PACKAGE_REVISION` (default 1). The `~debN` suffix orders an upgrade across suites: `0.1.0-1~deb12` < `0.1.0-1~deb13` < `0.1.0-1`.
- Raspberry Pi OS is Debian: its suite is the Debian codename, and the 64-bit image is arm64. The Pi 3B (Cortex-A53) is the oldest target.
- **Only maintained suites.** A suite is a target while Debian still publishes security updates for it, from the security team or from Debian LTS; paid extended support doesn't count. A new Debian stable is added in the first stationd release after it comes out. When a suite's LTS ends, the next release drops it: `build-all.sh` stops building it, the installer refuses it (§6), and packages already in the repository stay but get no new versions. Nothing is built for a suite past its end, even for an existing station whose owner would rather not reinstall.
- Today: bookworm (Debian LTS until June 2028) and trixie. bullseye's LTS ended on 2026-08-31, so it is refused.
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

`build-deb.sh` needs only `git` and Docker with BuildKit (`docker build` with buildx). Everything else happens in `debian:<suite>` containers on the Docker daemon, driven by [packaging/deb/Dockerfile](../packaging/deb/Dockerfile) with the repository root as the build context. Nothing is bind-mounted and the result comes back with `--output type=local`, so it works the same on a CI runner, on a workstation and with a remote Docker daemon.

1. **The radiod and librtlsdr sources.** Each pinned commit is fetched once (`git fetch --depth 1`) into `target/packaging/ka9q-radio.git` and `target/packaging/rtl-sdr.git`, and `git archive | gzip -n` makes `ka9q-radio-384b269a.tar.gz` and `rtl-sdr-619ac318.tar.gz`, the same bytes on every run. radiod and librtlsdr are built **from those tarballs**, each a named build context (`ka9q`, `rtlsdr`), so the published source is exactly what was compiled: for radiod, the tarball with our patches, which `build-deb.sh` copies next to it as `ka9q-radio-384b269a-<name>.patch`.
2. **Stage `toolchain`** (cached as an image layer per suite): the native and `aarch64-linux-gnu` C toolchains; the `-dev` libraries radiod and librtlsdr link, for amd64 and, through multiarch, arm64 (`libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libusb-1.0-0-dev`; the suite's `librtlsdr-dev` is not used); `cmake`; `protobuf-compiler` for station-proto; `dpkg-dev`; Rust from rustup with both targets. The build needs no arm64 emulation: arm64 is cross-compiled.
3. **Stage `librtlsdr`** ([the bundled librtlsdr](#the-bundled-librtlsdr)): checks the tarball's SHA-256 against the one `build-deb.sh` computed (the one the copyright file names), then builds only the shared library with CMake (`-DCMAKE_BUILD_TYPE=Release`, no udev rule, no zero-copy, and `DETACH_KERNEL_DRIVER` on as in Debian's build; for arm64 the cross compiler and the arm64 `pkg-config` directory). The library, `librtlsdr.so.0` (its SONAME), is stripped, and its two public headers go to radiod's stage.
4. **Stage `radiod`**: applies our patches, `packaging/patches/ka9q-radio/*.patch` in order, to the unpacked tarball (`patch -p1`; one that doesn't apply fails the build), then `make radiod`, then `make rtlsdr.so` against the private librtlsdr (its headers, and `-Wl,--enable-new-dtags,-rpath,/usr/lib/coaxhub/radiod`, so `rtlsdr.so` has `RUNPATH` `/usr/lib/coaxhub/radiod` and `NEEDED` `librtlsdr.so.0`; the stage checks both), with every hardware driver off except `ENABLE_RTLSDR=1`.
   - `ARCHOPTS` is always passed. Upstream's `-march=native` would tie the binary to the build machine. The defaults are `-march=x86-64-v2 -mtune=generic` for amd64 (any x86-64 CPU since about 2009) and `-march=armv8-a -mtune=cortex-a53` for arm64 (every 64-bit Pi).
   - `config_paths.h` is written by the Dockerfile instead of upstream's Makefile rule, which asks git (the tarball has no `.git`). It holds the same commit, date and summary, the branch ` (coaxhub-stationd, patches 0001,…)` that names the patches applied (`radiod -V` and radiod's log show it), and the package's private paths: `PKGLIBDIR`, `PKGDATADIR` and `CONFDIR` are `/usr/lib/coaxhub/radiod` (so a `/etc/radio` from another radiod install never overrides our presets), and `STATEDIR` is `/var/lib/coaxhub-radiod` (FFTW wisdom).
   - Upstream's Makefile calls a bare `ar`; the build puts the target's `ar` first on `PATH`.
   - radiod and `rtlsdr.so` are stripped.
5. **Stage `stationd`**: `cargo build --release --locked -p stationd --features packaged --target <triple>` with `COAXHUB_DOMAIN` and the root certificate (a named build context holding only `root.pem`). The cargo registry and the target directory are BuildKit cache mounts, so a rebuild compiles only what changed. `cargo tree` lists the linked crates for the copyright file.
6. **Stage `package`**: [packaging/deb/assemble.sh](../packaging/deb/assemble.sh) lays out the file tree, runs `dpkg-shlibdeps` for the target architecture on the four ELF files (stationd, radiod, `rtlsdr.so`, `librtlsdr.so.0`), checks that `Depends` names libusb and no librtlsdr, writes `control` and `md5sums` (the package has no conffile), sets every mtime to the last commit's time, and runs `dpkg-deb --root-owner-group -Zxz`.

Build times on a 16-thread x86-64 development machine:

| | first build | stationd changed |
|---|---|---|
| bookworm amd64 | about 75 s (toolchain image about 40 s, stationd 37 s) | 12 s |
| bookworm arm64 (toolchain cached) | 33 s | 10 s |
| trixie amd64 | 87 s | 11 s |
| trixie arm64 (toolchain cached) | 55 s | 10 s |
| `build-all.sh` | about 4 min | about 45 s |

Package sizes: 2.5 MB (amd64), 2.2 MB (arm64); installed about 7.9 MB.

If `docker build` fails with "error getting credentials", the credential helper named in `~/.docker/config.json` is not reachable. Point Docker at an empty config for the build: `mkdir -p /tmp/docker-config && echo '{}' >/tmp/docker-config/config.json && export DOCKER_CONFIG=/tmp/docker-config`.

## 3. Contents

| Path | Contents |
|---|---|
| `/usr/bin/stationd` | stationd built with `--features packaged`: the domain and root are compiled in, the SDR table, the profiles and the capacity claims are fixed, and it reads no configuration file (no `--config`, no `check-config`) |
| `/usr/lib/coaxhub/radiod/` | `radiod`, `rtlsdr.so`, `librtlsdr.so.0` (below), `presets.conf`. Private, not on `PATH`. |
| `/usr/share/coaxhub/radiod-templates/` | `rtlsdr.conf`, `rtlsdr-hf.conf`, `rtlsdr-v4.conf` (the development-only `siggen` is left out) |
| `/usr/lib/systemd/system/` | `stationd.service`, `coaxhub-radiod@.service` (the template unit: one instance per receiver) |
| `/usr/share/polkit-1/rules.d/coaxhub-stationd.rules` | lets the `coaxhub` user start, restart and stop `coaxhub-radiod@<number>.service` over D-Bus, and nothing else (§4) |
| `/usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules` | RTL-SDR device nodes for the `coaxhub-radiod` group only |
| `/usr/lib/modprobe.d/coaxhub-stationd.conf` | blacklists `dvb_usb_rtl28xxu` |
| `/usr/share/doc/coaxhub-stationd/copyright` | DEP-5: GPL-3.0-only; the ka9q-radio commit, its tarball and SHA-256, our patches to it each with its SHA-256, the `ARCHOPTS`; librtlsdr's version, commit, tarball and SHA-256, its copyright holders and GPL-2.0-or-later; the Rust crates linked into stationd with their licences |

- Units, udev rules and modprobe files are under `/usr/lib` on both suites. Both are merged-`/usr` systems, where `/lib` is `/usr/lib`, and trixie's dpkg expects packages to ship files under `/usr` only.
- `Depends` comes from `dpkg-shlibdeps` (libc6, libfftw3-single3, libopus0, libiniparser1 or libiniparser4, libbsd0, libusb-1.0-0, libgcc-s1), plus `adduser`, `iproute2` and `polkitd | policykit-1`: without polkit the system bus refuses the `coaxhub` user the receivers' units (§4), so a station could not start a single receiver, and `polkitd` brings the system bus (`dbus`) with it. No librtlsdr package is a dependency, and one installed on the system is never used. `Recommends: systemd-timesyncd | time-daemon`.

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

A template unit: `coaxhub-radiod@<receiver_id>.service` is the radiod of one receiver, and `%i` is the receiver's ID. `stationd` starts, restarts and stops the instances (below); nothing enables them.

| Setting | Why |
|---|---|
| `User=coaxhub-radiod` | radiod gets the SDRs and nothing else; it never sees the station key |
| `RuntimeDirectory=coaxhub-radiod/%i`, `StateDirectory=coaxhub-radiod` | `/run/coaxhub-radiod/%i/radiod.conf` for this instance and FFTW wisdom, writable under `ProtectSystem=strict` |
| `ExecStartPre=+ip link set dev lo multicast on` | radiod sends with `IP_MULTICAST_IF=lo`; a stock `lo` lacks the MULTICAST flag and radiod would exit ([radiod notes §2](radiod-notes.md)). `+` runs only this command as root. |
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
| `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK` | the tunnel, radiod's multicast, DNS; `AF_UNIX` is the system bus socket (D-Bus), through which stationd starts the receivers' units ([below](#how-stationd-starts-the-receivers)); connecting to it needs no capability |
| `PrivateDevices`, `PrivateTmp`, `ProtectHome`, `NoNewPrivileges`, `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`, `LockPersonality`, `MemoryDenyWriteExecute`, `SystemCallArchitectures=native`, `SystemCallFilter=@system-service ~@privileged @resources` | stationd is plain Rust and Tokio: no devices, no JIT, no privileged system calls |

`systemd-analyze security` rates it 1.5–1.7 ("OK").

Neither unit is enabled or started by the package: a fresh station is not enrolled, and stationd would exit 3. The installer (or the owner) enrolls it, then runs `systemctl enable --now stationd.service`. **That is the only unit anyone enables.** The `coaxhub-radiod@` instances have no `[Install]` section and are never enabled; `stationd` starts them.

### How stationd starts the receivers

- At boot, `stationd` reads `receivers.json` and starts `coaxhub-radiod@<receiver_id>.service` for each receiver in it whose SDR is plugged in, before it connects to the hub. A receiver therefore runs while the tunnel is down. Afterwards it starts, restarts and stops instances as `ConfigureReceivers` changes the set, stops one when its SDR is unplugged, and starts it when the SDR is plugged in.
- It does so over the system bus, with systemd's `StartUnit`, `RestartUnit` and `StopUnit`. `stationd` runs as `coaxhub`, with no capability, so the package ships `/usr/share/polkit-1/rules.d/coaxhub-stationd.rules`: for the user `coaxhub`, the action `org.freedesktop.systemd1.manage-units` is allowed only for a unit named `coaxhub-radiod@<digits>.service` and only for those three verbs. Every other unit, and every other verb, falls through to polkit's defaults, which refuse a service user. The rule does not allow enabling or disabling units, changing their files or reloading systemd.
- `stationd` never edits a unit or a running radiod's config. A change to a receiver restarts its instance, and `ExecStartPre` renders the config again from `receivers.json`.

### udev and the DVB driver

- `72-coaxhub-rtlsdr.rules` gives RTL-SDR nodes (`0bda:2838`, `0bda:2832`, the Blog V4 included) `root:coaxhub-radiod 0660` and removes the `uaccess` tag. The package needs no rule from the system's librtlsdr packages; when one is installed anyway (bookworm's `60-librtlsdr0.rules` sets `plugdev`), this rule runs after it, and after `70-uaccess.rules` (which tags software radios for the desktop user), and before `73-seat-late.rules` applies that tag. The assignments use `:=` so later rules cannot widen them.
- The kernel's DVB-T driver `dvb_usb_rtl28xxu` claims RTL-SDR dongles. The package's own blacklist, `/usr/lib/modprobe.d/coaxhub-stationd.conf`, applies from the next boot; the installer unloads the module if it is loaded, and `postinst` says so otherwise.

## 5. Maintainer scripts

| | |
|---|---|
| `postinst configure` | creates the users and directories (idempotent; existing credentials are never touched), enables `systemd-time-wait-sync.service` (and remembers that it did), reloads udev rules and re-triggers RTL-SDR devices, `daemon-reload`. On an **upgrade** it runs `systemctl try-restart` on `stationd.service` and on the `coaxhub-radiod@*.service` instances, so only running units restart. |
| `prerm remove` | stops `stationd.service` and every `coaxhub-radiod@*.service` instance. `prerm upgrade` leaves them running until the new `postinst` restarts them. |
| `postrm remove` | `daemon-reload`. The enrollment, the applied receiver set and the users stay, so reinstalling needs no new enrollment and the receivers start again. |
| `postrm purge` | removes `stationd.service`'s enablement link, disables `systemd-time-wait-sync` if the package enabled it, removes `/var/lib/coaxhub` (the station key and `receivers/`), `/var/lib/coaxhub-radiod`, the runtime directories, and both users and groups. |

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
2. **SDRs**: it asks nothing about them and has no `--profile` option; the owner chooses each receiver's model and band in the web app, from the list the station reports ([architecture](architecture.md#hardware-and-receivers)). As information only, it lists the SDRs it sees under `/sys/bus/usb/devices` (`0bda:2838` and `0bda:2832`, with the USB `product` string), and says so when there are none; the install continues either way, since a station can be installed before its SDR arrives. Its one question is the enrollment token (step 6), read from `/dev/tty`, because stdin is the script itself under `curl | sudo sh`; `main` redirects its own stdin from `/dev/null`, so no command it runs can swallow the rest of the script. Without a terminal, `--token-file` supplies the token and `--yes` takes the default of any confirmation the installer might ask.
3. **Repository key**: installs `gpg` and `curl` if missing, downloads `<APT_URL>/coaxhub-archive-keyring.asc` into a temporary directory and checks, with `gpg --show-keys --with-colons` in a throwaway `GNUPGHOME`, that it holds **exactly one** primary key whose fingerprint is the pinned one (an extra key appended to the file is refused). Only then does it write the dearmored keyring to `/usr/share/keyrings/coaxhub-archive-keyring.gpg` and the deb822 source `/etc/apt/sources.list.d/coaxhub.sources` with `Signed-By:` that keyring alone. A mismatch stops before anything is written. The initial download is protected by HTTPS in production; everything after it by the pinned key.
4. **Package**: `apt-get update`, `apt-get install coaxhub-stationd`. The package has no conffile, so there is nothing for an upgrade to ask about. It unloads `dvb_usb_rtl28xxu` if loaded.
5. **Clock**: waits up to 2 minutes for `timedatectl show -p NTPSynchronized --value` to say `yes`. Without `timedatectl` it stops unless `--skip-time-sync` is given (containers without systemd).
6. **Enrollment**: skipped when `stationd status` says the station is enrolled, unless `--reenroll` (which passes `--force`). stationd runs as `coaxhub` (`runuser`), so it writes its own files. With a terminal, stationd's standard input **is** the terminal: stationd turns echo off, then prompts, reads the token and zeroes it after use. The token never passes through the shell, an argument, the environment or a log. stationd exits 4 when the token is refused, malformed or empty; the installer then asks again, at most 3 times. With `--token-file`, the file is stationd's standard input. Any other failure is reported with stationd's message and the installer exits 1. The installer saves the terminal's settings first and restores them on exit, so a Ctrl-C at the hidden prompt never leaves echo off.
7. **Start**: `systemctl enable` and `restart` of `stationd.service` alone, unless `--no-start`, then `stationd status`. The installer enables and starts no radiod: the station connects, reports its SDRs, and the hub starts the receivers the owner set up (on a station with receivers already applied, `stationd` starts them as it starts).

Running it again upgrades the package and keeps the enrollment and the receivers.

`COAXHUB_INSTALL_USB_DEVICES` replaces `/sys/bus/usb/devices` for listing the SDRs; it exists for the tests (`sudo` drops it anyway).

## 7. Tests

```sh
packaging/test/test-packages.sh [--quick]     # the packages; --quick: bookworm only
packaging/test/test-installer.sh [--quick]    # the installer
```

Both need the four packages in `target/packages` and a Docker daemon, and nothing else: the tests copy files into `debian:<suite>` containers with `docker cp`, bind no host ports and mount nothing. Inside, checks print TAP-like `ok N - …` lines.

**`test-packages.sh`**, per suite:
- **Artefacts** (both architectures, not installed, so arm64 is checked on an amd64 host): control fields, the `~debN` version, `Depends` (libusb, and no librtlsdr), the file list (the template unit `coaxhub-radiod@.service` and the polkit rule in; no `coaxhub-radiod.service`, no `/etc/coaxhub`), no conffile, root ownership, no PEM file shipped, both source tarballs and radiod's patches next to the packages with the SHA-256s the copyright file names; `readelf -d`: `rtlsdr.so` needs `librtlsdr.so.0` with `RUNPATH` `/usr/lib/coaxhub/radiod`, and `librtlsdr.so.0` has that SONAME, needs libusb and knows the Blog V4; and `file` on stationd, radiod, `rtlsdr.so` and `librtlsdr.so.0`: ELF 64-bit, `x86-64` or `ARM aarch64`, stripped.
- **Install, amd64**, twice: on the bare image (no systemd) and with systemd installed. In a clean container: `apt-get install ./coaxhub-stationd_*.deb` resolves every dependency and installs no librtlsdr package; `dpkg --verify`; users, groups, modes and paths as in §3; radiod not on `PATH`; `/var/lib/coaxhub/receivers` empty with its owner and set-group-ID mode; the copyright file; `stationd --version`; the packaged build takes no `--config` and has no `check-config` (clap's exit 2); `doctor` reports the station is not enrolled and that there is no RTL-SDR on USB (it names the IDs it looked for, `0bda:2838` and `0bda:2832`); `status` and `run` exit 3; the polkit rule is present and limited, checked as text: it names the user `coaxhub`, the `org.freedesktop.systemd1.manage-units` action, the unit pattern `coaxhub-radiod@[0-9]+\.service` and the three verbs `start`, `restart` and `stop`, and allows nothing else; with a `receivers.json` the test writes as `coaxhub`, as stationd does (three receivers on the three profiles, in slots that are not 0, mode 0640; the file takes the group `coaxhub-radiod` from the directory, and `coaxhub-radiod` reads it), `backend-config --receiver ID` renders each receiver's profile with the serial, the centre and its slot's groups (`239.77.<slot>.6` and `239.77.<slot>.4`) and TTL 0 (`rtlsdr-v4` without direct sampling and with the tuner's gain, or the receiver's own gain), also as `coaxhub-radiod` into `/run/coaxhub-radiod/ID`; it refuses, with exit 2, an ID that is not in `receivers.json`, receiver 0, a `siggen` profile (the packaged build has none) and a `receivers.json` with a key stationd's schema doesn't have; `radiod -V` reports the pinned commit and names our patches; every shared library resolves, and `ldd` resolves `rtlsdr.so`'s librtlsdr to the package's; radiod, run on a rendered config without an SDR, loads its presets and driver from `/usr/lib/coaxhub/radiod` and stops at the missing device. With systemd: `systemd-analyze verify` on both units (the template unit through an instance, `coaxhub-radiod@1.service`) and `security` (exposure ≤ 2.5 and ≤ 4.5), `systemd-time-wait-sync` enabled. Then a reinstall keeps credentials, modes and `receivers.json`; `remove` keeps credentials, the receiver set and users; `purge` leaves no file, user, group or enablement link behind and disables `systemd-time-wait-sync` again.

**`test-installer.sh`**, per suite: a signing key generated in the test, a repository built with `apt-ftparchive` (`Release`, `InRelease`, `Release.gpg`, amd64 and arm64) and served by `python3 -m http.server` on `127.0.0.1:28080` inside the container. The installer is filled in with `sed`, as its publisher does.
- Refusals, each leaving no keyring, source or package behind: the unfilled template; `--token` on the command line; an unknown option (`--profile` among them); not root; Debian bullseye, Ubuntu, Fedora; armhf and i386 (a `dpkg` shim); a wrong fingerprint; a key file with a second key appended.
- The happy path up to enrollment, with the script piped into `sh -s --` as `curl | sudo sh` does (`--token-file`, `--skip-time-sync`, `--no-start`, a fake RTL-SDR): the SDR listed as information, keyring and source written, package installed, and enrollment failing cleanly (no control plane) with stationd's reason and exit 1; the token appears nowhere in the output. No file is written under `/etc/coaxhub` and no `receivers.json`.
- With no SDR at all (an empty sysfs), on a terminal and without one: the installer says it sees none and carries on, with no question.
- On a pseudo-terminal, as a person runs it (`in-container/interactive.py`): no profile question and no SDR question; a refused token asking again ("2 of 3"), three refused tokens stopping; no token is ever echoed.
- An RTL-SDR Blog V4 (a fake sysfs device with product `Blog V4` and manufacturer `RTLSDRBlog`): listed by its product string, with no question on a terminal or without one.
- Re-runs: keyring and source unchanged, one source file; a malformed token file; with fake credentials (openssl), a re-run skips enrollment, waits for NTP through a `timedatectl` shim and exits 0, and stops when `timedatectl` is missing; `--reenroll` enrolls again, and a failed attempt keeps the old credentials.

Each suite's installer test takes about 2.5 minutes and each package test about 1 minute, mostly `apt-get`.

**Not covered by the container tests**: the units running under a real systemd (the sandbox, `IPAddressDeny`, `DevicePolicy`, the `+` `ExecStartPre`), `stationd` starting the instances over D-Bus under the polkit rule (the rule is checked as text only), several radiod instances on one host, and radiod with real RTL-SDRs. A container that boots systemd needs `--privileged`, which would share the host's kernel modules and udev with the test; a run on a real Raspberry Pi covers these instead.
