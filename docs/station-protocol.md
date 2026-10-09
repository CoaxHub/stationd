<!-- SPDX-License-Identifier: MIT -->
# Station protocol

Protocol `coaxhub.station.v1`, version 1.1. This document is MIT-licensed, like the `.proto` files and the `coaxhub-station-proto` crate.

The wire definitions are in [`proto/coaxhub/station/v1/`](../proto/coaxhub/station/v1/). This document adds what a `.proto` file cannot say: lifecycle, validation, limits and the backend mapping. It is meant for anyone who writes a station or a hub that has to interoperate. The limits in it are the constants of [`limits.rs`](../crates/station-proto/src/limits.rs); the validation rules are implemented in `station_proto::validate`, shared by the hub and the station, so both enforce the same numbers.

Words: a **station** runs `stationd` on the owner's premises. A **hub** is the server a station keeps its tunnel to. The **control plane** is the service that enrolls stations and issues their certificates. A station is untrusted: the hub validates every message it sends.

---

## 1. Roles and transport

| | Station (`stationd`) | Hub gateway | Control plane |
|---|---|---|---|
| gRPC role on the tunnel | client (dials out) | server | — |
| Logical role on the tunnel | answers requests, streams what was requested | **issues requests** | — |
| Trust | untrusted | trusted | trusted |

- **Tunnel.** `StationGateway.Connect` is one bidirectional stream per station, to its home hub. It runs over HTTP/2 with TLS 1.3 on TCP 443, at `gw.<hub>.<domain>`. Both sides require TLS 1.3 and ALPN `h2`. The station refuses a gateway that negotiates anything else.
- **Authentication.** Mutual TLS. The client certificate is issued by the station CA and binds the station ID only (§8). The station verifies the gateway against the CA bundle it received at enrollment, never against public WebPKI or system roots.
- **Control-plane services.**
  - `Enrollment` is at `https://enroll.<domain>`. It uses TLS 1.3 and a one-time token. No client certificate. The station verifies the server against the root certificate built into its release build.
  - `StationControl` is at `https://station-api.<domain>`. It uses mutual TLS with the station certificate. The station verifies the server against the CA bundle received at enrollment. It has two calls: `GetAssignment` and `RenewCertificate`. With `Enroll` on the other endpoint, a station uses three control-plane calls in all.

---

## 2. Session and reconnection

```
stationd                                         hub gateway
   │ TLS 1.3 + mTLS handshake (cert → station_id) │
   │───────────── Connect (HTTP/2 stream) ────────►│  check: cert valid, not revoked,
   │                                               │         station not disabled
   │── Hello{version, daemon, backend, boot_id} ──►│  check: major supported
   │◄──────────── Welcome{version, limits} ────────│  (or Goodbye + close)
   │◄──── Request{1, GetCapabilities} ─────────────│
   │───── Response{1, OK, Capabilities} ──────────►│  hub stores capabilities,
   │                                               │  marks station online
   │◄──── Request{k, StartMeasurements} ───────────│  1.1: a station that measures (§4.1)
   │───── Response{k, OK, MeasurementsStarted} ───►│
   │◄──── Request{n, SubscribeWaterfall …} ────────│  only when listeners need it
   │───── Response{n, OK, WaterfallSubscribed} ───►│
   │───── WaterfallRow* / AudioFrame* ────────────►│
   │───── NoiseFloorReport* (1 per measured minute)►│
   │───── StationStatus (every status_interval) ──►│
   │◄──── Request{m, Ping} … Response{m, Pong} ────│  every 15 s; RTT metric
   │                     …                         │
   │◄──── Goodbye{reason, retry_after_ms} ─────────│  or TCP/stream loss
```

### 2.1 Timeouts

Hub side:

| Condition | Limit | Action |
|---|---|---|
| TLS + first `Hello` | 10 s | close |
| Response to any request | 5 s (`GetCapabilities`: 10 s) | the hub treats the request as failed; 3 consecutive timeouts → `Goodbye{PROTOCOL_VIOLATION}` |
| No message at all (including `Pong`) | 45 s | close; the station reconnects |
| HTTP/2 keepalive ping | every 20 s, 10 s timeout | close |

Station side:

| Condition | Limit | Action |
|---|---|---|
| TCP connect + TLS handshake | 10 s | the attempt failed; back off (§2.2) |
| `Welcome` after sending `Hello` | 15 s (the hub's 10 s `Hello` limit plus margin) | close the stream; back off |
| HTTP/2 keepalive ping (also sent while idle) | every 20 s, 10 s timeout | close; back off |
| Graceful stop (`Event{SHUTTING_DOWN}` sent) | until the hub closes its side, at most 2 s | close |

### 2.2 Reconnection

- On any disconnect the station backs off exponentially: first cap 1 s, factor 2, at most 60 s, with **full jitter** (the delay is uniform in 0 to the cap). The count resets after 5 minutes connected.
- `Goodbye.retry_after_ms` is **added** to the jittered delay.
- `DISABLED` waits `max(5 min, retry_after)`. The owner may re-enable the station, so the station keeps trying.
- `UNSUPPORTED_VERSION` waits `max(6 h, retry_after)` and logs a prominent message asking the owner to update. Neither of these two increases the failure count.
- `Goodbye.refresh_assignment = true`, or the reason `REASSIGNED`: call `StationControl.GetAssignment` before reconnecting.
- `REVOKED`: stop connecting, log an error, report `revoked` in `stationd status` and exit with code 3. The service does not restart on that code. A new enrollment is required.
- **Restarting a hub never requires action on a station.** A station that loses its tunnel reconnects by itself.
- A hub that drains sends `Goodbye{DRAINING}` with a random `retry_after_ms`, so stations don't reconnect all at once.

### 2.3 State after reconnect

All channels, subscriptions and the measurement plan (§4.1) belong to **one connection**. When a connection ends, the station closes them all and stops measuring. It buffers nothing. The hub re-opens whatever its listeners still need and sends a new plan on the new connection. The station is stateless across connections: there is nothing to resynchronise.

---

## 3. Version negotiation

- `ProtocolVersion{major, minor}`. This document describes **1.1**.
- The station sends its version in `Hello`; the hub answers with `Welcome` carrying the version it will speak, or with `Goodbye{UNSUPPORTED_VERSION}`. Both sides use the lower minor.
- **Minor** bumps only add things: optional fields, enum values, `oneof` members. Receivers **must ignore unknown fields** and **treat unknown enum values as unsupported**: answer `UNSUPPORTED`, don't disconnect.
- **Major** bumps may break. A hub supports the current major and, for a migration window of at least 6 months, the previous one. Only major 1 exists.
- **Minor 1** added measurements (`measure.proto`, §4.1): `Capabilities.measurements`, `StartMeasurements` and `StopMeasurements`, `MeasurementsStarted`, `NoiseFloorReport`, the events `MEASUREMENT_STOPPED` and `MEASUREMENT_DUTY` with their fields, and `StationStatus.measurement_reports_dropped_total`. A session negotiated at 1.0 has none of them: the hub sends no plan, a `NoiseFloorReport` is a violation, and the 1.1 fields of `Capabilities` and `Event` are ignored.

---

## 4. Requests and responses

- The hub assigns `request_id`s, strictly increasing from 1. The station answers each exactly once, in any order.
- The hub keeps at most **32 outstanding requests** per station.
- **The hub assigns `channel_id` and `subscription_id`.** The station never invents IDs, so the hub validates media by table lookup.
- `Response.payload` must match the command type. A mismatched or unsolicited `Response` is a protocol violation.

| Command | The station must | Typical errors |
|---|---|---|
| `GetCapabilities` | return its current capabilities | — |
| `OpenChannel` | create a channel with exactly that ID and return the actual parameters | `OUT_OF_RANGE`, `RESOURCE_EXHAUSTED`, `UNSUPPORTED`, `ALREADY_EXISTS` |
| `UpdateChannel` | change the channel in place without interrupting audio (the sequence continues) | `NOT_FOUND`, `OUT_OF_RANGE` |
| `CloseChannel` | stop audio; the ID may be reused afterwards | `NOT_FOUND` (harmless) |
| `SetFrontend` | apply the change and return `FrontendState`; close channels outside a new passband with `Event{CHANNEL_CLOSED}` | `PERMISSION_DENIED` (the owner disabled frontend control), `UNSUPPORTED` |
| `SubscribeWaterfall` | start rows and return the actual parameters | `RESOURCE_EXHAUSTED`, `OUT_OF_RANGE` |
| `UnsubscribeWaterfall` | stop rows | `NOT_FOUND` |
| `Ping` | answer `Pong` with the same nonce | — |
| `AdminOp` | answer `OK` and execute it if the kind is in the local `allowed_admin_ops`; otherwise `PERMISSION_DENIED` | `PERMISSION_DENIED` |
| `StartMeasurements` (1.1) | start the plan and return it as it runs (`MeasurementsStarted`, §4.1) | `PERMISSION_DENIED` (measuring is off locally), `UNSUPPORTED`, `ALREADY_EXISTS` (a plan runs), `OUT_OF_RANGE` (no band inside the tunable range), `INVALID_ARGUMENT` |
| `StopMeasurements` (1.1) | stop the plan; no report for it afterwards | `NOT_FOUND` (harmless) |

The hub's limits are always `min(station capability, owner setting, hub policy)`. A station may advertise more capacity than that, but it never receives more work than the hub decides to give it.

### 4.1 Measurements (1.1)

The code's "the measurement design" is this section: window planning, the reduction to quantiles, the schedule and the pause rules, all of `method_version` 1.

The hub asks a station to measure the noise floor of a few frequency ranges. The station runs the plan on its own clock, on spectrum channels of its own, and **never retunes its front end for it**. Every value is an untrusted claim: uncalibrated, on the backend's scale.

**Capabilities.** A station that can measure reports `Capabilities.measurements`: `enabled` (the owner's `[measurements] enabled`, §7), `max_noise_bands` (the hub clamps it to 32) and `method_version` (1). Without the field, or with `enabled` false, the hub sends no plan.

**The plan.** `StartMeasurements` carries:

- a hub-assigned `measurement_id` (not 0; one plan per connection);
- up to 32 `NoiseBand`s: a hub-assigned `band_id`, 1–8 sorted and disjoint `ranges` inside the station's coverage, and a `key` for logs (`"40m"`);
- the shape of the measurement channel: `bin_count` × `resolution_bw_hz` (1024 × 500 Hz: one 512 kHz window), `dwell_ms` per window (4000) and `poll_interval_ms` (500);
- the report cadence `noise_interval_ms` (60,000).

A station accepts bin counts 64–4096, resolutions over 200 Hz up to 10,000 Hz (radiod's wideband mode), dwells of 1–15 s and polls of 100 ms–2 s (at most 10 per second). Anything else is `INVALID_ARGUMENT`.

**`MeasurementsStarted`** is the plan as the station runs it:

- the accepted subset of the bands: ranges only shrink, because the station cuts them to its own coverage and clips a band wider than one minute of windows;
- the actual cadence: 60,000, or 120,000 or 300,000 when the windows need 2 or 5 measured minutes;
- the duty from the next UTC minute on;
- the `FrontendState` the plan runs under;
- the `method_version`.

**Not listener capacity.** Measurement channels count neither against `max_channels` nor against `max_waterfall_subscriptions`. The station never refuses a listener because of them, and sends no front-end command for measuring.

**Window planning.**

- A **window** is one position of the measurement channel, `bin_count × resolution_bw_hz` wide, centred inside the tunable range.
- Before planning, each band's ranges are cut to the usable part of the tunable range: 5 % off each end (the anti-alias roll-off) and ±2 kHz around the front end's centre (the DC spike).
- A band is covered by the fewest windows that span its accepted ranges.
- Window centres sit on the backend's placement grid (radiod's: a multiple of 1 kHz from the front end's LO with 500 Hz bins). The station plans windows one grid step narrower than they are, so snapping never opens a gap.
- One channel visits the windows in slots of `dwell_ms`, polling every `poll_interval_ms`. A minute has `60 s / dwell` slots (15 with a 4 s dwell). Within a minute, slot `s` goes to window `s mod n` of that minute's `n` windows, so a lone window is polled all minute.
- Bands go into minutes in order and whole: a band never spans two minutes. One cycle visits every window once and takes 1, 2 or 5 measured minutes (`cycle_minutes`, `cycle_index` in the report). Bands that don't fit in 5 are not accepted.
- A hop's own reply is the dwell's first spectrum. Nothing is discarded after an open or a hop. The channel's bin count, resolution and averaging never change while it lives.

**Reduction.** The station reduces each band's minute to four quantiles:

- every bin of every spectrum is normalised to power spectral density in dB/Hz: per-bin power minus 10·log10(bin width), as for the waterfall (§6.2), so a value doesn't move with the resolution;
- only bins whose centre lies inside the band's accepted ranges (`[start, end)`) are pooled; for radiod, bin `i` of `n` is centred at `center + (i − ⌈n/2⌉) · bin width`;
- the pooled values go into one histogram per band and minute: 0.1 dB cells from −250 to +50 dB/Hz, with values outside clamped to the ends;
- the report carries the 10th, 25th, 50th and 90th percentiles, by nearest rank, on the 0.1 dB grid, so p10 ≤ p25 ≤ p50 ≤ p90 always holds. The noise floor is p10; p90 − p10 is a rough activity figure;
- `samples` is bins × spectra pooled, at most 10⁶; `measured` is the range from the lowest to the highest bin edge pooled, inside the band's accepted ranges.

**Duty.**

- **Continuous** while the station has a listener channel or waterfall subscription open (lingering ones included): every UTC minute is measured.
- **Idle** while it has none: only the **measuring minutes**, whose number is a multiple of 5 (hh:00, hh:05, …). The channel opens 2 s before such a minute and closes 0.5 s after it. radiod's front end then runs about 62.5 s in 300 s.
- The station decides the duty at each minute boundary, and it changes at most once a minute. It sends `Event{MEASUREMENT_DUTY}` (`measurement_id`, `measurement_duty`, `effective_unix_ms`: the minute it applies from) when it changes. The hub never sets it.

**Reports.** One `NoiseFloorReport` per measured minute, sent after it:

- `window_start_unix_ms` (the minute, a multiple of 60,000) and `window_ms` (60,000);
- the window's `FrontendState`, the ADC `overloads` and the `ClockState`;
- `partial`: fewer spectra than planned went in;
- the cycle (`cycle_minutes`, `cycle_index`);
- per band (`BandNoise`): the `measured` range, the resolution, `samples` and the four quantiles.

Under idle duty only measuring minutes are reported.

**Pauses.** The station reports nothing (the time is "not measured") while any of these holds at a minute boundary:

- its 1-minute load average is at least 0.75 × its CPU count (an unknown CPU count turns this rule off);
- its outbound queue dropped audio or waterfall since the last check, a minute earlier;
- its backend is degraded.

It resumes after 5 minutes without any of them.

**Stops.** The plan stops on a front-end change (an accepted `SetFrontend`, `Event{FRONTEND_CHANGED}`), on `Event{BACKEND_RESTARTED}`, and when the backend refuses or loses the channel. The station sends `Event{MEASUREMENT_STOPPED}` (`measurement_id`, `measurement_stop_reason`) before the event that caused the stop. The hub reads the capabilities again and sends a new plan. A measurement never straddles a change of settings.

**`ClockState`** says whether the station's clock is synchronised (`stationd` reports false only when systemd-timesyncd runs and hasn't synchronised yet) and its maximum error (0 when unknown). The hub adds its own offset estimate from `Ping` and `Pong`.

---

## 5. Hub-side validation (normative)

Every `StationMessage` goes through these checks before anything else touches it. A **violation** ends the session with `Goodbye{PROTOCOL_VIOLATION}` and records a **strike** against the station.

Three strikes within 24 hours quarantine the station. A quarantined station is refused at connect time and cannot renew its certificate (§8) until its owner enables it again or a moderator clears it. Only strikes after the clearing count toward the next quarantine. The station sees a refused connect as `Goodbye{DISABLED}` (§2.2).

| Check | Rule | On failure |
|---|---|---|
| Size | encoded message ≤ `max_message_bytes` (**65,536**), enforced by the gRPC decoder limit | violation |
| First message | must be `Hello`; `Hello` never again | violation |
| Strings | valid UTF-8; `detail` and `message` ≤ 256 B, `sdr_model` ≤ 64 B, `antenna_ports` ≤ 8 × 16 B, versions ≤ 64 B | violation |
| Enums | known values only for the negotiated minor; `UNSPECIFIED` is invalid where a value is required | violation |
| Response | `request_id` outstanding, payload type matches the command | violation |
| `AudioFrame` | `channel_id` open, payload 1–1275 B, rate ≤ 1.5 × (1000 / `frame_duration_ms`) per second | violation (rate: drop first, violation if sustained 10 s) |
| `WaterfallRow` | `subscription_id` open, compressed ≤ 16 KiB, decompressed **exactly** `bins` bytes (decompression capped at `bins` + 1 bytes: zip-bomb guard), rate ≤ 1.5 × (1000 / `row_interval_ms`) | violation |
| `ChannelSignal` | channel open, ≤ 5/s per channel, floats finite | drop excess; NaN or inf → violation |
| `StationStatus` | ≤ 1 per `status_interval_ms` / 2; floats finite | drop excess |
| `Event` | ≤ 10/s, burst 50; referenced IDs must be open (or just closed within 5 s) | drop excess |
| `Capabilities` | `tunable_range` ⊆ `hardware_range`; `max_channels` ≤ 64; `max_bins` ≤ 16,384; ranges ≤ 6 GHz; `measurements.max_noise_bands` ≤ 32 (1.1) | clamp to hub limits; nonsense ranges → violation |
| `ClockState` | `synchronized` false, `max_error_ms` > 100, or the hub's offset estimate > 250 ms | keep, mark the clock suspect |
| Bandwidth | per-station ingress quota: **2 Mbit/s** sustained, token bucket (burst 4 Mbit) | throttle reads; sustained 60 s → `Goodbye{QUOTA_EXCEEDED}` |

The measurement messages (1.1) have their own rules in §5.2.

### 5.1 Detailed rules

These refine the table above.

- **Hello comes before negotiation.** "Known enum values for the negotiated minor" can't apply to `Hello` itself. Any non-zero `BackendKind` is accepted there; only `UNSPECIFIED` is a violation.
- **String caps not in the table.**
  - `Hello.platform`: 64 B.
  - `FrontendState.antenna_port`: 16 B, like the antenna labels.
- **Non-finite floats are a violation in every message**, including `StationStatus`, `Capabilities.frontend`, `FrontendState` and `Quantization`. "Drop excess" in the table refers to rate only.
- **Media rate limits.**
  - The burst allowance is 1 s at the allowed rate, with a minimum of 2 messages.
  - For `AudioFrame`, "sustained 10 s" means an episode of drops with no gap longer than 1 s.
  - A `WaterfallRow` over the rate is a violation as soon as the burst is spent.
- **Channel and subscription IDs.**
  - An ID opens when the hub **sends** `OpenChannel` or `SubscribeWaterfall`, because media can race the response.
  - A non-`OK` response removes the ID.
  - Waterfall rows that arrive before `WaterfallSubscribed` are dropped: their bin count and compression aren't known yet.
- **Closed IDs.**
  - After the hub closes an ID, media for it is **dropped** for 5 s, then treated as a violation. Events that refer to it are dropped, never a violation.
  - `CHANNEL_CLOSED` without `channel_id`, or `SUBSCRIPTION_CLOSED` without `subscription_id`, is a violation.
  - Those events, and `BACKEND_RESTARTED`, close the IDs with the same 5 s grace.
- **Capabilities, clamp versus violation.**
  - Violation: `start > end`; a range starting above 6 GHz; a tunable range not inside the hardware range; missing ranges; `UNSPECIFIED` entries in `modes`, `codecs` or `allowed_admin_ops`.
  - Clamp: range ends to 6 GHz; `max_channels` to 64; `max_bins` to 16,384; `max_subscriptions` to 16.
- **Responses must echo the request.** These are violations:
  - a different `channel_id`, `subscription_id` or `Pong.nonce`;
  - more bins, or a faster `row_interval_ms`, than requested;
  - a `ChannelOpened` outside the allowed `AudioParams` values (bitrate 6–64 kbit/s; sample rate 8, 12, 16, 24 or 48 kHz; frame 10, 20, 40 or 60 ms), above 6 GHz, or with `low_cut > high_cut`.
- **Late responses** (§2.1).
  - A response to a timed-out request is dropped once. The hub remembers the last 64 timed-out IDs.
  - A second answer to the same request, or an ID the hub never sent, is an unsolicited-response violation.
  - Late responses don't reset the count of consecutive timeouts.
- **Health sub-messages.** A `BackendHealth` or `FrontendHealth` that is present must have a specified state.
- **Ingress quota.** "Sustained 60 s" means throttling continues with no gap longer than 1 s after each throttle period ends.

### 5.2 Measurement messages (1.1)

| Message | Rule | On failure |
|---|---|---|
| `MeasurementsStarted` | echoes `measurement_id`; band IDs ⊆ requested, each range inside a requested range of its band; ranges sorted, apart, 1–8; ≤ 32 bands, keys `[a-z0-9]{1,8}`; `noise_interval_ms` 60,000, 120,000 or 300,000 and ≥ requested; `duty` continuous or idle; `frontend` present | violation |
| Report or event | `measurement_id` open (after a stop: dropped for 5 s, then a violation, as for channels); floats finite | violation |
| `NoiseFloorReport` | ≤ 1 per `noise_interval_ms` / 2, burst 2; ≤ 32 bands, each an accepted ID, no duplicates; `measured` inside the band's accepted ranges; quantiles in −250…50 dB/Hz with p10 ≤ p25 ≤ p50 ≤ p90; `samples` ≤ 10⁶; `window_ms` 1–300,000; `cycle_minutes` 1, 2 or 5 and `cycle_index` below it; `frontend` and `clock` present | violation (rate: drop first, violation if sustained 60 s) |
| `NoiseFloorReport` time | `window_start` a multiple of 60,000, within [hub now − 15 min, hub now + 5 s]; under idle duty, a measuring minute | drop, count |
| `Event{MEASUREMENT_DUTY}` | `measurement_id`, `measurement_duty` (continuous or idle) and `effective_unix_ms` present; ≤ 1 per 60 s, burst 2 | violation (rate: drop first, violation if sustained 10 min) |
| Duty time | `effective_unix_ms` a multiple of 60,000, within [hub now − 60 s, hub now + 65 s] | drop, count |
| `Event{MEASUREMENT_STOPPED}` | `measurement_id` present | violation |

- A plan opens when the hub **sends** `StartMeasurements`. A non-`OK` answer removes it. Reports that arrive before `MeasurementsStarted` are dropped, because the accepted bands aren't known yet.
- `StopMeasurements` and `Event{MEASUREMENT_STOPPED}` close the plan with the 5 s grace of channels. Measurement events for a plan that is neither open nor just closed are violations, unlike other events.
- Structural errors that a correct implementation can't produce are violations. Plausibility problems that an honest station can produce are dropped and counted, and never earn a strike. These are a window or duty change outside the time limits against the hub's wall clock (a clock that drifted, a report delayed in the queue), and, under idle duty, a report for a minute the schedule skips. The duty at a window's start is the latest duty change at or before it, else the plan's initial duty.
- "Sustained" for reports and duty changes means an episode of drops in which each follows the previous within the time a report (`noise_interval_ms` / 2) or a duty change (60 s) is allowed, lasting 60 s or 10 minutes.
- Every measurement message is a few hundred bytes and counts against the ingress quota as usual.

---

## 6. Media

### 6.1 Audio

- One Opus packet per `AudioFrame`, mono. The hub chooses the parameters per channel. Defaults: 12 kHz sample rate for AM, SSB, CW and NFM; 24 kHz for FM; 20 ms frames; a bitrate of 16–32 kbit/s.
- `timestamp` is in 48 kHz units (the Opus clock) whatever the decoded rate, so a receiver can detect gaps and conceal them.
- The hub never decodes audio. It checks framing (size, rate) and forwards it.

### 6.2 Waterfall quantization

- The station computes power spectra and **normalises each bin to power spectral density** (dB/Hz: per-bin power minus 10·log10(bin width)). It maps each bin to `uint8` with the requested `Quantization`, then compresses each row (zstd by default).
- **Why density.** The backend reports power per bin, so without normalising, the noise floor moves with the zoom level. Measured on 40 m with an RTL-SDR: −50 dB with 1.8 kHz bins, −72.5 dB with 6.25 Hz bins, but about −81 dB/Hz for both. With density, one fixed `Quantization` per subscription fits every zoom.
- **Default `Quantization`** (chosen by the hub): `offset_db` −130, `step_db` 0.5. That covers −130 to −2.5 dB/Hz. A value `v` means `offset_db + v × step_db` dB/Hz, on the backend's uncalibrated scale, meant for display.
- **Peak-preserving.** When the station reduces its FFT to fewer `bins`, each output bin is the **maximum** of the FFT bins it covers, so narrow carriers stay visible.
- The hub decompresses (bounded), crops and decimates per listener, and re-encodes for the listener.

### 6.3 Backpressure (station)

The station has one bounded outbound queue with two priority classes: **audio, responses and events** before **waterfall and status**.

"Data older than *t*" means an item has waited in the queue longer than *t*.

- Waterfall rows and signal reports are dropped once older than 250 ms, **oldest first**.
- Audio frames are dropped only once older than 1 s.
- Only the latest `StationStatus` is kept.
- Responses, `Hello` and events are never dropped for age. Events are capped at 256 queued.
- Each droppable class also has a count cap. Drops since `stationd` started are counted in `StationStatus`.

### 6.4 Measurement reports on the wire

- `NoiseFloorReport` is in the waterfall and status class. It is dropped once older than 10 s, and each drop is counted in `StationStatus.measurement_reports_dropped_total`.
- Measurement events are events: never dropped for age.
- A dropped report leaves its minute "not measured". Nothing is resent, and nothing is buffered across a reconnect (§2.3).

---

## 7. `stationd.toml`

The owner controls `/etc/coaxhub/stationd.toml`. The hub cannot change it: there is no config-push operation.

```toml
# What the hub may ask for. Defaults shown.
allow_frontend_control = true       # SetFrontend
allowed_admin_ops = []              # any of: "restart_stationd", "restart_backend", "update", "reboot_host"

# Hard local caps. The hub may ask for less, never more.
max_channels = 16                   # at most 64
max_waterfall_subscriptions = 4     # at most 16

# Measuring the noise floor (1.1, §4.1). Off: Capabilities.measurements.enabled is
# false and every plan is refused with PERMISSION_DENIED.
[measurements]
enabled = true

# The receiver. The installer detects the SDR and sets the profile.
[sdr]
profile = "rtlsdr"                  # rtlsdr (VHF/UHF), rtlsdr-hf (HF by direct sampling)
# center_frequency_hz = 98000000    # optional start frequency; default from the profile
```

- Relative paths resolve against the config file's directory. Unknown keys are errors.
- The file has no backend or endpoint settings. The DSP backend is an internal part of the package. The control-plane hostnames and the root certificate are built into release builds. The gateway comes from the assignment.
- Development builds also accept test-only settings for tests and LAN setups: a `[backend]` section (the fake backend, custom radiod groups), an `[endpoints]` section (`enroll_url`, `station_api_url`, `root_ca`), static `[credentials]` that skip enrollment, `state_dir`, and `[measurements] ignore_host_load`, which keeps measuring from pausing for the host's load. Packaged builds reject all of them.

State lives under `/var/lib/coaxhub/stationd/`, owned by the `coaxhub` system user, mode 0700:

| File | Contents |
|---|---|
| `key.pem` | the private key (0600), generated on the station, never sent |
| `cert.pem`, `chain.pem` | the station certificate and its chain |
| `gateway-ca.pem` | the CA bundle the gateway and `station-api` are verified against |
| `assignment.json` | the last known home hub |
| `station-id` | the station's UUID |
| `next/` | present only while a renewal replaces the credentials |

Files are written atomically (temporary file, `fsync`, rename, directory `fsync`). A renewal replaces `key.pem`, `cert.pem`, `chain.pem` (and `gateway-ca.pem` when it rotated) as **one** swap. The new files are staged in a `.next.tmp-*` directory; renaming it to `next/` is the commit point; then they move over the live files. Readers take each file from `next/` while it exists, so they see one complete set. `stationd run` finishes an interrupted swap at startup.

---

## 8. Identity and certificates

- **Enrollment token.** `chs_enr_` followed by 32 Crockford base32 characters (160 bits). Single use. Valid for 15 minutes. The control plane stores only its hash.
- **Key.** ECDSA P-256, generated on the station, never transmitted. `Enroll` and `RenewCertificate` carry a CSR.
- **Station ID.** The certificate's subject is irrelevant. The station ID is the URI SAN `urn:coaxhub:station:<uuid>`, and the hub reads only that. A certificate with no such SAN, or with more than one, is refused.
- **Validity.** 30 days by default.
- **Renewal.** The station renews with `StationControl.RenewCertificate` (a new key each time) once two thirds of the validity period (`notBefore` to `notAfter`) have elapsed. The control plane refuses a CSR whose key equals the presented certificate's. After a successful renewal the station swaps its credentials (§7) and reconnects the tunnel right away with the new certificate. Listener sessions survive the short gap.
- **Retry.** A failed renewal is retried after 1/24 of the validity period, at least 30 s and at most 1 hour (hourly for 30-day certificates), with jitter. A renewed certificate that could not be stored is renewed again within a minute.
- **Expiry.** The hub ends a tunnel whose client certificate expires while connected (`Goodbye{SHUTDOWN}`). An expired certificate means re-enrollment.
- **Superseded certificates.** One hour after a successful renewal, the control plane revokes the station's other certificates. The delay covers a station whose write of the new certificate failed: it renews again with its old one, which is still valid. A copied key stops working one renewal cycle after the theft.
- **Re-enrollment.** A new token on the same station ID revokes all earlier certificates.
- **`StationControl` refusals.**
  - `PERMISSION_DENIED` with the message `revoked`: the presented certificate is revoked or unknown, or the station no longer exists. `stationd` treats it like `Goodbye{REVOKED}`, except that when the state directory already holds a different, unexpired certificate (a renewal or a forced enrollment just landed), it reconnects with that one.
  - `FAILED_PRECONDITION`: the station is killed or quarantined. It can't renew, so its certificate lapses.
  - A disabled station still gets its assignment and can renew, so re-enabling it never needs a new enrollment. The hub refuses its tunnel meanwhile.
- **Enrollment refusals** are `UNAUTHENTICATED` with a fixed message (`expired`, `used` or invalid) that never echoes the token. `stationd enroll` exits with code 4 on any of them.
- **Revocation.** Certificates are short-lived, and every hub also holds a list of revoked serials. The hub checks it at handshake time and cuts live connections within 5 s of a revocation.
- **Clock.** On every `Welcome` the station records the hub's time minus its own and warns above 30 s. It never refuses anything because of it.

---

## 9. Backends

`stationd` drives its DSP backend through the `Backend` trait of the `station-backend` crate. The first backend is ka9q-radio. `stationd` drives `radiod` as a **separate local process** through its TLV control and status protocol and receives its RTP output. The two are never linked. Control traffic stays on the host: multicast TTL 0, loopback interface.

The trait has one method per request of §4, plus `health`, `events`, and, for 1.1, the measurement-channel methods (open, retune, poll, close), which default to unsupported. IDs come from the hub: `open_channel` must create the channel with exactly the ID it is given.

| Protocol | radiod action |
|---|---|
| `GetCapabilities` | Read the front end's status (sample rate, tuning range, gain and attenuation capabilities) from radiod's status packets and combine it with the local caps. `sdr_model` is radiod's front-end `description`, which the profile's template sets. The HF profile's says "RTL-SDR HF direct sampling", so a service knows the tuner's gain is not in the signal path. |
| `OpenChannel` | Create a dynamic channel (AM `am`, FM `wfm`, NFM `fm`, USB `usb`, LSB `lsb`, CW `cwu`; always mono): a new random SSRC (not 0, not 0xffffffff), with frequency, demodulator preset, filter edges, Opus encoding, bitrate, sample rate and a finite `LIFETIME`. Map the hub's `channel_id` to the SSRC locally, and refresh `LIFETIME` while the channel is open. |
| `UpdateChannel` | Send frequency, preset and filter changes to that SSRC. |
| `CloseChannel` | Send `LIFETIME` 1: radiod removes the channel on the next frame. Frequency 0 only mutes a channel; it does not remove it. |
| `SubscribeWaterfall` | Create a spectrum channel (8-bit log bins, low to high). Its bins sit where radiod really puts them (in wideband mode on its analysis FFT's grid, not exactly at `RADIO_FREQUENCY` and `RESOLUTION_BW`) and tile a range inside the requested one, short of it by less than two bins. That range is `WaterfallSubscribed.range`. Poll the channel at `row_interval_ms`, because radiod sends spectrum status only when polled. Requantize each row from that reply's own `SPECTRUM_BASE` and `SPECTRUM_STEP` (radiod rescales per poll), normalise to dB/Hz with `RESOLUTION_BW` (§6.2), quantize with the requested `Quantization`, then compress in `stationd`. **Changing the bin count means a new SSRC**: changing `BIN_COUNT` on a live channel crashes radiod at the pinned version. |
| `AudioFrame` | Lift the Opus payload from each RTP packet for the SSRC; `seq` and `timestamp` come from RTP, re-based per channel. No DSP in `stationd`. |
| `ChannelSignal` | SNR and power from the unsolicited per-channel status radiod sends on the data group's status port (about 2 Hz), rate-limited to 5 Hz. |
| `SetFrontend` | radiod front-end commands where the hardware driver supports them. At the pinned version the `rtlsdr` and `sig_gen` drivers take only a frequency change, so gain, AGC, attenuation and antenna answer `UNSUPPORTED` and `Capabilities.frontend` advertises no controls. A retune succeeds only if radiod's reported first LO moved. Channels outside the new range close with `CHANNEL_CLOSED`, followed by `FRONTEND_CHANGED`. |
| `StationStatus` | Front-end status (overloads, input power, sample rate) plus host metrics from `/proc` and `/sys`. |
| `StartMeasurements` (1.1) | One spectrum channel of `stationd`'s own per plan (`BIN_COUNT` × `RESOLUTION_BW` from the plan, `BIN_COUNT` fixed for its life), outside listener capacity. A hop sends `RADIO_FREQUENCY` only, in place. Poll at the plan's interval; decode each row with its own `SPECTRUM_BASE` and `SPECTRUM_STEP` and normalise to dB/Hz in `stationd`. Close with `LIFETIME` 1 between idle measuring minutes, so radiod's front end stops when nothing else is open. |

Implementation notes:

- The TLV tag table is **generated** from radiod's `status.h` at the pinned radiod version, not written by hand. `scripts/update-ka9q-headers.sh` refreshes it, and a check fails when the pinned header changes.
- **Squelch off** also sets radiod's squelch open and close thresholds to −100 dB: the `fm` and `wfm` demodulators always squelch and would otherwise send no RTP without a signal. Squelch on uses close = open − 1 dB.
- **WFM** output is always 48 kHz. radiod applies that override only after replying to the creating command, so `stationd` reports 48 kHz in `ChannelOpened` without waiting for it.
- Every command carries `LIFETIME`: a command to an unknown SSRC (even a poll) creates a channel from the template, and the template's lifetime never expires.
- radiod queues at most 2 commands per channel and silently drops the rest, so `stationd` keeps **at most one command in flight per SSRC** and waits for the reply, matched by `COMMAND_TAG`.
- radiod's config is generated from the owner's SDR profile by `stationd backend-config`, which `coaxhub-radiod.service` runs before every start. It renders the package's template for the profile and fixes the multicast groups that `stationd` then uses. `stationd` never changes radiod's config while radiod runs.
- radiod may start after `stationd`. Until it answers, requests get `BACKEND_ERROR` and `StationStatus` reports the backend `DOWN`.

---

## 10. Conformance and fuzzing

- **`coaxhub-station-proto`** (MIT) holds the generated types and the **validation functions shared by hub and station**, so both enforce identical limits. A hub implementation depends on this crate and on nothing else from this repository.
- **Fuzzing** (cargo-fuzz, `scripts/fuzz.sh`) covers:
  - protobuf decoding of `StationMessage` and the hub validator (`station_message`);
  - waterfall decompression (`waterfall_decompress`);
  - the certificate identity parser (`identity`);
  - the TLV status decoder of the ka9q backend (`tlv_status`).

  Every CI run fuzzes each target for 60 s. Longer runs can be scheduled separately.
- **Conformance suite** (`station-conformance`): a scripted fake hub that exercises every command, error path and violation against a real `stationd` with a fake backend. For measurements it checks a plan and its idle-duty reports for measuring minutes only, the duty following a listener channel, the stop on `SetFrontend`, one plan per connection (none after a reconnect), and the local switch. A test station's measuring runs on an accelerated clock, so a minute takes a second; `stationd.toml` has no such setting.
