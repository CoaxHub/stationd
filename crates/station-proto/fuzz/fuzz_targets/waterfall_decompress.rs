// SPDX-License-Identifier: MIT
//! Fuzzes bounded waterfall decompression (station-protocol §5
//! `WaterfallRow`, zip-bomb guard).
//!
//! Input: `[bins: u16 LE][compression: u8][payload...]`. A decoder is reused
//! across inputs (as the hub's session does), so recovery after errors is
//! exercised too. The output is never more than `bins` bytes on success and
//! the buffer never grows past `bins + 1`.
#![no_main]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use station_proto::validate::WaterfallDecoder;

thread_local! {
    static DECODER: RefCell<(WaterfallDecoder, Vec<u8>)> = RefCell::new((WaterfallDecoder::new(), Vec::new()));
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let bins = u32::from(u16::from_le_bytes([data[0], data[1]]));
    // 0 = UNSPECIFIED, 1 = NONE, 2 = ZSTD, 3 = unknown; mostly ZSTD.
    let compression = match data[2] % 8 {
        0 => 0,
        1 => 1,
        7 => 3,
        _ => 2,
    };
    let payload = &data[3..];
    DECODER.with(|cell| {
        let mut guard = cell.borrow_mut();
        let (decoder, out) = &mut *guard;
        let result = decoder.decode(payload, compression, bins, out);
        if result.is_ok() {
            assert_eq!(out.len() as u64, u64::from(bins));
        }
        assert!(out.len() <= bins as usize + 1);
        assert!(out.capacity() <= 16_385 + 64);
    });
});
