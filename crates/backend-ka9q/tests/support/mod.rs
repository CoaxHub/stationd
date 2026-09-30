// SPDX-License-Identifier: GPL-3.0-only
//! A fake radiod for hermetic client tests.
//!
//! It listens for `CMD` packets on a unicast 127.0.0.1 port and models the
//! parts of radiod's behaviour the client depends on
//! (`docs/radiod-notes.md`):
//!
//! - any command to an unknown SSRC creates a channel from the template
//!   (linear, `am`, 0 Hz, PCM, `LIFETIME` 0 = immortal);
//! - SSRC 0 is ignored; SSRC `0xffffffff` sends every channel's status;
//! - each channel queues at most two commands and silently drops the rest;
//!   one queued command is executed per 20 ms frame;
//! - every executed command resets the lifetime countdown, which is
//!   decremented once per frame; the channel is removed when it hits 0;
//! - `PRESET` is applied first, the other TLVs in order;
//! - the status reply echoes `COMMAND_TAG` and goes to the command's source
//!   address (the unicast stand-in for the status group);
//! - a spectrum2 channel's first reply has no bin data; later replies carry
//!   `BIN_BYTE_DATA` with a fresh `SPECTRUM_BASE`/`SPECTRUM_STEP` each time;
//! - changing `BIN_COUNT` on a live spectrum2 channel "crashes" the fake
//!   (it stops answering), as it crashes radiod;
//! - an audio (linear, FM or WFM) Opus channel with a non-zero frequency
//!   sends one RTP packet per frame (PT 111, +1 sequence, +960 timestamp),
//!   with optional loss, duplication and reordering;
//! - audio channels also send unsolicited status every 25 frames (0.5 s) to
//!   [`FakeState::status_dest`], the unicast stand-in for the data group's
//!   port 5006;
//! - presets set the demodulator, sample rate and passband (`wfm` always
//!   outputs 48 kHz, like `wfm.c`), and every status carries the front-end
//!   fields of the signal-generator config (24 MS/s real, 0 to 12 MHz);
//! - `FIRST_LO_FREQUENCY` retunes the front end only when
//!   [`FakeState::fe_tunable`] is set, as radiod ignores it for a locked or
//!   fixed tuner.
//!
//! It also records what the client did, so tests can assert on it: every
//! command, the deepest per-SSRC queue ever seen (the client must never have
//! more than one command in flight), dropped commands and removed channels.

#![allow(dead_code, reason = "each test binary uses a different subset")]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use backend_ka9q::tags::{PktType, StatusType};
use backend_ka9q::tlv::{self, Encoder};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

/// radiod's frame (block) duration.
pub const FRAME: Duration = Duration::from_millis(20);
/// Per-channel command queue depth (`CQLEN` in radio.h).
pub const QUEUE_DEPTH: usize = 2;
/// Input sample rate reported in status (24 MS/s like the `sig_gen` config).
pub const INPUT_SAMPRATE: u32 = 24_000_000;
/// Frames between unsolicited status packets of an audio channel.
pub const STATUS_INTERVAL_FRAMES: u32 = 25;
/// The reported `BASEBAND_POWER` of audio channels, dB.
pub const BASEBAND_POWER_DB: f64 = -19.5;
/// The reported `NOISE_DENSITY`, dB/Hz.
pub const NOISE_DENSITY_DB: f64 = -91.5;
/// The reported `IF_POWER`, dBFS.
pub const IF_POWER_DB: f64 = -20.4;

/// One command as the fake received it.
#[derive(Debug, Clone, PartialEq)]
pub struct CmdRecord {
    pub ssrc: u32,
    pub tag: Option<u64>,
    pub lifetime: Option<u32>,
    pub bin_count: Option<u32>,
    pub frequency: Option<f64>,
}

/// One channel's state.
#[derive(Debug, Clone)]
pub struct FakeChannel {
    pub demod: i32,
    pub preset: String,
    pub frequency: f64,
    pub low_edge: f64,
    pub high_edge: f64,
    pub samprate: u32,
    pub encoding: i32,
    pub opus_bitrate: i32,
    pub output_channels: u32,
    pub snr_squelch: bool,
    pub squelch_open: f64,
    pub squelch_close: f64,
    pub bin_count: u32,
    pub rbw: f64,
    pub avg: u32,
    pub lifetime_set: u32,
    pub lifetime_left: u32,
    pub cmd_cnt: u64,
    pub last_tag: u64,
    pub spectrum_ready: bool,
    pub polls: u32,
    pub rtp_seq: u16,
    pub rtp_ts: u32,
    pub rtp_sent: u64,
    /// Demodulator restarts caused by commands (a sample-rate or demod change
    /// on a live channel), as radiod's `decode_radio_commands` reports them.
    pub restarts: u32,
    held_rtp: Option<Vec<u8>>,
    queue: VecDeque<(Vec<u8>, SocketAddr)>,
}

impl FakeChannel {
    fn template(ssrc: u32) -> Self {
        Self {
            demod: 0,
            preset: "am".into(),
            frequency: 0.0,
            low_edge: -5000.0,
            high_edge: 5000.0,
            samprate: 12_000,
            encoding: 2, // S16BE
            opus_bitrate: 0,
            output_channels: 1,
            snr_squelch: false,
            squelch_open: 8.0,
            squelch_close: 7.0,
            bin_count: 0,
            rbw: 0.0,
            avg: 10,
            lifetime_set: 0,
            lifetime_left: 0,
            cmd_cnt: 0,
            last_tag: 0,
            spectrum_ready: false,
            polls: 0,
            rtp_seq: u16::try_from(ssrc & 0xfff).unwrap_or(0).wrapping_mul(7),
            rtp_ts: ssrc.wrapping_mul(3),
            rtp_sent: 0,
            restarts: 0,
            held_rtp: None,
            queue: VecDeque::new(),
        }
    }
}

/// Everything the fake knows and records.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools, reason = "independent test switches")]
pub struct FakeState {
    pub channels: BTreeMap<u32, FakeChannel>,
    pub rtp_dest: Option<SocketAddr>,
    pub log: Vec<CmdRecord>,
    pub max_queue_depth: HashMap<u32, usize>,
    pub dropped_commands: u64,
    pub invalid_packets: u64,
    pub removed: Vec<u32>,
    pub created: Vec<u32>,
    /// Swallow the next N replies (the commands are still executed).
    pub drop_replies: u32,
    /// Execute commands but never reply.
    pub mute: bool,
    /// Ignore `RADIO_FREQUENCY` (to provoke a "rejected" open).
    pub ignore_frequency: bool,
    /// Set when the client changed `BIN_COUNT` on a live spectrum channel.
    pub crashed: bool,
    /// Drop every Nth RTP packet (0 = never). The sequence still advances.
    pub rtp_drop_every: u16,
    /// Send every Nth RTP packet twice (0 = never).
    pub rtp_duplicate_every: u16,
    /// Swap every Nth RTP packet with the next one (0 = never).
    pub rtp_swap_every: u16,
    /// Where audio channels send unsolicited status (none if `None`).
    pub status_dest: Option<SocketAddr>,
    /// `OUTPUT_DATA_DEST_SOCKET` reported in status (none if `None`).
    pub data_dest: Option<SocketAddr>,
    /// Front end: `FIRST_LO_FREQUENCY`, Hz.
    pub fe_lo: f64,
    /// Front end: `FE_LOW_EDGE` and `FE_HIGH_EDGE`, Hz (relative to the LO).
    pub fe_low: f64,
    pub fe_high: f64,
    /// Front end: `FE_ISREAL`.
    pub fe_isreal: bool,
    /// Whether `FIRST_LO_FREQUENCY` commands retune the front end.
    pub fe_tunable: bool,
    /// Front end: `AD_OVER`, A/D overrange count.
    pub ad_over: u64,
    /// Frames since start (for the unsolicited status cadence).
    pub frames: u64,
}

impl FakeState {
    fn new() -> Self {
        Self {
            fe_high: f64::from(INPUT_SAMPRATE / 2),
            fe_isreal: true,
            ..Self::default()
        }
    }
}

impl FakeState {
    /// Commands whose SSRC is not the all-channels SSRC.
    pub fn channel_commands(&self) -> impl Iterator<Item = &CmdRecord> {
        self.log.iter().filter(|c| c.ssrc != u32::MAX)
    }
}

/// A running fake radiod.
#[derive(Debug)]
pub struct FakeRadiod {
    pub addr: SocketAddr,
    state: Arc<Mutex<FakeState>>,
    socket: Arc<UdpSocket>,
    task: JoinHandle<()>,
}

impl Drop for FakeRadiod {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeRadiod {
    pub async fn start() -> Self {
        let socket = Arc::new(
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind fake radiod"),
        );
        let addr = socket.local_addr().expect("fake address");
        let state = Arc::new(Mutex::new(FakeState::new()));
        let task = tokio::spawn(run(Arc::clone(&socket), Arc::clone(&state)));
        Self {
            addr,
            state,
            socket,
            task,
        }
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn set_rtp_dest(&self, dest: SocketAddr) {
        self.state().rtp_dest = Some(dest);
    }

    /// Where audio channels send their unsolicited status.
    pub fn set_status_dest(&self, dest: SocketAddr) {
        self.state().status_dest = Some(dest);
    }

    /// Removes a channel as if its lifetime had run out.
    pub fn expire(&self, ssrc: u32) {
        let mut st = self.state();
        st.channels.remove(&ssrc);
        st.removed.push(ssrc);
    }

    pub fn has_channel(&self, ssrc: u32) -> bool {
        self.state().channels.contains_key(&ssrc)
    }

    pub fn channel(&self, ssrc: u32) -> Option<FakeChannel> {
        self.state().channels.get(&ssrc).cloned()
    }

    /// Sends raw bytes from the fake's socket (e.g. garbage to the client).
    pub async fn send_raw(&self, bytes: &[u8], to: SocketAddr) {
        self.socket.send_to(bytes, to).await.expect("send raw");
    }

    /// Polls `cond` every frame until it holds or `timeout` passes.
    pub async fn wait_until(&self, timeout: Duration, cond: impl Fn(&FakeState) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond(&self.state()) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(FRAME).await;
        }
    }

    /// Asserts the client's contract on everything it sent so far: every
    /// channel command carried `LIFETIME`, at most one command per SSRC was
    /// ever queued, nothing was dropped and the fake did not crash.
    pub fn assert_client_contract(&self) {
        let st = self.state();
        for c in st.channel_commands() {
            assert!(c.lifetime.is_some(), "command without LIFETIME: {c:?}");
            assert_ne!(c.lifetime, Some(0), "immortal channel requested: {c:?}");
        }
        for (ssrc, depth) in &st.max_queue_depth {
            assert!(*depth <= 1, "SSRC {ssrc} had {depth} commands in flight");
        }
        assert_eq!(st.dropped_commands, 0, "commands were dropped");
        assert!(
            !st.crashed,
            "the client changed BIN_COUNT on a live channel"
        );
    }
}

async fn run(socket: Arc<UdpSocket>, state: Arc<Mutex<FakeState>>) {
    let mut ticker = tokio::time::interval(FRAME);
    let mut buf = vec![0u8; 65_536];
    loop {
        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                if let Ok((n, from)) = r {
                    let out = receive(&state, &buf[..n], from);
                    for (bytes, to) in out {
                        let _ = socket.send_to(&bytes, to).await;
                    }
                }
            }
            _ = ticker.tick() => {
                let out = frame(&state);
                for (bytes, to) in out {
                    let _ = socket.send_to(&bytes, to).await;
                }
            }
        }
    }
}

fn lock(state: &Mutex<FakeState>) -> MutexGuard<'_, FakeState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

type Outgoing = Vec<(Vec<u8>, SocketAddr)>;

/// Handles one incoming datagram; returns immediate replies (dump-all).
fn receive(state: &Mutex<FakeState>, bytes: &[u8], from: SocketAddr) -> Outgoing {
    let mut st = lock(state);
    if st.crashed {
        return Vec::new();
    }
    let Ok(pkt) = tlv::parse(bytes) else {
        st.invalid_packets += 1;
        return Vec::new();
    };
    if pkt.kind != PktType::Cmd || bytes.len() < 3 {
        st.invalid_packets += 1;
        return Vec::new();
    }
    let Some(ssrc) = pkt.ssrc().filter(|&s| s != 0) else {
        st.invalid_packets += 1;
        return Vec::new();
    };
    let get_u32 = |t| pkt.last(t).and_then(|f| f.as_u32().ok());
    st.log.push(CmdRecord {
        ssrc,
        tag: pkt.command_tag(),
        lifetime: get_u32(StatusType::Lifetime),
        bin_count: get_u32(StatusType::BinCount),
        frequency: pkt
            .last(StatusType::RadioFrequency)
            .and_then(|f| f.as_f64().ok()),
    });
    if ssrc == u32::MAX {
        return st
            .channels
            .iter()
            .map(|(&s, ch)| (status_packet(&st, s, ch, false), from))
            .collect();
    }
    let mut created = false;
    let ch = st.channels.entry(ssrc).or_insert_with(|| {
        created = true;
        FakeChannel::template(ssrc)
    });
    let full = ch.queue.len() >= QUEUE_DEPTH;
    if !full {
        ch.queue.push_back((bytes.to_vec(), from));
    }
    let depth = ch.queue.len();
    if created {
        st.created.push(ssrc);
    }
    if full {
        st.dropped_commands += 1;
        return Vec::new();
    }
    let max = st.max_queue_depth.entry(ssrc).or_default();
    *max = (*max).max(depth);
    Vec::new()
}

/// One frame: execute one queued command per channel, count lifetimes down,
/// send RTP.
fn frame(state: &Mutex<FakeState>) -> Outgoing {
    let mut st = lock(state);
    if st.crashed {
        return Vec::new();
    }
    let mut out = Vec::new();
    st.frames += 1;
    let periodic = st.frames % u64::from(STATUS_INTERVAL_FRAMES) == 0;
    let ssrcs: Vec<u32> = st.channels.keys().copied().collect();
    for ssrc in ssrcs {
        let Some(mut ch) = st.channels.remove(&ssrc) else {
            continue;
        };
        if let Some((cmd, from)) = ch.queue.pop_front() {
            if !apply(&mut st, &mut ch, &cmd) {
                st.crashed = true;
                return Vec::new();
            }
            let with_bins = ch.demod == 4 && ch.spectrum_ready;
            let reply = status_packet(&st, ssrc, &ch, with_bins);
            if ch.demod == 4 {
                ch.spectrum_ready = true;
                ch.polls += 1;
            }
            if st.mute {
                // Executed, never answered.
            } else if st.drop_replies > 0 {
                st.drop_replies -= 1;
            } else {
                out.push((reply, from));
            }
        }
        if ch.lifetime_left > 0 {
            ch.lifetime_left -= 1;
            if ch.lifetime_left == 0 {
                st.removed.push(ssrc);
                continue;
            }
        }
        let audio = ch.demod <= 2 && ch.encoding == 3 && ch.frequency != 0.0;
        if let Some(dest) = st.rtp_dest.filter(|_| audio) {
            send_rtp(&st, ssrc, &mut ch, dest, &mut out);
        }
        if let Some(dest) = st.status_dest.filter(|_| periodic && ch.demod <= 2) {
            out.push((status_packet(&st, ssrc, &ch, false), dest));
        }
        st.channels.insert(ssrc, ch);
    }
    out
}

fn send_rtp(st: &FakeState, ssrc: u32, ch: &mut FakeChannel, dest: SocketAddr, out: &mut Outgoing) {
    let seq = ch.rtp_seq;
    ch.rtp_seq = seq.wrapping_add(1);
    let ts = ch.rtp_ts;
    ch.rtp_ts = ts.wrapping_add(960);
    ch.rtp_sent += 1;
    let n = ch.rtp_sent;
    let every = |k: u16| k != 0 && n % u64::from(k) == 0;
    if every(st.rtp_drop_every) {
        return;
    }
    let mut pkt = vec![0x80, 111];
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&ts.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    // TOC 0x28: SILK MB 20 ms mono, code 0; then a recognisable body.
    pkt.extend_from_slice(&[0x28, ssrc.to_be_bytes()[3], seq.to_be_bytes()[1], 0xaa]);
    if every(st.rtp_swap_every) {
        ch.held_rtp = Some(pkt);
        return;
    }
    if every(st.rtp_duplicate_every) {
        out.push((pkt.clone(), dest));
    }
    out.push((pkt, dest));
    if let Some(held) = ch.held_rtp.take() {
        out.push((held, dest));
    }
}

/// Applies one command. Returns false if it would crash radiod.
fn apply(st: &mut FakeState, ch: &mut FakeChannel, cmd: &[u8]) -> bool {
    let Ok(pkt) = tlv::parse(cmd) else {
        return true;
    };
    // First pass: PRESET.
    if let Some(p) = pkt.last(StatusType::Preset) {
        let name = p.as_string_lossy().into_owned();
        // (demod, samprate, low, high) of the pinned presets.conf.
        let (demod, samprate, low, high) = match name.as_str() {
            "fm" => (1, 24_000, -8000.0, 8000.0),
            "nfm" => (1, 24_000, -6250.0, 6250.0),
            "wfm" => (2, 48_000, -110_000.0, 110_000.0),
            "usb" => (0, 12_000, 50.0, 3000.0),
            "lsb" => (0, 12_000, -3000.0, -50.0),
            "cwu" => (0, 12_000, -200.0, 200.0),
            _ => (0, 12_000, -5000.0, 5000.0),
        };
        ch.demod = demod;
        ch.samprate = samprate;
        ch.low_edge = low;
        ch.high_edge = high;
        ch.output_channels = 1;
        ch.preset = name;
    }
    let was_spectrum = ch.demod == 4;
    let (old_samprate, old_demod) = (ch.samprate, ch.demod);
    for f in pkt.fields() {
        let Some(tag) = f.status_type() else {
            continue;
        };
        match tag {
            StatusType::DemodType => {
                let d = f.as_i32().unwrap_or(0);
                if d == 4 && ch.demod != 4 {
                    ch.spectrum_ready = false;
                }
                ch.demod = d;
            }
            StatusType::RadioFrequency if !st.ignore_frequency => {
                ch.frequency = f.as_f64().unwrap_or(0.0);
            }
            StatusType::LowEdge => ch.low_edge = f.as_f32_or_f64().unwrap_or(0.0),
            StatusType::HighEdge => ch.high_edge = f.as_f32_or_f64().unwrap_or(0.0),
            StatusType::OutputSamprate => ch.samprate = f.as_u32().unwrap_or(0),
            StatusType::OutputEncoding => ch.encoding = f.as_i32().unwrap_or(0),
            StatusType::OpusBitRate => ch.opus_bitrate = f.as_i32().unwrap_or(0),
            StatusType::OutputChannels => {
                let n = f.as_u32().unwrap_or(0);
                if n == 1 || n == 2 {
                    ch.output_channels = n;
                }
            }
            StatusType::SnrSquelch => ch.snr_squelch = f.as_bool().unwrap_or(false),
            StatusType::SquelchOpen => ch.squelch_open = f.as_f32_or_f64().unwrap_or(0.0),
            StatusType::SquelchClose => ch.squelch_close = f.as_f32_or_f64().unwrap_or(0.0),
            StatusType::FirstLoFrequency if st.fe_tunable => {
                st.fe_lo = f.as_f64().unwrap_or(0.0).abs();
            }
            StatusType::BinCount => {
                let n = f.as_u32().unwrap_or(0);
                if was_spectrum && ch.demod == 4 && n != ch.bin_count {
                    return false;
                }
                ch.bin_count = n;
            }
            StatusType::ResolutionBw => ch.rbw = f.as_f32_or_f64().unwrap_or(0.0),
            StatusType::SpectrumAvg => ch.avg = f.as_u32().unwrap_or(0),
            StatusType::Lifetime => ch.lifetime_set = f.as_u32().unwrap_or(0),
            _ => {}
        }
    }
    // WFM always outputs 48 kHz whatever was requested (radiod wfm.c).
    if ch.preset == "wfm" {
        ch.samprate = 48_000;
    }
    if ch.samprate != old_samprate || ch.demod != old_demod {
        ch.restarts += 1;
    }
    ch.cmd_cnt += 1;
    ch.last_tag = pkt.command_tag().unwrap_or(0);
    ch.lifetime_left = ch.lifetime_set;
    true
}

/// The spectrum row of poll `n`: a carrier in the center bin, AM sidebands
/// next to it, a noise floor that changes with `n`.
pub fn fake_bins(bin_count: u32, n: u32) -> Vec<u8> {
    let count = usize::try_from(bin_count).unwrap_or(0);
    let center = count / 2;
    (0..count)
        .map(|i| {
            if i == center {
                255
            } else if i + 1 == center || i == center + 1 {
                180
            } else {
                u8::try_from((i * 7 + n as usize) % 20 + 5).unwrap_or(0)
            }
        })
        .collect()
}

/// `SPECTRUM_BASE`/`SPECTRUM_STEP` of poll `n`: they change every poll,
/// like radiod's auto-scaling.
pub fn fake_scale(n: u32) -> (f32, f32) {
    #[allow(clippy::cast_precision_loss)]
    let base = -100.0 - (n % 4) as f32;
    #[allow(clippy::cast_precision_loss)]
    let step = 0.25 + 0.05 * (n % 3) as f32;
    (base, step)
}

fn status_packet(st: &FakeState, ssrc: u32, ch: &FakeChannel, with_bins: bool) -> Vec<u8> {
    let mut e = Encoder::new(PktType::Status)
        .u32(StatusType::OutputSsrc, ssrc)
        .u64(StatusType::CommandTag, ch.last_tag)
        .u64(StatusType::CmdCnt, ch.cmd_cnt)
        .str(StatusType::Description, "fake radiod")
        .u32(StatusType::Lifetime, ch.lifetime_left)
        .u32(StatusType::InputSamprate, INPUT_SAMPRATE)
        .bool(StatusType::FeIsreal, st.fe_isreal)
        .f32(StatusType::FeLowEdge, st.fe_low)
        .f32(StatusType::FeHighEdge, st.fe_high)
        .f64(StatusType::FirstLoFrequency, st.fe_lo)
        .f32(StatusType::IfPower, IF_POWER_DB)
        .u64(StatusType::AdOver, st.ad_over)
        .u32(StatusType::FilterBlocksize, INPUT_SAMPRATE / 50)
        .f64(StatusType::RadioFrequency, ch.frequency)
        .i32(StatusType::DemodType, ch.demod)
        .str(StatusType::Preset, &ch.preset);
    if let Some(dest) = st.data_dest {
        e = e.socket(StatusType::OutputDataDestSocket, dest);
    }
    if ch.demod == 4 {
        e = e
            .u32(StatusType::BinCount, ch.bin_count)
            .f32(StatusType::ResolutionBw, ch.rbw)
            .u32(StatusType::SpectrumAvg, ch.avg);
        if with_bins {
            let (base, step) = fake_scale(ch.polls);
            e = e
                .f32(StatusType::SpectrumBase, f64::from(base))
                .f32(StatusType::SpectrumStep, f64::from(step))
                .bytes(StatusType::BinByteData, &fake_bins(ch.bin_count, ch.polls));
        }
    } else {
        e = e
            .f32(StatusType::LowEdge, ch.low_edge)
            .f32(StatusType::HighEdge, ch.high_edge)
            .u32(StatusType::OutputSamprate, ch.samprate)
            .u32(StatusType::OutputChannels, ch.output_channels)
            .i32(StatusType::OutputEncoding, ch.encoding)
            .byte(StatusType::RtpPt, 111)
            .i32(StatusType::OpusBitRate, ch.opus_bitrate)
            .f32(StatusType::BasebandPower, BASEBAND_POWER_DB)
            .f32(StatusType::NoiseDensity, NOISE_DENSITY_DB)
            .bool(StatusType::SnrSquelch, ch.snr_squelch)
            .f32(StatusType::SquelchOpen, ch.squelch_open)
            .f32(StatusType::SquelchClose, ch.squelch_close);
    }
    e.finish()
}
