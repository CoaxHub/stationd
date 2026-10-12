// SPDX-License-Identifier: GPL-3.0-only
//! The receivers on fake units, fake backends and simulated SDRs.

use std::time::Duration;

use station_proto::validate::sanitize_capabilities;
use station_proto::{limits, v1};

use super::testing::{STANDARD_SDRS, Station, drain, receiver, restarted, standard_set, states};
use super::units::UnitCall;
use super::*;
use crate::measure::StopReason;
use crate::profile::ProfileId;

use v1::ReceiverState as S;

/// Waits until `pred` holds for the events seen so far (at most 10 s).
async fn until(
    rx: &mut broadcast::Receiver<RxEvent>,
    seen: &mut Vec<RxEvent>,
    pred: impl Fn(&[RxEvent]) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !pred(seen) {
            match rx.recv().await {
                Ok(e) => seen.push(e),
                Err(e) => panic!("events: {e}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; seen {:?}", states(seen)));
}

#[tokio::test]
async fn a_set_is_checked_stored_started_and_answered() {
    let st = Station::new(&STANDARD_SDRS).await;
    let mut events = st.receivers.subscribe();
    let hw = st.receivers.hardware();
    assert_eq!(hw.sdrs.len(), 2);
    assert!(hw.sdrs.iter().all(|s| s.receiver_id.is_none()));
    let answer = st.receivers.configure(&standard_set()).await.expect("set");
    let answered: Vec<(u32, i32)> = answer
        .receivers
        .iter()
        .map(|r| (r.receiver_id, r.state))
        .collect();
    assert_eq!(answered, [(1, S::Starting as i32), (2, S::Starting as i32)]);
    let mut seen = drain(&mut events);
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(1, S::Running)) && states(e).contains(&(2, S::Running))
    })
    .await;
    // Entering RUNNING stops a plan; STARTING doesn't.
    for e in &seen {
        let running = e.event.receiver_state == Some(S::Running as i32);
        assert_eq!(e.stop.is_some(), running, "{e:?}");
    }
    assert_eq!(st.units.calls(), [UnitCall::Start(1), UnitCall::Start(2)]);
    // The store, with slots.
    let stored = StoredSet::load(&st.dir.path().join("receivers/receivers.json")).expect("store");
    let slots: Vec<(u32, u8)> = stored
        .receivers
        .iter()
        .map(|r| (r.receiver_id, r.slot))
        .collect();
    assert_eq!(slots, [(1, 0), (2, 1)]);
    // The hardware names the receivers on their SDRs.
    let serving: Vec<Option<u32>> = st
        .receivers
        .hardware()
        .sdrs
        .iter()
        .map(|s| s.receiver_id)
        .collect();
    assert_eq!(serving, [Some(1), Some(2)]);
    // Capabilities: both, in frequency order, within the claims.
    let caps = st.receivers.capabilities().await;
    let ids: Vec<u32> = caps.receivers.iter().map(|r| r.receiver_id).collect();
    assert_eq!(ids, [1, 2]);
    let hf = &caps.receivers[0];
    assert_eq!(hf.max_channels, limits::CHANNELS_PER_RECEIVER);
    assert_eq!(hf.sdr_model, "RTL-SDR HF direct sampling");
    assert_eq!(
        hf.tunable_range,
        Some(v1::FrequencyRange {
            start_hz: 6_076_000,
            end_hz: 8_124_000
        })
    );
    assert_eq!(
        hf.hardware_range,
        Some(ProfileId::RtlsdrHf.profile().hardware_range())
    );
    let controls = hf.frontend.clone().expect("controls");
    assert!(controls.can_retune && !controls.has_gain);
    assert_eq!(caps.receivers[1].sdr_model, "RTL-SDR Blog V4");
    assert_eq!(
        caps.waterfall.map(|w| w.max_subscriptions),
        Some(limits::waterfall_subscriptions_claim(2))
    );
    assert_eq!(caps.modes.len(), 6);
    assert!(caps.measurements.is_some());
    assert!(
        sanitize_capabilities(&caps, 0)
            .expect("valid")
            .clamped
            .is_empty()
    );
    assert_eq!(st.receivers.running().len(), 2);
    assert!(st.receivers.running_backend(9).is_none());
    assert_eq!(st.receivers.config(2).map(|c| c.slot), Some(1));
    let summaries = st.receivers.summaries();
    assert_eq!(summaries[1].state, "running");
    assert_eq!(summaries[1].profile, "rtlsdr-v4");
}

#[tokio::test]
async fn a_refused_set_changes_nothing() {
    let st = Station::standard().await;
    let mut events = st.receivers.subscribe();
    let before = std::fs::read(st.dir.path().join("receivers/receivers.json")).expect("store");
    let mut v4_on_hf = standard_set();
    v4_on_hf.receivers[1].profile = v1::SdrProfile::RtlsdrHf as i32;
    let refused = st
        .receivers
        .configure(&v4_on_hf)
        .await
        .expect_err("refused");
    assert_eq!(refused.code, v1::ResultCode::PermissionDenied as i32);
    assert_eq!(
        refused.message,
        "receiver 2: profile not offered for its model"
    );
    let mut vhf_on_hf = standard_set();
    vhf_on_hf.receivers[0].center_frequency_hz = 99_000_000;
    let refused = st
        .receivers
        .configure(&vhf_on_hf)
        .await
        .expect_err("refused");
    assert_eq!(refused.code, v1::ResultCode::InvalidArgument as i32);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(drain(&mut events).is_empty());
    let after = std::fs::read(st.dir.path().join("receivers/receivers.json")).expect("store");
    assert_eq!(before, after);
    assert_eq!(st.receivers.running().len(), 2);
}

#[tokio::test]
async fn the_difference_is_applied() {
    let st = Station::standard().await;
    let calls = st.units.calls().len();
    let mut events = st.receivers.subscribe();
    // The same set: nothing restarts.
    let same = st.receivers.configure(&standard_set()).await.expect("same");
    assert!(same.receivers.iter().all(|r| r.state == S::Running as i32));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(drain(&mut events).is_empty());
    assert_eq!(st.units.calls().len(), calls);
    // Receiver 1 moves, receiver 2 goes, receiver 3 (the V4 again) comes.
    let next = v1::ConfigureReceivers {
        receivers: vec![
            receiver(1, "CH000001", ProfileId::RtlsdrHf, 7_300_000),
            receiver(3, "CH000002", ProfileId::RtlsdrV4, 145_000_000),
        ],
    };
    let answer = st.receivers.configure(&next).await.expect("next");
    let ids: Vec<u32> = answer.receivers.iter().map(|r| r.receiver_id).collect();
    assert_eq!(ids, [1, 3]);
    let mut seen = drain(&mut events);
    assert_eq!(restarted(&seen), [2, 1], "removals first, then changes");
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(1, S::Running)) && states(e).contains(&(3, S::Running))
    })
    .await;
    assert_eq!(
        st.units.calls()[calls..],
        [UnitCall::Stop(2), UnitCall::Restart(1), UnitCall::Start(3)]
    );
    // 3 takes the slot 2 freed.
    let slots: Vec<(u32, u8)> = st
        .receivers
        .stored()
        .receivers
        .iter()
        .map(|r| (r.receiver_id, r.slot))
        .collect();
    assert_eq!(slots, [(1, 0), (3, 1)]);
    let caps = st.receivers.capabilities().await;
    let tunable = caps.receivers[0].tunable_range.expect("range");
    assert!(tunable.start_hz < 7_300_000 && 7_300_000 < tunable.end_hz);
    // An empty set stops everything.
    let none = st
        .receivers
        .configure(&v1::ConfigureReceivers::default())
        .await
        .expect("empty");
    assert!(none.receivers.is_empty());
    assert!(st.receivers.running().is_empty());
    assert!(st.receivers.capabilities().await.receivers.is_empty());
}

#[tokio::test]
async fn sdrs_come_and_go() {
    let st = Station::standard().await;
    let mut events = st.receivers.subscribe();
    let mut seen = Vec::new();
    // Receiver 2's SDR goes: its channels with it, NO_DEVICE, its unit
    // stopped.
    st.unplug(STANDARD_SDRS[1].0);
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::NoDevice))
    })
    .await;
    assert_eq!(seen[0].event.kind, v1::event::Kind::HardwareChanged as i32);
    assert_eq!(restarted(&seen), [2]);
    let no_device = seen.last().expect("state");
    assert_eq!(
        no_device.event.detail,
        "no SDR with serial CH000002 is plugged in"
    );
    assert_eq!(st.units.calls().last(), Some(&UnitCall::Stop(2)));
    let health = st.receivers.health(|_| 0);
    assert_eq!(health[1].state, S::NoDevice as i32);
    assert_eq!(
        health[1].detail,
        "no SDR with serial CH000002 is plugged in"
    );
    assert_eq!(
        health[1].backend.as_ref().map(|b| b.state),
        Some(v1::backend_health::State::Down as i32)
    );
    assert!(health[1].frontend.is_none());
    assert!(health[0].frontend.is_some());
    assert_eq!(st.receivers.running().len(), 1);
    assert!(
        st.receivers
            .hardware()
            .sdrs
            .iter()
            .all(|s| s.serial != "CH000002")
    );
    // The hub can still send the set that names it (§4.2): unchanged,
    // nothing restarts, and receiver 2 is answered NO_DEVICE.
    let calls = st.units.calls().len();
    let answer = st
        .receivers
        .configure(&standard_set())
        .await
        .expect("accepted");
    let answered: Vec<(u32, i32)> = answer
        .receivers
        .iter()
        .map(|r| (r.receiver_id, r.state))
        .collect();
    assert_eq!(answered, [(1, S::Running as i32), (2, S::NoDevice as i32)]);
    assert_eq!(st.units.calls().len(), calls);
    // It returns: HARDWARE_CHANGED, then it starts by itself.
    seen.clear();
    st.plug(STANDARD_SDRS[1].0, STANDARD_SDRS[1].1, STANDARD_SDRS[1].2);
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::Running))
    })
    .await;
    assert_eq!(seen[0].event.kind, v1::event::Kind::HardwareChanged as i32);
    assert_eq!(st.units.calls().last(), Some(&UnitCall::Start(2)));
}

#[tokio::test]
async fn a_kept_receiver_without_its_sdr_waits_for_it() {
    // The set was applied with both SDRs; the station starts with one.
    let st = Station::standard().await;
    let dir = st.dir;
    drop(st.receivers);
    let again = Station::in_dir(dir);
    again.unplug(STANDARD_SDRS[1].0);
    let mut events = again.receivers.subscribe();
    again.receivers.start().await;
    let mut seen = drain(&mut events);
    assert!(states(&seen).contains(&(2, S::NoDevice)));
    assert!(!again.units.calls().contains(&UnitCall::Start(2)));
    again.plug(STANDARD_SDRS[1].0, STANDARD_SDRS[1].1, STANDARD_SDRS[1].2);
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::Running))
    })
    .await;
}

#[tokio::test]
async fn a_receiver_configured_before_its_sdr_is_plugged_in() {
    // Only the generic RTL-SDR; the set names the V4 too.
    let st = Station::new(&STANDARD_SDRS[..1]).await;
    let mut events = st.receivers.subscribe();
    let answer = st
        .receivers
        .configure(&standard_set())
        .await
        .expect("accepted");
    let answered: Vec<(u32, i32, &str)> = answer
        .receivers
        .iter()
        .map(|r| (r.receiver_id, r.state, r.detail.as_str()))
        .collect();
    assert_eq!(
        answered,
        [
            (1, S::Starting as i32, ""),
            (
                2,
                S::NoDevice as i32,
                "no SDR with serial CH000002 is plugged in"
            )
        ]
    );
    assert!(!st.units.calls().contains(&UnitCall::Start(2)));
    // It is in receivers.json, with its slot.
    assert_eq!(st.receivers.stored().get(2).map(|r| r.slot), Some(1));
    // The hardware lists the V4's profile though no V4 is plugged in.
    let profiles: Vec<i32> = st
        .receivers
        .hardware()
        .profiles
        .iter()
        .map(|p| p.profile)
        .collect();
    assert!(profiles.contains(&(v1::SdrProfile::RtlsdrV4 as i32)));
    // Plugged in: it starts by itself.
    let mut seen = drain(&mut events);
    st.plug(STANDARD_SDRS[1].0, STANDARD_SDRS[1].1, STANDARD_SDRS[1].2);
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::Running))
    })
    .await;
    assert_eq!(st.units.calls().last(), Some(&UnitCall::Start(2)));
    assert_eq!(
        st.receivers
            .hardware()
            .sdrs
            .iter()
            .map(|s| s.receiver_id)
            .collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
}

#[tokio::test]
async fn an_sdr_that_does_not_offer_the_model_fails_the_receiver() {
    let st = Station::new(&STANDARD_SDRS[..1]).await;
    let mut events = st.receivers.subscribe();
    // Configured as a V4, but the SDR with that serial is a generic one.
    st.receivers.configure(&standard_set()).await.expect("set");
    let mut seen = drain(&mut events);
    st.plug("1-1.3", "CH000002", "RTL2838UHIDIR");
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::Failed))
    })
    .await;
    let failed = seen
        .iter()
        .find(|e| e.event.receiver_state == Some(S::Failed as i32))
        .expect("failed");
    assert_eq!(
        failed.event.detail,
        "model rtlsdr_blog_v4 not offered for its SDR (0bda:2838, \"RTL2838UHIDIR\")"
    );
    assert!(!st.units.calls().contains(&UnitCall::Start(2)));
    assert!(
        st.receivers
            .hardware()
            .sdrs
            .iter()
            .all(|s| s.receiver_id != Some(2))
    );
    // The right SDR in its place: it runs.
    st.unplug("1-1.3");
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::NoDevice))
    })
    .await;
    st.plug(STANDARD_SDRS[1].0, STANDARD_SDRS[1].1, STANDARD_SDRS[1].2);
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::Running))
    })
    .await;
    // A profile the model doesn't have: the generic SDR as RTLSDR_OTHER on
    // HF (only a hub that skipped the shared rules could send it; a kept
    // set can hold it).
    let mut other_on_hf = v1::ConfigureReceivers {
        receivers: vec![receiver(3, "CH000009", ProfileId::RtlsdrHf, 7_100_000)],
    };
    other_on_hf.receivers[0].model = v1::SdrModel::RtlsdrOther as i32;
    st.receivers.configure(&other_on_hf).await.expect("set");
    seen = drain(&mut events);
    st.plug("1-1.4", "CH000009", "RTL2838UHIDIR");
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(3, S::Failed))
    })
    .await;
    let failed = seen
        .iter()
        .rev()
        .find(|e| e.event.receiver_state == Some(S::Failed as i32))
        .expect("failed");
    assert_eq!(
        failed.event.detail,
        "profile rtlsdr-hf not offered for model rtlsdr_other"
    );
}

#[tokio::test]
async fn the_empty_serial_waits_for_the_only_rtl_sdr() {
    // No SDR yet: an empty serial is accepted, NO_DEVICE.
    let st = Station::new(&[]).await;
    let mut events = st.receivers.subscribe();
    let only = v1::ConfigureReceivers {
        receivers: vec![receiver(1, "", ProfileId::Rtlsdr, 98_000_000)],
    };
    let answer = st.receivers.configure(&only).await.expect("accepted");
    assert_eq!(answer.receivers[0].state, S::NoDevice as i32);
    assert_eq!(
        answer.receivers[0].detail,
        "the station's only RTL-SDR is not plugged in"
    );
    let mut seen = drain(&mut events);
    st.plug("1-1.2", "CH000001", "RTL2838UHIDIR");
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(1, S::Running))
    })
    .await;
    // A second RTL-SDR: no longer the only one.
    st.plug("1-1.3", "CH000003", "RTL2838UHIDIR");
    until(&mut events, &mut seen, |e| {
        states(e).ends_with(&[(1, S::NoDevice)])
    })
    .await;
    assert_eq!(
        seen.last().expect("state").event.detail,
        "2 RTL-SDRs are plugged in; an empty serial needs exactly one"
    );
    // Back to one: it runs again.
    st.unplug("1-1.3");
    until(&mut events, &mut seen, |e| {
        states(e).ends_with(&[(1, S::Starting), (1, S::Running)])
    })
    .await;
}

#[tokio::test]
async fn the_empty_serial_yields_to_a_receiver_naming_the_sdr() {
    // 1 on the only RTL-SDR, 2 on CH000009; neither is plugged in.
    let st = Station::new(&[]).await;
    let mut events = st.receivers.subscribe();
    let set = v1::ConfigureReceivers {
        receivers: vec![
            receiver(1, "", ProfileId::Rtlsdr, 98_000_000),
            receiver(2, "CH000009", ProfileId::Rtlsdr, 435_000_000),
        ],
    };
    st.receivers.configure(&set).await.expect("accepted");
    let mut seen = drain(&mut events);
    // CH000009 is the only RTL-SDR, and it is receiver 2's.
    st.plug("1-1.2", "CH000009", "RTL2838UHIDIR");
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(2, S::Running)) && states(e).contains(&(1, S::Failed))
    })
    .await;
    assert_eq!(
        st.receivers.summaries()[0].detail,
        "the station's only RTL-SDR is receiver 2's"
    );
    assert!(!st.units.calls().contains(&UnitCall::Start(1)));
}

#[tokio::test]
async fn units_that_fail_and_restart() {
    let st = Station::standard().await;
    let mut events = st.receivers.subscribe();
    let mut seen = Vec::new();
    // radiod restarts by itself: the backend goes, the receiver comes back.
    st.units.crash(1);
    until(&mut events, &mut seen, |e| {
        states(e).ends_with(&[(1, S::Starting), (1, S::Running)])
    })
    .await;
    assert_eq!(restarted(&seen), [1]);
    // It fails for good.
    seen.clear();
    st.units.fail(
        1,
        Some("coaxhub-radiod@1.service failed (exit-code, exit status 1)"),
    );
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(1, S::Failed))
    })
    .await;
    let failed = seen
        .iter()
        .find(|e| e.event.receiver_state == Some(S::Failed as i32))
        .expect("failed");
    assert!(failed.event.detail.contains("exit status 1"), "{failed:?}");
    assert_eq!(failed.stop, Some(StopReason::FrontendChanged));
    let health = st.receivers.health(|_| 0);
    assert!(health[0].detail.contains("exit status 1"));
    // An unchanged set leaves it failed; a fixed unit runs it again.
    st.receivers.configure(&standard_set()).await.expect("same");
    st.units.fail(1, None);
    st.units.crash(1);
    seen.clear();
    until(&mut events, &mut seen, |e| {
        states(e).contains(&(1, S::Running))
    })
    .await;
}

#[tokio::test]
async fn backend_events_carry_their_receiver() {
    let st = Station::standard().await;
    let mut events = st.receivers.subscribe();
    let backend = st.backends.get(2).expect("receiver 2's backend");
    backend.emit_event(v1::Event {
        kind: v1::event::Kind::FrontendChanged as i32,
        ..Default::default()
    });
    backend.emit_event(v1::Event {
        kind: v1::event::Kind::ChannelClosed as i32,
        channel_id: Some(4),
        ..Default::default()
    });
    let changed = events.recv().await.expect("event");
    assert_eq!(changed.event.receiver_id, Some(2));
    assert_eq!(changed.stop, Some(StopReason::FrontendChanged));
    let closed = events.recv().await.expect("event");
    assert_eq!(closed.event.channel_id, Some(4));
    assert_eq!(closed.event.receiver_id, None);
    assert_eq!(closed.stop, None);
}

#[tokio::test]
async fn the_set_runs_again_at_start() {
    let st = Station::standard().await;
    let dir = st.dir;
    drop(st.receivers);
    // The same directory: the same SDRs and receivers.json.
    let again = Station::in_dir(dir);
    again.receivers.start().await;
    again.wait_running(&[1, 2]).await;
    assert_eq!(
        again.units.calls(),
        [UnitCall::Start(1), UnitCall::Start(2)]
    );
    // A broken store starts nothing.
    let dir = again.dir;
    drop(again.receivers);
    std::fs::write(dir.path().join("receivers/receivers.json"), "{").expect("write");
    let broken = Station::in_dir(dir);
    broken.receivers.start().await;
    assert!(broken.receivers.stored().receivers.is_empty());
}

#[tokio::test]
async fn retunes_are_checked_and_kept() {
    let st = Station::standard().await;
    assert!(st.receivers.check_retune(1, 7_200_000).is_ok());
    let e = st
        .receivers
        .check_retune(1, 30_000_000)
        .expect_err("above HF");
    assert_eq!(e.code, v1::ResultCode::OutOfRange);
    let e = st
        .receivers
        .check_retune(1, 13_500_000)
        .expect_err("onto receiver 2");
    assert_eq!(e.code, v1::ResultCode::InvalidArgument);
    assert!(e.message.contains("receiver 2"), "{}", e.message);
    let e = st
        .receivers
        .check_retune(9, 7_000_000)
        .expect_err("unknown");
    assert_eq!(e.code, v1::ResultCode::NotFound);
    st.receivers.retuned(1, 7_200_000).await;
    let stored = StoredSet::load(&st.dir.path().join("receivers/receivers.json")).expect("store");
    assert_eq!(
        stored.get(1).map(|r| r.center_frequency_hz),
        Some(7_200_000)
    );
    // The hub's set with the new centre restarts nothing.
    let mut moved = standard_set();
    moved.receivers[0].center_frequency_hz = 7_200_000;
    let calls = st.units.calls().len();
    st.receivers.configure(&moved).await.expect("moved");
    assert_eq!(st.units.calls().len(), calls);
}

#[tokio::test]
async fn an_unwritable_store_refuses_the_set() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("not-a-dir"), "x").expect("write");
    let receivers = Receivers::new(
        ReceiverPaths {
            store: dir.path().join("not-a-dir/receivers.json"),
            usb_devices: dir.path().join("usb"),
            packaged: false,
        },
        Driver::fake(),
        ReceiverOptions::default(),
    );
    super::testing::plug(&dir.path().join("usb"), "1-1", "CH000001", "RTL2838UHIDIR");
    receivers.start().await;
    let set = v1::ConfigureReceivers {
        receivers: vec![receiver(1, "CH000001", ProfileId::Rtlsdr, 98_000_000)],
    };
    let refused = receivers.configure(&set).await.expect_err("unwritable");
    assert_eq!(refused.code, v1::ResultCode::BackendError as i32);
    assert!(refused.message.starts_with("cannot store the receiver set"));
    assert!(receivers.stored().receivers.is_empty());
}

#[test]
fn state_names() {
    for s in [
        S::Starting,
        S::Running,
        S::Failed,
        S::NoDevice,
        S::Unspecified,
    ] {
        assert!(!state_name(s).is_empty());
    }
    let rx = receiver_capabilities(
        3,
        ProfileId::RtlsdrV4,
        v1::ReceiverCapabilities {
            max_channels: 64,
            sdr_model: "x".repeat(100),
            tunable_range: Some(v1::FrequencyRange {
                start_hz: 1,
                end_hz: 7_000_000_000,
            }),
            frontend: Some(v1::FrontendControls {
                antenna_ports: vec!["y".repeat(40); 9],
                min_gain_db: f32::NAN,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    assert_eq!(rx.receiver_id, 3);
    assert_eq!(rx.max_channels, limits::CHANNELS_PER_RECEIVER);
    assert_eq!(rx.sdr_model.len(), limits::MAX_SDR_MODEL_BYTES);
    assert_eq!(
        rx.tunable_range.map(|r| r.end_hz),
        Some(limits::MAX_FREQUENCY_HZ)
    );
    let hw = rx.hardware_range.expect("hardware");
    // The profile's range (from 0 Hz) widened to hold the tunable one.
    assert_eq!(hw.start_hz, 0);
    assert_eq!(hw.end_hz, limits::MAX_FREQUENCY_HZ);
    let f = rx.frontend.expect("controls");
    assert_eq!(f.antenna_ports.len(), limits::MAX_ANTENNA_PORTS);
    assert!(f.min_gain_db.is_finite());
    assert!(format!("{:?}", Driver::fake()).contains("FakeBackends"));
    assert!(format!("{:?}", SharedBackend(Arc::new(FakeBackend::default()))).contains("fake-"));
}
