// SPDX-License-Identifier: GPL-3.0-only
//! Certificate renewal (station-protocol §8).
//!
//! A task in the daemon renews the station certificate through
//! `StationControl.RenewCertificate` once two thirds of its validity period
//! (`notBefore` to `notAfter`) has passed:
//!
//! | Situation | Next attempt |
//! |---|---|
//! | not due yet | at [`due_at`] (re-checked at least every [`MAX_WAIT`], so a wall clock set late, as on a Pi without a clock battery, is noticed) |
//! | renewal failed (network, refusal) | after [`retry_after`]: 1/24 of the validity period, 30 s to 1 h (hourly for 30-day certificates), with jitter |
//! | the renewed certificate could not be stored | within a minute ([`LOCAL_WRITE_RETRY`]): the control plane revokes the old certificate 1 hour after a renewal |
//! | refused as revoked | the daemon is told ([`Signal::Revoked`]) and stops, unless a newer certificate is on disk |
//!
//! After a successful renewal and atomic swap ([`crate::state`]), the task
//! tells the daemon ([`Signal::Renewed`]), which closes the tunnel and
//! reconnects at once with the new certificate (no backoff: this is not a
//! failure).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::credentials::{CredentialSource, CredentialsError};
use crate::station_api::{ControlError, Renewed, StationApi};
use crate::tls::Validity;

/// Wall-clock time, Unix seconds (injectable for tests).
pub type UnixClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Shortest wait before retrying a failed renewal.
pub const MIN_RETRY: Duration = Duration::from_secs(30);
/// Longest wait before retrying a failed renewal.
pub const MAX_RETRY: Duration = Duration::from_secs(3600);
/// Wait before renewing again after the renewed certificate could not be
/// stored: uniform in `[LOCAL_WRITE_RETRY / 2, LOCAL_WRITE_RETRY]`.
pub const LOCAL_WRITE_RETRY: Duration = Duration::from_secs(60);
/// Longest single sleep while waiting for the renewal time; the wall clock
/// is read again after it.
pub const MAX_WAIT: Duration = Duration::from_secs(15 * 60);

/// When a certificate with validity `v` is due for renewal, Unix seconds:
/// `notBefore + ⅔ · (notAfter − notBefore)` (rounded down).
pub fn due_at(v: Validity) -> i64 {
    let lifetime = i128::from(v.not_after_unix_s) - i128::from(v.not_before_unix_s);
    let due = i128::from(v.not_before_unix_s) + (2 * lifetime.max(0)) / 3;
    i64::try_from(due).unwrap_or(i64::MAX)
}

/// The base wait before retrying a failed renewal of a certificate with
/// validity `v`: `(notAfter − notBefore) / 24`, clamped to 30 s … 1 h.
pub fn retry_after(v: Validity) -> Duration {
    let lifetime = i128::from(v.not_after_unix_s) - i128::from(v.not_before_unix_s);
    let secs = u64::try_from((lifetime / 24).max(0)).unwrap_or(u64::MAX);
    Duration::from_secs(secs).clamp(MIN_RETRY, MAX_RETRY)
}

/// `d` with jitter: uniform in `[¾·d, d]` (millisecond resolution), from
/// the random value `r`.
pub fn jitter(d: Duration, r: u64) -> Duration {
    let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let span = ms / 4;
    let extra = r % span.saturating_add(1);
    Duration::from_millis(ms - span + extra)
}

/// What `stationd status` shows about renewal (runtime only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewalStatus {
    /// When this daemon last renewed the certificate, Unix seconds.
    pub last_renewal_unix_s: Option<i64>,
    /// When the next attempt is planned, Unix seconds.
    pub next_renewal_unix_s: Option<i64>,
    /// Why the last attempt failed.
    pub last_error: Option<String>,
}

/// Timing knobs for renewal. [`RenewalOptions::default`] is the production
/// behaviour; tests inject a clock tied to Tokio's paused time.
pub struct RenewalOptions {
    /// Wall-clock time, Unix seconds.
    pub clock: UnixClock,
    /// Random source for the jitter.
    pub random: Box<dyn FnMut() -> u64 + Send>,
    /// Longest single sleep until the renewal time ([`MAX_WAIT`]).
    pub max_wait: Duration,
}

impl Default for RenewalOptions {
    fn default() -> Self {
        Self {
            clock: Arc::new(crate::local_status::unix_now),
            random: Box::new(rand::random::<u64>),
            max_wait: MAX_WAIT,
        }
    }
}

impl fmt::Debug for RenewalOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenewalOptions")
            .field("max_wait", &self.max_wait)
            .finish_non_exhaustive()
    }
}

/// What the renewal task tells the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Signal {
    /// A new certificate is stored; reconnect with it.
    Renewed {
        /// Its serial.
        serial: String,
    },
    /// `station-api` refused the certificate with this serial as revoked.
    Revoked {
        /// The refused serial.
        serial: String,
    },
}

/// The current certificate, as renewal needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CertInfo {
    pub serial: String,
    pub validity: Validity,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The two operations renewal needs (a trait so the timing can be tested
/// without a network).
pub(crate) trait Renewal: Send + Sync {
    /// The certificate in use now.
    fn current(&self) -> Result<CertInfo, CredentialsError>;
    /// Renews and stores a new certificate.
    fn renew(&self) -> BoxFuture<'_, Result<Renewed, ControlError>>;
}

/// Renewal through `station-api` for the credentials in `source`.
pub(crate) struct ApiRenewal {
    pub api: Arc<StationApi>,
    pub source: Arc<dyn CredentialSource>,
}

impl Renewal for ApiRenewal {
    fn current(&self) -> Result<CertInfo, CredentialsError> {
        let c = self.source.load()?;
        Ok(CertInfo {
            serial: c.tls.serial,
            validity: c.tls.validity,
        })
    }

    fn renew(&self) -> BoxFuture<'_, Result<Renewed, ControlError>> {
        Box::pin(self.api.renew())
    }
}

/// Sleeps `d`; `false` if `shutdown` came first.
async fn sleep_or_stop(d: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        () = tokio::time::sleep(d) => true,
        () = shutdown.cancelled() => false,
    }
}

fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// The renewal task: runs until `shutdown` or until the daemon stops
/// listening on `signals`. `publish` receives every status change.
pub(crate) async fn run(
    renewal: Arc<dyn Renewal>,
    mut options: RenewalOptions,
    shutdown: CancellationToken,
    signals: mpsc::Sender<Signal>,
    publish: impl Fn(&RenewalStatus) + Send,
) {
    let mut status = RenewalStatus::default();
    let mut load_warned = false;
    loop {
        let current = match renewal.current() {
            Ok(c) => {
                load_warned = false;
                c
            }
            Err(e) => {
                if !load_warned {
                    warn!(error = %e, "renewal: cannot read the current certificate");
                    load_warned = true;
                }
                status.last_error = Some(e.to_string());
                publish(&status);
                if !sleep_or_stop(options.max_wait, &shutdown).await {
                    return;
                }
                continue;
            }
        };
        let now = (options.clock)();
        let due = due_at(current.validity);
        if now < due {
            if status.next_renewal_unix_s != Some(due) {
                status.next_renewal_unix_s = Some(due);
                publish(&status);
                debug!(serial = %current.serial, due, "certificate renewal scheduled");
            }
            let wait = Duration::from_secs(u64::try_from(due - now).unwrap_or(0));
            if !sleep_or_stop(wait.min(options.max_wait), &shutdown).await {
                return;
            }
            continue;
        }
        info!(
            serial = %current.serial,
            not_after = current.validity.not_after_unix_s,
            "renewing the station certificate (two thirds of its lifetime have passed)"
        );
        let result = tokio::select! {
            r = renewal.renew() => r,
            () = shutdown.cancelled() => return,
        };
        let wait = match result {
            Ok(renewed) => {
                status.last_renewal_unix_s = Some((options.clock)());
                status.last_error = None;
                status.next_renewal_unix_s = Some(due_at(renewed.validity));
                publish(&status);
                if signals
                    .send(Signal::Renewed {
                        serial: renewed.serial,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(ControlError::Revoked { serial }) => {
                status.last_error = Some("the certificate was revoked".to_owned());
                publish(&status);
                if signals.send(Signal::Revoked { serial }).await.is_err() {
                    return;
                }
                // The daemon stops, unless a newer certificate is on disk
                // (the owner enrolled again): then carry on with that one.
                jitter(retry_after(current.validity), (options.random)())
            }
            Err(e @ ControlError::Store(_)) => {
                error!(
                    error = %e,
                    "renewal: the new certificate could not be stored; renewing again within a minute"
                );
                status.last_error = Some(e.to_string());
                jitter(LOCAL_WRITE_RETRY, (options.random)()).max(LOCAL_WRITE_RETRY / 2)
            }
            Err(e) => {
                let wait = jitter(retry_after(current.validity), (options.random)());
                warn!(
                    error = %e,
                    retry_in_s = wait.as_secs(),
                    "renewal failed; retrying"
                );
                status.last_error = Some(e.to_string());
                wait
            }
        };
        status.next_renewal_unix_s = Some(now.saturating_add(secs(wait)));
        publish(&status);
        if !sleep_or_stop(wait, &shutdown).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Mutex;

    fn v(nb: i64, na: i64) -> Validity {
        Validity {
            not_before_unix_s: nb,
            not_after_unix_s: na,
        }
    }

    #[test]
    fn schedule() {
        // 30 days: due after 20 days; retry hourly.
        let d30 = v(0, 30 * 86_400);
        assert_eq!(due_at(d30), 20 * 86_400);
        assert_eq!(retry_after(d30), MAX_RETRY);
        // A 6-minute certificate backdated 5 minutes (11 minutes as seen
        // in the certificate): due 7 min 20 s after notBefore, i.e. about
        // 2 min 20 s after issuance; retry after 30 s.
        let six = v(1000, 1000 + 660);
        assert_eq!(due_at(six), 1000 + 440);
        assert_eq!(retry_after(six), MIN_RETRY);
        // 1 day: retry after 1 h; 12 h: after 30 min.
        assert_eq!(retry_after(v(0, 86_400)), Duration::from_secs(3600));
        assert_eq!(retry_after(v(0, 43_200)), Duration::from_secs(1800));
        // Garbage validity never panics and is due at once.
        assert_eq!(due_at(v(10, 5)), 10);
        assert_eq!(retry_after(v(10, 5)), MIN_RETRY);
        assert_eq!(due_at(v(i64::MIN, i64::MAX)), 3_074_457_345_618_258_602);
        assert_eq!(retry_after(v(i64::MIN, i64::MAX)), MAX_RETRY);
    }

    #[test]
    fn jitter_bounds() {
        let d = Duration::from_secs(100);
        assert_eq!(jitter(d, 0), Duration::from_secs(75));
        assert_eq!(jitter(d, 25_000), d);
        assert_eq!(jitter(Duration::ZERO, 7), Duration::ZERO);
        assert_eq!(
            jitter(Duration::from_millis(3), u64::MAX),
            Duration::from_millis(3)
        );
    }

    proptest! {
        #[test]
        fn due_is_inside_the_validity(nb in -1_i64 << 40..1_i64 << 40, len in 0_i64..1 << 32) {
            let validity = v(nb, nb + len);
            let due = due_at(validity);
            prop_assert!(due >= nb && due <= nb + len);
            prop_assert!(3 * (due - nb) <= 2 * len && 3 * (due - nb) + 3 > 2 * len);
            let r = retry_after(validity);
            prop_assert!(r >= MIN_RETRY && r <= MAX_RETRY);
        }

        #[test]
        fn jitter_is_between_three_quarters_and_all(ms in 0_u64..1 << 40, r: u64) {
            let d = Duration::from_millis(ms);
            let j = jitter(d, r);
            prop_assert!(j <= d);
            prop_assert!(j.as_millis() * 4 >= d.as_millis() * 3);
        }
    }

    /// A scripted renewal: the certificate validity and what each renewal
    /// returns; records when renewals happen (Tokio time since start).
    struct Fake {
        start: tokio::time::Instant,
        current: Mutex<CertInfo>,
        results: Mutex<Vec<Result<Renewed, ControlError>>>,
        calls: Mutex<Vec<Duration>>,
        broken: Mutex<bool>,
    }

    impl Fake {
        fn new(validity: Validity, results: Vec<Result<Renewed, ControlError>>) -> Arc<Self> {
            Arc::new(Self {
                start: tokio::time::Instant::now(),
                current: Mutex::new(CertInfo {
                    serial: "01".into(),
                    validity,
                }),
                results: Mutex::new(results),
                calls: Mutex::new(Vec::new()),
                broken: Mutex::new(false),
            })
        }

        fn calls(&self) -> Vec<u64> {
            self.calls
                .lock()
                .expect("lock")
                .iter()
                .map(Duration::as_secs)
                .collect()
        }
    }

    impl Renewal for Fake {
        fn current(&self) -> Result<CertInfo, CredentialsError> {
            if *self.broken.lock().expect("lock") {
                return Err(CredentialsError::NotEnrolled {
                    dir: "/x".into(),
                    missing: vec!["key.pem"],
                });
            }
            Ok(self.current.lock().expect("lock").clone())
        }

        fn renew(&self) -> BoxFuture<'_, Result<Renewed, ControlError>> {
            Box::pin(async move {
                self.calls.lock().expect("lock").push(self.start.elapsed());
                let mut results = self.results.lock().expect("lock");
                let r = if results.is_empty() {
                    Err(ControlError::Timeout {
                        endpoint: "x".into(),
                    })
                } else {
                    results.remove(0)
                };
                if let Ok(renewed) = &r {
                    *self.current.lock().expect("lock") = CertInfo {
                        serial: renewed.serial.clone(),
                        validity: renewed.validity,
                    };
                }
                r
            })
        }
    }

    fn renewed(serial: &str, validity: Validity) -> Renewed {
        Renewed {
            serial: serial.into(),
            validity,
            previous_serial: "01".into(),
            ca_rotated: false,
        }
    }

    /// Unix time that follows Tokio's (paused) clock from `base`.
    fn clock(base: i64) -> UnixClock {
        let start = tokio::time::Instant::now();
        Arc::new(move || base + i64::try_from(start.elapsed().as_secs()).unwrap_or(0))
    }

    struct Running {
        signals: mpsc::Receiver<Signal>,
        statuses: Arc<Mutex<Vec<RenewalStatus>>>,
        shutdown: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    fn start(fake: &Arc<Fake>, base: i64, random: u64) -> Running {
        let (tx, signals) = mpsc::channel(8);
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let shutdown = CancellationToken::new();
        let seen = Arc::clone(&statuses);
        let renewal: Arc<dyn Renewal> = fake.clone();
        let task = tokio::spawn(run(
            renewal,
            RenewalOptions {
                clock: clock(base),
                random: Box::new(move || random),
                max_wait: MAX_WAIT,
            },
            shutdown.clone(),
            tx,
            move |s: &RenewalStatus| seen.lock().expect("lock").push(s.clone()),
        ));
        Running {
            signals,
            statuses,
            shutdown,
            task,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn renews_at_two_thirds_then_schedules_the_next() {
        // Issued at 0, valid 3000 s: due at 2000 s.
        let next = v(2000, 5000);
        let fake = Fake::new(v(0, 3000), vec![Ok(renewed("02", next))]);
        let mut r = start(&fake, 100, 0);
        let signal = r.signals.recv().await.expect("signal");
        assert_eq!(
            signal,
            Signal::Renewed {
                serial: "02".into()
            }
        );
        // Started at unix 100: the renewal ran 1900 s later.
        assert_eq!(fake.calls(), vec![1900]);
        let s = r.statuses.lock().expect("lock").clone();
        assert_eq!(s[0].next_renewal_unix_s, Some(2000));
        let last = s.last().expect("status");
        assert_eq!(last.last_renewal_unix_s, Some(2000));
        assert_eq!(last.next_renewal_unix_s, Some(4000));
        assert_eq!(last.last_error, None);
        // Nothing more until the new certificate is due.
        tokio::time::sleep(Duration::from_secs(1999)).await;
        assert_eq!(fake.calls().len(), 1);
        r.shutdown.cancel();
        r.task.await.expect("task");
    }

    #[tokio::test(start_paused = true)]
    async fn failures_retry_on_the_schedule() {
        // 2400 s validity: due at 1600, retry every 100 s (jitter 0 → 75 s).
        let fake = Fake::new(v(0, 2400), vec![]);
        let r = start(&fake, 1600, 0);
        tokio::time::sleep(Duration::from_secs(301)).await;
        assert_eq!(fake.calls(), vec![0, 75, 150, 225, 300]);
        let s = r.statuses.lock().expect("lock").clone();
        let last = s.last().expect("status");
        assert!(
            last.last_error
                .as_deref()
                .is_some_and(|e| e.contains("in time"))
        );
        assert_eq!(last.next_renewal_unix_s, Some(1600 + 375));
        r.shutdown.cancel();
        r.task.await.expect("task");
    }

    #[tokio::test(start_paused = true)]
    async fn thirty_day_certificates_retry_hourly() {
        let fake = Fake::new(v(0, 30 * 86_400), vec![]);
        // Full jitter span: 3600 s exactly.
        let r = start(&fake, 25 * 86_400, 900_000);
        tokio::time::sleep(Duration::from_secs(3 * 3600 + 1)).await;
        assert_eq!(fake.calls(), vec![0, 3600, 7200, 10_800]);
        r.shutdown.cancel();
        r.task.await.expect("task");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_local_write_retries_within_a_minute() {
        let store = || {
            Err(ControlError::Store(crate::state::StateError {
                path: "/x".into(),
                source: std::io::Error::other("disk full"),
            }))
        };
        let fake = Fake::new(
            v(0, 30 * 86_400),
            vec![store(), store(), Ok(renewed("03", v(0, 60 * 86_400)))],
        );
        let mut r = start(&fake, 21 * 86_400, 0);
        assert_eq!(
            r.signals.recv().await,
            Some(Signal::Renewed {
                serial: "03".into()
            })
        );
        assert_eq!(fake.calls(), vec![0, 45, 90]);
        r.shutdown.cancel();
        r.task.await.expect("task");
    }

    #[tokio::test(start_paused = true)]
    async fn revocation_is_signalled() {
        let revoked = || {
            Err(ControlError::Revoked {
                serial: "01".into(),
            })
        };
        let fake = Fake::new(v(0, 3000), vec![revoked(), revoked()]);
        let mut r = start(&fake, 2500, 0);
        assert_eq!(
            r.signals.recv().await,
            Some(Signal::Revoked {
                serial: "01".into()
            })
        );
        let s = r.statuses.lock().expect("lock").clone();
        assert!(s.last().expect("status").last_error.is_some());
        // The daemon went away: the task ends at its next signal.
        drop(r.signals);
        tokio::time::sleep(Duration::from_secs(200)).await;
        assert!(r.task.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn unreadable_credentials_are_retried_and_shutdown_stops() {
        let fake = Fake::new(v(0, 3000), vec![]);
        *fake.broken.lock().expect("lock") = true;
        let r = start(&fake, 0, 0);
        tokio::time::sleep(MAX_WAIT * 2 + Duration::from_secs(1)).await;
        assert!(fake.calls().is_empty());
        let s = r.statuses.lock().expect("lock").clone();
        assert_eq!(s.len(), 3, "{s:?}");
        *fake.broken.lock().expect("lock") = false;
        tokio::time::sleep(MAX_WAIT).await;
        r.shutdown.cancel();
        r.task.await.expect("stops");
        // A clock far behind notBefore still wakes up every MAX_WAIT.
        let fake = Fake::new(v(1_000_000, 4_000_000), vec![]);
        let r = start(&fake, 0, 0);
        tokio::time::sleep(MAX_WAIT * 3).await;
        assert!(fake.calls().is_empty());
        r.shutdown.cancel();
        r.task.await.expect("stops");
        assert!(format!("{:?}", RenewalOptions::default()).contains("max_wait"));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_a_renewal_or_a_retry() {
        let fake = Fake::new(v(0, 3000), vec![]);
        let r = start(&fake, 2000, 0);
        tokio::time::sleep(Duration::from_secs(10)).await;
        r.shutdown.cancel();
        r.task.await.expect("stops during the retry wait");
    }
}
