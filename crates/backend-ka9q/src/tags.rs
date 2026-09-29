// SPDX-License-Identifier: GPL-3.0-only
//! ka9q-radio tag enums generated at build time from the pinned headers.
//!
//! `build.rs` parses `vendor/ka9q-radio/status.h` and `rtp.h` (pinned in
//! `vendor/ka9q-radio/VERSION`) and generates [`PktType`], [`StatusType`],
//! [`DemodType`] and [`Encoding`]. Each has `ALL`, `value()`, `c_name()`, a
//! checked constructor (`from_u8` or `from_i32`) and `Display` (the C name).
//! Changing the pinned headers is a deliberate act: a test compares their
//! SHA-256 with `VERSION`.

include!(concat!(env!("OUT_DIR"), "/tags.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    /// Tag values hand-counted from the pinned `status.h`
    /// (commit 384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd).
    #[test]
    fn status_type_known_values() {
        let expected: &[(StatusType, u8)] = &[
            (StatusType::Eol, 0),
            (StatusType::CommandTag, 1),
            (StatusType::CmdCnt, 2),
            (StatusType::GpsTime, 3),
            (StatusType::Description, 4),
            (StatusType::StatusDestSocket, 5),
            (StatusType::BinByteData, 9),
            (StatusType::InputSamprate, 10),
            (StatusType::SpectrumBase, 11),
            (StatusType::SpectrumAvg, 12),
            (StatusType::WindowType, 14),
            (StatusType::OutputDataDestSocket, 17),
            (StatusType::OutputSsrc, 18),
            (StatusType::OutputSamprate, 20),
            (StatusType::LnaGain, 25),
            (StatusType::RadioFrequency, 33),
            (StatusType::FirstLoFrequency, 34),
            (StatusType::LowEdge, 39),
            (StatusType::HighEdge, 40),
            (StatusType::IfPower, 45),
            (StatusType::BasebandPower, 46),
            (StatusType::NoiseDensity, 47),
            (StatusType::DemodType, 48),
            (StatusType::OutputChannels, 49),
            (StatusType::SnrSquelch, 57),
            (StatusType::PllSnr, 58),
            (StatusType::AgcEnable, 62),
            (StatusType::FmSnr, 66),
            (StatusType::Gain, 68),
            (StatusType::OutputLevel, 69),
            (StatusType::OpusBitRate, 71),
            (StatusType::SpectrumFftN, 76),
            (StatusType::Unused4, 81),
            (StatusType::SquelchOpen, 83),
            (StatusType::SquelchClose, 84),
            (StatusType::Preset, 85),
            (StatusType::Unused3, 88),
            (StatusType::Unused2, 92),
            (StatusType::ResolutionBw, 93),
            (StatusType::BinCount, 94),
            (StatusType::BinData, 96),
            (StatusType::RfAtten, 97),
            (StatusType::RfGain, 98),
            (StatusType::RfAgc, 99),
            (StatusType::FeLowEdge, 100),
            (StatusType::FeHighEdge, 101),
            (StatusType::FeIsreal, 102),
            (StatusType::Unused, 103),
            (StatusType::AdOver, 104),
            (StatusType::RtpPt, 105),
            (StatusType::StatusInterval, 106),
            (StatusType::OutputEncoding, 107),
            (StatusType::SamplesSinceOver, 108),
            (StatusType::SpectrumStep, 115),
            (StatusType::SpectrumOverlap, 116),
            (StatusType::Lifetime, 117),
        ];
        for &(tag, value) in expected {
            assert_eq!(tag.value(), value, "{tag}");
            assert_eq!(StatusType::from_u8(value), Some(tag));
        }
    }

    #[test]
    fn status_type_is_dense_and_complete() {
        assert_eq!(StatusType::ALL.len(), 118);
        for (i, tag) in StatusType::ALL.iter().enumerate() {
            assert_eq!(usize::from(tag.value()), i);
        }
        assert_eq!(StatusType::from_u8(118), None);
        assert_eq!(StatusType::from_u8(255), None);
    }

    #[test]
    fn pkt_type_values() {
        assert_eq!(PktType::Status.value(), 0);
        assert_eq!(PktType::Cmd.value(), 1);
        assert_eq!(PktType::from_u8(1), Some(PktType::Cmd));
        assert_eq!(PktType::from_u8(2), None);
        assert_eq!(PktType::ALL, &[PktType::Status, PktType::Cmd]);
    }

    #[test]
    fn demod_type_values() {
        assert_eq!(DemodType::Linear.value(), 0);
        assert_eq!(DemodType::Fm.value(), 1);
        assert_eq!(DemodType::Wfm.value(), 2);
        assert_eq!(DemodType::Spect.value(), 3);
        assert_eq!(DemodType::Spect2.value(), 4);
        assert_eq!(DemodType::Idle.value(), 5);
        assert_eq!(DemodType::ALL.len(), 6);
        assert_eq!(DemodType::from_i32(-1), None, "INVALID_DEMOD is skipped");
        assert_eq!(DemodType::from_i32(6), None, "N_DEMOD is skipped");
        assert_eq!(DemodType::Spect2.c_name(), "SPECT2_DEMOD");
    }

    #[test]
    fn encoding_values() {
        let expected = [
            (Encoding::NoEncoding, 0),
            (Encoding::S16le, 1),
            (Encoding::S16be, 2),
            (Encoding::Opus, 3),
            (Encoding::F32le, 4),
            (Encoding::Ax25, 5),
            (Encoding::F16le, 6),
            (Encoding::OpusVoip, 7),
            (Encoding::F32be, 8),
            (Encoding::F16be, 9),
            (Encoding::Mulaw, 10),
            (Encoding::Alaw, 11),
        ];
        assert_eq!(Encoding::ALL.len(), expected.len());
        for (e, v) in expected {
            assert_eq!(e.value(), v);
            assert_eq!(Encoding::from_i32(v), Some(e));
        }
        assert_eq!(Encoding::from_i32(12), None, "UNUSED_ENCODING is skipped");
    }

    #[test]
    fn c_names_and_display() {
        assert_eq!(StatusType::RadioFrequency.c_name(), "RADIO_FREQUENCY");
        assert_eq!(StatusType::OpusBitRate.to_string(), "OPUS_BIT_RATE");
        assert_eq!(PktType::Cmd.to_string(), "CMD");
        assert_eq!(Encoding::Opus.to_string(), "OPUS");
        assert_eq!(DemodType::Linear.to_string(), "LINEAR_DEMOD");
        for e in Encoding::ALL {
            assert_eq!(Encoding::from_i32(e.value()), Some(*e));
        }
        for d in DemodType::ALL {
            assert_eq!(DemodType::from_i32(d.value()), Some(*d));
        }
    }

    #[test]
    fn upstream_constants() {
        assert_eq!(UPSTREAM_COMMIT, "384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd");
        assert_eq!(UPSTREAM_URL, "https://github.com/ka9q/ka9q-radio");
    }

    /// CI guard: the vendored headers must match the SHA-256 recorded in
    /// `VERSION`. Re-vendor with `scripts/update-ka9q-headers.sh`.
    #[test]
    fn vendored_headers_match_version_hashes() {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/ka9q-radio");
        let version = crate::codegen::parse_version_file(
            &std::fs::read_to_string(dir.join("VERSION")).unwrap(),
        )
        .unwrap();
        assert_eq!(version.commit, UPSTREAM_COMMIT);
        let files: Vec<&str> = version.sha256.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(files, ["status.h", "rtp.h"]);
        for (file, want) in &version.sha256 {
            let bytes = std::fs::read(dir.join(file)).unwrap();
            let got = Sha256::digest(&bytes)
                .iter()
                .fold(String::new(), |mut hex, b| {
                    let _ = write!(hex, "{b:02x}");
                    hex
                });
            assert_eq!(&got, want, "{file} differs from the pinned VERSION hash");
        }
    }
}
