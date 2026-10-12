// SPDX-License-Identifier: MIT
//! Receivers (station-protocol §4.2, §5, §5.1): the rules of a receiver set
//! against the hardware, the `Hardware` report, which receivers the hub
//! serves, and the lifetime of a receiver ID in the session.

// Test tables are long by nature.
#![allow(clippy::too_many_lines)]

mod common;

use std::time::Instant;

use common::*;
use station_proto::limits;
use station_proto::v1::{self, request::Command, response::Payload};
use station_proto::validate::{
    DropReason, MessageKind, OverlappingReceiver, ReceiverSetError, Verdict, ViolationKind,
    check_message, check_receiver_set, predicted_tunable_range, select_receivers,
};

use v1::{SdrModel as M, SdrProfile as P};

fn violation(v: Verdict) -> ViolationKind {
    match v {
        Verdict::Violation(k) => k,
        other => panic!("expected a violation, got {other:?}"),
    }
}

fn rx(receiver_id: u32, serial: &str, model: M, profile: P, center: u64) -> v1::ReceiverConfig {
    v1::ReceiverConfig {
        receiver_id,
        sdr_serial: serial.into(),
        model: model as i32,
        profile: profile as i32,
        center_frequency_hz: center,
        ..Default::default()
    }
}

fn set(receivers: Vec<v1::ReceiverConfig>) -> v1::ConfigureReceivers {
    v1::ConfigureReceivers { receivers }
}

/// 40 m on the first V3 (direct sampling), 20 m on the second, 2 m on the V4.
fn good_set() -> v1::ConfigureReceivers {
    set(vec![
        rx(
            1,
            "00000001",
            M::RtlsdrDirectSampling,
            P::RtlsdrHf,
            7_100_000,
        ),
        rx(
            2,
            "00000002",
            M::RtlsdrDirectSampling,
            P::RtlsdrHf,
            14_200_000,
        ),
        rx(3, "00000003", M::RtlsdrBlogV4, P::RtlsdrV4, 145_000_000),
    ])
}

// ------------------------------------------------------------------ check_receiver_set

#[test]
fn a_good_set_passes() {
    let hw = hardware();
    assert_eq!(check_receiver_set(&good_set(), &hw), Ok(()));
    // An empty set stops every receiver.
    assert_eq!(check_receiver_set(&set(vec![]), &hw), Ok(()));
    assert_eq!(
        check_receiver_set(&set(vec![]), &v1::Hardware::default()),
        Ok(())
    );
    // The common builder's sets pass too.
    let Command::ConfigureReceivers(c) = configure(&[RX, RX2]) else {
        unreachable!()
    };
    assert_eq!(check_receiver_set(&c, &hw), Ok(()));
}

#[test]
fn ids_count_and_uniqueness() {
    let hw = hardware();
    let mut s = good_set();
    s.receivers[1].receiver_id = 0;
    assert_eq!(
        check_receiver_set(&s, &hw),
        Err(ReceiverSetError::ZeroReceiverId)
    );
    let mut s = good_set();
    s.receivers[2].receiver_id = 1;
    assert_eq!(
        check_receiver_set(&s, &hw),
        Err(ReceiverSetError::DuplicateReceiverId { receiver_id: 1 })
    );
    let many = set((1..=17)
        .map(|id| rx(id, "x", M::RtlsdrOther, P::Rtlsdr, 100_000_000))
        .collect());
    assert_eq!(
        check_receiver_set(&many, &hw),
        Err(ReceiverSetError::TooManyReceivers { count: 17 })
    );
}

#[test]
fn serials() {
    let hw = hardware();
    // Two receivers on one SDR.
    let s = set(vec![
        rx(
            1,
            "00000001",
            M::RtlsdrDirectSampling,
            P::RtlsdrHf,
            7_100_000,
        ),
        rx(2, "00000001", M::RtlsdrOther, P::Rtlsdr, 145_000_000),
    ]);
    assert_eq!(
        check_receiver_set(&s, &hw),
        Err(ReceiverSetError::SdrInUse {
            receiver_id: 2,
            other: 1
        })
    );
    // Empty serial with three RTL-SDRs.
    let s = set(vec![rx(1, "", M::RtlsdrOther, P::Rtlsdr, 100_000_000)]);
    assert_eq!(
        check_receiver_set(&s, &hw),
        Err(ReceiverSetError::EmptySerial {
            receiver_id: 1,
            rtlsdrs: 3
        })
    );
}

#[test]
fn a_receiver_whose_sdr_is_not_plugged_in() {
    let hw = hardware();
    // A serial the station doesn't report: accepted, NO_DEVICE until it is
    // plugged in, beside receivers on SDRs that are.
    let absent = rx(4, "12345678", M::RtlsdrBlogV4, P::RtlsdrV4, 435_000_000);
    let mut s = good_set();
    s.receivers.push(absent.clone());
    assert_eq!(check_receiver_set(&s, &hw), Ok(()));
    // Any profile the package has, whatever the SDRs plugged in offer.
    let only_v3 = v1::Hardware {
        sdrs: vec![sdr(
            "00000001",
            "RTL2838UHIDIR",
            &[(M::RtlsdrOther, &[P::Rtlsdr])],
        )],
        ..hardware()
    };
    assert_eq!(
        check_receiver_set(&set(vec![absent.clone()]), &only_v3),
        Ok(())
    );
    // A model specified...
    for model in [0, 9] {
        let mut r = absent.clone();
        r.model = model;
        let e = check_receiver_set(&set(vec![r]), &hw).expect_err("model");
        assert_eq!(e, ReceiverSetError::ModelNotOffered { receiver_id: 4 });
        assert_eq!(e.result_code(), v1::ResultCode::PermissionDenied);
    }
    // ...a profile `profiles` lists...
    for profile in [0, 9] {
        let mut r = absent.clone();
        r.profile = profile;
        assert_eq!(
            check_receiver_set(&set(vec![r]), &hw),
            Err(ReceiverSetError::ProfileNotOffered { receiver_id: 4 })
        );
    }
    let mut no_v4 = hw.clone();
    no_v4.profiles.retain(|p| p.profile != P::RtlsdrV4 as i32);
    assert_eq!(
        check_receiver_set(&set(vec![absent.clone()]), &no_v4),
        Err(ReceiverSetError::ProfileNotOffered { receiver_id: 4 })
    );
    // ...and a centre and settings that fit it.
    let mut r = absent.clone();
    r.center_frequency_hz = 100_000;
    assert_eq!(
        check_receiver_set(&set(vec![r]), &hw),
        Err(ReceiverSetError::CenterOutOfRange {
            receiver_id: 4,
            center_hz: 100_000
        })
    );
    let mut r = absent.clone();
    r.gain_db = Some(60.0);
    assert_eq!(
        check_receiver_set(&set(vec![r]), &hw),
        Err(ReceiverSetError::SettingOutOfRange {
            receiver_id: 4,
            setting: "gain_db"
        })
    );
    // The model isn't checked against an SDR yet: any specified one with
    // any listed profile (the station checks it when the SDR appears).
    let mut r = absent.clone();
    r.model = M::RtlsdrOther as i32;
    assert_eq!(check_receiver_set(&set(vec![r]), &hw), Ok(()));
    // The other rules still hold: one receiver per serial, no overlap.
    let twice = set(vec![absent.clone(), {
        let mut r = absent.clone();
        r.receiver_id = 5;
        r.center_frequency_hz = 145_000_000;
        r
    }]);
    assert_eq!(
        check_receiver_set(&twice, &hw),
        Err(ReceiverSetError::SdrInUse {
            receiver_id: 5,
            other: 4
        })
    );
    let overlapping = set(vec![
        rx(3, "00000003", M::RtlsdrBlogV4, P::RtlsdrV4, 435_500_000),
        absent,
    ]);
    assert_eq!(
        check_receiver_set(&overlapping, &hw),
        Err(ReceiverSetError::Overlap {
            receiver_id: 4,
            other: 3
        })
    );
}

#[test]
fn the_empty_serial_with_no_rtl_sdr() {
    // No RTL-SDR (an unsupported SDR only, or nothing): NO_DEVICE until one
    // is plugged in.
    let none = v1::Hardware {
        sdrs: vec![sdr("A", "Some dongle", &[])],
        ..hardware()
    };
    let one = set(vec![rx(1, "", M::RtlsdrOther, P::Rtlsdr, 100_000_000)]);
    assert_eq!(check_receiver_set(&one, &none), Ok(()));
    let nothing = v1::Hardware {
        sdrs: vec![],
        ..hardware()
    };
    assert_eq!(check_receiver_set(&one, &nothing), Ok(()));
    // Still one receiver on it.
    let two = set(vec![
        rx(1, "", M::RtlsdrOther, P::Rtlsdr, 100_000_000),
        rx(2, "", M::RtlsdrOther, P::Rtlsdr, 400_000_000),
    ]);
    assert_eq!(
        check_receiver_set(&two, &nothing),
        Err(ReceiverSetError::SdrInUse {
            receiver_id: 2,
            other: 1
        })
    );
    // A profile the package doesn't list.
    let no_profiles = v1::Hardware::default();
    assert_eq!(
        check_receiver_set(&one, &no_profiles),
        Err(ReceiverSetError::ProfileNotOffered { receiver_id: 1 })
    );
}

#[test]
fn the_empty_serial_means_the_only_rtl_sdr() {
    // One RTL-SDR (with an empty serial of its own) and an unsupported SDR.
    let hw = v1::Hardware {
        sdrs: vec![
            sdr("", "RTL2838UHIDIR", &[(M::RtlsdrOther, &[P::Rtlsdr])]),
            sdr("ABC", "Some other SDR", &[]),
        ],
        profiles: vec![profile_info(P::Rtlsdr)],
    };
    let one = set(vec![rx(1, "", M::RtlsdrOther, P::Rtlsdr, 100_000_000)]);
    assert_eq!(check_receiver_set(&one, &hw), Ok(()));
    // The model must still be one offered for it.
    let wrong = set(vec![rx(
        1,
        "",
        M::RtlsdrDirectSampling,
        P::Rtlsdr,
        100_000_000,
    )]);
    assert_eq!(
        check_receiver_set(&wrong, &hw),
        Err(ReceiverSetError::ModelNotOffered { receiver_id: 1 })
    );
    // Two receivers both on the only one.
    let two = set(vec![
        rx(1, "", M::RtlsdrOther, P::Rtlsdr, 100_000_000),
        rx(2, "", M::RtlsdrOther, P::Rtlsdr, 400_000_000),
    ]);
    assert_eq!(
        check_receiver_set(&two, &hw),
        Err(ReceiverSetError::SdrInUse {
            receiver_id: 2,
            other: 1
        })
    );
    // The empty serial and the same SDR by its serial.
    let hw = v1::Hardware {
        sdrs: vec![sdr(
            "00000009",
            "RTL2838UHIDIR",
            &[(M::RtlsdrOther, &[P::Rtlsdr])],
        )],
        profiles: vec![profile_info(P::Rtlsdr)],
    };
    let both = set(vec![
        rx(1, "", M::RtlsdrOther, P::Rtlsdr, 100_000_000),
        rx(2, "00000009", M::RtlsdrOther, P::Rtlsdr, 400_000_000),
    ]);
    assert_eq!(
        check_receiver_set(&both, &hw),
        Err(ReceiverSetError::SdrInUse {
            receiver_id: 2,
            other: 1
        })
    );
}

#[test]
fn a_shared_serial_serves_one_receiver() {
    // Two dongles that both report the factory serial 00000001.
    let v3: &[(M, &[P])] = &[(M::RtlsdrDirectSampling, &[P::Rtlsdr, P::RtlsdrHf])];
    let hw = v1::Hardware {
        sdrs: vec![
            sdr("00000001", "RTL2838UHIDIR", v3),
            sdr("00000001", "RTL2838UHIDIR", v3),
        ],
        profiles: vec![profile_info(P::Rtlsdr), profile_info(P::RtlsdrHf)],
    };
    let one = set(vec![rx(
        1,
        "00000001",
        M::RtlsdrDirectSampling,
        P::RtlsdrHf,
        7_100_000,
    )]);
    assert_eq!(check_receiver_set(&one, &hw), Ok(()));
    let two = set(vec![
        rx(
            1,
            "00000001",
            M::RtlsdrDirectSampling,
            P::RtlsdrHf,
            7_100_000,
        ),
        rx(
            2,
            "00000001",
            M::RtlsdrDirectSampling,
            P::Rtlsdr,
            145_000_000,
        ),
    ]);
    assert_eq!(
        check_receiver_set(&two, &hw),
        Err(ReceiverSetError::SdrInUse {
            receiver_id: 2,
            other: 1
        })
    );
    // A model only one of them offers is not usable through the serial.
    let mut hw = hw;
    hw.sdrs[1].models = vec![v1::ModelOption {
        model: M::RtlsdrOther as i32,
        profiles: vec![P::Rtlsdr as i32],
    }];
    assert_eq!(
        check_receiver_set(&one, &hw),
        Err(ReceiverSetError::ModelNotOffered { receiver_id: 1 })
    );
}

#[test]
fn models_and_profiles_from_the_table() {
    let hw = hardware();
    // The V4 is offered only its own model...
    let s = set(vec![rx(
        3,
        "00000003",
        M::RtlsdrDirectSampling,
        P::RtlsdrHf,
        7_100_000,
    )]);
    let e = check_receiver_set(&s, &hw).expect_err("model");
    assert_eq!(e, ReceiverSetError::ModelNotOffered { receiver_id: 3 });
    assert_eq!(e.result_code(), v1::ResultCode::PermissionDenied);
    // ...and its own profile.
    let s = set(vec![rx(
        3,
        "00000003",
        M::RtlsdrBlogV4,
        P::Rtlsdr,
        145_000_000,
    )]);
    let e = check_receiver_set(&s, &hw).expect_err("profile");
    assert_eq!(e, ReceiverSetError::ProfileNotOffered { receiver_id: 3 });
    assert_eq!(e.result_code(), v1::ResultCode::PermissionDenied);
    // RTLSDR_OTHER has no HF profile.
    let s = set(vec![rx(
        1,
        "00000001",
        M::RtlsdrOther,
        P::RtlsdrHf,
        7_100_000,
    )]);
    assert_eq!(
        check_receiver_set(&s, &hw),
        Err(ReceiverSetError::ProfileNotOffered { receiver_id: 1 })
    );
    // UNSPECIFIED and unknown values are never offered.
    for (model, profile) in [(0, 2), (9, 2), (2, 0), (2, 9)] {
        let mut s = set(vec![rx(
            1,
            "00000001",
            M::RtlsdrDirectSampling,
            P::RtlsdrHf,
            7_100_000,
        )]);
        s.receivers[0].model = model;
        s.receivers[0].profile = profile;
        let e = check_receiver_set(&s, &hw).expect_err("not offered");
        assert_eq!(e.result_code(), v1::ResultCode::PermissionDenied, "{e}");
    }
    // A profile offered but missing from `profiles` (a report the hub would
    // refuse) is not usable.
    let mut broken = hw.clone();
    broken.profiles.retain(|p| p.profile != P::RtlsdrHf as i32);
    let s = set(vec![rx(
        1,
        "00000001",
        M::RtlsdrDirectSampling,
        P::RtlsdrHf,
        7_100_000,
    )]);
    assert_eq!(
        check_receiver_set(&s, &broken),
        Err(ReceiverSetError::ProfileNotOffered { receiver_id: 1 })
    );
}

#[test]
fn centre_inside_the_profile() {
    let hw = hardware();
    for (center, ok) in [
        (1_024_000, true),
        (28_800_000, true),
        (1_023_999, false),
        (28_800_001, false),
        (145_000_000, false),
    ] {
        let s = set(vec![rx(
            1,
            "00000001",
            M::RtlsdrDirectSampling,
            P::RtlsdrHf,
            center,
        )]);
        let got = check_receiver_set(&s, &hw);
        if ok {
            assert_eq!(got, Ok(()), "{center}");
        } else {
            assert_eq!(
                got,
                Err(ReceiverSetError::CenterOutOfRange {
                    receiver_id: 1,
                    center_hz: center
                }),
            );
        }
    }
}

#[test]
fn settings_inside_the_profile() {
    let hw = hardware();
    let tuner = |f: &dyn Fn(&mut v1::ReceiverConfig)| {
        let mut r = rx(3, "00000003", M::RtlsdrBlogV4, P::RtlsdrV4, 145_000_000);
        f(&mut r);
        check_receiver_set(&set(vec![r]), &hw)
    };
    assert_eq!(tuner(&|r| r.gain_db = Some(0.0)), Ok(()));
    assert_eq!(tuner(&|r| r.gain_db = Some(49.6)), Ok(()));
    assert_eq!(tuner(&|r| r.hardware_agc = Some(true)), Ok(()));
    assert_eq!(tuner(&|r| r.hardware_agc = Some(false)), Ok(()));
    let out = |setting| {
        Err(ReceiverSetError::SettingOutOfRange {
            receiver_id: 3,
            setting,
        })
    };
    assert_eq!(tuner(&|r| r.gain_db = Some(50.0)), out("gain_db"));
    assert_eq!(tuner(&|r| r.gain_db = Some(-0.1)), out("gain_db"));
    assert_eq!(tuner(&|r| r.gain_db = Some(f32::NAN)), out("gain_db"));
    // The V4 profile has no attenuator.
    assert_eq!(
        tuner(&|r| r.attenuation_db = Some(0.0)),
        out("attenuation_db")
    );
    // Direct sampling: no gain, no AGC.
    let hf = |f: &dyn Fn(&mut v1::ReceiverConfig)| {
        let mut r = rx(
            1,
            "00000001",
            M::RtlsdrDirectSampling,
            P::RtlsdrHf,
            7_100_000,
        );
        f(&mut r);
        check_receiver_set(&set(vec![r]), &hw)
    };
    let out = |setting| {
        Err(ReceiverSetError::SettingOutOfRange {
            receiver_id: 1,
            setting,
        })
    };
    assert_eq!(hf(&|r| r.gain_db = Some(10.0)), out("gain_db"));
    assert_eq!(hf(&|r| r.hardware_agc = Some(true)), out("hardware_agc"));
    assert_eq!(hf(&|r| r.hardware_agc = Some(false)), Ok(()));
    // An attenuator, when the profile has one.
    let mut hw = hardware();
    if let Some(f) = hw.profiles[2].frontend.as_mut() {
        f.has_attenuator = true;
        f.max_attenuation_db = 20.0;
    }
    let att = |a: f32| {
        let mut r = rx(3, "00000003", M::RtlsdrBlogV4, P::RtlsdrV4, 145_000_000);
        r.attenuation_db = Some(a);
        check_receiver_set(&set(vec![r]), &hw)
    };
    assert_eq!(att(0.0), Ok(()));
    assert_eq!(att(20.0), Ok(()));
    for bad in [-1.0, 20.5, f32::INFINITY] {
        assert_eq!(
            att(bad),
            Err(ReceiverSetError::SettingOutOfRange {
                receiver_id: 3,
                setting: "attenuation_db"
            })
        );
    }
}

#[test]
fn predicted_ranges_never_overlap() {
    let hw = hardware();
    // 2.048 MHz spans: 7.1 MHz covers 6.076–8.124 MHz.
    assert_eq!(
        predicted_tunable_range(7_100_000, 2_048_000),
        range(6_076_000, 8_124_000)
    );
    let pair = |second: u64| {
        set(vec![
            rx(
                1,
                "00000001",
                M::RtlsdrDirectSampling,
                P::RtlsdrHf,
                7_100_000,
            ),
            rx(2, "00000002", M::RtlsdrDirectSampling, P::RtlsdrHf, second),
        ])
    };
    // Touching: 8.124 MHz is where both meet.
    assert_eq!(check_receiver_set(&pair(9_148_000), &hw), Ok(()));
    let e = check_receiver_set(&pair(9_147_999), &hw).expect_err("overlap");
    assert_eq!(
        e,
        ReceiverSetError::Overlap {
            receiver_id: 2,
            other: 1
        }
    );
    assert_eq!(e.code(), "receivers_overlap");
    assert_eq!(e.receiver_id(), Some(2));
    assert_eq!(e.other_receiver_id(), Some(1));
    assert_eq!(e.result_code(), v1::ResultCode::InvalidArgument);
    // Same centre.
    assert!(check_receiver_set(&pair(7_100_000), &hw).is_err());
    // Below.
    assert!(check_receiver_set(&pair(5_052_001), &hw).is_err());
    assert_eq!(check_receiver_set(&pair(5_052_000), &hw), Ok(()));
}

#[test]
fn the_first_broken_rule_in_set_order() {
    let hw = hardware();
    let mut s = good_set();
    s.receivers[1].center_frequency_hz = 99_000_000; // out of the HF profile
    s.receivers[2].model = M::RtlsdrOther as i32; // not offered for the V4
    assert_eq!(
        check_receiver_set(&s, &hw),
        Err(ReceiverSetError::CenterOutOfRange {
            receiver_id: 2,
            center_hz: 99_000_000
        })
    );
    // Every message fits Result.message.
    let e = check_receiver_set(&s, &hw).expect_err("refused");
    let r = e.to_result();
    assert_eq!(r.code, v1::ResultCode::InvalidArgument as i32);
    assert!(r.message.len() <= limits::MAX_DETAIL_BYTES);
    assert!(r.message.contains("receiver 2"), "{}", r.message);
}

// ------------------------------------------------------------------ Hardware (§5)

fn hardware_error(h: v1::Hardware) -> Option<ViolationKind> {
    check_message(&response(1, OK, Some(Payload::Hardware(h))), 0).err()
}

#[test]
fn hardware_report_rules() {
    assert_eq!(hardware_error(hardware()), None);
    // At most 16 SDRs.
    let mut h = hardware();
    h.sdrs = (0..17)
        .map(|i| sdr(&format!("{i}"), "RTL2838UHIDIR", &[]))
        .collect();
    assert_eq!(
        hardware_error(h),
        Some(ViolationKind::TooManyEntries {
            field: "hardware.sdrs",
            len: 17,
            max: 16
        })
    );
    // An SDR the station doesn't support: no models, fine.
    let mut h = hardware();
    h.sdrs.push(sdr("X", "Something else", &[]));
    assert_eq!(hardware_error(h), None);
    // A profile offered must be listed...
    let mut h = hardware();
    h.profiles.retain(|p| p.profile != P::RtlsdrV4 as i32);
    assert_eq!(
        hardware_error(h),
        Some(ViolationKind::MissingField {
            field: "hardware.profiles"
        })
    );
    // ...once.
    let mut h = hardware();
    h.profiles.push(profile_info(P::Rtlsdr));
    assert_eq!(
        hardware_error(h),
        Some(ViolationKind::OutOfBounds {
            field: "hardware.profiles.profile"
        })
    );
    // A centre range present, ordered, inside 6 GHz, with its default.
    let with_profile = |f: &dyn Fn(&mut v1::ProfileInfo)| {
        let mut h = hardware();
        f(&mut h.profiles[0]);
        hardware_error(h)
    };
    assert_eq!(
        with_profile(&|p| p.center_range = None),
        Some(ViolationKind::MissingField {
            field: "hardware.profiles.center_range"
        })
    );
    for r in [range(10, 5), range(0, 6_000_000_001)] {
        assert_eq!(
            with_profile(&|p| p.center_range = Some(r)),
            Some(ViolationKind::InvalidRange {
                field: "hardware.profiles.center_range"
            })
        );
    }
    assert_eq!(
        with_profile(&|p| p.default_center_hz = 1_000),
        Some(ViolationKind::OutOfBounds {
            field: "hardware.profiles.default_center_hz"
        })
    );
    assert!(matches!(
        with_profile(&|p| {
            if let Some(f) = p.frontend.as_mut() {
                f.max_gain_db = f32::NAN;
            }
        }),
        Some(ViolationKind::NonFiniteFloat { .. })
    ));
    assert_eq!(with_profile(&|p| p.frontend = None), None);
    // A receiver_id of 0 never names a receiver.
    let mut h = hardware();
    h.sdrs[0].receiver_id = Some(0);
    assert!(matches!(
        hardware_error(h),
        Some(ViolationKind::UnknownReceiver { receiver_id: 0, .. })
    ));
}

// ------------------------------------------------------------------ select_receivers (§5.1)

#[test]
fn the_hub_keeps_the_lower_id_and_its_limit() {
    let mut caps = capabilities();
    caps.receivers = vec![
        receiver_capabilities(4, 50_000_000),
        receiver_capabilities(2, 7_000_000), // 7.0–9.4 MHz
        receiver_capabilities(1, 5_900_000), // 5.9–8.3 MHz
        receiver_capabilities(9, 100_000_000),
        receiver_capabilities(3, 14_000_000),
        receiver_capabilities(7, 200_000_000),
        receiver_capabilities(8, 300_000_000),
        receiver_capabilities(5, 400_000_000),
    ];
    let s = select_receivers(&caps.receivers, limits::DEFAULT_HUB_MAX_RECEIVERS);
    // The lowest six IDs: 1, 2, 3, 4, 5, 7; 2 overlaps 1.
    assert_eq!(s.over_limit, [8, 9]);
    assert_eq!(
        s.overlapping,
        [OverlappingReceiver {
            receiver_id: 2,
            overlaps: 1
        }]
    );
    assert_eq!(s.serve, [4, 1, 3, 7, 5]);
}

// ------------------------------------------------------------------ the session (§5.1)

fn get_capabilities() -> Command {
    Command::GetCapabilities(v1::GetCapabilities {})
}

fn get_hardware() -> Command {
    Command::GetHardware(v1::GetHardware {})
}

fn caps_of(ids: &[u32]) -> Payload {
    let mut caps = capabilities();
    caps.receivers = ids
        .iter()
        .map(|&id| receiver_capabilities(id, u64::from(id) * 10_000_000))
        .collect();
    Payload::Capabilities(caps)
}

#[test]
fn a_receiver_is_configured_from_the_set_that_names_it() {
    let t0 = Instant::now();
    let mut s = established(t0);
    assert!(!s.is_receiver_configured(RX, t0));
    s.on_request_sent(&request(1, configure(&[RX])), t0)
        .expect("request");
    // Configured on send: a status before the answer is fine.
    assert!(s.is_receiver_configured(RX, t0));
    assert_eq!(s.check(&status(), t0), Verdict::Accept);
    assert_eq!(
        s.check(&response(1, OK, Some(receivers_configured(&[RX]))), t0),
        Verdict::Accept
    );
    // Capabilities with it, and no other.
    s.on_request_sent(&request(2, get_capabilities()), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(2, OK, Some(caps_of(&[RX]))), t0),
        Verdict::Accept
    );
    assert_eq!(s.running_receivers(), [RX]);
    s.on_request_sent(&request(3, get_capabilities()), t0)
        .expect("request");
    assert_eq!(
        violation(s.check(&response(3, OK, Some(caps_of(&[RX, 5]))), t0)),
        ViolationKind::UnknownReceiver {
            kind: MessageKind::Response,
            receiver_id: 5
        }
    );
}

#[test]
fn a_later_set_drops_receivers_with_a_grace() {
    let t0 = Instant::now();
    let mut s = configured(t0, &[RX, RX2]);
    s.on_request_sent(&request(2, open_channel_on(RX2, 7, 20)), t0)
        .expect("request");
    s.on_request_sent(&request(3, subscribe_on(RX2, 8, 256, 100)), t0)
        .expect("request");
    s.on_request_sent(&request(4, open_channel(9, 20)), t0)
        .expect("request");
    // A set without RX2: until the station accepts it, RX2 stays.
    let t1 = t0 + secs(10);
    s.on_request_sent(&request(5, configure(&[RX])), t1)
        .expect("request");
    assert!(s.is_receiver_configured(RX2, t1));
    assert_eq!(s.check(&status_of(&[RX, RX2]), t1), Verdict::Accept);
    let t2 = t1 + secs(1);
    assert_eq!(
        s.check(&response(5, OK, Some(receivers_configured(&[RX]))), t2),
        Verdict::Accept
    );
    assert!(!s.is_receiver_configured(RX2, t2));
    assert!(s.is_receiver_configured(RX, t2));
    // Its channels and subscriptions closed with it; RX's channel stays.
    assert!(!s.is_channel_open(7, t2));
    assert!(!s.is_subscription_open(8, t2));
    assert!(s.is_channel_open(9, t2));
    // Within 5 s: status naming it and media on it are dropped, events accepted.
    assert_eq!(
        s.check(&status_of(&[RX, RX2]), t2 + secs(5)),
        Verdict::Drop(DropReason::ReceiverRemoved {
            kind: MessageKind::Status,
            receiver_id: RX2
        })
    );
    assert_eq!(
        s.check(&audio(7, 80), t2 + secs(5)),
        Verdict::Drop(DropReason::RecentlyClosed {
            kind: MessageKind::Audio,
            id: 7
        })
    );
    assert_eq!(
        s.check(
            &receiver_event(v1::event::Kind::BackendRestarted, RX2),
            t2 + secs(5)
        ),
        Verdict::Accept
    );
    // After it: violations, and events are dropped.
    let t3 = t2 + secs(6);
    assert_eq!(s.check(&status_of(&[RX]), t3), Verdict::Accept);
    assert_eq!(
        violation(s.check(&status_of(&[RX, RX2]), t3)),
        ViolationKind::UnknownReceiver {
            kind: MessageKind::Status,
            receiver_id: RX2
        }
    );
    assert_eq!(
        s.check(&receiver_event(v1::event::Kind::DeviceLost, RX2), t3),
        Verdict::Drop(DropReason::UnknownEventTarget)
    );
    assert!(matches!(
        violation(s.check(&audio(7, 80), t3)),
        ViolationKind::UnknownChannel { channel_id: 7, .. }
    ));
}

#[test]
fn a_refused_set_changes_nothing() {
    let t0 = Instant::now();
    let mut s = configured(t0, &[RX, RX2]);
    s.on_request_sent(&request(2, open_channel_on(RX2, 7, 20)), t0)
        .expect("request");
    // Swap RX2 for 3; the station refuses (say, 3's SDR was just unplugged).
    s.on_request_sent(&request(3, configure(&[RX, 3])), t0)
        .expect("request");
    assert!(s.is_receiver_configured(3, t0));
    let refused = v1::ResultCode::InvalidArgument as i32;
    assert_eq!(s.check(&response(3, refused, None), t0), Verdict::Accept);
    assert!(!s.is_receiver_configured(3, t0));
    assert!(s.is_receiver_configured(RX2, t0));
    assert!(s.is_channel_open(7, t0));
    // RX2 runs on.
    let later = t0 + secs(60);
    assert_eq!(s.check(&status_of(&[RX, RX2]), later), Verdict::Accept);
    assert!(is_violation(
        &s.check(&status_of(&[RX, 3]), later + secs(10))
    ));
}

#[test]
fn before_the_first_set_the_station_runs_its_own() {
    let t0 = Instant::now();
    let mut s = established(t0);
    // The station runs receivers 5 and 6 from its kept set: the hardware
    // names them, and the status and events are dropped.
    let mut hw = hardware();
    hw.sdrs[0].receiver_id = Some(5);
    hw.sdrs[1].receiver_id = Some(6);
    s.on_request_sent(&request(1, get_hardware()), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(1, OK, Some(Payload::Hardware(hw.clone()))), t0),
        Verdict::Accept
    );
    assert_eq!(
        s.check(&status_of(&[5, 6]), t0),
        Verdict::Drop(DropReason::AwaitingReceivers {
            kind: MessageKind::Status
        })
    );
    // The hub's set: 5 stays, 6 goes, 7 is new. Before the answer the
    // station still runs 6.
    s.on_request_sent(&request(2, configure(&[5, 7])), t0)
        .expect("request");
    assert_eq!(
        s.check(&status_of(&[5, 6]), t0 + secs(5)),
        Verdict::Drop(DropReason::AwaitingReceivers {
            kind: MessageKind::Status
        })
    );
    assert_eq!(
        s.check(
            &response(2, OK, Some(receivers_configured(&[5, 7]))),
            t0 + secs(6)
        ),
        Verdict::Accept
    );
    // Now 6 is unknown.
    let t1 = t0 + secs(20);
    assert_eq!(s.check(&status_of(&[5, 7]), t1), Verdict::Accept);
    assert!(is_violation(&s.check(&status_of(&[6]), t1 + secs(10))));
    s.on_request_sent(&request(3, get_hardware()), t1)
        .expect("request");
    assert_eq!(
        violation(s.check(&response(3, OK, Some(Payload::Hardware(hw))), t1)),
        ViolationKind::UnknownReceiver {
            kind: MessageKind::Response,
            receiver_id: 6
        }
    );
    let mut hw = hardware();
    hw.sdrs[0].receiver_id = Some(5);
    hw.sdrs[2].receiver_id = Some(7);
    s.on_request_sent(&request(4, get_hardware()), t1)
        .expect("request");
    assert_eq!(
        s.check(&response(4, OK, Some(Payload::Hardware(hw))), t1),
        Verdict::Accept
    );
}

#[test]
fn an_unchanged_set_keeps_everything() {
    let t0 = Instant::now();
    let mut s = configured(t0, &[RX]);
    s.on_request_sent(&request(2, open_channel(1, 20)), t0)
        .expect("request");
    s.on_request_sent(&request(3, configure(&[RX])), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(3, OK, Some(receivers_configured(&[RX]))), t0),
        Verdict::Accept
    );
    assert!(s.is_channel_open(1, t0));
    assert!(s.is_receiver_configured(RX, t0));
    // An empty set drops all.
    s.on_request_sent(&request(4, configure(&[])), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(4, OK, Some(receivers_configured(&[]))), t0),
        Verdict::Accept
    );
    assert!(!s.is_receiver_configured(RX, t0));
    assert!(!s.is_channel_open(1, t0));
}

#[test]
fn ingress_follows_the_running_receivers() {
    use station_proto::validate::{IngressQuota, QuotaDecision};
    let t0 = Instant::now();
    let mut s = configured(t0, &[1, 2, 3]);
    s.on_request_sent(&request(2, get_capabilities()), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(2, OK, Some(caps_of(&[1, 2, 3]))), t0),
        Verdict::Accept
    );
    let mut q = IngressQuota::new(0, t0);
    q.set_running_receivers(s.running_receivers().len(), t0);
    // 4 Mbit/s: 500 kB/s for a minute stays within the quota.
    for i in 0..600u64 {
        let now = t0 + ms(100 * i);
        assert_eq!(q.on_bytes(50_000, now), QuotaDecision::Allow);
    }
}
