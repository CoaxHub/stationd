// SPDX-License-Identifier: GPL-3.0-only
//! Typed builder for radiod command (`CMD`) packets.
//!
//! Each setter documents the upstream encoder that matches how radiod's
//! `decode_radio_commands()` (`radio_status.c`) decodes the field. Mixing
//! them up is silent on the wire: radiod reads `RADIO_FREQUENCY` with
//! `decode_double`, so a `float`-encoded frequency would become a tiny
//! denormal.
//!
//! [`Command::encode`] always puts `OUTPUT_SSRC` and `COMMAND_TAG` first and
//! always ends with `EOL` (radiod drops a last TLV that is not followed by at
//! least one byte). The other fields follow in a fixed order chosen so that
//! radiod's in-order processing does what the caller means:
//!
//! 1. `PRESET` (radiod applies it in a first pass anyway),
//! 2. `DEMOD_TYPE`, `OUTPUT_SAMPRATE`, `OUTPUT_CHANNELS`, then
//!    `OUTPUT_ENCODING` (selecting Opus forces 48 kHz only if the rate set
//!    just before is not a legal Opus rate) and `OPUS_BIT_RATE`,
//! 3. `RADIO_FREQUENCY`, `LOW_EDGE`, `HIGH_EDGE`,
//! 4. `GAIN` before `AGC_ENABLE` (radiod turns the AGC off when it receives
//!    `GAIN`, so an explicit `AGC_ENABLE` must come after it to win),
//!    `RF_GAIN`, `RF_ATTEN`, squelch settings,
//! 5. spectrum settings, `STATUS_INTERVAL`, `OUTPUT_DATA_DEST_SOCKET`,
//!    `LIFETIME`.
//!
//! Upstream quirk: radiod rejects a new `LOW_EDGE` above the current high
//! edge and a new `HIGH_EDGE` below the current low edge, so moving the
//! passband to a range disjoint from the current one needs two commands.
//!
//! Integer fields that radiod decodes with `decode_int` (a C `int`) take
//! `u32` here and are clamped to `i32::MAX`, so a large value can never be
//! read as negative by radiod. `f32` setters use `encode_float`, which skips
//! NaN, so a NaN value leaves the field out.

use std::net::SocketAddr;

use crate::tags::{DemodType, Encoding, PktType, StatusType};
use crate::tlv::Encoder;

/// SSRC that asks radiod to send the status of every channel.
pub const SSRC_ALL_CHANNELS: u32 = 0xffff_ffff;

/// A command for one radiod channel. Unset fields are not sent, so a command
/// with no fields is a poll.
///
/// ```
/// use backend_ka9q::command::Command;
/// use backend_ka9q::tags::Encoding;
///
/// let pkt = Command::new(1000, 7)
///     .preset("am")
///     .radio_frequency_hz(10e6)
///     .output_encoding(Encoding::Opus)
///     .encode();
/// assert_eq!(pkt[0], 1); // CMD
/// assert_eq!(*pkt.last().unwrap(), 0); // EOL
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
#[must_use]
pub struct Command {
    ssrc: u32,
    tag: u32,
    preset: Option<String>,
    demod_type: Option<DemodType>,
    output_samprate: Option<u32>,
    output_channels: Option<u32>,
    output_encoding: Option<Encoding>,
    opus_bit_rate: Option<u32>,
    radio_frequency_hz: Option<f64>,
    first_lo_hz: Option<f64>,
    low_edge_hz: Option<f32>,
    high_edge_hz: Option<f32>,
    gain_db: Option<f32>,
    agc_enable: Option<bool>,
    rf_gain_db: Option<f32>,
    rf_atten_db: Option<f32>,
    snr_squelch: Option<bool>,
    squelch_open_db: Option<f32>,
    squelch_close_db: Option<f32>,
    bin_count: Option<u32>,
    resolution_bw_hz: Option<f32>,
    window_type: Option<u32>,
    spectrum_avg: Option<u32>,
    spectrum_base_db: Option<f32>,
    spectrum_step_db: Option<f32>,
    status_interval: Option<u32>,
    output_data_dest_socket: Option<SocketAddr>,
    lifetime: Option<u32>,
}

fn c_int(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

impl Command {
    /// A command for channel `ssrc` carrying `tag`, which radiod echoes as
    /// `COMMAND_TAG` in its status replies. A new SSRC creates a dynamic
    /// channel from radiod's template; SSRC 0 is reserved and ignored by
    /// radiod. (radiod stores tags as 64-bit; 32 random bits are plenty.)
    pub fn new(ssrc: u32, tag: u32) -> Self {
        Self {
            ssrc,
            tag,
            ..Self::default()
        }
    }

    /// A poll: only `OUTPUT_SSRC` and `COMMAND_TAG`. radiod answers with a
    /// full status packet (and restarts the channel's `LIFETIME` timer).
    pub fn poll(ssrc: u32, tag: u32) -> Self {
        Self::new(ssrc, tag)
    }

    /// Tears a channel down.
    ///
    /// At the pinned commit, `RADIO_FREQUENCY` 0 only mutes and parks the
    /// channel (`set_freq()` treats 0 Hz as "disabled"); the channel object is
    /// removed only when its `LIFETIME` runs out. This command therefore sends
    /// `RADIO_FREQUENCY` 0 and `LIFETIME` 1, so radiod removes the channel on
    /// its next frame (about 20 ms).
    pub fn destroy(ssrc: u32, tag: u32) -> Self {
        Self::new(ssrc, tag).radio_frequency_hz(0.0).lifetime(1)
    }

    /// Asks radiod to send the status of every running channel (SSRC
    /// `0xffffffff`). radiod does not forward this command to the channels,
    /// so `tag` is not echoed; the replies carry each channel's last tag.
    pub fn dump_all(tag: u32) -> Self {
        Self::new(SSRC_ALL_CHANNELS, tag)
    }

    /// The target SSRC.
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The command tag.
    pub fn tag(&self) -> u32 {
        self.tag
    }

    /// `PRESET` (`encode_string`): a preset name from radiod's presets file,
    /// applied before every other field of the packet.
    pub fn preset(mut self, name: impl Into<String>) -> Self {
        self.preset = Some(name.into());
        self
    }

    /// `DEMOD_TYPE` (`encode_int`; radiod `decode_int`).
    pub fn demod_type(mut self, demod: DemodType) -> Self {
        self.demod_type = Some(demod);
        self
    }

    /// `OUTPUT_SAMPRATE`, Hz (`encode_int`; radiod `decode_int`, rounded to a
    /// multiple of the block rate, ignored for spectrum channels).
    pub fn output_samprate(mut self, hz: u32) -> Self {
        self.output_samprate = Some(hz);
        self
    }

    /// `OUTPUT_CHANNELS` (`encode_int`; radiod accepts only 1 or 2).
    pub fn output_channels(mut self, channels: u32) -> Self {
        self.output_channels = Some(channels);
        self
    }

    /// `OUTPUT_ENCODING` (`encode_int`; radiod rejects `AX25`). Selecting Opus
    /// forces 48 kHz when the current sample rate is not a legal Opus rate.
    pub fn output_encoding(mut self, encoding: Encoding) -> Self {
        self.output_encoding = Some(encoding);
        self
    }

    /// `OPUS_BIT_RATE`, bit/s (`encode_int`; 0 lets Opus choose).
    pub fn opus_bit_rate(mut self, bps: u32) -> Self {
        self.opus_bit_rate = Some(bps);
        self
    }

    /// `RADIO_FREQUENCY`, Hz (`encode_double`; radiod `decode_double` and
    /// takes the absolute value).
    pub fn radio_frequency_hz(mut self, hz: f64) -> Self {
        self.radio_frequency_hz = Some(hz);
        self
    }

    /// `FIRST_LO_FREQUENCY`: the front end's tuner frequency, Hz
    /// (`encode_double`; radiod `decode_double`, absolute value). Shared by
    /// every channel; radiod ignores it when the tuner is locked (a fixed
    /// `frequency =` in its config) or cannot tune (the signal generator), so
    /// check `FIRST_LO_FREQUENCY` in the reply.
    pub fn first_lo_hz(mut self, hz: f64) -> Self {
        self.first_lo_hz = Some(hz);
        self
    }

    /// `LOW_EDGE` of the passband relative to the carrier, Hz
    /// (`encode_float`; radiod `decode_float`).
    pub fn low_edge_hz(mut self, hz: f32) -> Self {
        self.low_edge_hz = Some(hz);
        self
    }

    /// `HIGH_EDGE` of the passband relative to the carrier, Hz
    /// (`encode_float`; radiod `decode_float`).
    pub fn high_edge_hz(mut self, hz: f32) -> Self {
        self.high_edge_hz = Some(hz);
        self
    }

    /// `GAIN`, dB (`encode_float`). radiod also turns the AGC off.
    pub fn gain_db(mut self, db: f32) -> Self {
        self.gain_db = Some(db);
        self
    }

    /// `AGC_ENABLE` (`encode_bool`; linear modes).
    pub fn agc_enable(mut self, on: bool) -> Self {
        self.agc_enable = Some(on);
        self
    }

    /// `RF_GAIN`, dB (`encode_float`; front ends with settable gain only).
    pub fn rf_gain_db(mut self, db: f32) -> Self {
        self.rf_gain_db = Some(db);
        self
    }

    /// `RF_ATTEN`, dB (`encode_float`; front ends with an attenuator only).
    pub fn rf_atten_db(mut self, db: f32) -> Self {
        self.rf_atten_db = Some(db);
        self
    }

    /// `SNR_SQUELCH` enable (`encode_bool`).
    pub fn snr_squelch(mut self, on: bool) -> Self {
        self.snr_squelch = Some(on);
        self
    }

    /// `SQUELCH_OPEN` SNR threshold, dB (`encode_float`).
    pub fn squelch_open_db(mut self, db: f32) -> Self {
        self.squelch_open_db = Some(db);
        self
    }

    /// `SQUELCH_CLOSE` SNR threshold, dB (`encode_float`).
    pub fn squelch_close_db(mut self, db: f32) -> Self {
        self.squelch_close_db = Some(db);
        self
    }

    /// `BIN_COUNT` of a spectrum channel (`encode_int`; must be > 0).
    pub fn bin_count(mut self, bins: u32) -> Self {
        self.bin_count = Some(bins);
        self
    }

    /// `RESOLUTION_BW` (bin width) of a spectrum channel, Hz
    /// (`encode_float`).
    pub fn resolution_bw_hz(mut self, hz: f32) -> Self {
        self.resolution_bw_hz = Some(hz);
        self
    }

    /// `WINDOW_TYPE` of a spectrum channel (`encode_int`). Values from
    /// upstream `window.h`: 0 Kaiser, 1 rectangular, 2 Blackman, 3 exact
    /// Blackman, 4 Gaussian, 5 Hann (default), 6 Hamming, 7 Blackman-Harris,
    /// 8 HP5FT.
    pub fn window_type(mut self, window: u32) -> Self {
        self.window_type = Some(window);
        self
    }

    /// `SPECTRUM_AVG`: FFTs averaged per spectrum response (`encode_int`;
    /// radiod enforces at least 1).
    pub fn spectrum_avg(mut self, ffts: u32) -> Self {
        self.spectrum_avg = Some(ffts);
        self
    }

    /// `SPECTRUM_BASE`: level of byte 0 in spectrum2 bins, dB
    /// (`encode_float`).
    pub fn spectrum_base_db(mut self, db: f32) -> Self {
        self.spectrum_base_db = Some(db);
        self
    }

    /// `SPECTRUM_STEP`: dB per step of spectrum2 bins (`encode_float`).
    pub fn spectrum_step_db(mut self, db: f32) -> Self {
        self.spectrum_step_db = Some(db);
        self
    }

    /// `STATUS_INTERVAL`: send status on the data channel every N frames
    /// (`encode_int`; 0 disables).
    pub fn status_interval(mut self, frames: u32) -> Self {
        self.status_interval = Some(frames);
        self
    }

    /// `OUTPUT_DATA_DEST_SOCKET` (`encode_socket`). radiod uses only the
    /// address: it forces the data port to 5004 and sets the channel's status
    /// destination to the same address on port 5006.
    pub fn output_data_dest_socket(mut self, addr: SocketAddr) -> Self {
        self.output_data_dest_socket = Some(addr);
        self
    }

    /// `LIFETIME`: frames (typically 20 ms each) until the channel removes
    /// itself (`encode_int`). Every command restarts the timer; 0 means
    /// never.
    pub fn lifetime(mut self, frames: u32) -> Self {
        self.lifetime = Some(frames);
        self
    }

    /// Encodes the command as a `CMD` packet ending with `EOL`.
    pub fn encode(&self) -> Vec<u8> {
        use StatusType as S;
        let mut e = Encoder::new(PktType::Cmd)
            .u32(S::OutputSsrc, self.ssrc)
            .u32(S::CommandTag, self.tag);
        if let Some(p) = &self.preset {
            e = e.str(S::Preset, p);
        }
        if let Some(d) = self.demod_type {
            e = e.i32(S::DemodType, d.value());
        }
        let ints = [
            (S::OutputSamprate, self.output_samprate),
            (S::OutputChannels, self.output_channels),
        ];
        for (tag, v) in ints {
            if let Some(v) = v {
                e = e.i32(tag, c_int(v));
            }
        }
        if let Some(enc) = self.output_encoding {
            e = e.i32(S::OutputEncoding, enc.value());
        }
        if let Some(v) = self.opus_bit_rate {
            e = e.i32(S::OpusBitRate, c_int(v));
        }
        if let Some(f) = self.radio_frequency_hz {
            e = e.f64(S::RadioFrequency, f);
        }
        if let Some(f) = self.first_lo_hz {
            e = e.f64(S::FirstLoFrequency, f);
        }
        let floats = [
            (S::LowEdge, self.low_edge_hz),
            (S::HighEdge, self.high_edge_hz),
            (S::Gain, self.gain_db),
        ];
        for (tag, v) in floats {
            if let Some(v) = v {
                e = e.f32(tag, f64::from(v));
            }
        }
        if let Some(on) = self.agc_enable {
            e = e.bool(S::AgcEnable, on);
        }
        let floats = [(S::RfGain, self.rf_gain_db), (S::RfAtten, self.rf_atten_db)];
        for (tag, v) in floats {
            if let Some(v) = v {
                e = e.f32(tag, f64::from(v));
            }
        }
        if let Some(on) = self.snr_squelch {
            e = e.bool(S::SnrSquelch, on);
        }
        let floats = [
            (S::SquelchOpen, self.squelch_open_db),
            (S::SquelchClose, self.squelch_close_db),
        ];
        for (tag, v) in floats {
            if let Some(v) = v {
                e = e.f32(tag, f64::from(v));
            }
        }
        if let Some(v) = self.bin_count {
            e = e.i32(S::BinCount, c_int(v));
        }
        if let Some(v) = self.resolution_bw_hz {
            e = e.f32(S::ResolutionBw, f64::from(v));
        }
        let ints = [
            (S::WindowType, self.window_type),
            (S::SpectrumAvg, self.spectrum_avg),
        ];
        for (tag, v) in ints {
            if let Some(v) = v {
                e = e.i32(tag, c_int(v));
            }
        }
        let floats = [
            (S::SpectrumBase, self.spectrum_base_db),
            (S::SpectrumStep, self.spectrum_step_db),
        ];
        for (tag, v) in floats {
            if let Some(v) = v {
                e = e.f32(tag, f64::from(v));
            }
        }
        if let Some(v) = self.status_interval {
            e = e.i32(S::StatusInterval, c_int(v));
        }
        if let Some(a) = self.output_data_dest_socket {
            e = e.socket(S::OutputDataDestSocket, a);
        }
        if let Some(v) = self.lifetime {
            e = e.i32(S::Lifetime, c_int(v));
        }
        e.finish()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "the codec must be bit-exact")]
mod tests {
    use super::*;
    use crate::tlv::{Field, parse};
    use proptest::prelude::*;

    fn fields(pkt: &[u8]) -> Vec<Field<'_>> {
        let p = parse(pkt).unwrap();
        assert_eq!(p.kind, PktType::Cmd);
        p.fields().collect()
    }

    #[test]
    fn poll_bytes() {
        // CMD, OUTPUT_SSRC(18) len 2 0x1234, COMMAND_TAG(1) len 1 0x07, EOL
        let pkt = Command::poll(0x1234, 7).encode();
        assert_eq!(pkt, [1, 18, 2, 0x12, 0x34, 1, 1, 7, 0]);
        let c = Command::poll(0x1234, 7);
        assert_eq!(c.ssrc(), 0x1234);
        assert_eq!(c.tag(), 7);
        // Zero tag is length 0.
        assert_eq!(Command::poll(1, 0).encode(), [1, 18, 1, 1, 1, 0, 0]);
    }

    #[test]
    fn destroy_bytes() {
        // RADIO_FREQUENCY(33) 0.0 -> len 0, LIFETIME(117) 1 -> len 1 0x01
        let pkt = Command::destroy(5, 0x0102_0304).encode();
        assert_eq!(pkt, [1, 18, 1, 5, 1, 4, 1, 2, 3, 4, 33, 0, 117, 1, 1, 0]);
    }

    #[test]
    fn dump_all_bytes() {
        let pkt = Command::dump_all(9).encode();
        assert_eq!(pkt, [1, 18, 4, 0xff, 0xff, 0xff, 0xff, 1, 1, 9, 0]);
        assert_eq!(parse(&pkt).unwrap().ssrc(), Some(SSRC_ALL_CHANNELS));
    }

    #[test]
    fn linear_opus_channel_bytes() {
        let pkt = Command::new(1000, 1)
            .preset("usb")
            .output_encoding(Encoding::Opus)
            .radio_frequency_hz(10e6)
            .low_edge_hz(-2.5)
            .high_edge_hz(1.0)
            .lifetime(500)
            .encode();
        let want: Vec<u8> = [
            &[1u8][..],
            &[18, 2, 0x03, 0xe8],                         // OUTPUT_SSRC 1000
            &[1, 1, 1],                                   // COMMAND_TAG 1
            &[85, 3, b'u', b's', b'b'],                   // PRESET "usb"
            &[107, 1, 3],                                 // OUTPUT_ENCODING OPUS
            &[33, 8, 0x41, 0x63, 0x12, 0xd0, 0, 0, 0, 0], // RADIO_FREQUENCY double 10 MHz
            &[39, 4, 0xc0, 0x20, 0, 0],                   // LOW_EDGE float -2.5
            &[40, 4, 0x3f, 0x80, 0, 0],                   // HIGH_EDGE float 1.0
            &[117, 2, 0x01, 0xf4],                        // LIFETIME 500
            &[0],
        ]
        .concat();
        assert_eq!(pkt, want);
    }

    #[test]
    fn spectrum2_channel_round_trip() {
        let pkt = Command::new(2000, 0xabcd)
            .demod_type(DemodType::Spect2)
            .radio_frequency_hz(14.1e6)
            .bin_count(1620)
            .resolution_bw_hz(100.0)
            .window_type(5)
            .spectrum_avg(5)
            .spectrum_base_db(-120.0)
            .spectrum_step_db(0.5)
            .status_interval(5)
            .lifetime(1000)
            .encode();
        let f = fields(&pkt);
        let tags: Vec<StatusType> = f.iter().map(|f| f.status_type().unwrap()).collect();
        assert_eq!(
            tags,
            [
                StatusType::OutputSsrc,
                StatusType::CommandTag,
                StatusType::DemodType,
                StatusType::RadioFrequency,
                StatusType::BinCount,
                StatusType::ResolutionBw,
                StatusType::WindowType,
                StatusType::SpectrumAvg,
                StatusType::SpectrumBase,
                StatusType::SpectrumStep,
                StatusType::StatusInterval,
                StatusType::Lifetime,
            ]
        );
        // Decode each with the function radiod uses.
        assert_eq!(f[0].as_u32().unwrap(), 2000); // get_ssrc: decode_int32
        assert_eq!(f[1].as_u64().unwrap(), 0xabcd); // decode_int64
        assert_eq!(f[2].as_i32().unwrap(), 4); // decode_int
        assert_eq!(f[3].as_f64().unwrap(), 14.1e6); // decode_double
        assert_eq!(f[4].as_i32().unwrap(), 1620);
        assert_eq!(f[5].as_f32_or_f64().unwrap(), 100.0); // decode_float
        assert_eq!(f[6].as_i32().unwrap(), 5);
        assert_eq!(f[7].as_i32().unwrap(), 5);
        assert_eq!(f[8].as_f32_or_f64().unwrap(), -120.0);
        assert_eq!(f[9].as_f32_or_f64().unwrap(), 0.5);
        assert_eq!(f[10].as_i32().unwrap(), 5);
        assert_eq!(f[11].as_i32().unwrap(), 1000);
    }

    #[test]
    fn every_setter_round_trips_in_order() {
        use StatusType as S;
        let dest: SocketAddr = "239.9.8.7:5004".parse().unwrap();
        let pkt = Command::new(3, 4)
            .lifetime(10)
            .output_data_dest_socket(dest)
            .status_interval(2)
            .spectrum_step_db(0.25)
            .spectrum_base_db(-100.0)
            .spectrum_avg(3)
            .window_type(0)
            .resolution_bw_hz(50.0)
            .bin_count(64)
            .squelch_close_db(6.0)
            .squelch_open_db(8.0)
            .snr_squelch(true)
            .rf_atten_db(10.0)
            .rf_gain_db(20.0)
            .agc_enable(true)
            .gain_db(-3.0)
            .high_edge_hz(3000.0)
            .low_edge_hz(100.0)
            .radio_frequency_hz(7.074e6)
            .opus_bit_rate(32000)
            .output_encoding(Encoding::Opus)
            .output_channels(1)
            .output_samprate(12000)
            .demod_type(DemodType::Linear)
            .preset("usb")
            .encode();
        let f = fields(&pkt);
        let order: Vec<StatusType> = f.iter().map(|f| f.status_type().unwrap()).collect();
        assert_eq!(
            order,
            [
                S::OutputSsrc,
                S::CommandTag,
                S::Preset,
                S::DemodType,
                S::OutputSamprate,
                S::OutputChannels,
                S::OutputEncoding,
                S::OpusBitRate,
                S::RadioFrequency,
                S::LowEdge,
                S::HighEdge,
                S::Gain,
                S::AgcEnable,
                S::RfGain,
                S::RfAtten,
                S::SnrSquelch,
                S::SquelchOpen,
                S::SquelchClose,
                S::BinCount,
                S::ResolutionBw,
                S::WindowType,
                S::SpectrumAvg,
                S::SpectrumBase,
                S::SpectrumStep,
                S::StatusInterval,
                S::OutputDataDestSocket,
                S::Lifetime,
            ]
        );
        assert_eq!(f[2].as_str().unwrap(), "usb");
        assert_eq!(f[3].as_i32().unwrap(), 0);
        assert_eq!(f[3].value.len(), 0, "linear demod is zero -> length 0");
        assert_eq!(f[4].as_i32().unwrap(), 12000);
        assert_eq!(f[5].as_i32().unwrap(), 1);
        assert_eq!(f[6].as_i32().unwrap(), 3);
        assert_eq!(f[7].as_i32().unwrap(), 32000);
        assert_eq!(f[8].as_f64().unwrap(), 7.074e6);
        assert_eq!(f[9].as_f32_or_f64().unwrap(), 100.0);
        assert_eq!(f[10].as_f32_or_f64().unwrap(), 3000.0);
        assert_eq!(f[11].as_f32_or_f64().unwrap(), -3.0);
        assert!(f[12].as_bool().unwrap());
        assert_eq!(f[13].as_f32_or_f64().unwrap(), 20.0);
        assert_eq!(f[14].as_f32_or_f64().unwrap(), 10.0);
        assert!(f[15].as_bool().unwrap());
        assert_eq!(f[16].as_f32_or_f64().unwrap(), 8.0);
        assert_eq!(f[17].as_f32_or_f64().unwrap(), 6.0);
        assert_eq!(f[18].as_i32().unwrap(), 64);
        assert_eq!(f[19].as_f32_or_f64().unwrap(), 50.0);
        assert_eq!(f[20].as_i32().unwrap(), 0);
        assert_eq!(f[21].as_i32().unwrap(), 3);
        assert_eq!(f[22].as_f32_or_f64().unwrap(), -100.0);
        assert_eq!(f[23].as_f32_or_f64().unwrap(), 0.25);
        assert_eq!(f[24].as_i32().unwrap(), 2);
        assert_eq!(f[25].as_socket().unwrap(), dest);
        assert_eq!(f[26].as_i32().unwrap(), 10);
        assert_eq!(*pkt.last().unwrap(), 0);
    }

    #[test]
    fn large_ints_are_clamped_to_c_int() {
        let pkt = Command::new(1, 1).lifetime(u32::MAX).encode();
        let f = fields(&pkt);
        assert_eq!(f[2].as_i32().unwrap(), i32::MAX);
        assert_eq!(f[2].value, [0x7f, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn nan_floats_are_omitted() {
        let pkt = Command::new(1, 1)
            .radio_frequency_hz(f64::NAN)
            .low_edge_hz(f32::NAN)
            .encode();
        assert_eq!(pkt, Command::poll(1, 1).encode());
    }

    #[test]
    fn setters_override() {
        let pkt = Command::new(1, 1).preset("am").preset("fm").encode();
        let f = fields(&pkt);
        assert_eq!(f.len(), 3);
        assert_eq!(f[2].as_str().unwrap(), "fm");
    }

    proptest! {
        #[test]
        fn round_trip_arbitrary(
            ssrc: u32, tag: u32, freq in 0.0f64..6e9, low in -24000.0f32..0.0,
            high in 0.0f32..24000.0, rate in 0u32..=192_000, life: u32,
            bins in 0u32..=65535, preset in "[a-z0-9]{0,40}", agc: bool,
        ) {
            let pkt = Command::new(ssrc, tag)
                .preset(preset.clone())
                .radio_frequency_hz(freq)
                .low_edge_hz(low)
                .high_edge_hz(high)
                .output_samprate(rate)
                .bin_count(bins)
                .agc_enable(agc)
                .lifetime(life)
                .encode();
            let p = parse(&pkt).unwrap();
            prop_assert_eq!(p.ssrc(), Some(ssrc));
            prop_assert_eq!(p.command_tag(), Some(u64::from(tag)));
            let get = |t| p.last(t).unwrap();
            prop_assert_eq!(get(StatusType::Preset).as_str().unwrap(), preset.as_str());
            prop_assert_eq!(get(StatusType::RadioFrequency).as_f64().unwrap(), freq);
            prop_assert_eq!(get(StatusType::LowEdge).as_f32_or_f64().unwrap(), f64::from(low));
            prop_assert_eq!(get(StatusType::HighEdge).as_f32_or_f64().unwrap(), f64::from(high));
            prop_assert_eq!(get(StatusType::OutputSamprate).as_i32().unwrap(), c_int(rate));
            prop_assert_eq!(get(StatusType::BinCount).as_i32().unwrap(), c_int(bins));
            prop_assert_eq!(get(StatusType::AgcEnable).as_bool().unwrap(), agc);
            prop_assert_eq!(get(StatusType::Lifetime).as_i32().unwrap(), c_int(life));
            prop_assert_eq!(pkt.last(), Some(&0));
        }
    }
}
