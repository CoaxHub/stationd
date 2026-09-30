// SPDX-License-Identifier: GPL-3.0-only
//! Writes the committed seed corpus of the backend-ka9q fuzz target:
//!
//! ```text
//! cargo run -p backend-ka9q --example fuzz_seeds [-- <corpus dir>]
//! ```
//!
//! The default directory is `crates/backend-ka9q/fuzz/corpus`. Seeds are
//! radiod packets shaped like the ones in `docs/radiod-notes.md`: commands,
//! an audio channel status, a spectrum2 reply with bins, and an RTP packet.

use std::error::Error;
use std::path::{Path, PathBuf};

use backend_ka9q::command::Command;
use backend_ka9q::tags::{Encoding, PktType, StatusType as S};
use backend_ka9q::tlv::Encoder;

type Res<T> = Result<T, Box<dyn Error>>;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> Res<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(name), bytes)?;
    Ok(())
}

fn main() -> Res<()> {
    let root = std::env::args().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus"),
        PathBuf::from,
    );
    let dir = root.join("tlv_status");
    let create = Command::new(1234, 7)
        .preset("am")
        .output_samprate(12_000)
        .output_encoding(Encoding::Opus)
        .radio_frequency_hz(10e6)
        .low_edge_hz(-5000.0)
        .high_edge_hz(5000.0)
        .lifetime(500)
        .encode();
    write(&dir, "cmd_create_opus", &create)?;
    write(&dir, "cmd_destroy", &Command::destroy(1234, 8).encode())?;
    let audio = Encoder::new(PktType::Status)
        .u32(S::OutputSsrc, 1234)
        .u64(S::CommandTag, 0xb0c5_103c)
        .u64(S::CmdCnt, 2)
        .str(S::Description, "coaxhub-test sig_gen")
        .u32(S::Lifetime, 499)
        .socket(S::StatusDestSocket, "239.77.0.6:5006".parse()?)
        .u32(S::InputSamprate, 24_000_000)
        .bool(S::FeIsreal, true)
        .f32(S::FeLowEdge, 0.0)
        .f32(S::FeHighEdge, 12e6)
        .f64(S::RadioFrequency, 10e6)
        .f64(S::FirstLoFrequency, 0.0)
        .f32(S::IfPower, -20.4)
        .u64(S::AdOver, 0)
        .f32(S::NoiseDensity, -91.5)
        .i32(S::DemodType, 0)
        .str(S::Preset, "am")
        .f32(S::LowEdge, -5000.0)
        .f32(S::HighEdge, 5000.0)
        .u32(S::OutputSamprate, 12_000)
        .f32(S::BasebandPower, -19.5)
        .u32(S::OutputChannels, 1)
        .socket(S::OutputDataDestSocket, "239.77.0.4:5004".parse()?)
        .byte(S::RtpPt, 111)
        .i32(S::OutputEncoding, 3)
        .i32(S::OpusBitRate, 0)
        .finish();
    write(&dir, "status_audio", &audio)?;
    let bins: Vec<u8> = (0..512u32)
        .map(|i| {
            if i == 256 {
                255
            } else {
                u8::try_from(i % 32).unwrap_or(0)
            }
        })
        .collect();
    let spectrum = Encoder::new(PktType::Status)
        .u32(S::OutputSsrc, 5000)
        .u64(S::CmdCnt, 54)
        .u32(S::Lifetime, 1499)
        .f64(S::RadioFrequency, 10e6)
        .i32(S::DemodType, 4)
        .f32(S::ResolutionBw, 1000.0)
        .u32(S::BinCount, 512)
        .u32(S::SpectrumFftN, 24_000)
        .u32(S::SpectrumAvg, 10)
        .f32(S::SpectrumBase, -71.15)
        .f32(S::SpectrumStep, 0.2055)
        .bytes(S::BinByteData, &bins)
        .finish();
    write(&dir, "status_spectrum2", &spectrum)?;
    let mut rtp = vec![0x80, 111, 0x12, 0x34, 0, 0, 0x03, 0xc0, 0, 0, 0x04, 0xd2];
    rtp.extend_from_slice(&[0x28, 1, 2, 3, 4, 5]);
    write(&dir, "rtp_opus", &rtp)?;
    println!("seed corpus written to {}", root.display());
    Ok(())
}
