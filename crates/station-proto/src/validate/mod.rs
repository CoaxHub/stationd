// SPDX-License-Identifier: MIT
//! Hub-side validation of everything a station sends (station-protocol §5),
//! version negotiation (§3), strikes, the ingress quota, and the station-side
//! check of incoming requests (§3: unknown values → `UNSUPPORTED`).
//!
//! Stations are untrusted. The hub runs every decoded
//! [`StationMessage`](crate::v1::StationMessage) through
//! [`Session::check`] before anything else touches it:
//!
//! - [`Verdict::Accept`]: forward or store it.
//! - [`Verdict::Drop`]: discard it silently (count it in metrics). Not a strike.
//! - [`Verdict::Violation`]: end the session with
//!   [`ViolationKind::goodbye_reason`] and, if [`ViolationKind::is_strike`],
//!   record a strike with [`Strikes`].
//!
//! Every function here is total: no input makes it panic.

mod capabilities;
mod fields;
mod measure;
mod negotiate;
mod quota;
mod rate;
mod request;
mod session;
mod strikes;
mod waterfall;

use std::fmt;

use crate::v1;

pub use capabilities::{SanitizedCapabilities, sanitize_capabilities};
pub use fields::{check_message, decode_station_message};
pub use measure::{duty_of, is_band_key, is_measuring_minute};
pub use negotiate::{
    Unsupported, negotiate, negotiate_against, negotiate_welcome, negotiate_welcome_against,
};
pub use quota::{IngressQuota, QuotaDecision};
pub use request::{RequestRejection, check_request};
pub use session::{ExpiredRequests, RequestError, Session};
pub use strikes::{StrikeOutcome, Strikes};
pub use waterfall::{WaterfallDecoder, decompress_row};

/// The outcome of checking one station message (station-protocol §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The message is valid; process it.
    Accept,
    /// Discard the message; the session continues and no strike is recorded.
    Drop(DropReason),
    /// End the session (`Goodbye` per [`ViolationKind::goodbye_reason`]).
    Violation(ViolationKind),
}

/// Why a message was dropped rather than accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropReason {
    /// Over the per-type rate limit of §5, below the violation threshold.
    RateLimited {
        /// The kind of message that was dropped.
        kind: MessageKind,
    },
    /// Media for a channel or subscription the hub closed less than
    /// [`ID_CLOSE_GRACE`](crate::limits::ID_CLOSE_GRACE) ago (in flight when
    /// it was closed).
    RecentlyClosed {
        /// The kind of message that was dropped.
        kind: MessageKind,
        /// The channel or subscription id.
        id: u32,
    },
    /// A response to a request that already timed out (§2.1).
    LateResponse {
        /// The request id.
        request_id: u64,
    },
    /// A waterfall row for a subscription whose `WaterfallSubscribed` has not
    /// arrived yet, so its bin count and compression are unknown.
    AwaitingSubscription {
        /// The subscription id.
        subscription_id: u32,
    },
    /// An `Event` naming a channel or subscription that is neither open nor
    /// just closed (§5 `Event`).
    UnknownEventTarget,
    /// A noise report for a plan whose `MeasurementsStarted` has not arrived
    /// yet (§5.2).
    AwaitingMeasurements {
        /// The measurement id.
        measurement_id: u32,
    },
    /// §5.2: a measurement report or duty change whose time is implausible
    /// against the hub's clock (not a whole minute, too old or ahead). An
    /// honest station with a drifting clock or a stalled queue can send it.
    MeasurementTime {
        /// The kind of message.
        kind: MessageKind,
    },
    /// §5.2: under idle duty, a noise report for a minute that is not a
    /// measuring minute.
    NotScheduled,
}

/// A protocol violation (station-protocol §5): ends the session, and in most
/// cases records a strike. [`code`](Self::code) is the stable strike kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ViolationKind {
    /// §5 Size: the encoded message is larger than `max_message_bytes`.
    #[error("message of {len} bytes exceeds the {max}-byte limit")]
    MessageTooLarge {
        /// Encoded length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The bytes are not a valid `StationMessage` (protobuf or UTF-8 error).
    #[error("message does not decode as a StationMessage")]
    Decode,
    /// The message has no body (an empty message, or a `oneof` member the
    /// negotiated minor does not have).
    #[error("message has no body")]
    EmptyMessage,
    /// §5 First message: the first message was not `Hello`.
    #[error("first message must be Hello, got {got}")]
    FirstMessageNotHello {
        /// What arrived instead.
        got: MessageKind,
    },
    /// §5 First message: `Hello` was sent again.
    #[error("Hello sent twice")]
    DuplicateHello,
    /// A required field or sub-message is absent.
    #[error("required field {field} is missing")]
    MissingField {
        /// Field path.
        field: &'static str,
    },
    /// §5 Strings: a string is longer than its cap.
    #[error("{field} is {len} bytes, limit {max}")]
    StringTooLong {
        /// Field path.
        field: &'static str,
        /// Length in bytes.
        len: usize,
        /// The cap.
        max: usize,
    },
    /// §5 Strings: a repeated field has too many entries.
    #[error("{field} has {len} entries, limit {max}")]
    TooManyEntries {
        /// Field path.
        field: &'static str,
        /// Number of entries.
        len: usize,
        /// The cap.
        max: usize,
    },
    /// §5 Enums: a value unknown in the negotiated minor.
    #[error("{field} has value {value}, unknown in minor {minor}")]
    UnknownEnumValue {
        /// Field path.
        field: &'static str,
        /// The raw value.
        value: i32,
        /// The negotiated minor.
        minor: u32,
    },
    /// §5 Enums: `UNSPECIFIED` where a value is required.
    #[error("{field} is UNSPECIFIED")]
    UnspecifiedEnum {
        /// Field path.
        field: &'static str,
    },
    /// §5: a float is NaN or infinite.
    #[error("{field} is not finite")]
    NonFiniteFloat {
        /// Field path.
        field: &'static str,
    },
    /// A numeric field outside the values the protocol allows.
    #[error("{field} is out of bounds")]
    OutOfBounds {
        /// Field path.
        field: &'static str,
    },
    /// §5 `Capabilities`: a nonsense frequency range (start > end, or
    /// entirely above [`MAX_FREQUENCY_HZ`](crate::limits::MAX_FREQUENCY_HZ)).
    #[error("{field} is not a valid frequency range")]
    InvalidRange {
        /// Field path.
        field: &'static str,
    },
    /// §5 `Capabilities`: `tunable_range` is not inside `hardware_range`.
    #[error("tunable_range is not inside hardware_range")]
    TunableOutsideHardware,
    /// §5 Response: no outstanding request has this id.
    #[error("response to request {request_id}, which is not outstanding")]
    UnsolicitedResponse {
        /// The request id.
        request_id: u64,
    },
    /// §5 Response: the payload does not match the command (wrong type,
    /// missing on `OK`, or present on an error).
    #[error(
        "response to {command} request {request_id} (result {code}) carries payload {payload:?}"
    )]
    PayloadMismatch {
        /// The request id.
        request_id: u64,
        /// The command that was sent.
        command: CommandKind,
        /// The raw `ResultCode` of the response.
        code: i32,
        /// The payload received, if any.
        payload: Option<PayloadKind>,
    },
    /// §4: the payload does not echo the request (e.g. another
    /// `channel_id`, another `Pong.nonce`, more bins than requested).
    #[error("response to {command} request {request_id} does not match the request in {field}")]
    ResponseFieldMismatch {
        /// The request id.
        request_id: u64,
        /// The command that was sent.
        command: CommandKind,
        /// Field path.
        field: &'static str,
    },
    /// §5 `AudioFrame`/`ChannelSignal`: the channel was never opened on this
    /// connection, or was closed more than the grace period ago.
    #[error("{kind} for channel {channel_id}, which is not open")]
    UnknownChannel {
        /// The kind of message.
        kind: MessageKind,
        /// The channel id.
        channel_id: u32,
    },
    /// §5 `WaterfallRow`: the subscription is not open.
    #[error("waterfall row for subscription {subscription_id}, which is not open")]
    UnknownSubscription {
        /// The subscription id.
        subscription_id: u32,
    },
    /// §5 `AudioFrame`: payload outside 1–1275 bytes.
    #[error("audio payload of {len} bytes is outside 1..=1275")]
    AudioPayloadSize {
        /// Payload length.
        len: usize,
    },
    /// §5 `AudioFrame`: over the rate limit for longer than 10 s.
    #[error("audio on channel {channel_id} over the rate limit for 10 s")]
    AudioRateSustained {
        /// The channel id.
        channel_id: u32,
    },
    /// §5 `WaterfallRow`: compressed payload larger than 16 KiB.
    #[error("waterfall payload of {len} bytes exceeds 16 KiB")]
    WaterfallPayloadTooLarge {
        /// Payload length.
        len: usize,
    },
    /// §5 `WaterfallRow`: the payload is not a valid zstd frame.
    #[error("waterfall payload does not decompress")]
    WaterfallDecompression,
    /// §5 `WaterfallRow`: decompressed length is not exactly `bins`.
    /// `actual == expected + 1` means "more than `bins`" (the zip-bomb cap).
    #[error("waterfall row decompresses to {actual} bytes, expected {expected}")]
    WaterfallLength {
        /// `bins`.
        expected: usize,
        /// Decompressed length, capped at `bins + 1`.
        actual: usize,
    },
    /// §5 `WaterfallRow`: rows faster than 1.5 × the confirmed rate.
    #[error("waterfall rows on subscription {subscription_id} over the rate limit")]
    WaterfallRate {
        /// The subscription id.
        subscription_id: u32,
    },
    /// §2.1: consecutive response timeouts.
    #[error("{consecutive} consecutive response timeouts")]
    ResponseTimeouts {
        /// How many in a row.
        consecutive: u32,
    },
    /// §3: a message type the negotiated minor does not have (a noise report
    /// on a 1.0 session).
    #[error("{kind} is not part of minor {minor}")]
    MessageNotInMinor {
        /// The kind of message.
        kind: MessageKind,
        /// The negotiated minor.
        minor: u32,
    },
    /// §5.2: a noise report or measurement event for a plan the hub never
    /// started on this connection, or stopped more than the grace period ago.
    #[error("{kind} for measurement {measurement_id}, which is not open")]
    UnknownMeasurement {
        /// The kind of message.
        kind: MessageKind,
        /// The measurement id.
        measurement_id: u32,
    },
    /// §5.2: noise reports or duty changes over the rate limit for too long.
    #[error("{kind} for measurement {measurement_id} over the rate limit")]
    MeasurementRate {
        /// The kind of message.
        kind: MessageKind,
        /// The measurement id.
        measurement_id: u32,
    },
    /// §3: the station's protocol major is not supported. Not a strike: the
    /// hub answers `Goodbye{UNSUPPORTED_VERSION}`.
    #[error("unsupported protocol version {major}.{minor}")]
    UnsupportedVersion {
        /// Station's major.
        major: u32,
        /// Station's minor.
        minor: u32,
    },
}

impl ViolationKind {
    /// Stable `snake_case` identifier, for strike records, metrics and log
    /// fields.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MessageTooLarge { .. } => "message_too_large",
            Self::Decode => "decode",
            Self::EmptyMessage => "empty_message",
            Self::FirstMessageNotHello { .. } => "first_message_not_hello",
            Self::DuplicateHello => "duplicate_hello",
            Self::MissingField { .. } => "missing_field",
            Self::StringTooLong { .. } => "string_too_long",
            Self::TooManyEntries { .. } => "too_many_entries",
            Self::UnknownEnumValue { .. } => "unknown_enum_value",
            Self::UnspecifiedEnum { .. } => "unspecified_enum",
            Self::NonFiniteFloat { .. } => "non_finite_float",
            Self::OutOfBounds { .. } => "out_of_bounds",
            Self::InvalidRange { .. } => "invalid_range",
            Self::TunableOutsideHardware => "tunable_outside_hardware",
            Self::UnsolicitedResponse { .. } => "unsolicited_response",
            Self::PayloadMismatch { .. } => "payload_mismatch",
            Self::ResponseFieldMismatch { .. } => "response_field_mismatch",
            Self::UnknownChannel { .. } => "unknown_channel",
            Self::UnknownSubscription { .. } => "unknown_subscription",
            Self::AudioPayloadSize { .. } => "audio_payload_size",
            Self::AudioRateSustained { .. } => "audio_rate_sustained",
            Self::WaterfallPayloadTooLarge { .. } => "waterfall_payload_too_large",
            Self::WaterfallDecompression => "waterfall_decompression",
            Self::WaterfallLength { .. } => "waterfall_length",
            Self::WaterfallRate { .. } => "waterfall_rate",
            Self::ResponseTimeouts { .. } => "response_timeouts",
            Self::MessageNotInMinor { .. } => "message_not_in_minor",
            Self::UnknownMeasurement { .. } => "unknown_measurement",
            Self::MeasurementRate { .. } => "measurement_rate",
            Self::UnsupportedVersion { .. } => "unsupported_version",
        }
    }

    /// Whether this counts as a strike (§5). Everything except an unsupported
    /// version does.
    pub fn is_strike(&self) -> bool {
        !matches!(self, Self::UnsupportedVersion { .. })
    }

    /// The `Goodbye` reason the hub sends for this violation.
    pub fn goodbye_reason(&self) -> v1::goodbye::Reason {
        match self {
            Self::UnsupportedVersion { .. } => v1::goodbye::Reason::UnsupportedVersion,
            _ => v1::goodbye::Reason::ProtocolViolation,
        }
    }
}

/// The type of a `StationMessage` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// `Hello`.
    Hello,
    /// `Response`.
    Response,
    /// `AudioFrame`.
    Audio,
    /// `WaterfallRow`.
    Waterfall,
    /// `ChannelSignal`.
    Signal,
    /// `StationStatus`.
    Status,
    /// `Event`.
    Event,
    /// `NoiseFloorReport` (1.1).
    Noise,
}

impl MessageKind {
    /// The kind of a message body.
    pub fn of(body: &v1::station_message::Body) -> Self {
        use v1::station_message::Body;
        match body {
            Body::Hello(_) => Self::Hello,
            Body::Response(_) => Self::Response,
            Body::Audio(_) => Self::Audio,
            Body::Waterfall(_) => Self::Waterfall,
            Body::Signal(_) => Self::Signal,
            Body::Status(_) => Self::Status,
            Body::Event(_) => Self::Event,
            Body::Noise(_) => Self::Noise,
        }
    }

    /// Stable `snake_case` name for metrics and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Response => "response",
            Self::Audio => "audio",
            Self::Waterfall => "waterfall",
            Self::Signal => "signal",
            Self::Status => "status",
            Self::Event => "event",
            Self::Noise => "noise",
        }
    }
}

impl fmt::Display for MessageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The command of a hub `Request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    /// `GetCapabilities`.
    GetCapabilities,
    /// `OpenChannel`.
    OpenChannel,
    /// `UpdateChannel`.
    UpdateChannel,
    /// `CloseChannel`.
    CloseChannel,
    /// `SetFrontend`.
    SetFrontend,
    /// `SubscribeWaterfall`.
    SubscribeWaterfall,
    /// `UnsubscribeWaterfall`.
    UnsubscribeWaterfall,
    /// `Ping`.
    Ping,
    /// `AdminOp`.
    AdminOp,
    /// `StartMeasurements` (1.1).
    StartMeasurements,
    /// `StopMeasurements` (1.1).
    StopMeasurements,
}

impl CommandKind {
    /// Every command, in proto field order.
    pub const ALL: [Self; 11] = [
        Self::GetCapabilities,
        Self::OpenChannel,
        Self::UpdateChannel,
        Self::CloseChannel,
        Self::SetFrontend,
        Self::SubscribeWaterfall,
        Self::UnsubscribeWaterfall,
        Self::Ping,
        Self::AdminOp,
        Self::StartMeasurements,
        Self::StopMeasurements,
    ];

    /// The kind of a request command.
    pub fn of(command: &v1::request::Command) -> Self {
        use v1::request::Command;
        match command {
            Command::GetCapabilities(_) => Self::GetCapabilities,
            Command::OpenChannel(_) => Self::OpenChannel,
            Command::UpdateChannel(_) => Self::UpdateChannel,
            Command::CloseChannel(_) => Self::CloseChannel,
            Command::SetFrontend(_) => Self::SetFrontend,
            Command::SubscribeWaterfall(_) => Self::SubscribeWaterfall,
            Command::UnsubscribeWaterfall(_) => Self::UnsubscribeWaterfall,
            Command::Ping(_) => Self::Ping,
            Command::AdminOp(_) => Self::AdminOp,
            Command::StartMeasurements(_) => Self::StartMeasurements,
            Command::StopMeasurements(_) => Self::StopMeasurements,
        }
    }

    /// The payload an `OK` response to this command carries (gateway.proto
    /// `Response.payload`, §4), or `None` if it carries none.
    pub fn ok_payload(self) -> Option<PayloadKind> {
        match self {
            Self::GetCapabilities => Some(PayloadKind::Capabilities),
            Self::OpenChannel => Some(PayloadKind::ChannelOpened),
            Self::UpdateChannel => Some(PayloadKind::ChannelUpdated),
            Self::SetFrontend => Some(PayloadKind::FrontendState),
            Self::SubscribeWaterfall => Some(PayloadKind::WaterfallSubscribed),
            Self::Ping => Some(PayloadKind::Pong),
            Self::StartMeasurements => Some(PayloadKind::MeasurementsStarted),
            Self::CloseChannel
            | Self::UnsubscribeWaterfall
            | Self::AdminOp
            | Self::StopMeasurements => None,
        }
    }

    /// How long the station has to answer (§2.1).
    pub fn timeout(self) -> std::time::Duration {
        match self {
            Self::GetCapabilities => crate::limits::GET_CAPABILITIES_TIMEOUT,
            _ => crate::limits::RESPONSE_TIMEOUT,
        }
    }

    /// Stable `snake_case` name for metrics and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GetCapabilities => "get_capabilities",
            Self::OpenChannel => "open_channel",
            Self::UpdateChannel => "update_channel",
            Self::CloseChannel => "close_channel",
            Self::SetFrontend => "set_frontend",
            Self::SubscribeWaterfall => "subscribe_waterfall",
            Self::UnsubscribeWaterfall => "unsubscribe_waterfall",
            Self::Ping => "ping",
            Self::AdminOp => "admin_op",
            Self::StartMeasurements => "start_measurements",
            Self::StopMeasurements => "stop_measurements",
        }
    }
}

impl fmt::Display for CommandKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The payload type of a station `Response`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PayloadKind {
    /// `Capabilities`.
    Capabilities,
    /// `ChannelOpened`.
    ChannelOpened,
    /// `ChannelUpdated`.
    ChannelUpdated,
    /// `FrontendState`.
    FrontendState,
    /// `WaterfallSubscribed`.
    WaterfallSubscribed,
    /// `Pong`.
    Pong,
    /// `MeasurementsStarted` (1.1).
    MeasurementsStarted,
}

impl PayloadKind {
    /// Every payload type, in proto field order.
    pub const ALL: [Self; 7] = [
        Self::Capabilities,
        Self::ChannelOpened,
        Self::ChannelUpdated,
        Self::FrontendState,
        Self::WaterfallSubscribed,
        Self::Pong,
        Self::MeasurementsStarted,
    ];

    /// The kind of a response payload.
    pub fn of(payload: &v1::response::Payload) -> Self {
        use v1::response::Payload;
        match payload {
            Payload::Capabilities(_) => Self::Capabilities,
            Payload::ChannelOpened(_) => Self::ChannelOpened,
            Payload::ChannelUpdated(_) => Self::ChannelUpdated,
            Payload::FrontendState(_) => Self::FrontendState,
            Payload::WaterfallSubscribed(_) => Self::WaterfallSubscribed,
            Payload::Pong(_) => Self::Pong,
            Payload::MeasurementsStarted(_) => Self::MeasurementsStarted,
        }
    }
}
