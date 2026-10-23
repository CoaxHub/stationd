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
    // Request ids are those of the target's busy session.
    let receiver = |receiver_id, start_hz| v1::ReceiverCapabilities {
        receiver_id,
        tunable_range: Some(range(start_hz, start_hz + 2_048_000)),
        hardware_range: Some(range(0, 28_800_000)),
        sample_rate_hz: 2_048_000,
        max_channels: 16,
        frontend: Some(v1::FrontendControls {
            can_retune: true,
            ..Default::default()
        }),
        sdr_model: "RTL-SDR HF direct sampling".into(),
    };
    let capabilities = ok(
        8,
        Payload::Capabilities(v1::Capabilities {
            receivers: vec![receiver(1, 6_076_000), receiver(2, 13_176_000)],
            modes: vec![1, 2, 3, 4, 5, 6],
            codecs: vec![1],
            waterfall: Some(v1::WaterfallLimits {
                max_bins: 4096,
                min_row_interval_ms: 50,
                max_subscriptions: 8,
            }),
            measurements: Some(v1::MeasurementLimits {
                max_noise_bands: 32,
                method_version: 1,
            }),
            cw_pitch_hz: 500,
        }),
    );
    let sdr = |serial: &str, product: &str, models: Vec<v1::ModelOption>, receiver_id| v1::Sdr {
        usb_vendor_id: 0x0bda,
        usb_product_id: 0x2838,
        manufacturer: "RTLSDRBlog".into(),
        product: product.into(),
        serial: serial.into(),
        port: "1-1.3".into(),
        models,
        receiver_id,
    };
    let option = |model: v1::SdrModel, profiles: &[v1::SdrProfile]| v1::ModelOption {
        model: model as i32,
        profiles: profiles.iter().map(|p| *p as i32).collect(),
    };
    let v3 = vec![
        option(
            v1::SdrModel::RtlsdrDirectSampling,
            &[v1::SdrProfile::Rtlsdr, v1::SdrProfile::RtlsdrHf],
        ),
        option(v1::SdrModel::RtlsdrOther, &[v1::SdrProfile::Rtlsdr]),
    ];
    let profile = |profile: v1::SdrProfile, start_hz, end_hz, default_center_hz| v1::ProfileInfo {
        profile: profile as i32,
        center_range: Some(range(start_hz, end_hz)),
        default_center_hz,
        sample_rate_hz: 2_048_000,
        tunable_span_hz: 2_048_000,
        frontend: Some(v1::FrontendControls {
            can_retune: true,
            ..Default::default()
        }),
    };
    let hardware = ok(
        19,
        Payload::Hardware(v1::Hardware {
            sdrs: vec![
                sdr("00000001", "RTL2838UHIDIR", v3.clone(), Some(1)),
                sdr("00000002", "RTL2838UHIDIR", v3, Some(2)),
                sdr(
                    "00000003",
                    "Blog V4",
                    vec![option(
                        v1::SdrModel::RtlsdrBlogV4,
                        &[v1::SdrProfile::RtlsdrV4],
                    )],
                    None,
                ),
            ],
            profiles: vec![
                profile(
                    v1::SdrProfile::Rtlsdr,
                    24_000_000,
                    1_766_000_000,
                    98_000_000,
                ),
                profile(v1::SdrProfile::RtlsdrHf, 1_024_000, 28_800_000, 7_100_000),
                profile(v1::SdrProfile::RtlsdrV4, 500_000, 1_766_000_000, 7_100_000),
            ],
        }),
    );
    let receivers_configured = ok(
        20,
        Payload::ReceiversConfigured(v1::ReceiversConfigured {
            receivers: [1, 2, 3]
                .into_iter()
                .map(|receiver_id| v1::ReceiverStatus {
                    receiver_id,
                    state: v1::ReceiverState::Starting as i32,
                    detail: String::new(),
                })
                .collect(),
        }),
    );
    let receiver_state = Body::Event(v1::Event {
        kind: v1::event::Kind::ReceiverState as i32,
        receiver_id: Some(2),
        receiver_state: Some(v1::ReceiverState::NoDevice as i32),
        detail: "SDR 00000002 is not plugged in".into(),
        ..Default::default()
    });
    let pong = ok(
        15,
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
    let health = |receiver_id| v1::ReceiverHealth {
        receiver_id,
        state: v1::ReceiverState::Running as i32,
        detail: String::new(),
        backend: Some(v1::BackendHealth {
            state: 1,
            version: "ka9q".into(),
        }),
        frontend: Some(v1::FrontendHealth {
            state: 1,
            gain_db: 30.0,
            ..Default::default()
        }),
        active_channels: 1,
        backend_cpu: Some(0.2),
    };
    let status = Body::Status(v1::StationStatus {
        uptime_s: 60,
        host: Some(v1::HostHealth {
            load_avg_1m: 0.3,
            cpu_count: 4,
            cpu_temp_c: Some(45.0),
            ..Default::default()
        }),
        receivers: vec![health(1), health(2)],
        ..Default::default()
    });
    let closed = Body::Event(v1::Event {
        kind: v1::event::Kind::ChannelClosed as i32,
        channel_id: Some(3),
        subscription_id: None,
        detail: "passband moved".into(),
        ..Default::default()
    });
    let updated = ok(
        10,
        Payload::ChannelUpdated(v1::ChannelUpdated {
            channel_id: 1,
            frequency_hz: 7_150_000,
            mode: 4,
            low_cut_hz: 200,
            high_cut_hz: 2800,
        }),
    );
    let subscribed = ok(
        13,
        Payload::WaterfallSubscribed(v1::WaterfallSubscribed {
            subscription_id: 3,
            receiver_id: 1,
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

    // Plan 1 of the target's busy session (receiver 1 idle, band 1).
    let frontend = |receiver_id| v1::FrontendState {
        receiver_id,
        ..Default::default()
    };
    let noise = |window_start_unix_ms| {
        Body::Noise(v1::NoiseFloorReport {
            measurement_id: 1,
            receiver_id: 1,
            window_start_unix_ms,
            window_ms: 60_000,
            frontend: Some(frontend(1)),
            overloads: 0,
            clock: Some(v1::ClockState {
                synchronized: true,
                max_error_ms: 5,
            }),
            bands: vec![v1::BandNoise {
                band_id: 1,
                measured: Some(range(7_000_000, 7_300_000)),
                resolution_bw_hz: 500.0,
                samples: 72_000,
                p10_db_hz: -132.5,
                p25_db_hz: -131.0,
                p50_db_hz: -129.9,
                p90_db_hz: -110.0,
            }],
            partial: false,
            cycle_minutes: 1,
            cycle_index: 0,
        })
    };
    let duty = Body::Event(v1::Event {
        kind: v1::event::Kind::MeasurementDuty as i32,
        receiver_id: Some(1),
        measurement_id: Some(1),
        measurement_duty: Some(v1::MeasurementDuty::Continuous as i32),
        effective_unix_ms: Some(1_790_942_460_000),
        ..Default::default()
    });
    let stopped = Body::Event(v1::Event {
        kind: v1::event::Kind::MeasurementStopped as i32,
        measurement_id: Some(1),
        measurement_stop_reason: Some(v1::MeasurementStopReason::FrontendChanged as i32),
        ..Default::default()
    });
    let started = ok(
        17,
        Payload::MeasurementsStarted(v1::MeasurementsStarted {
            measurement_id: 2,
            noise_bands: vec![v1::NoiseBand {
                band_id: 2,
                receiver_id: 2,
                ranges: vec![range(14_000_000, 14_350_000)],
                key: "20m".into(),
            }],
            receivers: vec![v1::MeasuredReceiver {
                receiver_id: 2,
                noise_interval_ms: 60_000,
                duty: v1::MeasurementDuty::Continuous as i32,
                frontend: Some(frontend(2)),
            }],
            method_version: 1,
        }),
    );
    let mut measuring = Vec::new();
    frame(&mut measuring, 0, noise(1_790_942_400_000))?;
    frame(&mut measuring, 0, duty.clone())?;
    frame(&mut measuring, 250, noise(1_790_942_460_000))?;
    frame(&mut measuring, 0, started.clone())?;
    frame(&mut measuring, 0, stopped.clone())?;
    frame(&mut measuring, 0, noise(1_790_942_460_000))?;
    write(dir, "measuring", &measuring)?;

    // The receiver set: hardware, the new set accepted, a receiver's state.
    let mut receivers = Vec::new();
    frame(&mut receivers, 0, hardware.clone())?;
    frame(&mut receivers, 0, receivers_configured.clone())?;
    frame(&mut receivers, 1, receiver_state.clone())?;
    frame(&mut receivers, 0, capabilities.clone())?;
    frame(&mut receivers, 250, status.clone())?;
    write(dir, "receivers", &receivers)?;

    let singles: Vec<(&str, Body)> = vec![
        ("response_hardware", hardware),
        ("response_receivers_configured", receivers_configured),
        ("event_receiver_state", receiver_state),
        ("noise", noise(1_790_942_400_000)),
        ("event_measurement_duty", duty),
        ("event_measurement_stopped", stopped),
        ("response_measurements_started", started),
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
