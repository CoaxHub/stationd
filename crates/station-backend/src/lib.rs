// SPDX-License-Identifier: GPL-3.0-only
//! The DSP backend interface of `stationd` (station-protocol §9) and a
//! deterministic [`FakeBackend`].
//!
//! A [`Backend`] drives **one receiver**: one SDR and its DSP process
//! (radiod). `stationd` owns the tunnel, the station's receivers (which
//! backend serves which receiver, §4.2) and the per-connection channel and
//! subscription tables; it holds one backend per running receiver and routes
//! each request by its `receiver_id`. A backend doesn't know its receiver's
//! ID: it echoes the request's where an answer has one, and `stationd` sets
//! it in every answer, `FrontendState` and event that names a receiver.
//! Every method takes and returns `station_proto::v1` types, so the daemon
//! forwards results to the hub without translation.
//!
//! Contract for implementations:
//!
//! - IDs come from the hub (§4). `open_channel` must create a channel with
//!   exactly `req.channel_id` and fail with `ALREADY_EXISTS` if it is in use;
//!   likewise for subscriptions. A channel never leaves its receiver.
//! - Failures are a [`BackendError`] carrying the `ResultCode` the hub gets
//!   (`OUT_OF_RANGE`, `RESOURCE_EXHAUSTED`, `UNSUPPORTED`, `NOT_FOUND`,
//!   `BACKEND_ERROR`, ...). The message is shown to the owner, escaped.
//! - Media streams ([`AudioStream`], [`RowStream`]) end when the channel or
//!   subscription is closed (by `close_channel`/`unsubscribe_waterfall`, or
//!   by the backend itself, which then also emits `Event{CHANNEL_CLOSED}` or
//!   `Event{SUBSCRIPTION_CLOSED}` on [`Backend::events`]). `stationd` may
//!   also simply drop a stream; the backend must tolerate that.
//! - Nothing here is per connection: `stationd` closes every channel and
//!   subscription itself when the tunnel drops (§2.3).
//! - **Measurement channels** (station protocol §4.1) are
//!   `stationd`'s own spectrum channels, outside listener capacity: never
//!   counted against `max_channels` or the waterfall subscriptions, and
//!   never a reason to refuse a listener. Their IDs ([`MeasurementId`]) are
//!   `stationd`'s, not the hub's. A backend that can measure overrides
//!   [`Backend::can_measure`] and the four `*_measurement` methods: open with
//!   a fixed bin count and resolution, retune (the bin count never changes on
//!   a live channel), poll, close. Measuring never changes the front end.
//!
//! `backend-ka9q` implements this trait for radiod.

pub mod fake;
pub mod reduce;

use std::pin::Pin;

use futures_util::Stream;
use station_proto::v1;

pub use fake::{FAKE_MEASUREMENT_GRID_HZ, FakeBackend, FakeConfig, MeasurementOp};

/// A hub-assigned audio channel ID (`OpenChannel.channel_id`).
pub type ChannelId = u32;

/// A hub-assigned waterfall subscription ID
/// (`SubscribeWaterfall.subscription_id`).
pub type SubscriptionId = u32;

/// A measurement channel ID, chosen by `stationd` (not by the hub).
pub type MeasurementId = u32;

/// A measurement spectrum channel to open (the measurement design §2.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeasurementChannel {
    /// The first window's centre, Hz.
    pub center_hz: u64,
    /// Bins per spectrum, fixed for the channel's life.
    pub bin_count: u32,
    /// Width of one bin, Hz.
    pub resolution_bw_hz: f32,
    /// How often `stationd` will poll it (a backend that polls on its own
    /// clock uses this).
    pub poll_interval: std::time::Duration,
}

/// Where a backend places a measurement window exactly: centres whose offset
/// from `anchor_hz` (radiod: the front end's LO) is a multiple of `grid_hz`.
/// Off the grid a spectrum may sit up to one bin away from where it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementGrid {
    /// The grid's origin, Hz.
    pub anchor_hz: u64,
    /// Its step, Hz.
    pub grid_hz: u64,
}

/// One polled spectrum of a measurement channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Spectrum {
    /// The centre the spectrum was taken at, Hz.
    pub center_hz: u64,
    /// Width of one bin, Hz.
    pub bin_hz: f32,
    /// Power per bin in dB on the backend's uncalibrated scale (not yet
    /// normalised to density), lowest frequency first; `bin_count` values.
    /// Bin `i` is centred at `center_hz + (i − ⌈n/2⌉) · bin_hz` (radiod's
    /// layout: an odd count has one more bin below the centre).
    pub power_db: Vec<f32>,
}

/// What an open channel produces, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelOutput {
    /// One Opus packet (§6.1). `channel_id` is the hub's ID.
    Audio(v1::AudioFrame),
    /// An S-meter reading, at most 5 per second (§5 `ChannelSignal`).
    Signal(v1::ChannelSignal),
}

/// The output of one audio channel; ends when the channel is closed.
pub type AudioStream = Pin<Box<dyn Stream<Item = ChannelOutput> + Send + 'static>>;

/// The rows of one waterfall subscription; ends when it is closed.
pub type RowStream = Pin<Box<dyn Stream<Item = v1::WaterfallRow> + Send + 'static>>;

/// Asynchronous backend events (`Event.Kind` other than `SHUTTING_DOWN`,
/// which `stationd` sends itself).
pub type EventStream = Pin<Box<dyn Stream<Item = v1::Event> + Send + 'static>>;

/// What one receiver's backend can do (§9 `GetCapabilities`): the
/// receiver's own capabilities, and what its backend demodulates, encodes
/// and streams. `stationd` combines the backends of the running receivers
/// into one `Capabilities`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BackendCapabilities {
    /// The receiver: tunable and hardware ranges, sample rate, channel
    /// limit, front-end controls and `sdr_model`. `receiver_id` is 0;
    /// `stationd` sets it.
    pub receiver: v1::ReceiverCapabilities,
    /// The modes it demodulates (`v1::Mode` values).
    pub modes: Vec<i32>,
    /// The codecs it encodes (`v1::AudioCodec` values).
    pub codecs: Vec<i32>,
    /// Its waterfall; `None` when it has none.
    pub waterfall: Option<v1::WaterfallLimits>,
}

/// Backend and front-end health, for `StationStatus` (§9).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Health {
    /// The DSP backend process (radiod) as seen by `stationd`.
    pub backend: v1::BackendHealth,
    /// The SDR front end; `None` when unknown (backend down).
    pub frontend: Option<v1::FrontendHealth>,
}

/// A failed backend operation, answered to the hub as `Result{code, message}`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", code.as_str_name())]
pub struct BackendError {
    /// The result code the hub receives. Never `OK` or `UNSPECIFIED`.
    pub code: v1::ResultCode,
    /// Detail for the owner's dashboard (untrusted by the hub, escaped).
    pub message: String,
}

impl BackendError {
    /// An error with `code` and `message`.
    pub fn new(code: v1::ResultCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `RESULT_CODE_UNSUPPORTED`.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(v1::ResultCode::Unsupported, message)
    }

    /// `RESULT_CODE_NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(v1::ResultCode::NotFound, message)
    }

    /// `RESULT_CODE_INVALID_ARGUMENT`.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(v1::ResultCode::InvalidArgument, message)
    }

    /// `RESULT_CODE_OUT_OF_RANGE`.
    pub fn out_of_range(message: impl Into<String>) -> Self {
        Self::new(v1::ResultCode::OutOfRange, message)
    }

    /// `RESULT_CODE_RESOURCE_EXHAUSTED`.
    pub fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(v1::ResultCode::ResourceExhausted, message)
    }

    /// `RESULT_CODE_ALREADY_EXISTS`.
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self::new(v1::ResultCode::AlreadyExists, message)
    }

    /// `RESULT_CODE_BACKEND_ERROR`.
    pub fn backend(message: impl Into<String>) -> Self {
        Self::new(v1::ResultCode::BackendError, message)
    }
}

/// The DSP backend of one receiver, driven by `stationd`
/// (station-protocol §9).
///
/// All methods may be called concurrently. Implementations must never panic
/// on hub-supplied values: `stationd` has only checked enums and required
/// fields (`station_proto::validate::check_request`) and that the request
/// names this receiver, not ranges.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// The backend kind and version reported in `Hello.backend`.
    fn info(&self) -> v1::BackendInfo;

    /// Current capabilities (§9 `GetCapabilities`). An answer also tells
    /// `stationd` that the backend answers: until it does, the receiver is
    /// `STARTING` (§4.2).
    async fn capabilities(&self) -> Result<BackendCapabilities, BackendError>;

    /// Creates channel `req.channel_id` and starts its audio (§4).
    async fn open_channel(
        &self,
        req: v1::OpenChannel,
    ) -> Result<(v1::ChannelOpened, AudioStream), BackendError>;

    /// Changes an open channel in place; audio continues (§4).
    async fn update_channel(
        &self,
        req: v1::UpdateChannel,
    ) -> Result<v1::ChannelUpdated, BackendError>;

    /// Closes a channel; its [`AudioStream`] ends.
    async fn close_channel(&self, id: ChannelId) -> Result<(), BackendError>;

    /// Applies front-end settings to this receiver (§4 `SetFrontend`): what
    /// its `FrontendControls` offer live, `UNSUPPORTED` for the rest.
    /// `stationd` has already checked a new centre against the receiver's
    /// profile.
    async fn set_frontend(&self, req: v1::SetFrontend) -> Result<v1::FrontendState, BackendError>;

    /// Starts waterfall subscription `req.subscription_id` (§4, §6.2).
    async fn subscribe_waterfall(
        &self,
        req: v1::SubscribeWaterfall,
    ) -> Result<(v1::WaterfallSubscribed, RowStream), BackendError>;

    /// Stops a subscription; its [`RowStream`] ends.
    async fn unsubscribe_waterfall(&self, id: SubscriptionId) -> Result<(), BackendError>;

    /// A health snapshot for `StationStatus`. Must not block.
    fn health(&self) -> Health;

    /// A new stream of backend events, from now on. Each call returns an
    /// independent stream (`stationd` subscribes once per connection).
    fn events(&self) -> EventStream;

    /// The current front-end settings, for measurement reports. The default
    /// builds them from [`Backend::capabilities`] and [`Backend::health`]
    /// (gain and centre; no AGC, attenuation or antenna port).
    async fn frontend_state(&self) -> Result<v1::FrontendState, BackendError> {
        let caps = self.capabilities().await?.receiver;
        let health = self.health();
        let frontend = health.frontend.unwrap_or_default();
        let center = caps
            .tunable_range
            .map_or(frontend.center_frequency_hz, |r| {
                if frontend.center_frequency_hz == 0 {
                    r.start_hz / 2 + r.end_hz / 2
                } else {
                    frontend.center_frequency_hz
                }
            });
        Ok(v1::FrontendState {
            receiver_id: 0,
            gain_db: frontend.gain_db,
            hardware_agc: false,
            attenuation_db: 0.0,
            antenna_port: String::new(),
            center_frequency_hz: center,
            tunable_range: caps.tunable_range,
        })
    }

    /// Whether this backend has measurement channels (station protocol §4.1).
    fn can_measure(&self) -> bool {
        false
    }

    /// Opens measurement channel `id`.
    async fn open_measurement(
        &self,
        id: MeasurementId,
        channel: MeasurementChannel,
    ) -> Result<(), BackendError> {
        let _ = (id, channel);
        Err(BackendError::unsupported("this backend cannot measure"))
    }

    /// Where windows of `resolution_bw_hz` bins sit exactly on the current
    /// front end; `None` when the backend places them anywhere (or can't
    /// tell).
    async fn measurement_grid(&self, resolution_bw_hz: f32) -> Option<MeasurementGrid> {
        let _ = resolution_bw_hz;
        None
    }

    /// Moves measurement channel `id` to `center_hz` (same bin count and
    /// resolution). Returns the spectrum the move's own reply carries when
    /// the backend's reply has one taken after the move (radiod's does):
    /// `stationd` counts it as the dwell's first spectrum.
    async fn retune_measurement(
        &self,
        id: MeasurementId,
        center_hz: u64,
    ) -> Result<Option<Spectrum>, BackendError> {
        let _ = (id, center_hz);
        Err(BackendError::unsupported("this backend cannot measure"))
    }

    /// Polls measurement channel `id` for a fresh spectrum.
    async fn poll_measurement(&self, id: MeasurementId) -> Result<Spectrum, BackendError> {
        let _ = id;
        Err(BackendError::unsupported("this backend cannot measure"))
    }

    /// Closes measurement channel `id`.
    async fn close_measurement(&self, id: MeasurementId) -> Result<(), BackendError> {
        let _ = id;
        Err(BackendError::unsupported("this backend cannot measure"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_constructors_carry_codes() {
        let cases = [
            (BackendError::unsupported("x"), v1::ResultCode::Unsupported),
            (BackendError::not_found("x"), v1::ResultCode::NotFound),
            (
                BackendError::invalid_argument("x"),
                v1::ResultCode::InvalidArgument,
            ),
            (BackendError::out_of_range("x"), v1::ResultCode::OutOfRange),
            (
                BackendError::resource_exhausted("x"),
                v1::ResultCode::ResourceExhausted,
            ),
            (
                BackendError::already_exists("x"),
                v1::ResultCode::AlreadyExists,
            ),
            (BackendError::backend("x"), v1::ResultCode::BackendError),
        ];
        for (err, code) in cases {
            assert_eq!(err.code, code);
            assert_eq!(err.message, "x");
            assert!(err.to_string().starts_with("RESULT_CODE_"), "{err}");
        }
    }
}
