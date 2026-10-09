<!-- SPDX-License-Identifier: MIT -->
# Architecture

This document describes `stationd` from the station's side: what it connects to, what it trusts, what it keeps on disk and what the hub can ask of it. The wire details are in [the station protocol](station-protocol.md).

## Overview

```
  control plane ◄── Enroll / GetAssignment / RenewCertificate (TLS, dialed out by the station)
  hub gateway   ◄── one gRPC stream over HTTP/2 + TLS 1.3 + mTLS (dialed out by the station)
                         ▲
  ┌──────────────────────┴────────── station (the owner's premises) ─────────┐
  │ stationd                                                                │
  │   ├─ tunnel client (reconnects with backoff and jitter)                 │
  │   ├─ request handler and local policy (stationd.toml)                   │
  │   ├─ backend adapter ── ka9q-radio (TLV control, RTP Opus, spectrum)    │
  │   ├─ waterfall quantizer and zstd                                       │
  │   └─ health, measurements, certificate renewal                          │
  │ radiod (ka9q-radio, C, a separate process) ◄── SDR hardware (USB)      │
  └─────────────────────────────────────────────────────────────────────────┘
```

A station does the radio work: it channelises, demodulates, encodes Opus and computes spectra. The hub fans the result out to listeners. `stationd` is written in Rust. radiod is upstream C and runs as its own service.

## The outbound-only tunnel

The station only dials out. It opens no listening port, needs no port forwarding and works behind NAT or a firewall that allows TCP 443 outbound. The hub does not connect to the station. Instead, the station opens one bidirectional gRPC stream to its home hub, and the hub sends its requests on that stream.

The station is the client of the stream but the **responder** in the protocol: it answers requests and streams what was asked for. It sends audio and waterfall rows only for channels and subscriptions that the hub opened. When the stream drops, everything on it ends, and the station reconnects with jittered exponential backoff (up to 60 s). Restarting a hub needs no action on any station.

## Endpoints

| Hostname | Used for | Server certificate verified against | Client certificate |
|---|---|---|---|
| `enroll.<domain>` | `Enrollment.Enroll`, once | the root certificate built into the release build | none (a one-time token) |
| `station-api.<domain>` | `StationControl`: `GetAssignment`, `RenewCertificate` | `gateway-ca.pem` | the station certificate |
| `gw.<hub>.<domain>` | the tunnel, `StationGateway.Connect` | `gateway-ca.pem` | the station certificate |

The hub's gateway URL comes from the assignment (`assignment.json`), never from configuration. A release build has the domain and the root certificate compiled in, so an installed station has no endpoint settings to get wrong.

TLS is 1.3 only, with ALPN `h2` offered and required. `stationd` trusts no WebPKI or system roots. Each hostname is checked against a fixed trust anchor: the built-in root for enrollment, the CA bundle received at enrollment for everything else.

## Enrollment

1. The owner creates a station in the web app and gets a one-time token and an install command. The token is `chs_enr_` plus 32 characters, valid for 15 minutes, usable once.
2. The installer adds the package repository, installs `coaxhub-stationd`, detects the SDR and writes its profile to `stationd.toml`. It then runs `stationd enroll`, which reads the token from the terminal with echo off, or from stdin.
3. `stationd` generates an ECDSA P-256 key. The key never leaves the station.
4. It sends the token and a CSR to `enroll.<domain>`. The control plane consumes the token and returns the signed certificate, the CA bundle for the gateway and `station-api`, and the assignment (the home hub).
5. `stationd` stores them in its state directory and the services start. The station connects to its hub and reports itself online.

Re-enrollment, after a revocation or a lost disk, uses a new token on the same station ID and revokes every earlier certificate. A station that was refused a token exits with code 4.

## Certificates and renewal

- A station certificate is valid for 30 days by default.
- At two thirds of its validity period, `stationd` makes a new key and a CSR and calls `StationControl.RenewCertificate`, presenting the current certificate. It checks the answer, swaps the new credentials in as one atomic set and reconnects the tunnel at once.
- A failed renewal is retried after 1/24 of the validity period, between 30 s and 1 hour.
- About an hour after a successful renewal, the control plane revokes the station's older certificates. A stolen key therefore stops working one renewal cycle after the theft.
- A tunnel never outlives its certificate: the hub ends it with `Goodbye{SHUTDOWN}` when `notAfter` passes. An expired certificate means re-enrollment.

**Revocation as the station sees it.** `Goodbye{REVOKED}` from the hub, or `PERMISSION_DENIED revoked` from `station-api`, makes `stationd` log an error, report `revoked` in `stationd status` and exit with code 3. The service unit does not restart on code 3. The owner enrolls again.

**Disabled, killed and quarantined.** A disabled station is refused at the tunnel with `Goodbye{DISABLED}` and retries every 5 minutes. It still renews, so enabling it again needs no new enrollment. A killed or quarantined station is refused at the tunnel and cannot renew, so its certificate lapses. A station is quarantined after three protocol violations in 24 hours.

## PKI shape

| Certificate | Issued by | Lifetime |
|---|---|---|
| Root | itself, kept offline | long |
| Station intermediate | the root; issues the station leaves at enrollment and renewal | long |
| Server intermediate | the root; issues the hub gateway, `enroll` and `station-api` server certificates | long |
| Station leaf | the station intermediate | 30 days |

A station leaf carries the station ID in a URI SAN, `urn:coaxhub:station:<uuid>`. The subject is ignored. The hub verifies the chain against the station intermediate and reads only that SAN. `stationd` keeps the leaf, the chain and the CA bundle in its state directory; the root it needs for enrollment is part of the binary.

## Audio and waterfall parameters

The hub chooses the parameters per channel; the station honours them or answers an error.

| Mode | radiod preset | Sample rate | Bitrate |
|---|---|---|---|
| AM, USB, LSB, CW, NFM | `am`, `usb`, `lsb`, `cwu`, `fm` | 12 kHz | 16–32 kbit/s |
| FM (broadcast) | `wfm` | 24 kHz (radiod runs `wfm` at 48 kHz) | 20–32 kbit/s |

- Audio is mono Opus, 20 ms frames. `AudioFrame.timestamp` counts in 48 kHz units whatever the sample rate. The protocol allows bitrates of 6–64 kbit/s, sample rates of 8, 12, 16, 24 and 48 kHz and frames of 10, 20, 40 and 60 ms.
- The waterfall is a polled spectrum channel. Each bin is normalised to dB/Hz, mapped to `uint8` with the subscription's `Quantization` (default offset −130 dB, step 0.5 dB, so −130 to −2.5 dB/Hz) and compressed with zstd. A row has 64–16,384 bins; the hub asks for what it needs, up to the station's `max_bins`.
- When the station reduces its FFT to fewer bins, each output bin is the maximum of the bins it covers.
- Under load the station drops waterfall rows before audio ([backpressure](station-protocol.md#63-backpressure-station)).
- A station that measures also runs a low-rate spectrum channel of its own for the noise floor ([measurements](station-protocol.md#41-measurements-11)). It is not listener capacity and never retunes the front end.

## The package on a station

One package, `coaxhub-stationd`, built for `arm64` and `amd64`:

| Path | Contents |
|---|---|
| `/usr/bin/stationd` | the daemon |
| `/usr/lib/coaxhub/radiod/` | the pinned ka9q-radio build (`radiod`, its drivers and presets), private and not on `PATH`; its GPL-3.0 source is published with each release |
| `/usr/share/coaxhub/radiod-templates/` | radiod config templates, one per SDR profile (`rtlsdr`, `rtlsdr-hf`) |
| `/etc/coaxhub/stationd.toml` | the owner's only configuration file |
| `/var/lib/coaxhub/stationd/` | state: key, certificate, chain, CA bundle, assignment, station ID (mode 0700) |
| `/run/coaxhub/stationd/status.json` | runtime status, read by `stationd status` |
| `stationd.service` | the daemon, as user `coaxhub` |
| `coaxhub-radiod.service` | radiod, as user `coaxhub-radiod`; `stationd` starts after it |

radiod is an implementation detail. The owner picks an SDR profile in `stationd.toml`. Before every start, `coaxhub-radiod.service` runs `stationd backend-config`, which renders radiod's config from the profile into `/run/coaxhub-radiod/`. The multicast groups are fixed by the package and `stationd` connects only to the bundled radiod. Each release pins one radiod commit: the one the TLV tag table was generated from.

Hardening:

- radiod runs as its own user with USB access to the SDR and `CAP_SYS_NICE`. A udev rule gives the RTL-SDR to that user's group alone, and the package blacklists the `dvb_usb_rtl28xxu` kernel driver. Its network is limited to loopback and multicast.
- `stationd.service` runs with `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, restricted address families, an empty capability bounding set and write access to `/var/lib/coaxhub` only. It waits for the clock to synchronise, because TLS would fail without it. It never runs as root.
- `RestartPreventExitStatus=3` keeps a revoked or unenrolled station from restarting in a loop.

The `stationd` commands (`--config` defaults to `/etc/coaxhub/stationd.toml`):

- `run`: serves until SIGTERM or SIGINT; a connected station then sends `Event{SHUTTING_DOWN}`, closes the stream and exits 0.
- `enroll [--token chs_enr_…] [--force]`: generates the key, sends a CSR and the token to the enrollment service and stores the credentials; it refuses an enrolled station without `--force`.
- `status [--json]`: the enrollment, the certificate and the daemon's state, from `status.json`.
- `doctor`: checks the config, the credentials, the SDR, radiod and the endpoints.
- `unenroll`: deletes the credentials and the assignment, keeps `stationd.toml`.
- `backend-config [--out PATH|-] [--templates DIR]`: renders radiod's config for the profile.
- `check-config`: validates the config and the credentials without contacting radiod.

Exit codes: 0 success; 1 a runtime failure, or a failed `doctor` check; 2 invalid config or credentials, and usage errors; 3 not enrolled, or the certificate was revoked (`Goodbye{REVOKED}`, or `revoked` from `station-api`), so enroll again; 4 (`enroll` only) the token was refused, malformed or empty, so get a new one. The service unit prevents restarts on code 3; the installer asks for another token on code 4.

Logs go to stderr, coloured only when stderr is a terminal, so the journal gets plain text. `RUST_LOG` sets the filter (default `info`) and `STATIOND_LOG_FORMAT=json` gives one JSON object per line. Key material and tokens are never logged. The owner's view of all this is on the website's [station page](https://coaxhub.com/docs/station).

## What the hub can and cannot do

The hub can send the requests of [the station protocol](station-protocol.md#4-requests-and-responses): open, update and close channels, subscribe to waterfalls, ask for capabilities, send pings, change the front end, run a measurement plan and, if allowed, run an admin operation. Everything on that list is bounded by the owner's local caps in `stationd.toml`, and the station answers `PERMISSION_DENIED` or `RESOURCE_EXHAUSTED` beyond them.

The hub cannot:

- **Run a shell or any command.** There is no such request.
- **Push configuration.** `stationd.toml` is changed only by the owner, on the station. The hub can ask for less than the local caps, never more.
- **Retune the front end** when `allow_frontend_control = false`.
- **Send a request the station doesn't know.** An unknown command or enum value is answered `UNSUPPORTED`.
- **Read the key.** It never leaves the station.

**Admin operations** are typed: `restart_stationd`, `restart_backend`, `update` and `reboot_host`. Each is refused unless the owner listed it in `allowed_admin_ops`, which is empty by default. `stationd` itself never runs as root: an operation that needs privilege goes through a separate helper that allows only those specific invocations. This repository does not yet ship that helper, so these operations are not yet available on an installed station.

**The station is untrusted in the other direction.** The hub validates every message against the limits in [the protocol's validation rules](station-protocol.md#5-hub-side-validation-normative). A violation ends the session and counts a strike. These checks are the same functions in `coaxhub-station-proto` that the station's own tests use.
