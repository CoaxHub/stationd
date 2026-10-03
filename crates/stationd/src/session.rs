// SPDX-License-Identifier: GPL-3.0-only
//! One tunnel connection (station-protocol §2): dial, `Hello`, `Welcome`,
//! then answer requests, stream media, send `StationStatus`, and forward
//! backend events until the stream ends, the hub says `Goodbye`, or a
//! shutdown is requested.
//!
//! On any end, every channel and subscription of the connection is closed
//! in the backend (§2.3), and its measurement plan stops: nothing survives
//! into the next connection, and nothing measured is buffered for it.

use std::sync::{Arc, PoisonError};
use std::time::Duration;

use futures_util::StreamExt as _;
use station_backend::{ChannelOutput, EventStream};
use station_proto::v1::station_gateway_client::StationGatewayClient;
use station_proto::validate::negotiate_welcome;
use station_proto::{limits, v1};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Uri};
use tracing::{debug, error, info, warn};

use crate::daemon::{ConnectionState, Ctx, Link};
use crate::dispatch::{self, Media, RequestContext, Tables, truncate_utf8};
use crate::measure::{self, MeasureDeps, Measurer, StopReason};
use crate::outbound::Outbound;
use crate::renew::Signal;
use crate::status;

use v1::hub_message::Body as HubBody;
use v1::station_message::Body;

/// How a session ended.
#[derive(Debug)]
pub(crate) enum End {
    /// Shutdown was requested.
    Shutdown,
    /// The hub sent `Goodbye` (or `Welcome` named a version we cannot use,
    /// reported as `UNSUPPORTED_VERSION`).
    Goodbye(v1::Goodbye),
    /// Connect failure, stream error or end, welcome timeout.
    Lost(String),
    /// A renewed certificate is stored: reconnect with it at once.
    Renewed,
    /// `station-api` refused the certificate with this serial as revoked.
    Revoked(String),
}

/// A session's result for the reconnect loop.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub end: End,
    /// When `Welcome` arrived, if it did.
    pub established_at: Option<std::time::Instant>,
}

impl Outcome {
    fn before_welcome(end: End) -> Self {
        Self {
            end,
            established_at: None,
        }
    }
}

/// The error chain of `e`, joined with ": ".
fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}

fn endpoint(ctx: &Ctx, link: &Link) -> Result<Endpoint, String> {
    // tonic dials through our own TLS connector; the endpoint URI only
    // selects it, and `origin` sets the :scheme/:authority of requests.
    let authority = link.gateway.authority();
    let origin: Uri = format!("https://{authority}")
        .parse()
        .map_err(|e| format!("invalid gateway authority {authority}: {e}"))?;
    Ok(Endpoint::from_shared(format!("http://{authority}"))
        .map_err(|e| chain(&e))?
        .origin(origin)
        .connect_timeout(ctx.connect_timeout)
        .http2_keep_alive_interval(ctx.keepalive_interval)
        .keep_alive_timeout(ctx.keepalive_timeout)
        .keep_alive_while_idle(true)
        .tcp_nodelay(true))
}

/// Runs one connection attempt and, if it is established, the session.
pub(crate) async fn run(
    ctx: &Arc<Ctx>,
    link: &Link,
    shutdown: &CancellationToken,
    state: &watch::Sender<ConnectionState>,
    signals: &mut mpsc::Receiver<Signal>,
) -> Outcome {
    let endpoint = match endpoint(ctx, link) {
        Ok(e) => e,
        Err(e) => return Outcome::before_welcome(End::Lost(e)),
    };
    let channel = tokio::select! {
        r = endpoint.connect_with_connector(link.connector.clone()) => match r {
            Ok(c) => c,
            Err(e) => return Outcome::before_welcome(End::Lost(chain(&e))),
        },
        () = shutdown.cancelled() => return Outcome::before_welcome(End::Shutdown),
    };
    let outbound = Outbound::new(ctx.queue, Arc::clone(&ctx.drops));
    // Whatever happens next, end the request stream when this attempt ends:
    // hyper polls it from a background task, and an unfinished request
    // stream would keep the HTTP/2 stream and the connection alive.
    let _close_outbound = CloseOnDrop(Arc::clone(&outbound));
    outbound.push_body(Body::Hello(status::hello(&ctx.backend.info(), ctx.boot_id)));
    let mut client = StationGatewayClient::new(channel)
        .max_decoding_message_size(limits::MAX_MESSAGE_BYTES)
        .max_encoding_message_size(limits::MAX_MESSAGE_BYTES);
    let response = tokio::select! {
        r = client.connect(Arc::clone(&outbound).stream()) => r,
        () = shutdown.cancelled() => return Outcome::before_welcome(End::Shutdown),
    };
    let mut inbound = match response {
        Ok(r) => r.into_inner(),
        Err(status) => {
            return Outcome::before_welcome(End::Lost(format!("Connect failed: {status}")));
        }
    };
    let welcome = match await_welcome(ctx, &mut inbound, shutdown).await {
        Ok(w) => w,
        Err(end) => return Outcome::before_welcome(end),
    };
    let version = match negotiate_welcome(&welcome) {
        Ok(v) => v,
        Err(e) => {
            error!(error = %e, "the hub chose a protocol version this stationd cannot speak");
            let goodbye = v1::Goodbye {
                reason: v1::goodbye::Reason::UnsupportedVersion as i32,
                message: e.to_string(),
                ..Default::default()
            };
            return Outcome::before_welcome(End::Goodbye(goodbye));
        }
    };
    let established_at = tokio::time::Instant::now().into_std();
    record_clock_skew(ctx, welcome.hub_time_unix_ms, unix_now_ms());
    let connection_id = truncate_utf8(&welcome.connection_id, 64);
    info!(
        connection_id = ?connection_id,
        version = %format_args!("{}.{}", version.major, version.minor),
        status_interval_ms = welcome.status_interval_ms,
        "connected to the hub"
    );
    state.send_replace(ConnectionState::Connected {
        connection_id,
        version,
    });
    let status_interval = match welcome.status_interval_ms {
        0 => limits::DEFAULT_STATUS_INTERVAL,
        ms => Duration::from_millis(u64::from(ms)),
    };
    let tables: Arc<std::sync::Mutex<Tables>> = Arc::default();
    let measurer = (version.minor >= limits::MEASUREMENTS_MINOR).then(|| {
        Measurer::new(MeasureDeps {
            backend: Arc::clone(&ctx.backend),
            outbound: Arc::clone(&outbound),
            tables: Arc::clone(&tables),
            drops: Arc::clone(&ctx.drops),
            clock: Arc::clone(&ctx.measure_clock),
            host: Arc::new(status::host_health),
            clock_state: Arc::new(measure::clock::clock_state),
            enabled: ctx.policy.measurements,
        })
    });
    let session = Session {
        ctx: Arc::clone(ctx),
        outbound,
        requests: RequestContext {
            backend: Arc::clone(&ctx.backend),
            policy: Arc::clone(&ctx.policy),
            minor: version.minor,
            tables,
            measurer,
        },
    };
    let end = session
        .serve(inbound, status_interval, shutdown, signals, &link.serial)
        .await;
    Outcome {
        end,
        established_at: Some(established_at),
    }
}

/// Waits for the hub's first message: `Welcome`, or the reason there is
/// none.
async fn await_welcome(
    ctx: &Ctx,
    inbound: &mut tonic::Streaming<v1::HubMessage>,
    shutdown: &CancellationToken,
) -> Result<v1::Welcome, End> {
    let first = tokio::select! {
        r = tokio::time::timeout(ctx.welcome_timeout, inbound.message()) => r,
        () = shutdown.cancelled() => return Err(End::Shutdown),
    };
    match first {
        Err(_) => Err(End::Lost("no Welcome from the hub in time".into())),
        Ok(Err(status)) => Err(End::Lost(format!("stream error: {status}"))),
        Ok(Ok(None)) => Err(End::Lost("hub closed the stream before Welcome".into())),
        Ok(Ok(Some(msg))) => match msg.body {
            Some(HubBody::Welcome(w)) => Ok(w),
            Some(HubBody::Goodbye(g)) => Err(End::Goodbye(g)),
            Some(HubBody::Request(_)) | None => Err(End::Lost(
                "hub sent something other than Welcome or Goodbye first".into(),
            )),
        },
    }
}

/// Local wall-clock time, Unix milliseconds.
fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Records hub time minus local time (`Welcome.hub_time_unix_ms`, 0 when
/// the hub did not say), for `stationd status` and `doctor`. The round
/// trip is ignored: this is a coarse diagnostic, never used to refuse
/// anything.
fn record_clock_skew(ctx: &Ctx, hub_time_unix_ms: i64, local_unix_ms: i64) {
    if hub_time_unix_ms == 0 {
        return;
    }
    let skew = hub_time_unix_ms.saturating_sub(local_unix_ms);
    if skew.unsigned_abs() > crate::local_status::CLOCK_SKEW_WARN_MS.unsigned_abs() {
        warn!(
            clock_skew_ms = skew,
            "this station's clock differs from the hub's by more than 30 s; check NTP \
             (TLS certificates depend on the clock)"
        );
    }
    ctx.observed.send_modify(|o| o.clock_skew_ms = Some(skew));
}

/// Closes the outbound queue when dropped.
struct CloseOnDrop(Arc<Outbound>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct Session {
    ctx: Arc<Ctx>,
    outbound: Arc<Outbound>,
    requests: RequestContext,
}

impl Session {
    async fn serve(
        self,
        mut inbound: tonic::Streaming<v1::HubMessage>,
        status_interval: Duration,
        shutdown: &CancellationToken,
        signals: &mut mpsc::Receiver<Signal>,
        serial: &str,
    ) -> End {
        let mut tasks = JoinSet::new();
        let mut status_tick = tokio::time::interval(status_interval);
        status_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut events: EventStream = self.ctx.backend.events();
        let end = loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    self.shut_down(&mut inbound).await;
                    break End::Shutdown;
                }
                msg = inbound.message() => match msg {
                    Ok(Some(v1::HubMessage { body: Some(HubBody::Request(req)) })) => {
                        let requests = self.requests.clone();
                        let outbound = Arc::clone(&self.outbound);
                        tasks.spawn(handle_request(requests, outbound, req));
                    }
                    Ok(Some(v1::HubMessage { body: Some(HubBody::Goodbye(g)) })) => break End::Goodbye(g),
                    Ok(Some(v1::HubMessage { body: Some(HubBody::Welcome(_)) })) => {
                        warn!("hub sent a second Welcome; ignored");
                    }
                    Ok(Some(v1::HubMessage { body: None })) => {
                        debug!("hub message of an unknown type ignored (newer minor)");
                    }
                    Ok(None) => break End::Lost("hub closed the stream".into()),
                    Err(status) => break End::Lost(format!("stream error: {}", chain(&status))),
                },
                _ = status_tick.tick() => {
                    self.outbound.push_body(Body::Status(self.status()));
                }
                Some(signal) = signals.recv() => match signal {
                    // Renewed before this connection loaded it: already in use.
                    Signal::Renewed { serial: s } if s == serial => {}
                    Signal::Renewed { .. } => {
                        info!("certificate renewed: closing the tunnel to reconnect with it");
                        break End::Renewed;
                    }
                    Signal::Revoked { serial } => break End::Revoked(serial),
                },
                event = events.next() => {
                    if let Some(event) = event {
                        self.forward_event(event);
                    } else {
                        warn!("backend event stream ended");
                        events = Box::pin(futures_util::stream::pending());
                    }
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(e) = joined {
                        if e.is_panic() {
                            error!(error = %e, "request task panicked");
                        }
                    }
                }
            }
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.outbound.close();
        self.close_all().await;
        end
    }

    fn status(&self) -> v1::StationStatus {
        let (channels, subscriptions) = {
            let t = self
                .requests
                .tables
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (t.open_channels(), t.open_subscriptions())
        };
        status::station_status(
            &self.ctx.backend.health(),
            status::host_health(),
            status::Counters {
                uptime_s: self.ctx.started.elapsed().as_secs(),
                active_channels: u32::try_from(channels).unwrap_or(u32::MAX),
                active_subscriptions: u32::try_from(subscriptions).unwrap_or(u32::MAX),
                audio_frames_dropped_total: self.ctx.drops.audio(),
                waterfall_rows_dropped_total: self.ctx.drops.waterfall(),
                measurement_reports_dropped_total: self.ctx.drops.measurements(),
            },
        )
    }

    fn forward_event(&self, mut event: v1::Event) {
        use v1::event::Kind;
        // A measurement never straddles a change of settings or coverage: the
        // plan stops first, so the hub sees MEASUREMENT_STOPPED before the
        // event that made it stop.
        let stop = match Kind::try_from(event.kind) {
            Ok(Kind::FrontendChanged) => Some(StopReason::FrontendChanged),
            Ok(Kind::BackendRestarted) => Some(StopReason::BackendRestarted),
            _ => None,
        };
        if let (Some(reason), Some(m)) = (stop, &self.requests.measurer) {
            m.stop_for(reason);
        }
        {
            let mut t = self
                .requests
                .tables
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match Kind::try_from(event.kind) {
                Ok(Kind::ChannelClosed) => {
                    if let Some(id) = event.channel_id {
                        t.channel_closed(id);
                    }
                }
                Ok(Kind::SubscriptionClosed) => {
                    if let Some(id) = event.subscription_id {
                        t.subscription_closed(id);
                    }
                }
                Ok(Kind::BackendRestarted) => {
                    // All channels and subscriptions are gone (event docs).
                    let _ = t.take_all();
                }
                _ => {}
            }
        }
        event.detail = truncate_utf8(&event.detail, limits::MAX_DETAIL_BYTES);
        self.outbound.push_body(Body::Event(event));
    }

    /// Graceful end: `Event{SHUTTING_DOWN}`, half-close, wait (bounded) for
    /// the queue to flush and the hub to close its side.
    async fn shut_down(&self, inbound: &mut tonic::Streaming<v1::HubMessage>) {
        info!("shutting down: notifying the hub");
        self.outbound.push_body(Body::Event(v1::Event {
            kind: v1::event::Kind::ShuttingDown as i32,
            ..Default::default()
        }));
        self.outbound.close();
        let drained = tokio::time::timeout(self.ctx.shutdown_grace, async {
            // Done when the hub ends its side (normally in reply to our
            // end of stream). Requests arriving now cannot be answered.
            let flushed = self.outbound.finished();
            tokio::pin!(flushed);
            let mut is_flushed = false;
            loop {
                tokio::select! {
                    () = &mut flushed, if !is_flushed => is_flushed = true,
                    msg = inbound.message() => {
                        if !matches!(msg, Ok(Some(_))) {
                            break;
                        }
                    }
                }
            }
        })
        .await;
        if drained.is_err() {
            debug!("hub did not close the stream within the shutdown grace period");
        }
    }

    /// §2.3: closes every channel and subscription of this connection, and
    /// stops its plan.
    async fn close_all(&self) {
        if let Some(m) = &self.requests.measurer {
            m.shutdown();
        }
        let (channels, subscriptions) = self
            .requests
            .tables
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take_all();
        for id in channels {
            if let Err(e) = self.ctx.backend.close_channel(id).await {
                debug!(channel_id = id, error = %e, "closing channel after disconnect");
            }
        }
        for id in subscriptions {
            if let Err(e) = self.ctx.backend.unsubscribe_waterfall(id).await {
                debug!(subscription_id = id, error = %e, "closing subscription after disconnect");
            }
        }
    }
}

/// Answers one request, then (for an opened channel or subscription) pumps
/// its media into the queue until it is closed.
async fn handle_request(ctx: RequestContext, outbound: Arc<Outbound>, req: v1::Request) {
    let (response, media) = dispatch::dispatch(&ctx, req).await;
    outbound.push_body(Body::Response(response));
    match media {
        None => {}
        Some(Media::Audio { stream, stop, .. }) => {
            let mut stream = std::pin::pin!(stream.take_until(stop.cancelled_owned()));
            while let Some(item) = stream.next().await {
                let body = match item {
                    ChannelOutput::Audio(frame) => Body::Audio(frame),
                    ChannelOutput::Signal(signal) => Body::Signal(signal),
                };
                outbound.push_body(body);
            }
        }
        Some(Media::Rows { stream, stop, .. }) => {
            let mut stream = std::pin::pin!(stream.take_until(stop.cancelled_owned()));
            while let Some(row) = stream.next().await {
                outbound.push_body(Body::Waterfall(row));
            }
        }
    }
}
