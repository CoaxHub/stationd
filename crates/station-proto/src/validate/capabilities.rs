// SPDX-License-Identifier: MIT
//! §5 `Capabilities`: nonsense is a violation, excess is clamped to hub caps
//! (§5.1). Which receivers the hub serves is [`super::select_receivers`].

use std::collections::HashSet;

use super::fields::{check_enum, check_len, missing};
use super::receivers::{ControlsFields, check_frontend_controls};
use super::{MessageKind, ViolationKind};
use crate::limits;
use crate::v1;

/// Capabilities after [`sanitize_capabilities`]: clamped to the hub's global
/// caps, plus the fields that were clamped (for logs).
///
/// These are still claims: the hub schedules against
/// `min(capability, owner setting, hub policy)` (§4). Overlapping receivers
/// and receivers beyond the hub's limit are still in it; see
/// [`super::select_receivers`].
#[derive(Debug, Clone, PartialEq)]
pub struct SanitizedCapabilities {
    /// The clamped capabilities.
    pub capabilities: v1::Capabilities,
    /// Field paths that were reduced to a hub cap, each once.
    pub clamped: Vec<&'static str>,
}

const RECEIVER_CONTROLS: ControlsFields = ControlsFields {
    min_gain_db: "capabilities.receivers.frontend.min_gain_db",
    max_gain_db: "capabilities.receivers.frontend.max_gain_db",
    max_attenuation_db: "capabilities.receivers.frontend.max_attenuation_db",
    antenna_ports: "capabilities.receivers.frontend.antenna_ports",
};

/// Validates station capabilities (§5 `Capabilities`, §5.1) and clamps them
/// to the hub's global caps.
///
/// Violations ("nonsense"):
/// - more than [`limits::MAX_RECEIVERS`] receivers, a `receiver_id` twice,
///   or a `receiver_id` of 0 (never configured);
/// - a receiver's `tunable_range` or `hardware_range` missing,
///   `start_hz > end_hz`, or a range starting above
///   [`limits::MAX_FREQUENCY_HZ`];
/// - a `tunable_range` not inside its `hardware_range`;
/// - enum lists with unknown or `UNSPECIFIED` entries;
/// - non-finite floats, too many or too long antenna labels, `sdr_model` too
///   long.
///
/// Clamped (not a violation): range ends above 6 GHz (to 6 GHz),
/// `max_channels` above [`limits::MAX_CHANNELS`], `waterfall.max_bins` above
/// [`limits::MAX_BINS`], `waterfall.max_subscriptions` above
/// [`limits::MAX_WATERFALL_SUBSCRIPTIONS`], `measurements.max_noise_bands`
/// above [`limits::MAX_NOISE_BANDS`] (§5.2).
///
/// Whether each receiver is one the hub configured is the session's check
/// ([`super::Session`]); overlap is not a violation (§5.1).
pub fn sanitize_capabilities(
    caps: &v1::Capabilities,
    minor: u32,
) -> Result<SanitizedCapabilities, ViolationKind> {
    let mut out = caps.clone();
    let mut clamped = Vec::new();

    if caps.receivers.len() > limits::MAX_RECEIVERS {
        return Err(ViolationKind::TooManyEntries {
            field: "capabilities.receivers",
            len: caps.receivers.len(),
            max: limits::MAX_RECEIVERS,
        });
    }
    let mut ids = HashSet::with_capacity(caps.receivers.len());
    for rx in &caps.receivers {
        check_receiver(rx, &mut ids)?;
    }
    for rx in &mut out.receivers {
        clamp_receiver(rx, &mut clamped);
    }

    for &m in &caps.modes {
        check_enum::<v1::Mode>("capabilities.modes", m, minor, true)?;
    }
    for &c in &caps.codecs {
        check_enum::<v1::AudioCodec>("capabilities.codecs", c, minor, true)?;
    }
    if let Some(w) = out.waterfall.as_mut() {
        clamp(
            &mut w.max_bins,
            limits::MAX_BINS,
            "capabilities.waterfall.max_bins",
            &mut clamped,
        );
        clamp(
            &mut w.max_subscriptions,
            limits::MAX_WATERFALL_SUBSCRIPTIONS,
            "capabilities.waterfall.max_subscriptions",
            &mut clamped,
        );
    }
    if let Some(m) = out.measurements.as_mut() {
        clamp(
            &mut m.max_noise_bands,
            limits::MAX_NOISE_BANDS,
            "capabilities.measurements.max_noise_bands",
            &mut clamped,
        );
    }

    Ok(SanitizedCapabilities {
        capabilities: out,
        clamped,
    })
}

fn check_receiver(
    rx: &v1::ReceiverCapabilities,
    ids: &mut HashSet<u32>,
) -> Result<(), ViolationKind> {
    let receiver_id = rx.receiver_id;
    if receiver_id == 0 {
        return Err(ViolationKind::UnknownReceiver {
            kind: MessageKind::Response,
            receiver_id,
        });
    }
    if !ids.insert(receiver_id) {
        return Err(ViolationKind::DuplicateReceiver {
            field: "capabilities.receivers.receiver_id",
            receiver_id,
        });
    }
    let hw = rx
        .hardware_range
        .as_ref()
        .ok_or(missing("capabilities.receivers.hardware_range"))?;
    let tun = rx
        .tunable_range
        .as_ref()
        .ok_or(missing("capabilities.receivers.tunable_range"))?;
    check_nonsense("capabilities.receivers.hardware_range", hw)?;
    check_nonsense("capabilities.receivers.tunable_range", tun)?;
    if tun.start_hz < hw.start_hz || tun.end_hz > hw.end_hz {
        return Err(ViolationKind::TunableOutsideHardware { receiver_id });
    }
    if let Some(f) = &rx.frontend {
        check_frontend_controls(f, &RECEIVER_CONTROLS)?;
    }
    check_len(
        "capabilities.receivers.sdr_model",
        &rx.sdr_model,
        limits::MAX_SDR_MODEL_BYTES,
    )
}

fn clamp_receiver(rx: &mut v1::ReceiverCapabilities, clamped: &mut Vec<&'static str>) {
    if let Some(r) = rx.hardware_range.as_mut() {
        clamp(
            &mut r.end_hz,
            limits::MAX_FREQUENCY_HZ,
            "capabilities.receivers.hardware_range.end_hz",
            clamped,
        );
    }
    if let Some(r) = rx.tunable_range.as_mut() {
        clamp(
            &mut r.end_hz,
            limits::MAX_FREQUENCY_HZ,
            "capabilities.receivers.tunable_range.end_hz",
            clamped,
        );
    }
    clamp(
        &mut rx.max_channels,
        limits::MAX_CHANNELS,
        "capabilities.receivers.max_channels",
        clamped,
    );
}

/// A range is nonsense if it is reversed or starts above 6 GHz (nothing of it
/// is usable). An end above 6 GHz is clamped instead.
fn check_nonsense(field: &'static str, r: &v1::FrequencyRange) -> Result<(), ViolationKind> {
    if r.start_hz > r.end_hz || r.start_hz > limits::MAX_FREQUENCY_HZ {
        return Err(ViolationKind::InvalidRange { field });
    }
    Ok(())
}

fn clamp<T: PartialOrd + Copy>(
    v: &mut T,
    max: T,
    field: &'static str,
    clamped: &mut Vec<&'static str>,
) {
    if *v > max {
        *v = max;
        if !clamped.contains(&field) {
            clamped.push(field);
        }
    }
}
