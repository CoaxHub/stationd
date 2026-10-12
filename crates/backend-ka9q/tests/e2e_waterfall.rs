// SPDX-License-Identifier: GPL-3.0-only
//! Waterfall labels against a real radiod: for many ranges and bin counts
//! around the signal generator's carrier, in wideband and narrowband mode,
//! the bin a listener would label with the carrier's frequency (from
//! `WaterfallSubscribed.range` in equal bins) is the bin radiod puts it in
//! (radiod-notes.md §10 "Waterfall geometry"). Run by
//! `scripts/e2e-station.sh`; skipped unless `COAXHUB_E2E=1`.
//!
//! - `COAXHUB_E2E_STATUS_GROUP`: radiod's status group (default
//!   `239.77.0.6:5006`);
//! - `COAXHUB_E2E_CARRIER_HZ`: the carrier (default 10 MHz, as in
//!   `testdata/radiod-siggen.conf`).

#![allow(
    clippy::cast_precision_loss,
    reason = "frequencies and bin indices are far below 2^52"
)]

use std::net::SocketAddr;
use std::time::Duration;

use backend_ka9q::backend::{Ka9qBackend, Ka9qConfig};
use futures_util::StreamExt as _;
use station_backend::Backend;
use station_proto::v1;
use station_proto::validate::WaterfallDecoder;

fn enabled() -> bool {
    std::env::var_os("COAXHUB_E2E").is_some_and(|v| v == "1")
}

fn status_group() -> SocketAddr {
    std::env::var("COAXHUB_E2E_STATUS_GROUP")
        .unwrap_or_else(|_| "239.77.0.6:5006".to_owned())
        .parse()
        .expect("COAXHUB_E2E_STATUS_GROUP is ip:port")
}

fn carrier_hz() -> u64 {
    std::env::var("COAXHUB_E2E_CARRIER_HZ").map_or(10_000_000, |v| {
        v.parse().expect("COAXHUB_E2E_CARRIER_HZ in Hz")
    })
}

/// The requests: the full span, then spans of several widths with the
/// carrier at several places in them (off any grid), with even and odd bin
/// counts. Bins of 200 Hz or less are narrowband.
fn requests(tunable: v1::FrequencyRange, carrier: u64) -> Vec<(v1::FrequencyRange, u32)> {
    let mut out = vec![(tunable, 1024), (tunable, 1023), (tunable, 2048)];
    let bins = [1024, 1023, 701, 64, 4096];
    let spans: [u64; 6] = [2_000_000, 512_000, 150_000, 40_000, 100_000, 20_000];
    let mut n = 0;
    for span in spans {
        for (j, frac) in [0.13, 0.37, 0.5, 0.71, 0.93].into_iter().enumerate() {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a fraction of the span"
            )]
            let below = (span as f64 * frac) as u64 + 137 * j as u64;
            let Some(start) = carrier.checked_sub(below) else {
                continue;
            };
            let range = v1::FrequencyRange {
                start_hz: start,
                end_hz: start + span,
            };
            if range.start_hz < tunable.start_hz || range.end_hz > tunable.end_hz {
                continue;
            }
            // Narrowband spans keep their bins at 200 Hz or less.
            let b = if span <= 100_000 {
                [1024, 1023][n % 2]
            } else {
                bins[n % bins.len()]
            };
            out.push((range, b));
            n += 1;
        }
    }
    out
}

/// The strongest bin of a row; the middle one when several share it.
fn peak(row: &[u8]) -> usize {
    let max = row.iter().copied().max().unwrap_or(0);
    let at: Vec<usize> = (0..row.len()).filter(|&i| row[i] == max).collect();
    at[at.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
async fn waterfall_labels_match_radiod() {
    if !enabled() {
        eprintln!("skipped: set COAXHUB_E2E=1 and run radiod (scripts/e2e-station.sh)");
        return;
    }
    let backend = Ka9qBackend::new(Ka9qConfig::new(status_group(), None)).expect("backend");
    let caps = backend.capabilities().await.expect("capabilities");
    let tunable = caps.receiver.tunable_range.expect("tunable range");
    let carrier = carrier_hz();
    let mut failures = Vec::new();
    let mut strict = 0;
    let requests = requests(tunable, carrier);
    for (id, (range, bins)) in (1u32..).zip(&requests) {
        let req = v1::SubscribeWaterfall {
            subscription_id: id,
            receiver_id: 1,
            range: Some(*range),
            bins: *bins,
            row_interval_ms: 50,
            quantization: None,
            compression: v1::WaterfallCompression::Zstd as i32,
        };
        let (sub, mut rows) = backend.subscribe_waterfall(req).await.expect("subscribe");
        let covered = sub.range.expect("range");
        let width = (covered.end_hz - covered.start_hz) as f64 / f64::from(sub.bins);
        let label = |i: usize| covered.start_hz as f64 + (i as f64 + 0.5) * width;
        let mut decoder = WaterfallDecoder::new();
        let mut row = Vec::new();
        let mut peaks = Vec::new();
        for _ in 0..4 {
            let r = tokio::time::timeout(Duration::from_secs(2), rows.next())
                .await
                .expect("a row within 2 s")
                .expect("row stream open");
            decoder
                .decode(&r.payload, sub.compression, sub.bins, &mut row)
                .expect("decodable row");
            peaks.push(peak(&row));
        }
        backend
            .unsubscribe_waterfall(id)
            .await
            .expect("unsubscribe");
        // The bin seen most often (the first row of a channel can be off).
        let p = *peaks
            .iter()
            .max_by_key(|p| peaks.iter().filter(|q| q == p).count())
            .expect("rows");
        // The labelled bin the carrier is in, and where in it. Within 0.15
        // bins of an edge either neighbour may win (noise, the window's
        // leakage), and the neighbour's centre is then up to 0.65 bins off;
        // elsewhere the peak must be that bin exactly.
        let pos = (carrier as f64 - covered.start_hz as f64) / width;
        let near_edge = !(0.15..=0.85).contains(&pos.fract());
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a bin index, checked below"
        )]
        let expected = pos.max(0.0) as usize;
        let error = label(p) - carrier as f64;
        let ok = if near_edge {
            error.abs() <= 0.65 * width
        } else {
            strict += 1;
            p == expected
        };
        let inside = covered.start_hz >= range.start_hz && covered.end_hz <= range.end_hz;
        println!(
            "E2E waterfall request={}..{} bins={bins} covered={}..{} bin_hz={width:.2} \
             expected={expected} near_edge={near_edge} peak={p} label_error_hz={error:.1} \
             peaks={peaks:?}",
            range.start_hz, range.end_hz, covered.start_hz, covered.end_hz
        );
        if !ok || !inside {
            failures.push(format!(
                "{range:?} in {bins} bins: covered {covered:?}, carrier in bin {p}, labelled \
                 {error:.1} Hz off, expected bin {expected} ({width:.2} Hz bins)"
            ));
        }
    }
    println!(
        "E2E waterfall_label_checks={} exact={strict} failures={}",
        requests.len(),
        failures.len()
    );
    assert!(failures.is_empty(), "{failures:#?}");
}
