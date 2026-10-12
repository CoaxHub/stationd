<!-- SPDX-License-Identifier: GPL-3.0-only -->

# stationd

stationd is the station daemon of [CoaxHub](https://coaxhub.com). It connects an SDR station to the CoaxHub service over one outbound mTLS tunnel, so that listeners can hear the station from the web. It opens no inbound port, so it works behind NAT. It drives a bundled [ka9q-radio](https://github.com/ka9q/ka9q-radio) `radiod` and answers the hub's requests (open a channel, subscribe to a waterfall, measure the noise floor) over the station protocol.

**To run a station, see [coaxhub.com/docs/station](https://coaxhub.com/docs/station).** This repository is for building, changing and packaging the software. It holds everything on the station side:

- the **station protocol**, `proto/` and `crates/station-proto/` (MIT): the wire format that the hub and any station implementation build against, with the validation both sides share;
- **stationd**, `crates/stationd/`, its backend trait, `crates/station-backend/`, and the radiod backend, `crates/backend-ka9q/` (GPL-3.0-only);
- **station-conformance**, `crates/station-conformance/` (MIT): a scripted fake hub for testing a station implementation;
- the Debian package `coaxhub-stationd` (`packaging/`) and the installer (`installer/`), GPL-3.0-only;
- `scripts/`: radiod built at its pinned commit, the end-to-end runs, fuzzing and the license-boundary check;
- `testdata/`: radiod configs (a signal generator, so that everything can be tested without SDR hardware, and the three RTL-SDR profiles: VHF/UHF, HF by direct sampling, and the RTL-SDR Blog V4).

## Installing a station

A station is a Debian or Raspberry Pi OS computer (bookworm or trixie, amd64 or arm64) with one or more RTL-SDRs on USB. The installer adds the signed CoaxHub APT repository, installs the `coaxhub-stationd` package, asks for the enrollment token from the web app (with the input hidden) and starts `stationd.service`:

```sh
curl -fsSL https://get.coaxhub.com/station | sudo sh
```

It asks nothing about the SDRs. [coaxhub.com/docs/station](https://coaxhub.com/docs/station) is the owner's guide; [docs/packaging.md](docs/packaging.md) describes the package and every step of the installer.

## Receivers

A station serves one **receiver** per SDR it uses, up to 16. stationd reports the SDRs plugged in, each with the models its package offers for it, and every profile the package has. The owner sets up the receivers in the web app (which SDR, its model, the band), and the hub sends the whole set to the station as `ConfigureReceivers` ([station protocol §4.2](docs/station-protocol.md#42-hardware-and-receivers)). stationd checks the set against fixed rules (the SDR table, the profiles' centre ranges and gains, no overlapping receivers), keeps it, and runs one radiod per receiver, `coaxhub-radiod@<receiver_id>.service`, each rendered from its profile's template. A receiver whose SDR is unplugged waits for it (`NO_DEVICE`) and starts when it is plugged in.

| Profile | For | Centre |
|---|---|---|
| `rtlsdr` | an RTL-SDR on VHF/UHF, through its tuner | 24–1766 MHz |
| `rtlsdr-hf` | an RTL-SDR with direct sampling (such as a Blog V3) on HF | 0.5–28.8 MHz |
| `rtlsdr-v4` | an RTL-SDR Blog V4, on HF through its upconverter and on VHF/UHF | 0.5–1766 MHz |

## What a station keeps

A packaged station has no settings file: everything the owner chooses lives in the web app ([station protocol §7](docs/station-protocol.md#7-what-the-station-keeps)). On the station there is:

- `/var/lib/coaxhub/stationd/` (mode 0700, user `coaxhub`): the station's key, which never leaves it, its certificate, the gateway's CA bundle, its home hub and its id;
- `/var/lib/coaxhub/receivers/receivers.json` (mode 0640, group `coaxhub-radiod`): the receiver set the hub last applied, so the receivers run while the tunnel is down and after a reboot;
- `/run/coaxhub-radiod/<receiver_id>/radiod.conf`: each receiver's radiod config, rendered again before every start.

## The commands

```text
stationd run                       the daemon (stationd.service runs it)
stationd enroll [--force]          enroll with a one-time token from the web app
stationd status [--json]           enrollment, certificate, home hub, receivers
stationd doctor                    checks the enrollment, the SDRs, the receivers, the network and the clock
stationd unenroll                  deletes the credentials, keeps the receivers
stationd backend-config --receiver ID
                                   renders a receiver's radiod config (its unit runs it)
```

[docs/architecture.md](docs/architecture.md#the-stationd-commands) has the options and the exit codes.

## Building

Run every command from the repository root. You need the Rust release in `rust-toolchain.toml` (rustup installs it on the first `cargo` call), `protoc`, `jq`, `cargo-nextest` and `cargo-deny`; Docker with BuildKit for the packages; and ka9q-radio's build dependencies for the end-to-end tests. On Debian:

```sh
sudo apt-get install --no-install-recommends \
    protobuf-compiler libprotobuf-dev pkg-config libssl-dev clang \
    libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libogg-dev \
    libsamplerate0-dev libusb-1.0-0-dev libncurses-dev librtlsdr-dev rtl-sdr \
    sox tcpdump opus-tools sysstat iproute2
```

The end-to-end tests' radiod (`scripts/build-radiod.sh`) links the system's librtlsdr, which drives an RTL-SDR Blog V4 from version 2.0.0 on (trixie's 2.0.2, not bookworm's 0.6.0). The package bundles its own, the same on every suite ([docs/packaging.md](docs/packaging.md#the-bundled-librtlsdr)).

```sh
cargo build --release
```

That is the **development build**: no CoaxHub domain or root certificate built in, the `siggen` SDR profile, and the test-only settings below. The **packaged build** (`--features packaged`), which the Debian package ships, compiles the domain and the root in from `COAXHUB_DOMAIN` and `COAXHUB_ROOT_CA` and rejects those settings; [docs/packaging.md](docs/packaging.md) builds the packages. Only the package built by the release process is the supported way to run a station: a package you build yourself enrolls only against the service whose domain and root you gave it ([SECURITY.md](SECURITY.md)).

Tests, fuzzing, the end-to-end runs against radiod and the package tests: [CONTRIBUTING.md](CONTRIBUTING.md).

## Running a development build

A development build reads `stationd.toml` (`--config`, default `/etc/coaxhub/stationd.toml`) for the settings tests and LAN setups need; a packaged build reads no file. Every key is optional, and unknown keys are errors:

```toml
state_dir = "/tmp/stationd-state"               # default /var/lib/coaxhub/stationd
[endpoints]                                     # enrollment against a development enrollment server
enroll_url = "https://localhost:7501"
station_api_url = "https://localhost:7502"
root_ca = "dev-root.pem"                        # the ONLY root trusted for enrollment
[credentials]                                   # static mode: no enrollment (tests, conformance, scripts)
gateway_url = "https://localhost:7443"          # https only; the host is the TLS server name
cert = "/path/station.pem"                      # client certificate (URI SAN urn:coaxhub:station:<uuid>)
key = "/path/station.key"                       # warns if group or world readable
gateway_ca = "/path/ca.pem"                     # the ONLY roots trusted for the gateway
[backend]
kind = "fake"                                   # synthetic media, no radio; or "ka9q" (the default)
usb_devices = "usb"                             # where SDRs are read from (default /sys/bus/usb/devices);
                                                # simulated SDRs are directories laid out like it
[backend.ka9q]                                  # one radiod of your own for every receiver; stationd starts no unit
status_group = "239.77.0.6:5006"                # radiod's `status =` group
data_group = "239.77.0.4:5004"                  # optional: radiod's `data =` group; default: asked from radiod
[measurements]
ignore_host_load = true                         # measuring never pauses for the load average (e2e on a busy machine)
```

- Without `[credentials]`, stationd runs on the credentials in the state directory, which `stationd enroll` gets from `[endpoints]`.
- Without `[backend]`, or with `kind = "ka9q"` and no `[backend.ka9q]`, each receiver runs the bundled radiod on its slot's groups, started through systemd, as on a packaged station. With `kind = "fake"`, each receiver is a fake backend and no unit is started.
- With `state_dir`, the applied receivers live in `<state_dir>/receivers/receivers.json`.
- `stationd check-config` (development builds only) validates the file, the credentials and the applied receivers.
- `[credentials]` takes PEM files of the shape `crates/station-conformance/src/pki.rs` generates in memory: a P-256 key readable only by stationd's user, a client certificate whose only URI SAN is `urn:coaxhub:station:<uuid>` (followed by its intermediate, if any), and the root that signed the gateway's certificate, the only trust anchor used for it.

## Documents

| Document | License | What |
|---|---|---|
| [coaxhub.com/docs/station](https://coaxhub.com/docs/station) | — | running a station: requirements, install, settings, troubleshooting, removal |
| [docs/station-protocol.md](docs/station-protocol.md) | MIT | the station protocol: transport, session, requests, validation limits, media, configuration, backends |
| [docs/architecture.md](docs/architecture.md) | GPL-3.0-only | how the station fits together: trust model, enrollment, the package, what the hub can and cannot do |
| [docs/packaging.md](docs/packaging.md) | GPL-3.0-only | the package and the installer: building, contents, units, scripts, tests |
| [docs/releasing.md](docs/releasing.md) | GPL-3.0-only | how a release is made and verified |
| [docs/radiod-notes.md](docs/radiod-notes.md) | GPL-3.0-only | observed radiod behaviour that the ka9q backend relies on; read it before changing the client |
| [SECURITY.md](SECURITY.md) | GPL-3.0-only | reporting a vulnerability; the station-side security model |
| [CONTRIBUTING.md](CONTRIBUTING.md) | MIT | DCO, the license boundary, tests |

## License

Two licenses, split by directory: **MIT** ([LICENSE-MIT](LICENSE-MIT)) for the station protocol, `station-proto`, `station-conformance` and the protocol document; **GPL-3.0-only** ([LICENSE-GPL-3.0](LICENSE-GPL-3.0)) for everything else. Every source file starts with an SPDX header naming which applies. [CONTRIBUTING.md](CONTRIBUTING.md) has the full table and the boundary rule: an MIT crate never depends on a GPL crate, so the protocol side stays usable by a non-GPL station implementation.
