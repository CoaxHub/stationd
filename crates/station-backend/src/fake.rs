// SPDX-License-Identifier: GPL-3.0-only
//! [`FakeBackend`]: a deterministic in-memory [`Backend`] of one receiver,
//! for `stationd`'s tests, the conformance suite and development stations
//! (`[backend] kind = "fake"`: one per receiver, with simulated SDRs).
//!
//! It keeps the same bookkeeping a real backend must keep and answers every
//! command with the result codes of station-protocol §4, but produces
//! synthetic media instead of touching a radio:
//!
//! - **Audio:** one fixed Opus "silence" packet per `frame_duration_ms`
//!   (`seq` +1, `timestamp` + 48 × `frame_duration_ms`), plus a
//!   `ChannelSignal` (SNR 12.5 dB, −40 dBFS) about twice per second.
//! - **Waterfall:** one row per confirmed `row_interval_ms`: a flat floor
//!   with a small deterministic ripple and a peak in the middle bin,
//!   compressed as requested (zstd level 1 or none).
//! - **Front end:** gain, hardware AGC and retuning are modelled; retuning
//!   moves the tunable range (center ± sample rate / 2, inside the hardware
//!   range), closes channels that fall outside it with
//!   `Event{CHANNEL_CLOSED}` and sends `Event{FRONTEND_CHANGED}`.
//! - **Measurement channels** (station protocol §4.1): each poll returns a
//!   spectrum of a noise floor around −125 dB/Hz that drifts by ±3 dB over
//!   the UTC day, with deterministic per-bin scatter and a few steady
//!   carriers ([`FAKE_CARRIERS_HZ`]). Opens, retunes and closes are logged
//!   ([`FakeBackend::measurement_log`]) for tests.
//!
//! Timing uses Tokio's clock, so tests can run it under a paused clock. The
//! only wall-clock read is the hour of the noise floor's daily curve, from
//! a replaceable clock ([`FakeBackend::with_wall_clock`]). Nothing here reads
//! a random source.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::StreamExt as _;
use station_proto::{limits, v1};
use tokio::sync::broadcast;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::{
    AudioStream, Backend, BackendCapabilities, BackendError, ChannelId, ChannelOutput, EventStream,
    Health, MeasurementChannel, MeasurementId, RowStream, Spectrum, SubscriptionId,
};

/// The steady carriers in the fake measurement spectra, Hz: FT8 and CW on
/// 40 m and 20 m, a broadcast station on 41 m, and an FM broadcast carrier.
pub const FAKE_CARRIERS_HZ: [u64; 6] = [
    7_040_000, 7_074_000, 7_255_000, 14_074_000, 14_230_000, 98_000_000,
];

/// The fake's measurement placement grid, Hz from the front-end centre (as
/// radiod's with 500 Hz bins).
pub const FAKE_MEASUREMENT_GRID_HZ: u64 = 1_000;

/// The fake noise floor's daily mean, dB/Hz...
const FAKE_FLOOR_DB_HZ: f64 = -125.0;
/// ...and its daily swing, ± dB.
const FAKE_FLOOR_SWING_DB: f64 = 3.0;
/// How far a fake carrier stands above the floor, dB.
const FAKE_CARRIER_DB: f32 = 30.0;

/// What a test sees of the measurement channels ([`FakeBackend::measurement_log`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementOp {
    /// Opened at this centre.
    Open {
        /// The channel.
        id: MeasurementId,
        /// Hz.
        center_hz: u64,
    },
    /// Moved to this centre.
    Retune {
        /// The channel.
        id: MeasurementId,
        /// Hz.
        center_hz: u64,
    },
    /// Closed.
    Close {
        /// The channel.
        id: MeasurementId,
    },
}

#[derive(Debug, Clone, Copy)]
struct FakeMeasurement {
    center_hz: u64,
    bin_count: u32,
    resolution_bw_hz: f32,
    polls: u64,
}

/// The wall clock of the fake noise floor's daily curve: Unix ms.
pub type WallClock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Default quantization when a `SubscribeWaterfall` carries none.
const DEFAULT_QUANTIZATION: v1::Quantization = v1::Quantization {
    offset_db: -130.0,
    step_db: 0.5,
};

/// Waterfall floor value (−80 dB/Hz with the default quantization).
const ROW_FLOOR: u8 = 100;
/// Waterfall peak value in the middle bin.
const ROW_PEAK: u8 = 200;
/// Largest `row_interval_ms` accepted (one row per minute).
const MAX_ROW_INTERVAL_MS: u32 = 60_000;
/// Buffered backend events per subscriber.
const EVENT_BUFFER: usize = 64;
/// The synthetic `ChannelSignal` values.
const SIGNAL_SNR_DB: f32 = 12.5;
const SIGNAL_POWER_DBFS: f32 = -40.0;

/// How a [`FakeBackend`] presents itself.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeConfig {
    /// Reported in `Hello.backend`. The protocol has no "fake" backend kind,
    /// so the default claims `BACKEND_KIND_KA9Q_RADIO` with version
    /// `"fake-<crate version>"` (the hub rejects `UNSPECIFIED`).
    pub info: v1::BackendInfo,
    /// The capabilities. The tunable range is recomputed on retune;
    /// everything else is returned as is.
    pub capabilities: BackendCapabilities,
    /// The initial front-end state.
    pub frontend: v1::FrontendState,
    /// The initial health (see [`FakeBackend::set_health`]).
    pub health: Health,
}

impl Default for FakeConfig {
    /// An RTL-SDR-like receiver tuned to 7.1 MHz at 2.048 MS/s.
    fn default() -> Self {
        Self::receiver(
            7_100_000,
            2_048_000,
            v1::FrequencyRange {
                start_hz: 500_000,
                end_hz: 1_766_000_000,
            },
            "CoaxHub fake SDR",
        )
    }
}

impl FakeConfig {
    /// An RTL-SDR-like receiver centred on `center_hz`, sampling at
    /// `sample_rate_hz` (its passband is the centre ± half of it), which can
    /// be retuned inside `hardware`; gain (0 to 49.6 dB) and hardware AGC are
    /// modelled. `sdr_model` is what its capabilities report.
    pub fn receiver(
        center_hz: u64,
        sample_rate_hz: u32,
        hardware: v1::FrequencyRange,
        sdr_model: &str,
    ) -> Self {
        let center = center_hz;
        let tunable = passband(center, sample_rate_hz, &hardware);
        Self {
            info: v1::BackendInfo {
                kind: v1::BackendKind::Ka9qRadio as i32,
                version: concat!("fake-", env!("CARGO_PKG_VERSION")).to_owned(),
            },
            capabilities: BackendCapabilities {
                receiver: v1::ReceiverCapabilities {
                    receiver_id: 0,
                    tunable_range: Some(tunable),
                    hardware_range: Some(hardware),
                    sample_rate_hz,
                    max_channels: 16,
                    frontend: Some(v1::FrontendControls {
                        has_gain: true,
                        min_gain_db: 0.0,
                        max_gain_db: 49.6,
                        has_hardware_agc: true,
                        has_attenuator: false,
                        max_attenuation_db: 0.0,
                        antenna_ports: Vec::new(),
                        can_retune: true,
                    }),
                    sdr_model: sdr_model.to_owned(),
                },
                modes: [
                    v1::Mode::Am,
                    v1::Mode::Fm,
                    v1::Mode::Nfm,
                    v1::Mode::Usb,
                    v1::Mode::Lsb,
                    v1::Mode::Cw,
                ]
                .into_iter()
                .map(|m| m as i32)
                .collect(),
                codecs: vec![v1::AudioCodec::Opus as i32],
                waterfall: Some(v1::WaterfallLimits {
                    max_bins: 4096,
                    min_row_interval_ms: 50,
                    max_subscriptions: 4,
                }),
            },
            frontend: v1::FrontendState {
                receiver_id: 0,
                gain_db: 20.0,
                hardware_agc: false,
                attenuation_db: 0.0,
                antenna_port: String::new(),
                center_frequency_hz: center,
                tunable_range: Some(tunable),
            },
            health: Health {
                backend: v1::BackendHealth {
                    state: v1::backend_health::State::Running as i32,
                    version: concat!("fake-", env!("CARGO_PKG_VERSION")).to_owned(),
                },
                frontend: Some(v1::FrontendHealth {
                    state: v1::frontend_health::State::Ok as i32,
                    sample_rate_hz,
                    center_frequency_hz: center,
                    gain_db: 20.0,
                    overloads_total: 0,
                    input_power_dbfs: Some(-35.0),
                }),
            },
        }
    }
}

/// `center ± sample_rate / 2`, clipped to `hardware`.
fn passband(center: u64, sample_rate_hz: u32, hardware: &v1::FrequencyRange) -> v1::FrequencyRange {
    let half = u64::from(sample_rate_hz / 2);
    v1::FrequencyRange {
        start_hz: center.saturating_sub(half).max(hardware.start_hz),
        end_hz: center.saturating_add(half).min(hardware.end_hz),
    }
}

fn contains(range: Option<&v1::FrequencyRange>, hz: u64) -> bool {
    range.is_some_and(|r| (r.start_hz..=r.end_hz).contains(&hz))
}

#[derive(Debug)]
struct OpenChannel {
    params: v1::ChannelOpened,
    stop: CancellationToken,
}

#[derive(Debug)]
struct State {
    capabilities: BackendCapabilities,
    frontend: v1::FrontendState,
    health: Health,
    channels: BTreeMap<ChannelId, OpenChannel>,
    subscriptions: BTreeMap<SubscriptionId, CancellationToken>,
    measurements: BTreeMap<MeasurementId, FakeMeasurement>,
    measurement_log: Vec<(Instant, MeasurementOp)>,
    measurement_failure: bool,
}

/// A deterministic in-memory backend; see the [module docs](self).
///
/// Cloning gives another handle to the same backend.
#[derive(Clone)]
pub struct FakeBackend {
    info: v1::BackendInfo,
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<v1::Event>,
    epoch: Instant,
    wall_clock: WallClock,
}

impl std::fmt::Debug for FakeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeBackend")
            .field("info", &self.info)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new(FakeConfig::default())
    }
}

impl FakeBackend {
    /// A backend presenting `config`.
    pub fn new(config: FakeConfig) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Self {
            info: config.info,
            state: Arc::new(Mutex::new(State {
                capabilities: config.capabilities,
                frontend: config.frontend,
                health: config.health,
                channels: BTreeMap::new(),
                subscriptions: BTreeMap::new(),
                measurements: BTreeMap::new(),
                measurement_log: Vec::new(),
                measurement_failure: false,
            })),
            events,
            epoch: Instant::now(),
            wall_clock: Arc::new(system_unix_ms),
        }
    }

    /// Replaces the wall clock of the noise floor's daily curve (Unix ms).
    #[must_use]
    pub fn with_wall_clock(mut self, clock: WallClock) -> Self {
        self.wall_clock = clock;
        self
    }

    /// Every open, retune and close of a measurement channel so far, with
    /// the Tokio instant it happened at.
    pub fn measurement_log(&self) -> Vec<(Instant, MeasurementOp)> {
        self.state().measurement_log.clone()
    }

    /// The open measurement channels, ascending.
    pub fn measurement_ids(&self) -> Vec<MeasurementId> {
        self.state().measurements.keys().copied().collect()
    }

    /// Makes every measurement call fail with `BACKEND_ERROR` (or work again).
    pub fn set_measurement_failure(&self, fail: bool) {
        self.state().measurement_failure = fail;
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // The lock is never held across an await or a panic point; recover
        // from poisoning rather than propagating a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the health reported by [`Backend::health`].
    pub fn set_health(&self, health: Health) {
        self.state().health = health;
    }

    /// Sends `event` to every [`Backend::events`] stream (tests).
    pub fn emit_event(&self, event: v1::Event) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }

    /// The IDs of the open channels, ascending.
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.state().channels.keys().copied().collect()
    }

    /// The IDs of the open subscriptions, ascending.
    pub fn subscription_ids(&self) -> Vec<SubscriptionId> {
        self.state().subscriptions.keys().copied().collect()
    }

    fn check_channel_request(
        state: &State,
        req: &v1::OpenChannel,
    ) -> Result<v1::AudioParams, BackendError> {
        let caps = &state.capabilities;
        if state.channels.contains_key(&req.channel_id) {
            return Err(BackendError::already_exists(format!(
                "channel {} is already open",
                req.channel_id
            )));
        }
        let max_channels = caps.receiver.max_channels;
        if state.channels.len() >= usize::try_from(max_channels).unwrap_or(usize::MAX) {
            return Err(BackendError::resource_exhausted(format!(
                "all {max_channels} channels are in use"
            )));
        }
        check_mode(caps, req.mode)?;
        let audio = req
            .audio
            .ok_or_else(|| BackendError::invalid_argument("audio parameters are missing"))?;
        if !caps.codecs.contains(&audio.codec) {
            return Err(BackendError::unsupported("codec not supported"));
        }
        if !limits::AUDIO_BITRATE_BPS.contains(&audio.bitrate_bps)
            || !limits::AUDIO_SAMPLE_RATES_HZ.contains(&audio.sample_rate_hz)
            || !limits::AUDIO_FRAME_DURATIONS_MS.contains(&audio.frame_duration_ms)
        {
            return Err(BackendError::invalid_argument(
                "bitrate, sample rate or frame duration not allowed",
            ));
        }
        if req.low_cut_hz > req.high_cut_hz {
            return Err(BackendError::invalid_argument(
                "low_cut_hz is above high_cut_hz",
            ));
        }
        if !contains(caps.receiver.tunable_range.as_ref(), req.frequency_hz) {
            return Err(BackendError::out_of_range(
                "frequency outside the tunable range",
            ));
        }
        Ok(audio)
    }

    fn check_subscription_request(
        state: &State,
        req: &v1::SubscribeWaterfall,
    ) -> Result<v1::WaterfallSubscribed, BackendError> {
        let caps = &state.capabilities;
        let Some(limits_) = caps.waterfall else {
            return Err(BackendError::unsupported("no waterfall on this backend"));
        };
        if state.subscriptions.contains_key(&req.subscription_id) {
            return Err(BackendError::already_exists(format!(
                "subscription {} is already open",
                req.subscription_id
            )));
        }
        if state.subscriptions.len()
            >= usize::try_from(limits_.max_subscriptions).unwrap_or(usize::MAX)
        {
            return Err(BackendError::resource_exhausted(format!(
                "all {} waterfall subscriptions are in use",
                limits_.max_subscriptions
            )));
        }
        match v1::WaterfallCompression::try_from(req.compression) {
            Ok(v1::WaterfallCompression::None | v1::WaterfallCompression::Zstd) => {}
            Ok(v1::WaterfallCompression::Unspecified) => {
                return Err(BackendError::invalid_argument("compression is unspecified"));
            }
            Err(_) => return Err(BackendError::unsupported("compression not supported")),
        }
        let range = req
            .range
            .ok_or_else(|| BackendError::invalid_argument("range is missing"))?;
        if range.start_hz >= range.end_hz {
            return Err(BackendError::invalid_argument("range is empty or reversed"));
        }
        let tunable = caps.receiver.tunable_range.as_ref();
        if !contains(tunable, range.start_hz) || !contains(tunable, range.end_hz) {
            return Err(BackendError::out_of_range(
                "range outside the tunable range",
            ));
        }
        if req.bins < limits::MIN_BINS || req.bins > limits_.max_bins {
            return Err(BackendError::out_of_range(format!(
                "bins must be {}..={}",
                limits::MIN_BINS,
                limits_.max_bins
            )));
        }
        if req.row_interval_ms > MAX_ROW_INTERVAL_MS {
            return Err(BackendError::out_of_range(format!(
                "row_interval_ms must be at most {MAX_ROW_INTERVAL_MS}"
            )));
        }
        let quantization = req.quantization.unwrap_or(DEFAULT_QUANTIZATION);
        if !quantization.offset_db.is_finite()
            || !quantization.step_db.is_finite()
            || quantization.step_db <= 0.0
        {
            return Err(BackendError::invalid_argument("invalid quantization"));
        }
        Ok(v1::WaterfallSubscribed {
            subscription_id: req.subscription_id,
            receiver_id: req.receiver_id,
            range: Some(range),
            bins: req.bins,
            row_interval_ms: req.row_interval_ms.max(limits_.min_row_interval_ms).max(1),
            quantization: Some(quantization),
            compression: req.compression,
        })
    }
}

fn check_mode(caps: &BackendCapabilities, mode: i32) -> Result<(), BackendError> {
    if mode == v1::Mode::Unspecified as i32 {
        return Err(BackendError::invalid_argument("mode is unspecified"));
    }
    if !caps.modes.contains(&mode) {
        return Err(BackendError::unsupported("mode not supported"));
    }
    Ok(())
}

#[async_trait::async_trait]
impl Backend for FakeBackend {
    fn info(&self) -> v1::BackendInfo {
        self.info.clone()
    }

    async fn capabilities(&self) -> Result<BackendCapabilities, BackendError> {
        Ok(self.state().capabilities.clone())
    }

    async fn open_channel(
        &self,
        req: v1::OpenChannel,
    ) -> Result<(v1::ChannelOpened, AudioStream), BackendError> {
        let mut state = self.state();
        let audio = Self::check_channel_request(&state, &req)?;
        let opened = v1::ChannelOpened {
            channel_id: req.channel_id,
            receiver_id: req.receiver_id,
            frequency_hz: req.frequency_hz,
            mode: req.mode,
            low_cut_hz: req.low_cut_hz,
            high_cut_hz: req.high_cut_hz,
            audio: Some(audio),
        };
        let stop = CancellationToken::new();
        state.channels.insert(
            req.channel_id,
            OpenChannel {
                params: opened,
                stop: stop.clone(),
            },
        );
        let stream = audio_stream(req.channel_id, audio.frame_duration_ms, stop);
        Ok((opened, stream))
    }

    async fn update_channel(
        &self,
        req: v1::UpdateChannel,
    ) -> Result<v1::ChannelUpdated, BackendError> {
        let mut state = self.state();
        let State {
            capabilities,
            channels,
            ..
        } = &mut *state;
        let Some(channel) = channels.get_mut(&req.channel_id) else {
            return Err(BackendError::not_found(format!(
                "channel {} is not open",
                req.channel_id
            )));
        };
        let mut next = channel.params;
        if let Some(hz) = req.frequency_hz {
            if !contains(capabilities.receiver.tunable_range.as_ref(), hz) {
                return Err(BackendError::out_of_range(
                    "frequency outside the tunable range",
                ));
            }
            next.frequency_hz = hz;
        }
        if let Some(mode) = req.mode {
            check_mode(capabilities, mode)?;
            next.mode = mode;
        }
        next.low_cut_hz = req.low_cut_hz.unwrap_or(next.low_cut_hz);
        next.high_cut_hz = req.high_cut_hz.unwrap_or(next.high_cut_hz);
        if next.low_cut_hz > next.high_cut_hz {
            return Err(BackendError::invalid_argument(
                "low_cut_hz is above high_cut_hz",
            ));
        }
        if req.squelch_snr_db.is_some_and(|s| !s.is_finite()) {
            return Err(BackendError::invalid_argument("squelch is not finite"));
        }
        channel.params = next;
        let p = &channel.params;
        Ok(v1::ChannelUpdated {
            channel_id: p.channel_id,
            frequency_hz: p.frequency_hz,
            mode: p.mode,
            low_cut_hz: p.low_cut_hz,
            high_cut_hz: p.high_cut_hz,
        })
    }

    async fn close_channel(&self, id: ChannelId) -> Result<(), BackendError> {
        match self.state().channels.remove(&id) {
            Some(channel) => {
                channel.stop.cancel();
                Ok(())
            }
            None => Err(BackendError::not_found(format!("channel {id} is not open"))),
        }
    }

    async fn set_frontend(&self, req: v1::SetFrontend) -> Result<v1::FrontendState, BackendError> {
        let mut closed = Vec::new();
        let (reply, retuned) = {
            let mut state = self.state();
            let controls = state
                .capabilities
                .receiver
                .frontend
                .clone()
                .unwrap_or_default();
            let mut next = state.frontend.clone();
            next.receiver_id = req.receiver_id;
            if let Some(gain) = req.gain_db {
                if !controls.has_gain {
                    return Err(BackendError::unsupported("gain is not adjustable"));
                }
                if !gain.is_finite() || gain < controls.min_gain_db || gain > controls.max_gain_db {
                    return Err(BackendError::out_of_range("gain outside the allowed range"));
                }
                next.gain_db = gain;
            }
            if let Some(agc) = req.hardware_agc {
                if !controls.has_hardware_agc {
                    return Err(BackendError::unsupported("no hardware AGC"));
                }
                next.hardware_agc = agc;
            }
            if let Some(att) = req.attenuation_db {
                if !controls.has_attenuator {
                    return Err(BackendError::unsupported("no attenuator"));
                }
                if !att.is_finite() || att < 0.0 || att > controls.max_attenuation_db {
                    return Err(BackendError::out_of_range(
                        "attenuation outside the allowed range",
                    ));
                }
                next.attenuation_db = att;
            }
            if let Some(port) = &req.antenna_port {
                if !controls.antenna_ports.contains(port) {
                    return Err(BackendError::invalid_argument("unknown antenna port"));
                }
                next.antenna_port.clone_from(port);
            }
            let mut retuned = false;
            if let Some(center) = req.center_frequency_hz {
                if !controls.can_retune {
                    return Err(BackendError::unsupported("the front end cannot retune"));
                }
                let receiver = &mut state.capabilities.receiver;
                let hardware = receiver.hardware_range.unwrap_or_default();
                if !contains(Some(&hardware), center) {
                    return Err(BackendError::out_of_range(
                        "center frequency outside the hardware range",
                    ));
                }
                let tunable = passband(center, receiver.sample_rate_hz, &hardware);
                next.center_frequency_hz = center;
                next.tunable_range = Some(tunable);
                receiver.tunable_range = Some(tunable);
                retuned = true;
                state.channels.retain(|id, ch| {
                    let keep = contains(Some(&tunable), ch.params.frequency_hz);
                    if !keep {
                        ch.stop.cancel();
                        closed.push(*id);
                    }
                    keep
                });
            }
            state.frontend = next.clone();
            if let Some(fh) = state.health.frontend.as_mut() {
                fh.center_frequency_hz = next.center_frequency_hz;
                fh.gain_db = next.gain_db;
            }
            (next, retuned)
        };
        for id in closed {
            self.emit_event(v1::Event {
                kind: v1::event::Kind::ChannelClosed as i32,
                channel_id: Some(id),
                subscription_id: None,
                detail: "outside the new passband".to_owned(),
                ..Default::default()
            });
        }
        if retuned {
            self.emit_event(v1::Event {
                kind: v1::event::Kind::FrontendChanged as i32,
                ..Default::default()
            });
        }
        Ok(reply)
    }

    async fn subscribe_waterfall(
        &self,
        req: v1::SubscribeWaterfall,
    ) -> Result<(v1::WaterfallSubscribed, RowStream), BackendError> {
        let mut state = self.state();
        let subscribed = Self::check_subscription_request(&state, &req)?;
        let stop = CancellationToken::new();
        state
            .subscriptions
            .insert(req.subscription_id, stop.clone());
        let stream = row_stream(&subscribed, self.epoch, stop);
        Ok((subscribed, stream))
    }

    async fn unsubscribe_waterfall(&self, id: SubscriptionId) -> Result<(), BackendError> {
        match self.state().subscriptions.remove(&id) {
            Some(stop) => {
                stop.cancel();
                Ok(())
            }
            None => Err(BackendError::not_found(format!(
                "subscription {id} is not open"
            ))),
        }
    }

    fn health(&self) -> Health {
        self.state().health.clone()
    }

    fn events(&self) -> EventStream {
        // A lagging subscriber skips what it missed.
        Box::pin(BroadcastStream::new(self.events.subscribe()).filter_map(|e| async { e.ok() }))
    }

    async fn frontend_state(&self) -> Result<v1::FrontendState, BackendError> {
        Ok(self.state().frontend.clone())
    }

    fn can_measure(&self) -> bool {
        true
    }

    async fn open_measurement(
        &self,
        id: MeasurementId,
        channel: MeasurementChannel,
    ) -> Result<(), BackendError> {
        let mut state = self.state();
        if state.measurement_failure {
            return Err(BackendError::backend("measurement channels are failing"));
        }
        if state.measurements.contains_key(&id) {
            return Err(BackendError::already_exists(format!(
                "measurement {id} is already open"
            )));
        }
        if !limits::MEASURE_BIN_COUNTS.contains(&channel.bin_count)
            || !limits::is_measurement_rbw(channel.resolution_bw_hz)
        {
            return Err(BackendError::invalid_argument(
                "bin count or resolution not allowed",
            ));
        }
        if !contains(
            state.capabilities.receiver.tunable_range.as_ref(),
            channel.center_hz,
        ) {
            return Err(BackendError::out_of_range(
                "centre outside the tunable range",
            ));
        }
        state.measurements.insert(
            id,
            FakeMeasurement {
                center_hz: channel.center_hz,
                bin_count: channel.bin_count,
                resolution_bw_hz: channel.resolution_bw_hz,
                polls: 0,
            },
        );
        let center_hz = channel.center_hz;
        state
            .measurement_log
            .push((Instant::now(), MeasurementOp::Open { id, center_hz }));
        Ok(())
    }

    async fn measurement_grid(&self, _resolution_bw_hz: f32) -> Option<crate::MeasurementGrid> {
        Some(crate::MeasurementGrid {
            anchor_hz: self.state().frontend.center_frequency_hz,
            grid_hz: FAKE_MEASUREMENT_GRID_HZ,
        })
    }

    async fn retune_measurement(
        &self,
        id: MeasurementId,
        center_hz: u64,
    ) -> Result<Option<Spectrum>, BackendError> {
        let mut state = self.state();
        if state.measurement_failure {
            return Err(BackendError::backend("measurement channels are failing"));
        }
        if !contains(
            state.capabilities.receiver.tunable_range.as_ref(),
            center_hz,
        ) {
            return Err(BackendError::out_of_range(
                "centre outside the tunable range",
            ));
        }
        let Some(m) = state.measurements.get_mut(&id) else {
            return Err(BackendError::not_found(format!(
                "measurement {id} is not open"
            )));
        };
        m.center_hz = center_hz;
        m.polls = m.polls.wrapping_add(1);
        let m = *m;
        state
            .measurement_log
            .push((Instant::now(), MeasurementOp::Retune { id, center_hz }));
        drop(state);
        // As radiod's: the move's reply carries a spectrum of the new window.
        Ok(Some(fake_spectrum(id, &m, (self.wall_clock)())))
    }

    async fn poll_measurement(&self, id: MeasurementId) -> Result<Spectrum, BackendError> {
        let m = {
            let mut state = self.state();
            if state.measurement_failure {
                return Err(BackendError::backend("measurement channels are failing"));
            }
            let Some(m) = state.measurements.get_mut(&id) else {
                return Err(BackendError::not_found(format!(
                    "measurement {id} is not open"
                )));
            };
            m.polls = m.polls.wrapping_add(1);
            *m
        };
        Ok(fake_spectrum(id, &m, (self.wall_clock)()))
    }

    async fn close_measurement(&self, id: MeasurementId) -> Result<(), BackendError> {
        let mut state = self.state();
        if state.measurements.remove(&id).is_none() {
            return Err(BackendError::not_found(format!(
                "measurement {id} is not open"
            )));
        }
        state
            .measurement_log
            .push((Instant::now(), MeasurementOp::Close { id }));
        Ok(())
    }
}

/// `SplitMix64`: a deterministic scatter for the fake spectra.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The fake noise floor at `unix_ms`, dB/Hz: lowest before dawn (UTC), a
/// ±3 dB daily curve.
fn fake_floor_db_hz(unix_ms: i64) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "ms of a day fit an f64")]
    let day = unix_ms.rem_euclid(86_400_000) as f64 / 86_400_000.0;
    FAKE_FLOOR_DB_HZ + FAKE_FLOOR_SWING_DB * (std::f64::consts::TAU * (day - 0.375)).sin()
}

/// One fake spectrum: per-bin power (dB, not density) of the floor, a
/// scatter like averaged noise, and the carriers in range.
fn fake_spectrum(id: MeasurementId, m: &FakeMeasurement, unix_ms: i64) -> Spectrum {
    let rbw = f64::from(m.resolution_bw_hz);
    let floor = fake_floor_db_hz(unix_ms) + 10.0 * rbw.log10();
    let half = f64::from(m.bin_count) / 2.0;
    #[allow(clippy::cast_precision_loss, reason = "frequencies below 2^53 Hz")]
    let center = m.center_hz as f64;
    let power_db = (0..m.bin_count)
        .map(|i| {
            let hz = center + (f64::from(i) - half.floor()) * rbw;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "positive"
            )]
            let bin_key = (hz / rbw.max(1.0)).max(0.0) as u64;
            let r = mix(bin_key ^ m.polls.wrapping_mul(0x1000_0000_01B3) ^ u64::from(id) << 48);
            // Uniform in (0, 1), then the log of an exponential (noise power),
            // halved: an averaged spectrum scatters less.
            #[allow(clippy::cast_precision_loss, reason = "53 bits of a random u64")]
            let u = ((r >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
            let scatter = 5.0 * (-u.ln()).log10();
            let carrier = FAKE_CARRIERS_HZ
                .iter()
                .map(|&c| {
                    #[allow(clippy::cast_precision_loss, reason = "frequencies below 2^53 Hz")]
                    let off = ((hz - c as f64) / rbw).abs();
                    if off < 0.5 {
                        f64::from(FAKE_CARRIER_DB)
                    } else if off < 1.5 {
                        f64::from(FAKE_CARRIER_DB) - 10.0
                    } else {
                        0.0
                    }
                })
                .fold(0.0, f64::max);
            #[allow(clippy::cast_possible_truncation, reason = "dB values")]
            let v = (floor + scatter + carrier) as f32;
            v
        })
        .collect();
    Spectrum {
        center_hz: m.center_hz,
        bin_hz: m.resolution_bw_hz,
        power_db,
    }
}

/// An Opus packet of digital silence for `frame_duration_ms` (CELT FB,
/// mono; code 1/3 packing for 40 and 60 ms). Never decoded by the hub.
fn silence_packet(frame_duration_ms: u32) -> Vec<u8> {
    const FRAME: [u8; 2] = [0xFF, 0xFE];
    match frame_duration_ms {
        10 => [&[0xF0][..], &FRAME].concat(),
        40 => [&[0xF9][..], &FRAME, &FRAME].concat(),
        60 => [&[0xFB, 0x03][..], &FRAME, &FRAME, &FRAME].concat(),
        _ => [&[0xF8][..], &FRAME].concat(),
    }
}

fn ticker(period: Duration) -> Interval {
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

struct AudioGen {
    channel_id: ChannelId,
    interval: Interval,
    stop: CancellationToken,
    payload: Vec<u8>,
    seq: u32,
    timestamp: u32,
    step: u32,
    frames: u32,
    signal_every: u32,
    pending_signal: bool,
}

fn audio_stream(
    channel_id: ChannelId,
    frame_duration_ms: u32,
    stop: CancellationToken,
) -> AudioStream {
    let frame_ms = frame_duration_ms.max(1);
    let state = AudioGen {
        channel_id,
        interval: ticker(Duration::from_millis(u64::from(frame_ms))),
        stop,
        payload: silence_packet(frame_ms),
        seq: 0,
        timestamp: 0,
        step: 48u32.saturating_mul(frame_ms),
        frames: 0,
        signal_every: (500 / frame_ms).max(1),
        pending_signal: false,
    };
    Box::pin(futures_util::stream::unfold(state, |mut st| async move {
        if st.pending_signal {
            st.pending_signal = false;
            let signal = v1::ChannelSignal {
                channel_id: st.channel_id,
                snr_db: SIGNAL_SNR_DB,
                power_dbfs: SIGNAL_POWER_DBFS,
                squelch_open: None,
            };
            return Some((ChannelOutput::Signal(signal), st));
        }
        tokio::select! {
            biased;
            () = st.stop.cancelled() => None,
            _ = st.interval.tick() => {
                let frame = v1::AudioFrame {
                    channel_id: st.channel_id,
                    seq: st.seq,
                    timestamp: st.timestamp,
                    payload: st.payload.clone(),
                };
                st.seq = st.seq.wrapping_add(1);
                st.timestamp = st.timestamp.wrapping_add(st.step);
                st.frames = st.frames.wrapping_add(1);
                st.pending_signal = st.frames % st.signal_every == 0;
                Some((ChannelOutput::Audio(frame), st))
            }
        }
    }))
}

/// The synthetic row `seq` of `bins` values.
fn synthetic_row(bins: usize, seq: u32) -> Vec<u8> {
    (0..bins)
        .map(|i| {
            if i == bins / 2 {
                ROW_PEAK
            } else {
                // Deterministic ripple of 0..=4 above the floor.
                let ripple = (i as u64)
                    .wrapping_mul(7)
                    .wrapping_add(u64::from(seq).wrapping_mul(13))
                    % 5;
                ROW_FLOOR + u8::try_from(ripple).unwrap_or(0)
            }
        })
        .collect()
}

struct RowGen {
    subscription_id: SubscriptionId,
    interval: Interval,
    stop: CancellationToken,
    bins: usize,
    zstd: bool,
    seq: u32,
    epoch: Instant,
}

fn row_stream(sub: &v1::WaterfallSubscribed, epoch: Instant, stop: CancellationToken) -> RowStream {
    let state = RowGen {
        subscription_id: sub.subscription_id,
        interval: ticker(Duration::from_millis(u64::from(sub.row_interval_ms.max(1)))),
        stop,
        bins: usize::try_from(sub.bins).unwrap_or(0),
        zstd: sub.compression == v1::WaterfallCompression::Zstd as i32,
        seq: 0,
        epoch,
    };
    Box::pin(futures_util::stream::unfold(state, |mut st| async move {
        tokio::select! {
            biased;
            () = st.stop.cancelled() => None,
            _ = st.interval.tick() => {
                let row = synthetic_row(st.bins, st.seq);
                // Compressing a few KiB in memory cannot fail in practice; if
                // it ever does, end the stream rather than send a bad row.
                let payload = if st.zstd {
                    zstd::bulk::compress(&row, 1).ok()?
                } else {
                    row
                };
                let item = v1::WaterfallRow {
                    subscription_id: st.subscription_id,
                    seq: st.seq,
                    timestamp_us: u64::try_from(st.epoch.elapsed().as_micros()).unwrap_or(u64::MAX),
                    payload,
                };
                st.seq = st.seq.wrapping_add(1);
                Some((item, st))
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use station_proto::validate::{WaterfallDecoder, sanitize_capabilities};

    fn opus(frame_duration_ms: u32) -> v1::AudioParams {
        v1::AudioParams {
            codec: v1::AudioCodec::Opus as i32,
            bitrate_bps: 16_000,
            sample_rate_hz: 12_000,
            frame_duration_ms,
        }
    }

    fn open_req(channel_id: u32, frequency_hz: u64) -> v1::OpenChannel {
        v1::OpenChannel {
            channel_id,
            receiver_id: 3,
            frequency_hz,
            mode: v1::Mode::Lsb as i32,
            low_cut_hz: -2800,
            high_cut_hz: -200,
            audio: Some(opus(20)),
            squelch_snr_db: None,
        }
    }

    fn sub_req(
        subscription_id: u32,
        compression: v1::WaterfallCompression,
    ) -> v1::SubscribeWaterfall {
        v1::SubscribeWaterfall {
            subscription_id,
            receiver_id: 3,
            range: Some(v1::FrequencyRange {
                start_hz: 7_000_000,
                end_hz: 7_200_000,
            }),
            bins: 256,
            row_interval_ms: 100,
            quantization: None,
            compression: compression as i32,
        }
    }

    fn code<T>(r: Result<T, BackendError>) -> v1::ResultCode {
        match r {
            Ok(_) => v1::ResultCode::Ok,
            Err(e) => e.code,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn measurement_channels() {
        // 06:00 UTC: the bottom of the daily curve.
        let b = FakeBackend::default().with_wall_clock(Arc::new(|| 6 * 3_600_000));
        assert!(b.can_measure());
        let ch = MeasurementChannel {
            center_hz: 7_150_000,
            bin_count: 1024,
            resolution_bw_hz: 500.0,
            poll_interval: Duration::from_millis(500),
        };
        b.open_measurement(1, ch).await.expect("open");
        assert_eq!(
            code(b.open_measurement(1, ch).await),
            v1::ResultCode::AlreadyExists
        );
        let far = MeasurementChannel {
            center_hz: 14_000_000,
            ..ch
        };
        assert_eq!(
            code(b.open_measurement(2, far).await),
            v1::ResultCode::OutOfRange
        );
        let s = b.poll_measurement(1).await.expect("poll");
        assert_eq!(s.power_db.len(), 1024);
        assert_eq!(s.center_hz, 7_150_000);
        // The floor in density: −125 − 3 dB/Hz, plus 27 dB for 500 Hz bins.
        let mut sorted = s.power_db.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[512] - 10.0 * 500f32.log10();
        assert!((-131.0..-125.0).contains(&median), "{median}");
        // The 7.074 MHz carrier stands out (bin i is centred at 7.15 MHz + (i − 512) · 500 Hz).
        let bin = usize::try_from((7_074_000 - (7_150_000 - 256_000)) / 500).expect("bin");
        assert!(s.power_db[bin] > sorted[1000], "carrier");
        // Each poll scatters differently, deterministically.
        let s2 = b.poll_measurement(1).await.expect("poll");
        assert_ne!(s.power_db, s2.power_db);
        let moved = b.retune_measurement(1, 7_000_000).await.expect("retune");
        assert_eq!(
            moved.map(|s| s.center_hz),
            Some(7_000_000),
            "the reply's spectrum"
        );
        assert_eq!(
            b.measurement_grid(500.0).await,
            Some(crate::MeasurementGrid {
                anchor_hz: 7_100_000,
                grid_hz: 1_000
            })
        );
        assert_eq!(
            b.poll_measurement(1).await.expect("poll").center_hz,
            7_000_000
        );
        b.set_measurement_failure(true);
        assert_eq!(
            code(b.poll_measurement(1).await),
            v1::ResultCode::BackendError
        );
        b.set_measurement_failure(false);
        b.close_measurement(1).await.expect("close");
        assert_eq!(code(b.close_measurement(1).await), v1::ResultCode::NotFound);
        assert!(b.measurement_ids().is_empty());
        let ops: Vec<MeasurementOp> = b.measurement_log().into_iter().map(|(_, op)| op).collect();
        assert_eq!(
            ops,
            [
                MeasurementOp::Open {
                    id: 1,
                    center_hz: 7_150_000
                },
                MeasurementOp::Retune {
                    id: 1,
                    center_hz: 7_000_000
                },
                MeasurementOp::Close { id: 1 },
            ]
        );
        assert_eq!(
            b.frontend_state()
                .await
                .expect("frontend")
                .center_frequency_hz,
            7_100_000
        );
        // The trait's default: a backend that cannot measure.
        assert!(fake_floor_db_hz(18 * 3_600_000) > fake_floor_db_hz(6 * 3_600_000));
    }

    #[tokio::test]
    async fn default_capabilities_pass_hub_validation() {
        let b = FakeBackend::default();
        let caps = b.capabilities().await.expect("caps");
        // As one receiver of a station's capabilities.
        let station = v1::Capabilities {
            receivers: vec![v1::ReceiverCapabilities {
                receiver_id: 1,
                ..caps.receiver.clone()
            }],
            modes: caps.modes.clone(),
            codecs: caps.codecs.clone(),
            waterfall: caps.waterfall,
            measurements: None,
        };
        let sanitized = sanitize_capabilities(&station, 0).expect("valid capabilities");
        assert!(sanitized.clamped.is_empty());
        assert_eq!(
            caps.receiver.tunable_range,
            Some(v1::FrequencyRange {
                start_hz: 6_076_000,
                end_hz: 8_124_000
            })
        );
        assert_eq!(b.info().kind, v1::BackendKind::Ka9qRadio as i32);
        assert!(b.info().version.starts_with("fake-"));
        let h = b.health();
        assert_eq!(h.backend.state, v1::backend_health::State::Running as i32);
    }

    #[tokio::test(start_paused = true)]
    async fn audio_is_paced_and_ends_on_close() {
        let b = FakeBackend::default();
        let (opened, mut audio) = b.open_channel(open_req(3, 7_100_000)).await.expect("open");
        assert_eq!(opened.channel_id, 3);
        assert_eq!(opened.audio, Some(opus(20)));
        assert_eq!(b.channel_ids(), vec![3]);
        let start = Instant::now();
        let mut frames = Vec::new();
        let mut signals = 0;
        while frames.len() < 50 {
            match audio.next().await.expect("item") {
                ChannelOutput::Audio(f) => frames.push(f),
                ChannelOutput::Signal(s) => {
                    assert_eq!(s.channel_id, 3);
                    assert!(s.snr_db.is_finite() && s.power_dbfs.is_finite());
                    signals += 1;
                }
            }
        }
        // 50 frames of 20 ms = 1 s; about two signals per second.
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert_eq!(
            signals, 1,
            "signal after frame 25; the next comes after frame 50"
        );
        for (i, f) in frames.iter().enumerate() {
            let i = u32::try_from(i).expect("small");
            assert_eq!(f.channel_id, 3);
            assert_eq!(f.seq, i);
            assert_eq!(f.timestamp, i * 960);
            assert_eq!(f.payload, vec![0xF8, 0xFF, 0xFE]);
        }
        b.close_channel(3).await.expect("close");
        // The pending signal (after frame 50) may still come out; then it ends.
        while let Some(item) = audio.next().await {
            assert!(matches!(item, ChannelOutput::Signal(_)));
        }
        assert!(b.channel_ids().is_empty());
        assert_eq!(code(b.close_channel(3).await), v1::ResultCode::NotFound);
    }

    #[test]
    fn silence_packets_fit_opus_limits() {
        for ms in [10, 20, 40, 60] {
            let p = silence_packet(ms);
            assert!((limits::MIN_OPUS_PACKET..=limits::MAX_OPUS_PACKET).contains(&p.len()));
        }
        assert_eq!(silence_packet(40).len(), 5);
        assert_eq!(silence_packet(60)[1], 3);
    }

    #[tokio::test]
    async fn open_channel_errors() {
        let b = FakeBackend::default();
        let _stream = b.open_channel(open_req(1, 7_100_000)).await.expect("open");
        assert_eq!(
            code(b.open_channel(open_req(1, 7_100_000)).await),
            v1::ResultCode::AlreadyExists
        );
        assert_eq!(
            code(b.open_channel(open_req(2, 100_000_000)).await),
            v1::ResultCode::OutOfRange
        );
        let mut r = open_req(2, 7_100_000);
        r.mode = v1::Mode::Unspecified as i32;
        assert_eq!(
            code(b.open_channel(r).await),
            v1::ResultCode::InvalidArgument
        );
        let mut r = open_req(2, 7_100_000);
        r.mode = 99;
        assert_eq!(code(b.open_channel(r).await), v1::ResultCode::Unsupported);
        let mut r = open_req(2, 7_100_000);
        r.audio = None;
        assert_eq!(
            code(b.open_channel(r).await),
            v1::ResultCode::InvalidArgument
        );
        let mut r = open_req(2, 7_100_000);
        r.audio = Some(v1::AudioParams {
            codec: 7,
            ..opus(20)
        });
        assert_eq!(code(b.open_channel(r).await), v1::ResultCode::Unsupported);
        let mut r = open_req(2, 7_100_000);
        r.audio = Some(opus(25));
        assert_eq!(
            code(b.open_channel(r).await),
            v1::ResultCode::InvalidArgument
        );
        let mut r = open_req(2, 7_100_000);
        r.low_cut_hz = 100;
        r.high_cut_hz = -100;
        assert_eq!(
            code(b.open_channel(r).await),
            v1::ResultCode::InvalidArgument
        );
    }

    #[tokio::test]
    async fn channel_capacity_is_enforced() {
        let mut config = FakeConfig::default();
        config.capabilities.receiver.max_channels = 2;
        let b = FakeBackend::new(config);
        let _a = b.open_channel(open_req(1, 7_100_000)).await.expect("1");
        let _c = b.open_channel(open_req(2, 7_100_000)).await.expect("2");
        assert_eq!(
            code(b.open_channel(open_req(3, 7_100_000)).await),
            v1::ResultCode::ResourceExhausted
        );
    }

    #[tokio::test]
    async fn update_channel_changes_in_place() {
        let b = FakeBackend::default();
        let _s = b.open_channel(open_req(5, 7_100_000)).await.expect("open");
        let updated = b
            .update_channel(v1::UpdateChannel {
                channel_id: 5,
                frequency_hz: Some(7_150_000),
                mode: Some(v1::Mode::Usb as i32),
                low_cut_hz: Some(200),
                high_cut_hz: Some(2800),
                squelch_snr_db: Some(6.0),
                squelch_off: false,
            })
            .await
            .expect("update");
        assert_eq!(
            updated,
            v1::ChannelUpdated {
                channel_id: 5,
                frequency_hz: 7_150_000,
                mode: v1::Mode::Usb as i32,
                low_cut_hz: 200,
                high_cut_hz: 2800,
            }
        );
        // Absent fields keep their value.
        let same = b
            .update_channel(v1::UpdateChannel {
                channel_id: 5,
                ..Default::default()
            })
            .await
            .expect("noop");
        assert_eq!(same, updated);
        let upd = |u: v1::UpdateChannel| b.update_channel(u);
        assert_eq!(
            code(
                upd(v1::UpdateChannel {
                    channel_id: 9,
                    ..Default::default()
                })
                .await
            ),
            v1::ResultCode::NotFound
        );
        assert_eq!(
            code(
                upd(v1::UpdateChannel {
                    channel_id: 5,
                    frequency_hz: Some(1),
                    ..Default::default()
                })
                .await
            ),
            v1::ResultCode::OutOfRange
        );
        assert_eq!(
            code(
                upd(v1::UpdateChannel {
                    channel_id: 5,
                    mode: Some(42),
                    ..Default::default()
                })
                .await
            ),
            v1::ResultCode::Unsupported
        );
        assert_eq!(
            code(
                upd(v1::UpdateChannel {
                    channel_id: 5,
                    low_cut_hz: Some(5000),
                    ..Default::default()
                })
                .await
            ),
            v1::ResultCode::InvalidArgument
        );
        assert_eq!(
            code(
                upd(v1::UpdateChannel {
                    channel_id: 5,
                    squelch_snr_db: Some(f32::NAN),
                    ..Default::default()
                })
                .await
            ),
            v1::ResultCode::InvalidArgument
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waterfall_rows_decode_to_exact_bins() {
        let b = FakeBackend::default();
        for (id, compression) in [
            (1, v1::WaterfallCompression::Zstd),
            (2, v1::WaterfallCompression::None),
        ] {
            let (sub, mut rows) = b
                .subscribe_waterfall(sub_req(id, compression))
                .await
                .expect("subscribe");
            assert_eq!(sub.bins, 256);
            assert_eq!(sub.row_interval_ms, 100);
            assert_eq!(sub.quantization, Some(DEFAULT_QUANTIZATION));
            let start = Instant::now();
            let mut decoder = WaterfallDecoder::new();
            let mut out = Vec::new();
            for seq in 0..3 {
                let row = rows.next().await.expect("row");
                assert_eq!(row.subscription_id, id);
                assert_eq!(row.seq, seq);
                decoder
                    .decode(&row.payload, compression as i32, 256, &mut out)
                    .expect("decodes");
                assert_eq!(out.len(), 256);
                assert_eq!(out[128], ROW_PEAK);
                assert!(out[0] >= ROW_FLOOR && out[0] < ROW_FLOOR + 5);
            }
            assert_eq!(start.elapsed(), Duration::from_millis(300));
            b.unsubscribe_waterfall(id).await.expect("unsubscribe");
            assert!(rows.next().await.is_none());
        }
        assert!(b.subscription_ids().is_empty());
        assert_eq!(
            code(b.unsubscribe_waterfall(1).await),
            v1::ResultCode::NotFound
        );
    }

    #[tokio::test]
    async fn subscribe_errors_and_clamping() {
        let b = FakeBackend::default();
        let mut r = sub_req(1, v1::WaterfallCompression::Zstd);
        r.row_interval_ms = 1;
        let (sub, _rows) = b.subscribe_waterfall(r).await.expect("clamped");
        assert_eq!(sub.row_interval_ms, 50, "clamped up to min_row_interval_ms");
        let b_ref = &b;
        let sub_code = |r| async move { code(b_ref.subscribe_waterfall(r).await) };
        assert_eq!(
            sub_code(sub_req(1, v1::WaterfallCompression::Zstd)).await,
            v1::ResultCode::AlreadyExists
        );
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.bins = 10;
        assert_eq!(sub_code(r).await, v1::ResultCode::OutOfRange);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.bins = 100_000;
        assert_eq!(sub_code(r).await, v1::ResultCode::OutOfRange);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.range = Some(v1::FrequencyRange {
            start_hz: 1,
            end_hz: 2,
        });
        assert_eq!(sub_code(r).await, v1::ResultCode::OutOfRange);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.range = Some(v1::FrequencyRange {
            start_hz: 7_100_000,
            end_hz: 7_100_000,
        });
        assert_eq!(sub_code(r).await, v1::ResultCode::InvalidArgument);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.range = None;
        assert_eq!(sub_code(r).await, v1::ResultCode::InvalidArgument);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.compression = 0;
        assert_eq!(sub_code(r).await, v1::ResultCode::InvalidArgument);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.compression = 77;
        assert_eq!(sub_code(r).await, v1::ResultCode::Unsupported);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.row_interval_ms = MAX_ROW_INTERVAL_MS + 1;
        assert_eq!(sub_code(r).await, v1::ResultCode::OutOfRange);
        let mut r = sub_req(2, v1::WaterfallCompression::Zstd);
        r.quantization = Some(v1::Quantization {
            offset_db: -130.0,
            step_db: 0.0,
        });
        assert_eq!(sub_code(r).await, v1::ResultCode::InvalidArgument);
        // Capacity: 4 subscriptions by default.
        for id in 2..=4 {
            let _ = b
                .subscribe_waterfall(sub_req(id, v1::WaterfallCompression::None))
                .await
                .expect("fits");
        }
        assert_eq!(
            sub_code(sub_req(5, v1::WaterfallCompression::None)).await,
            v1::ResultCode::ResourceExhausted
        );
        // No waterfall at all.
        let mut config = FakeConfig::default();
        config.capabilities.waterfall = None;
        let none = FakeBackend::new(config);
        assert_eq!(
            code(
                none.subscribe_waterfall(sub_req(1, v1::WaterfallCompression::None))
                    .await
            ),
            v1::ResultCode::Unsupported
        );
    }

    #[tokio::test]
    async fn retune_closes_channels_outside_and_announces_it() {
        let b = FakeBackend::default();
        let mut events = b.events();
        let (_, mut inside) = b.open_channel(open_req(1, 7_100_000)).await.expect("1");
        let (_, mut outside) = b.open_channel(open_req(2, 6_200_000)).await.expect("2");
        let state = b
            .set_frontend(v1::SetFrontend {
                gain_db: Some(30.0),
                hardware_agc: Some(true),
                center_frequency_hz: Some(7_500_000),
                ..Default::default()
            })
            .await
            .expect("retune");
        assert_eq!(state.center_frequency_hz, 7_500_000);
        assert!(state.hardware_agc);
        assert!((state.gain_db - 30.0).abs() < f32::EPSILON);
        let tunable = v1::FrequencyRange {
            start_hz: 6_476_000,
            end_hz: 8_524_000,
        };
        assert_eq!(state.tunable_range, Some(tunable));
        assert_eq!(
            b.capabilities().await.expect("caps").receiver.tunable_range,
            Some(tunable)
        );
        assert_eq!(b.channel_ids(), vec![1]);
        assert!(
            outside.next().await.is_none(),
            "closed channel's audio ends"
        );
        let closed = events.next().await.expect("event");
        assert_eq!(closed.kind, v1::event::Kind::ChannelClosed as i32);
        assert_eq!(closed.channel_id, Some(2));
        let changed = events.next().await.expect("event");
        assert_eq!(changed.kind, v1::event::Kind::FrontendChanged as i32);
        assert_eq!(
            b.health().frontend.map(|f| f.center_frequency_hz),
            Some(7_500_000)
        );
        b.close_channel(1).await.expect("close");
        while inside.next().await.is_some() {}
    }

    #[tokio::test]
    async fn set_frontend_errors() {
        let b = FakeBackend::default();
        let set = |r| b.set_frontend(r);
        let r = |f: fn(&mut v1::SetFrontend)| {
            let mut req = v1::SetFrontend::default();
            f(&mut req);
            req
        };
        assert_eq!(
            code(set(r(|q| q.gain_db = Some(100.0))).await),
            v1::ResultCode::OutOfRange
        );
        assert_eq!(
            code(set(r(|q| q.gain_db = Some(f32::INFINITY))).await),
            v1::ResultCode::OutOfRange
        );
        assert_eq!(
            code(set(r(|q| q.attenuation_db = Some(10.0))).await),
            v1::ResultCode::Unsupported
        );
        assert_eq!(
            code(set(r(|q| q.antenna_port = Some("B".into()))).await),
            v1::ResultCode::InvalidArgument
        );
        assert_eq!(
            code(set(r(|q| q.center_frequency_hz = Some(10_000_000_000))).await),
            v1::ResultCode::OutOfRange
        );

        let mut config = FakeConfig::default();
        config.capabilities.receiver.frontend = Some(v1::FrontendControls {
            has_attenuator: true,
            max_attenuation_db: 20.0,
            antenna_ports: vec!["A".into(), "B".into()],
            ..Default::default()
        });
        let b2 = FakeBackend::new(config);
        let set2 = |r| b2.set_frontend(r);
        assert_eq!(
            code(set2(r(|q| q.gain_db = Some(1.0))).await),
            v1::ResultCode::Unsupported
        );
        assert_eq!(
            code(set2(r(|q| q.hardware_agc = Some(true))).await),
            v1::ResultCode::Unsupported
        );
        assert_eq!(
            code(set2(r(|q| q.center_frequency_hz = Some(7_000_000))).await),
            v1::ResultCode::Unsupported
        );
        assert_eq!(
            code(set2(r(|q| q.attenuation_db = Some(30.0))).await),
            v1::ResultCode::OutOfRange
        );
        let ok = set2(r(|q| {
            q.attenuation_db = Some(10.0);
            q.antenna_port = Some("B".into());
        }))
        .await
        .expect("ok");
        assert_eq!(ok.antenna_port, "B");
        assert!((ok.attenuation_db - 10.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn events_and_health_are_injectable() {
        let b = FakeBackend::default();
        b.emit_event(v1::Event::default()); // no subscriber: dropped
        let mut events = b.events();
        let event = v1::Event {
            kind: v1::event::Kind::DeviceLost as i32,
            ..Default::default()
        };
        b.emit_event(event.clone());
        assert_eq!(events.next().await, Some(event));
        let health = Health {
            backend: v1::BackendHealth {
                state: v1::backend_health::State::Down as i32,
                version: String::new(),
            },
            frontend: None,
        };
        b.set_health(health.clone());
        assert_eq!(b.clone().health(), health);
    }

    #[tokio::test]
    async fn lagging_event_subscriber_skips() {
        let b = FakeBackend::default();
        let mut events = b.events();
        for i in 0..(EVENT_BUFFER + 10) {
            b.emit_event(v1::Event {
                kind: v1::event::Kind::FrontendOverload as i32,
                detail: i.to_string(),
                ..Default::default()
            });
        }
        let first = events.next().await.expect("event after lag");
        assert_eq!(first.detail, "10");
    }
}
