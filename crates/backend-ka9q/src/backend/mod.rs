// SPDX-License-Identifier: GPL-3.0-only
//! [`Ka9qBackend`]: `station_backend::Backend` for ka9q-radio's `radiod`
//! (station-protocol §9), built on [`Ka9qClient`].
//!
//! | Protocol | radiod |
//! |---|---|
//! | `GetCapabilities` | front-end fields of a fresh status (a temporary probe channel when nothing recent was heard): tunable range, sample rate, `DESCRIPTION` ([`map::capabilities`]) |
//! | `OpenChannel` | a dynamic Opus channel under a new random SSRC: preset from the mode ([`map::preset_for`]), frequency, edges, sample rate, bit rate, mono, squelch, `LIFETIME` refreshed by the client |
//! | `UpdateChannel` | the changed fields on the same SSRC (a mode change re-sends the full configuration); the RTP sequence continues |
//! | `CloseChannel` | `LIFETIME` 1 |
//! | `SubscribeWaterfall` | a spectrum2 channel whose `BIN_COUNT` × `RESOLUTION_BW` tiles the range ([`map::geometry`]), polled every `row_interval_ms`; rows requantized to dB/Hz and zstd-compressed ([`media`]) |
//! | `SetFrontend` | `FIRST_LO_FREQUENCY` for a retune, verified in the reply; gain, AGC, attenuation and antenna are `UNSUPPORTED` |
//! | `AudioFrame` | the Opus payload of each RTP packet; `seq` re-based per channel (gaps kept), `timestamp` on the 48 kHz RTP clock re-based per channel |
//! | `ChannelSignal` | SNR and baseband power from the channel's status, at most every 200 ms |
//! | measurement channels (1.1) | a spectrum2 channel per plan that stationd polls itself, `BIN_COUNT`, `RESOLUTION_BW` and `SPECTRUM_AVG` fixed for its life, hopped by `RADIO_FREQUENCY` in place on radiod's placement grid, closed with `LIFETIME` 1 ([`measure`]) |
//!
//! The reported `ChannelOpened.audio` is what radiod actually runs: its
//! `OUTPUT_SAMPRATE` (48 kHz for `wfm`, which radiod forces only once the
//! demodulator restarts, [`map::actual_sample_rate`]), and the Opus frame
//! duration of its block time (20 ms).
//!
//! **Connecting.** The client is created at [`Ka9qBackend::new`] when the
//! data group is configured, otherwise on first use: a temporary client
//! probes radiod for its `OUTPUT_DATA_DEST_SOCKET` first. Until that
//! succeeds, requests fail with `BACKEND_ERROR` and [`Backend::health`]
//! reports `DOWN`; each request retries.
//!
//! **Health.** `RUNNING` while radiod answers; `DOWN` if it never answered
//! or its last command timed out after the last status seen; `DEGRADED` if
//! nothing was heard for [`STALE_AFTER`]. A background monitor probes an
//! idle radiod every [`IDLE_PROBE_INTERVAL`] so the state stays current,
//! counts front-end overloads (`AD_OVER` increases) and sends
//! `Event{FRONTEND_OVERLOAD}` at most every [`OVERLOAD_EVENT_INTERVAL`].
//!
//! radiod does not report its tuner's range, whether the tuner is locked,
//! or which runtime controls its driver has, so capabilities advertise no
//! front-end controls and `hardware_range` equals the tunable range.

pub mod group;
pub mod map;
pub mod measure;
pub mod media;
pub mod placement;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use futures_util::StreamExt as _;
use station_backend::{
    AudioStream, Backend, BackendError, ChannelId, EventStream, Health, MeasurementChannel,
    MeasurementGrid, MeasurementId, RowStream, Spectrum, SubscriptionId,
};
use station_proto::v1;
use tokio::sync::{OnceCell, broadcast};
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::client::{
    AudioChannel, AudioChannelParams, AudioUpdate, ClientConfig, ClientError, Ka9qClient,
    SpectrumChannel, SpectrumParams,
};
use crate::status::ChannelStatus;
use crate::tags;

use media::{RowEncoder, SharedSquelch, StatusSource};

/// A radiod status younger than this is used as is for capabilities and
/// range checks; an older one is refreshed with a probe.
pub const FRESH_STATUS: Duration = Duration::from_secs(5);
/// With nothing heard from radiod for this long, health is `DEGRADED`.
pub const STALE_AFTER: Duration = Duration::from_secs(120);
/// How often the monitor probes a radiod that has been silent (no channel
/// of this or any other client) and retries connecting.
pub const IDLE_PROBE_INTERVAL: Duration = Duration::from_secs(60);
/// How often the monitor looks at front-end overloads.
pub const MONITOR_INTERVAL: Duration = Duration::from_secs(1);
/// `Event{FRONTEND_OVERLOAD}` is sent at most this often.
pub const OVERLOAD_EVENT_INTERVAL: Duration = Duration::from_secs(10);
/// The front end counts as overloaded this long after `AD_OVER` grew.
pub const OVERLOAD_HOLD: Duration = Duration::from_secs(10);
/// A retune within this many Hz of the request counts as applied (tuners
/// round to their synthesizer step).
pub const RETUNE_TOLERANCE_HZ: f64 = 1000.0;
/// Buffered backend events per subscriber.
const EVENT_BUFFER: usize = 64;

/// How to reach radiod.
#[derive(Debug, Clone, PartialEq)]
pub struct Ka9qConfig {
    /// radiod's status and command group (`status =` in its config, port
    /// 5006); see [`group::parse_group`]. A unicast address selects the
    /// client's hermetic test mode.
    pub status_group: SocketAddr,
    /// radiod's RTP group (`data =`, port 5004). `None` asks radiod
    /// (`OUTPUT_DATA_DEST_SOCKET` of a probe channel).
    pub data_group: Option<SocketAddr>,
    /// Timing and queue settings for the client. Its `status_addr`,
    /// `data_addr` and `channel_status_addr` are replaced from the groups;
    /// the default uses a 10 s channel lifetime, 250 ms command timeout,
    /// 2 retries and a 1 s audio queue.
    pub client: ClientConfig,
}

impl Ka9qConfig {
    /// radiod at `status_group`, with the data group given or discovered.
    pub fn new(status_group: SocketAddr, data_group: Option<SocketAddr>) -> Self {
        Self {
            status_group,
            data_group,
            client: ClientConfig {
                audio_queue_len: 50,
                spectrum_queue_len: 8,
                ..ClientConfig::default()
            },
        }
    }

    fn client_config(&self, data: SocketAddr) -> ClientConfig {
        ClientConfig {
            status_addr: self.status_group,
            data_addr: data,
            channel_status_addr: Some(group::channel_status_addr(data)),
            ..self.client.clone()
        }
    }

    /// The longest `row_interval_ms` the client can poll at (a quarter of
    /// the channel lifetime).
    fn max_row_interval_ms(&self) -> u32 {
        u32::try_from((self.client.lifetime() / 4).as_millis()).unwrap_or(u32::MAX)
    }
}

/// An open audio channel.
#[derive(Debug)]
struct Channel {
    /// `None` once closed. The async mutex serializes updates and close.
    handle: Arc<tokio::sync::Mutex<Option<AudioChannel>>>,
    frequency_hz: u64,
    mode: i32,
    squelch: SharedSquelch,
    stop: CancellationToken,
}

/// An open waterfall subscription.
#[derive(Debug)]
struct Subscription {
    handle: Arc<tokio::sync::Mutex<Option<SpectrumChannel>>>,
    range: v1::FrequencyRange,
    stop: CancellationToken,
}

#[derive(Debug, Default)]
struct State {
    channels: BTreeMap<ChannelId, Channel>,
    subscriptions: BTreeMap<SubscriptionId, Subscription>,
    opening_channels: BTreeSet<ChannelId>,
    opening_subscriptions: BTreeSet<SubscriptionId>,
    /// stationd's measurement channels (protocol 1.1), outside listener capacity.
    measurements: BTreeMap<MeasurementId, Arc<tokio::sync::Mutex<measure::Measurement>>>,
    /// Monitor intervals in which `AD_OVER` grew, since start.
    overloads_total: u64,
    last_ad_over: Option<u64>,
    last_overload: Option<Instant>,
    last_overload_event: Option<Instant>,
    last_probe: Option<Instant>,
}

#[derive(Debug)]
struct Inner {
    config: Ka9qConfig,
    client: OnceCell<Ka9qClient>,
    state: Mutex<State>,
    events: broadcast::Sender<v1::Event>,
    epoch: Instant,
    monitor: Mutex<Option<AbortHandle>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = self
            .monitor
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

/// The ka9q-radio backend; see the [module docs](self). Cloning gives
/// another handle to the same backend.
#[derive(Debug, Clone)]
pub struct Ka9qBackend {
    inner: Arc<Inner>,
}

/// Maps a client error to the result code the hub gets.
fn client_error(e: ClientError) -> BackendError {
    match e {
        ClientError::InvalidParams(m) => BackendError::invalid_argument(m),
        ClientError::NoFreeSsrc => BackendError::resource_exhausted(e.to_string()),
        ClientError::Timeout { .. } => {
            BackendError::backend(format!("radiod is not answering: {e}"))
        }
        other => BackendError::backend(other.to_string()),
    }
}

/// Removes a reserved ID from an `opening` set when dropped.
struct Reservation<'a> {
    backend: &'a Ka9qBackend,
    id: u32,
    channel: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut st = self.backend.state();
        if self.channel {
            st.opening_channels.remove(&self.id);
        } else {
            st.opening_subscriptions.remove(&self.id);
        }
    }
}

impl Ka9qBackend {
    /// A backend for radiod at `config`. Must be called inside a Tokio
    /// runtime: it starts the monitor task and, when `data_group` is set,
    /// binds the client's sockets (an error here means they could not be
    /// bound, for example a missing loopback MULTICAST flag).
    pub fn new(config: Ka9qConfig) -> Result<Self, BackendError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| BackendError::backend("the ka9q backend needs a Tokio runtime"))?;
        let probe_data = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        config
            .client_config(config.data_group.unwrap_or(probe_data))
            .validate()
            .map_err(client_error)?;
        let client = OnceCell::new();
        if let Some(data) = config.data_group {
            let c = Ka9qClient::connect(config.client_config(data)).map_err(client_error)?;
            // A fresh cell cannot be full.
            let _ = client.set(c);
        }
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let inner = Arc::new(Inner {
            config,
            client,
            state: Mutex::new(State::default()),
            events,
            epoch: Instant::now(),
            monitor: Mutex::new(None),
        });
        let task = runtime.spawn(monitor(Arc::downgrade(&inner)));
        *inner.monitor.lock().unwrap_or_else(PoisonError::into_inner) = Some(task.abort_handle());
        Ok(Self { inner })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: v1::Event) {
        // No subscriber is fine.
        let _ = self.inner.events.send(event);
    }

    /// The radiod client, connecting (and discovering the data group) first
    /// if needed.
    pub async fn client(&self) -> Result<Ka9qClient, BackendError> {
        self.inner
            .client
            .get_or_try_init(|| self.connect_client())
            .await
            .cloned()
    }

    async fn connect_client(&self) -> Result<Ka9qClient, BackendError> {
        let config = &self.inner.config;
        let data = match config.data_group {
            Some(data) => data,
            None => self.discover_data_group().await?,
        };
        tracing::info!(%data, status = %config.status_group, "connecting to radiod");
        Ka9qClient::connect(config.client_config(data)).map_err(client_error)
    }

    /// Asks radiod where it sends RTP, with a temporary client whose data
    /// socket is an unused local port.
    async fn discover_data_group(&self) -> Result<SocketAddr, BackendError> {
        let config = &self.inner.config;
        let probe = Ka9qClient::connect(ClientConfig {
            status_addr: config.status_group,
            data_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            channel_status_addr: None,
            ..config.client.clone()
        })
        .map_err(client_error)?;
        let st = probe.probe().await.map_err(client_error)?;
        st.output_data_dest_socket.ok_or_else(|| {
            BackendError::backend(
                "radiod did not report its data group; set [backend.ka9q] data_group",
            )
        })
    }

    /// A radiod status with front-end fields, at most [`FRESH_STATUS`] old.
    async fn frontend_status(
        &self,
        client: &Ka9qClient,
    ) -> Result<Arc<ChannelStatus>, BackendError> {
        if let Some((at, st)) = client.last_status() {
            if at.elapsed() <= FRESH_STATUS && st.input_samprate.is_some() {
                return Ok(st);
            }
        }
        self.state().last_probe = Some(Instant::now());
        client.probe().await.map_err(client_error)
    }

    async fn tunable_range(&self, client: &Ka9qClient) -> Result<v1::FrequencyRange, BackendError> {
        let st = self.frontend_status(client).await?;
        map::tunable_range(&st)
            .ok_or_else(|| BackendError::backend("radiod's status has no front-end range"))
    }

    fn reserve(&self, id: u32, channel: bool) -> Result<Reservation<'_>, BackendError> {
        let mut st = self.state();
        let st = &mut *st;
        let (open, opening, max, what) = if channel {
            (
                st.channels.contains_key(&id),
                &mut st.opening_channels,
                st.channels.len(),
                "channel",
            )
        } else {
            (
                st.subscriptions.contains_key(&id),
                &mut st.opening_subscriptions,
                st.subscriptions.len(),
                "subscription",
            )
        };
        if open || opening.contains(&id) {
            return Err(BackendError::already_exists(format!(
                "{what} {id} is already open"
            )));
        }
        let cap = if channel {
            map::MAX_CHANNELS
        } else {
            map::MAX_SUBSCRIPTIONS
        };
        if max + opening.len() >= usize::try_from(cap).unwrap_or(usize::MAX) {
            return Err(BackendError::resource_exhausted(format!(
                "all {cap} {what}s are in use"
            )));
        }
        opening.insert(id);
        Ok(Reservation {
            backend: self,
            id,
            channel,
        })
    }

    /// The monitor's periodic work (see the module docs).
    async fn tick(&self) {
        let now = Instant::now();
        let Some(client) = self.inner.client.get().cloned() else {
            let due = self
                .state()
                .last_probe
                .is_none_or(|t| now.duration_since(t) >= IDLE_PROBE_INTERVAL);
            if due {
                self.state().last_probe = Some(now);
                if let Err(e) = self.client().await {
                    tracing::warn!("radiod is not reachable: {}", e.message);
                }
            }
            return;
        };
        let last = client.last_status();
        let idle = last
            .as_ref()
            .is_none_or(|(at, _)| now.duration_since(*at) >= IDLE_PROBE_INTERVAL);
        let probe_due = self
            .state()
            .last_probe
            .is_none_or(|t| now.duration_since(t) >= IDLE_PROBE_INTERVAL);
        if idle && probe_due {
            self.state().last_probe = Some(now);
            if let Err(e) = client.probe().await {
                tracing::warn!("radiod did not answer a health probe: {e}");
            }
        }
        if let Some(ad_over) = last.and_then(|(_, st)| st.ad_over) {
            self.note_ad_over(ad_over, now);
        }
    }

    fn note_ad_over(&self, ad_over: u64, now: Instant) {
        let send = {
            let mut st = self.state();
            let grew = st.last_ad_over.is_some_and(|prev| ad_over > prev);
            st.last_ad_over = Some(ad_over);
            if !grew {
                return;
            }
            st.overloads_total += 1;
            st.last_overload = Some(now);
            let send = st
                .last_overload_event
                .is_none_or(|t| now.duration_since(t) >= OVERLOAD_EVENT_INTERVAL);
            if send {
                st.last_overload_event = Some(now);
            }
            send
        };
        if send {
            self.emit(v1::Event {
                kind: v1::event::Kind::FrontendOverload as i32,
                channel_id: None,
                subscription_id: None,
                detail: "radiod reports A/D overrange".to_owned(),
                ..Default::default()
            });
        }
    }

    /// Closes the channels and subscriptions outside `range` after a
    /// retune, with `CHANNEL_CLOSED`/`SUBSCRIPTION_CLOSED` events.
    async fn close_outside(&self, range: &v1::FrequencyRange) {
        let (channels, subscriptions) = {
            let st = self.state();
            let channels: Vec<u32> = st
                .channels
                .iter()
                .filter(|(_, c)| !map::contains(range, c.frequency_hz))
                .map(|(id, _)| *id)
                .collect();
            let subscriptions: Vec<u32> = st
                .subscriptions
                .iter()
                .filter(|(_, s)| {
                    !(map::contains(range, s.range.start_hz)
                        && map::contains(range, s.range.end_hz))
                })
                .map(|(id, _)| *id)
                .collect();
            (channels, subscriptions)
        };
        for id in channels {
            if self.close_channel(id).await.is_ok() {
                self.emit(v1::Event {
                    kind: v1::event::Kind::ChannelClosed as i32,
                    channel_id: Some(id),
                    subscription_id: None,
                    detail: "outside the new passband".to_owned(),
                    ..Default::default()
                });
            }
        }
        for id in subscriptions {
            if self.unsubscribe_waterfall(id).await.is_ok() {
                self.emit(v1::Event {
                    kind: v1::event::Kind::SubscriptionClosed as i32,
                    channel_id: None,
                    subscription_id: Some(id),
                    detail: "outside the new passband".to_owned(),
                    ..Default::default()
                });
            }
        }
    }

    /// The IDs of the open channels, ascending (tests and diagnostics).
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.state().channels.keys().copied().collect()
    }

    /// The IDs of the open subscriptions, ascending.
    pub fn subscription_ids(&self) -> Vec<SubscriptionId> {
        self.state().subscriptions.keys().copied().collect()
    }

    /// The SSRC radiod uses for channel `id`, if it is open.
    pub async fn channel_ssrc(&self, id: ChannelId) -> Option<u32> {
        let handle = Arc::clone(&self.state().channels.get(&id)?.handle);
        let guard = handle.lock().await;
        guard.as_ref().map(AudioChannel::ssrc)
    }

    async fn open(
        &self,
        req: &v1::OpenChannel,
        client: &Ka9qClient,
    ) -> Result<(v1::ChannelOpened, AudioChannel), BackendError> {
        let preset = map::preset_for(req.mode)?;
        let audio = map::check_audio(req.audio)?;
        let (low, high) = map::open_edges(req.low_cut_hz, req.high_cut_hz)?;
        let squelch = map::open_squelch(req.squelch_snr_db)?;
        let range = self.tunable_range(client).await?;
        if req.frequency_hz == 0 || !map::contains(&range, req.frequency_hz) {
            return Err(BackendError::out_of_range(
                "frequency outside the tunable range",
            ));
        }
        let params = AudioChannelParams {
            frequency_hz: map::freq(req.frequency_hz),
            preset: Some(preset.to_owned()),
            low_edge_hz: low,
            high_edge_hz: high,
            samprate: Some(audio.sample_rate_hz),
            opus_bitrate: Some(audio.bitrate_bps),
            output_channels: Some(1),
            squelch: Some(squelch),
        };
        let channel = client
            .open_audio_channel(params)
            .await
            .map_err(client_error)?;
        match opened_from(req, audio, &channel) {
            Ok(opened) => Ok((opened, channel)),
            Err(e) => {
                if let Err(d) = channel.close().await {
                    tracing::debug!("closing an unusable channel: {d}");
                }
                Err(e)
            }
        }
    }

    async fn subscribe(
        &self,
        req: &v1::SubscribeWaterfall,
        client: &Ka9qClient,
    ) -> Result<(v1::WaterfallSubscribed, SpectrumChannel), BackendError> {
        let range = req
            .range
            .ok_or_else(|| BackendError::invalid_argument("range is missing"))?;
        let fe = self.frontend_status(client).await?;
        let tunable = map::tunable_range(&fe)
            .ok_or_else(|| BackendError::backend("radiod's status has no front-end range"))?;
        if !map::contains(&tunable, range.start_hz) || !map::contains(&tunable, range.end_hz) {
            return Err(BackendError::out_of_range(
                "range outside the tunable range",
            ));
        }
        let g = map::geometry(
            &range,
            req.bins,
            placement::FrontEnd::from_status(&fe).as_ref(),
        )?;
        let row_interval_ms = req.row_interval_ms.max(map::MIN_ROW_INTERVAL_MS);
        let params = SpectrumParams {
            center_hz: g.center_hz,
            bin_count: g.bins,
            resolution_bw_hz: g.rbw_hz,
            avg: None,
            row_interval: Duration::from_millis(u64::from(row_interval_ms)),
        };
        let channel = client
            .open_spectrum_channel(params)
            .await
            .map_err(client_error)?;
        // Where radiod put the bins, from its reply; else where it should have.
        let covered = channel
            .status()
            .and_then(|st| map::covered_range(&st, g.bins));
        let subscribed = v1::WaterfallSubscribed {
            subscription_id: req.subscription_id,
            range: Some(covered.unwrap_or(g.covered)),
            bins: g.bins,
            row_interval_ms,
            quantization: Some(map::quantization(req.quantization)?),
            compression: req.compression,
        };
        Ok((subscribed, channel))
    }

    fn check_subscription(&self, req: &v1::SubscribeWaterfall) -> Result<(), BackendError> {
        match v1::WaterfallCompression::try_from(req.compression) {
            Ok(v1::WaterfallCompression::None | v1::WaterfallCompression::Zstd) => {}
            Ok(v1::WaterfallCompression::Unspecified) => {
                return Err(BackendError::invalid_argument("compression is unspecified"));
            }
            Err(_) => return Err(BackendError::unsupported("compression not supported")),
        }
        let range = req
            .range
            .ok_or_else(|| BackendError::invalid_argument("range is missing"))?;
        if range.start_hz >= range.end_hz {
            return Err(BackendError::invalid_argument("range is empty or reversed"));
        }
        if !(station_proto::limits::MIN_BINS..=map::MAX_BINS).contains(&req.bins) {
            return Err(BackendError::out_of_range(format!(
                "bins must be {}..={}",
                station_proto::limits::MIN_BINS,
                map::MAX_BINS
            )));
        }
        let max = self.inner.config.max_row_interval_ms();
        if req.row_interval_ms > max {
            return Err(BackendError::out_of_range(format!(
                "row_interval_ms must be at most {max}"
            )));
        }
        map::quantization(req.quantization).map(drop)
    }
}

/// `ChannelOpened` from what radiod reports for a new channel.
fn opened_from(
    req: &v1::OpenChannel,
    audio: v1::AudioParams,
    channel: &AudioChannel,
) -> Result<v1::ChannelOpened, BackendError> {
    let st = channel.status();
    let params = channel.params();
    let frame_duration_ms = match channel.frame_duration() {
        None => audio.frame_duration_ms,
        Some(frame) => map::frame_duration_ms(frame).ok_or_else(|| {
            BackendError::backend(format!(
                "radiod's block time of {frame:?} is not an Opus frame duration"
            ))
        })?,
    };
    let bitrate_bps = st
        .as_ref()
        .and_then(|s| s.opus_bit_rate)
        .and_then(|b| u32::try_from(b).ok())
        .filter(|b| station_proto::limits::AUDIO_BITRATE_BPS.contains(b))
        .unwrap_or(audio.bitrate_bps);
    let sample_rate_hz = map::actual_sample_rate(
        params.preset.as_deref().unwrap_or_default(),
        audio.sample_rate_hz,
        params.samprate,
    );
    let edge = |v: Option<f64>, fallback: i32| v.and_then(map::edge_hz).unwrap_or(fallback);
    let mut low = edge(st.as_ref().and_then(|s| s.low_edge_hz), req.low_cut_hz);
    let mut high = edge(st.as_ref().and_then(|s| s.high_edge_hz), req.high_cut_hz);
    if low > high {
        (low, high) = (req.low_cut_hz, req.high_cut_hz);
    }
    Ok(v1::ChannelOpened {
        channel_id: req.channel_id,
        frequency_hz: st
            .as_ref()
            .and_then(|s| s.radio_frequency_hz)
            .and_then(map::hz)
            .unwrap_or(req.frequency_hz),
        mode: req.mode,
        low_cut_hz: low,
        high_cut_hz: high,
        audio: Some(v1::AudioParams {
            codec: v1::AudioCodec::Opus as i32,
            bitrate_bps,
            sample_rate_hz,
            frame_duration_ms,
        }),
    })
}

/// The monitor task: runs [`Ka9qBackend::tick`] every [`MONITOR_INTERVAL`]
/// until the backend is dropped.
async fn monitor(inner: Weak<Inner>) {
    let mut ticker = tokio::time::interval(MONITOR_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(inner) = inner.upgrade() else {
            return;
        };
        Ka9qBackend { inner }.tick().await;
    }
}

#[async_trait::async_trait]
impl Backend for Ka9qBackend {
    fn info(&self) -> v1::BackendInfo {
        v1::BackendInfo {
            kind: v1::BackendKind::Ka9qRadio as i32,
            version: format!("ka9q-radio {}", &tags::UPSTREAM_COMMIT[..12]),
        }
    }

    async fn capabilities(&self) -> Result<v1::Capabilities, BackendError> {
        let client = self.client().await?;
        let st = self.frontend_status(&client).await?;
        map::capabilities(&st)
            .ok_or_else(|| BackendError::backend("radiod's status has no front-end information"))
    }

    async fn open_channel(
        &self,
        req: v1::OpenChannel,
    ) -> Result<(v1::ChannelOpened, AudioStream), BackendError> {
        let _reservation = self.reserve(req.channel_id, true)?;
        let client = self.client().await?;
        let (opened, channel) = self.open(&req, &client).await?;
        let stop = CancellationToken::new();
        let squelch_db = req.squelch_snr_db;
        let squelch: SharedSquelch = Arc::new(Mutex::new(squelch_db));
        let stream = media::audio_stream(
            req.channel_id,
            channel.receiver(),
            StatusSource {
                client,
                ssrc: channel.ssrc(),
            },
            Arc::clone(&squelch),
            stop.clone(),
        );
        tracing::info!(
            channel_id = req.channel_id,
            ssrc = channel.ssrc(),
            frequency_hz = opened.frequency_hz,
            "channel opened"
        );
        self.state().channels.insert(
            req.channel_id,
            Channel {
                handle: Arc::new(tokio::sync::Mutex::new(Some(channel))),
                frequency_hz: opened.frequency_hz,
                mode: req.mode,
                squelch,
                stop,
            },
        );
        Ok((opened, stream))
    }

    async fn update_channel(
        &self,
        req: v1::UpdateChannel,
    ) -> Result<v1::ChannelUpdated, BackendError> {
        let not_found =
            || BackendError::not_found(format!("channel {} is not open", req.channel_id));
        let (handle, mode) = {
            let st = self.state();
            let ch = st.channels.get(&req.channel_id).ok_or_else(not_found)?;
            (Arc::clone(&ch.handle), ch.mode)
        };
        let preset = req.mode.map(map::preset_for).transpose()?;
        let squelch = map::update_squelch(req.squelch_snr_db, req.squelch_off)?;
        let client = self.client().await?;
        if let Some(f) = req.frequency_hz {
            let range = self.tunable_range(&client).await?;
            if f == 0 || !map::contains(&range, f) {
                return Err(BackendError::out_of_range(
                    "frequency outside the tunable range",
                ));
            }
        }
        let update = AudioUpdate {
            preset: preset.map(str::to_owned),
            frequency_hz: req.frequency_hz.map(map::freq),
            low_edge_hz: req.low_cut_hz.map(map::edge),
            high_edge_hz: req.high_cut_hz.map(map::edge),
            opus_bitrate: None,
            squelch,
        };
        let guard = handle.lock().await;
        let channel = guard.as_ref().ok_or_else(not_found)?;
        let status = channel.update(update).await.map_err(client_error)?;
        let params = channel.params();
        drop(guard);
        let frequency_hz = status
            .radio_frequency_hz
            .and_then(map::hz)
            .or_else(|| map::hz(params.frequency_hz))
            .unwrap_or(0);
        let mode = req.mode.unwrap_or(mode);
        if let Some(ch) = self.state().channels.get_mut(&req.channel_id) {
            ch.frequency_hz = frequency_hz;
            ch.mode = mode;
            if let Some(sq) = squelch {
                *ch.squelch.lock().unwrap_or_else(PoisonError::into_inner) = match sq {
                    crate::client::Squelch::Off => None,
                    crate::client::Squelch::Snr { open_db, .. } => Some(open_db),
                };
            }
        }
        let edge = |v: Option<f64>, p: Option<f32>| {
            v.and_then(map::edge_hz)
                .or_else(|| p.and_then(|x| map::edge_hz(f64::from(x))))
                .unwrap_or(0)
        };
        Ok(v1::ChannelUpdated {
            channel_id: req.channel_id,
            frequency_hz,
            mode,
            low_cut_hz: edge(status.low_edge_hz, params.low_edge_hz),
            high_cut_hz: edge(status.high_edge_hz, params.high_edge_hz),
        })
    }

    async fn close_channel(&self, id: ChannelId) -> Result<(), BackendError> {
        let ch = self
            .state()
            .channels
            .remove(&id)
            .ok_or_else(|| BackendError::not_found(format!("channel {id} is not open")))?;
        ch.stop.cancel();
        let channel = ch.handle.lock().await.take();
        if let Some(channel) = channel {
            // If radiod does not answer, the channel expires on its own.
            if let Err(e) = channel.close().await {
                tracing::warn!(channel_id = id, "closing channel: {e}");
            }
        }
        Ok(())
    }

    async fn set_frontend(&self, req: v1::SetFrontend) -> Result<v1::FrontendState, BackendError> {
        if req.gain_db.is_some()
            || req.hardware_agc.is_some()
            || req.attenuation_db.is_some()
            || req.antenna_port.is_some()
        {
            return Err(BackendError::unsupported(
                "radiod offers no runtime gain, AGC, attenuator or antenna control for this front end",
            ));
        }
        let client = self.client().await?;
        let Some(center) = req.center_frequency_hz else {
            let st = self.frontend_status(&client).await?;
            return Ok(map::frontend_state(&st));
        };
        if center == 0 || center > station_proto::limits::MAX_FREQUENCY_HZ {
            return Err(BackendError::out_of_range("center frequency out of range"));
        }
        let want = map::freq(center);
        let st = client
            .probe_with(|c| c.first_lo_hz(want))
            .await
            .map_err(client_error)?;
        let applied = st
            .first_lo_hz
            .is_some_and(|lo| (lo - want).abs() <= RETUNE_TOLERANCE_HZ);
        if !applied {
            return Err(BackendError::unsupported(
                "radiod did not retune the front end (fixed or locked tuner)",
            ));
        }
        let state = map::frontend_state(&st);
        if let Some(range) = state.tunable_range {
            self.close_outside(&range).await;
        }
        self.emit(v1::Event {
            kind: v1::event::Kind::FrontendChanged as i32,
            channel_id: None,
            subscription_id: None,
            detail: String::new(),
            ..Default::default()
        });
        Ok(state)
    }

    async fn subscribe_waterfall(
        &self,
        req: v1::SubscribeWaterfall,
    ) -> Result<(v1::WaterfallSubscribed, RowStream), BackendError> {
        self.check_subscription(&req)?;
        let _reservation = self.reserve(req.subscription_id, false)?;
        let client = self.client().await?;
        let (subscribed, channel) = self.subscribe(&req, &client).await?;
        let stop = CancellationToken::new();
        let stream = media::row_stream(
            channel.receiver(),
            RowEncoder::new(&subscribed, self.inner.epoch),
            stop.clone(),
        );
        tracing::info!(
            subscription_id = req.subscription_id,
            ssrc = channel.ssrc(),
            bins = subscribed.bins,
            "waterfall subscribed"
        );
        self.state().subscriptions.insert(
            req.subscription_id,
            Subscription {
                handle: Arc::new(tokio::sync::Mutex::new(Some(channel))),
                range: subscribed.range.unwrap_or_default(),
                stop,
            },
        );
        Ok((subscribed, stream))
    }

    async fn unsubscribe_waterfall(&self, id: SubscriptionId) -> Result<(), BackendError> {
        let sub = self
            .state()
            .subscriptions
            .remove(&id)
            .ok_or_else(|| BackendError::not_found(format!("subscription {id} is not open")))?;
        sub.stop.cancel();
        let channel = sub.handle.lock().await.take();
        if let Some(channel) = channel {
            if let Err(e) = channel.close().await {
                tracing::warn!(subscription_id = id, "closing spectrum channel: {e}");
            }
        }
        Ok(())
    }

    fn health(&self) -> Health {
        let version = self.info().version;
        let down = Health {
            backend: v1::BackendHealth {
                state: v1::backend_health::State::Down as i32,
                version: version.clone(),
            },
            frontend: None,
        };
        let Some(client) = self.inner.client.get() else {
            return down;
        };
        let Some((at, st)) = client.last_status() else {
            return down;
        };
        if client.last_command_timeout().is_some_and(|t| t > at) {
            return down;
        }
        let now = Instant::now();
        let state = if now.duration_since(at) > STALE_AFTER {
            v1::backend_health::State::Degraded
        } else {
            v1::backend_health::State::Running
        };
        let (overloads_total, last_overload) = {
            let s = self.state();
            (s.overloads_total, s.last_overload)
        };
        let overloaded = last_overload.is_some_and(|t| now.duration_since(t) <= OVERLOAD_HOLD);
        Health {
            backend: v1::BackendHealth {
                state: state as i32,
                version,
            },
            frontend: Some(frontend_health(&st, overloads_total, overloaded)),
        }
    }

    fn events(&self) -> EventStream {
        // A lagging subscriber skips what it missed.
        Box::pin(
            BroadcastStream::new(self.inner.events.subscribe()).filter_map(|e| async { e.ok() }),
        )
    }

    fn can_measure(&self) -> bool {
        true
    }

    async fn open_measurement(
        &self,
        id: MeasurementId,
        channel: MeasurementChannel,
    ) -> Result<(), BackendError> {
        if self.state().measurements.contains_key(&id) {
            return Err(BackendError::already_exists(format!(
                "measurement {id} is already open"
            )));
        }
        let client = self.client().await?;
        let m = measure::Measurement::open(&client, channel).await?;
        let previous = self
            .state()
            .measurements
            .insert(id, Arc::new(tokio::sync::Mutex::new(m)));
        if let Some(old) = previous {
            // Two opens raced: keep the newer, close the other.
            let _ = old.lock().await.close().await;
        }
        Ok(())
    }

    async fn measurement_grid(&self, resolution_bw_hz: f32) -> Option<MeasurementGrid> {
        let client = self.client().await.ok()?;
        let st = self.frontend_status(&client).await.ok()?;
        measure::grid_of(&st, resolution_bw_hz)
    }

    async fn retune_measurement(
        &self,
        id: MeasurementId,
        center_hz: u64,
    ) -> Result<Option<Spectrum>, BackendError> {
        let m = self.measurement(id)?;
        let mut m = m.lock().await;
        m.retune(center_hz).await
    }

    async fn poll_measurement(&self, id: MeasurementId) -> Result<Spectrum, BackendError> {
        let m = self.measurement(id)?;
        let mut m = m.lock().await;
        m.poll().await
    }

    async fn close_measurement(&self, id: MeasurementId) -> Result<(), BackendError> {
        let m = self
            .state()
            .measurements
            .remove(&id)
            .ok_or_else(|| BackendError::not_found(format!("measurement {id} is not open")))?;
        let mut m = m.lock().await;
        m.close().await
    }
}

impl Ka9qBackend {
    fn measurement(
        &self,
        id: MeasurementId,
    ) -> Result<Arc<tokio::sync::Mutex<measure::Measurement>>, BackendError> {
        self.state()
            .measurements
            .get(&id)
            .cloned()
            .ok_or_else(|| BackendError::not_found(format!("measurement {id} is not open")))
    }

    /// The IDs of the open measurement channels, ascending (tests).
    pub fn measurement_ids(&self) -> Vec<MeasurementId> {
        self.state().measurements.keys().copied().collect()
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "dB values are small; f32 is the protocol type"
)]
fn frontend_health(
    st: &ChannelStatus,
    overloads_total: u64,
    overloaded: bool,
) -> v1::FrontendHealth {
    let state = map::frontend_state(st);
    v1::FrontendHealth {
        state: if overloaded {
            v1::frontend_health::State::Overload
        } else {
            v1::frontend_health::State::Ok
        } as i32,
        sample_rate_hz: st.input_samprate.unwrap_or(0),
        center_frequency_hz: state.center_frequency_hz,
        gain_db: state.gain_db,
        overloads_total,
        input_power_dbfs: st
            .if_power_db
            .filter(|p| p.is_finite())
            .map(|p| p.clamp(-400.0, 400.0) as f32),
    }
}
