// SPDX-License-Identifier: MIT
//! Station side of §3: requests the station cannot interpret are answered
//! with an error result instead of disconnecting.

use super::CommandKind;
use super::fields::enum_known;
use crate::v1;

/// Why the station refuses a request before executing it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RequestRejection {
    /// §3: a command or enum value unknown in the negotiated minor (e.g. from
    /// a newer hub). Answer `RESULT_CODE_UNSUPPORTED`.
    #[error("{field} is not supported")]
    Unsupported {
        /// Field path.
        field: &'static str,
    },
    /// A required field is missing, `UNSPECIFIED` or 0. Answer
    /// `RESULT_CODE_INVALID_ARGUMENT`.
    #[error("{field} is invalid")]
    InvalidArgument {
        /// Field path.
        field: &'static str,
    },
}

impl RequestRejection {
    /// The result code to answer with.
    pub fn result_code(&self) -> v1::ResultCode {
        match self {
            Self::Unsupported { .. } => v1::ResultCode::Unsupported,
            Self::InvalidArgument { .. } => v1::ResultCode::InvalidArgument,
        }
    }

    /// A `Result` ready to put in the `Response`.
    pub fn to_result(&self) -> v1::Result {
        v1::Result {
            code: self.result_code() as i32,
            message: self.to_string(),
        }
    }
}

/// Station side: checks a hub request's command and enum fields against the
/// negotiated `minor` (§3). Unknown values → [`RequestRejection::Unsupported`];
/// `request_id` 0, a missing command-specific sub-message or `UNSPECIFIED`
/// where a value is required → [`RequestRejection::InvalidArgument`].
///
/// A plan with `measurement_id` 0 is invalid. In `ConfigureReceivers`, every
/// `model` and `profile` must be a known, specified value; the set's rules
/// against the hardware are [`super::check_receiver_set`]. Range, receiver
/// and capacity checks (a running receiver, its tunable range,
/// `max_channels`, the plan's bands, ...) are the station's own business and
/// not done here.
pub fn check_request(req: &v1::Request, minor: u32) -> Result<CommandKind, RequestRejection> {
    use v1::request::Command;
    let command = req.command.as_ref().ok_or(RequestRejection::Unsupported {
        field: "request.command",
    })?;
    if req.request_id == 0 {
        return Err(RequestRejection::InvalidArgument {
            field: "request.request_id",
        });
    }
    match command {
        Command::OpenChannel(o) => {
            required::<v1::Mode>("open_channel.mode", o.mode, minor)?;
            let audio = o.audio.as_ref().ok_or(RequestRejection::InvalidArgument {
                field: "open_channel.audio",
            })?;
            required::<v1::AudioCodec>("open_channel.audio.codec", audio.codec, minor)?;
        }
        Command::UpdateChannel(u) => {
            if let Some(mode) = u.mode {
                required::<v1::Mode>("update_channel.mode", mode, minor)?;
            }
        }
        Command::SubscribeWaterfall(s) => {
            required::<v1::WaterfallCompression>(
                "subscribe_waterfall.compression",
                s.compression,
                minor,
            )?;
            if s.range.is_none() {
                return Err(RequestRejection::InvalidArgument {
                    field: "subscribe_waterfall.range",
                });
            }
        }
        Command::AdminOp(a) => required::<v1::AdminOpKind>("admin_op.kind", a.kind, minor)?,
        Command::StartMeasurements(m) => {
            if m.measurement_id == 0 {
                return Err(RequestRejection::InvalidArgument {
                    field: "start_measurements.measurement_id",
                });
            }
        }
        Command::ConfigureReceivers(c) => {
            for rx in &c.receivers {
                required::<v1::SdrModel>("configure_receivers.receivers.model", rx.model, minor)?;
                required::<v1::SdrProfile>(
                    "configure_receivers.receivers.profile",
                    rx.profile,
                    minor,
                )?;
            }
        }
        Command::StopMeasurements(_)
        | Command::GetCapabilities(_)
        | Command::GetHardware(_)
        | Command::CloseChannel(_)
        | Command::SetFrontend(_)
        | Command::UnsubscribeWaterfall(_)
        | Command::Ping(_) => {}
    }
    Ok(CommandKind::of(command))
}

fn required<E: TryFrom<i32>>(
    field: &'static str,
    value: i32,
    _minor: u32,
) -> Result<(), RequestRejection> {
    if !enum_known::<E>(value) {
        return Err(RequestRejection::Unsupported { field });
    }
    if value == 0 {
        return Err(RequestRejection::InvalidArgument { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use v1::request::Command;

    fn req(command: Command) -> v1::Request {
        v1::Request {
            request_id: 1,
            command: Some(command),
        }
    }

    fn open(mode: i32, codec: i32) -> Command {
        Command::OpenChannel(v1::OpenChannel {
            channel_id: 1,
            mode,
            audio: Some(v1::AudioParams {
                codec,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    #[test]
    fn accepts_known_commands() {
        assert_eq!(
            check_request(&req(open(1, 1)), 0),
            Ok(CommandKind::OpenChannel)
        );
        assert_eq!(
            check_request(&req(Command::Ping(v1::Ping::default())), 0),
            Ok(CommandKind::Ping)
        );
        assert_eq!(
            check_request(
                &req(Command::UpdateChannel(v1::UpdateChannel {
                    mode: Some(2),
                    ..Default::default()
                })),
                0
            ),
            Ok(CommandKind::UpdateChannel)
        );
        assert_eq!(
            check_request(
                &req(Command::UpdateChannel(v1::UpdateChannel::default())),
                0
            ),
            Ok(CommandKind::UpdateChannel)
        );
        let sub = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            compression: 2,
            range: Some(v1::FrequencyRange::default()),
            ..Default::default()
        });
        assert_eq!(
            check_request(&req(sub), 0),
            Ok(CommandKind::SubscribeWaterfall)
        );
        assert_eq!(
            check_request(
                &req(Command::AdminOp(v1::AdminOp {
                    kind: 2,
                    receiver_id: Some(3)
                })),
                0
            ),
            Ok(CommandKind::AdminOp)
        );
        assert_eq!(
            check_request(&req(Command::GetHardware(v1::GetHardware {})), 0),
            Ok(CommandKind::GetHardware)
        );
        assert_eq!(
            check_request(&req(configure(1, 2)), 0),
            Ok(CommandKind::ConfigureReceivers)
        );
        // An empty set stops every receiver.
        assert_eq!(
            check_request(
                &req(Command::ConfigureReceivers(
                    v1::ConfigureReceivers::default()
                )),
                0
            ),
            Ok(CommandKind::ConfigureReceivers)
        );
    }

    fn configure(model: i32, profile: i32) -> Command {
        Command::ConfigureReceivers(v1::ConfigureReceivers {
            receivers: vec![v1::ReceiverConfig {
                receiver_id: 1,
                model,
                profile,
                center_frequency_hz: 7_100_000,
                ..Default::default()
            }],
        })
    }

    #[test]
    fn receiver_set_enums() {
        assert_eq!(
            check_request(&req(configure(9, 2)), 0),
            Err(RequestRejection::Unsupported {
                field: "configure_receivers.receivers.model"
            })
        );
        assert_eq!(
            check_request(&req(configure(1, 9)), 0),
            Err(RequestRejection::Unsupported {
                field: "configure_receivers.receivers.profile"
            })
        );
        assert_eq!(
            check_request(&req(configure(0, 2)), 0),
            Err(RequestRejection::InvalidArgument {
                field: "configure_receivers.receivers.model"
            })
        );
        assert_eq!(
            check_request(&req(configure(1, 0)), 0),
            Err(RequestRejection::InvalidArgument {
                field: "configure_receivers.receivers.profile"
            })
        );
    }

    #[test]
    fn unknown_values_are_unsupported() {
        let r = check_request(&req(open(99, 1)), 0).expect_err("unknown mode");
        assert_eq!(r.result_code(), v1::ResultCode::Unsupported);
        assert_eq!(r.to_result().code, v1::ResultCode::Unsupported as i32);
        assert!(check_request(&req(open(1, 7)), 0).is_err());
        assert!(
            check_request(
                &req(Command::AdminOp(v1::AdminOp {
                    kind: 42,
                    receiver_id: None
                })),
                0
            )
            .is_err()
        );
        assert!(
            check_request(
                &req(Command::UpdateChannel(v1::UpdateChannel {
                    mode: Some(-1),
                    ..Default::default()
                })),
                0
            )
            .is_err()
        );
        // A oneof member from a newer minor decodes as `None`.
        let none = v1::Request {
            request_id: 1,
            command: None,
        };
        assert_eq!(
            check_request(&none, 0),
            Err(RequestRejection::Unsupported {
                field: "request.command"
            })
        );
    }

    #[test]
    fn unspecified_or_missing_is_invalid() {
        let r = check_request(&req(open(0, 1)), 0).expect_err("unspecified mode");
        assert_eq!(r.result_code(), v1::ResultCode::InvalidArgument);
        let no_audio = Command::OpenChannel(v1::OpenChannel {
            mode: 1,
            ..Default::default()
        });
        assert!(check_request(&req(no_audio), 0).is_err());
        let no_range = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            compression: 1,
            ..Default::default()
        });
        assert_eq!(
            check_request(&req(no_range), 0),
            Err(RequestRejection::InvalidArgument {
                field: "subscribe_waterfall.range"
            })
        );
        let zero_id = v1::Request {
            request_id: 0,
            command: Some(Command::Ping(v1::Ping::default())),
        };
        assert!(check_request(&zero_id, 0).is_err());
    }
}
