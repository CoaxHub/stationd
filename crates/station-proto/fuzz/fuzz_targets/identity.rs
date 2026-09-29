// SPDX-License-Identifier: MIT
//! Fuzzes the station-ID extraction from client certificates
//! (station-protocol §8): arbitrary DER must never panic, and an
//! accepted ID must round-trip through its canonical URI.
#![no_main]

use libfuzzer_sys::fuzz_target;
use station_proto::identity::{station_id_from_cert_der, station_id_from_uri, station_uri};

fuzz_target!(|data: &[u8]| {
    if let Ok(id) = station_id_from_cert_der(data) {
        assert!(!id.is_nil());
        assert_eq!(station_id_from_uri(&station_uri(id)), Ok(id));
    }
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = station_id_from_uri(s);
    }
});
