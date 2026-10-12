// SPDX-License-Identifier: GPL-3.0-only
//! A station's receivers on fake backends, fake units and simulated SDRs,
//! for the unit tests of the receivers, the dispatch and measuring.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use station_proto::v1;

use super::{Driver, FakeBackends, FakeUnits, ReceiverOptions, ReceiverPaths, Receivers, RxEvent};
use crate::profile::ProfileId;

/// Simulated SDRs, fake backends and units, in a scratch directory.
pub(crate) struct Station {
    pub receivers: Receivers,
    pub backends: FakeBackends,
    pub units: FakeUnits,
    pub dir: tempfile::TempDir,
}

/// The generic RTL-SDR and the V4 the standard set runs on.
pub(crate) const STANDARD_SDRS: [(&str, &str, &str); 2] = [
    ("1-1.2", "CH000001", "RTL2838UHIDIR"),
    ("1-1.3", "CH000002", "Blog V4"),
];

/// Writes an RTL-SDR at `port` under `usb`.
pub(crate) fn plug(usb: &Path, port: &str, serial: &str, product: &str) {
    let d = usb.join(port);
    std::fs::create_dir_all(&d).expect("mkdir");
    std::fs::write(d.join("idVendor"), "0bda\n").expect("write");
    std::fs::write(d.join("idProduct"), "2838\n").expect("write");
    std::fs::write(d.join("product"), format!("{product}\n")).expect("write");
    std::fs::write(d.join("serial"), format!("{serial}\n")).expect("write");
}

impl Station {
    /// Receivers over `sdrs`, started, watched every 10 ms.
    pub(crate) async fn new(sdrs: &[(&str, &str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tmp");
        let station = Self::in_dir(dir);
        for (port, serial, product) in sdrs {
            station.plug(port, serial, product);
        }
        station.receivers.start().await;
        station
    }

    /// Receivers in `dir` (its `receivers.json` and `usb/`), not started.
    pub(crate) fn in_dir(dir: tempfile::TempDir) -> Self {
        Self::in_dir_with(dir, FakeBackends::default())
    }

    /// The standard station on `backends`.
    pub(crate) async fn standard_with(backends: FakeBackends) -> Self {
        let st = Self::in_dir_with(tempfile::tempdir().expect("tmp"), backends);
        for (port, serial, product) in STANDARD_SDRS {
            st.plug(port, serial, product);
        }
        st.receivers.start().await;
        st.receivers
            .configure(&standard_set())
            .await
            .expect("the standard set");
        st.wait_running(&[1, 2]).await;
        st
    }

    /// Receivers in `dir` on `backends`, not started.
    pub(crate) fn in_dir_with(dir: tempfile::TempDir, backends: FakeBackends) -> Self {
        let units = FakeUnits::default();
        let receivers = Receivers::new(
            ReceiverPaths {
                store: dir.path().join("receivers/receivers.json"),
                usb_devices: dir.path().join("usb"),
                packaged: false,
            },
            Driver {
                backends: Arc::new(backends.clone()),
                units: Arc::new(units.clone()),
            },
            ReceiverOptions {
                poll: Duration::from_millis(10),
                hardware_poll: Duration::from_millis(20),
            },
        );
        Self {
            receivers,
            backends,
            units,
            dir,
        }
    }

    /// The standard station: [`STANDARD_SDRS`], with [`standard_set`]
    /// running.
    pub(crate) async fn standard() -> Self {
        let st = Self::new(&STANDARD_SDRS).await;
        st.receivers
            .configure(&standard_set())
            .await
            .expect("the standard set");
        st.wait_running(&[1, 2]).await;
        st
    }

    pub(crate) fn usb(&self) -> PathBuf {
        self.dir.path().join("usb")
    }

    pub(crate) fn plug(&self, port: &str, serial: &str, product: &str) {
        plug(&self.usb(), port, serial, product);
    }

    pub(crate) fn unplug(&self, port: &str) {
        let d = self.usb().join(port);
        for f in ["idVendor", "idProduct", "product", "serial"] {
            std::fs::remove_file(d.join(f)).expect("remove");
        }
        std::fs::remove_dir(d).expect("rmdir");
    }

    /// Waits (at most 10 s) until every receiver of `ids` runs.
    pub(crate) async fn wait_running(&self, ids: &[u32]) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let running: Vec<u32> = self.receivers.running().iter().map(|r| r.0).collect();
                if ids.iter().all(|id| running.contains(id)) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("receivers {ids:?} not running"));
    }
}

/// Receiver `id` on `serial` with `profile` at `center`.
pub(crate) fn receiver(
    id: u32,
    serial: &str,
    profile: ProfileId,
    center: u64,
) -> v1::ReceiverConfig {
    let model = match profile {
        ProfileId::RtlsdrV4 => v1::SdrModel::RtlsdrBlogV4,
        ProfileId::RtlsdrHf => v1::SdrModel::RtlsdrDirectSampling,
        ProfileId::Rtlsdr | ProfileId::Siggen => v1::SdrModel::RtlsdrOther,
    };
    v1::ReceiverConfig {
        receiver_id: id,
        sdr_serial: serial.into(),
        model: model as i32,
        profile: profile.profile().proto.map_or(0, |p| p as i32),
        center_frequency_hz: center,
        ..Default::default()
    }
}

/// Receiver 1 by direct sampling on 40 m, receiver 2 on the V4 on 20 m.
pub(crate) fn standard_set() -> v1::ConfigureReceivers {
    v1::ConfigureReceivers {
        receivers: vec![
            receiver(1, "CH000001", ProfileId::RtlsdrHf, 7_100_000),
            receiver(2, "CH000002", ProfileId::RtlsdrV4, 14_200_000),
        ],
    }
}

/// Every event already sent on `rx`.
pub(crate) fn drain(rx: &mut tokio::sync::broadcast::Receiver<RxEvent>) -> Vec<RxEvent> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

/// The (receiver, state) of the `RECEIVER_STATE` events in `events`.
pub(crate) fn states(events: &[RxEvent]) -> Vec<(u32, v1::ReceiverState)> {
    events
        .iter()
        .filter(|e| e.event.kind == v1::event::Kind::ReceiverState as i32)
        .filter_map(|e| {
            Some((
                e.event.receiver_id?,
                v1::ReceiverState::try_from(e.event.receiver_state?).ok()?,
            ))
        })
        .collect()
}

/// The receivers of the `BACKEND_RESTARTED` events in `events`.
pub(crate) fn restarted(events: &[RxEvent]) -> Vec<u32> {
    events
        .iter()
        .filter(|e| e.event.kind == v1::event::Kind::BackendRestarted as i32)
        .filter_map(|e| e.event.receiver_id)
        .collect()
}
