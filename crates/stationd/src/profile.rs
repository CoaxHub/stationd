// SPDX-License-Identifier: GPL-3.0-only
//! SDR profiles (`[sdr] profile`) and the fixed radiod multicast groups
//! (station-protocol §9).
//!
//! The owner picks a backend-neutral profile; the package renders radiod's
//! config from the profile's template (`stationd backend-config`) with the
//! groups below, and stationd's ka9q backend uses the same groups. This
//! module is the **only** place the groups are defined.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

/// radiod's status and command group (`status =`, port 5006).
pub const RADIOD_STATUS_GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 77, 0, 6), 5006);

/// radiod's RTP group for dynamic channels (`data =`, port 5004).
pub const RADIOD_DATA_GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 77, 0, 4), 5004);

/// [`RADIOD_STATUS_GROUP`] as a [`SocketAddr`].
pub fn radiod_status_group() -> SocketAddr {
    SocketAddr::V4(RADIOD_STATUS_GROUP)
}

/// [`RADIOD_DATA_GROUP`] as a [`SocketAddr`].
pub fn radiod_data_group() -> SocketAddr {
    SocketAddr::V4(RADIOD_DATA_GROUP)
}

/// A USB vendor and product ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsbId {
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
}

impl fmt::Display for UsbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

/// RTL2832U dongles: `0bda:2838` and `0bda:2832`.
pub const RTL_SDR_USB_IDS: &[UsbId] = &[
    UsbId {
        vendor: 0x0bda,
        product: 0x2838,
    },
    UsbId {
        vendor: 0x0bda,
        product: 0x2832,
    },
];

/// The tunable centre frequency of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CenterFrequency {
    /// Used when `center_frequency_hz` is not set.
    pub default_hz: u64,
    /// Lowest accepted value.
    pub min_hz: u64,
    /// Highest accepted value.
    pub max_hz: u64,
}

/// One SDR profile.
#[derive(Debug, PartialEq, Eq)]
pub struct SdrProfile {
    /// The `[sdr] profile` value and the template file stem.
    pub name: &'static str,
    /// For humans.
    pub description: &'static str,
    /// USB IDs of devices this profile drives (empty: no hardware).
    pub usb_ids: &'static [UsbId],
    /// `None`: the front end has no settable centre frequency.
    pub center: Option<CenterFrequency>,
    /// Development builds only (no hardware; tests and dev tooling).
    pub dev_only: bool,
}

/// Every profile, production ones first.
pub const PROFILES: &[SdrProfile] = &[
    SdrProfile {
        name: "rtlsdr",
        description: "RTL-SDR (RTL2832U + R820T/R828D), VHF/UHF",
        usb_ids: RTL_SDR_USB_IDS,
        // R820T tuner range; the default is the FM broadcast band.
        center: Some(CenterFrequency {
            default_hz: 98_000_000,
            min_hz: 24_000_000,
            max_hz: 1_766_000_000,
        }),
        dev_only: false,
    },
    SdrProfile {
        name: "rtlsdr-hf",
        description: "RTL-SDR on HF by direct sampling (Q branch), for dongles with direct \
                      sampling such as the RTL-SDR Blog V3, not the V4",
        usb_ids: RTL_SDR_USB_IDS,
        // Direct sampling at the 28.8 MHz ADC clock; the default covers 40 m.
        center: Some(CenterFrequency {
            default_hz: 7_100_000,
            min_hz: 500_000,
            max_hz: 28_800_000,
        }),
        dev_only: false,
    },
    SdrProfile {
        name: "rtlsdr-v4",
        description: "RTL-SDR Blog V4 (HF by its upconverter, VHF/UHF)",
        usb_ids: RTL_SDR_USB_IDS,
        // The V4's R828D tuner with its built-in HF upconverter, which
        // librtlsdr switches in below 28.8 MHz; the default covers 40 m.
        center: Some(CenterFrequency {
            default_hz: 7_100_000,
            min_hz: 500_000,
            max_hz: 1_766_000_000,
        }),
        dev_only: false,
    },
    SdrProfile {
        name: "siggen",
        description: "radiod's built-in signal generator (development, no hardware)",
        usb_ids: &[],
        center: None,
        dev_only: true,
    },
];

/// Why a profile name was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown SDR profile {name:?}; supported: {}", supported.join(", "))]
pub struct UnknownProfile {
    /// The name as configured.
    pub name: String,
    /// The profiles this build supports.
    pub supported: Vec<&'static str>,
}

/// The profiles a build supports (`packaged`: no dev-only ones).
pub fn supported(packaged: bool) -> impl Iterator<Item = &'static SdrProfile> {
    PROFILES.iter().filter(move |p| !(packaged && p.dev_only))
}

/// The profile called `name`, if this build supports it.
pub fn by_name(name: &str, packaged: bool) -> Result<&'static SdrProfile, UnknownProfile> {
    supported(packaged)
        .find(|p| p.name == name)
        .ok_or_else(|| UnknownProfile {
            name: name.to_owned(),
            supported: supported(packaged).map(|p| p.name).collect(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_match_the_test_configs() {
        assert_eq!(radiod_status_group().to_string(), "239.77.0.6:5006");
        assert_eq!(radiod_data_group().to_string(), "239.77.0.4:5004");
        assert!(radiod_status_group().ip().is_multicast());
    }

    #[test]
    fn profiles_by_flavour() {
        assert_eq!(by_name("rtlsdr", true).expect("rtlsdr").name, "rtlsdr");
        assert!(by_name("rtlsdr-hf", true).is_ok());
        assert_eq!(
            by_name("rtlsdr-v4", true).expect("v4").usb_ids,
            RTL_SDR_USB_IDS
        );
        assert!(by_name("siggen", false).is_ok());
        let e = by_name("siggen", true).expect_err("dev only");
        assert_eq!(e.supported, vec!["rtlsdr", "rtlsdr-hf", "rtlsdr-v4"]);
        assert!(
            e.to_string().contains("rtlsdr, rtlsdr-hf, rtlsdr-v4"),
            "{e}"
        );
        let e = by_name("airspy", false).expect_err("unknown");
        assert!(e.to_string().contains("siggen"), "{e}");
        assert_eq!(RTL_SDR_USB_IDS[0].to_string(), "0bda:2838");
        for p in PROFILES {
            if let Some(c) = p.center {
                assert!((c.min_hz..=c.max_hz).contains(&c.default_hz), "{}", p.name);
            }
        }
    }
}
