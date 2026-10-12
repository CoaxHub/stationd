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
  │   ├─ request handler, checked by the package's fixed rules              │
  │   ├─ hardware report; applied receiver set (receivers.json)             │
  │   ├─ starts and stops the receivers' units over D-Bus (polkit rule)     │
  │   ├─ backend adapter ── one ka9q-radio client per receiver              │
  │   │                     (TLV control, RTP Opus, spectrum)               │
  │   ├─ waterfall quantizer and zstd                                       │
  │   └─ health, measurements, certificate renewal                          │
  │ radiod, one process per receiver (ka9q-radio, C) ◄── its SDR (USB)      │
  └─────────────────────────────────────────────────────────────────────────┘
```

A station does the radio work: it channelises, demodulates, encodes Opus and computes spectra. The hub fans the result out to listeners. `stationd` is written in Rust. radiod is upstream C and runs as its own service, one per receiver.

A station has one or more **receivers**: a receiver is one SDR with its own radiod, passband, front-end controls, health, waterfall and noise-floor measurements. The station stays the unit for everything else: one certificate, one tunnel, one `stationd`. The owner sets the receivers up on the station's page in the web app, not on the station ([hardware and receivers](#hardware-and-receivers)).

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
2. The installer adds the package repository, installs `coaxhub-stationd` and waits for the clock to synchronise. It asks nothing about the SDR. It then runs `stationd enroll`, which reads the token from the terminal with echo off, or from stdin.
3. `stationd` generates an ECDSA P-256 key. The key never leaves the station.
4. It sends the token and a CSR to `enroll.<domain>`. The control plane consumes the token and returns the signed certificate, the CA bundle for the gateway and `station-api`, and the assignment (the home hub).
5. `stationd` stores them in its state directory and the installer starts `stationd.service`. The station connects to its hub, reports itself online and reports the SDRs it sees. The receivers come from the hub: the owner sets them up on the station's page, and the hub configures them ([hardware and receivers](#hardware-and-receivers)). A station can be installed before its SDR is plugged in.

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
- A waterfall subscription, like a channel, names one receiver. The hub composes a listener's row from several receivers' rows when the viewport spans them.
- A station that measures (a setting in the web app) also runs a low-rate spectrum channel of its own on each receiver in the plan, for the noise floor ([measurements](station-protocol.md#41-measurements)). It is not listener capacity and never retunes the front end.

## The package on a station

One package, `coaxhub-stationd`, built for `arm64` and `amd64`:

| Path | Contents |
|---|---|
| `/usr/bin/stationd` | the daemon |
| `/usr/lib/coaxhub/radiod/` | the pinned ka9q-radio build (`radiod`, its drivers and presets) and the pinned osmocom librtlsdr its RTL-SDR driver links, private and not on `PATH`; their GPL source is published with each release |
| `/usr/share/coaxhub/radiod-templates/` | radiod config templates, one per SDR profile (`rtlsdr`, `rtlsdr-hf`, `rtlsdr-v4`) |
| `/var/lib/coaxhub/stationd/` | state: key, certificate, chain, CA bundle, assignment, station ID (mode 0700) |
| `/var/lib/coaxhub/receivers/receivers.json` | the receiver set the hub last applied, with each receiver's local slot (owner `coaxhub`, group `coaxhub-radiod`, mode 0640) |
| `/run/coaxhub/stationd/status.json` | runtime status, read by `stationd status` |
| `/run/coaxhub-radiod/<receiver_id>/radiod.conf` | one receiver's radiod config, rendered at every start of its unit |
| `stationd.service` | the daemon, as user `coaxhub` |
| `coaxhub-radiod@.service` | the template unit: one instance per receiver, `coaxhub-radiod@<receiver_id>.service`, as user `coaxhub-radiod`; `stationd` starts and stops the instances |
| `/usr/share/polkit-1/rules.d/coaxhub-stationd.rules` | lets `stationd` start, restart and stop those instances, and nothing else |

A packaged station has no configuration file ([station protocol §7](station-protocol.md#7-what-the-station-keeps)). What the owner chooses lives in the web app, and the hub sends it as typed requests.

## Hardware and receivers

radiod is an implementation detail. The owner never writes its configuration, and nothing the hub sends is configuration text.

1. **The station reports its SDRs.** `stationd` reads the USB SDRs from sysfs (vendor and product IDs, manufacturer, product and serial strings, the port; no device access is needed) and answers `GetHardware`. It raises `Event{HARDWARE_CHANGED}` when one is plugged in or removed. For each SDR it adds the models and profiles its package's table allows, and it lists every profile the package has with what each can do ([station protocol §4.2](station-protocol.md#42-hardware-and-receivers)). Strings from the SDR are display-only claims.
2. **The owner sets up receivers in the web app**, one per SDR they want to use: the model (from the list the station gave), the band or centre, an antenna and a label. The control plane stores only models and profiles the hardware report allows, and predicts each receiver's tunable range from the profile's span so it can refuse overlapping receivers. A receiver keeps its SDR's serial while the SDR is unplugged, so the set can be sent again and edited.
3. **The hub sends `ConfigureReceivers`** on every connection and whenever the set changes: for each receiver its ID, its SDR's serial, model, profile, centre and front-end settings, as enums, numbers and a serial. `stationd` checks the whole set against the package's rules and refuses it, changing nothing, if one is broken. A receiver whose SDR is not plugged in is accepted when its profile is one the package has and its centre and settings fit it; its model is checked against the SDR when the SDR appears.
4. **`stationd` applies the difference.** It writes the set to `receivers.json`, then starts, restarts or stops `coaxhub-radiod@<receiver_id>.service` through systemd's D-Bus API. Before every start the unit runs `stationd backend-config --receiver <receiver_id>`, which renders that receiver's radiod config from the profile's template and `receivers.json` into `/run/coaxhub-radiod/<receiver_id>/radiod.conf`. It reports each receiver's state (`STARTING`, `RUNNING`, `FAILED` with the reason, `NO_DEVICE` while its SDR is not plugged in) as `Event{RECEIVER_STATE}`, and starts a `NO_DEVICE` receiver by itself when its SDR is plugged in.
5. **At boot**, `stationd` starts every receiver in `receivers.json` before it connects, so the receivers run while the tunnel is down. A setting saved while the station is offline is applied when it returns.

Each receiver has a **local slot** (0–15) that `stationd` keeps for the receiver's life on this station. The slot fixes the receiver's multicast groups on loopback (status `239.77.<slot>.6:5006`, data `239.77.<slot>.4:5004`, TTL 0); the hub and the control plane never see it. `stationd` connects only to the bundled radiod instances. Each release pins one radiod commit: the one the TLV tag table was generated from. It also pins one librtlsdr, the same on every suite, new enough for the RTL-SDR Blog V4 ([packaging.md](packaging.md#the-bundled-librtlsdr)).

A radiod selects its SDR by USB `serial`, which survives reboots and cabling changes, unlike a device index. SDRs that report the same serial can't be told apart, so at most one receiver uses a group of SDRs sharing a serial, and an empty serial means the station's only RTL-SDR and runs only while exactly one is plugged in ([station protocol §4.2](station-protocol.md#42-hardware-and-receivers)).

### The SDR profiles

A profile is chosen **per receiver**, from the web app, and only among those the SDR table offers for that SDR. The table is fixed in the package (`crates/stationd/src/hardware.rs`); `stationd` refuses anything else, so a Blog V4 cannot be given `rtlsdr-hf` (a receiver whose SDR is not plugged in is accepted with any profile of the package, and is `FAILED` when its SDR appears if the table doesn't offer its model and profile for that SDR).

| The SDR reports (`0bda:2838` or `0bda:2832`) | Model offered | Profiles |
|---|---|---|
| product `Blog V4` | RTL-SDR Blog V4 | `rtlsdr-v4` |
| any other product | RTL-SDR Blog V3, or another with direct sampling | `rtlsdr`, `rtlsdr-hf` |
| | Other RTL-SDR | `rtlsdr` |
| any other USB identity | none: not supported | |

For a model with several profiles, the band decides: a centre in HF gets `rtlsdr-hf`, VHF/UHF gets `rtlsdr`.

| Profile | Receiver | Centre frequency |
|---|---|---|
| `rtlsdr` | an RTL-SDR on VHF/UHF, through its tuner | 24 MHz to 1766 MHz; 98 MHz by default |
| `rtlsdr-hf` | an RTL-SDR with direct sampling (Q branch), such as the Blog V3, on HF; the tuner is bypassed | 0.5 MHz to 28.8 MHz; 7.1 MHz by default |
| `rtlsdr-v4` | an RTL-SDR Blog V4 on HF, through its built-in upconverter, and on VHF/UHF; direct sampling receives nothing on a V4 | 0.5 MHz to 1766 MHz; 7.1 MHz by default |

The hardware report lists every profile of the package, each with its centre range, default centre, sample rate and the width of its tunable range, so the control plane can predict a receiver's range before it is applied, even while its SDR is unplugged.

## Hardening

- Every receiver's radiod runs as the same user, `coaxhub-radiod`, from the one template unit, with USB access to the SDRs and `CAP_SYS_NICE`. A udev rule gives the RTL-SDRs to that user's group alone, and the package blacklists the `dvb_usb_rtl28xxu` kernel driver. Its network is limited to loopback and multicast.
- `stationd.service` runs with `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, restricted address families (`AF_UNIX` is kept for the system bus), an empty capability bounding set and write access to `/var/lib/coaxhub` only. It waits for the clock to synchronise, because TLS would fail without it. It never runs as root. It reaches systemd over the system bus, and a polkit rule allows the `coaxhub` user to start, restart and stop the units matching `coaxhub-radiod@[0-9]+.service`, those three verbs only.
- `RestartPreventExitStatus=3` keeps a revoked or unenrolled station from restarting in a loop.

## The `stationd` commands

In a packaged build there is no `--config`. A development build takes `--config` (default `/etc/coaxhub/stationd.toml`) for the settings tests and tooling need ([station protocol §7](station-protocol.md#7-what-the-station-keeps)).

- `run`: serves until SIGTERM or SIGINT; a connected station then sends `Event{SHUTTING_DOWN}`, closes the stream and exits 0.
- `enroll [--token chs_enr_…] [--force]`: generates the key, sends a CSR and the token to the enrollment service and stores the credentials; it refuses an enrolled station without `--force`.
- `status [--json]`: the enrollment, the certificate and the daemon's state, from `status.json`.
- `doctor`: checks the credentials and the endpoints, lists the SDRs it sees and reports each receiver's state from the applied set, so a station without its tunnel still explains itself.
- `unenroll`: deletes the credentials and the assignment, keeps the applied receiver set (`receivers.json`).
- `backend-config --receiver ID [--out PATH|-] [--templates DIR]`: renders that receiver's radiod config from `receivers.json`. The receiver's unit runs it before every start.
- `check-config` (development builds only): validates the config file and the credentials without contacting radiod.

Exit codes: 0 success; 1 a runtime failure, or a failed `doctor` check; 2 invalid credentials (and, in development builds, an invalid config), and usage errors; 3 not enrolled, or the certificate was revoked (`Goodbye{REVOKED}`, or `revoked` from `station-api`), so enroll again; 4 (`enroll` only) the token was refused, malformed or empty, so get a new one. The service unit prevents restarts on code 3; the installer asks for another token on code 4.

Logs go to stderr, coloured only when stderr is a terminal, so the journal gets plain text. `RUST_LOG` sets the filter (default `info`) and `STATIOND_LOG_FORMAT=json` gives one JSON object per line. Key material and tokens are never logged. The owner's view of all this is on the website's [station page](https://coaxhub.com/docs/station).

## What the hub can and cannot do

What reaches the station is **typed requests, checked by fixed rules in the package**. The hub can send the requests of [the station protocol](station-protocol.md#4-requests-and-responses): ask for the hardware report, set the receivers (`ConfigureReceivers`), open, update and close channels on a receiver, subscribe to waterfalls, ask for capabilities, send pings, change a receiver's front end, run a measurement plan and, when they exist, run an admin operation. There is no station-side list of what is allowed. The bounds are the requests' types, which carry enums, numbers and an SDR's serial and no paths or free text, and the rules `stationd` applies to every one of them: the SDR table and the profiles' limits, at most 16 receivers, the capacity claims fixed in the package (channels per receiver, waterfall subscriptions), and no overlapping receivers. A request that breaks a rule is refused (`PERMISSION_DENIED`, `RESOURCE_EXHAUSTED` or `INVALID_ARGUMENT`) and changes nothing.

**What someone who controls the hub, the control plane or the owner's account can do** is to use that interface: retune a receiver, reassign or stop and restart receivers, change a receiver's gain, turn measuring on or off, and (with admin operations) restart the station's services, update the package from the signed repository or reboot. A compromised account can silence or misconfigure its own stations, as it can by deleting them.

The hub cannot:

- **Run a shell or any command.** There is no such request.
- **Write files other than the station's own.** `stationd` writes radiod configs only from the package's templates with values it has checked, and its own state. It takes no configuration text and no paths.
- **Reach the owner's network.** The hub sends nothing to be fetched, forwarded or connected to.
- **Start any other unit.** The polkit rule names `coaxhub-radiod@<number>.service` and three verbs.
- **Use an SDR, model or profile the package doesn't offer** for it. A Blog V4 can't be given a profile that hears nothing, and an SDR the table doesn't know can't be configured.
- **Send a request the station doesn't know.** An unknown command or enum value is answered `UNSUPPORTED`.
- **Read the key.** It never leaves the station.

**Admin operations** are typed: `restart_stationd`, `restart_backend`, `update` and `reboot_host`. They are not implemented yet: whether and how they ship is not decided, and there is no allow-list for them on the station. `stationd` itself never runs as root: an operation that needs privilege would go through a separate helper that allows only those specific invocations. This repository does not yet ship that helper, so these operations are not available on an installed station.

**The owner keeps the last word on the computer.** `stationd` is GPL software they can read, change or stop, and `sudo apt remove coaxhub-stationd` ends it.

**The station is untrusted in the other direction.** The hub validates every message against the limits in [the protocol's validation rules](station-protocol.md#5-hub-side-validation-normative). A violation ends the session and counts a strike. These checks are the same functions in `coaxhub-station-proto` that the station's own tests use.
