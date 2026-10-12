// SPDX-License-Identifier: GPL-3.0-only
//! `receivers.json`: the receiver set the station last applied
//! (station-protocol §7).
//!
//! `/var/lib/coaxhub/receivers/receivers.json` in a packaged station
//! (`<state_dir>/receivers/receivers.json` in a development build with its
//! own `state_dir`), owned by `coaxhub`, group `coaxhub-radiod`, mode 0640,
//! so each receiver's unit renders its radiod config from it
//! (`stationd backend-config`). Written atomically. The schema:
//!
//! ```json
//! {
//!   "receivers": [
//!     {"receiver_id": 11, "slot": 3, "sdr_serial": "A0000011",
//!      "model": "rtlsdr_blog_v4", "profile": "rtlsdr_v4",
//!      "center_frequency_hz": 14200000, "gain_db": 28.0}
//!   ]
//! }
//! ```
//!
//! - `receiver_id`: the hub's, never 0, unique.
//! - `slot`: the receiver's local slot, 0 to 15, unique, kept for the
//!   receiver's life on this station ([`assign`]); it fixes its radiod's
//!   multicast groups (`backend_ka9q::backend::group::slot_groups`).
//! - `sdr_serial`: the SDR's USB serial; empty means the station's only
//!   RTL-SDR.
//! - `model`: `rtlsdr_blog_v4`, `rtlsdr_direct_sampling` or `rtlsdr_other`.
//! - `profile`: `rtlsdr`, `rtlsdr_hf` or `rtlsdr_v4` (`siggen` in development
//!   builds).
//! - `center_frequency_hz`: the front end's centre.
//! - `gain_db`, `hardware_agc`, `attenuation_db`: optional; absent means
//!   the profile's default.
//!
//! Unknown keys are errors.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use backend_ka9q::backend::group;
use serde::{Deserialize, Serialize};
use station_proto::v1;

use crate::profile::ProfileId;
use crate::state::{self, StateError};

/// The file's name in the receivers directory.
pub const RECEIVERS_FILE: &str = "receivers.json";

/// The file's mode: the radiod group reads it.
pub const RECEIVERS_MODE: u32 = 0o640;

/// The SDR model, as `receivers.json` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Model {
    /// `SDR_MODEL_RTLSDR_BLOG_V4`.
    RtlsdrBlogV4,
    /// `SDR_MODEL_RTLSDR_DIRECT_SAMPLING`.
    RtlsdrDirectSampling,
    /// `SDR_MODEL_RTLSDR_OTHER`.
    RtlsdrOther,
}

impl Model {
    /// The hub's model, if this package has it.
    pub fn from_proto(model: v1::SdrModel) -> Option<Self> {
        match model {
            v1::SdrModel::RtlsdrBlogV4 => Some(Self::RtlsdrBlogV4),
            v1::SdrModel::RtlsdrDirectSampling => Some(Self::RtlsdrDirectSampling),
            v1::SdrModel::RtlsdrOther => Some(Self::RtlsdrOther),
            v1::SdrModel::Unspecified => None,
        }
    }

    /// The name in `receivers.json`.
    pub fn name(self) -> &'static str {
        match self {
            Self::RtlsdrBlogV4 => "rtlsdr_blog_v4",
            Self::RtlsdrDirectSampling => "rtlsdr_direct_sampling",
            Self::RtlsdrOther => "rtlsdr_other",
        }
    }

    /// The protocol's value.
    pub fn proto(self) -> v1::SdrModel {
        match self {
            Self::RtlsdrBlogV4 => v1::SdrModel::RtlsdrBlogV4,
            Self::RtlsdrDirectSampling => v1::SdrModel::RtlsdrDirectSampling,
            Self::RtlsdrOther => v1::SdrModel::RtlsdrOther,
        }
    }
}

/// One applied receiver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredReceiver {
    /// The hub's ID.
    pub receiver_id: u32,
    /// The local slot, 0 to 15.
    pub slot: u8,
    /// The SDR's USB serial; empty: the only RTL-SDR.
    pub sdr_serial: String,
    /// The SDR model.
    pub model: Model,
    /// The profile.
    pub profile: ProfileId,
    /// The front end's centre, Hz.
    pub center_frequency_hz: u64,
    /// Tuner gain, dB; absent: the profile's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f32>,
    /// Hardware AGC; absent: the profile's default (off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_agc: Option<bool>,
    /// Attenuation, dB; absent: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attenuation_db: Option<f32>,
}

impl StoredReceiver {
    /// Whether `other` runs the same way: everything but the slot equal
    /// (an unchanged receiver is left alone, §4.2).
    pub fn same_config(&self, other: &Self) -> bool {
        Self {
            slot: other.slot,
            ..self.clone()
        } == *other
    }

    /// Its radiod's status and data groups.
    pub fn groups(&self) -> (std::net::SocketAddr, std::net::SocketAddr) {
        group::slot_groups(self.slot)
    }

    /// As the hub configured it (for the shared rules of §4.2); `None` for
    /// a development profile the hub never names.
    pub fn to_proto(&self) -> Option<v1::ReceiverConfig> {
        Some(v1::ReceiverConfig {
            receiver_id: self.receiver_id,
            sdr_serial: self.sdr_serial.clone(),
            model: self.model.proto() as i32,
            profile: self.profile.profile().proto? as i32,
            center_frequency_hz: self.center_frequency_hz,
            gain_db: self.gain_db,
            hardware_agc: self.hardware_agc,
            attenuation_db: self.attenuation_db,
        })
    }
}

/// The whole applied set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredSet {
    /// In the order the hub sent them.
    pub receivers: Vec<StoredReceiver>,
}

/// Why `receivers.json` can't be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// It can't be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// It is not a receiver set.
    #[error("invalid {}: {message}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
    /// It can't be written.
    #[error(transparent)]
    Write(#[from] StateError),
}

impl StoredSet {
    /// The receiver `id`.
    pub fn get(&self, receiver_id: u32) -> Option<&StoredReceiver> {
        self.receivers.iter().find(|r| r.receiver_id == receiver_id)
    }

    /// Reads `path`; a missing file is the empty set.
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(StoreError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let invalid = |message: String| StoreError::Invalid {
            path: path.to_owned(),
            message,
        };
        let set: Self = serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
        set.check().map_err(invalid)?;
        Ok(set)
    }

    /// The set's own rules: IDs not 0 and unique, slots in range and
    /// unique.
    fn check(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        let mut slots = HashSet::new();
        for r in &self.receivers {
            if r.receiver_id == 0 || !ids.insert(r.receiver_id) {
                return Err(format!(
                    "receiver_id {} is 0 or appears twice",
                    r.receiver_id
                ));
            }
            if !group::SLOTS.contains(&r.slot) || !slots.insert(r.slot) {
                return Err(format!(
                    "receiver {}: slot {} is out of range or taken",
                    r.receiver_id, r.slot
                ));
            }
        }
        Ok(())
    }

    /// Writes the set to `path` atomically, mode 0640. The directory's
    /// group (`coaxhub-radiod`, set-group-ID) becomes the file's.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        let mut json = serde_json::to_vec_pretty(self).map_err(|e| StoreError::Invalid {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        json.push(b'\n');
        state::write_atomic(path, &json, RECEIVERS_MODE)?;
        Ok(())
    }
}

/// The stored form of the hub's `set` (already checked against the
/// hardware, §4.2): each receiver keeps its slot from `previous`, and a new
/// one takes the lowest slot free. Receivers of a development profile can't
/// be in a hub's set; a receiver whose model or profile this package lacks
/// is left out (the shared rules have refused it already).
pub fn assign(previous: &StoredSet, set: &v1::ConfigureReceivers) -> StoredSet {
    let kept: Vec<u8> = set
        .receivers
        .iter()
        .filter_map(|r| previous.get(r.receiver_id).map(|p| p.slot))
        .collect();
    let mut free = group::SLOTS.filter(|s| !kept.contains(s));
    let receivers = set
        .receivers
        .iter()
        .filter_map(|r| {
            let model = Model::from_proto(v1::SdrModel::try_from(r.model).ok()?)?;
            let profile = ProfileId::from_proto(v1::SdrProfile::try_from(r.profile).ok()?)?;
            let slot = match previous.get(r.receiver_id) {
                Some(p) => p.slot,
                None => free.next()?,
            };
            Some(StoredReceiver {
                receiver_id: r.receiver_id,
                slot,
                sdr_serial: r.sdr_serial.clone(),
                model,
                profile,
                center_frequency_hz: r.center_frequency_hz,
                gain_db: r.gain_db,
                hardware_agc: r.hardware_agc,
                attenuation_db: r.attenuation_db,
            })
        })
        .collect();
    StoredSet { receivers }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn rx(id: u32, serial: &str, center: u64) -> v1::ReceiverConfig {
        v1::ReceiverConfig {
            receiver_id: id,
            sdr_serial: serial.into(),
            model: v1::SdrModel::RtlsdrBlogV4 as i32,
            profile: v1::SdrProfile::RtlsdrV4 as i32,
            center_frequency_hz: center,
            gain_db: Some(28.0),
            ..Default::default()
        }
    }

    #[test]
    fn slots_are_kept_and_the_lowest_free_is_taken() {
        let set = |rs: Vec<v1::ReceiverConfig>| v1::ConfigureReceivers { receivers: rs };
        let first = assign(
            &StoredSet::default(),
            &set(vec![rx(5, "a", 1), rx(9, "b", 2)]),
        );
        let slots: Vec<(u32, u8)> = first
            .receivers
            .iter()
            .map(|r| (r.receiver_id, r.slot))
            .collect();
        assert_eq!(slots, [(5, 0), (9, 1)]);
        // 5 leaves, 7 arrives before 9: 9 keeps slot 1, 7 takes slot 0.
        let second = assign(&first, &set(vec![rx(7, "c", 3), rx(9, "b", 2)]));
        let slots: Vec<(u32, u8)> = second
            .receivers
            .iter()
            .map(|r| (r.receiver_id, r.slot))
            .collect();
        assert_eq!(slots, [(7, 0), (9, 1)]);
        assert!(
            second
                .get(9)
                .expect("9")
                .same_config(first.get(9).expect("9"))
        );
        assert!(
            !second
                .get(7)
                .expect("7")
                .same_config(first.get(5).expect("5"))
        );
        // Sixteen receivers fill every slot.
        let full = assign(
            &StoredSet::default(),
            &set((1..=16).map(|i| rx(i, "x", u64::from(i))).collect()),
        );
        let mut slots: Vec<u8> = full.receivers.iter().map(|r| r.slot).collect();
        slots.sort_unstable();
        assert_eq!(slots, (0..=15).collect::<Vec<u8>>());
        // A model this package lacks is left out.
        let mut odd = rx(3, "y", 1);
        odd.model = 99;
        assert!(
            assign(&StoredSet::default(), &set(vec![odd]))
                .receivers
                .is_empty()
        );
        let r = first.get(5).expect("5");
        assert_eq!(r.groups().0.to_string(), "239.77.0.6:5006");
        assert_eq!(r.to_proto(), Some(rx(5, "a", 1)));
    }

    #[test]
    fn the_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join(RECEIVERS_FILE);
        assert_eq!(
            StoredSet::load(&path).expect("missing"),
            StoredSet::default()
        );
        let set = assign(
            &StoredSet::default(),
            &v1::ConfigureReceivers {
                receivers: vec![rx(11, "A0000011", 14_200_000)],
            },
        );
        set.save(&path).expect("save");
        assert_eq!(StoredSet::load(&path).expect("load"), set);
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("\"profile\": \"rtlsdr_v4\""), "{text}");
        assert!(text.contains("\"model\": \"rtlsdr_blog_v4\""), "{text}");
        assert!(!text.contains("hardware_agc"), "{text}");
        // The packaging tests' shape.
        std::fs::write(
            &path,
            r#"{"receivers":[{"receiver_id":11,"slot":3,"sdr_serial":"A0000011","model":"rtlsdr_other","profile":"rtlsdr","center_frequency_hz":98000000},
            {"receiver_id":12,"slot":7,"sdr_serial":"","model":"rtlsdr_direct_sampling","profile":"rtlsdr_hf","center_frequency_hz":7100000}]}"#,
        )
        .expect("write");
        let set = StoredSet::load(&path).expect("load");
        assert_eq!(set.get(12).expect("12").profile, ProfileId::RtlsdrHf);
        for bad in [
            "not json",
            r#"{"receivers":[{"receiver_id":0,"slot":0,"sdr_serial":"","model":"rtlsdr_other","profile":"rtlsdr","center_frequency_hz":1}]}"#,
            r#"{"receivers":[{"receiver_id":1,"slot":16,"sdr_serial":"","model":"rtlsdr_other","profile":"rtlsdr","center_frequency_hz":1}]}"#,
            r#"{"receivers":[{"receiver_id":1,"slot":1,"sdr_serial":"","model":"rtlsdr_other","profile":"rtlsdr","center_frequency_hz":1},{"receiver_id":2,"slot":1,"sdr_serial":"","model":"rtlsdr_other","profile":"rtlsdr","center_frequency_hz":1}]}"#,
            r#"{"receivers":[{"receiver_id":1,"slot":1,"sdr_serial":"","model":"airspy","profile":"rtlsdr","center_frequency_hz":1}]}"#,
            r#"{"receivers":[],"extra":1}"#,
        ] {
            std::fs::write(&path, bad).expect("write");
            let e = StoredSet::load(&path).expect_err(bad);
            assert!(matches!(e, StoreError::Invalid { .. }), "{bad}: {e}");
        }
        assert!(matches!(
            StoredSet::load(tmp.path()),
            Err(StoreError::Read { .. })
        ));
        let e = set
            .save(&tmp.path().join("missing/receivers.json"))
            .expect_err("no dir");
        assert!(matches!(e, StoreError::Write(_)), "{e}");
    }
}
