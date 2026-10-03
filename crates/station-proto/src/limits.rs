// SPDX-License-Identifier: MIT
//! Every number of the station protocol, as named constants.
//!
//! Section references are to `docs/station-protocol.md` unless noted.
//! A few values are implementation choices where the spec gives a rule but no
//! number; those say so explicitly ("implementation choice").

use std::time::Duration;

// ------------------------------------------------------------------ §2 / §2.1

/// §2.1: time allowed for TLS plus the first `Hello`; after it the hub closes.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// §2.1: time the station has to answer any request except `GetCapabilities`.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// §2.1: time the station has to answer `GetCapabilities`.
pub const GET_CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);

/// §2.1: this many consecutive response timeouts end the session with
/// `Goodbye{PROTOCOL_VIOLATION}`.
pub const MAX_CONSECUTIVE_RESPONSE_TIMEOUTS: u32 = 3;

/// §2.1: with no message at all from the station (including `Pong`) for this
/// long, the hub closes the connection.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// §2.1: HTTP/2 keepalive ping interval.
pub const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// §2.1: HTTP/2 keepalive ping timeout.
pub const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// §2 (sequence diagram): the hub sends `Request{Ping}` this often.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);

/// `Welcome.status_interval_ms` default (gateway.proto): the station sends
/// `StationStatus` this often. Also used when the hub sends 0.
pub const DEFAULT_STATUS_INTERVAL: Duration = Duration::from_millis(10_000);

// ------------------------------------------------------------------ §2.2

/// §2.2: first reconnection backoff cap.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);

/// §2.2: the backoff cap doubles on every consecutive failure.
pub const BACKOFF_FACTOR: u32 = 2;

/// §2.2: the backoff cap never exceeds this.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// §2.2: the backoff resets after the station has been connected this long.
pub const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(5 * 60);

/// §2.2: after `Goodbye{DISABLED}` the station retries this often.
pub const DISABLED_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// §2.2: after `Goodbye{UNSUPPORTED_VERSION}` the station retries this often.
pub const UNSUPPORTED_VERSION_RETRY_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

// ------------------------------------------------------------------ §3

/// §3: protocol majors the hub accepts. The current major, plus the previous
/// one during a migration window of at least 6 months (none yet for v1).
pub const SUPPORTED_MAJORS: &[u32] = &[1];

// ------------------------------------------------------------------ §4

/// §4: the first `request_id` on a connection; ids are strictly increasing.
pub const FIRST_REQUEST_ID: u64 = 1;

/// §4: the hub keeps at most this many outstanding requests per station.
pub const MAX_OUTSTANDING_REQUESTS: usize = 32;

// ------------------------------------------------------------------ §5

/// §5 Size: largest encoded `StationMessage` the hub accepts, in bytes
/// (also the gRPC decoder limit and `Welcome.max_message_bytes`).
pub const MAX_MESSAGE_BYTES: usize = 65_536;

/// §5 Strings: `Result.message` and `Event.detail` (and the hub's own
/// `Goodbye.message`), in bytes of UTF-8.
pub const MAX_DETAIL_BYTES: usize = 256;

/// §5 Strings: `Capabilities.sdr_model`, in bytes.
pub const MAX_SDR_MODEL_BYTES: usize = 64;

/// §5 Strings: at most this many `FrontendControls.antenna_ports` entries.
pub const MAX_ANTENNA_PORTS: usize = 8;

/// §5 Strings: each antenna port label, in bytes. Also applied to
/// `FrontendState.antenna_port` (implementation choice: same label).
pub const MAX_ANTENNA_PORT_BYTES: usize = 16;

/// §5 Strings: version strings (`Hello.daemon_version`,
/// `BackendInfo.version`, `BackendHealth.version`), in bytes.
pub const MAX_VERSION_BYTES: usize = 64;

/// `Hello.platform`, in bytes. Implementation choice: §5 does not list it;
/// it is capped like the version strings it sits next to.
pub const MAX_PLATFORM_BYTES: usize = 64;

/// §5 `AudioFrame`: smallest Opus packet.
pub const MIN_OPUS_PACKET: usize = 1;

/// §5 `AudioFrame`: largest Opus packet (RFC 6716).
pub const MAX_OPUS_PACKET: usize = 1275;

/// §5 `AudioFrame`/`WaterfallRow`: media may arrive at up to this multiple of
/// the nominal rate (1000 / `frame_duration_ms` or 1000 / `row_interval_ms`).
pub const MEDIA_RATE_TOLERANCE: f64 = 1.5;

/// Burst allowance of the media rate limiters, as time at the allowed rate
/// (implementation choice: §5 gives rates, not bursts). One second covers the
/// station's backpressure queue (§6.3 drops waterfall beyond 250 ms and audio
/// beyond 1 s), so a flush after a stall is not penalised.
pub const MEDIA_BURST: Duration = Duration::from_secs(1);

/// §5 `AudioFrame`: over-rate frames are dropped; if the excess is sustained
/// this long it is a violation.
pub const AUDIO_RATE_SUSTAINED: Duration = Duration::from_secs(10);

/// An over-rate episode ends when no message was dropped for this long
/// (implementation choice defining "sustained" for §5's audio and bandwidth
/// rules).
pub const SUSTAINED_GAP: Duration = Duration::from_secs(1);

/// §5 `WaterfallRow`: largest compressed payload, in bytes (16 KiB).
pub const MAX_WATERFALL_COMPRESSED: usize = 16 * 1024;

/// §5 `ChannelSignal`: at most this many per second per channel.
pub const MAX_SIGNALS_PER_SECOND: u32 = 5;

/// §5 `StationStatus`: at most one per `status_interval_ms` divided by this.
pub const STATUS_RATE_DIVISOR: u32 = 2;

/// §5 `Event`: sustained rate, per second.
pub const EVENT_RATE_PER_SECOND: u32 = 10;

/// §5 `Event`: burst.
pub const EVENT_BURST: u32 = 50;

/// §5 `Event` (and media in flight): IDs closed less than this long ago are
/// still "just closed".
pub const ID_CLOSE_GRACE: Duration = Duration::from_secs(5);

/// §5 `Capabilities`: hub cap on `max_channels`.
pub const MAX_CHANNELS: u32 = 64;

/// §5 `Capabilities`: hub cap on `WaterfallLimits.max_bins` (also the largest
/// row the hub ever decompresses).
pub const MAX_BINS: u32 = 16_384;

/// control.proto `SubscribeWaterfall.bins`: fewest bins per row.
pub const MIN_BINS: u32 = 64;

/// Hub cap on `WaterfallLimits.max_subscriptions`. Implementation choice:
/// §5 says "clamp to hub limits" without a number; stationd's default local
/// cap is 4 (§7).
pub const MAX_WATERFALL_SUBSCRIPTIONS: u32 = 16;

/// §5 `Capabilities`: frequency ranges end at or below 6 GHz.
pub const MAX_FREQUENCY_HZ: u64 = 6_000_000_000;

/// §5 Bandwidth: per-station sustained ingress quota, bits per second.
pub const INGRESS_RATE_BITS_PER_SECOND: u64 = 2_000_000;

/// §5 Bandwidth: token-bucket burst, bits.
pub const INGRESS_BURST_BITS: u64 = 4_000_000;

/// §5 Bandwidth: throttling sustained this long ends the session with
/// `Goodbye{QUOTA_EXCEEDED}`.
pub const INGRESS_SUSTAINED: Duration = Duration::from_secs(60);

/// §5: a station with this many strikes...
pub const STRIKES_TO_QUARANTINE: usize = 3;

/// §5: ...within this window is quarantined.
pub const STRIKE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Late responses remembered after a timeout, so they are dropped rather than
/// treated as unsolicited (implementation choice; see
/// [`crate::validate::Session::expire_requests`]).
pub const TIMED_OUT_REQUESTS_REMEMBERED: usize = 64;

// ------------------------------------------------------------------ §4.1, §5.2: measurements (1.1)

/// §3: the first minor with measurements (measure.proto).
pub const MEASUREMENTS_MINOR: u32 = 1;

/// §5.2: hub cap on `MeasurementLimits.max_noise_bands`, and the most bands in
/// a plan or a report.
pub const MAX_NOISE_BANDS: u32 = 32;

/// §5.2: the most ranges in one `NoiseBand`.
pub const MAX_BAND_RANGES: usize = 8;

/// §5.2: `NoiseBand.key`, in bytes of `[a-z0-9]`.
pub const MAX_BAND_KEY_BYTES: usize = 8;

/// §4.1: the report cadence the hub asks for: one report per measured minute.
pub const NOISE_INTERVAL_MS: u32 = 60_000;

/// §4.1: the cadences a station may run (a cycle stretched over 2 or 5 minutes).
pub const NOISE_INTERVALS_MS: &[u32] = &[60_000, 120_000, 300_000];

/// §4.1: the length of a measured window (one UTC minute).
pub const NOISE_WINDOW_MS: u32 = 60_000;

/// §5.2: the lowest and highest quantile a station may report, dB/Hz.
pub const NOISE_DB_HZ: std::ops::RangeInclusive<f32> = -250.0..=50.0;

/// §5.2: the most bins × spectra one band's window may pool.
pub const MAX_NOISE_SAMPLES: u32 = 1_000_000;

/// §5.2: a report's window may start at most this long before the hub's clock...
pub const NOISE_REPORT_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// §5.2: ...and at most this long after it.
pub const NOISE_REPORT_MAX_LEAD: Duration = Duration::from_secs(5);

/// §5.2: a duty change may apply from at most this long before the hub's clock...
pub const DUTY_MAX_AGE: Duration = Duration::from_secs(60);

/// §5.2: ...and at most this long after it.
pub const DUTY_MAX_LEAD: Duration = Duration::from_secs(65);

/// §5.2: noise reports over the rate limit are dropped; sustained this long, a
/// violation.
pub const NOISE_RATE_SUSTAINED: Duration = Duration::from_secs(60);

/// §5.2: at most one `Event{MEASUREMENT_DUTY}` per this long...
pub const DUTY_EVENT_INTERVAL: Duration = Duration::from_secs(60);

/// §5.2: ...with this burst...
pub const DUTY_EVENT_BURST: u32 = 2;

/// §5.2: ...and over the limit for this long, a violation.
pub const DUTY_RATE_SUSTAINED: Duration = Duration::from_secs(10 * 60);

/// §4.1: under idle duty, one UTC minute in this many is measured (hh:00,
/// hh:05, …).
pub const IDLE_PERIOD_MINUTES: i64 = 5;

/// §4.1: under idle duty the measurement channels open this long before a
/// measuring minute (the front end's warm-up: radiod's first spectrum is
/// valid about 70 ms after the open on an RTL-SDR, as measured; the rest
/// is margin)...
pub const IDLE_OPEN_BEFORE: Duration = Duration::from_secs(2);

/// §4.1: ...and close this long after it ends.
pub const IDLE_CLOSE_AFTER: Duration = Duration::from_millis(500);

/// §6.3: measurement reports waiting longer than this are dropped.
pub const QUEUE_MEASUREMENT_DROP_AFTER: Duration = Duration::from_secs(10);

/// §4.1: the measurement spectrum channel's bin count (fixed for its life).
pub const MEASURE_BIN_COUNT: u32 = 1024;

/// §4.1: its resolution bandwidth, Hz: 1024 × 500 Hz is one 512 kHz window.
pub const MEASURE_RESOLUTION_BW_HZ: f32 = 500.0;

/// §4.1: time on each window, ms.
pub const MEASURE_DWELL_MS: u32 = 4_000;

/// §4.1: time between two polls, ms (2 polls/s).
pub const MEASURE_POLL_INTERVAL_MS: u32 = 500;

/// §4.1: the bin counts a station accepts in a plan.
pub const MEASURE_BIN_COUNTS: std::ops::RangeInclusive<u32> = 64..=4_096;

/// §4.1: the resolution bandwidths a station accepts are wider than this, Hz
/// (radiod's narrowband mode, 200 Hz bins or less, is not for measuring)...
pub const MEASURE_NARROWBAND_MAX_RBW_HZ: f32 = 200.0;

/// §4.1: ...and at most this, Hz.
pub const MEASURE_MAX_RESOLUTION_BW_HZ: f32 = 10_000.0;

/// §4.1: whether a station accepts bins of `rbw_hz` for measuring.
pub fn is_measurement_rbw(rbw_hz: f32) -> bool {
    rbw_hz > MEASURE_NARROWBAND_MAX_RBW_HZ && rbw_hz <= MEASURE_MAX_RESOLUTION_BW_HZ
}

/// §4.1: the dwell times a station accepts, ms.
pub const MEASURE_DWELL_MS_RANGE: std::ops::RangeInclusive<u32> = 1_000..=15_000;

/// §4.1: the poll intervals a station accepts, ms (at most 10 polls/s).
pub const MEASURE_POLL_INTERVAL_MS_RANGE: std::ops::RangeInclusive<u32> = 100..=2_000;

/// §4.1: the first measurement method (`method_version`).
pub const MEASURE_METHOD_VERSION: u32 = 1;

// ------------------------------------------------------------------ common.proto AudioParams

/// common.proto `AudioParams.bitrate_bps`: allowed range.
pub const AUDIO_BITRATE_BPS: std::ops::RangeInclusive<u32> = 6_000..=64_000;

/// common.proto `AudioParams.sample_rate_hz`: allowed values.
pub const AUDIO_SAMPLE_RATES_HZ: &[u32] = &[8_000, 12_000, 16_000, 24_000, 48_000];

/// common.proto `AudioParams.frame_duration_ms`: allowed values.
pub const AUDIO_FRAME_DURATIONS_MS: &[u32] = &[10, 20, 40, 60];

// ------------------------------------------------------------------ §6.3

/// §6.3: when the station's outbound queue exceeds this much data it drops
/// the oldest waterfall rows first.
pub const QUEUE_WATERFALL_DROP_AFTER: Duration = Duration::from_millis(250);

/// §6.3: audio is dropped only if the queue still exceeds this.
pub const QUEUE_AUDIO_DROP_AFTER: Duration = Duration::from_secs(1);

// ------------------------------------------------------------------ §8

/// §8: enrollment token prefix.
pub const ENROLLMENT_TOKEN_PREFIX: &str = "chs_enr_";

/// §8: Crockford base32 characters after the prefix (160 bits).
pub const ENROLLMENT_TOKEN_CHARS: usize = 32;

/// §8: an enrollment token is single use and expires after this.
pub const ENROLLMENT_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// §8: station certificate validity.
pub const STATION_CERT_VALIDITY: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// §8: stationd renews once this fraction (numerator) of the certificate
/// lifetime has elapsed...
pub const CERT_RENEW_AT_NUMERATOR: u32 = 2;

/// §8: ...over this denominator (two thirds).
pub const CERT_RENEW_AT_DENOMINATOR: u32 = 3;

/// §8: failed renewals are retried this often.
pub const CERT_RENEW_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// §8: after a revocation-list push the hub cuts live connections within this.
pub const REVOCATION_CUTOFF: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_numbers() {
        assert_eq!(MAX_MESSAGE_BYTES, 65_536);
        assert_eq!(MAX_OUTSTANDING_REQUESTS, 32);
        assert_eq!(IDLE_TIMEOUT, Duration::from_secs(45));
        assert_eq!(PING_INTERVAL, Duration::from_secs(15));
        assert_eq!(MAX_OPUS_PACKET, 1275);
        assert_eq!(MAX_WATERFALL_COMPRESSED, 16_384);
        assert_eq!(STRIKES_TO_QUARANTINE, 3);
        assert_eq!(STRIKE_WINDOW, Duration::from_secs(86_400));
        assert_eq!(INGRESS_BURST_BITS, 2 * INGRESS_RATE_BITS_PER_SECOND);
        assert_eq!(
            BACKOFF_INITIAL * BACKOFF_FACTOR.pow(6),
            Duration::from_secs(64)
        );
        // An uncompressed row of the largest size still fits the payload cap.
        assert_eq!(
            usize::try_from(MAX_BINS).ok(),
            Some(MAX_WATERFALL_COMPRESSED)
        );
    }
}
