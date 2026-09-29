// SPDX-License-Identifier: GPL-3.0-only
//! Typed decoder for radiod channel status (`STATUS`) packets.
//!
//! Field types follow what `encode_radio_status()` in upstream
//! `radio_status.c` sends; each field's doc names the upstream encoder and
//! the unit. Every field is optional because radiod sends different sets per
//! demodulator (for example no audio fields for spectrum channels) and older
//! or newer radiod versions may differ.
//!
//! Decoding rules:
//! - `CMD` packets are rejected ([`TlvError::WrongPacketType`]).
//! - Unknown tags, and known tags this struct does not model, are ignored.
//! - If a tag appears more than once, the last occurrence wins.
//! - A malformed value of a modelled field (for example a 9-byte integer or
//!   a 7-byte socket) fails the whole packet with [`TlvError::Field`].
//! - A zero-length socket (the "null socket" the upstream docs allow) decodes
//!   as `None`.
//! - Enum fields ([`DemodType`], [`Encoding`]) with a value this build does
//!   not know decode as `None`.
//!
//! Station-reported values are claims used for display only.

use std::net::SocketAddr;

use crate::tags::{DemodType, Encoding, PktType, StatusType};
use crate::tlv::{Field, TlvError, parse};

/// The decoded status of one radiod channel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChannelStatus {
    /// `OUTPUT_SSRC`: channel SSRC, also the RTP SSRC (`encode_int32`).
    pub ssrc: Option<u32>,
    /// `COMMAND_TAG`: tag of the last command received (`encode_int64`).
    pub command_tag: Option<u64>,
    /// `CMD_CNT`: commands received by this channel (`encode_int64`).
    pub cmd_cnt: Option<u64>,
    /// `GPS_TIME`: nanoseconds since the GPS epoch (`encode_int64` of an
    /// `int64_t`).
    pub gps_time_ns: Option<i64>,
    /// `DESCRIPTION`: front end description (`encode_string`, UTF-8 lossy).
    pub description: Option<String>,
    /// `STATUS_DEST_SOCKET`: where status is multicast (`encode_socket`).
    pub status_dest_socket: Option<SocketAddr>,
    /// `OUTPUT_DATA_DEST_SOCKET`: where RTP data is sent (`encode_socket`).
    pub output_data_dest_socket: Option<SocketAddr>,
    /// `OUTPUT_SAMPRATE`, Hz (`encode_int32`).
    pub output_samprate: Option<u32>,
    /// `OUTPUT_CHANNELS`: 1 or 2 (`encode_int32`).
    pub output_channels: Option<u32>,
    /// `OUTPUT_ENCODING` (`encode_int`).
    pub output_encoding: Option<Encoding>,
    /// `RTP_PT`: RTP payload type (`encode_byte`).
    pub rtp_pt: Option<u8>,
    /// `OPUS_BIT_RATE`, bit/s, 0 = automatic (`encode_int`; Opus only).
    pub opus_bit_rate: Option<i32>,
    /// `RADIO_FREQUENCY`, Hz (`encode_double`).
    pub radio_frequency_hz: Option<f64>,
    /// `FIRST_LO_FREQUENCY`: front end tuner frequency, Hz (`encode_double`).
    pub first_lo_hz: Option<f64>,
    /// `LOW_EDGE` of the passband, Hz (`encode_float`).
    pub low_edge_hz: Option<f64>,
    /// `HIGH_EDGE` of the passband, Hz (`encode_float`).
    pub high_edge_hz: Option<f64>,
    /// `DEMOD_TYPE` (`encode_byte`).
    pub demod_type: Option<DemodType>,
    /// `PRESET` name (`encode_string`, UTF-8 lossy).
    pub preset: Option<String>,
    /// `INPUT_SAMPRATE`: front end sample rate, Hz (`encode_int32`).
    pub input_samprate: Option<u32>,
    /// `FE_LOW_EDGE`: front end filter low edge, Hz (`encode_float`).
    pub fe_low_edge_hz: Option<f64>,
    /// `FE_HIGH_EDGE`: front end filter high edge, Hz (`encode_float`).
    pub fe_high_edge_hz: Option<f64>,
    /// `FE_ISREAL`: front end samples are real, not complex (`encode_bool`).
    pub fe_isreal: Option<bool>,
    /// `AD_OVER`: A/D overrange sample count (`encode_int64`).
    pub ad_over: Option<u64>,
    /// `SAMPLES_SINCE_OVER`: samples since the last overrange
    /// (`encode_int64`; sent only after an overrange).
    pub samples_since_over: Option<u64>,
    /// `LNA_GAIN`, dB (`encode_int32` of an `int`; tuners with one only).
    pub lna_gain_db: Option<i32>,
    /// `MIXER_GAIN`, dB (`encode_int32` of an `int`).
    pub mixer_gain_db: Option<i32>,
    /// `IF_GAIN`, dB (`encode_int32` of an `int`).
    pub if_gain_db: Option<i32>,
    /// `RF_GAIN`, dB (`encode_float`; sent when known).
    pub rf_gain_db: Option<f64>,
    /// `RF_ATTEN`, dB (`encode_float`; sent when known).
    pub rf_atten_db: Option<f64>,
    /// `RF_AGC`: front end AGC on (`encode_bool`).
    pub rf_agc: Option<bool>,
    /// `IF_POWER`: A/D input power, dBFS (`encode_float`).
    pub if_power_db: Option<f64>,
    /// `BASEBAND_POWER`: channel power, dB (`encode_float`).
    pub baseband_power_db: Option<f64>,
    /// `NOISE_DENSITY`: N0, dB/Hz (`encode_float`; sent when positive).
    pub noise_density_db: Option<f64>,
    /// `OUTPUT_LEVEL`, dB (`encode_float`; not for spectrum channels).
    pub output_level_db: Option<f64>,
    /// `PLL_SNR`, dB (`encode_float`; linear mode with PLL only).
    pub pll_snr_db: Option<f64>,
    /// `FM_SNR`, dB (`encode_float`; FM and WFM only).
    pub fm_snr_db: Option<f64>,
    /// `AGC_ENABLE` (`encode_bool`; linear only).
    pub agc_enable: Option<bool>,
    /// `GAIN`, dB (`encode_float`; linear only).
    pub gain_db: Option<f64>,
    /// `SNR_SQUELCH` enabled (`encode_bool`; not for spectrum channels).
    pub snr_squelch: Option<bool>,
    /// `SQUELCH_OPEN` threshold, dB (`encode_float`).
    pub squelch_open_db: Option<f64>,
    /// `SQUELCH_CLOSE` threshold, dB (`encode_float`).
    pub squelch_close_db: Option<f64>,
    /// `LIFETIME`: frames left before the channel removes itself, 0 = never
    /// (`encode_int32`).
    pub lifetime: Option<u32>,
    /// `STATUS_INTERVAL`: frames between status packets on the data channel
    /// (`encode_int32`).
    pub status_interval: Option<u32>,
    /// `BIN_COUNT` of a spectrum channel (`encode_int`).
    pub bin_count: Option<u32>,
    /// `RESOLUTION_BW`: spectrum bin width, Hz (`encode_float`).
    pub resolution_bw_hz: Option<f64>,
    /// `WINDOW_TYPE` of a spectrum channel (`encode_int`; see
    /// [`crate::command::Command::window_type`]).
    pub window_type: Option<u32>,
    /// `SPECTRUM_BASE`: level of byte 0 in `bin_bytes`, dB (`encode_float`;
    /// spectrum2 only).
    pub spectrum_base_db: Option<f64>,
    /// `SPECTRUM_STEP`: dB per step of `bin_bytes` (`encode_float`;
    /// spectrum2 only).
    pub spectrum_step_db: Option<f64>,
    /// `SPECTRUM_FFT_N`: analysis FFT size (`encode_int`).
    pub spectrum_fft_n: Option<u32>,
    /// `SPECTRUM_AVG`: FFTs averaged per response (`encode_int`).
    pub spectrum_avg: Option<u32>,
    /// `BIN_BYTE_DATA`: spectrum2 bins, one byte each, low-to-high frequency
    /// (`encode_string`).
    pub bin_bytes: Option<Vec<u8>>,
    /// `BIN_DATA`: spectrum (v1) bin energies, big-endian `f32`, DC first
    /// (`encode_vector`).
    pub bin_floats: Option<Vec<f32>>,
}

/// Decodes a socket field; the zero-length "null socket" is `None`.
fn socket(f: &Field<'_>) -> Result<Option<SocketAddr>, TlvError> {
    if f.value.is_empty() {
        Ok(None)
    } else {
        f.as_socket().map(Some)
    }
}

impl ChannelStatus {
    /// Decodes a `STATUS` packet (see the module docs for the rules).
    pub fn decode(buf: &[u8]) -> Result<Self, TlvError> {
        let packet = parse(buf)?;
        if packet.kind != PktType::Status {
            return Err(TlvError::WrongPacketType {
                expected: PktType::Status,
                got: packet.kind,
            });
        }
        let mut s = Self::default();
        for field in packet.fields() {
            let Some(tag) = field.status_type() else {
                continue;
            };
            s.apply(tag, &field).map_err(|e| TlvError::Field {
                tag,
                source: Box::new(e),
            })?;
        }
        Ok(s)
    }

    /// Stores one field.
    fn apply(&mut self, tag: StatusType, f: &Field<'_>) -> Result<(), TlvError> {
        use StatusType as S;
        let float = || f.as_f32_or_f64().map(Some);
        let uint = || f.as_u32().map(Some);
        match tag {
            S::OutputSsrc => self.ssrc = uint()?,
            S::CommandTag => self.command_tag = Some(f.as_u64()?),
            S::CmdCnt => self.cmd_cnt = Some(f.as_u64()?),
            S::GpsTime => self.gps_time_ns = Some(f.as_i64()?),
            S::Description => self.description = Some(f.as_string_lossy().into_owned()),
            S::StatusDestSocket => self.status_dest_socket = socket(f)?,
            S::OutputDataDestSocket => self.output_data_dest_socket = socket(f)?,
            S::OutputSamprate => self.output_samprate = uint()?,
            S::OutputChannels => self.output_channels = uint()?,
            S::OutputEncoding => self.output_encoding = Encoding::from_i32(f.as_i32()?),
            S::RtpPt => self.rtp_pt = Some(f.as_u8()?),
            S::OpusBitRate => self.opus_bit_rate = Some(f.as_i32()?),
            S::RadioFrequency => self.radio_frequency_hz = Some(f.as_f64()?),
            S::FirstLoFrequency => self.first_lo_hz = Some(f.as_f64()?),
            S::LowEdge => self.low_edge_hz = float()?,
            S::HighEdge => self.high_edge_hz = float()?,
            S::DemodType => self.demod_type = DemodType::from_i32(f.as_i32()?),
            S::Preset => self.preset = Some(f.as_string_lossy().into_owned()),
            S::InputSamprate => self.input_samprate = uint()?,
            S::FeLowEdge => self.fe_low_edge_hz = float()?,
            S::FeHighEdge => self.fe_high_edge_hz = float()?,
            S::FeIsreal => self.fe_isreal = Some(f.as_bool()?),
            S::AdOver => self.ad_over = Some(f.as_u64()?),
            S::SamplesSinceOver => self.samples_since_over = Some(f.as_u64()?),
            S::LnaGain => self.lna_gain_db = Some(f.as_i32()?),
            S::MixerGain => self.mixer_gain_db = Some(f.as_i32()?),
            S::IfGain => self.if_gain_db = Some(f.as_i32()?),
            S::RfGain => self.rf_gain_db = float()?,
            S::RfAtten => self.rf_atten_db = float()?,
            S::RfAgc => self.rf_agc = Some(f.as_bool()?),
            S::IfPower => self.if_power_db = float()?,
            S::BasebandPower => self.baseband_power_db = float()?,
            S::NoiseDensity => self.noise_density_db = float()?,
            S::OutputLevel => self.output_level_db = float()?,
            S::PllSnr => self.pll_snr_db = float()?,
            S::FmSnr => self.fm_snr_db = float()?,
            S::AgcEnable => self.agc_enable = Some(f.as_bool()?),
            S::Gain => self.gain_db = float()?,
            S::SnrSquelch => self.snr_squelch = Some(f.as_bool()?),
            S::SquelchOpen => self.squelch_open_db = float()?,
            S::SquelchClose => self.squelch_close_db = float()?,
            S::Lifetime => self.lifetime = uint()?,
            S::StatusInterval => self.status_interval = uint()?,
            S::BinCount => self.bin_count = uint()?,
            S::ResolutionBw => self.resolution_bw_hz = float()?,
            S::WindowType => self.window_type = uint()?,
            S::SpectrumBase => self.spectrum_base_db = float()?,
            S::SpectrumStep => self.spectrum_step_db = float()?,
            S::SpectrumFftN => self.spectrum_fft_n = uint()?,
            S::SpectrumAvg => self.spectrum_avg = uint()?,
            S::BinByteData => self.bin_bytes = Some(f.as_bytes().to_vec()),
            S::BinData => self.bin_floats = Some(f.as_f32_array()?),
            _ => {}
        }
        Ok(())
    }

    /// Channel SNR in dB, derived as upstream `control.c` does from the
    /// passband width, `BASEBAND_POWER` and `NOISE_DENSITY`:
    /// `S = max(P_bb - N0 * B, 0)`, `SNR = S / (N0 * B)`. `None` when an
    /// input is missing or the bandwidth or noise density is zero; `-inf`
    /// when the channel holds only noise.
    pub fn snr_db(&self) -> Option<f64> {
        let bw = (self.high_edge_hz? - self.low_edge_hz?).abs();
        let bb = 10f64.powf(self.baseband_power_db? / 10.0);
        let n0 = 10f64.powf(self.noise_density_db? / 10.0);
        let noise = n0 * bw;
        if !(noise > 0.0 && noise.is_finite()) || bb.is_nan() {
            return None;
        }
        let signal = (bb - noise).max(0.0);
        Some(10.0 * (signal / noise).log10())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "the codec must be bit-exact")]
mod tests {
    use super::*;
    use crate::tlv::Encoder;
    use proptest::prelude::*;

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Front end and common part of `encode_radio_status()`, in upstream order.
    fn common(e: Encoder, ssrc: u32, demod: DemodType) -> Encoder {
        use StatusType as S;
        e.u32(S::OutputSsrc, ssrc)
            .u64(S::CommandTag, 0xdead_beef_0000_0001)
            .u64(S::CmdCnt, 3)
            .str(S::Description, "siggen test")
            .u32(S::Lifetime, 480)
            .socket(S::StatusDestSocket, sock("239.1.2.3:5006"))
            .i64(S::GpsTime, 1_400_000_000_000_000_000)
            .u64(S::InputSamples, 123_456)
            .u32(S::InputSamprate, 64_800_000)
            .bool(S::FeIsreal, true)
            .f64(S::Calibrate, 0.0)
            .f32(S::RfGain, 10.0)
            .f32(S::RfAtten, 0.0)
            .bool(S::RfAgc, false)
            .u32(S::LnaGain, 12)
            .u32(S::MixerGain, 8)
            .u32(S::IfGain, 0xffff_fffe_u32) // (uint32_t) of int -2
            .f32(S::FeLowEdge, 0.0)
            .f32(S::FeHighEdge, 32.4e6)
            .u32(S::AdBitsPerSample, 16)
            .f64(S::RadioFrequency, 14.074e6)
            .f64(S::FirstLoFrequency, 0.0)
            .f64(S::SecondLoFrequency, -14.074e6)
            .u32(S::FilterBlocksize, 64_800)
            .u32(S::FilterFirLength, 16_201)
            .u32(S::FilterDrops, 0)
            .f32(S::IfPower, -30.5)
            .u64(S::AdOver, 2)
            .u64(S::SamplesSinceOver, 99)
            .f32(S::NoiseDensity, -150.0)
            .byte(S::DemodType, u8::try_from(demod.value()).unwrap())
    }

    /// A linear (USB) Opus channel as radiod encodes it.
    fn linear_opus_status() -> Vec<u8> {
        use StatusType as S;
        common(Encoder::new(PktType::Status), 1000, DemodType::Linear)
            .str(S::Preset, "usb")
            .f32(S::KaiserBeta, 11.0)
            .f32(S::LowEdge, 50.0)
            .f32(S::HighEdge, 3000.0)
            .u32(S::OutputSamprate, 12_000)
            .f32(S::BasebandPower, -80.0)
            .u32(S::OutputChannels, 1)
            // LINEAR_DEMOD block
            .bool(S::PllEnable, false)
            .bool(S::Envelope, false)
            .f64(S::ShiftFrequency, 0.0)
            .bool(S::AgcEnable, true)
            .f32(S::AgcHangtime, 1.1)
            .f32(S::AgcThreshold, -15.0)
            .f32(S::AgcRecoveryRate, 20.0)
            .bool(S::IndependentSideband, false)
            .f32(S::Gain, 42.0)
            // non-spectrum block
            .bool(S::SnrSquelch, false)
            .f32(S::SquelchOpen, 8.0)
            .f32(S::SquelchClose, 7.0)
            .u32(S::RtpTimesnap, 777)
            .u64(S::OutputDataPackets, 50)
            .i32(S::Filter2, 0)
            .f32(S::OutputLevel, -20.0)
            .u64(S::OutputSamples, 480_000)
            .i32(S::OpusBitRate, 0)
            .i32(S::OpusBandwidth, 1105)
            .i32(S::OpusApplication, 2049)
            .i32(S::OpusFec, 0)
            .bool(S::OpusDtx, false)
            .f32(S::Headroom, -15.0)
            .f64(S::DopplerFrequency, 0.0)
            .f64(S::DopplerFrequencyRate, 0.0)
            .socket(S::OutputDataSourceSocket, sock("0.0.0.0:40000"))
            .socket(S::OutputDataDestSocket, sock("239.1.2.3:5004"))
            .u32(S::OutputTtl, 0)
            .byte(S::RtpPt, 111)
            .u32(S::StatusInterval, 0)
            .i32(S::OutputEncoding, Encoding::Opus.value())
            .i32(S::Maxdelay, 0)
            .u64(S::OutputMetadataPackets, 9)
            .u64(S::Setopts, 0)
            .u64(S::OutputErrors, 0)
            .finish()
    }

    /// A spectrum2 channel as radiod encodes it.
    fn spectrum2_status(bins: &[u8]) -> Vec<u8> {
        use StatusType as S;
        common(Encoder::new(PktType::Status), 2000, DemodType::Spect2)
            .f32(S::KaiserBeta, 0.0)
            .f32(S::LowEdge, -50_000.0)
            .f32(S::HighEdge, 50_000.0)
            .u32(S::OutputSamprate, 0)
            .f32(S::BasebandPower, -60.0)
            .u32(S::OutputChannels, 1)
            // SPECT2_DEMOD block
            .i32(S::WindowType, 5)
            .f32(S::ResolutionBw, 100.0)
            .i32(S::BinCount, i32::try_from(bins.len()).unwrap())
            .f32(S::Crossover, 200.0)
            .i32(S::SpectrumFftN, 648_000)
            .f32(S::NoiseBw, 1.5)
            .i32(S::SpectrumAvg, 10)
            .f32(S::SpectrumOverlap, 0.0)
            .f32(S::SpectrumBase, -120.0)
            .f32(S::SpectrumStep, 0.5)
            .bytes(S::BinByteData, bins)
            .u64(S::OutputMetadataPackets, 4)
            .u64(S::Setopts, 0)
            .u64(S::OutputErrors, 0)
            .finish()
    }

    #[test]
    fn decodes_linear_opus_channel() {
        let s = ChannelStatus::decode(&linear_opus_status()).unwrap();
        assert_eq!(s.ssrc, Some(1000));
        assert_eq!(s.command_tag, Some(0xdead_beef_0000_0001));
        assert_eq!(s.cmd_cnt, Some(3));
        assert_eq!(s.gps_time_ns, Some(1_400_000_000_000_000_000));
        assert_eq!(s.description.as_deref(), Some("siggen test"));
        assert_eq!(s.status_dest_socket, Some(sock("239.1.2.3:5006")));
        assert_eq!(s.output_data_dest_socket, Some(sock("239.1.2.3:5004")));
        assert_eq!(s.output_samprate, Some(12_000));
        assert_eq!(s.output_channels, Some(1));
        assert_eq!(s.output_encoding, Some(Encoding::Opus));
        assert_eq!(s.rtp_pt, Some(111));
        assert_eq!(s.opus_bit_rate, Some(0));
        assert_eq!(s.radio_frequency_hz, Some(14.074e6));
        assert_eq!(s.first_lo_hz, Some(0.0));
        assert_eq!(s.low_edge_hz, Some(50.0));
        assert_eq!(s.high_edge_hz, Some(3000.0));
        assert_eq!(s.demod_type, Some(DemodType::Linear));
        assert_eq!(s.preset.as_deref(), Some("usb"));
        assert_eq!(s.input_samprate, Some(64_800_000));
        assert_eq!(s.fe_low_edge_hz, Some(0.0));
        assert_eq!(s.fe_high_edge_hz, Some(f64::from(32.4e6_f32)));
        assert_eq!(s.fe_isreal, Some(true));
        assert_eq!(s.ad_over, Some(2));
        assert_eq!(s.samples_since_over, Some(99));
        assert_eq!(s.lna_gain_db, Some(12));
        assert_eq!(s.mixer_gain_db, Some(8));
        assert_eq!(s.if_gain_db, Some(-2));
        assert_eq!(s.rf_gain_db, Some(10.0));
        assert_eq!(s.rf_atten_db, Some(0.0));
        assert_eq!(s.rf_agc, Some(false));
        assert_eq!(s.if_power_db, Some(-30.5));
        assert_eq!(s.baseband_power_db, Some(-80.0));
        assert_eq!(s.noise_density_db, Some(-150.0));
        assert_eq!(s.output_level_db, Some(-20.0));
        assert_eq!(s.pll_snr_db, None);
        assert_eq!(s.fm_snr_db, None);
        assert_eq!(s.agc_enable, Some(true));
        assert_eq!(s.gain_db, Some(42.0));
        assert_eq!(s.snr_squelch, Some(false));
        assert_eq!(s.squelch_open_db, Some(8.0));
        assert_eq!(s.squelch_close_db, Some(7.0));
        assert_eq!(s.lifetime, Some(480));
        assert_eq!(s.status_interval, Some(0));
        assert_eq!(s.bin_count, None);
        assert_eq!(s.bin_bytes, None);
        assert_eq!(s.bin_floats, None);
        assert!(s.snr_db().is_some());
    }

    #[test]
    fn decodes_spectrum2_channel() {
        let bins: Vec<u8> = (0..=255).cycle().take(1620).collect();
        let s = ChannelStatus::decode(&spectrum2_status(&bins)).unwrap();
        assert_eq!(s.ssrc, Some(2000));
        assert_eq!(s.demod_type, Some(DemodType::Spect2));
        assert_eq!(s.window_type, Some(5));
        assert_eq!(s.resolution_bw_hz, Some(100.0));
        assert_eq!(s.bin_count, Some(1620));
        assert_eq!(s.spectrum_fft_n, Some(648_000));
        assert_eq!(s.spectrum_avg, Some(10));
        assert_eq!(s.spectrum_base_db, Some(-120.0));
        assert_eq!(s.spectrum_step_db, Some(0.5));
        assert_eq!(s.bin_bytes.as_deref(), Some(&bins[..]));
        assert_eq!(s.bin_floats, None);
        // No audio fields for spectrum channels.
        assert_eq!(s.output_encoding, None);
        assert_eq!(s.output_data_dest_socket, None);
        assert_eq!(s.snr_squelch, None);
        assert_eq!(s.preset, None);
    }

    #[test]
    fn decodes_spectrum_v1_float_bins() {
        let bins = [1.0f32, 0.5, -0.0, 1e-12];
        let pkt = Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 7)
            .byte(StatusType::DemodType, 3)
            .f32_array(StatusType::BinData, &bins)
            .finish();
        let s = ChannelStatus::decode(&pkt).unwrap();
        assert_eq!(s.demod_type, Some(DemodType::Spect));
        assert_eq!(s.bin_floats.as_deref(), Some(&bins[..]));
    }

    #[test]
    fn fm_and_pll_snr() {
        let pkt = Encoder::new(PktType::Status)
            .f32(StatusType::PllSnr, 12.5)
            .f32(StatusType::FmSnr, 20.0)
            .finish();
        let s = ChannelStatus::decode(&pkt).unwrap();
        assert_eq!(s.pll_snr_db, Some(12.5));
        assert_eq!(s.fm_snr_db, Some(20.0));
    }

    #[test]
    fn rejects_command_packets() {
        let pkt = Encoder::new(PktType::Cmd)
            .u32(StatusType::OutputSsrc, 1)
            .finish();
        assert_eq!(
            ChannelStatus::decode(&pkt),
            Err(TlvError::WrongPacketType {
                expected: PktType::Status,
                got: PktType::Cmd
            })
        );
        assert_eq!(ChannelStatus::decode(&[]), Err(TlvError::Empty));
    }

    #[test]
    fn empty_status_is_all_none() {
        assert_eq!(
            ChannelStatus::decode(&[0, 0]).unwrap(),
            ChannelStatus::default()
        );
        assert_eq!(ChannelStatus::default().snr_db(), None);
    }

    #[test]
    fn unknown_and_unmodelled_tags_are_ignored() {
        let pkt = [0, 250, 3, 1, 2, 3, 24, 1, 9, 18, 1, 5, 0];
        let s = ChannelStatus::decode(&pkt).unwrap();
        assert_eq!(s.ssrc, Some(5));
    }

    #[test]
    fn duplicate_tags_last_wins() {
        let pkt = Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 1)
            .str(StatusType::Preset, "am")
            .u32(StatusType::OutputSsrc, 2)
            .str(StatusType::Preset, "fm")
            .finish();
        let s = ChannelStatus::decode(&pkt).unwrap();
        assert_eq!(s.ssrc, Some(2));
        assert_eq!(s.preset.as_deref(), Some("fm"));
    }

    #[test]
    fn unknown_enum_values_are_none() {
        let pkt = Encoder::new(PktType::Status)
            .byte(StatusType::DemodType, 200)
            .i32(StatusType::OutputEncoding, 99)
            .finish();
        let s = ChannelStatus::decode(&pkt).unwrap();
        assert_eq!(s.demod_type, None);
        assert_eq!(s.output_encoding, None);
    }

    #[test]
    fn null_socket_is_none() {
        let pkt = [0, 17, 0, 5, 0, 0];
        let s = ChannelStatus::decode(&pkt).unwrap();
        assert_eq!(s.output_data_dest_socket, None);
        assert_eq!(s.status_dest_socket, None);
    }

    #[test]
    fn invalid_utf8_strings_are_lossy() {
        let pkt = [0, 4, 2, 0xff, b'x', 85, 1, 0xc3, 0];
        let s = ChannelStatus::decode(&pkt).unwrap();
        assert_eq!(s.description.as_deref(), Some("\u{fffd}x"));
        assert_eq!(s.preset.as_deref(), Some("\u{fffd}"));
    }

    #[test]
    fn malformed_fields_fail_with_context() {
        let cases: &[(&[u8], StatusType, TlvError)] = &[
            (
                &[0, 39, 9, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
                StatusType::LowEdge,
                TlvError::IntegerTooLong(9),
            ),
            (
                &[0, 17, 7, 1, 2, 3, 4, 5, 6, 7, 0],
                StatusType::OutputDataDestSocket,
                TlvError::BadSocketLength(7),
            ),
            (
                &[0, 96, 6, 0, 0, 0, 0, 0, 0, 0],
                StatusType::BinData,
                TlvError::BadFloatArrayLength(6),
            ),
            (
                &[0, 18, 9, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
                StatusType::OutputSsrc,
                TlvError::IntegerTooLong(9),
            ),
        ];
        for (pkt, tag, source) in cases {
            assert_eq!(
                ChannelStatus::decode(pkt),
                Err(TlvError::Field {
                    tag: *tag,
                    source: Box::new(source.clone())
                }),
                "{tag}"
            );
        }
        // Structural errors pass through unchanged.
        assert_eq!(
            ChannelStatus::decode(&[0, 18, 5, 1]),
            Err(TlvError::LengthExceedsBuffer {
                tag: 18,
                offset: 1,
                len: 5,
                remaining: 1
            })
        );
    }

    #[test]
    fn snr_matches_control_c() {
        // B = 10 kHz, N0 = -100 dB/Hz -> noise 1e-6; P_bb = -50 dB = 1e-5;
        // S = 9e-6; SNR = 10 log10(9) = 9.542 dB.
        let s = ChannelStatus {
            low_edge_hz: Some(-5000.0),
            high_edge_hz: Some(5000.0),
            baseband_power_db: Some(-50.0),
            noise_density_db: Some(-100.0),
            ..ChannelStatus::default()
        };
        let snr = s.snr_db().unwrap();
        assert!((snr - 9.0f64.log10() * 10.0).abs() < 1e-9, "{snr}");
        // Noise only.
        let noise_only = ChannelStatus {
            baseband_power_db: Some(-70.0),
            ..s.clone()
        };
        assert_eq!(noise_only.snr_db(), Some(f64::NEG_INFINITY));
        // Zero bandwidth.
        let zero_bw = ChannelStatus {
            high_edge_hz: Some(-5000.0),
            ..s.clone()
        };
        assert_eq!(zero_bw.snr_db(), None);
        // Missing input.
        let missing = ChannelStatus {
            noise_density_db: None,
            ..s
        };
        assert_eq!(missing.snr_db(), None);
    }

    fn any_tag() -> impl Strategy<Value = StatusType> {
        (1..StatusType::ALL.len()).prop_map(|i| StatusType::ALL[i])
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_input(buf in proptest::collection::vec(any::<u8>(), 0..512)) {
            if let Ok(s) = ChannelStatus::decode(&buf) {
                let _ = s.snr_db();
            }
        }

        #[test]
        fn never_panics_on_known_tags_with_arbitrary_values(
            items in proptest::collection::vec(
                (any_tag(), proptest::collection::vec(any::<u8>(), 0..24)), 0..40)
        ) {
            let mut e = Encoder::new(PktType::Status);
            for (tag, v) in &items {
                e = e.bytes(*tag, v);
            }
            if let Ok(s) = ChannelStatus::decode(&e.finish()) {
                let _ = s.snr_db();
            }
        }

        #[test]
        fn round_trip_core_fields(
            ssrc: u32, tag: u64, freq in any::<f64>().prop_filter("not NaN", |f| !f.is_nan()),
            low in -1e5f32..1e5, bins in proptest::collection::vec(any::<u8>(), 0..2000),
        ) {
            let pkt = Encoder::new(PktType::Status)
                .u32(StatusType::OutputSsrc, ssrc)
                .u64(StatusType::CommandTag, tag)
                .f64(StatusType::RadioFrequency, freq)
                .f32(StatusType::LowEdge, f64::from(low))
                .bytes(StatusType::BinByteData, &bins)
                .finish();
            let s = ChannelStatus::decode(&pkt).unwrap();
            prop_assert_eq!(s.ssrc, Some(ssrc));
            prop_assert_eq!(s.command_tag, Some(tag));
            prop_assert_eq!(s.radio_frequency_hz.map(f64::to_bits), Some(freq.to_bits()));
            prop_assert_eq!(s.low_edge_hz, Some(f64::from(low)));
            prop_assert_eq!(s.bin_bytes, Some(bins));
        }
    }
}
