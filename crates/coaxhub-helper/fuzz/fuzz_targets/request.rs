// SPDX-License-Identifier: GPL-3.0-only
//! Fuzzes the helper's request parser, the one input that root acts on:
//! arbitrary bytes must never panic, and an accepted line must be exactly
//! what the protocol writes (canonical, at most 64 bytes, printable ASCII).
#![no_main]

use coaxhub_helper::protocol::{MAX_REQUEST_BYTES, Reply, Request};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(request) = Request::parse(data) {
        let line = request.encode();
        assert_eq!(line.as_bytes(), data);
        assert!(data.len() <= MAX_REQUEST_BYTES);
        assert!(data.ends_with(b"\n"));
        assert!(data[..data.len() - 1].iter().all(|b| (0x20..=0x7e).contains(b)));
    }
    if let Ok(text) = std::str::from_utf8(data) {
        if let Some(Reply::Error(detail)) = Reply::parse(text) {
            // Whatever came back, a reply built from it is one line.
            assert_eq!(Reply::Error(detail).encode().matches('\n').count(), 1);
        }
    }
});
