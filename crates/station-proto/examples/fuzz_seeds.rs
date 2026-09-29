// SPDX-License-Identifier: MIT
//! Writes the committed seed corpus of the cargo-fuzz targets:
//!
//! ```text
//! cargo run -p coaxhub-station-proto --example fuzz_seeds [-- <corpus dir>]
//! ```
//!
//! The default directory is `crates/station-proto/fuzz/corpus`. Seeds are a
//! few valid inputs per target, in each target's input format (see the
//! target's doc comment); certificates are generated fresh with `rcgen`, so
//! re-running changes the identity seeds.

use std::error::Error;
use std::path::{Path, PathBuf};

use prost::Message as _;
use rcgen::{CertificateParams, KeyPair, SanType};
use station_proto::identity::STATION_ID_URI_PREFIX;
use station_proto::v1::{self, response::Payload, station_message::Body};

type Res<T> = Result<T, Box<dyn Error>>;

fn main() -> Res<()> {
    let dir = std::env::args().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus"),
        PathBuf::from,
    );
    station_message_seeds(&dir.join("station_message"))?;
    waterfall_seeds(&dir.join("waterfall_decompress"))?;
    identity_seeds(&dir.join("identity"))?;
    println!("seed corpus written to {}", dir.display());
    Ok(())
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> Res<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(name), bytes)?;
    Ok(())
}

// ------------------------------------------------------------------ station_message

/// One frame of the `station_message` target: `[len u16 LE][step u8][bytes]`.
fn frame(out: &mut Vec<u8>, step: u8, body: Body) -> Res<()> {
    let bytes = v1::StationMessage { body: Some(body) }.encode_to_vec();
    out.extend_from_slice(&u16::try_from(bytes.len())?.to_le_bytes());
    out.push(step);
    out.extend_from_slice(&bytes);
    Ok(())
}

fn ok(request_id: u64, payload: Payload) -> Body {
    Body::Response(v1::Response {
        request_id,
        result: Some(v1::Result {
            code: v1::ResultCode::Ok as i32,
            message: String::new(),
        }),
        payload: Some(payload),
    })
}

fn range(start_hz: u64, end_hz: u64) -> v1::FrequencyRange {
    v1::FrequencyRange { start_hz, end_hz }
}

fn row(bins: usize) -> Res<Vec<u8>> {
    let raw: Vec<u8> = (0..bins)
        .map(|i| u8::try_from(i % 97).unwrap_or(0))
        .collect();
    Ok(zstd::bulk::compress(&raw, 3)?)
}

#[allow(clippy::too_many_lines)] // one table of seeds
fn station_message_seeds(dir: &Path) -> Res<()> {
    let hello = Body::Hello(v1::Hello {
        protocol_version: Some(station_proto::PROTOCOL_VERSION),
        daemon_version: "0.1.0".into(),
        backend: Some(v1::BackendInfo {
            kind: v1::BackendKind::Ka9qRadio as i32,
            version: "ka9q".into(),
        }),
        platform: "linux-aarch64".into(),
        boot_id: 1,
    });
    let capabilities = ok(
        6,
        Payload::Capabilities(v1::Capabilities {
            tunable_range: Some(range(0, 2_400_000)),
            hardware_range: Some(range(0, 1_766_000_000)),
            sample_rate_hz: 2_400_000,
            max_channels: 16,
            modes: vec![1, 2, 3, 4, 5, 6],
            codecs: vec![1],
            waterfall: Some(v1::WaterfallLimits {
                max_bins: 4096,
                min_row_interval_ms: 50,
                max_subscriptions: 4,
            }),
            frontend: Some(v1::FrontendControls {
                has_gain: true,
                max_gain_db: 49.6,
                antenna_ports: vec!["A".into()],
                ..Default::default()
            }),
            allowed_admin_ops: vec![],
            frontend_control_allowed: true,
            sdr_model: "RTL-SDR".into(),
        }),
    );
    let pong = ok(
        13,
        Payload::Pong(v1::Pong {
            nonce: 5,
            station_time_unix_ms: 0,
        }),
    );
    let audio = |channel_id| {
        Body::Audio(v1::AudioFrame {
            channel_id,
            seq: 1,
            timestamp: 960,
            payload: vec![0xfc; 80],
        })
    };
    let waterfall = |subscription_id| -> Res<Body> {
        Ok(Body::Waterfall(v1::WaterfallRow {
            subscription_id,
            seq: 1,
            timestamp_us: 100_000,
            payload: row(256)?,
        }))
    };
    let signal = Body::Signal(v1::ChannelSignal {
        channel_id: 1,
        snr_db: 12.5,
        power_dbfs: -40.0,
        squelch_open: Some(true),
    });
    let status = Body::Status(v1::StationStatus {
        uptime_s: 60,
        host: Some(v1::HostHealth {
            load_avg_1m: 0.3,
            cpu_count: 4,
            cpu_temp_c: Some(45.0),
            ..Default::default()
        }),
        backend: Some(v1::BackendHealth {
            state: 1,
            version: "ka9q".into(),
        }),
        frontend: Some(v1::FrontendHealth {
            state: 1,
            gain_db: 30.0,
            ..Default::default()
        }),
        ..Default::default()
    });
    let closed = Body::Event(v1::Event {
        kind: v1::event::Kind::ChannelClosed as i32,
        channel_id: Some(3),
        subscription_id: None,
        detail: "passband moved".into(),
    });
    let updated = ok(
        8,
        Payload::ChannelUpdated(v1::ChannelUpdated {
            channel_id: 1,
            frequency_hz: 7_150_000,
            mode: 4,
            low_cut_hz: 200,
            high_cut_hz: 2800,
        }),
    );
    let subscribed = ok(
        11,
        Payload::WaterfallSubscribed(v1::WaterfallSubscribed {
            subscription_id: 3,
            range: Some(range(7_000_000, 7_300_000)),
            bins: 256,
            row_interval_ms: 100,
            quantization: Some(v1::Quantization {
                offset_db: -130.0,
                step_db: 0.5,
            }),
            compression: v1::WaterfallCompression::Zstd as i32,
        }),
    );

    let singles: Vec<(&str, Body)> = vec![
        ("hello", hello.clone()),
        ("response_capabilities", capabilities),
        ("response_pong", pong.clone()),
        ("audio", audio(1)),
        ("waterfall", waterfall(1)?),
        ("signal", signal.clone()),
        ("status", status.clone()),
        ("event_channel_closed", closed.clone()),
    ];
    for (name, body) in singles {
        let mut out = Vec::new();
        frame(&mut out, 1, body)?;
        write(dir, name, &out)?;
    }

    let mut mixed = Vec::new();
    for _ in 0..3 {
        frame(&mut mixed, 1, audio(1))?;
    }
    frame(&mut mixed, 1, signal)?;
    frame(&mut mixed, 0, updated)?;
    frame(&mut mixed, 5, status)?;
    frame(&mut mixed, 0, closed)?;
    frame(&mut mixed, 0, pong)?;
    write(dir, "mixed_session", &mixed)?;

    let mut sub = Vec::new();
    frame(&mut sub, 0, subscribed)?;
    frame(&mut sub, 5, waterfall(3)?)?;
    frame(&mut sub, 5, waterfall(3)?)?;
    write(dir, "subscribe_then_rows", &sub)?;

    // Hello first, for the fresh session.
    let mut fresh = Vec::new();
    frame(&mut fresh, 0, hello)?;
    frame(&mut fresh, 1, audio(2))?;
    write(dir, "hello_then_audio", &fresh)?;
    Ok(())
}

// ------------------------------------------------------------------ waterfall_decompress

/// `[bins u16 LE][compression u8][payload]`; compression byte 2 = ZSTD, 1 = NONE.
fn waterfall_input(bins: u16, compression: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = bins.to_le_bytes().to_vec();
    out.push(compression);
    out.extend_from_slice(payload);
    out
}

fn waterfall_seeds(dir: &Path) -> Res<()> {
    write(dir, "zstd_256", &waterfall_input(256, 2, &row(256)?))?;
    let raw: Vec<u8> = (0..300u16)
        .map(|i| u8::try_from(i % 200).unwrap_or(0))
        .collect();
    let streamed = zstd::stream::encode_all(&raw[..], 3)?;
    write(
        dir,
        "zstd_300_no_content_size",
        &waterfall_input(300, 2, &streamed),
    )?;
    write(dir, "none_64", &waterfall_input(64, 1, &raw[..64]))?;
    // 1 MiB of zeros declared for a 1024-bin row: the zip-bomb guard.
    let bomb = zstd::bulk::compress(&vec![0u8; 1 << 20], 19)?;
    write(dir, "zstd_bomb", &waterfall_input(1024, 2, &bomb))?;
    Ok(())
}

// ------------------------------------------------------------------ identity

fn cert(sans: Vec<SanType>) -> Res<Vec<u8>> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans;
    Ok(params.self_signed(&key)?.der().to_vec())
}

fn identity_seeds(dir: &Path) -> Res<()> {
    let id = "0b0e5d0c-6f3a-4b8e-8d21-6c2f9a1e7b44";
    let uri = |s: String| -> Res<SanType> { Ok(SanType::URI(s.try_into()?)) };
    write(
        dir,
        "valid",
        &cert(vec![uri(format!("{STATION_ID_URI_PREFIX}{id}"))?])?,
    )?;
    write(dir, "no_san", &cert(vec![])?)?;
    write(
        dir,
        "two_station_uris",
        &cert(vec![
            uri(format!("{STATION_ID_URI_PREFIX}{id}"))?,
            uri(format!("{STATION_ID_URI_PREFIX}{id}"))?,
        ])?,
    )?;
    write(
        dir,
        "dns_and_other_uri",
        &cert(vec![
            SanType::DnsName("station.example".try_into()?),
            uri("urn:example:1".into())?,
        ])?,
    )?;
    Ok(())
}
