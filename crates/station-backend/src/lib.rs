// SPDX-License-Identifier: GPL-3.0-only
//! The DSP backend interface of `stationd` (station-protocol §9) and a
//! deterministic [`FakeBackend`].
//!
//! `stationd` owns the tunnel, the local policy (`stationd.toml`) and the
//! per-connection channel and subscription tables; a [`Backend`] only drives
//! the radio. Every method takes and returns `station_proto::v1` types, so
//! the daemon forwards results to the hub without translation.
//!
//! Contract for implementations:
//!
//! - IDs come from the hub (§4). `open_channel` must create a channel with
//!   exactly `req.channel_id` and fail with `ALREADY_EXISTS` if it is in use;
//!   likewise for subscriptions.
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
//!
//! The `backend-ka9q` crate implements this trait.

pub mod fake;

use std::pin::Pin;

use futures_util::Stream;
use station_proto::v1;

pub use fake::{FakeBackend, FakeConfig};

/// A hub-assigned audio channel ID (`OpenChannel.channel_id`).
pub type ChannelId = u32;

/// A hub-assigned waterfall subscription ID
/// (`SubscribeWaterfall.subscription_id`).
pub type SubscriptionId = u32;

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

/// A DSP backend driven by `stationd` (station-protocol §9).
///
/// All methods may be called concurrently. Implementations must never panic
/// on hub-supplied values: `stationd` has only checked enums and required
/// fields (`station_proto::validate::check_request`), not ranges.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// The backend kind and version reported in `Hello.backend`.
    fn info(&self) -> v1::BackendInfo;

    /// Current capabilities (§9 `GetCapabilities`). `stationd` intersects
    /// them with the owner's local caps before answering.
    async fn capabilities(&self) -> Result<v1::Capabilities, BackendError>;

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

    /// Applies shared front-end settings (§4 `SetFrontend`). `stationd` has
    /// already checked `allow_frontend_control`.
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
