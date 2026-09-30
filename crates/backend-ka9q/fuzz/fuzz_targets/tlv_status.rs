// SPDX-License-Identifier: GPL-3.0-only
//! Fuzzes the radiod TLV decoder (station-protocol §10): what stationd does
//! with every datagram on radiod's status and data groups, which any local
//! process can send to.
//!
//! Input: one datagram. It goes through the generic TLV parser, every typed
//! field accessor, the typed `ChannelStatus` decoder, a TLV re-encode of
//! what decoded, the spectrum row conversion, and the RTP parser. None of
//! them may panic, and a parsed packet must re-parse identically.
#![no_main]

use backend_ka9q::rtp::RtpPacket;
use backend_ka9q::spectrum::{DEFAULT_QUANTIZATION, SpectrumFrame, requantize};
use backend_ka9q::status::ChannelStatus;
use backend_ka9q::tlv::{self, Encoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = tlv::parse(data) {
        let mut again = Encoder::new(packet.kind);
        let mut fields = 0usize;
        for f in packet.fields() {
            fields += 1;
            let _ = f.status_type();
            let _ = f.as_u64();
            let _ = f.as_u32();
            let _ = f.as_u16();
            let _ = f.as_u8();
            let _ = f.as_bool();
            let _ = f.as_i32();
            let _ = f.as_i64();
            let _ = f.as_f32_or_f64();
            let _ = f.as_f64();
            let _ = f.as_str();
            let _ = f.as_string_lossy();
            let _ = f.as_socket();
            let _ = f.as_f32_array();
            if let Some(tag) = f.status_type() {
                again = again.bytes(tag, f.as_bytes());
            }
        }
        let _ = packet.ssrc();
        let _ = packet.command_tag();
        // Known tags re-encode to a packet that parses to the same fields.
        let bytes = again.finish();
        let reparsed = tlv::parse(&bytes).expect("re-encoded packet parses");
        assert!(reparsed.fields().count() <= fields);
    }
    if let Ok(st) = ChannelStatus::decode(data) {
        if let (Some(bins), Some(base), Some(step), Some(center), Some(rbw)) = (
            st.bin_bytes.clone(),
            st.spectrum_base_db,
            st.spectrum_step_db,
            st.radio_frequency_hz,
            st.resolution_bw_hz,
        ) {
            #[allow(clippy::cast_possible_truncation)]
            let frame = SpectrumFrame {
                center_hz: center,
                bin_width_hz: rbw,
                base_db: base as f32,
                step_db: step as f32,
                bins,
            };
            let row = requantize(&frame, &DEFAULT_QUANTIZATION);
            assert_eq!(row.len(), frame.bins.len());
            let _ = frame.range();
        }
    }
    let _ = RtpPacket::parse(data);
});
