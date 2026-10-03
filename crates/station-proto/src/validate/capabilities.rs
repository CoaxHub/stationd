// SPDX-License-Identifier: MIT
//! §5 `Capabilities`: nonsense is a violation, excess is clamped to hub caps.

use super::ViolationKind;
use super::fields::{check_enum, check_finite, check_len, missing};
use crate::limits;
use crate::v1;

/// Capabilities after [`sanitize_capabilities`]: clamped to the hub's global
/// caps, plus the fields that were clamped (for logs).
///
/// These are still claims: the hub schedules against
/// `min(capability, owner setting, plan limit, hub cap)` (§5).
#[derive(Debug, Clone, PartialEq)]
pub struct SanitizedCapabilities {
    /// The clamped capabilities.
    pub capabilities: v1::Capabilities,
    /// Field paths that were reduced to a hub cap.
    pub clamped: Vec<&'static str>,
}

/// Validates station capabilities (§5 `Capabilities`) and clamps them to the
/// hub's global caps.
///
/// Violations ("nonsense"):
/// - `tunable_range` or `hardware_range` missing, `start_hz > end_hz`, or a
///   range starting above [`limits::MAX_FREQUENCY_HZ`];
/// - `tunable_range` not inside `hardware_range`;
/// - enum lists with unknown or `UNSPECIFIED` entries;
/// - non-finite floats, too many or too long antenna labels, `sdr_model` too
///   long.
///
/// Clamped (not a violation): range ends above 6 GHz (to 6 GHz),
/// `max_channels` above [`limits::MAX_CHANNELS`], `waterfall.max_bins` above
/// [`limits::MAX_BINS`], `waterfall.max_subscriptions` above
/// [`limits::MAX_WATERFALL_SUBSCRIPTIONS`], `measurements.max_noise_bands`
/// above [`limits::MAX_NOISE_BANDS`] (§5.2). A 1.0 session drops
/// `measurements` (a newer field it does not know).
pub fn sanitize_capabilities(
    caps: &v1::Capabilities,
    minor: u32,
) -> Result<SanitizedCapabilities, ViolationKind> {
    let mut out = caps.clone();
    let mut clamped = Vec::new();

    let hw = caps
        .hardware_range
        .as_ref()
        .ok_or(missing("capabilities.hardware_range"))?;
    let tun = caps
        .tunable_range
        .as_ref()
        .ok_or(missing("capabilities.tunable_range"))?;
    check_nonsense("capabilities.hardware_range", hw)?;
    check_nonsense("capabilities.tunable_range", tun)?;
    if tun.start_hz < hw.start_hz || tun.end_hz > hw.end_hz {
        return Err(ViolationKind::TunableOutsideHardware);
    }
    if let Some(r) = out.hardware_range.as_mut() {
        clamp(
            &mut r.end_hz,
            limits::MAX_FREQUENCY_HZ,
            "capabilities.hardware_range.end_hz",
            &mut clamped,
        );
    }
    if let Some(r) = out.tunable_range.as_mut() {
        clamp(
            &mut r.end_hz,
            limits::MAX_FREQUENCY_HZ,
            "capabilities.tunable_range.end_hz",
            &mut clamped,
        );
    }

    clamp(
        &mut out.max_channels,
        limits::MAX_CHANNELS,
        "capabilities.max_channels",
        &mut clamped,
    );
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

    for &m in &caps.modes {
        check_enum::<v1::Mode>("capabilities.modes", m, minor, true)?;
    }
    for &c in &caps.codecs {
        check_enum::<v1::AudioCodec>("capabilities.codecs", c, minor, true)?;
    }
    for &k in &caps.allowed_admin_ops {
        check_enum::<v1::AdminOpKind>("capabilities.allowed_admin_ops", k, minor, true)?;
    }

    if let Some(f) = &caps.frontend {
        check_finite("capabilities.frontend.min_gain_db", f.min_gain_db)?;
        check_finite("capabilities.frontend.max_gain_db", f.max_gain_db)?;
        check_finite(
            "capabilities.frontend.max_attenuation_db",
            f.max_attenuation_db,
        )?;
        if f.antenna_ports.len() > limits::MAX_ANTENNA_PORTS {
            return Err(ViolationKind::TooManyEntries {
                field: "capabilities.frontend.antenna_ports",
                len: f.antenna_ports.len(),
                max: limits::MAX_ANTENNA_PORTS,
            });
        }
        for port in &f.antenna_ports {
            check_len(
                "capabilities.frontend.antenna_ports",
                port,
                limits::MAX_ANTENNA_PORT_BYTES,
            )?;
        }
    }
    check_len(
        "capabilities.sdr_model",
        &caps.sdr_model,
        limits::MAX_SDR_MODEL_BYTES,
    )?;
    clamp_measurements(&mut out, minor, &mut clamped);

    Ok(SanitizedCapabilities {
        capabilities: out,
        clamped,
    })
}

/// 1.1: a 1.0 session ignores `measurements` (§3); a 1.1 one clamps it.
fn clamp_measurements(out: &mut v1::Capabilities, minor: u32, clamped: &mut Vec<&'static str>) {
    if minor < limits::MEASUREMENTS_MINOR {
        out.measurements = None;
    } else if let Some(m) = out.measurements.as_mut() {
        clamp(
            &mut m.max_noise_bands,
            limits::MAX_NOISE_BANDS,
            "capabilities.measurements.max_noise_bands",
            clamped,
        );
    }
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
        clamped.push(field);
    }
}
