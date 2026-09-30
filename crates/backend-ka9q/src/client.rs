// SPDX-License-Identifier: GPL-3.0-only
//! Async radiod client: creates, refreshes and tears down channels over the
//! TLV control protocol, receives their Opus RTP and polls spectrum rows.
//!
//! The client follows the contract in `docs/radiod-notes.md`:
//!
//! - **Every command carries `LIFETIME`** (§4, §7). radiod creates a channel
//!   for any command to an unknown SSRC, even a poll or a delete, and a
//!   channel created without `LIFETIME` never expires. The one exception is
//!   the all-channels status request ([`Ka9qClient::dump_all`]), which radiod
//!   does not forward to any channel.
//! - **At most one command in flight per SSRC.** radiod queues at most two
//!   commands per channel and silently drops the rest (§4). Every command
//!   takes the channel's command lock, is sent with a fresh `COMMAND_TAG`,
//!   and the lock is held until the status reply with that tag arrives or
//!   [`ClientConfig::command_timeout`] passes.
//! - **Retries.** A command that gets no reply is re-sent up to
//!   [`ClientConfig::command_retries`] times, each time with a new tag and
//!   still one at a time. Every command the client sends sets absolute
//!   values, so a duplicate (the first copy was applied but its reply was
//!   lost) is harmless. After the last attempt the call fails with
//!   [`ClientError::Timeout`].
//! - **Channel teardown** sends `LIFETIME` 1 ([`Command::destroy`]):
//!   `RADIO_FREQUENCY` 0 alone does not delete a channel (§7).
//! - **Keepalive.** An audio channel is refreshed with a poll (which
//!   carries `LIFETIME`) every third of its lifetime. The frame duration
//!   comes from the channel's status (`FILTER_BLOCKSIZE` /
//!   `INPUT_SAMPRATE`, 20 ms on every radiod config seen so far); until a
//!   status has been seen, [`ClientConfig::frame_duration`] is assumed. A
//!   spectrum channel's row polls refresh it, so it needs no separate
//!   keepalive.
//! - **Self-healing.** If a refresh or poll reply shows that radiod no
//!   longer has the channel as configured (it expired, radiod restarted,
//!   and the refresh re-created it from the template), the client re-sends
//!   the full create command on the same SSRC.
//! - **Spectrum geometry.** Changing `BIN_COUNT` on a live spectrum channel
//!   crashes radiod (§6), so [`SpectrumChannel::update`] with a new bin count
//!   creates a new channel under a new SSRC and destroys the old one.
//!
//! # Tasks and queues
//!
//! [`Ka9qClient::connect`] starts two background tasks:
//!
//! - The **status task** decodes `STATUS` packets on the control socket.
//!   Command packets (the client's own, looped back by multicast, or other
//!   clients'), undecodable packets and packets without an SSRC are counted
//!   and ignored. A reply whose (`OUTPUT_SSRC`, `COMMAND_TAG`) matches a
//!   waiting command completes it; any status of an owned SSRC becomes that
//!   channel's latest status.
//! - The **RTP task** parses RTP packets on the data socket and routes them
//!   by SSRC to the owning [`AudioChannel`]. A per-channel
//!   [`SequenceTracker`] turns the 16-bit RTP sequence into [`AudioPacket`]s
//!   with a 32-bit `seq` and a `lost_before` gap count; duplicates and late
//!   packets are dropped and counted.
//!
//! Each channel has a bounded queue. **On overflow the oldest item is
//! dropped** and counted ([`AudioStats::queue_drops`],
//! [`SpectrumStats::queue_drops`]): for a live stream, a consumer that falls
//! behind should resume with fresh data rather than stale data. A dropped
//! audio packet shows up to the consumer as a jump in [`AudioPacket::seq`]
//! (it is not included in `lost_before`, which counts network losses only).
//!
//! The background tasks stop when the [`Ka9qClient`] and every channel
//! opened from it have been dropped.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::{Notify, broadcast, oneshot};
use tokio::task::AbortHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::command::{Command, SSRC_ALL_CHANNELS};
use crate::net::{self, NetError, SendOptions};
use crate::rtp::{RtpPacket, SequenceTracker};
use crate::spectrum::SpectrumFrame;
use crate::status::ChannelStatus;
use crate::tags::{DemodType, Encoding, StatusType};
use crate::tlv;

/// radiod's control/status group in the test config
/// (radiod-notes.md §2).
pub const DEFAULT_STATUS_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 77, 0, 6), 5006));
/// radiod's RTP data group in the test config.
pub const DEFAULT_DATA_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 77, 0, 4), 5004));

/// radiod's per-channel status copy in the test config: the
/// data group on port 5006 (radiod-notes.md §2).
pub const DEFAULT_CHANNEL_STATUS_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 77, 0, 4), 5006));

/// Largest bin count accepted for a spectrum channel (station-protocol's
/// `max_bins` cap).
pub const MAX_BIN_COUNT: u32 = 16_384;

/// Lifetime, in frames, of the temporary channel [`Ka9qClient::probe`]
/// creates (1 s at 20 ms frames), in case its explicit destroy is lost.
pub const PROBE_LIFETIME_FRAMES: u32 = 50;

/// A reply frequency within this many Hz of the requested one confirms it.
const FREQ_TOLERANCE_HZ: f64 = 1.0;
/// Receive buffer for one datagram (the UDP maximum).
const MAX_DATAGRAM: usize = 65_536;
/// Capacity of the all-status broadcast used by [`Ka9qClient::dump_all`].
const STATUS_BROADCAST_CAPACITY: usize = 256;
/// Foreign SSRCs remembered to avoid allocating one another client uses.
const MAX_FOREIGN_SSRCS: usize = 4096;
/// Attempts at picking a random unused SSRC.
const SSRC_ALLOCATION_ATTEMPTS: usize = 64;
/// Pause after a socket receive error, so a persistent error cannot spin.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(10);
/// Sane bounds for a frame duration derived from status or configured.
const MIN_FRAME: Duration = Duration::from_millis(1);
const MAX_FRAME: Duration = Duration::from_secs(1);

/// Errors from the client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The [`ClientConfig`] is unusable.
    #[error("invalid client configuration: {0}")]
    InvalidConfig(&'static str),
    /// Channel parameters are out of range.
    #[error("invalid channel parameters: {0}")]
    InvalidParams(String),
    /// Socket setup failed.
    #[error(transparent)]
    Net(#[from] NetError),
    /// A socket operation failed after setup.
    #[error("{op}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// radiod did not answer a command, including retries.
    #[error("no status reply from radiod for SSRC {ssrc} after {attempts} attempt(s)")]
    Timeout {
        /// The channel.
        ssrc: u32,
        /// Commands sent, each waiting [`ClientConfig::command_timeout`].
        attempts: u32,
    },
    /// radiod answered, but its status shows the command was not applied.
    #[error("radiod did not apply the command for SSRC {ssrc}: {reason}")]
    Rejected {
        /// The channel.
        ssrc: u32,
        /// What differs from the request.
        reason: String,
    },
    /// No unused SSRC was found (practically impossible with 2^32 values).
    #[error("could not allocate an unused SSRC")]
    NoFreeSsrc,
    /// The client's background tasks have stopped.
    #[error("the radiod client has shut down")]
    Closed,
}

/// Client configuration. [`Default`] matches the test setup
/// (radiod-notes.md §2).
#[derive(Debug, Clone, PartialEq)]
pub struct ClientConfig {
    /// Where commands are sent and status replies received: radiod's
    /// `status =` group and port 5006. A unicast address selects the
    /// hermetic test mode (see [`crate::net`]).
    pub status_addr: SocketAddr,
    /// Where RTP arrives: radiod's `data =` group and port 5004, or a
    /// unicast address (port 0 picks a free one) in tests.
    pub data_addr: SocketAddr,
    /// Where radiod multicasts each audio channel's **unsolicited** status
    /// (every `STATUS_INTERVAL` frames, 0.5 s by default) and a copy of
    /// every command reply: the data group on port 5006
    /// (radiod-notes.md §2, §4). `None` (the default) does not listen
    /// there; the client then only sees command replies on
    /// [`ClientConfig::status_addr`]. A unicast address (port 0 picks a
    /// free one) is the hermetic test mode.
    pub channel_status_addr: Option<SocketAddr>,
    /// Local interface for group membership and multicast sends.
    pub interface: Ipv4Addr,
    /// Multicast TTL for commands; 0 keeps them on this host.
    pub ttl: u32,
    /// How long to wait for the status reply to one command.
    pub command_timeout: Duration,
    /// Extra attempts after a command timed out.
    pub command_retries: u32,
    /// `LIFETIME` sent with every command, in frames. Must be at least 2
    /// (0 means "never expire" and 1 means "destroy" to radiod).
    pub lifetime_frames: u32,
    /// Frame duration assumed until a channel's status reports it.
    pub frame_duration: Duration,
    /// Per-audio-channel queue length, in packets (50 per second).
    pub audio_queue_len: usize,
    /// Per-spectrum-channel queue length, in rows.
    pub spectrum_queue_len: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            status_addr: DEFAULT_STATUS_ADDR,
            data_addr: DEFAULT_DATA_ADDR,
            channel_status_addr: None,
            interface: Ipv4Addr::LOCALHOST,
            ttl: 0,
            command_timeout: Duration::from_millis(250),
            command_retries: 2,
            lifetime_frames: 500,
            frame_duration: Duration::from_millis(20),
            audio_queue_len: 250,
            spectrum_queue_len: 64,
        }
    }
}

impl ClientConfig {
    /// Checks the configuration.
    pub fn validate(&self) -> Result<(), ClientError> {
        if self.command_timeout.is_zero() {
            return Err(ClientError::InvalidConfig("command_timeout must be > 0"));
        }
        if self.lifetime_frames < 2 {
            return Err(ClientError::InvalidConfig("lifetime_frames must be >= 2"));
        }
        if !(MIN_FRAME..=MAX_FRAME).contains(&self.frame_duration) {
            return Err(ClientError::InvalidConfig(
                "frame_duration must be between 1 ms and 1 s",
            ));
        }
        if self.audio_queue_len == 0 || self.spectrum_queue_len == 0 {
            return Err(ClientError::InvalidConfig("queue lengths must be >= 1"));
        }
        let budget = self
            .command_timeout
            .saturating_mul(self.command_retries.saturating_add(1))
            .saturating_mul(2);
        if self.lifetime() <= budget {
            return Err(ClientError::InvalidConfig(
                "lifetime must exceed twice the full retry budget of one command",
            ));
        }
        Ok(())
    }

    /// The channel lifetime at the assumed frame duration.
    pub fn lifetime(&self) -> Duration {
        self.frame_duration.saturating_mul(self.lifetime_frames)
    }

    fn send_options(&self) -> SendOptions {
        SendOptions {
            interface: self.interface,
            ttl: self.ttl,
        }
    }
}

/// Counters of the client's background tasks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientStats {
    /// Commands sent (including retries).
    pub commands_sent: u64,
    /// Commands that got no reply within the timeout.
    pub command_timeouts: u64,
    /// Well-formed `STATUS` packets received.
    pub status_packets: u64,
    /// `STATUS` packets for SSRCs this client does not own.
    pub foreign_status: u64,
    /// `CMD` packets seen on the control socket (multicast loopback).
    pub command_packets_seen: u64,
    /// Undecodable packets on the control socket.
    pub garbage_status: u64,
    /// RTP packets delivered to an owned channel's tracker.
    pub rtp_packets: u64,
    /// RTP packets for SSRCs this client does not own.
    pub rtp_foreign: u64,
    /// Undecodable packets on the data socket.
    pub rtp_garbage: u64,
    /// Socket receive errors (both sockets).
    pub recv_errors: u64,
}

#[derive(Debug, Default)]
struct Counters {
    commands_sent: AtomicU64,
    command_timeouts: AtomicU64,
    status_packets: AtomicU64,
    foreign_status: AtomicU64,
    command_packets_seen: AtomicU64,
    garbage_status: AtomicU64,
    rtp_packets: AtomicU64,
    rtp_foreign: AtomicU64,
    rtp_garbage: AtomicU64,
    recv_errors: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

impl Counters {
    fn snapshot(&self) -> ClientStats {
        ClientStats {
            commands_sent: get(&self.commands_sent),
            command_timeouts: get(&self.command_timeouts),
            status_packets: get(&self.status_packets),
            foreign_status: get(&self.foreign_status),
            command_packets_seen: get(&self.command_packets_seen),
            garbage_status: get(&self.garbage_status),
            rtp_packets: get(&self.rtp_packets),
            rtp_foreign: get(&self.rtp_foreign),
            rtp_garbage: get(&self.rtp_garbage),
            recv_errors: get(&self.recv_errors),
        }
    }
}

/// A bounded single-consumer queue that drops the **oldest** item when full.
#[derive(Debug)]
struct DropOldestQueue<T> {
    inner: Mutex<QueueInner<T>>,
    notify: Notify,
    capacity: usize,
}

#[derive(Debug)]
struct QueueInner<T> {
    items: VecDeque<T>,
    closed: bool,
    dropped: u64,
}

impl<T> DropOldestQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(QueueInner {
                items: VecDeque::with_capacity(capacity.min(1024)),
                closed: false,
                dropped: 0,
            }),
            notify: Notify::new(),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueInner<T>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, item: T) {
        {
            let mut q = self.lock();
            if q.closed {
                return;
            }
            if q.items.len() >= self.capacity {
                q.items.pop_front();
                q.dropped += 1;
            }
            q.items.push_back(item);
        }
        self.notify.notify_one();
    }

    fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_one();
    }

    fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    fn try_recv(&self) -> Option<T> {
        self.lock().items.pop_front()
    }

    /// The next item; `None` once the queue is closed and drained.
    /// `notify_one` stores a permit when nobody waits, so a push between the
    /// check and the `notified().await` is not lost.
    async fn recv(&self) -> Option<T> {
        loop {
            {
                let mut q = self.lock();
                if let Some(item) = q.items.pop_front() {
                    return Some(item);
                }
                if q.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }
}

/// One Opus packet of an [`AudioChannel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    /// Station sequence number: +1 per accepted packet, wraps at 2^32.
    pub seq: u32,
    /// RTP timestamp (48 kHz Opus clock) relative to the first packet.
    pub timestamp: u32,
    /// Packets lost on the network just before this one.
    pub lost_before: u32,
    /// RTP payload type (radiod uses 111 for Opus).
    pub payload_type: u8,
    /// The Opus packet.
    pub payload: Vec<u8>,
}

#[derive(Debug)]
struct AudioSink {
    tracker: SequenceTracker,
    queue: Arc<DropOldestQueue<AudioPacket>>,
}

/// Client-side state of one owned SSRC.
#[derive(Debug, Default)]
struct Slot {
    latest: Option<Arc<ChannelStatus>>,
    frame: Option<Duration>,
    audio: Option<AudioSink>,
}

type WaiterKey = (u32, u64);

#[derive(Debug, Default)]
struct State {
    waiters: HashMap<WaiterKey, oneshot::Sender<Arc<ChannelStatus>>>,
    channels: HashMap<u32, Slot>,
    foreign: HashSet<u32>,
    next_tag: u32,
    /// The newest status packet of any SSRC (front-end fields are global
    /// to the radiod instance) and when it arrived.
    last_status: Option<(Instant, Arc<ChannelStatus>)>,
    /// When a command last failed with [`ClientError::Timeout`].
    last_timeout: Option<Instant>,
}

/// State shared by the background tasks and the handles.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    counters: Counters,
    statuses: broadcast::Sender<Arc<ChannelStatus>>,
}

/// Frame duration from a status packet: `FILTER_BLOCKSIZE` input samples
/// per frame at `INPUT_SAMPRATE`. `None` if either is missing or the result
/// is outside 1 ms ..= 1 s.
fn frame_duration_of(bytes: &[u8], status: &ChannelStatus) -> Option<Duration> {
    let packet = tlv::parse(bytes).ok()?;
    let block = packet.last(StatusType::FilterBlocksize)?.as_u32().ok()?;
    let rate = status.input_samprate?;
    if rate == 0 {
        return None;
    }
    let frame = Duration::from_secs_f64(f64::from(block) / f64::from(rate));
    (MIN_FRAME..=MAX_FRAME).contains(&frame).then_some(frame)
}

impl Shared {
    fn new() -> Self {
        let (statuses, _) = broadcast::channel(STATUS_BROADCAST_CAPACITY);
        Self {
            state: Mutex::new(State {
                next_tag: rand::random(),
                ..State::default()
            }),
            counters: Counters::default(),
            statuses,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_tag(&self) -> u32 {
        let mut st = self.lock();
        loop {
            let tag = st.next_tag;
            st.next_tag = tag.wrapping_add(1);
            if tag != 0 {
                return tag;
            }
        }
    }

    /// Reserves a random SSRC that is not 0, not the all-channels SSRC, not
    /// owned and not seen in another client's traffic.
    fn allocate_ssrc(
        &self,
        audio: Option<Arc<DropOldestQueue<AudioPacket>>>,
    ) -> Result<u32, ClientError> {
        let mut st = self.lock();
        for _ in 0..SSRC_ALLOCATION_ATTEMPTS {
            let ssrc: u32 = rand::random();
            if ssrc == 0
                || ssrc == SSRC_ALL_CHANNELS
                || st.channels.contains_key(&ssrc)
                || st.foreign.contains(&ssrc)
            {
                continue;
            }
            let audio = audio.map(|queue| AudioSink {
                tracker: SequenceTracker::new(ssrc),
                queue,
            });
            st.channels.insert(
                ssrc,
                Slot {
                    audio,
                    ..Slot::default()
                },
            );
            return Ok(ssrc);
        }
        Err(ClientError::NoFreeSsrc)
    }

    /// Forgets an SSRC and closes its audio queue.
    fn release(&self, ssrc: u32) {
        let slot = self.lock().channels.remove(&ssrc);
        if let Some(sink) = slot.and_then(|s| s.audio) {
            sink.queue.close();
        }
    }

    fn register_waiter(&self, key: WaiterKey) -> oneshot::Receiver<Arc<ChannelStatus>> {
        let (tx, rx) = oneshot::channel();
        self.lock().waiters.insert(key, tx);
        rx
    }

    fn latest(&self, ssrc: u32) -> Option<Arc<ChannelStatus>> {
        self.lock()
            .channels
            .get(&ssrc)
            .and_then(|s| s.latest.clone())
    }

    fn frame(&self, ssrc: u32) -> Option<Duration> {
        self.lock().channels.get(&ssrc).and_then(|s| s.frame)
    }

    /// Handles one datagram from the control socket. Never panics.
    fn handle_status_packet(&self, bytes: &[u8]) {
        match bytes.first() {
            Some(0) => {}
            Some(1) => {
                bump(&self.counters.command_packets_seen);
                return;
            }
            _ => {
                bump(&self.counters.garbage_status);
                return;
            }
        }
        let Ok(status) = ChannelStatus::decode(bytes) else {
            bump(&self.counters.garbage_status);
            return;
        };
        let Some(ssrc) = status.ssrc else {
            bump(&self.counters.garbage_status);
            return;
        };
        bump(&self.counters.status_packets);
        let frame = frame_duration_of(bytes, &status);
        let status = Arc::new(status);
        // No subscriber is the normal case; that is not an error.
        let _ = self.statuses.send(Arc::clone(&status));

        let mut st = self.lock();
        st.last_status = Some((Instant::now(), Arc::clone(&status)));
        if let Some(slot) = st.channels.get_mut(&ssrc) {
            slot.latest = Some(Arc::clone(&status));
            if frame.is_some() {
                slot.frame = frame;
            }
        } else {
            bump(&self.counters.foreign_status);
            if st.foreign.len() >= MAX_FOREIGN_SSRCS {
                st.foreign.clear();
            }
            st.foreign.insert(ssrc);
        }
        let waiter = status
            .command_tag
            .and_then(|tag| st.waiters.remove(&(ssrc, tag)));
        if let Some(waiter) = waiter {
            // The waiter may have timed out and gone; ignore.
            let _ = waiter.send(status);
        }
    }

    /// Handles one datagram from the data socket. Never panics.
    fn handle_rtp_packet(&self, bytes: &[u8]) {
        let Ok(pkt) = RtpPacket::parse(bytes) else {
            bump(&self.counters.rtp_garbage);
            return;
        };
        let mut st = self.lock();
        let Some(sink) = st
            .channels
            .get_mut(&pkt.ssrc)
            .and_then(|slot| slot.audio.as_mut())
        else {
            bump(&self.counters.rtp_foreign);
            return;
        };
        bump(&self.counters.rtp_packets);
        // A duplicate or late packet is dropped; the tracker counts it.
        if let Ok(acc) = sink.tracker.accept(pkt.ssrc, pkt.seq, pkt.timestamp) {
            sink.queue.push(AudioPacket {
                seq: acc.seq,
                timestamp: acc.timestamp,
                lost_before: acc.lost_before,
                payload_type: pkt.payload_type,
                payload: pkt.payload.to_vec(),
            });
        }
    }
}

/// Removes a waiter when the command finishes, times out or is cancelled.
struct WaiterGuard<'a> {
    shared: &'a Shared,
    key: WaiterKey,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.shared.lock().waiters.remove(&self.key);
    }
}

async fn status_task(socket: Arc<UdpSocket>, shared: Arc<Shared>) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, _)) => shared.handle_status_packet(&buf[..n]),
            Err(e) => {
                bump(&shared.counters.recv_errors);
                tracing::debug!("status socket receive error: {e}");
                tokio::time::sleep(RECV_ERROR_BACKOFF).await;
            }
        }
    }
}

async fn rtp_task(socket: UdpSocket, shared: Arc<Shared>) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, _)) => shared.handle_rtp_packet(&buf[..n]),
            Err(e) => {
                bump(&shared.counters.recv_errors);
                tracing::debug!("data socket receive error: {e}");
                tokio::time::sleep(RECV_ERROR_BACKOFF).await;
            }
        }
    }
}

/// The per-SSRC command lock.
#[derive(Debug)]
struct ChannelCtx {
    ssrc: u32,
    cmd_lock: tokio::sync::Mutex<()>,
}

impl ChannelCtx {
    fn new(ssrc: u32) -> Arc<Self> {
        Arc::new(Self {
            ssrc,
            cmd_lock: tokio::sync::Mutex::new(()),
        })
    }
}

#[derive(Debug)]
struct Core {
    config: ClientConfig,
    ctrl: Arc<UdpSocket>,
    shared: Arc<Shared>,
    local_status: SocketAddr,
    local_data: SocketAddr,
    local_channel_status: Option<SocketAddr>,
    tasks: Vec<AbortHandle>,
}

impl Drop for Core {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Core {
    /// Sends one command built by `build(tag)` on `ctx`'s SSRC, holding the
    /// channel's command lock until the reply (see the module docs for the
    /// retry policy).
    async fn execute<F>(
        &self,
        ctx: &ChannelCtx,
        build: F,
    ) -> Result<Arc<ChannelStatus>, ClientError>
    where
        F: Fn(u32) -> Command,
    {
        let _guard = ctx.cmd_lock.lock().await;
        let attempts = self.config.command_retries.saturating_add(1);
        for _ in 0..attempts {
            let tag = self.shared.next_tag();
            let cmd = build(tag);
            debug_assert_eq!(cmd.ssrc(), ctx.ssrc);
            let key = (ctx.ssrc, u64::from(tag));
            let rx = self.shared.register_waiter(key);
            let _waiter = WaiterGuard {
                shared: &self.shared,
                key,
            };
            self.ctrl
                .send_to(&cmd.encode(), self.config.status_addr)
                .await
                .map_err(|source| ClientError::Io {
                    op: "send command",
                    source,
                })?;
            bump(&self.shared.counters.commands_sent);
            match tokio::time::timeout(self.config.command_timeout, rx).await {
                Ok(Ok(status)) => return Ok(status),
                Ok(Err(_)) => return Err(ClientError::Closed),
                Err(_) => {
                    bump(&self.shared.counters.command_timeouts);
                    tracing::debug!(ssrc = ctx.ssrc, tag, "command timed out");
                }
            }
        }
        self.shared.lock().last_timeout = Some(Instant::now());
        Err(ClientError::Timeout {
            ssrc: ctx.ssrc,
            attempts,
        })
    }

    /// A poll that also refreshes the lifetime.
    fn poll_cmd(&self, ssrc: u32, tag: u32) -> Command {
        Command::poll(ssrc, tag).lifetime(self.config.lifetime_frames)
    }

    /// Best-effort, non-blocking destroy for `Drop`. If it is lost, radiod
    /// removes the channel when its lifetime runs out.
    fn destroy_nowait(&self, ssrc: u32) {
        let tag = self.shared.next_tag();
        let bytes = Command::destroy(ssrc, tag).encode();
        if let Err(e) = self.ctrl.try_send_to(&bytes, self.config.status_addr) {
            tracing::debug!(ssrc, "destroy on drop not sent: {e}");
        } else {
            bump(&self.shared.counters.commands_sent);
        }
    }

    /// Destroys `ctx`'s channel and forgets it, even if radiod does not answer.
    async fn destroy(&self, ctx: &ChannelCtx) -> Result<(), ClientError> {
        let res = self
            .execute(ctx, |tag| Command::destroy(ctx.ssrc, tag))
            .await;
        self.shared.release(ctx.ssrc);
        res.map(|_| ())
    }

    fn keepalive_interval(&self, ssrc: u32) -> Duration {
        let frame = self
            .shared
            .frame(ssrc)
            .unwrap_or(self.config.frame_duration);
        (frame.saturating_mul(self.config.lifetime_frames) / 3).max(frame)
    }
}

/// An async client for one radiod instance. Cheap to clone; clones share
/// sockets and background tasks.
#[derive(Debug, Clone)]
pub struct Ka9qClient {
    core: Arc<Core>,
}

impl Ka9qClient {
    /// Opens the sockets and starts the background tasks on the current
    /// Tokio runtime ([`NetError::NoRuntime`] outside one).
    pub fn connect(config: ClientConfig) -> Result<Self, ClientError> {
        config.validate()?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| NetError::NoRuntime)?;
        let opts = config.send_options();
        let ctrl = Arc::new(net::bind_control(config.status_addr, opts)?);
        let data = net::bind_receiver(config.data_addr, opts)?;
        let local_status = ctrl.local_addr().map_err(|source| ClientError::Io {
            op: "control socket address",
            source,
        })?;
        let local_data = data.local_addr().map_err(|source| ClientError::Io {
            op: "data socket address",
            source,
        })?;
        let channel_status = config
            .channel_status_addr
            .map(|addr| net::bind_receiver(addr, opts))
            .transpose()?;
        let local_channel_status = channel_status
            .as_ref()
            .map(UdpSocket::local_addr)
            .transpose()
            .map_err(|source| ClientError::Io {
                op: "channel status socket address",
                source,
            })?;
        let shared = Arc::new(Shared::new());
        let mut tasks = vec![
            runtime
                .spawn(status_task(Arc::clone(&ctrl), Arc::clone(&shared)))
                .abort_handle(),
            runtime
                .spawn(rtp_task(data, Arc::clone(&shared)))
                .abort_handle(),
        ];
        if let Some(socket) = channel_status {
            // Unsolicited channel status and reply copies are handled exactly
            // like the control socket's packets.
            tasks.push(
                runtime
                    .spawn(status_task(Arc::new(socket), Arc::clone(&shared)))
                    .abort_handle(),
            );
        }
        Ok(Self {
            core: Arc::new(Core {
                config,
                ctrl,
                shared,
                local_status,
                local_data,
                local_channel_status,
                tasks,
            }),
        })
    }

    /// The configuration in use.
    pub fn config(&self) -> &ClientConfig {
        &self.core.config
    }

    /// Local address of the control socket (in unicast mode, where a peer
    /// must send status replies).
    pub fn local_status_addr(&self) -> SocketAddr {
        self.core.local_status
    }

    /// Local address of the data socket (in unicast mode, where a peer must
    /// send RTP).
    pub fn local_data_addr(&self) -> SocketAddr {
        self.core.local_data
    }

    /// Local address of the channel status socket, if
    /// [`ClientConfig::channel_status_addr`] is set (in unicast mode, where
    /// a peer must send unsolicited channel status).
    pub fn local_channel_status_addr(&self) -> Option<SocketAddr> {
        self.core.local_channel_status
    }

    /// Background-task counters.
    pub fn stats(&self) -> ClientStats {
        self.core.shared.counters.snapshot()
    }

    /// The newest status packet received for any SSRC (including other
    /// clients' channels), and when it arrived. Its front-end fields
    /// (`INPUT_SAMPRATE`, `FIRST_LO_FREQUENCY`, `FE_LOW_EDGE`, `IF_POWER`,
    /// `AD_OVER`, ...) describe the radiod instance as a whole.
    pub fn last_status(&self) -> Option<(Instant, Arc<ChannelStatus>)> {
        self.core.shared.lock().last_status.clone()
    }

    /// The latest status of an SSRC this client owns (a channel opened
    /// from it), or `None`.
    pub fn status_of(&self, ssrc: u32) -> Option<Arc<ChannelStatus>> {
        self.core.shared.latest(ssrc)
    }

    /// When a command last failed because radiod did not answer, including
    /// retries.
    pub fn last_command_timeout(&self) -> Option<Instant> {
        self.core.shared.lock().last_timeout
    }

    /// Creates an Opus audio channel under a new SSRC and waits until radiod
    /// confirms its frequency and encoding. On any failure the channel is
    /// destroyed again.
    pub async fn open_audio_channel(
        &self,
        mut params: AudioChannelParams,
    ) -> Result<AudioChannel, ClientError> {
        params.validate()?;
        let core = &self.core;
        let queue = Arc::new(DropOldestQueue::new(core.config.audio_queue_len));
        let ssrc = core.shared.allocate_ssrc(Some(Arc::clone(&queue)))?;
        let ctx = ChannelCtx::new(ssrc);
        let lifetime = core.config.lifetime_frames;
        let created = core
            .execute(&ctx, |tag| params.create_command(ssrc, tag, lifetime))
            .await
            .and_then(|st| params.confirm(ssrc, st));
        match created {
            Ok(st) => {
                params.adopt_reported(ssrc, &st);
            }
            Err(e) => {
                // The command may have created the channel even if no reply came.
                if let Err(d) = core.destroy(&ctx).await {
                    tracing::debug!(ssrc, "destroy after failed open: {d}");
                }
                return Err(e);
            }
        }
        let shared = Arc::new(AudioShared {
            ctx,
            params: Mutex::new(params),
            keepalives: AtomicU64::new(0),
            keepalive_failures: AtomicU64::new(0),
            recreations: AtomicU64::new(0),
        });
        let keepalive = tokio::spawn(audio_keepalive(Arc::clone(core), Arc::clone(&shared)));
        Ok(AudioChannel {
            core: Arc::clone(core),
            shared,
            queue,
            keepalive: keepalive.abort_handle(),
            open: true,
        })
    }

    /// Creates a spectrum2 channel under a new SSRC, waits for radiod to
    /// confirm it and starts polling it every `params.row_interval`.
    pub async fn open_spectrum_channel(
        &self,
        params: SpectrumParams,
    ) -> Result<SpectrumChannel, ClientError> {
        params.validate(&self.core.config)?;
        let ctx = create_spectrum(&self.core, &params).await?;
        let shared = Arc::new(SpectrumShared {
            ssrc: AtomicU32::new(ctx.ssrc),
            current: tokio::sync::Mutex::new(SpectrumCurrent {
                ctx,
                params: params.clone(),
            }),
            queue: DropOldestQueue::new(self.core.config.spectrum_queue_len),
            counters: SpectrumCounters::default(),
        });
        let poller = tokio::spawn(spectrum_poll_loop(
            Arc::clone(&self.core),
            Arc::clone(&shared),
            params.row_interval,
        ));
        Ok(SpectrumChannel {
            core: Arc::clone(&self.core),
            shared,
            poller: poller.abort_handle(),
            open: true,
        })
    }

    /// Asks radiod for the status of every channel (SSRC `0xffffffff`) and
    /// collects the status packets seen during `window`, one per SSRC,
    /// sorted by SSRC. This creates no channel. radiod staggers the
    /// replies at four channels per frame, and sends nothing when it has no
    /// channels (use [`Ka9qClient::probe`] for front-end information then).
    pub async fn dump_all(&self, window: Duration) -> Result<Vec<Arc<ChannelStatus>>, ClientError> {
        let mut rx = self.core.shared.statuses.subscribe();
        let tag = self.core.shared.next_tag();
        self.core
            .ctrl
            .send_to(
                &Command::dump_all(tag).encode(),
                self.core.config.status_addr,
            )
            .await
            .map_err(|source| ClientError::Io {
                op: "send command",
                source,
            })?;
        bump(&self.core.shared.counters.commands_sent);
        let deadline = Instant::now() + window;
        let mut by_ssrc = BTreeMap::new();
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(status)) => {
                    if let Some(ssrc) = status.ssrc {
                        by_ssrc.insert(ssrc, status);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => return Err(ClientError::Closed),
                Err(_) => break,
            }
        }
        Ok(by_ssrc.into_values().collect())
    }

    /// Returns one status packet with front-end information by creating a
    /// temporary channel (with a [`PROBE_LIFETIME_FRAMES`] lifetime) and
    /// destroying it right after. Starting the channel starts radiod's front
    /// end if no other channel exists.
    pub async fn probe(&self) -> Result<Arc<ChannelStatus>, ClientError> {
        self.probe_with(|c| c).await
    }

    /// Like [`Ka9qClient::probe`], but the temporary channel's command is
    /// first passed through `extra`, which may add front-end fields such as
    /// [`Command::first_lo_hz`]: radiod applies those to the shared front
    /// end, whichever channel carries them. The reply shows whether radiod
    /// applied them. `extra` must not change the SSRC or the lifetime.
    pub async fn probe_with<F>(&self, extra: F) -> Result<Arc<ChannelStatus>, ClientError>
    where
        F: Fn(Command) -> Command,
    {
        let core = &self.core;
        let ssrc = core.shared.allocate_ssrc(None)?;
        let ctx = ChannelCtx::new(ssrc);
        let res = core
            .execute(&ctx, |tag| {
                extra(Command::poll(ssrc, tag)).lifetime(PROBE_LIFETIME_FRAMES)
            })
            .await;
        if let Err(e) = core.destroy(&ctx).await {
            tracing::debug!(ssrc, "destroy of probe channel: {e}");
        }
        res
    }
}

fn check_finite_positive(name: &str, v: f64) -> Result<(), ClientError> {
    if v.is_finite() && v > 0.0 {
        Ok(())
    } else {
        Err(ClientError::InvalidParams(format!(
            "{name} must be finite and > 0, got {v}"
        )))
    }
}

fn check_frequency(name: &str, requested: f64, reported: Option<f64>) -> Result<(), String> {
    match reported {
        Some(f) if (f - requested).abs() <= FREQ_TOLERANCE_HZ => Ok(()),
        Some(f) => Err(format!("{name} is {f} Hz, requested {requested} Hz")),
        None => Err(format!("status has no {name}")),
    }
}

/// Parameters of an Opus audio channel.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChannelParams {
    /// Carrier or dial frequency, Hz.
    pub frequency_hz: f64,
    /// radiod preset (mode) name from its `presets.conf`, for example `am`,
    /// `usb` or `fm`. `None` keeps the template's preset.
    pub preset: Option<String>,
    /// Passband low edge relative to the frequency, Hz. `None` keeps the
    /// preset's.
    pub low_edge_hz: Option<f32>,
    /// Passband high edge relative to the frequency, Hz.
    pub high_edge_hz: Option<f32>,
    /// Output sample rate, Hz. Must be a legal Opus rate (8, 12, 16, 24 or
    /// 48 kHz) or radiod forces 48 kHz and the open fails.
    pub samprate: Option<u32>,
    /// Opus bit rate, bit/s. 0 or `None` lets the encoder choose.
    pub opus_bitrate: Option<u32>,
    /// `OUTPUT_CHANNELS`: 1 (mono) or 2. `None` keeps the preset's. For
    /// `wfm`, 1 also turns FM stereo decoding off.
    pub output_channels: Option<u32>,
    /// The SNR squelch. `None` keeps the preset's.
    pub squelch: Option<Squelch>,
}

/// SNR squelch of an audio channel.
///
/// radiod's FM demodulators (`fm`, `wfm`) squelch on the channel SNR even
/// when `SNR_SQUELCH` is off (`fm.c`, and `wfm.c` forces `snr_enable` on at
/// start), using `SQUELCH_OPEN`/`SQUELCH_CLOSE`. [`Squelch::Off`] therefore
/// also sets both thresholds to [`SQUELCH_OFF_DB`], which keeps them open
/// for any signal with a positive SNR. Linear modes only squelch with
/// `SNR_SQUELCH` on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Squelch {
    /// No squelch (see the type docs for FM).
    Off,
    /// Open above `open_db` SNR, close below `close_db` (hysteresis).
    Snr {
        /// Opening threshold, dB SNR.
        open_db: f32,
        /// Closing threshold, dB SNR; at most `open_db`.
        close_db: f32,
    },
}

/// The squelch thresholds [`Squelch::Off`] sends, dB SNR.
pub const SQUELCH_OFF_DB: f32 = -100.0;

impl Squelch {
    fn validate(self) -> Result<(), ClientError> {
        match self {
            Self::Off => Ok(()),
            Self::Snr { open_db, close_db } => {
                if open_db.is_finite() && close_db.is_finite() && close_db <= open_db {
                    Ok(())
                } else {
                    Err(ClientError::InvalidParams(format!(
                        "squelch thresholds must be finite with close <= open, got open {open_db} close {close_db}"
                    )))
                }
            }
        }
    }

    fn apply(self, c: Command) -> Command {
        match self {
            Self::Off => c
                .snr_squelch(false)
                .squelch_open_db(SQUELCH_OFF_DB)
                .squelch_close_db(SQUELCH_OFF_DB),
            Self::Snr { open_db, close_db } => c
                .snr_squelch(true)
                .squelch_open_db(open_db)
                .squelch_close_db(close_db),
        }
    }
}

/// Longest accepted preset name.
const MAX_PRESET_LEN: usize = 32;
/// Opus' own bit rate ceiling.
const MAX_OPUS_BITRATE: u32 = 510_000;
/// Sample rates Opus accepts.
const OPUS_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

impl AudioChannelParams {
    /// Parameters for `frequency_hz` with a preset; everything else from the
    /// preset.
    pub fn new(frequency_hz: f64, preset: &str) -> Self {
        Self {
            frequency_hz,
            preset: Some(preset.to_owned()),
            low_edge_hz: None,
            high_edge_hz: None,
            samprate: None,
            opus_bitrate: None,
            output_channels: None,
            squelch: None,
        }
    }

    /// Checks ranges. Also used by the CLI.
    pub fn validate(&self) -> Result<(), ClientError> {
        check_finite_positive("frequency_hz", self.frequency_hz)?;
        if let Some(p) = &self.preset {
            let ok = !p.is_empty()
                && p.len() <= MAX_PRESET_LEN
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
            if !ok {
                return Err(ClientError::InvalidParams(format!(
                    "preset must be 1..={MAX_PRESET_LEN} characters of [A-Za-z0-9._-], got {p:?}"
                )));
            }
        }
        check_edges(self.low_edge_hz, self.high_edge_hz)?;
        if let Some(sr) = self.samprate.filter(|sr| !OPUS_RATES.contains(sr)) {
            return Err(ClientError::InvalidParams(format!(
                "samprate {sr} Hz is not an Opus rate ({OPUS_RATES:?})"
            )));
        }
        if let Some(b) = self.opus_bitrate.filter(|&b| b > MAX_OPUS_BITRATE) {
            return Err(ClientError::InvalidParams(format!(
                "opus_bitrate {b} exceeds {MAX_OPUS_BITRATE}"
            )));
        }
        if let Some(n) = self.output_channels.filter(|n| !(1..=2).contains(n)) {
            return Err(ClientError::InvalidParams(format!(
                "output_channels must be 1 or 2, got {n}"
            )));
        }
        if let Some(sq) = self.squelch {
            sq.validate()?;
        }
        Ok(())
    }

    fn create_command(&self, ssrc: u32, tag: u32, lifetime: u32) -> Command {
        let mut c = Command::new(ssrc, tag);
        if let Some(p) = &self.preset {
            c = c.preset(p.clone());
        }
        if let Some(sr) = self.samprate {
            c = c.output_samprate(sr);
        }
        if let Some(n) = self.output_channels {
            c = c.output_channels(n);
        }
        c = c.output_encoding(Encoding::Opus);
        if let Some(b) = self.opus_bitrate {
            c = c.opus_bit_rate(b);
        }
        c = c.radio_frequency_hz(self.frequency_hz);
        if let Some(lo) = self.low_edge_hz {
            c = c.low_edge_hz(lo);
        }
        if let Some(hi) = self.high_edge_hz {
            c = c.high_edge_hz(hi);
        }
        if let Some(sq) = self.squelch {
            c = sq.apply(c);
        }
        c.lifetime(lifetime)
    }

    /// Checks that a status reply shows this is still our channel: the
    /// frequency and Opus encoding we set. A channel radiod re-created from
    /// the template fails this (0 Hz, PCM).
    ///
    /// The output sample rate is deliberately not checked: radiod overrides it
    /// for some modes (WFM always outputs 48 kHz, `wfm.c`; Opus forces 48 kHz
    /// for rates it cannot encode). Treating that as a lost channel made the
    /// keepalive re-send the create, which restarts the demodulator every
    /// time. Use [`AudioChannelParams::adopt_reported`] instead.
    fn check(&self, st: &ChannelStatus) -> Result<(), String> {
        check_frequency("RADIO_FREQUENCY", self.frequency_hz, st.radio_frequency_hz)?;
        if st.output_encoding != Some(Encoding::Opus) {
            return Err(format!(
                "OUTPUT_ENCODING is {:?}, requested Opus",
                st.output_encoding
            ));
        }
        Ok(())
    }

    /// Adopts the output sample rate radiod reports when it differs from the
    /// requested one, so that a later re-create asks for what radiod will
    /// actually run instead of forcing a demodulator restart. Returns true if
    /// the parameters changed.
    fn adopt_reported(&mut self, ssrc: u32, st: &ChannelStatus) -> bool {
        match (self.samprate, st.output_samprate) {
            (Some(want), Some(got)) if want != got => {
                tracing::info!(
                    ssrc,
                    "radiod runs OUTPUT_SAMPRATE {got} Hz (requested {want} Hz)"
                );
                self.samprate = Some(got);
                true
            }
            _ => false,
        }
    }

    fn confirm(
        &self,
        ssrc: u32,
        st: Arc<ChannelStatus>,
    ) -> Result<Arc<ChannelStatus>, ClientError> {
        match self.check(&st) {
            Ok(()) => Ok(st),
            Err(reason) => Err(ClientError::Rejected { ssrc, reason }),
        }
    }
}

fn check_edges(low: Option<f32>, high: Option<f32>) -> Result<(), ClientError> {
    for (name, v) in [("low_edge_hz", low), ("high_edge_hz", high)] {
        if v.is_some_and(|v| !v.is_finite()) {
            return Err(ClientError::InvalidParams(format!("{name} must be finite")));
        }
    }
    if let (Some(lo), Some(hi)) = (low, high) {
        if lo >= hi {
            return Err(ClientError::InvalidParams(format!(
                "low_edge_hz {lo} must be below high_edge_hz {hi}"
            )));
        }
    }
    Ok(())
}

/// A change to a live [`AudioChannel`]. `None` fields are left alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioUpdate {
    /// New preset (mode). radiod reloads the preset, which resets the
    /// sample rate, passband and demodulator, so the client re-sends the
    /// channel's full configuration with it (the other fields of this
    /// update applied on top).
    pub preset: Option<String>,
    /// New frequency, Hz.
    pub frequency_hz: Option<f64>,
    /// New passband low edge, Hz.
    pub low_edge_hz: Option<f32>,
    /// New passband high edge, Hz.
    pub high_edge_hz: Option<f32>,
    /// New Opus bit rate, bit/s.
    pub opus_bitrate: Option<u32>,
    /// New squelch.
    pub squelch: Option<Squelch>,
}

/// Counters of one [`AudioChannel`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioStats {
    /// RTP packets seen for this SSRC.
    pub rtp_packets: u64,
    /// Packets lost on the network (sequence gaps).
    pub lost: u64,
    /// Duplicate or late packets dropped.
    pub duplicates_or_late: u64,
    /// Packets dropped because the consumer fell behind (drop-oldest).
    pub queue_drops: u64,
    /// Successful lifetime refreshes.
    pub keepalives: u64,
    /// Refreshes that failed after all retries.
    pub keepalive_failures: u64,
    /// Times the channel was re-created after radiod lost it.
    pub recreations: u64,
}

#[derive(Debug)]
struct AudioShared {
    ctx: Arc<ChannelCtx>,
    params: Mutex<AudioChannelParams>,
    keepalives: AtomicU64,
    keepalive_failures: AtomicU64,
    recreations: AtomicU64,
}

impl AudioShared {
    fn params(&self) -> AudioChannelParams {
        self.params
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

async fn audio_keepalive(core: Arc<Core>, shared: Arc<AudioShared>) {
    let ssrc = shared.ctx.ssrc;
    let lifetime = core.config.lifetime_frames;
    loop {
        tokio::time::sleep(core.keepalive_interval(ssrc)).await;
        match core
            .execute(&shared.ctx, |tag| core.poll_cmd(ssrc, tag))
            .await
        {
            Ok(st) => {
                bump(&shared.keepalives);
                let params = {
                    let mut guard = shared.params.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.adopt_reported(ssrc, &st);
                    guard.clone()
                };
                if let Err(why) = params.check(&st) {
                    tracing::warn!(ssrc, "channel lost its configuration ({why}); re-creating");
                    bump(&shared.recreations);
                    if let Err(e) = core
                        .execute(&shared.ctx, |tag| {
                            params.create_command(ssrc, tag, lifetime)
                        })
                        .await
                    {
                        tracing::warn!(ssrc, "re-creating channel: {e}");
                    }
                }
            }
            Err(ClientError::Closed) => return,
            Err(e) => {
                bump(&shared.keepalive_failures);
                tracing::warn!(ssrc, "keepalive failed: {e}");
            }
        }
    }
}

/// A live Opus audio channel. Dropping it sends a best-effort destroy;
/// prefer [`AudioChannel::close`], which waits for radiod's reply.
#[derive(Debug)]
pub struct AudioChannel {
    core: Arc<Core>,
    shared: Arc<AudioShared>,
    queue: Arc<DropOldestQueue<AudioPacket>>,
    keepalive: AbortHandle,
    open: bool,
}

impl AudioChannel {
    /// The channel's RTP SSRC.
    pub fn ssrc(&self) -> u32 {
        self.shared.ctx.ssrc
    }

    /// The current parameters.
    pub fn params(&self) -> AudioChannelParams {
        self.shared.params()
    }

    /// The next packet, or `None` after the channel was closed.
    pub async fn recv(&mut self) -> Option<AudioPacket> {
        self.queue.recv().await
    }

    /// The next packet if one is queued.
    pub fn try_recv(&mut self) -> Option<AudioPacket> {
        self.queue.try_recv()
    }

    /// A handle that receives this channel's packets without borrowing
    /// the channel, so that one task can consume audio while another
    /// updates or closes the channel. It returns `None` once the channel is
    /// closed or dropped. The queue has a single consumer: use either the
    /// receiver or [`AudioChannel::recv`], not both.
    pub fn receiver(&self) -> AudioReceiver {
        AudioReceiver {
            queue: Arc::clone(&self.queue),
        }
    }

    /// The latest status radiod sent for this channel.
    pub fn status(&self) -> Option<Arc<ChannelStatus>> {
        self.core.shared.latest(self.ssrc())
    }

    /// radiod's frame (block) duration for this channel, from its status
    /// (`FILTER_BLOCKSIZE` / `INPUT_SAMPRATE`); one Opus packet per frame.
    /// `None` until a status with both fields has arrived.
    pub fn frame_duration(&self) -> Option<Duration> {
        self.core.shared.frame(self.ssrc())
    }

    /// Counters.
    pub fn stats(&self) -> AudioStats {
        let (rtp_packets, lost, duplicates_or_late) = self
            .core
            .shared
            .lock()
            .channels
            .get(&self.ssrc())
            .and_then(|s| s.audio.as_ref())
            .map_or((0, 0, 0), |a| {
                (a.tracker.packets, a.tracker.lost, a.tracker.dropped)
            });
        AudioStats {
            rtp_packets,
            lost,
            duplicates_or_late,
            queue_drops: self.queue.dropped(),
            keepalives: get(&self.shared.keepalives),
            keepalive_failures: get(&self.shared.keepalive_failures),
            recreations: get(&self.shared.recreations),
        }
    }

    /// Changes the channel and waits for radiod to confirm it. A new
    /// preset re-sends the full configuration (see [`AudioUpdate::preset`]);
    /// anything else sends only the changed fields.
    pub async fn update(&self, update: AudioUpdate) -> Result<Arc<ChannelStatus>, ClientError> {
        let mut next = self.shared.params();
        if update.preset.is_some() {
            next.preset.clone_from(&update.preset);
        }
        if update.squelch.is_some() {
            next.squelch = update.squelch;
        }
        if let Some(f) = update.frequency_hz {
            next.frequency_hz = f;
        }
        if update.low_edge_hz.is_some() {
            next.low_edge_hz = update.low_edge_hz;
        }
        if update.high_edge_hz.is_some() {
            next.high_edge_hz = update.high_edge_hz;
        }
        if update.opus_bitrate.is_some() {
            next.opus_bitrate = update.opus_bitrate;
        }
        next.validate()?;
        let ssrc = self.ssrc();
        let lifetime = self.core.config.lifetime_frames;
        let full = update.preset.is_some();
        let st = self
            .core
            .execute(&self.shared.ctx, |tag| {
                if full {
                    return next.create_command(ssrc, tag, lifetime);
                }
                let mut c = Command::new(ssrc, tag);
                if let Some(sq) = update.squelch {
                    c = sq.apply(c);
                }
                if let Some(b) = update.opus_bitrate {
                    c = c.opus_bit_rate(b);
                }
                if let Some(f) = update.frequency_hz {
                    c = c.radio_frequency_hz(f);
                }
                if let Some(lo) = update.low_edge_hz {
                    c = c.low_edge_hz(lo);
                }
                if let Some(hi) = update.high_edge_hz {
                    c = c.high_edge_hz(hi);
                }
                c.lifetime(lifetime)
            })
            .await?;
        let st = next.confirm(ssrc, st)?;
        next.adopt_reported(ssrc, &st);
        *self
            .shared
            .params
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = next;
        Ok(st)
    }

    /// Destroys the channel (`LIFETIME` 1) and waits for radiod's reply.
    /// The channel is forgotten locally even if radiod does not answer; it
    /// then expires on its own.
    pub async fn close(mut self) -> Result<(), ClientError> {
        self.keepalive.abort();
        self.open = false;
        self.core.destroy(&self.shared.ctx).await
    }
}

/// Receives an [`AudioChannel`]'s packets; see [`AudioChannel::receiver`].
#[derive(Debug, Clone)]
pub struct AudioReceiver {
    queue: Arc<DropOldestQueue<AudioPacket>>,
}

impl AudioReceiver {
    /// The next packet, or `None` after the channel was closed or dropped.
    pub async fn recv(&self) -> Option<AudioPacket> {
        self.queue.recv().await
    }

    /// The next packet if one is queued.
    pub fn try_recv(&self) -> Option<AudioPacket> {
        self.queue.try_recv()
    }
}

impl Drop for AudioChannel {
    fn drop(&mut self) {
        self.keepalive.abort();
        if self.open {
            self.core.destroy_nowait(self.ssrc());
            self.core.shared.release(self.ssrc());
        }
    }
}

/// Parameters of a spectrum2 channel (8-bit log bins, low to high).
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumParams {
    /// Center frequency, Hz.
    pub center_hz: f64,
    /// Number of bins. Fixed for the life of one radiod channel.
    pub bin_count: u32,
    /// Bin spacing (`RESOLUTION_BW`), Hz.
    pub resolution_bw_hz: f32,
    /// FFTs averaged per row (`SPECTRUM_AVG`). `None` keeps radiod's default.
    pub avg: Option<u32>,
    /// Time between row polls. Must be at least one frame and at most a
    /// quarter of the lifetime.
    pub row_interval: Duration,
}

impl SpectrumParams {
    /// Parameters with radiod's default averaging and 10 rows/s.
    pub fn new(center_hz: f64, bin_count: u32, resolution_bw_hz: f32) -> Self {
        Self {
            center_hz,
            bin_count,
            resolution_bw_hz,
            avg: None,
            row_interval: Duration::from_millis(100),
        }
    }

    /// Checks ranges against `config`.
    pub fn validate(&self, config: &ClientConfig) -> Result<(), ClientError> {
        check_finite_positive("center_hz", self.center_hz)?;
        check_finite_positive("resolution_bw_hz", f64::from(self.resolution_bw_hz))?;
        if !(1..=MAX_BIN_COUNT).contains(&self.bin_count) {
            return Err(ClientError::InvalidParams(format!(
                "bin_count must be 1..={MAX_BIN_COUNT}, got {}",
                self.bin_count
            )));
        }
        if self.avg == Some(0) {
            return Err(ClientError::InvalidParams("avg must be >= 1".into()));
        }
        if self.row_interval < config.frame_duration || self.row_interval > config.lifetime() / 4 {
            return Err(ClientError::InvalidParams(format!(
                "row_interval must be between one frame ({:?}) and a quarter of the lifetime ({:?})",
                config.frame_duration,
                config.lifetime() / 4
            )));
        }
        Ok(())
    }

    fn create_command(&self, ssrc: u32, tag: u32, lifetime: u32) -> Command {
        let mut c = Command::new(ssrc, tag)
            .demod_type(DemodType::Spect2)
            .radio_frequency_hz(self.center_hz)
            .bin_count(self.bin_count)
            .resolution_bw_hz(self.resolution_bw_hz);
        if let Some(avg) = self.avg {
            c = c.spectrum_avg(avg);
        }
        c.lifetime(lifetime)
    }

    fn check(&self, st: &ChannelStatus) -> Result<(), String> {
        if st.demod_type != Some(DemodType::Spect2) {
            return Err(format!(
                "DEMOD_TYPE is {:?}, requested spectrum2",
                st.demod_type
            ));
        }
        if st.bin_count != Some(self.bin_count) {
            return Err(format!(
                "BIN_COUNT is {:?}, requested {}",
                st.bin_count, self.bin_count
            ));
        }
        check_frequency("RADIO_FREQUENCY", self.center_hz, st.radio_frequency_hz)
    }
}

/// A change to a live [`SpectrumChannel`]. `None` fields are left alone.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SpectrumUpdate {
    /// New center frequency, Hz.
    pub center_hz: Option<f64>,
    /// New bin spacing, Hz.
    pub resolution_bw_hz: Option<f32>,
    /// New bin count: re-creates the channel under a new SSRC.
    pub bin_count: Option<u32>,
    /// New averaging.
    pub avg: Option<u32>,
}

/// Counters of one [`SpectrumChannel`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpectrumStats {
    /// Polls sent (including retries: see [`ClientStats::commands_sent`]).
    pub polls: u64,
    /// Rows delivered to the queue.
    pub frames: u64,
    /// Replies without usable bin data (the first reply after creation has
    /// none, per radiod-notes.md §6).
    pub empty_replies: u64,
    /// Polls that failed after all retries.
    pub poll_failures: u64,
    /// Rows dropped because the consumer fell behind (drop-oldest).
    pub queue_drops: u64,
    /// Times the channel was re-created after radiod lost it.
    pub recreations: u64,
    /// Bin-count changes (each one a new SSRC).
    pub geometry_changes: u64,
}

#[derive(Debug, Default)]
struct SpectrumCounters {
    polls: AtomicU64,
    frames: AtomicU64,
    empty_replies: AtomicU64,
    poll_failures: AtomicU64,
    recreations: AtomicU64,
    geometry_changes: AtomicU64,
}

#[derive(Debug)]
struct SpectrumCurrent {
    ctx: Arc<ChannelCtx>,
    params: SpectrumParams,
}

#[derive(Debug)]
struct SpectrumShared {
    ssrc: AtomicU32,
    current: tokio::sync::Mutex<SpectrumCurrent>,
    queue: DropOldestQueue<SpectrumFrame>,
    counters: SpectrumCounters,
}

async fn create_spectrum(
    core: &Core,
    params: &SpectrumParams,
) -> Result<Arc<ChannelCtx>, ClientError> {
    let ssrc = core.shared.allocate_ssrc(None)?;
    let ctx = ChannelCtx::new(ssrc);
    let lifetime = core.config.lifetime_frames;
    let created = core
        .execute(&ctx, |tag| params.create_command(ssrc, tag, lifetime))
        .await
        .and_then(|st| {
            params
                .check(&st)
                .map_err(|reason| ClientError::Rejected { ssrc, reason })
        });
    match created {
        Ok(()) => Ok(ctx),
        Err(e) => {
            if let Err(d) = core.destroy(&ctx).await {
                tracing::debug!(ssrc, "destroy after failed open: {d}");
            }
            Err(e)
        }
    }
}

/// Builds a row from a spectrum2 status reply, with that reply's own
/// `SPECTRUM_BASE`/`SPECTRUM_STEP` (radiod rescales them on every poll).
/// `None` if bin data or scaling is missing or the bin count differs.
fn frame_from_status(st: &ChannelStatus, bin_count: u32) -> Option<SpectrumFrame> {
    let bins = st.bin_bytes.as_ref()?;
    if bins.is_empty() || u32::try_from(bins.len()).ok()? != bin_count {
        return None;
    }
    let base = st.spectrum_base_db?;
    let step = st.spectrum_step_db?;
    let center_hz = st.radio_frequency_hz?;
    let bin_width_hz = st.resolution_bw_hz?;
    if !(base.is_finite() && step.is_finite() && step >= 0.0 && center_hz.is_finite()) {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "radiod sends BASE and STEP as C floats; the status decoder widened them"
    )]
    let (base_db, step_db) = (base as f32, step as f32);
    Some(SpectrumFrame {
        center_hz,
        bin_width_hz,
        base_db,
        step_db,
        bins: bins.clone(),
    })
}

async fn spectrum_poll_loop(core: Arc<Core>, shared: Arc<SpectrumShared>, interval: Duration) {
    let lifetime = core.config.lifetime_frames;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let cur = shared.current.lock().await;
        let ssrc = cur.ctx.ssrc;
        bump(&shared.counters.polls);
        match core.execute(&cur.ctx, |tag| core.poll_cmd(ssrc, tag)).await {
            Ok(st) => {
                if st.demod_type.is_some_and(|d| d != DemodType::Spect2) {
                    // The poll re-created an expired channel from the template.
                    tracing::warn!(ssrc, "spectrum channel lost; re-creating");
                    bump(&shared.counters.recreations);
                    if let Err(e) = core
                        .execute(&cur.ctx, |tag| {
                            cur.params.create_command(ssrc, tag, lifetime)
                        })
                        .await
                    {
                        tracing::warn!(ssrc, "re-creating spectrum channel: {e}");
                    }
                    continue;
                }
                if let Some(frame) = frame_from_status(&st, cur.params.bin_count) {
                    bump(&shared.counters.frames);
                    shared.queue.push(frame);
                } else {
                    bump(&shared.counters.empty_replies);
                }
            }
            Err(ClientError::Closed) => {
                shared.queue.close();
                return;
            }
            Err(e) => {
                bump(&shared.counters.poll_failures);
                tracing::debug!(ssrc, "spectrum poll failed: {e}");
            }
        }
    }
}

/// A live spectrum2 channel, polled in the background. Dropping it sends a
/// best-effort destroy; prefer [`SpectrumChannel::close`].
#[derive(Debug)]
pub struct SpectrumChannel {
    core: Arc<Core>,
    shared: Arc<SpectrumShared>,
    poller: AbortHandle,
    open: bool,
}

impl SpectrumChannel {
    /// The current SSRC (changes when the bin count changes).
    pub fn ssrc(&self) -> u32 {
        self.shared.ssrc.load(Ordering::Relaxed)
    }

    /// The current parameters.
    pub async fn params(&self) -> SpectrumParams {
        self.shared.current.lock().await.params.clone()
    }

    /// The next row, or `None` after the client shut down.
    pub async fn recv(&mut self) -> Option<SpectrumFrame> {
        self.shared.queue.recv().await
    }

    /// The next row if one is queued.
    pub fn try_recv(&mut self) -> Option<SpectrumFrame> {
        self.shared.queue.try_recv()
    }

    /// A handle that receives this channel's rows without borrowing the
    /// channel (see [`AudioChannel::receiver`]). It returns `None` once the
    /// channel is closed or dropped.
    pub fn receiver(&self) -> SpectrumReceiver {
        SpectrumReceiver {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The latest status radiod sent for the current SSRC.
    pub fn status(&self) -> Option<Arc<ChannelStatus>> {
        self.core.shared.latest(self.ssrc())
    }

    /// Counters.
    pub fn stats(&self) -> SpectrumStats {
        let c = &self.shared.counters;
        SpectrumStats {
            polls: get(&c.polls),
            frames: get(&c.frames),
            empty_replies: get(&c.empty_replies),
            poll_failures: get(&c.poll_failures),
            queue_drops: self.shared.queue.dropped(),
            recreations: get(&c.recreations),
            geometry_changes: get(&c.geometry_changes),
        }
    }

    /// Changes the channel. A new bin count creates a new radiod channel
    /// under a new SSRC first and then destroys the old one (changing
    /// `BIN_COUNT` in place crashes radiod); other changes are applied in
    /// place. Returns the (possibly new) SSRC. The row interval is fixed.
    pub async fn update(&self, update: SpectrumUpdate) -> Result<u32, ClientError> {
        let mut cur = self.shared.current.lock().await;
        let mut next = cur.params.clone();
        if let Some(c) = update.center_hz {
            next.center_hz = c;
        }
        if let Some(r) = update.resolution_bw_hz {
            next.resolution_bw_hz = r;
        }
        if let Some(b) = update.bin_count {
            next.bin_count = b;
        }
        if update.avg.is_some() {
            next.avg = update.avg;
        }
        next.validate(&self.core.config)?;
        if next.bin_count == cur.params.bin_count {
            let ssrc = cur.ctx.ssrc;
            let lifetime = self.core.config.lifetime_frames;
            let st = self
                .core
                .execute(&cur.ctx, |tag| {
                    let mut c = Command::new(ssrc, tag);
                    if let Some(f) = update.center_hz {
                        c = c.radio_frequency_hz(f);
                    }
                    if let Some(r) = update.resolution_bw_hz {
                        c = c.resolution_bw_hz(r);
                    }
                    if let Some(a) = update.avg {
                        c = c.spectrum_avg(a);
                    }
                    c.lifetime(lifetime)
                })
                .await?;
            next.check(&st)
                .map_err(|reason| ClientError::Rejected { ssrc, reason })?;
        } else {
            let new_ctx = create_spectrum(&self.core, &next).await?;
            let old = std::mem::replace(&mut cur.ctx, new_ctx);
            self.shared.ssrc.store(cur.ctx.ssrc, Ordering::Relaxed);
            bump(&self.shared.counters.geometry_changes);
            if let Err(e) = self.core.destroy(&old).await {
                tracing::warn!(ssrc = old.ssrc, "destroying the old spectrum channel: {e}");
            }
        }
        cur.params = next;
        Ok(cur.ctx.ssrc)
    }

    /// Stops polling, destroys the channel (`LIFETIME` 1) and waits for
    /// radiod's reply. The channel is forgotten locally even on error.
    pub async fn close(mut self) -> Result<(), ClientError> {
        self.poller.abort();
        self.open = false;
        self.shared.queue.close();
        let cur = self.shared.current.lock().await;
        self.core.destroy(&cur.ctx).await
    }
}

/// Receives a [`SpectrumChannel`]'s rows; see [`SpectrumChannel::receiver`].
#[derive(Debug, Clone)]
pub struct SpectrumReceiver {
    shared: Arc<SpectrumShared>,
}

impl SpectrumReceiver {
    /// The next row, or `None` after the channel was closed or dropped.
    pub async fn recv(&self) -> Option<SpectrumFrame> {
        self.shared.queue.recv().await
    }

    /// The next row if one is queued.
    pub fn try_recv(&self) -> Option<SpectrumFrame> {
        self.shared.queue.try_recv()
    }
}

impl Drop for SpectrumChannel {
    fn drop(&mut self) {
        self.poller.abort();
        self.shared.queue.close();
        if self.open {
            let ssrc = self.ssrc();
            self.core.destroy_nowait(ssrc);
            self.core.shared.release(ssrc);
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "values are copied, not computed")]
mod tests {
    use super::*;
    use crate::tags::PktType;
    use crate::tlv::Encoder;
    use proptest::prelude::*;

    fn status_bytes(ssrc: u32, tag: u32) -> Vec<u8> {
        Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, ssrc)
            .u32(StatusType::CommandTag, tag)
            .u32(StatusType::InputSamprate, 24_000_000)
            .u32(StatusType::FilterBlocksize, 480_000)
            .f64(StatusType::RadioFrequency, 10e6)
            .finish()
    }

    fn rtp(ssrc: u32, seq: u16, ts: u32) -> Vec<u8> {
        let mut b = vec![0x80, 111];
        b.extend_from_slice(&seq.to_be_bytes());
        b.extend_from_slice(&ts.to_be_bytes());
        b.extend_from_slice(&ssrc.to_be_bytes());
        b.extend_from_slice(&[0x28, 1, 2, 3]);
        b
    }

    #[test]
    fn default_config_is_valid_and_matches_test_setup() {
        let c = ClientConfig::default();
        c.validate().unwrap();
        assert_eq!(c.status_addr.to_string(), "239.77.0.6:5006");
        assert_eq!(c.data_addr.to_string(), "239.77.0.4:5004");
        assert_eq!(c.lifetime(), Duration::from_secs(10));
    }

    #[test]
    fn invalid_configs_are_rejected() {
        let base = ClientConfig::default();
        let cases = [
            ClientConfig {
                command_timeout: Duration::ZERO,
                ..base.clone()
            },
            ClientConfig {
                lifetime_frames: 1,
                ..base.clone()
            },
            ClientConfig {
                lifetime_frames: 0,
                ..base.clone()
            },
            ClientConfig {
                frame_duration: Duration::ZERO,
                ..base.clone()
            },
            ClientConfig {
                frame_duration: Duration::from_secs(2),
                ..base.clone()
            },
            ClientConfig {
                audio_queue_len: 0,
                ..base.clone()
            },
            ClientConfig {
                spectrum_queue_len: 0,
                ..base.clone()
            },
            ClientConfig {
                lifetime_frames: 10,
                ..base.clone()
            },
        ];
        for c in cases {
            assert!(
                matches!(c.validate(), Err(ClientError::InvalidConfig(_))),
                "{c:?}"
            );
        }
    }

    #[test]
    fn audio_params_validation() {
        let ok = AudioChannelParams::new(10e6, "am");
        ok.validate().unwrap();
        let bad = [
            AudioChannelParams::new(0.0, "am"),
            AudioChannelParams::new(f64::NAN, "am"),
            AudioChannelParams::new(-1.0, "am"),
            AudioChannelParams::new(10e6, ""),
            AudioChannelParams::new(10e6, "a m"),
            AudioChannelParams::new(10e6, &"x".repeat(33)),
            AudioChannelParams {
                samprate: Some(11_025),
                ..ok.clone()
            },
            AudioChannelParams {
                opus_bitrate: Some(600_000),
                ..ok.clone()
            },
            AudioChannelParams {
                low_edge_hz: Some(100.0),
                high_edge_hz: Some(-100.0),
                ..ok.clone()
            },
            AudioChannelParams {
                low_edge_hz: Some(f32::INFINITY),
                ..ok.clone()
            },
        ];
        for p in bad {
            assert!(
                matches!(p.validate(), Err(ClientError::InvalidParams(_))),
                "{p:?}"
            );
        }
        AudioChannelParams {
            preset: None,
            samprate: Some(12_000),
            opus_bitrate: Some(16_000),
            low_edge_hz: Some(-5000.0),
            high_edge_hz: Some(5000.0),
            ..ok
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn audio_check_reports_mismatches() {
        let p = AudioChannelParams {
            samprate: Some(12_000),
            ..AudioChannelParams::new(10e6, "am")
        };
        let good = ChannelStatus {
            radio_frequency_hz: Some(10e6 + 0.5),
            output_encoding: Some(Encoding::Opus),
            output_samprate: Some(12_000),
            ..ChannelStatus::default()
        };
        p.check(&good).unwrap();
        let wrong_freq = ChannelStatus {
            radio_frequency_hz: Some(0.0),
            ..good.clone()
        };
        assert!(
            p.check(&wrong_freq)
                .unwrap_err()
                .contains("RADIO_FREQUENCY")
        );
        let no_freq = ChannelStatus {
            radio_frequency_hz: None,
            ..good.clone()
        };
        assert!(
            p.check(&no_freq)
                .unwrap_err()
                .contains("no RADIO_FREQUENCY")
        );
        let pcm = ChannelStatus {
            output_encoding: Some(Encoding::S16be),
            ..good.clone()
        };
        assert!(p.check(&pcm).unwrap_err().contains("OUTPUT_ENCODING"));
        // radiod overriding the sample rate (e.g. WFM -> 48 kHz) is not a
        // lost channel: it is accepted and adopted.
        let rate = ChannelStatus {
            output_samprate: Some(48_000),
            ..good
        };
        let mut adopted = p.clone();
        assert!(adopted.confirm(7, Arc::new(rate.clone())).is_ok());
        assert!(adopted.adopt_reported(7, &rate));
        assert_eq!(adopted.samprate, Some(48_000));
        assert!(!adopted.adopt_reported(7, &rate), "already adopted");
        let mut unset = AudioChannelParams::new(10e6, "am");
        assert!(
            !unset.adopt_reported(7, &rate),
            "no requested rate: nothing to adopt"
        );
    }

    #[test]
    fn spectrum_params_validation() {
        let cfg = ClientConfig::default();
        let ok = SpectrumParams::new(10e6, 1024, 100.0);
        ok.validate(&cfg).unwrap();
        let bad = [
            SpectrumParams::new(0.0, 1024, 100.0),
            SpectrumParams::new(10e6, 0, 100.0),
            SpectrumParams::new(10e6, MAX_BIN_COUNT + 1, 100.0),
            SpectrumParams::new(10e6, 1024, 0.0),
            SpectrumParams::new(10e6, 1024, f32::NAN),
            SpectrumParams {
                avg: Some(0),
                ..ok.clone()
            },
            SpectrumParams {
                row_interval: Duration::from_millis(1),
                ..ok.clone()
            },
            SpectrumParams {
                row_interval: Duration::from_secs(5),
                ..ok
            },
        ];
        for p in bad {
            assert!(
                matches!(p.validate(&cfg), Err(ClientError::InvalidParams(_))),
                "{p:?}"
            );
        }
    }

    #[test]
    fn spectrum_check_and_frame_extraction() {
        let p = SpectrumParams::new(10e6, 4, 100.0);
        let st = ChannelStatus {
            demod_type: Some(DemodType::Spect2),
            bin_count: Some(4),
            radio_frequency_hz: Some(10e6),
            resolution_bw_hz: Some(100.0),
            spectrum_base_db: Some(-100.0),
            spectrum_step_db: Some(0.25),
            bin_bytes: Some(vec![0, 1, 255, 2]),
            ..ChannelStatus::default()
        };
        p.check(&st).unwrap();
        let f = frame_from_status(&st, 4).unwrap();
        assert_eq!(f.bins, vec![0, 1, 255, 2]);
        assert_eq!(f.base_db, -100.0);
        assert_eq!(f.step_db, 0.25);
        assert_eq!(f.frequency_of_bin(2), 10e6);
        assert!(frame_from_status(&st, 5).is_none());
        let linear = ChannelStatus {
            demod_type: Some(DemodType::Linear),
            ..st.clone()
        };
        assert!(p.check(&linear).is_err());
        let bins = ChannelStatus {
            bin_count: Some(8),
            ..st.clone()
        };
        assert!(p.check(&bins).unwrap_err().contains("BIN_COUNT"));
        for broken in [
            ChannelStatus {
                bin_bytes: None,
                ..st.clone()
            },
            ChannelStatus {
                bin_bytes: Some(Vec::new()),
                ..st.clone()
            },
            ChannelStatus {
                spectrum_step_db: Some(f64::NAN),
                ..st.clone()
            },
            ChannelStatus {
                spectrum_step_db: Some(-1.0),
                ..st.clone()
            },
            ChannelStatus {
                spectrum_base_db: None,
                ..st.clone()
            },
            ChannelStatus {
                resolution_bw_hz: None,
                ..st
            },
        ] {
            assert!(frame_from_status(&broken, 4).is_none());
        }
    }

    #[test]
    fn frame_duration_from_status() {
        let bytes = status_bytes(1, 2);
        let st = ChannelStatus::decode(&bytes).unwrap();
        assert_eq!(
            frame_duration_of(&bytes, &st),
            Some(Duration::from_millis(20))
        );
        let no_block = Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 1)
            .u32(StatusType::InputSamprate, 24_000_000)
            .finish();
        let st = ChannelStatus::decode(&no_block).unwrap();
        assert_eq!(frame_duration_of(&no_block, &st), None);
        let zero_rate = Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 1)
            .u32(StatusType::InputSamprate, 0)
            .u32(StatusType::FilterBlocksize, 480)
            .finish();
        let st = ChannelStatus::decode(&zero_rate).unwrap();
        assert_eq!(frame_duration_of(&zero_rate, &st), None);
        let absurd = Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 1)
            .u32(StatusType::InputSamprate, 1)
            .u32(StatusType::FilterBlocksize, 480)
            .finish();
        let st = ChannelStatus::decode(&absurd).unwrap();
        assert_eq!(frame_duration_of(&absurd, &st), None);
    }

    #[tokio::test]
    async fn status_routing_to_waiters_and_slots() {
        let shared = Shared::new();
        let ssrc = shared.allocate_ssrc(None).unwrap();
        assert_ne!(ssrc, 0);
        let rx = shared.register_waiter((ssrc, 42));
        let mut all = shared.statuses.subscribe();
        shared.handle_status_packet(&status_bytes(ssrc, 42));
        let st = rx.await.unwrap();
        assert_eq!(st.ssrc, Some(ssrc));
        assert_eq!(shared.latest(ssrc).unwrap().command_tag, Some(42));
        assert_eq!(shared.frame(ssrc), Some(Duration::from_millis(20)));
        assert_eq!(all.recv().await.unwrap().ssrc, Some(ssrc));
        // A foreign SSRC is counted and remembered.
        shared.handle_status_packet(&status_bytes(ssrc.wrapping_add(1), 1));
        let c = shared.counters.snapshot();
        assert_eq!(c.status_packets, 2);
        assert_eq!(c.foreign_status, 1);
        assert!(shared.lock().foreign.contains(&ssrc.wrapping_add(1)));
        shared.release(ssrc);
        assert!(shared.latest(ssrc).is_none());
    }

    #[test]
    fn garbage_status_is_counted() {
        let shared = Shared::new();
        shared.handle_status_packet(&[]);
        shared.handle_status_packet(&[7, 1, 2]);
        shared.handle_status_packet(&[1, 18, 1, 5, 0]); // a CMD packet
        shared.handle_status_packet(&[0, 18, 9, 1]); // truncated TLV
        shared.handle_status_packet(&Encoder::new(PktType::Status).finish()); // no SSRC
        let c = shared.counters.snapshot();
        assert_eq!(c.garbage_status, 4);
        assert_eq!(c.command_packets_seen, 1);
        assert_eq!(c.status_packets, 0);
    }

    #[test]
    fn foreign_ssrc_set_is_bounded() {
        let shared = Shared::new();
        for i in 1..=u32::try_from(MAX_FOREIGN_SSRCS).unwrap() + 5 {
            shared.handle_status_packet(&status_bytes(i, 0));
        }
        assert!(shared.lock().foreign.len() <= MAX_FOREIGN_SSRCS);
    }

    #[tokio::test]
    async fn rtp_demux_tracks_sequence_and_drops_foreign() {
        let shared = Shared::new();
        let q = Arc::new(DropOldestQueue::new(16));
        let ssrc = shared.allocate_ssrc(Some(Arc::clone(&q))).unwrap();
        shared.handle_rtp_packet(&rtp(ssrc, 10, 1000));
        shared.handle_rtp_packet(&rtp(ssrc, 13, 1000 + 3 * 960));
        shared.handle_rtp_packet(&rtp(ssrc, 12, 1000 + 2 * 960)); // late
        shared.handle_rtp_packet(&rtp(ssrc.wrapping_add(1), 1, 0)); // foreign
        shared.handle_rtp_packet(&[0x80, 111, 0]); // garbage
        let a = q.recv().await.unwrap();
        assert_eq!((a.seq, a.timestamp, a.lost_before), (0, 0, 0));
        assert_eq!(a.payload_type, 111);
        assert_eq!(a.payload, vec![0x28, 1, 2, 3]);
        let b = q.recv().await.unwrap();
        assert_eq!((b.seq, b.timestamp, b.lost_before), (1, 3 * 960, 2));
        assert!(q.try_recv().is_none());
        let c = shared.counters.snapshot();
        assert_eq!(c.rtp_packets, 3);
        assert_eq!(c.rtp_foreign, 1);
        assert_eq!(c.rtp_garbage, 1);
        shared.release(ssrc);
        assert_eq!(q.recv().await, None);
    }

    #[tokio::test]
    async fn queue_drops_oldest_and_closes() {
        let q = DropOldestQueue::new(2);
        q.push(1);
        q.push(2);
        q.push(3);
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.recv().await, Some(2));
        assert_eq!(q.try_recv(), Some(3));
        q.close();
        q.push(4);
        assert_eq!(q.recv().await, None);
        assert_eq!(q.dropped(), 1);
    }

    #[tokio::test]
    async fn queue_recv_wakes_on_push() {
        let q = Arc::new(DropOldestQueue::new(4));
        let q2 = Arc::clone(&q);
        let h = tokio::spawn(async move { q2.recv().await });
        tokio::task::yield_now().await;
        q.push(9u8);
        assert_eq!(h.await.unwrap(), Some(9));
    }

    #[test]
    fn tags_skip_zero() {
        let shared = Shared::new();
        shared.lock().next_tag = u32::MAX;
        assert_eq!(shared.next_tag(), u32::MAX);
        assert_eq!(shared.next_tag(), 1);
    }

    proptest! {
        #[test]
        fn status_handler_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let shared = Shared::new();
            shared.handle_status_packet(&bytes);
            let mut status = vec![0u8];
            status.extend_from_slice(&bytes);
            shared.handle_status_packet(&status);
        }

        #[test]
        fn rtp_handler_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let shared = Shared::new();
            let q = Arc::new(DropOldestQueue::new(4));
            let ssrc = shared.allocate_ssrc(Some(q)).unwrap();
            shared.handle_rtp_packet(&bytes);
            let mut pkt = rtp(ssrc, 1, 1);
            pkt.truncate(12);
            pkt.extend_from_slice(&bytes);
            shared.handle_rtp_packet(&pkt);
        }
    }
}
