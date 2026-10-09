<!-- SPDX-License-Identifier: GPL-3.0-only -->

# stationd

stationd is the station daemon of [CoaxHub](docs/operators.md). It connects an SDR station to the CoaxHub service over one outbound mTLS tunnel, so that listeners can hear the station from the web. It opens no inbound port, so it works behind NAT. It drives a bundled [ka9q-radio](https://github.com/ka9q/ka9q-radio) `radiod`, and it answers the hub's requests (open a channel, subscribe to a waterfall, measure the noise floor) over the station protocol.

This repository holds everything on the station side:
- the **station protocol** (`proto/`, `crates/station-proto/`), MIT: the wire format that the hub and any station implementation build against;
- **stationd** and its backends (a fake one, and ka9q-radio's `radiod`), GPL-3.0-only;
- **station-conformance**, a scripted fake hub for testing a station implementation, MIT;
- a **spike** (`ka9q-spike`): a command-line tool that uses the radiod backend to record Opus audio, dump waterfall rows and measure the noise floor from a local radiod, with scripts that run radiod with a built-in signal generator, so all of this can be tested without SDR hardware;
- the Debian package `coaxhub-stationd` and the installer that sets up a station.

To run a station, read the [operator guide](docs/operators.md).

## Layout

| Path | What |
|---|---|
| `proto/coaxhub/station/v1/` | the station protocol (MIT), source of truth |
| `crates/station-proto/` | MIT: generated prost/tonic types, `limits`, `validate`, `backoff`, `identity` |
| `crates/station-proto/fuzz/` | cargo-fuzz targets for `station-proto` (nightly only) |
| `crates/station-conformance/` | MIT: scripted fake hub used to test a station implementation |
| `crates/stationd/` | GPL-3.0-only: the station daemon (lib + bin `stationd`; feature `packaged` for release builds) |
| `crates/station-backend/` | GPL-3.0-only: the `Backend` trait and `FakeBackend` |
| `crates/backend-ka9q/` | GPL-3.0-only: the radiod backend (`[backend] kind = "ka9q"`) and its client |
| `  src/backend/` | `Ka9qBackend`, the `station_backend::Backend` for radiod: protocol ↔ radiod mapping, media streams, group addresses, waterfall placement |
| `  build.rs`, `vendor/ka9q-radio/` | tag enums generated from the pinned upstream `status.h` and `rtp.h`, checked by SHA-256 |
| `  src/tlv.rs`, `command.rs`, `status.rs` | TLV codec, typed command builder, typed status decoder |
| `  src/rtp.rs`, `spectrum.rs` | RTP parsing and sequence tracking; spectrum2 rows to uint8 rows, zstd |
| `  src/net.rs` | multicast and unicast UDP sockets (socket2 to Tokio) |
| `  src/client.rs` | async `Ka9qClient`: audio and spectrum channels, keepalive, per-SSRC command serialization |
| `  tests/client.rs`, `tests/backend.rs`, `tests/support/` | hermetic client and backend tests against a fake radiod on 127.0.0.1 |
| `  fuzz/`, `examples/fuzz_seeds.rs` | cargo-fuzz target for the TLV decoder (`tlv_status`, nightly only) and its seed generator |
| `crates/ka9q-spike/` | GPL-3.0-only: the spike CLI (`audio`, `waterfall`, `status`, `inspect`, `measure`) |
| `  src/ogg.rs`, `wfstream.rs`, `cpu.rs` | Ogg Opus writer, waterfall row file and PGM, `/proc` CPU meter |
| `  src/measure.rs`, `measure_run.rs` | the noise-floor measurement: the reduction as pure functions (window planning, dB/Hz, histogram, quantiles, UTC schedule), and the `measure hop`, `idle`, `rbw` and `hold` modes |
| `  tests/e2e.rs` | end-to-end tests against a real radiod (need `COAXHUB_E2E=1`) |
| `packaging/radiod-templates/` | radiod config templates, one per SDR profile, rendered by `stationd backend-config` |
| `packaging/build-deb.sh`, `build-all.sh` | build the `coaxhub-stationd` `.deb` (arm64 and amd64 × bookworm and trixie) in Debian containers |
| `packaging/deb/`, `systemd/`, `udev/`, `modprobe/` | the package build (Dockerfile, `assemble.sh`), maintainer scripts, default `stationd.toml`, the two units, the RTL-SDR udev rule, the DVB driver blacklist |
| `packaging/roots/` | the CoaxHub root certificate (public) that release packages trust |
| `packaging/test/` | container tests for the packages and the installer |
| `installer/station.sh` | the installer template served at `get.<domain>/station` |
| `docs/` | the protocol, the operator guide, packaging, releasing, and the radiod and spike notes (see below) |
| `scripts/build-radiod.sh` | builds radiod at the pinned commit and sets the loopback MULTICAST flag |
| `scripts/run-radiod-siggen.sh` | starts, stops or checks radiod with the signal-generator config |
| `scripts/e2e.sh` | runs the spike's end-to-end tests and the CPU measurements; always stops radiod |
| `scripts/e2e-station.sh` | stationd with the ka9q backend against a real radiod and the conformance fake hub; always stops radiod |
| `scripts/fuzz.sh` | runs every fuzz target (`station-proto`'s and `backend-ka9q`'s) for a fixed duration each (CI: 60 s) |
| `scripts/check-license-boundary.sh` | fails if an MIT crate depends on a GPL crate |
| `scripts/update-ka9q-headers.sh` | regenerates `crates/backend-ka9q/vendor/ka9q-radio/` from the pinned upstream commit |
| `scripts/ka9q-probe.py` | stdlib-only probe used to verify radiod's behaviour |
| `scripts/measurement-cpu.sh` | radiod's and the spike's CPU under the measurement scenarios (no channel, front end, continuous measuring, a listener, both) |
| `scripts/measurement-soak.sh` | the idle-duty soak: the measurement channel opened around every measuring minute for hours, radiod supervised |
| `testdata/noise-40m-sample.csv` | an hour of per-minute noise-floor quantiles on 40 m from an RTL-SDR, a test fixture |
| `testdata/radiod-siggen.conf` | radiod config: 24 MS/s signal generator, 10 MHz AM carrier with a 1 kHz tone |
| `testdata/radiod-rtlsdr*.conf` | radiod configs for a real RTL-SDR (98 MHz FM band; 7.1 MHz HF by direct sampling) |
| `deny.toml` | cargo-deny policy (GPL-3.0-only expected; advisories, bans, sources) |
| `LICENSE-MIT`, `LICENSE-GPL-3.0` | the two licenses used in this repository |
| `CONTRIBUTING.md`, `SECURITY.md` | how to contribute; how to report a vulnerability |

## Building and testing

Run every command from the repository root. You need:
- the Rust release named in `rust-toolchain.toml` (rustup installs it on the first `cargo` call);
- `protoc` (`protobuf-compiler`), for `station-proto`;
- `jq`, for `scripts/check-license-boundary.sh`;
- Docker with BuildKit, to build the packages;
- the build dependencies of ka9q-radio, for the end-to-end tests. On Debian:

```sh
sudo apt-get install --no-install-recommends \
    protobuf-compiler libprotobuf-dev pkg-config libssl-dev clang \
    libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libogg-dev \
    libsamplerate0-dev libusb-1.0-0-dev libncurses-dev librtlsdr-dev rtl-sdr \
    sox tcpdump opus-tools sysstat iproute2
```

```sh
cargo build --release
```

### Hermetic tests

These need no radiod and no multicast:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace      # e2e tests are skipped without COAXHUB_E2E=1
cargo deny check
scripts/check-license-boundary.sh
```

The packaged flavour of stationd needs a domain and a root certificate at build time. Any throwaway root will do for its tests:

```sh
D=$(mktemp -d) && openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout /dev/null -subj /CN=test-root -days 1 -out "$D/root.pem" && \
COAXHUB_DOMAIN=coaxhub.test COAXHUB_ROOT_CA="$D/root.pem" cargo nextest run -p stationd --features packaged
```

`crates/stationd/tests/enroll.rs` runs enrollment against an in-test `Enrollment` server with certificates generated in the test: success, refused tokens, an untrusted or misnamed server certificate, unusable responses, and no token in any log line. The radiod templates are checked against golden renderings in `crates/stationd/tests/golden/` (`STATIOND_UPDATE_GOLDEN=1` regenerates them) and against the settings of `testdata/radiod-*.conf`.

The client tests run on Tokio's paused clock against the fake radiod in `crates/backend-ka9q/tests/support/`. They take well under a second.

The fake radiod models the radiod contract the client depends on:
- a channel is created on any command;
- each channel queues at most two commands;
- channels expire when their `LIFETIME` runs out;
- status replies echo `COMMAND_TAG`;
- spectrum2 replies carry bin data;
- RTP is sent at 50 packets/s, with optional loss, duplication and reordering;
- changing `BIN_COUNT` on a live spectrum channel "crashes" the fake;
- audio channels send unsolicited status every 0.5 s (the data group's port 5006);
- presets set the demodulator, rate and passband (`wfm` outputs 48 kHz), and every status carries the signal generator's front-end fields; `FIRST_LO_FREQUENCY` retunes only when the test makes the front end tunable.

It also fails a test if the client ever:
- has more than one command in flight for an SSRC;
- sends a command without `LIFETIME`.

`crates/stationd/tests/ka9q.rs` runs every station-conformance scenario against stationd with the ka9q backend and this fake radiod, in real time (about 3 s). `crates/stationd/tests/conformance.rs` runs them against stationd with the fake backend, in about 2 s.

### End-to-end tests against radiod

```sh
scripts/e2e.sh
```

The script:
1. builds and installs radiod if needed (about 15 s the first time), and sets the MULTICAST flag on `lo`;
2. starts radiod with the signal generator;
3. runs `crates/ka9q-spike/tests/e2e.rs` in release mode;
4. measures CPU;
5. stops radiod, even if a step failed.

Results go to `${TMPDIR:-/tmp}/coaxhub-e2e`. [docs/spike-results.md](docs/spike-results.md) says what the tests check.

The second script runs stationd itself:

```sh
scripts/e2e-station.sh
```

It starts radiod with the signal generator and runs `crates/stationd/tests/e2e_ka9q.rs` (release, `COAXHUB_E2E=1`): stationd with the ka9q backend (data group discovered from radiod) serves station-conformance's fake hub, which validates every message with `station-proto`'s §5 rules. It checks the capabilities, audio at 50 frames/s, a continuous sequence across retunes, WFM at 48 kHz with the squelch open, waterfall rows at 10/s with the 10 MHz carrier in the right bin, and backend health. Timings are printed as `E2E key=value` lines and kept in `${TMPDIR:-/tmp}/coaxhub-e2e-m2-station`. If an RTL-SDR is plugged in (and radiod has its driver), the test runs again against `testdata/radiod-rtlsdr.conf`; `RTLSDR=0` skips that, `RTLSDR=1` requires it.

Both scripts need the loopback interface to have the MULTICAST flag. `scripts/build-radiod.sh` sets it with `sudo ip link set dev lo multicast on`; in a container, give it `NET_ADMIN` or run that command once per start.

### Fuzzing

```sh
rustup toolchain install nightly --profile minimal   # once
cargo install cargo-fuzz                             # once
scripts/fuzz.sh 60                                   # seconds per target; CI uses this
```

### The spike

Start radiod first:

```sh
scripts/build-radiod.sh && scripts/run-radiod-siggen.sh
```

Then, with `cargo run --release -p ka9q-spike --` or the built binary:

```sh
ka9q-spike --report audio --freq 10e6 --mode am --secs 10 --bitrate 16000 --samprate 12000 --out am.opus
ka9q-spike --report waterfall --center 10e6 --bins 1024 --rbw 1000 --rate 10 --secs 10 --out rows.cxwf --pgm rows.pgm
ka9q-spike status                 # all channels; probes the front end when radiod has none
ka9q-spike inspect rows.cxwf      # summarises a row file, no radiod needed
ka9q-spike measure hop --secs 600 --out-dir m    # continuous duty: hop, log every poll and minute
ka9q-spike measure idle --secs 3600 --out-dir m  # the idle schedule (front end on one minute in five)
```

Then stop radiod:

```sh
scripts/run-radiod-siggen.sh stop
```

Addresses and options:
- The default addresses are `--status-addr 239.77.0.6:5006`, `--data-addr 239.77.0.4:5004`, `--interface 127.0.0.1`, `--ttl 0`.
- Every command sends `LIFETIME` (`--lifetime-frames`, default 500 = 10 s). The client refreshes it, and closes channels with `LIFETIME 1`.
- `--report` adds CPU figures as a percentage of one core: the spike's own, and radiod's, found through the `run-radiod-siggen.sh` pidfile or `--radiod-pid`.

Output:
- Each command ends with a machine-readable `RESULT key=value ...` line.
- The waterfall row file format (CXWF) is documented in `crates/ka9q-spike/src/wfstream.rs`. Rows use the station quantization, −160 dB + 0.5 dB × v, and each row is one zstd frame.

Results and the first measurements are in [docs/spike-results.md](docs/spike-results.md). The noise-floor measurement (`ka9q-spike measure`) is in [docs/measurement-spike.md](docs/measurement-spike.md). The radiod behaviour the code relies on is in [docs/radiod-notes.md](docs/radiod-notes.md). Read the notes before changing the client.

### Packages and the installer

```sh
COAXHUB_DOMAIN=coaxhub.test COAXHUB_ROOT_CA=/path/to/root.pem packaging/build-all.sh
packaging/test/test-packages.sh      # install, verify, remove, purge; arm64 checked by ELF type
packaging/test/test-installer.sh     # the installer against a throwaway signed repository
```

The four `.deb`s (arm64 and amd64 × bookworm and trixie) are built in Debian containers, arm64 by cross-compilation, and land in `target/packages/`. See [docs/packaging.md](docs/packaging.md).

### Cross-compiling for a Raspberry Pi (arm64)

Install the `aarch64-unknown-linux-gnu` Rust target and Debian's cross toolchain (`gcc-aarch64-linux-gnu`, `libc6-dev-arm64-cross`). zstd-sys needs the cross C compiler and archiver:

```sh
rustup target add aarch64-unknown-linux-gnu
CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
cargo build --release --target aarch64-unknown-linux-gnu -p ka9q-spike
```

The binary is `target/aarch64-unknown-linux-gnu/release/ka9q-spike`:
- about 4.4 MB, or 3.2 MB after `aarch64-linux-gnu-strip`;
- dynamically linked against glibc 2.34 or newer only, so it runs on Raspberry Pi OS bookworm (64-bit).

## Running stationd

`stationd` dials out to its home hub's gateway over HTTP/2 + TLS 1.3 with mutual TLS, says `Hello`, answers the hub's requests and reconnects on its own with jittered backoff (station protocol §2). It drives the bundled radiod, chosen by the owner's SDR profile, or, in development builds, a fake backend with synthetic media. Its credentials come from enrollment (`stationd enroll`), or, in development builds, from static files.

**Build flavours:**

| | development (default) | packaged (`--features packaged`) |
|---|---|---|
| CoaxHub domain and root certificate | none built in | compiled in from `COAXHUB_DOMAIN` and `COAXHUB_ROOT_CA` (a PEM path) at build time; the build fails without them |
| enrollment / StationControl | `[endpoints]` in `stationd.toml` | `https://enroll.<domain>` (verified against the built-in root only) / `https://station-api.<domain>` (verified against `gateway-ca.pem` only) |
| `state_dir`, `[endpoints]`, `[credentials]`, `[backend]`, `[measurements] ignore_host_load` | accepted | rejected with a clear error |
| SDR profiles | `rtlsdr`, `rtlsdr-hf`, `siggen` | `rtlsdr`, `rtlsdr-hf` |
| runtime status | `<state_dir>/status.json` | `/run/coaxhub/stationd/status.json` |
| radiod templates | `packaging/radiod-templates/` | `/usr/share/coaxhub/radiod-templates/` |

Only the package built by the release process is the supported way to run a station. A package you build yourself trusts the domain and root you gave it, and enrolls only against that service ([SECURITY.md](SECURITY.md) says what that means).

**Config** (`/etc/coaxhub/stationd.toml`, station protocol §7). An installed station has only the owner's policy and the SDR profile:

```toml
allow_frontend_control = true                   # default true
allowed_admin_ops = []                          # restart_stationd, restart_backend, update, reboot_host
max_channels = 16                               # default 16, at most 64
max_waterfall_subscriptions = 4                 # default 4, at most 16
[measurements]
enabled = true                                  # measure the noise floor when the hub asks (default true)
[sdr]
profile = "rtlsdr"                              # rtlsdr | rtlsdr-hf (development builds also: siggen)
# center_frequency_hz = 98000000                # optional; default from the profile
```

Development builds also accept these test-only settings:

```toml
state_dir = "/tmp/stationd-state"               # default /var/lib/coaxhub/stationd
[endpoints]                                     # enrollment against a development enrollment server
enroll_url = "https://localhost:7501"
station_api_url = "https://localhost:7502"
root_ca = "dev-root.pem"                        # the ONLY root trusted for enrollment
[credentials]                                   # static mode: no enrollment (tests, conformance, scripts)
gateway_url = "https://localhost:7443"          # https only; host is the TLS server name
cert = "/path/station.pem"                      # client certificate (URI SAN urn:coaxhub:station:<uuid>)
key = "/path/station.key"                       # warns if group/world readable
gateway_ca = "/path/ca.pem"                     # the ONLY roots trusted for the gateway
[backend]
kind = "ka9q"                                   # radiod; or "fake" (synthetic media, no radio)
[backend.ka9q]                                  # required with kind = "ka9q", refused otherwise
status_group = "239.77.0.6:5006"                # radiod's `status =` group (port 5006 by default)
data_group = "239.77.0.4:5004"                  # optional: radiod's `data =` group; default: asked from radiod
[measurements]
ignore_host_load = true                         # measuring never pauses for the load average (e2e scripts on a busy machine)
```

- Relative paths are resolved against the config file's directory; unknown keys are errors.
- `[sdr]` is required unless `[backend]` chooses the backend. Without `[backend]`, stationd drives the bundled radiod on the package's fixed groups, `239.77.0.6:5006` (status) and `239.77.0.4:5004` (data), defined once in `crates/stationd/src/profile.rs`.
- Without `[credentials]`, stationd runs on the credentials in the state directory.

**Measuring the noise floor** (station protocol 1.1, §4.1; `crates/stationd/src/measure/`).
When the hub sends a plan (`StartMeasurements`: frequency ranges and the spectrum channel's shape), stationd measures on a spectrum channel of its own, which never counts against `max_channels` or `max_waterfall_subscriptions` and never sends a front-end command:
- **Windows:** the channel is 1024 bins × 500 Hz (512 kHz) and hops between windows, 4 s each, polled twice a second; a minute's 15 slots go round-robin over that minute's windows (slot `s` → window `s mod n`). Centres sit on the backend's placement grid (radiod's: 1 kHz from the LO), a hop's own reply is its dwell's first spectrum, and nothing is discarded. A band never spans two minutes; with more windows than one minute holds, the cycle stretches over 2 or 5 measured minutes.
- **The statistic:** every bin of every spectrum, normalised to dB/Hz as for the waterfall, goes into a 0.1 dB histogram per band and minute, without the 5 % edges of the tunable range and ±2 kHz around its centre; the report carries the 10th, 25th, 50th and 90th percentiles.
- **Duty:** every minute while a listener channel or waterfall subscription is open; otherwise only the UTC minutes hh:00, hh:05, …, with the channel opened 2 s before and closed 0.5 s after (radiod's front end then runs about 62.5 s in 300 s). The duty changes at minute boundaries (`Event{MEASUREMENT_DUTY}`).
- **Pauses:** nothing is measured while the load average is at least 0.75 × the CPU count, the outbound queue dropped audio or waterfall in the last minute, or the backend is degraded; measuring resumes after 5 quiet minutes. The load comes from `/proc/loadavg`; the library takes another source (`RunOptions::measure_load`), and the tests give it a fixed one, so they don't depend on how busy the machine running them is (`measure/load.rs`).
- **Stops:** a frontend change or a backend restart stops the plan (`Event{MEASUREMENT_STOPPED}`); the hub sends a new one. A plan belongs to one connection and nothing is buffered across a disconnect.
- **Wire:** reports are low priority and dropped once older than 10 s (`StationStatus.measurement_reports_dropped_total`).
- `[measurements] enabled = false` turns it off: the capabilities say so and every plan is refused with `PERMISSION_DENIED`.

**State directory** (`/var/lib/coaxhub/stationd`, mode 0700): `key.pem` (0600, generated on the station, never sent), `cert.pem`, `chain.pem`, `gateway-ca.pem`, `assignment.json` (`{hub_id, gateway_url}`) and `station-id`. Each file is written atomically (temporary file, `fsync`, rename, directory `fsync`) and gets the directory's owner when root writes it. The station is enrolled when all six are present and parseable and the key matches the certificate. The daemon reads them again before every connection attempt, so renewed credentials are picked up without a restart.

A renewal replaces `key.pem`, `cert.pem`, `chain.pem` (and a rotated `gateway-ca.pem`) as **one** swap: the new files are written and synced in a `.next.tmp-*` directory, which is renamed to `next/` (the commit point), and then moved over the live files one by one. Readers take each of these files from `next/` while it is there, so they see either the old set or the new one, never a new certificate with the old key. A crash before the commit leaves the old set (the temporary directory is removed later); a crash after it is finished when stationd starts. `enroll --force` and `unenroll` discard a pending swap.

**Certificates for static mode.** A development build with `[credentials]` needs three files, all PEM:
- `key`: the station's private key, ECDSA P-256, readable only by the user that runs stationd;
- `cert`: the station's client certificate, followed by its issuing intermediate if there is one. Its only URI subject alternative name is `urn:coaxhub:station:<uuid>`;
- `gateway_ca`: the root certificate that signed the gateway's server certificate. It is the only trust anchor stationd uses for the gateway. The gateway's certificate must carry the host of `gateway_url` as a DNS or IP name.

The hub side of the pair trusts the issuer of `cert` for station certificates. `crates/station-conformance/src/pki.rs` generates a throwaway PKI of this shape in memory: a root, a hub server certificate and a station client certificate. Read it for the exact extensions and key usages, or use it to produce files for a manual test.

**The ka9q backend** (`backend_ka9q::backend::Ka9qBackend`, station protocol §9):
- `status_group` and `data_group` take an IP address with an optional port, or a radiod name such as `coaxhub-status.local`, hashed into a `239.x.y.z` group exactly as radiod does with its default `dns = no` (no DNS or mDNS lookup).
- Keep radiod on this host with `ttl = 0`; the loopback interface needs the MULTICAST flag (`coaxhub-radiod.service` sets it before every start).
- radiod may start after stationd: until it answers, requests get `BACKEND_ERROR` and `StationStatus` reports the backend `DOWN`. `check-config` does not contact radiod; `doctor` does.
- Modes map to radiod presets (AM `am`, FM `wfm`, NFM `fm`, USB `usb`, LSB `lsb`, CW `cwu`); channels are mono Opus, `ChannelOpened` reports the rate radiod actually runs (48 kHz for `wfm`). The waterfall is a polled spectrum2 channel whose bins tile the requested range, requantized to dB/Hz and zstd-compressed. radiod exposes no runtime gain, AGC or attenuation for the tested drivers, so capabilities advertise no front-end controls.
- The radiod facts all this relies on are in [docs/radiod-notes.md](docs/radiod-notes.md) (§10 for the backend).

**Commands** (`--config` defaults to `/etc/coaxhub/stationd.toml`):

```sh
stationd enroll [--token chs_enr_…] [--force]   # without --token: a hidden prompt, or one line from stdin
stationd run                                     # serves until SIGTERM/SIGINT
stationd status [--json]                         # enrolled?, station, certificate expiry, home hub, tunnel, backend, last error
stationd doctor                                  # pass/warn/fail per check; non-zero exit on a failure
stationd unenroll                                # deletes the credentials and assignment, keeps stationd.toml
stationd backend-config [--out PATH|-] [--templates DIR]   # renders radiod's config (default /run/coaxhub-radiod/radiod.conf)
stationd check-config                            # validates the config and credentials
stationd --version
```

- `enroll` generates an ECDSA P-256 key, sends a CSR and the token to `Enrollment.Enroll` over TLS 1.3 (ALPN h2) and verifies the server against the CoaxHub root only. It refuses an enrolled station without `--force`. The token is never logged or echoed and is zeroed after use; errors say whether the token was refused (invalid, expired, used), the network failed or TLS failed.
- `status` exits 0 when enrolled and 3 when not. It shows the certificate's expiry, serial and renewal time, and from the daemon's `status.json` (written on every change and every 10 s): the tunnel, the backend, the last error, the clock skew against the hub, and the last and next renewal. After a revocation it shows `revoked`.
- `doctor` checks: the config; the credentials; the profile's SDR on USB (`/sys/bus/usb/devices`, RTL-SDR `0bda:2838`/`0bda:2832`); radiod's status group answering within 3 s; DNS and the TLS handshake to enroll (against the root), station-api and the gateway (mTLS, against the gateway CA bundle; station-api is a warning, since the station keeps running on its stored assignment); the clock against the certificate's validity (a warning past two thirds of its lifetime); the clock skew the daemon measured against the hub (a warning above 30 s, never a failure).
- `backend-config` renders `packaging/radiod-templates/<profile>.conf` with the fixed groups and the centre frequency and writes it atomically (0644). An unknown profile is an error listing the supported ones.
- Exit codes: 0 success, 1 runtime failure (or a failed `doctor` check), 2 invalid config or credentials (and usage errors), 3 not enrolled, or the certificate was revoked (`Goodbye{REVOKED}`, or `revoked` from station-api): enroll again, 4 (`enroll` only) the token was refused (invalid, expired or already used), malformed or empty: get a new token. The service unit prevents restarts on code 3 (`RestartPreventExitStatus=3`); the installer asks for another token on code 4.
- Logs go to stderr, coloured only when stderr is a terminal; the journal gets plain text. `RUST_LOG` sets the filter (default `info`); `STATIOND_LOG_FORMAT=json` gives one JSON object per line. Key material and tokens are never logged.
- On SIGTERM/SIGINT a connected stationd sends `Event{SHUTTING_DOWN}`, closes the stream and exits 0.

**Assignment, renewal and revocation** (enrolled stations with a station-api endpoint; static `[credentials]` skip all three):

- **Assignment:** before the first connection, `run` calls `StationControl.GetAssignment` (10 s timeout) and stores a changed home hub in `assignment.json`. If station-api cannot be reached or refuses (other than `revoked`), the stored assignment is used. It asks again after `Goodbye{refresh_assignment}` or `Goodbye{REASSIGNED}`, before reconnecting.
- **Renewal:** once two thirds of the certificate's validity period (`notBefore` to `notAfter`) have passed, stationd generates a new P-256 key, sends a CSR to `StationControl.RenewCertificate` with the current certificate, checks the answer (same station, the new key, not expired) and swaps the new credentials in atomically. It then closes the tunnel and reconnects at once with the new certificate (no backoff). A failed attempt is retried after 1/24 of the validity period, between 30 s and 1 hour (hourly for 30-day certificates), with jitter; a renewed certificate that could not be stored is renewed again within a minute, because the control plane revokes the old one an hour after a renewal. With 6-minute certificates (11 minutes as seen in the certificate, since the CA backdates `notBefore` by 5 minutes), the first renewal comes about 2 min 20 s after issuance.
- **Revocation:** `Goodbye{REVOKED}` from the hub, or `PERMISSION_DENIED revoked` from station-api, logs an error, sets `status` to `revoked` and exits with code 3. The exception: when the state directory already holds a different, unexpired certificate than the refused one (a renewal or `enroll --force` just landed), stationd reconnects with it instead.
- **Clock skew:** on every `Welcome`, stationd records the hub's time minus its own (`Welcome.hub_time_unix_ms`, round trip ignored) and warns in the log above 30 s. It never refuses anything because of it.

**TLS:** TLS 1.3 only, `ring` provider, ALPN `h2` offered and required. The gateway and station-api get the station's client certificate and are verified against the gateway CA bundle alone (`gateway-ca.pem`, or `gateway_ca` in static mode); enrollment presents no client certificate and is verified against the CoaxHub root alone. No WebPKI or system roots: `stationd` does not depend on `webpki-roots` or `rustls-native-certs`.

**Conformance:** `crates/station-conformance` (MIT) is a scripted fake hub with named scenarios (`scenarios::ALL`) that any station implementation can run against itself. `crates/stationd/tests/conformance.rs` runs every scenario against the real stationd in-process over mTLS on loopback, with the fake backend; `crates/stationd/tests/ka9q.rs` runs them all again with the ka9q backend and the fake radiod.

## Installing a station

Installing a station is in the [operator guide](docs/operators.md). How the package is built and why each unit setting is there is in [docs/packaging.md](docs/packaging.md).

On a Raspberry Pi the radiod behaviour stationd relies on is the same as on a PC. The differences from the signal generator:
- the RTL-SDR front end is 2.048 MS/s complex, so channels and waterfalls must fit inside the tuned span (about ±1 MHz around `center_frequency_hz`);
- there is no built-in test carrier: use a known local signal, such as a broadcast or repeater frequency;
- a stock Linux `lo` lacks the MULTICAST flag; `coaxhub-radiod.service` sets it before every start.

## Documents

| Document | License | What |
|---|---|---|
| [docs/station-protocol.md](docs/station-protocol.md) | MIT | the station protocol: transport, session, requests, validation limits, media, configuration, backends |
| [docs/architecture.md](docs/architecture.md) | GPL-3.0-only | how the station fits together: trust model, enrollment, package layout |
| [docs/operators.md](docs/operators.md) | GPL-3.0-only | the operator guide: requirements, install, upgrade, uninstall |
| [docs/packaging.md](docs/packaging.md) | GPL-3.0-only | the package and the installer: contents, units, scripts, tests |
| [docs/releasing.md](docs/releasing.md) | GPL-3.0-only | how a release is made and verified |
| [docs/radiod-notes.md](docs/radiod-notes.md) | GPL-3.0-only | observed radiod behaviour that the ka9q backend relies on |
| [docs/measurement-spike.md](docs/measurement-spike.md) | GPL-3.0-only | the noise-floor measurement checked against radiod |
| [docs/spike-results.md](docs/spike-results.md) | GPL-3.0-only | the first measurements: audio, waterfall, CPU, RTL-SDR |
| [SECURITY.md](SECURITY.md) | GPL-3.0-only | reporting a vulnerability; the station-side security model |
| [CONTRIBUTING.md](CONTRIBUTING.md) | MIT | DCO, license boundary, tests |

## License

Two licenses, split by directory ([CONTRIBUTING.md](CONTRIBUTING.md) has the full table, the DCO sign-off requirement and the SPDX header rule):

- **MIT** ([LICENSE-MIT](LICENSE-MIT)): the station protocol (`proto/coaxhub/station/v1/`), `crates/station-proto/` (crate `coaxhub-station-proto`, lib `station_proto`), `crates/station-conformance/`, and the protocol document.
- **GPL-3.0-only** ([LICENSE-GPL-3.0](LICENSE-GPL-3.0)): everything else: `stationd`, `station-backend`, `backend-ka9q`, `ka9q-spike`, packaging and the installer.

Every source file starts with an SPDX header naming which of the two applies to it. `scripts/check-license-boundary.sh` (run in CI) fails the build if an MIT crate ever grows a dependency, transitive or direct, on a GPL crate. The protocol side must stay usable by someone building a non-GPL station implementation.
