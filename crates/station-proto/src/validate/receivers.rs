// SPDX-License-Identifier: MIT
//! Receivers (station-protocol §4.2, §5, §5.1).
//!
//! - [`check_receiver_set`]: the station-side rules of a `ConfigureReceivers`
//!   set against the station's `Hardware` report (§4.2). `stationd` runs it
//!   before applying a set; the control plane runs it before storing the
//!   owner's set, so it never sends one the station would refuse.
//! - [`select_receivers`]: which receivers of a `Capabilities` the hub serves
//!   (§5.1): at most its own limit, highest IDs ignored first, and of two
//!   overlapping receivers the one with the lower ID.
//! - [`predicted_tunable_range`] and [`ranges_overlap`]: the arithmetic both
//!   rely on.
//! - The stateless hub-side checks of `Hardware` and `ReceiversConfigured`
//!   (§5), run by [`super::check_message`].

use std::collections::HashSet;

use super::fields::{check_enum, check_len, check_range, missing};
use super::{MessageKind, ViolationKind};
use crate::limits;
use crate::v1;

// ------------------------------------------------------------------ ranges

/// §4.2: the tunable range a profile gives a receiver centred on
/// `center_hz`: the centre ± half the profile's `tunable_span_hz`.
pub fn predicted_tunable_range(center_hz: u64, tunable_span_hz: u32) -> v1::FrequencyRange {
    let half = u64::from(tunable_span_hz / 2);
    v1::FrequencyRange {
        start_hz: center_hz.saturating_sub(half),
        end_hz: center_hz.saturating_add(half),
    }
}

/// §4.2, §5.1: whether two tunable ranges share a frequency. Ranges that
/// only touch (one ends where the other starts) don't overlap.
pub fn ranges_overlap(a: &v1::FrequencyRange, b: &v1::FrequencyRange) -> bool {
    a.start_hz < b.end_hz && b.start_hz < a.end_hz
}

// ------------------------------------------------------------------ select_receivers

/// A receiver the hub treats as offline because its tunable range overlaps
/// one with a lower ID (§5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlappingReceiver {
    /// The receiver treated as offline.
    pub receiver_id: u32,
    /// The receiver with the lower ID it overlaps, which the hub keeps.
    pub overlaps: u32,
}

/// Which receivers of a station's `Capabilities` the hub serves
/// ([`select_receivers`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiverSelection {
    /// The receivers to serve, in the order of `Capabilities.receivers`.
    pub serve: Vec<u32>,
    /// Receivers beyond the hub's limit, ignored, in ascending ID order.
    pub over_limit: Vec<u32>,
    /// Receivers treated as offline until their overlap ends, in ascending
    /// ID order, each with the receiver it overlaps (tell the owner why).
    pub overlapping: Vec<OverlappingReceiver>,
}

/// §5.1: the receivers the hub serves out of `receivers` (normally the
/// sanitised `Capabilities.receivers`), with the hub's limit `max_receivers`
/// (`[limits] max_receivers`, [`limits::DEFAULT_HUB_MAX_RECEIVERS`] by
/// default).
///
/// 1. **Limit.** Receivers beyond `max_receivers` are ignored, highest IDs
///    first.
/// 2. **Overlap.** In ascending ID order, a receiver whose tunable range
///    overlaps ([`ranges_overlap`]) one already kept is treated as offline;
///    the one with the lower ID stays. A receiver that overlaps only
///    receivers treated as offline is kept.
///
/// Overlap is not a violation (a front end can drift, or settle after a
/// retune). A receiver without a tunable range overlaps nothing. Duplicate
/// IDs (which [`super::sanitize_capabilities`] refuses) count once.
pub fn select_receivers(
    receivers: &[v1::ReceiverCapabilities],
    max_receivers: usize,
) -> ReceiverSelection {
    let mut ids: Vec<u32> = receivers.iter().map(|r| r.receiver_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let over_limit = ids.split_off(ids.len().min(max_receivers));

    let range_of = |id: u32| {
        receivers
            .iter()
            .find(|r| r.receiver_id == id)
            .and_then(|r| r.tunable_range)
            .unwrap_or_default()
    };
    let mut kept: Vec<(u32, v1::FrequencyRange)> = Vec::with_capacity(ids.len());
    let mut overlapping = Vec::new();
    for id in ids {
        let range = range_of(id);
        match kept.iter().find(|(_, k)| ranges_overlap(k, &range)) {
            Some(&(lower, _)) => overlapping.push(OverlappingReceiver {
                receiver_id: id,
                overlaps: lower,
            }),
            None => kept.push((id, range)),
        }
    }
    let mut serve = Vec::with_capacity(kept.len());
    for r in receivers {
        if kept.iter().any(|(id, _)| *id == r.receiver_id) && !serve.contains(&r.receiver_id) {
            serve.push(r.receiver_id);
        }
    }
    ReceiverSelection {
        serve,
        over_limit,
        overlapping,
    }
}

// ------------------------------------------------------------------ check_receiver_set

/// Why a `ConfigureReceivers` set breaks a rule of §4.2. Each names the
/// receiver (by `receiver_id`) and the rule; the `Display` text is what the
/// station puts in `Result.message` (always under
/// [`limits::MAX_DETAIL_BYTES`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReceiverSetError {
    /// More than [`limits::MAX_RECEIVERS`] receivers.
    #[error("{count} receivers, at most 16")]
    TooManyReceivers {
        /// Receivers in the set.
        count: usize,
    },
    /// A `receiver_id` of 0.
    #[error("receiver 0: receiver_id must not be 0")]
    ZeroReceiverId,
    /// A `receiver_id` twice in the set.
    #[error("receiver {receiver_id}: receiver_id appears twice")]
    DuplicateReceiverId {
        /// The receiver.
        receiver_id: u32,
    },
    /// An empty serial, but the station has more than one RTL-SDR.
    #[error(
        "receiver {receiver_id}: an empty serial needs one RTL-SDR at most, the station has {rtlsdrs}"
    )]
    EmptySerial {
        /// The receiver.
        receiver_id: u32,
        /// RTL-SDRs the station reports.
        rtlsdrs: usize,
    },
    /// Two receivers on one SDR, or on one serial shared by several SDRs.
    #[error("receiver {receiver_id}: its SDR is already receiver {other}'s")]
    SdrInUse {
        /// The receiver.
        receiver_id: u32,
        /// The receiver earlier in the set on the same serial.
        other: u32,
    },
    /// A model the station's table doesn't offer for that SDR, or, for an
    /// SDR not plugged in, a model not specified.
    #[error("receiver {receiver_id}: model not offered for its SDR")]
    ModelNotOffered {
        /// The receiver.
        receiver_id: u32,
    },
    /// A profile the station's table doesn't offer for that model, or one
    /// the station's `profiles` don't list.
    #[error("receiver {receiver_id}: profile not offered for its model")]
    ProfileNotOffered {
        /// The receiver.
        receiver_id: u32,
    },
    /// A centre outside the profile's centre range.
    #[error("receiver {receiver_id}: centre {center_hz} Hz outside the profile's centre range")]
    CenterOutOfRange {
        /// The receiver.
        receiver_id: u32,
        /// The centre asked for.
        center_hz: u64,
    },
    /// A front-end setting outside the profile's limits (or one the profile
    /// doesn't have).
    #[error("receiver {receiver_id}: {setting} outside the profile's front-end limits")]
    SettingOutOfRange {
        /// The receiver.
        receiver_id: u32,
        /// `gain_db`, `hardware_agc` or `attenuation_db`.
        setting: &'static str,
    },
    /// Two receivers' predicted tunable ranges overlap.
    #[error("receiver {receiver_id}: tunable range overlaps receiver {other}'s")]
    Overlap {
        /// The receiver.
        receiver_id: u32,
        /// The receiver earlier in the set it overlaps.
        other: u32,
    },
}

impl ReceiverSetError {
    /// The receiver the error names, if it names one.
    pub fn receiver_id(&self) -> Option<u32> {
        match self {
            Self::TooManyReceivers { .. } => None,
            Self::ZeroReceiverId => Some(0),
            Self::DuplicateReceiverId { receiver_id }
            | Self::EmptySerial { receiver_id, .. }
            | Self::SdrInUse { receiver_id, .. }
            | Self::ModelNotOffered { receiver_id }
            | Self::ProfileNotOffered { receiver_id }
            | Self::CenterOutOfRange { receiver_id, .. }
            | Self::SettingOutOfRange { receiver_id, .. }
            | Self::Overlap { receiver_id, .. } => Some(*receiver_id),
        }
    }

    /// The other receiver of a two-receiver rule ([`Self::SdrInUse`],
    /// [`Self::Overlap`]).
    pub fn other_receiver_id(&self) -> Option<u32> {
        match self {
            Self::SdrInUse { other, .. } | Self::Overlap { other, .. } => Some(*other),
            _ => None,
        }
    }

    /// Stable `snake_case` name of the rule, for API errors and logs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooManyReceivers { .. } => "too_many_receivers",
            Self::ZeroReceiverId => "zero_receiver_id",
            Self::DuplicateReceiverId { .. } => "duplicate_receiver_id",
            Self::EmptySerial { .. } => "empty_serial",
            Self::SdrInUse { .. } => "sdr_in_use",
            Self::ModelNotOffered { .. } => "model_not_offered",
            Self::ProfileNotOffered { .. } => "profile_not_offered",
            Self::CenterOutOfRange { .. } => "center_out_of_range",
            Self::SettingOutOfRange { .. } => "setting_out_of_range",
            Self::Overlap { .. } => "receivers_overlap",
        }
    }

    /// The result code the station answers with (§4):
    /// `PERMISSION_DENIED` for a model or profile its table doesn't allow,
    /// `INVALID_ARGUMENT` otherwise.
    pub fn result_code(&self) -> v1::ResultCode {
        match self {
            Self::ModelNotOffered { .. } | Self::ProfileNotOffered { .. } => {
                v1::ResultCode::PermissionDenied
            }
            _ => v1::ResultCode::InvalidArgument,
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

/// Whether `model` is an RTL-SDR model. Every model of protocol 2.0 is; the
/// empty-serial rule of §4.2 counts RTL-SDRs only.
fn is_rtlsdr_model(model: i32) -> bool {
    matches!(
        v1::SdrModel::try_from(model),
        Ok(v1::SdrModel::RtlsdrBlogV4
            | v1::SdrModel::RtlsdrDirectSampling
            | v1::SdrModel::RtlsdrOther)
    )
}

fn is_rtlsdr(sdr: &v1::Sdr) -> bool {
    sdr.models.iter().any(|m| is_rtlsdr_model(m.model))
}

/// Whether `model` is a model of the protocol, not `UNSPECIFIED`.
fn is_specified_model(model: i32) -> bool {
    v1::SdrModel::try_from(model).is_ok_and(|m| m != v1::SdrModel::Unspecified)
}

/// §4.2: checks the whole receiver set `set` against the station's
/// `hardware` report, before anything changes. The first broken rule, in
/// set order, is returned:
///
/// - at most [`limits::MAX_RECEIVERS`] receivers, each `receiver_id` not 0
///   and unique;
/// - an empty `sdr_serial` only when the station has at most one RTL-SDR
///   (the empty serial then means that SDR, or the one plugged in later);
///   no two receivers on one serial, so an SDR whose serial another SDR
///   shares serves one receiver at most;
/// - for a receiver whose SDR is plugged in, its `model` one the table
///   offers for that SDR (for every SDR with that serial), and its
///   `profile` one the table offers for that model and `profiles` lists
///   (`PERMISSION_DENIED`);
/// - for a receiver whose SDR is not plugged in (a serial the station
///   doesn't report, or an empty one with no RTL-SDR), a specified `model`
///   and a `profile` that `profiles` lists (`PERMISSION_DENIED`): the
///   station runs it `NO_DEVICE`, and checks the model against the SDR when
///   it appears;
/// - each centre inside the profile's `center_range`, and the front-end
///   settings inside the profile's `frontend`: a gain only with `has_gain`
///   and within `min_gain_db..=max_gain_db`, hardware AGC on only with
///   `has_hardware_agc`, an attenuation only with `has_attenuator` and
///   within `0..=max_attenuation_db`;
/// - no two receivers' predicted tunable ranges
///   ([`predicted_tunable_range`]) overlapping ([`ranges_overlap`]).
///
/// `hardware` is the station's own report, or one the hub accepted (§5
/// `Hardware`). The function is total: any input gives an answer.
pub fn check_receiver_set(
    set: &v1::ConfigureReceivers,
    hardware: &v1::Hardware,
) -> Result<(), ReceiverSetError> {
    if set.receivers.len() > limits::MAX_RECEIVERS {
        return Err(ReceiverSetError::TooManyReceivers {
            count: set.receivers.len(),
        });
    }
    let rtlsdrs: Vec<&v1::Sdr> = hardware.sdrs.iter().filter(|s| is_rtlsdr(s)).collect();
    let mut ids = HashSet::with_capacity(set.receivers.len());
    // (serial the receiver resolves to, receiver_id)
    let mut serials: Vec<(&str, u32)> = Vec::with_capacity(set.receivers.len());
    let mut ranges: Vec<(u32, v1::FrequencyRange)> = Vec::with_capacity(set.receivers.len());
    for rx in &set.receivers {
        let receiver_id = rx.receiver_id;
        if receiver_id == 0 {
            return Err(ReceiverSetError::ZeroReceiverId);
        }
        if !ids.insert(receiver_id) {
            return Err(ReceiverSetError::DuplicateReceiverId { receiver_id });
        }

        // The serial the receiver resolves to, and the SDRs with it (none:
        // its SDR is not plugged in).
        let (serial, sdrs): (&str, Vec<&v1::Sdr>) = if rx.sdr_serial.is_empty() {
            match rtlsdrs.as_slice() {
                [] => ("", Vec::new()),
                [only] => (only.serial.as_str(), vec![*only]),
                _ => {
                    return Err(ReceiverSetError::EmptySerial {
                        receiver_id,
                        rtlsdrs: rtlsdrs.len(),
                    });
                }
            }
        } else {
            let serial = rx.sdr_serial.as_str();
            let sdrs = hardware
                .sdrs
                .iter()
                .filter(|s| s.serial == serial)
                .collect();
            (serial, sdrs)
        };
        if let Some(&(_, other)) = serials.iter().find(|(s, _)| *s == serial) {
            return Err(ReceiverSetError::SdrInUse { receiver_id, other });
        }
        serials.push((serial, receiver_id));

        if sdrs.is_empty() {
            if !is_specified_model(rx.model) {
                return Err(ReceiverSetError::ModelNotOffered { receiver_id });
            }
        } else {
            let offered = |sdr: &v1::Sdr| sdr.models.iter().find(|m| m.model == rx.model).cloned();
            let mut options = Vec::with_capacity(sdrs.len());
            for sdr in &sdrs {
                let Some(option) = offered(sdr) else {
                    return Err(ReceiverSetError::ModelNotOffered { receiver_id });
                };
                options.push(option);
            }
            if !options.iter().all(|o| o.profiles.contains(&rx.profile)) {
                return Err(ReceiverSetError::ProfileNotOffered { receiver_id });
            }
        }
        if rx.profile == 0 {
            return Err(ReceiverSetError::ProfileNotOffered { receiver_id });
        }
        let Some(profile) = hardware.profiles.iter().find(|p| p.profile == rx.profile) else {
            return Err(ReceiverSetError::ProfileNotOffered { receiver_id });
        };

        let center_hz = rx.center_frequency_hz;
        let inside = profile
            .center_range
            .is_some_and(|r| r.start_hz <= center_hz && center_hz <= r.end_hz);
        if !inside {
            return Err(ReceiverSetError::CenterOutOfRange {
                receiver_id,
                center_hz,
            });
        }
        check_settings(rx, &profile.frontend.clone().unwrap_or_default())?;

        let range = predicted_tunable_range(center_hz, profile.tunable_span_hz);
        if let Some(&(other, _)) = ranges.iter().find(|(_, r)| ranges_overlap(r, &range)) {
            return Err(ReceiverSetError::Overlap { receiver_id, other });
        }
        ranges.push((receiver_id, range));
    }
    Ok(())
}

fn check_settings(
    rx: &v1::ReceiverConfig,
    controls: &v1::FrontendControls,
) -> Result<(), ReceiverSetError> {
    let receiver_id = rx.receiver_id;
    let out = |setting| ReceiverSetError::SettingOutOfRange {
        receiver_id,
        setting,
    };
    if let Some(gain) = rx.gain_db {
        let ok = controls.has_gain
            && gain.is_finite()
            && controls.min_gain_db <= gain
            && gain <= controls.max_gain_db;
        if !ok {
            return Err(out("gain_db"));
        }
    }
    if rx.hardware_agc == Some(true) && !controls.has_hardware_agc {
        return Err(out("hardware_agc"));
    }
    if let Some(att) = rx.attenuation_db {
        let ok = controls.has_attenuator
            && att.is_finite()
            && 0.0 <= att
            && att <= controls.max_attenuation_db;
        if !ok {
            return Err(out("attenuation_db"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ hub-side, stateless

/// The field names of one `FrontendControls` context, for
/// [`check_frontend_controls`].
pub(crate) struct ControlsFields {
    pub(crate) min_gain_db: &'static str,
    pub(crate) max_gain_db: &'static str,
    pub(crate) max_attenuation_db: &'static str,
    pub(crate) antenna_ports: &'static str,
}

/// §5: a `FrontendControls`' floats finite and its antenna labels within
/// the caps.
pub(crate) fn check_frontend_controls(
    f: &v1::FrontendControls,
    fields: &ControlsFields,
) -> Result<(), ViolationKind> {
    use super::fields::check_finite;
    check_finite(fields.min_gain_db, f.min_gain_db)?;
    check_finite(fields.max_gain_db, f.max_gain_db)?;
    check_finite(fields.max_attenuation_db, f.max_attenuation_db)?;
    if f.antenna_ports.len() > limits::MAX_ANTENNA_PORTS {
        return Err(ViolationKind::TooManyEntries {
            field: fields.antenna_ports,
            len: f.antenna_ports.len(),
            max: limits::MAX_ANTENNA_PORTS,
        });
    }
    for port in &f.antenna_ports {
        check_len(fields.antenna_ports, port, limits::MAX_ANTENNA_PORT_BYTES)?;
    }
    Ok(())
}

const PROFILE_CONTROLS: ControlsFields = ControlsFields {
    min_gain_db: "hardware.profiles.frontend.min_gain_db",
    max_gain_db: "hardware.profiles.frontend.max_gain_db",
    max_attenuation_db: "hardware.profiles.frontend.max_attenuation_db",
    antenna_ports: "hardware.profiles.frontend.antenna_ports",
};

/// §5 `Hardware`, without the session: at most [`limits::MAX_SDRS`] SDRs;
/// the `Sdr` strings within their caps; every model and profile a known,
/// specified value; `profiles` (every profile of the station's package,
/// plugged in or not) listing each profile once, and every profile an SDR
/// offers among them; each `center_range` present, ordered and inside 6 GHz
/// with `default_center_hz` inside it; finite front-end floats; a
/// `receiver_id`, when present, not 0. Whether a `receiver_id` is one the
/// hub configured is the session's check.
pub(crate) fn check_hardware(h: &v1::Hardware, minor: u32) -> Result<(), ViolationKind> {
    if h.sdrs.len() > limits::MAX_SDRS {
        return Err(ViolationKind::TooManyEntries {
            field: "hardware.sdrs",
            len: h.sdrs.len(),
            max: limits::MAX_SDRS,
        });
    }
    let mut listed = HashSet::with_capacity(h.profiles.len());
    for p in &h.profiles {
        check_enum::<v1::SdrProfile>("hardware.profiles.profile", p.profile, minor, true)?;
        if !listed.insert(p.profile) {
            return Err(super::fields::bounds("hardware.profiles.profile"));
        }
        let range = p
            .center_range
            .as_ref()
            .ok_or(missing("hardware.profiles.center_range"))?;
        check_range("hardware.profiles.center_range", range)?;
        if !(range.start_hz..=range.end_hz).contains(&p.default_center_hz) {
            return Err(super::fields::bounds("hardware.profiles.default_center_hz"));
        }
        if let Some(f) = &p.frontend {
            check_frontend_controls(f, &PROFILE_CONTROLS)?;
        }
    }
    for sdr in &h.sdrs {
        check_len(
            "hardware.sdrs.manufacturer",
            &sdr.manufacturer,
            limits::MAX_SDR_STRING_BYTES,
        )?;
        check_len(
            "hardware.sdrs.product",
            &sdr.product,
            limits::MAX_SDR_STRING_BYTES,
        )?;
        check_len(
            "hardware.sdrs.serial",
            &sdr.serial,
            limits::MAX_SDR_STRING_BYTES,
        )?;
        check_len("hardware.sdrs.port", &sdr.port, limits::MAX_SDR_PORT_BYTES)?;
        for m in &sdr.models {
            check_enum::<v1::SdrModel>("hardware.sdrs.models.model", m.model, minor, true)?;
            for &p in &m.profiles {
                check_enum::<v1::SdrProfile>("hardware.sdrs.models.profiles", p, minor, true)?;
                if !listed.contains(&p) {
                    return Err(missing("hardware.profiles"));
                }
            }
        }
        if sdr.receiver_id == Some(0) {
            return Err(ViolationKind::UnknownReceiver {
                kind: MessageKind::Response,
                receiver_id: 0,
            });
        }
    }
    Ok(())
}

/// §5 `ReceiversConfigured`, without the request: at most
/// [`limits::MAX_RECEIVERS`] receivers, each with a specified state and a
/// `detail` within [`limits::MAX_DETAIL_BYTES`]. That it holds each receiver
/// of the set once, and nothing else, is the session's check.
pub(crate) fn check_receivers_configured(
    r: &v1::ReceiversConfigured,
    minor: u32,
) -> Result<(), ViolationKind> {
    if r.receivers.len() > limits::MAX_RECEIVERS {
        return Err(ViolationKind::TooManyEntries {
            field: "receivers_configured.receivers",
            len: r.receivers.len(),
            max: limits::MAX_RECEIVERS,
        });
    }
    for rx in &r.receivers {
        check_enum::<v1::ReceiverState>(
            "receivers_configured.receivers.state",
            rx.state,
            minor,
            true,
        )?;
        check_len(
            "receivers_configured.receivers.detail",
            &rx.detail,
            limits::MAX_DETAIL_BYTES,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start_hz: u64, end_hz: u64) -> v1::FrequencyRange {
        v1::FrequencyRange { start_hz, end_hz }
    }

    fn caps(receiver_id: u32, start_hz: u64, end_hz: u64) -> v1::ReceiverCapabilities {
        v1::ReceiverCapabilities {
            receiver_id,
            tunable_range: Some(range(start_hz, end_hz)),
            hardware_range: Some(range(0, 1_766_000_000)),
            ..Default::default()
        }
    }

    #[test]
    fn prediction_and_overlap() {
        assert_eq!(
            predicted_tunable_range(7_100_000, 2_048_000),
            range(6_076_000, 8_124_000)
        );
        assert_eq!(predicted_tunable_range(1_000, 4_000), range(0, 3_000));
        assert_eq!(
            predicted_tunable_range(u64::MAX, 10),
            range(u64::MAX - 5, u64::MAX)
        );
        assert!(ranges_overlap(&range(0, 10), &range(5, 15)));
        assert!(ranges_overlap(&range(5, 15), &range(0, 10)));
        assert!(ranges_overlap(&range(0, 100), &range(10, 20)));
        // Touching is not overlapping.
        assert!(!ranges_overlap(&range(0, 10), &range(10, 20)));
        assert!(!ranges_overlap(&range(0, 10), &range(11, 20)));
        assert!(!ranges_overlap(&range(0, 0), &range(0, 0)));
    }

    #[test]
    fn selection_keeps_the_lower_id_of_an_overlap() {
        let rx = [
            caps(3, 14_000_000, 16_000_000),
            caps(1, 6_000_000, 8_000_000),
            caps(2, 7_500_000, 9_500_000),
        ];
        let s = select_receivers(&rx, 6);
        assert_eq!(s.serve, vec![3, 1]);
        assert!(s.over_limit.is_empty());
        assert_eq!(
            s.overlapping,
            vec![OverlappingReceiver {
                receiver_id: 2,
                overlaps: 1
            }]
        );
    }

    #[test]
    fn selection_overlap_is_against_kept_receivers_only() {
        // 1 overlaps 2, 2 overlaps 3, 1 and 3 apart: 2 goes, 3 stays.
        let rx = [
            caps(1, 0, 10),
            caps(2, 5, 15),
            caps(3, 12, 20),
            caps(4, 0, 30),
        ];
        let s = select_receivers(&rx, 16);
        assert_eq!(s.serve, vec![1, 3]);
        assert_eq!(
            s.overlapping,
            vec![
                OverlappingReceiver {
                    receiver_id: 2,
                    overlaps: 1
                },
                OverlappingReceiver {
                    receiver_id: 4,
                    overlaps: 1
                },
            ]
        );
    }

    #[test]
    fn selection_clamps_highest_ids_first() {
        let rx: Vec<_> = [9u32, 2, 7, 4, 1, 8, 3, 5]
            .iter()
            .map(|&id| caps(id, u64::from(id) * 10, u64::from(id) * 10 + 5))
            .collect();
        let s = select_receivers(&rx, 6);
        assert_eq!(s.serve, vec![2, 7, 4, 1, 3, 5]);
        assert_eq!(s.over_limit, vec![8, 9]);
        assert!(s.overlapping.is_empty());
        // The limit applies before the overlap rule: an ignored receiver
        // takes nobody offline.
        let rx = [caps(1, 0, 10), caps(2, 100, 200), caps(3, 150, 250)];
        let s = select_receivers(&rx, 2);
        assert_eq!(s.serve, vec![1, 2]);
        assert_eq!(s.over_limit, vec![3]);
        assert!(s.overlapping.is_empty());
        let s = select_receivers(&rx, 0);
        assert!(s.serve.is_empty());
        assert_eq!(s.over_limit, vec![1, 2, 3]);
    }

    #[test]
    fn selection_is_total() {
        let s = select_receivers(&[], 6);
        assert_eq!(s, ReceiverSelection::default());
        // Missing ranges overlap nothing; duplicates count once.
        let rx = [
            v1::ReceiverCapabilities {
                receiver_id: 5,
                ..Default::default()
            },
            caps(5, 0, 10),
            caps(6, 0, 10),
        ];
        let s = select_receivers(&rx, 6);
        assert_eq!(s.serve, vec![5, 6]);
    }

    #[test]
    fn error_metadata() {
        let e = ReceiverSetError::Overlap {
            receiver_id: 4,
            other: 2,
        };
        assert_eq!(e.receiver_id(), Some(4));
        assert_eq!(e.other_receiver_id(), Some(2));
        assert_eq!(e.code(), "receivers_overlap");
        assert_eq!(e.result_code(), v1::ResultCode::InvalidArgument);
        assert_eq!(
            e.to_result().message,
            "receiver 4: tunable range overlaps receiver 2's"
        );
        let e = ReceiverSetError::ProfileNotOffered { receiver_id: 1 };
        assert_eq!(e.result_code(), v1::ResultCode::PermissionDenied);
        assert_eq!(e.other_receiver_id(), None);
        let e = ReceiverSetError::TooManyReceivers { count: 17 };
        assert_eq!(e.receiver_id(), None);
        // Every message fits Result.message, even with the largest numbers.
        let longest = [
            ReceiverSetError::EmptySerial {
                receiver_id: u32::MAX,
                rtlsdrs: usize::MAX,
            },
            ReceiverSetError::CenterOutOfRange {
                receiver_id: u32::MAX,
                center_hz: u64::MAX,
            },
            ReceiverSetError::SettingOutOfRange {
                receiver_id: u32::MAX,
                setting: "attenuation_db",
            },
            ReceiverSetError::TooManyReceivers { count: usize::MAX },
        ];
        for e in longest {
            assert!(e.to_string().len() <= limits::MAX_DETAIL_BYTES);
        }
    }
}
