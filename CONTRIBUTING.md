<!-- SPDX-License-Identifier: MIT -->

# Contributing to stationd

Thanks for contributing. This repository is the station side of CoaxHub: the protocol, the station daemon, backend adapters, packaging and the conformance suite. The CoaxHub hub and control plane are developed elsewhere. This repository holds everything a station, or an alternative hub implementation, needs.

## Developer Certificate of Origin (DCO)

Every commit must be signed off, certifying that you wrote it or otherwise have the right to submit it under the license that applies to the files it touches (see [developercertificate.org](https://developercertificate.org/)):

```sh
git commit -s -m "Add ..."
```

This adds a `Signed-off-by: Your Name <you@example.com>` trailer to the commit message, using the name and email from your git config. CI rejects pull requests containing unsigned commits. There is no CLA: your contribution stays under the license of the part of the tree it lands in (see below), and you keep your copyright.

## License boundary

This repository mixes two licenses, split by directory:

| Path | License | What |
|---|---|---|
| `proto/coaxhub/station/v1/` | **MIT** | The station protocol, source of truth |
| `crates/station-proto/` | **MIT** | Generated types, limits, validation, backoff, station identity (crate `coaxhub-station-proto`, lib `station_proto`) |
| `crates/station-conformance/` | **MIT** | Scripted fake hub used to test a station implementation |
| `crates/stationd/` | **GPL-3.0-only** | The station daemon |
| `crates/station-backend/` | **GPL-3.0-only** | The `Backend` trait and `FakeBackend` |
| `crates/backend-ka9q/` | **GPL-3.0-only** | The ka9q-radio (`radiod`) backend |
| `crates/ka9q-spike/` | **GPL-3.0-only** | The spike CLI |
| `packaging/`, `installer/`, `scripts/` | **GPL-3.0-only** | Debian packaging, systemd units, the install script, test and build scripts |
| `docs/` | Protocol docs MIT, the rest GPL-3.0-only | Match the license of what they document |

The boundary rule, also enforced by `scripts/check-license-boundary.sh` in CI: **MIT crates never depend on GPL crates**, transitively or otherwise. `station-proto` and `station-conformance` must stay buildable and usable by someone who wants to write an alternative, non-GPL station implementation against the protocol. If you need to share code between the daemon and the protocol crate, it belongs in `station-proto`, not the other way around.

When you are not sure which license a new file should carry, match the directory it lives in. When adding a whole new crate, decide the directory (and therefore the license) first.

## SPDX headers

Every source file (`.rs`, `.proto`, `.sh`, Markdown, and any other file where this is meaningful) starts with an SPDX header naming the license of the directory it is in:

```rust
// SPDX-License-Identifier: MIT
```

or

```rust
// SPDX-License-Identifier: GPL-3.0-only
```

In Markdown the header is an HTML comment: `<!-- SPDX-License-Identifier: MIT -->`. `cargo fmt` and `clippy` don't check this for you. Review it yourself before opening a pull request.

## Running tests, fuzzing and the e2e suite

Run everything from the repository root. You need the Rust release in `rust-toolchain.toml`, `protoc`, `jq`, `cargo-nextest` and `cargo-deny`. Fuzzing needs a nightly toolchain and `cargo-fuzz`. The end-to-end tests need ka9q-radio's build dependencies; the [README](README.md#building-and-testing) lists the Debian packages.

**Hermetic tests** (no radiod, no multicast, no network):

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
./scripts/check-license-boundary.sh
```

**Fuzzing** (`crates/station-proto/fuzz/` and `crates/backend-ka9q/fuzz/`, nightly only):

```sh
rustup toolchain install nightly --profile minimal   # once
cargo install cargo-fuzz                             # once
./scripts/fuzz.sh 60                                 # seconds per target; CI uses this
```

A crash found by fuzzing gets a regression test using the saved corpus input before the fix. Don't just delete the crashing input.

**End-to-end tests** against a real `radiod` (no SDR hardware needed, they use the built-in signal generator):

```sh
./scripts/e2e.sh
./scripts/e2e-station.sh
```

`e2e.sh` builds and starts `radiod`, sets the loopback `MULTICAST` flag, runs the spike's end-to-end tests, and always stops `radiod` afterwards, even on failure. `e2e-station.sh` does the same for stationd with the ka9q backend, and repeats it on an RTL-SDR when one is plugged in. [docs/spike-results.md](docs/spike-results.md) says what the first checks. [docs/radiod-notes.md](docs/radiod-notes.md) describes the `radiod` behaviour the code depends on. Read it before touching the ka9q client.

**Packages and the installer** (Docker with BuildKit):

```sh
COAXHUB_DOMAIN=coaxhub.test COAXHUB_ROOT_CA=/path/to/throwaway-root.pem packaging/build-all.sh
packaging/test/test-packages.sh
packaging/test/test-installer.sh
```

## Pull requests

- Keep the license boundary intact; CI checks it.
- Add tests for new behaviour (unit, property, or e2e as appropriate) and update the documents when behaviour visible to operators or protocol implementers changes.
- Say in the description what the change implements. Cite the protocol section (`docs/station-protocol.md §n`) or link the issue. Nothing else is needed.
