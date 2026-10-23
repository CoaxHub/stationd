<!-- SPDX-License-Identifier: MIT -->
# Station protocol

Protocol `coaxhub.station.v1`, version 2.0. This document is MIT-licensed, like the `.proto` files and the `coaxhub-station-proto` crate.

The wire definitions are in [`proto/coaxhub/station/v1/`](../proto/coaxhub/station/v1/). This document adds what a `.proto` file cannot say: lifecycle, validation, limits and the backend mapping. It is meant for anyone who writes a station or a hub that has to interoperate. The limits in it are the constants of [`limits.rs`](../crates/station-proto/src/limits.rs); the validation rules are implemented in `station_proto::validate`, shared by the hub and the station, so both enforce the same numbers.

Words: a **station** runs `stationd` on the owner's premises. A **receiver** is one SDR at the station, with its own backend process, passband and front end; a station has one or more, and their tunable ranges never overlap. A **hub** is the server a station keeps its tunnel to. The **control plane** is the service that enrolls stations and issues their certificates. A station is untrusted: the hub validates every message it sends.

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
   │◄──── Request{1, GetHardware} ─────────────────│
   │───── Response{1, OK, Hardware} ──────────────►│  the SDRs plugged in (§4.2)
   │◄──── Request{2, ConfigureReceivers} ──────────│  the owner's receiver set
   │───── Response{2, OK, ReceiversConfigured} ───►│
   │───── Event{RECEIVER_STATE}* ─────────────────►│  starting → running
   │◄──── Request{3, GetCapabilities} ─────────────│
   │───── Response{3, OK, Capabilities} ──────────►│  hub stores the running receivers,
   │                                               │  marks station online
   │◄──── Request{k, StartMeasurements} ───────────│  a station that measures (§4.1)
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
- **The first delay after a dropped session is wider.** When a session that had reached `Welcome` ends with no `Goodbye` (the connection failed or the hub vanished, as when a hub crashes and restarts) and no failure is pending, the first cap is **5 s** instead of 1 s: the delay is uniform in 0 to 5 s. This spreads the reconnects of all the stations of a restarted hub over several seconds instead of its first one. The cap of the next failure is 2 s and the doubling continues as above. A failed attempt that never reached `Welcome`, and any `Goodbye` (with or without `retry_after_ms`), use the base rule.
- `Goodbye.retry_after_ms` is **added** to the jittered delay.
- `DISABLED` waits `max(5 min, retry_after)`. The owner may re-enable the station, so the station keeps trying.
- `UNSUPPORTED_VERSION` waits `max(6 h, retry_after)` and logs a prominent message asking the owner to update. Neither of these two increases the failure count.
- `Goodbye.refresh_assignment = true`, or the reason `REASSIGNED`: call `StationControl.GetAssignment` before reconnecting.
- `REVOKED`: stop connecting, log an error, report `revoked` in `stationd status` and exit with code 3. The service does not restart on that code. A new enrollment is required.
- **Restarting a hub never requires action on a station.** A station that loses its tunnel reconnects by itself.
- A hub that drains sends `Goodbye{DRAINING}` with a random `retry_after_ms`, so stations don't reconnect all at once.
- A hub that has just started may admit new sessions at a limited rate. It refuses a station over the rate with `Goodbye{SHUTDOWN}` (the reason of a hub that cannot take the station now) and a random `retry_after_ms` of 1 to 15 s, and the station follows the rules above.

### 2.3 State after reconnect

All channels, subscriptions and the measurement plan (§4.1) belong to **one connection**. When a connection ends, the station closes them all and stops measuring. It buffers nothing. The hub re-opens whatever its listeners still need and sends a new plan on the new connection.

**The receiver set is the one thing that outlives a connection.** The station keeps the last set it applied (§4.2, §7) and runs it while disconnected and from boot, so its receivers are up before the tunnel is. The hub sends its set again on every connection; when it equals what runs, nothing restarts. Nothing else is resynchronised.

---

## 3. Version negotiation

- `ProtocolVersion{major, minor}`. This document describes **2.1**; the package name stays `coaxhub.station.v1`. Minor 1 added `Capabilities.cw_pitch_hz` (§4.2), an optional field, as minor 1 of major 1 added measurements.
- The station sends its version in `Hello`; the hub answers with `Welcome` carrying the version it will speak, or with `Goodbye{UNSUPPORTED_VERSION}`. Both sides use the lower minor.
- **Minor** bumps only add things: optional fields, enum values (a new SDR model or profile, §4.2), `oneof` members. Receivers of a message **must ignore unknown fields** and **treat unknown enum values as unsupported**: answer `UNSUPPORTED`, don't disconnect.
- **Major** bumps may break. Hubs speak major 2 only and answer any other with `Goodbye{UNSUPPORTED_VERSION}`. Once a major 3 exists, a hub supports major 2 beside it for a migration window of at least 6 months.

---

## 4. Requests and responses

- The hub assigns `request_id`s, strictly increasing from 1. The station answers each exactly once, in any order.
- The hub keeps at most **32 outstanding requests** per station.
- **The hub assigns `channel_id`, `subscription_id`, `measurement_id` and `receiver_id`.** The station never invents IDs, so the hub validates media by table lookup. A receiver ID comes from the control plane, is never 0 and is never reused within the station.
- `Response.payload` must match the command type. A mismatched or unsolicited `Response` is a protocol violation.
- Channels and waterfall subscriptions live on one receiver, named when they open. A channel never moves to another receiver: tuning across receivers is a new channel (the hub's make-before-break).

| Command | The station must | Typical errors |
|---|---|---|
| `GetHardware` | return the SDRs plugged in, with the models and profiles it allows for each (§4.2) | — |
| `ConfigureReceivers` | check the whole set against its rules; if it passes, apply the difference and answer with each receiver's state (§4.2) | `INVALID_ARGUMENT` (the message names the receiver and the rule), `PERMISSION_DENIED` (a model or profile its table doesn't allow for that SDR) |
| `GetCapabilities` | return its current capabilities: the running receivers and the station-wide limits | — |
| `OpenChannel` | create a channel with exactly that ID on that receiver and return the actual parameters | `NOT_FOUND` (receiver not running), `OUT_OF_RANGE`, `RESOURCE_EXHAUSTED`, `UNSUPPORTED`, `ALREADY_EXISTS` |
| `UpdateChannel` | change the channel in place, on its receiver, without interrupting audio (the sequence continues) | `NOT_FOUND`, `OUT_OF_RANGE` |
| `CloseChannel` | stop audio; the ID may be reused afterwards | `NOT_FOUND` (harmless) |
| `SetFrontend` | apply the change to that receiver and return its `FrontendState`; close its channels outside a new passband with `Event{CHANNEL_CLOSED}` | `NOT_FOUND`, `UNSUPPORTED` (a control the receiver doesn't offer live) |
| `SubscribeWaterfall` | start rows on that receiver and return the actual parameters | `NOT_FOUND`, `RESOURCE_EXHAUSTED`, `OUT_OF_RANGE` |
| `UnsubscribeWaterfall` | stop rows | `NOT_FOUND` |
| `Ping` | answer `Pong` with the same nonce | — |
| `AdminOp` | answer `OK` and perform it, or `UNSUPPORTED` for a kind the station doesn't implement | `UNSUPPORTED` |
| `StartMeasurements` | start the plan and return it as it runs (`MeasurementsStarted`, §4.1) | `UNSUPPORTED`, `ALREADY_EXISTS` (a plan runs), `OUT_OF_RANGE` (no band inside its receiver's tunable range), `INVALID_ARGUMENT` |
| `StopMeasurements` | stop the plan; no report for it afterwards | `NOT_FOUND` (harmless) |

The hub's limits are always `min(station capability, owner setting, hub policy)`. A station may advertise more capacity than that, but it never receives more work than the hub decides to give it.

### 4.1 Measurements

The code's "the measurement design" is this section: window planning, the reduction to quantiles, the schedule and the pause rules, all of `method_version` 1.

The hub asks a station to measure the noise floor of a few frequency ranges, each inside one receiver. The station runs the plan on its own clock, on spectrum channels of its own (one per receiver measured), and **never retunes a front end for it**. Every value is an untrusted claim: uncalibrated, on the backend's scale. Whether a station measures at all is the owner's choice, kept by the control plane: the hub sends no plan to a station whose measuring is off.

**Capabilities.** A station that can measure reports `Capabilities.measurements`: `max_noise_bands` (across receivers; the hub clamps it to 32) and `method_version` (1). Without the field the hub sends no plan.

**The plan.** `StartMeasurements` carries:

- a hub-assigned `measurement_id` (not 0; one plan per connection);
- up to 32 `NoiseBand`s: a hub-assigned `band_id`, the `receiver_id` whose coverage holds it, 1–8 sorted and disjoint `ranges` inside that coverage, and a `key` for logs (`"40m"`);
- the shape of the measurement channel: `bin_count` × `resolution_bw_hz` (1024 × 500 Hz: one 512 kHz window), `dwell_ms` per window (4000) and `poll_interval_ms` (500);
- the report cadence `noise_interval_ms` (60,000).

A station accepts bin counts 64–4096, resolutions over 200 Hz up to 10,000 Hz (radiod's wideband mode), dwells of 1–15 s and polls of 100 ms–2 s (at most 10 per second). Anything else is `INVALID_ARGUMENT`.

**`MeasurementsStarted`** is the plan as the station runs it:

- the accepted subset of the bands: ranges only shrink, because the station cuts them to its receiver's coverage and clips a band wider than one minute of windows;
- for each receiver measured (`MeasuredReceiver`), once: its actual cadence (60,000, or 120,000 or 300,000 when its windows need 2 or 5 measured minutes), its duty from the next UTC minute on, and the `FrontendState` it runs under;
- the `method_version`.

**Not listener capacity.** Measurement channels count neither against a receiver's `max_channels` nor against the station's waterfall subscriptions. The station never refuses a listener because of them, and sends no front-end command for measuring.

**Window planning.** Each receiver is planned on its own, with its own channel:

- A **window** is one position of the receiver's measurement channel, `bin_count × resolution_bw_hz` wide, centred inside its tunable range.
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

**Duty**, per receiver, so a receiver nobody listens to keeps its front end stopped most of the time:

- **Continuous** while the receiver has a listener channel or waterfall subscription open (lingering ones included): every UTC minute is measured.
- **Idle** while it has none: only the **measuring minutes**, whose number is a multiple of 5 (hh:00, hh:05, …). The channel opens 2 s before such a minute and closes 0.5 s after it. Its radiod's front end then runs about 62.5 s in 300 s.
- The station decides each receiver's duty at each minute boundary, and it changes at most once a minute per receiver. It sends `Event{MEASUREMENT_DUTY}` (`measurement_id`, `receiver_id`, `measurement_duty`, `effective_unix_ms`: the minute it applies from) when it changes. The hub never sets it.

**Reports.** One `NoiseFloorReport` per receiver per measured minute, sent after it:

- the `receiver_id`;
- `window_start_unix_ms` (the minute, a multiple of 60,000) and `window_ms` (60,000);
- the receiver's `FrontendState` during the window, its ADC `overloads` and the `ClockState`;
- `partial`: fewer spectra than planned went in;
- the receiver's cycle (`cycle_minutes`, `cycle_index`);
- per band of that receiver (`BandNoise`): the `measured` range, the resolution, `samples` and the four quantiles.

Under idle duty only measuring minutes are reported.

**Pauses.** The station reports nothing for any receiver (the time is "not measured") while either of these holds at a minute boundary:

- its 1-minute load average is at least 0.75 × its CPU count (an unknown CPU count turns this rule off);
- its outbound queue dropped audio or waterfall since the last check, a minute earlier.

It reports nothing for one receiver while that receiver's backend is degraded. Each resumes after 5 minutes without its conditions.

**Stops.** The plan stops, for every receiver, on a front-end change (an accepted `SetFrontend`, `Event{FRONTEND_CHANGED}`), on a receiver entering or leaving `RUNNING` (`Event{RECEIVER_STATE}`), on `Event{BACKEND_RESTARTED}`, and when a backend refuses or loses a channel. The station sends `Event{MEASUREMENT_STOPPED}` (`measurement_id`, `measurement_stop_reason`) before the event that caused the stop. The hub reads the capabilities again and sends a new plan. A measurement never straddles a change of settings.

**`ClockState`** says whether the station's clock is synchronised (`stationd` reports false only when systemd-timesyncd runs and hasn't synchronised yet) and its maximum error (0 when unknown). The hub adds its own offset estimate from `Ping` and `Pong`.

### 4.2 Hardware and receivers

The station says what is plugged in and what its package allows for each SDR; the hub says which SDR is which receiver. Configuration crosses the tunnel only as enums, numbers and an SDR's serial. The station renders its backend's configuration from templates of its own and never takes configuration text, paths or commands.

**`GetHardware` → `Hardware`.**

- `sdrs`: every SDR on the station's USB buses (at most 16), in port order: the USB vendor and product IDs, the manufacturer, product and serial strings and the port, all display-only claims; the `models` the owner may pick for it, each with its `profiles`; and the `receiver_id` it serves now, if any. An SDR with no models is one the station doesn't support.
- `profiles`: every profile of the station's package, once, whether or not an SDR plugged in offers it, with what it can do: the centres it accepts, its default centre, its sample rate, the width of its tunable range around the centre (`tunable_span_hz`), and the front-end settings it accepts. Every profile offered above is among them. The hub predicts a receiver's tunable range from these before configuring it, including a receiver whose SDR is not plugged in.
- The models and profiles come from a table in the station's package, keyed by the USB identity. `stationd`'s (`crates/stationd/src/hardware.rs`):

  | The SDR reports | Models offered | Profiles |
  |---|---|---|
  | `0bda:2838` or `0bda:2832`, product `Blog V4` | `RTLSDR_BLOG_V4` | `RTLSDR_V4` |
  | `0bda:2838` or `0bda:2832`, any other product | `RTLSDR_DIRECT_SAMPLING` | `RTLSDR`, `RTLSDR_HF` |
  | | `RTLSDR_OTHER` | `RTLSDR` |
  | anything else | none | |

- `Event{HARDWARE_CHANGED}` tells the hub an SDR was plugged in or removed; the hub calls `GetHardware` again.

**`ConfigureReceivers`** carries the station's whole receiver set: for each receiver its `receiver_id`, `sdr_serial` (the SDR's serial, plugged in or not; empty selects the station's only RTL-SDR), `model`, `profile`, `center_frequency_hz` and optional front-end settings (gain, hardware AGC, attenuation; absent means the profile's default).

- **The hub sends it on every connection**, after `GetHardware` and before `GetCapabilities`, and again whenever the owner changes the set. An empty set stops every receiver.
- **The station checks the whole set** before changing anything. A set that breaks a rule is refused, nothing changes, and `Result.message` names the receiver and the rule:
  - at most 16 receivers, each `receiver_id` not 0 and unique;
  - an empty serial only while the station has at most one RTL-SDR; two receivers never on one serial, and an SDR whose serial another SDR shares is usable by one receiver only;
  - for a receiver whose SDR is plugged in, its model one the table offers for that SDR, and its profile one the table offers for that model (`PERMISSION_DENIED` otherwise);
  - for a receiver whose SDR is not plugged in (a serial the station doesn't report, or an empty serial with no RTL-SDR), a specified model and a profile in `profiles` (`PERMISSION_DENIED` otherwise). The receiver is accepted and is `NO_DEVICE`; when an SDR with its serial appears, the station checks the model and profile against what the table offers for it, and the receiver starts, or is `FAILED` with a `detail` that says the SDR doesn't offer them;
  - each centre inside the profile's centre range, and the settings inside the profile's front-end limits;
  - no two receivers' tunable ranges (centre ± half the profile's span) overlapping.
- **It applies the difference** with what runs: a new receiver starts; a receiver whose serial, model, profile, centre or settings changed restarts; a receiver not in the set stops; an unchanged one is left alone, its channels and subscriptions untouched. It keeps the applied set (§7), so the same receivers run while disconnected and after a reboot.
- **`ReceiversConfigured`** answers an accepted set with each receiver's state now. Each later change comes as `Event{RECEIVER_STATE}` (`receiver_id`, `receiver_state`, and `detail` for `FAILED` and `NO_DEVICE`).
  - `STARTING`: its backend is starting.
  - `RUNNING`: it is in `Capabilities.receivers`, and channels may open on it. The hub reads the capabilities again when a receiver enters or leaves `RUNNING`.
  - `FAILED`: its backend failed to start or stopped, or its SDR doesn't offer its model and profile; `detail` says why, from the backend's own report or the table.
  - `NO_DEVICE`: its SDR is not plugged in, or its serial is empty and more than one RTL-SDR is plugged in. It starts by itself when the SDR is plugged in.
- Stopping or restarting a receiver closes its channels and subscriptions with `Event{BACKEND_RESTARTED}` for it, and stops the measurement plan (§4.1).

**Receivers in the other messages.**

- `Capabilities.receivers` lists the running receivers in frequency order, each with its tunable and hardware ranges, sample rate, channel limit, front-end controls and `sdr_model`. The station-wide fields (`modes`, `codecs`, `waterfall`, `measurements`, `cw_pitch_hz`) apply to all of them; `waterfall.max_subscriptions` counts subscriptions across receivers.
- `Capabilities.cw_pitch_hz` is the audio pitch of a carrier tuned exactly on the frequency in `MODE_CW`: the tone the station's CW demodulator produces. The tone rises with the signal's frequency (upper-sideband CW, as radiod's `cwu` preset): a signal `d` Hz above the tuned frequency is heard at `cw_pitch_hz + d`, which is how a listener's decoder finds the signal and how "tune to the signal" moves the dial. A demodulator that works the other way reports 0. It is station-wide because the CW preset is (every receiver's radiod loads the same presets file), like `modes`. It is a claim for display only: a CW decoder in a listener's browser looks for the tone there. 0 or absent means the station does not report it. The hub keeps a value from 200 to 1200 Hz and drops any other (reads it as 0), never clamps it, and it is no violation. A station whose receivers disagree reports 0.
- `OpenChannel`, `SubscribeWaterfall` and `SetFrontend` name a running receiver; anything else is `NOT_FOUND`. Their answers echo it, and `FrontendState` carries it.
- `SetFrontend` changes only what the receiver's `FrontendControls` offer live. A setting its backend can't change while running is part of the receiver's configuration instead: the hub sends a new `ConfigureReceivers`, which restarts that receiver.
- `StationStatus.receivers` reports every configured receiver: its state, its backend's and front end's health, its channels and its backend's CPU.
- `AdminOp{RESTART_BACKEND}` takes an optional `receiver_id`: one receiver, or all when absent.

---

## 5. Hub-side validation (normative)

Every `StationMessage` goes through these checks before anything else touches it. A **violation** ends the session with `Goodbye{PROTOCOL_VIOLATION}` and records a **strike** against the station.

Three strikes within 24 hours quarantine the station. A quarantined station is refused at connect time and cannot renew its certificate (§8) until its owner enables it again or a moderator clears it. Only strikes after the clearing count toward the next quarantine. The station sees a refused connect as `Goodbye{DISABLED}` (§2.2).

| Check | Rule | On failure |
|---|---|---|
| Size | encoded message ≤ `max_message_bytes` (**65,536**), enforced by the gRPC decoder limit | violation |
| First message | must be `Hello`; `Hello` never again | violation |
| Strings | valid UTF-8; `detail` and `message` ≤ 256 B, `sdr_model` ≤ 64 B, `antenna_ports` ≤ 8 × 16 B, versions ≤ 64 B; `Sdr` manufacturer, product and serial ≤ 128 B, `port` ≤ 32 B | violation |
| Enums | known values only for the negotiated minor; `UNSPECIFIED` is invalid where a value is required | violation |
| Response | `request_id` outstanding, payload type matches the command | violation |
| `AudioFrame` | `channel_id` open, payload 1–1275 B, rate ≤ 1.5 × (1000 / `frame_duration_ms`) per second | violation (rate: drop first, violation if sustained 10 s) |
| `WaterfallRow` | `subscription_id` open, compressed ≤ 16 KiB, decompressed **exactly** `bins` bytes (decompression capped at `bins` + 1 bytes: zip-bomb guard), rate ≤ 1.5 × (1000 / `row_interval_ms`) | violation |
| `ChannelSignal` | channel open, ≤ 5/s per channel, floats finite | drop excess; NaN or inf → violation |
| `StationStatus` | ≤ 1 per `status_interval_ms` / 2; floats finite; ≤ 16 receivers, each configured, once | drop excess; more or unknown receivers → violation |
| `Event` | ≤ 10/s, burst 50; referenced IDs must be open (or just closed within 5 s); `receiver_id` one the hub configured | drop excess |
| `Capabilities` | ≤ 16 receivers, each configured, once; per receiver `tunable_range` ⊆ `hardware_range`, `max_channels` ≤ 64; no two tunable ranges overlapping; `max_bins` ≤ 16,384; ranges ≤ 6 GHz; `measurements.max_noise_bands` ≤ 32; `cw_pitch_hz` 0 or 200–1200 | clamp to hub limits; `cw_pitch_hz` outside its bounds dropped (0); nonsense ranges → violation; overlap: §5.1 |
| `Hardware` | ≤ 16 SDRs; each model and profile a known value; `profiles` lists each profile once, and every profile an SDR offers is among them; `center_range` inside 6 GHz with the default inside it; `receiver_id`, when present, one the hub configured | violation |
| `ReceiversConfigured` | each receiver of the set once, nothing else; a specified state | violation |
| `ClockState` | `synchronized` false, `max_error_ms` > 100, or the hub's offset estimate > 250 ms | keep, mark the clock suspect |
| Bandwidth | per-station ingress quota: **1 Mbit/s plus 1 Mbit/s per running receiver** sustained (2 Mbit/s with one), token bucket (burst twice that) | throttle reads; sustained 60 s → `Goodbye{QUOTA_EXCEEDED}` |

The measurement messages have their own rules in §5.2.

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
  - Violation: `start > end`; a range starting above 6 GHz; a tunable range not inside the hardware range; missing ranges; `UNSPECIFIED` entries in `modes` or `codecs`; a receiver the hub didn't configure, or one twice.
  - Clamp: range ends to 6 GHz; `max_channels` to 64; `max_bins` to 16,384; `max_subscriptions` to 16.
  - Drop: a `cw_pitch_hz` outside 200–1200 Hz is read as 0 (not reported), logged with the clamps.
  - **Receivers beyond the hub's limit** (`[limits] max_receivers`, 6 by default) are ignored, highest IDs first. The hub never configures more, so this only meets a station that kept an older set.
  - **Overlapping receivers** are not a violation (a front end can drift, or settle after a retune): the hub keeps the one with the lower `receiver_id` and treats the other as offline until the overlap ends, and tells the owner why. The limit applies first, then the overlap rule in ascending ID order: a receiver is offline only when it overlaps one the hub keeps, so in a chain where 1 overlaps 2 and 2 overlaps 3 but 1 and 3 don't overlap, 1 and 3 are served.
  - Two ranges **overlap** when they share more than an end: a range that ends where another starts touches it and doesn't overlap it.
- **Receiver IDs.**
  - A receiver ID is configured from the moment the hub **sends** a `ConfigureReceivers` that names it, like channel IDs.
  - The receivers a set leaves out are dropped when the station answers it `OK`, and then get the 5 s grace of closed IDs, with their channels and subscriptions: media and status for them are dropped, events about them are accepted. A refused set changes nothing, since the station keeps running the set it had; a set whose answer times out leaves both its new and its dropped receivers configured.
  - **Before the first accepted set on a connection** the station runs the set it kept (§2.3), which the hub hasn't configured on this connection: `Hardware` and `Capabilities` that name such receivers are accepted, and status and events that name them are dropped, never a violation. Once the station has answered a set `OK`, a receiver the hub didn't configure is a violation in `Hardware`, `Capabilities` and `StationStatus`, and dropped in events.
  - `RECEIVER_STATE`, `DEVICE_LOST`, `DEVICE_RESTORED`, `FRONTEND_OVERLOAD`, `FRONTEND_CHANGED` and `BACKEND_RESTARTED` without `receiver_id` are violations, and so is a `RECEIVER_STATE` without a specified `receiver_state`.
- **Responses must echo the request.** These are violations:
  - a different `channel_id`, `subscription_id`, `receiver_id` or `Pong.nonce`;
  - more bins, or a faster `row_interval_ms`, than requested;
  - a `ChannelOpened` outside the allowed `AudioParams` values (bitrate 6–64 kbit/s; sample rate 8, 12, 16, 24 or 48 kHz; frame 10, 20, 40 or 60 ms), above 6 GHz, or with `low_cut > high_cut`.
- **Late responses** (§2.1).
  - A response to a timed-out request is dropped once. The hub remembers the last 64 timed-out IDs.
  - A second answer to the same request, or an ID the hub never sent, is an unsolicited-response violation.
  - Late responses don't reset the count of consecutive timeouts.
- **Health sub-messages.** A `ReceiverHealth`, `BackendHealth` or `FrontendHealth` that is present must have a specified state.
- **Ingress quota.** "Sustained 60 s" means throttling continues with no gap longer than 1 s after each throttle period ends.

### 5.2 Measurement messages

| Message | Rule | On failure |
|---|---|---|
| `MeasurementsStarted` | echoes `measurement_id`; band IDs ⊆ requested, each with its requested `receiver_id` and each range inside a requested range of its band; ranges sorted, apart, 1–8; ≤ 32 bands, keys `[a-z0-9]{1,8}`; `receivers` exactly the receivers of the accepted bands, once each, each with `noise_interval_ms` 60,000, 120,000 or 300,000 and ≥ requested, `duty` continuous or idle, and `frontend` present for that receiver | violation |
| Report or event | `measurement_id` open (after a stop: dropped for 5 s, then a violation, as for channels); floats finite | violation |
| `NoiseFloorReport` | `receiver_id` one of the plan's; ≤ 1 per that receiver's `noise_interval_ms` / 2, burst 2, per receiver; ≤ 32 bands, each an accepted ID of that receiver, no duplicates; `measured` inside the band's accepted ranges; quantiles in −250…50 dB/Hz with p10 ≤ p25 ≤ p50 ≤ p90; `samples` ≤ 10⁶; `window_ms` 1–300,000; `cycle_minutes` 1, 2 or 5 and `cycle_index` below it; `frontend` (of that receiver) and `clock` present | violation (rate: drop first, violation if sustained 60 s) |
| `NoiseFloorReport` time | `window_start` a multiple of 60,000, within [hub now − 15 min, hub now + 5 s]; under the receiver's idle duty, a measuring minute | drop, count |
| `Event{MEASUREMENT_DUTY}` | `measurement_id`, `receiver_id` (one of the plan's), `measurement_duty` (continuous or idle) and `effective_unix_ms` present; ≤ 1 per 60 s per receiver, burst 2 | violation (rate: drop first, violation if sustained 10 min) |
| Duty time | `effective_unix_ms` a multiple of 60,000, within [hub now − 60 s, hub now + 65 s] | drop, count |
| `Event{MEASUREMENT_STOPPED}` | `measurement_id` present | violation |

- A plan opens when the hub **sends** `StartMeasurements`. A non-`OK` answer removes it. Reports that arrive before `MeasurementsStarted` are dropped, because the accepted bands aren't known yet.
- `StopMeasurements` and `Event{MEASUREMENT_STOPPED}` close the plan with the 5 s grace of channels. Measurement events for a plan that is neither open nor just closed are violations, unlike other events.
- Structural errors that a correct implementation can't produce are violations. Plausibility problems that an honest station can produce are dropped and counted, and never earn a strike. These are a window or duty change outside the time limits against the hub's wall clock (a clock that drifted, a report delayed in the queue), and, under idle duty, a report for a minute the schedule skips. The duty at a window's start is the receiver's latest duty change at or before it, else its initial duty in the plan.
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

## 7. What the station keeps

**A packaged station has no settings file.** Everything the owner sets lives in the control plane and reaches the station only as the typed requests of §4. The station keeps what it needs to start and reach the hub, and the receiver set it last applied:

- the control-plane hostnames and the root certificate, built into release builds; the gateway comes from the assignment;
- the credentials and the state below;
- the fixed rules of its package: the SDR table and the profiles with their templates (§4.2), and its capacity claims (16 channels per receiver, at most 64; 4 waterfall subscriptions per running receiver, at most 16 across the station; 32 noise bands).

**Development builds** read `stationd.toml` (`--config`, default `/etc/coaxhub/stationd.toml`) for test-only settings, for tests and LAN setups: a `[backend]` section (the fake backend with its simulated SDRs, custom radiod groups), an `[endpoints]` section (`enroll_url`, `station_api_url`, `root_ca`), static `[credentials]` that skip enrollment, `state_dir`, and `[measurements] ignore_host_load`, which keeps measuring from pausing for the host's load. Relative paths resolve against the file's directory, and unknown keys are errors. Packaged builds read no file.

**The applied receiver set** is `/var/lib/coaxhub/receivers/receivers.json`, owned by `coaxhub`, group `coaxhub-radiod` (from its set-group-ID directory), mode 0640, so each receiver's backend unit can render its own configuration from it (§9). For each receiver: its ID, its local **slot** (0–15, kept for the receiver's life on this station, which fixes its backend's multicast groups), its serial, model, profile, centre and front-end settings. Written atomically, like the files below.

Credentials and state live under `/var/lib/coaxhub/stationd/`, owned by the `coaxhub` system user, mode 0700:

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

`stationd` drives its DSP backend through the `Backend` trait of the `station-backend` crate. The first backend is ka9q-radio. `stationd` drives one `radiod` **per receiver**, each a **separate local process** (`coaxhub-radiod@<receiver_id>.service`), through its TLV control and status protocol, and receives its RTP output. They are never linked. Control traffic stays on the host: multicast TTL 0, loopback interface.

The trait describes one receiver: it has one method per request of §4 that acts on a receiver, plus `health`, `events`, and the measurement-channel methods (open, retune, poll, close), which default to unsupported. `stationd` holds one backend per running receiver and routes each request by its `receiver_id`. IDs come from the hub: `open_channel` must create the channel with exactly the ID it is given.

**Hardware and receivers** are `stationd`'s own, not the backend's:

- `GetHardware` reads `/sys/bus/usb/devices/*` (`idVendor`, `idProduct`, `manufacturer`, `product`, `serial`, the device name as the port); it needs no device access. The table of §4.2 is `crates/stationd/src/hardware.rs`. A udev-free poll of the same directory every 5 s, or a netlink `KOBJECT_UEVENT` socket, raises `HARDWARE_CHANGED`.
- `ConfigureReceivers` is checked against that table and the profiles (`crates/stationd/src/profile.rs`), then written to `receivers.json` (§7). `stationd` starts, restarts and stops `coaxhub-radiod@<receiver_id>.service` through systemd's D-Bus API (`StartUnit`, `RestartUnit`, `StopUnit`), which a polkit rule in the package allows the `coaxhub` user for units matching `coaxhub-radiod@[0-9]+.service` and those three verbs only.
- Each unit's `ExecStartPre` runs `stationd backend-config --receiver %i`, which renders that receiver's profile template from `receivers.json` into `/run/coaxhub-radiod/%i/radiod.conf`, with the device's serial (`serial =`), the centre and the settings, and the multicast groups of the receiver's slot: status `239.77.<slot>.6:5006`, data `239.77.<slot>.4:5004`. `stationd` never changes a running radiod's configuration file.
- A receiver's state follows its unit and its radiod: `STARTING` from the start until radiod's first status packet, `RUNNING` while radiod answers, `FAILED` when the unit fails (the `detail` from the unit's result and radiod's last log line, at most 256 B), `NO_DEVICE` while its serial is not on the bus (the unit is stopped, and started when the SDR is plugged in). Before starting a unit, `stationd` checks that the table offers the receiver's model and profile for the SDR plugged in, since the hub may have configured the receiver while its SDR was absent; when it doesn't, the receiver is `FAILED` with a `detail` that says so and its unit stays stopped. When the USB devices change, a receiver that can no longer run is stopped, and a `NO_DEVICE` or `FAILED` receiver that can run is started again.
- At boot `stationd` starts every receiver in `receivers.json`, before it connects.

| Protocol | radiod action |
|---|---|
| `GetCapabilities` | For each running receiver, read its front end's status (sample rate, tuning range, gain and attenuation capabilities) from its radiod's status packets and combine it with the package's caps. `sdr_model` is radiod's front-end `description`, which the profile's template sets. The HF profile's (`rtlsdr-hf`) says "RTL-SDR HF direct sampling", so a service knows the tuner's gain is not in the signal path; the RTL-SDR Blog V4's (`rtlsdr-v4`) says "RTL-SDR Blog V4", whose HF goes through its upconverter and tuner, gain included. |
| `OpenChannel` | Create a dynamic channel (AM `am`, FM `wfm`, NFM `fm`, USB `usb`, LSB `lsb`, CW `cwu`; always mono): a new random SSRC (not 0, not 0xffffffff), with frequency, demodulator preset, filter edges, Opus encoding, bitrate, sample rate and a finite `LIFETIME`. Map the hub's `channel_id` to the SSRC locally, and refresh `LIFETIME` while the channel is open. |
| `UpdateChannel` | Send frequency, preset and filter changes to that SSRC. |
| `CloseChannel` | Send `LIFETIME` 1: radiod removes the channel on the next frame. Frequency 0 only mutes a channel; it does not remove it. |
| `SubscribeWaterfall` | Create a spectrum channel (8-bit log bins, low to high). Its bins sit where radiod really puts them (in wideband mode on its analysis FFT's grid, not exactly at `RADIO_FREQUENCY` and `RESOLUTION_BW`) and tile a range inside the requested one, short of it by less than two bins. That range is `WaterfallSubscribed.range`. Poll the channel at `row_interval_ms`, because radiod sends spectrum status only when polled. Requantize each row from that reply's own `SPECTRUM_BASE` and `SPECTRUM_STEP` (radiod rescales per poll), normalise to dB/Hz with `RESOLUTION_BW` (§6.2), quantize with the requested `Quantization`, then compress in `stationd`. **Changing the bin count means a new SSRC**: changing `BIN_COUNT` on a live channel crashes radiod at the pinned version. |
| `AudioFrame` | Lift the Opus payload from each RTP packet for the SSRC; `seq` and `timestamp` come from RTP, re-based per channel. No DSP in `stationd`. |
| `ChannelSignal` | SNR and power from the unsolicited per-channel status radiod sends on the data group's status port (about 2 Hz), rate-limited to 5 Hz. |
| `SetFrontend` | radiod front-end commands to that receiver's radiod, where the hardware driver supports them. At the pinned version the `rtlsdr` and `sig_gen` drivers take only a frequency change, so gain, AGC, attenuation and antenna answer `UNSUPPORTED` and `ReceiverCapabilities.frontend` advertises only `can_retune`; the gain a profile accepts goes in `ConfigureReceivers` and its template instead. A retune succeeds only if radiod's reported first LO moved. The receiver's channels outside the new range close with `CHANNEL_CLOSED`, followed by `FRONTEND_CHANGED`. A retune is also written to `receivers.json`, so a restart keeps it. |
| `StationStatus` | Per receiver, its front-end status (overloads, input power, sample rate) and its radiod's CPU time (from the unit's cgroup, `cpu.stat`), plus host metrics from `/proc` and `/sys`. |
| `StartMeasurements` | One spectrum channel of `stationd`'s own per receiver in the plan (`BIN_COUNT` × `RESOLUTION_BW` from the plan, `BIN_COUNT` fixed for its life), outside listener capacity. A hop sends `RADIO_FREQUENCY` only, in place. Poll at the plan's interval; decode each row with its own `SPECTRUM_BASE` and `SPECTRUM_STEP` and normalise to dB/Hz in `stationd`. Close with `LIFETIME` 1 between idle measuring minutes, so radiod's front end stops when nothing else is open. |

Implementation notes:

- The TLV tag table is **generated** from radiod's `status.h` at the pinned radiod version, not written by hand. `scripts/update-ka9q-headers.sh` refreshes it, and a check fails when the pinned header changes.
- **Squelch off** also sets radiod's squelch open and close thresholds to −100 dB: the `fm` and `wfm` demodulators always squelch and would otherwise send no RTP without a signal. Squelch on uses close = open − 1 dB.
- **WFM** output is always 48 kHz. radiod applies that override only after replying to the creating command, so `stationd` reports 48 kHz in `ChannelOpened` without waiting for it.
- Every command carries `LIFETIME`: a command to an unknown SSRC (even a poll) creates a channel from the template, and the template's lifetime never expires.
- radiod queues at most 2 commands per channel and silently drops the rest, so `stationd` keeps **at most one command in flight per SSRC** and waits for the reply, matched by `COMMAND_TAG`.
- radiod's `rtlsdr` driver selects its device by `serial`, so two receivers can't share one (§4.2). A receiver configured with an empty serial gets no `serial =` line and opens the only RTL-SDR present. It runs only while exactly one RTL-SDR is plugged in, and not when another receiver names that SDR's serial (that receiver has it; the empty-serial one is `FAILED` until the SDRs change).
- A receiver's radiod may start after `stationd`. Until it answers, the receiver is `STARTING`, requests for it get `NOT_FOUND`, and `StationStatus` reports its backend `DOWN`.

---

## 10. Conformance and fuzzing

- **`coaxhub-station-proto`** (MIT) holds the generated types and the **validation functions shared by hub and station**, so both enforce identical limits. A hub implementation depends on this crate and on nothing else from this repository.
- **Fuzzing** (cargo-fuzz, `scripts/fuzz.sh`) covers:
  - protobuf decoding of `StationMessage` and the hub validator (`station_message`);
  - waterfall decompression (`waterfall_decompress`);
  - the certificate identity parser (`identity`);
  - the TLV status decoder of the ka9q backend (`tlv_status`).

  Every CI run fuzzes each target for 60 s. Longer runs can be scheduled separately.
- **Conformance suite** (`station-conformance`): a scripted fake hub that exercises every command, error path and violation against a real `stationd` with a fake backend and simulated SDRs. For receivers it checks the hardware report and its table (a V4 offered only `RTLSDR_V4`, every profile of the package listed), a set applied and its `RECEIVER_STATE` events, the refusals of §4.2 (a shared serial used twice, an empty serial among several RTL-SDRs, a profile the table doesn't offer, overlapping ranges, a centre out of range) with nothing changed, a set naming an SDR that is not plugged in (accepted, the receiver `NO_DEVICE`, then `RUNNING` when the SDR is plugged in), an unchanged set leaving channels open, a removed receiver closing its channels, an SDR unplugged and plugged back (`NO_DEVICE`, `HARDWARE_CHANGED`), and the set surviving a reconnect and a restart. For measurements it checks a plan over two receivers and their idle-duty reports for measuring minutes only, each receiver's duty following its own listener channel, the stop on `SetFrontend`, and one plan per connection (none after a reconnect). A test station's measuring runs on an accelerated clock, so a minute takes a second; the settings file has no such setting.
