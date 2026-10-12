// SPDX-License-Identifier: GPL-3.0-only
//! The SDR profiles of the package (station-protocol §4.2, §9).
//!
//! A profile is one way of using an SDR: a radiod template
//! (`<templates>/<name>.conf`, rendered per receiver by `stationd
//! backend-config`) and what a receiver of it can do. The hub chooses a
//! profile per receiver, among those the SDR table ([`crate::hardware`])
//! offers for its SDR; `stationd` reports each offered profile's
//! [`ProfileInfo`](v1::ProfileInfo) so the hub can predict a receiver's
//! tunable range before configuring it.
//!
//! | Profile | Receiver | Centre | Gain |
//! |---|---|---|---|
//! | `rtlsdr` | an RTL-SDR on VHF/UHF, through its tuner | 24–1766 MHz, 98 MHz by default | 0–49.6 dB, 30 by default |
//! | `rtlsdr-hf` | an RTL-SDR by direct sampling (Q branch), such as a Blog V3, on HF; the tuner is bypassed | 0.5–28.8 MHz, 7.1 MHz by default | none |
//! | `rtlsdr-v4` | an RTL-SDR Blog V4, HF through its upconverter and VHF/UHF | 0.5–1766 MHz, 7.1 MHz by default | 0–49.6 dB, 30 by default |
//! | `siggen` | radiod's signal generator (development builds only; never offered to the hub) | none | none |
//!
//! Every RTL-SDR profile samples at 2.048 MS/s complex, so a receiver's
//! tunable range is its centre ± 1.024 MHz. Gain is the only front-end
//! setting a receiver takes, and only in its configuration: the pinned
//! radiod changes nothing but the centre while running (§9 `SetFrontend`).

use std::fmt;

use serde::{Deserialize, Serialize};
use station_proto::v1;

/// Which profile: the name in `receivers.json` and the template's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileId {
    /// `rtlsdr`.
    Rtlsdr,
    /// `rtlsdr-hf`.
    RtlsdrHf,
    /// `rtlsdr-v4`.
    RtlsdrV4,
    /// `siggen` (development builds only).
    Siggen,
}

impl ProfileId {
    /// Every profile, production ones first.
    pub const ALL: [Self; 4] = [Self::Rtlsdr, Self::RtlsdrHf, Self::RtlsdrV4, Self::Siggen];

    /// The profile's definition.
    pub fn profile(self) -> &'static Profile {
        match self {
            Self::Rtlsdr => &RTLSDR,
            Self::RtlsdrHf => &RTLSDR_HF,
            Self::RtlsdrV4 => &RTLSDR_V4,
            Self::Siggen => &SIGGEN,
        }
    }

    /// The profile a hub names, if this package has it.
    pub fn from_proto(profile: v1::SdrProfile) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.profile().proto == Some(profile))
    }

    /// Whether a build of `packaged` flavour has this profile.
    pub fn in_build(self, packaged: bool) -> bool {
        !(packaged && self.profile().dev_only)
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.profile().name)
    }
}

/// The centre frequencies a profile accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CenterRange {
    /// Lowest accepted centre, Hz.
    pub min_hz: u64,
    /// Highest accepted centre, Hz.
    pub max_hz: u64,
    /// The centre when the owner chooses none, Hz.
    pub default_hz: u64,
}

impl CenterRange {
    /// Whether `hz` is accepted.
    pub fn contains(&self, hz: u64) -> bool {
        (self.min_hz..=self.max_hz).contains(&hz)
    }
}

/// The tuner gain a profile accepts in a receiver's configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainRange {
    /// Lowest, dB.
    pub min_db: f32,
    /// Highest, dB.
    pub max_db: f32,
    /// The template's gain when the owner chooses none, dB.
    pub default_db: f32,
}

/// One SDR profile.
#[derive(Debug, PartialEq)]
pub struct Profile {
    /// The template file stem (`rtlsdr-hf`).
    pub name: &'static str,
    /// The protocol's name for it; `None` for a development profile the hub
    /// never sees.
    pub proto: Option<v1::SdrProfile>,
    /// For humans.
    pub description: &'static str,
    /// radiod's front-end description in the template, which radiod
    /// reports as the receiver's `sdr_model`.
    pub sdr_model: &'static str,
    /// The centres it accepts; `None`: a front end with a fixed LO.
    pub center: Option<CenterRange>,
    /// Complex samples per second.
    pub sample_rate_hz: u32,
    /// The width of the tunable range around the centre, Hz.
    pub tunable_span_hz: u32,
    /// The gain it accepts; `None`: no gain in the signal path.
    pub gain: Option<GainRange>,
    /// Development builds only (no hardware).
    pub dev_only: bool,
}

/// The RTL-SDR's sample rate in every profile: 2.048 MS/s complex.
pub const RTLSDR_SAMPLE_RATE_HZ: u32 = 2_048_000;

/// The R820T/R828D tuner's gains, dB.
const TUNER_GAIN: GainRange = GainRange {
    min_db: 0.0,
    max_db: 49.6,
    default_db: 30.0,
};

/// `rtlsdr`.
pub const RTLSDR: Profile = Profile {
    name: "rtlsdr",
    proto: Some(v1::SdrProfile::Rtlsdr),
    description: "RTL-SDR (RTL2832U + R820T/R828D) on VHF/UHF, through its tuner",
    sdr_model: "coaxhub rtl-sdr",
    // The R820T's range; the default is the FM broadcast band.
    center: Some(CenterRange {
        min_hz: 24_000_000,
        max_hz: 1_766_000_000,
        default_hz: 98_000_000,
    }),
    sample_rate_hz: RTLSDR_SAMPLE_RATE_HZ,
    tunable_span_hz: RTLSDR_SAMPLE_RATE_HZ,
    gain: Some(TUNER_GAIN),
    dev_only: false,
};

/// `rtlsdr-hf`.
pub const RTLSDR_HF: Profile = Profile {
    name: "rtlsdr-hf",
    proto: Some(v1::SdrProfile::RtlsdrHf),
    description: "RTL-SDR on HF by direct sampling (Q branch), for dongles with direct \
                  sampling such as the RTL-SDR Blog V3, not the V4",
    sdr_model: "RTL-SDR HF direct sampling",
    // Direct sampling at the 28.8 MHz ADC clock; the default covers 40 m.
    center: Some(CenterRange {
        min_hz: 500_000,
        max_hz: 28_800_000,
        default_hz: 7_100_000,
    }),
    sample_rate_hz: RTLSDR_SAMPLE_RATE_HZ,
    tunable_span_hz: RTLSDR_SAMPLE_RATE_HZ,
    // The tuner is bypassed: its gain is not in the signal path.
    gain: None,
    dev_only: false,
};

/// `rtlsdr-v4`.
pub const RTLSDR_V4: Profile = Profile {
    name: "rtlsdr-v4",
    proto: Some(v1::SdrProfile::RtlsdrV4),
    description: "RTL-SDR Blog V4: HF through its upconverter, and VHF/UHF",
    sdr_model: "RTL-SDR Blog V4",
    // The R828D tuner with its built-in HF upconverter, which librtlsdr
    // switches in below 28.8 MHz; the default covers 40 m.
    center: Some(CenterRange {
        min_hz: 500_000,
        max_hz: 1_766_000_000,
        default_hz: 7_100_000,
    }),
    sample_rate_hz: RTLSDR_SAMPLE_RATE_HZ,
    tunable_span_hz: RTLSDR_SAMPLE_RATE_HZ,
    gain: Some(TUNER_GAIN),
    dev_only: false,
};

/// `siggen`: 24 MS/s real samples, the LO fixed at 0 Hz (0 to 12 MHz).
pub const SIGGEN: Profile = Profile {
    name: "siggen",
    proto: None,
    description: "radiod's built-in signal generator (development, no hardware)",
    sdr_model: "coaxhub sig_gen",
    center: None,
    sample_rate_hz: 24_000_000,
    tunable_span_hz: 24_000_000,
    gain: None,
    dev_only: true,
};

impl Profile {
    /// The front-end settings it accepts in a receiver's configuration
    /// (`ProfileInfo.frontend`): its gain, and a centre the hub may move
    /// while the receiver runs (`can_retune`).
    pub fn frontend_controls(&self) -> v1::FrontendControls {
        v1::FrontendControls {
            has_gain: self.gain.is_some(),
            min_gain_db: self.gain.map_or(0.0, |g| g.min_db),
            max_gain_db: self.gain.map_or(0.0, |g| g.max_db),
            has_hardware_agc: false,
            has_attenuator: false,
            max_attenuation_db: 0.0,
            antenna_ports: Vec::new(),
            can_retune: self.center.is_some(),
        }
    }

    /// What a receiver of it can do live (`ReceiverCapabilities.frontend`):
    /// a retune and nothing else, at the pinned radiod (§9).
    pub fn live_controls(&self) -> v1::FrontendControls {
        v1::FrontendControls {
            can_retune: self.center.is_some(),
            ..v1::FrontendControls::default()
        }
    }

    /// The range the receiver could cover at any accepted centre: the
    /// centre range widened by half the span on each side.
    pub fn hardware_range(&self) -> v1::FrequencyRange {
        let half = u64::from(self.tunable_span_hz / 2);
        match self.center {
            Some(c) => v1::FrequencyRange {
                start_hz: c.min_hz.saturating_sub(half),
                end_hz: c.max_hz.saturating_add(half),
            },
            None => v1::FrequencyRange {
                start_hz: 0,
                end_hz: u64::from(self.sample_rate_hz / 2),
            },
        }
    }

    /// `Hardware.profiles`' entry; `None` for a development profile.
    pub fn info(&self) -> Option<v1::ProfileInfo> {
        let (proto, center) = (self.proto?, self.center?);
        Some(v1::ProfileInfo {
            profile: proto as i32,
            center_range: Some(v1::FrequencyRange {
                start_hz: center.min_hz,
                end_hz: center.max_hz,
            }),
            default_center_hz: center.default_hz,
            sample_rate_hz: self.sample_rate_hz,
            tunable_span_hz: self.tunable_span_hz,
            frontend: Some(self.frontend_controls()),
        })
    }
}

/// Why a profile name was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown SDR profile {name:?}; this build has: {}", supported.join(", "))]
pub struct UnknownProfile {
    /// The name as given.
    pub name: String,
    /// The profiles this build has.
    pub supported: Vec<&'static str>,
}

/// The profiles a build has (`packaged`: no development ones).
pub fn supported(packaged: bool) -> impl Iterator<Item = ProfileId> {
    ProfileId::ALL
        .into_iter()
        .filter(move |p| p.in_build(packaged))
}

/// The profile whose template is `name`, if this build has it.
pub fn by_name(name: &str, packaged: bool) -> Result<ProfileId, UnknownProfile> {
    supported(packaged)
        .find(|p| p.profile().name == name)
        .ok_or_else(|| UnknownProfile {
            name: name.to_owned(),
            supported: supported(packaged).map(|p| p.profile().name).collect(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use station_proto::validate::predicted_tunable_range;

    #[test]
    fn the_table() {
        for id in ProfileId::ALL {
            let p = id.profile();
            if let Some(c) = p.center {
                assert!(c.contains(c.default_hz), "{}", p.name);
                let range = predicted_tunable_range(c.default_hz, p.tunable_span_hz);
                let hw = p.hardware_range();
                assert!(hw.start_hz <= range.start_hz && range.end_hz <= hw.end_hz);
            }
            if let Some(g) = p.gain {
                assert!((g.min_db..=g.max_db).contains(&g.default_db));
            }
            assert_eq!(p.info().is_some(), !p.dev_only, "{}", p.name);
            assert_eq!(id.to_string(), p.name);
            if let Some(proto) = p.proto {
                assert_eq!(ProfileId::from_proto(proto), Some(id));
            }
        }
        assert_eq!(ProfileId::from_proto(v1::SdrProfile::Unspecified), None);
        let hf = RTLSDR_HF.info().expect("info");
        assert_eq!(hf.profile, v1::SdrProfile::RtlsdrHf as i32);
        assert!(!hf.frontend.as_ref().expect("controls").has_gain);
        assert!(hf.frontend.expect("controls").can_retune);
        assert_eq!(
            predicted_tunable_range(7_100_000, RTLSDR_V4.tunable_span_hz),
            v1::FrequencyRange {
                start_hz: 6_076_000,
                end_hz: 8_124_000
            }
        );
        let live = RTLSDR_V4.live_controls();
        assert!(live.can_retune && !live.has_gain);
        assert_eq!(SIGGEN.hardware_range().end_hz, 12_000_000);
        assert!(!SIGGEN.live_controls().can_retune);
    }

    #[test]
    fn names_by_flavour() {
        assert_eq!(by_name("rtlsdr", true), Ok(ProfileId::Rtlsdr));
        assert_eq!(by_name("rtlsdr-hf", true), Ok(ProfileId::RtlsdrHf));
        assert_eq!(by_name("rtlsdr-v4", true), Ok(ProfileId::RtlsdrV4));
        assert_eq!(by_name("siggen", false), Ok(ProfileId::Siggen));
        let e = by_name("siggen", true).expect_err("development only");
        assert_eq!(e.supported, vec!["rtlsdr", "rtlsdr-hf", "rtlsdr-v4"]);
        assert!(
            e.to_string().contains("rtlsdr, rtlsdr-hf, rtlsdr-v4"),
            "{e}"
        );
        assert!(by_name("airspy", false).is_err());
        assert!(!ProfileId::Siggen.in_build(true));
        assert!(ProfileId::Siggen.in_build(false));
        let json = serde_json::to_string(&ProfileId::RtlsdrHf).expect("json");
        assert_eq!(json, "\"rtlsdr_hf\"");
    }
}
