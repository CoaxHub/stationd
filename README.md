<!-- SPDX-License-Identifier: GPL-3.0-only -->

# stationd

stationd is the station daemon of [CoaxHub](https://coaxhub.com). It connects an SDR station to the CoaxHub service over one outbound mTLS tunnel, so that listeners can hear the station from the web. It opens no inbound port, so it works behind NAT. It drives a bundled [ka9q-radio](https://github.com/ka9q/ka9q-radio) `radiod` and answers the hub's requests (open a channel, subscribe to a waterfall, measure the noise floor) over the station protocol.

**To run a station, see [coaxhub.com/docs/station](https://coaxhub.com/docs/station).** This repository is for building, changing and packaging the software. It holds everything on the station side:

- the **station protocol**, `proto/` and `crates/station-proto/` (MIT): the wire format that the hub and any station implementation build against, with the validation both sides share;
- **stationd**, `crates/stationd/`, its backend trait, `crates/station-backend/`, and the radiod backend, `crates/backend-ka9q/` (GPL-3.0-only);
- **station-conformance**, `crates/station-conformance/` (MIT): a scripted fake hub for testing a station implementation;
- the Debian package `coaxhub-stationd` (`packaging/`) and the installer (`installer/`), GPL-3.0-only;
- `scripts/`: radiod built at its pinned commit, the end-to-end runs, fuzzing and the license-boundary check;
- `testdata/`: radiod configs (a signal generator, so that everything can be tested without SDR hardware, and two RTL-SDR profiles).

## Building

Run every command from the repository root. You need the Rust release in `rust-toolchain.toml` (rustup installs it on the first `cargo` call), `protoc`, `jq`, `cargo-nextest` and `cargo-deny`; Docker with BuildKit for the packages; and ka9q-radio's build dependencies for the end-to-end tests. On Debian:

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

That is the **development build**: no CoaxHub domain or root certificate built in, the `siggen` SDR profile, and the test-only settings below. The **packaged build** (`--features packaged`), which the Debian package ships, compiles the domain and the root in from `COAXHUB_DOMAIN` and `COAXHUB_ROOT_CA` and rejects those settings; [docs/packaging.md](docs/packaging.md) builds the packages. Only the package built by the release process is the supported way to run a station: a package you build yourself enrolls only against the service whose domain and root you gave it ([SECURITY.md](SECURITY.md)).

Tests, fuzzing, the end-to-end runs against radiod and the package tests: [CONTRIBUTING.md](CONTRIBUTING.md).

## Running a development build

A development build accepts, in `stationd.toml`, settings a packaged build refuses (station protocol §7 has the owner's part of the file):

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
kind = "ka9q"                                   # radiod; or "fake" (synthetic media, no radio)
[backend.ka9q]                                  # required with kind = "ka9q", refused otherwise
status_group = "239.77.0.6:5006"                # radiod's `status =` group
data_group = "239.77.0.4:5004"                  # optional: radiod's `data =` group; default: asked from radiod
[measurements]
ignore_host_load = true                         # measuring never pauses for the load average (e2e on a busy machine)
```

- Without `[credentials]`, stationd runs on the credentials in the state directory, which `stationd enroll` gets from `[endpoints]`.
- Without `[backend]`, stationd drives the bundled radiod on the package's fixed groups, `239.77.0.6:5006` and `239.77.0.4:5004`, and `[sdr]` is required.
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
