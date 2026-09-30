// SPDX-License-Identifier: GPL-3.0-only
//! `ka9q-spike`: the station spike. Drives a local radiod through
//! `backend_ka9q::client` to
//!
//! - record an Opus channel to a playable `.opus` file (`audio`),
//! - dump spectrum2 rows as our quantized, zstd-compressed waterfall rows
//!   (`waterfall`, file format in [`wfstream`]),
//! - print radiod's channel and front-end status (`status`),
//!
//! and with `--report` measures the CPU used by the spike and by radiod.
//! Every command prints a final `RESULT key=value ...` line for scripts.

mod cpu;
/// The hermetic fake radiod of the backend's integration tests.
#[cfg(test)]
#[path = "../../backend-ka9q/tests/support/mod.rs"]
mod fake_radiod;
mod ogg;
mod wfstream;

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use backend_ka9q::client::{
    AudioChannelParams, ClientConfig, Ka9qClient, MAX_BIN_COUNT, SpectrumParams,
};
use backend_ka9q::spectrum::{DEFAULT_QUANTIZATION, Quantization, compress_row, requantize};
use backend_ka9q::status::ChannelStatus;
use clap::{Args, Parser, Subcommand};
use tokio::time::Instant;

use crate::ogg::{OggOpusWriter, OpusHeadParams};

/// radiod's RTP payload type for Opus.
const OPUS_PT: u8 = 111;
/// Our waterfall quantization for the spike: the default scale, in dB/Hz
/// (station-protocol §6.2).
const WATERFALL_Q: Quantization = DEFAULT_QUANTIZATION;
/// Longest gap filled with packet-loss-concealment frames, in packets.
const MAX_PLC_FILL: u32 = 50;
/// Opus pre-skip written to the Ogg header (see `ogg::OpusHeadParams`).
const PRE_SKIP: u16 = 312;
/// Longest run accepted for `--secs`.
const MAX_SECS: f64 = 86_400.0;

/// Drive a local ka9q-radio radiod: record Opus audio, dump waterfall rows,
/// show status.
#[derive(Debug, Parser)]
#[command(name = "ka9q-spike", version, about)]
struct Cli {
    /// radiod control/status group (its `status =` address), port 5006.
    #[arg(long, global = true, default_value = "239.77.0.6:5006")]
    status_addr: SocketAddr,
    /// radiod RTP data group (its `data =` address), port 5004.
    #[arg(long, global = true, default_value = "239.77.0.4:5004")]
    data_addr: SocketAddr,
    /// Local interface for multicast membership and sends.
    #[arg(long, global = true, default_value = "127.0.0.1")]
    interface: Ipv4Addr,
    /// Multicast TTL for commands (0 = this host only).
    #[arg(long, global = true, default_value_t = 0)]
    ttl: u32,
    /// Channel LIFETIME in 20 ms frames, sent with every command.
    #[arg(long, global = true, default_value_t = 500, value_parser = clap::value_parser!(u32).range(2..))]
    lifetime_frames: u32,
    /// Status reply timeout per command attempt, ms.
    #[arg(long, global = true, default_value_t = 250, value_parser = clap::value_parser!(u64).range(1..=10_000))]
    timeout_ms: u64,
    /// Measure CPU use of the spike and of radiod over the run.
    #[arg(long, global = true)]
    report: bool,
    /// radiod pid for `--report` (default: from run-radiod-siggen.sh's pidfile).
    #[arg(long, global = true)]
    radiod_pid: Option<u32>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Record an Opus channel to an Ogg Opus file.
    Audio(AudioArgs),
    /// Poll spectrum rows and write them as compressed waterfall rows.
    Waterfall(WaterfallArgs),
    /// Print the status of radiod's channels and front end.
    Status(StatusArgs),
    /// Summarise a row stream written by `waterfall --out` (no radiod needed).
    Inspect(InspectArgs),
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// Row stream file.
    file: PathBuf,
}

#[derive(Debug, Args)]
struct AudioArgs {
    /// Frequency, Hz (e.g. 10e6).
    #[arg(long, value_parser = parse_hz)]
    freq: f64,
    /// radiod preset (mode), e.g. am, usb, lsb, cw, fm.
    #[arg(long, default_value = "am")]
    mode: String,
    /// Recording length, seconds.
    #[arg(long, default_value_t = 10.0, value_parser = parse_secs)]
    secs: f64,
    /// Opus bit rate, bit/s (0 = encoder default).
    #[arg(long)]
    bitrate: Option<u32>,
    /// Output sample rate, Hz (8000, 12000, 16000, 24000 or 48000).
    #[arg(long)]
    samprate: Option<u32>,
    /// Passband low edge, Hz relative to the frequency.
    #[arg(long, allow_hyphen_values = true)]
    low: Option<f32>,
    /// Passband high edge, Hz relative to the frequency.
    #[arg(long, allow_hyphen_values = true)]
    high: Option<f32>,
    /// Output .opus file.
    #[arg(long)]
    out: PathBuf,
}

#[derive(Debug, Args)]
struct WaterfallArgs {
    /// Center frequency, Hz.
    #[arg(long, value_parser = parse_hz)]
    center: f64,
    /// Bins per row.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_BIN_COUNT)))]
    bins: u32,
    /// Bin width (resolution bandwidth), Hz.
    #[arg(long, default_value_t = 1000.0, value_parser = parse_rbw)]
    rbw: f32,
    /// FFTs averaged per row (radiod default when omitted).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    avg: Option<u32>,
    /// Rows per second.
    #[arg(long, default_value_t = 10.0, value_parser = parse_rate)]
    rate: f64,
    /// Run length, seconds.
    #[arg(long, default_value_t = 10.0, value_parser = parse_secs)]
    secs: f64,
    /// zstd level per row.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(i32).range(1..=19))]
    zstd_level: i32,
    /// Row stream output (format: see the wfstream module).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Grayscale PGM image output.
    #[arg(long)]
    pgm: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// How long to collect replies to the all-channels request, ms.
    #[arg(long, default_value_t = 500)]
    window_ms: u64,
    /// Also create a short-lived probe channel for front-end status (starts
    /// the front end if no channel is running). Done automatically when
    /// radiod has no channels.
    #[arg(long)]
    probe: bool,
}

fn parse_positive(s: &str, what: &str, max: f64) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("{what} must be a number, got {s:?}"))?;
    if v.is_finite() && v > 0.0 && v <= max {
        Ok(v)
    } else {
        Err(format!("{what} must be > 0 and <= {max}, got {s}"))
    }
}

fn parse_hz(s: &str) -> Result<f64, String> {
    parse_positive(s, "frequency", 1e12)
}

fn parse_secs(s: &str) -> Result<f64, String> {
    parse_positive(s, "seconds", MAX_SECS)
}

fn parse_rate(s: &str) -> Result<f64, String> {
    parse_positive(s, "rate", 50.0)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "bounded to 1e7 before the cast"
)]
fn parse_rbw(s: &str) -> Result<f32, String> {
    parse_positive(s, "rbw", 1e7).map(|v| v as f32)
}

impl Cli {
    fn client_config(&self) -> ClientConfig {
        ClientConfig {
            status_addr: self.status_addr,
            data_addr: self.data_addr,
            interface: self.interface,
            ttl: self.ttl,
            command_timeout: Duration::from_millis(self.timeout_ms),
            lifetime_frames: self.lifetime_frames,
            ..ClientConfig::default()
        }
    }
}

impl AudioArgs {
    fn params(&self) -> AudioChannelParams {
        AudioChannelParams {
            frequency_hz: self.freq,
            preset: Some(self.mode.clone()),
            low_edge_hz: self.low,
            high_edge_hz: self.high,
            samprate: self.samprate,
            opus_bitrate: self.bitrate,
            output_channels: None,
            squelch: None,
        }
    }
}

impl WaterfallArgs {
    fn params(&self) -> SpectrumParams {
        SpectrumParams {
            center_hz: self.center,
            bin_count: self.bins,
            resolution_bw_hz: self.rbw,
            avg: self.avg,
            row_interval: Duration::from_secs_f64(1.0 / self.rate),
        }
    }
}

/// `RESULT` line fields, in order.
type Fields = Vec<(&'static str, String)>;

fn result_line(fields: &Fields) -> String {
    let mut s = String::from("RESULT");
    for (k, v) in fields {
        let _ = write!(s, " {k}={v}");
    }
    s
}

/// Sleeps until `deadline`, or returns early on Ctrl-C.
async fn until(deadline: Instant) {
    tokio::select! {
        () = tokio::time::sleep_until(deadline) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "byte and packet counts stay far below 2^52"
)]
fn per_sec(n: u64, secs: f64) -> f64 {
    if secs > 0.0 { n as f64 / secs } else { 0.0 }
}

/// `a / b`, 0 when `b` is 0.
#[allow(
    clippy::cast_precision_loss,
    reason = "byte and row counts stay far below 2^52"
)]
fn ratio(a: u64, b: u64) -> f64 {
    if b > 0 { a as f64 / b as f64 } else { 0.0 }
}

async fn run_audio(client: &Ka9qClient, args: &AudioArgs) -> Result<Fields> {
    let params = args.params();
    params.validate()?;
    let file =
        File::create(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    let mut ch = client.open_audio_channel(params).await?;
    let ssrc = ch.ssrc();
    let opened = ch.status();
    let rate = opened
        .as_ref()
        .and_then(|s| s.output_samprate)
        .or(args.samprate)
        .unwrap_or(48_000);
    // radiod's Opus is mono except for stereo presets such as `wfm`.
    let channels = opened
        .as_ref()
        .and_then(|s| s.output_channels)
        .map_or(1, |c| if c == 2 { 2 } else { 1 });
    eprintln!(
        "opened audio channel SSRC {ssrc}: {} Hz {} at {rate} Hz, {channels} channel(s)",
        args.freq, args.mode
    );
    let mut ogg = OggOpusWriter::new(
        BufWriter::new(file),
        ssrc,
        OpusHeadParams {
            channels,
            pre_skip: PRE_SKIP,
            input_sample_rate: rate,
            output_gain: 0,
        },
    )?;

    let start = Instant::now();
    let deadline = start + Duration::from_secs_f64(args.secs);
    let stop = until(deadline);
    tokio::pin!(stop);
    let (mut packets, mut payload_bytes, mut plc, mut other_pt) = (0u64, 0u64, 0u64, 0u64);
    loop {
        let pkt = tokio::select! {
            () = &mut stop => break,
            p = ch.recv() => p,
        };
        let Some(pkt) = pkt else { break };
        if pkt.payload_type != OPUS_PT || pkt.payload.is_empty() {
            other_pt += 1;
            continue;
        }
        // Lost packets become empty frames of the same Opus configuration
        // (TOC only, code 0): the decoder conceals them and the file keeps
        // its timing.
        let toc = [pkt.payload[0] & !0x03];
        for _ in 0..pkt.lost_before.min(MAX_PLC_FILL) {
            ogg.write_packet(&toc)?;
            plc += 1;
        }
        ogg.write_packet(&pkt.payload)?;
        packets += 1;
        payload_bytes += pkt.payload.len() as u64;
    }
    let secs = start.elapsed().as_secs_f64();
    let stats = ch.stats();
    ch.close().await?;
    ogg.finish()?.flush()?;

    let payload_rate = per_sec(payload_bytes, secs);
    // RTP (12) + UDP (8) + IPv4 (20) per packet, for comparison.
    let wire_rate = per_sec(payload_bytes + packets * 40, secs);
    let fields = vec![
        ("cmd", "audio".to_owned()),
        ("ssrc", ssrc.to_string()),
        ("secs", format!("{secs:.2}")),
        ("packets", packets.to_string()),
        ("packets_per_s", format!("{:.1}", per_sec(packets, secs))),
        ("lost", stats.lost.to_string()),
        ("duplicates_or_late", stats.duplicates_or_late.to_string()),
        ("queue_drops", stats.queue_drops.to_string()),
        ("plc_frames", plc.to_string()),
        ("non_opus", other_pt.to_string()),
        ("keepalives", stats.keepalives.to_string()),
        ("payload_bytes", payload_bytes.to_string()),
        ("payload_bytes_per_s", format!("{payload_rate:.0}")),
        (
            "payload_kbit_per_s",
            format!("{:.1}", payload_rate * 8.0 / 1000.0),
        ),
        ("rtp_udp_ip_bytes_per_s", format!("{wire_rate:.0}")),
        ("channels", channels.to_string()),
        ("out", args.out.display().to_string()),
    ];
    Ok(fields)
}

/// The strongest bin of a row. When several bins share the maximum (a
/// strong carrier and its leakage or AM sidebands clip at 255 in our
/// quantization), the middle one of them.
fn peak_bin(row: &[u8]) -> Option<usize> {
    let max = *row.iter().max()?;
    let at_max: Vec<usize> = row
        .iter()
        .enumerate()
        .filter(|&(_, &v)| v == max)
        .map(|(i, _)| i)
        .collect();
    at_max.get(at_max.len() / 2).copied()
}

/// Row statistics shared by `waterfall` and `inspect`.
#[derive(Debug, Default)]
struct RowStats {
    rows: u64,
    compressed_bytes: u64,
    peak_votes: std::collections::BTreeMap<usize, u32>,
    first: Option<Duration>,
    last: Option<Duration>,
    /// Bins at 255 (clipped by our quantization), summed over rows.
    saturated: u64,
}

impl RowStats {
    fn add(&mut self, t: Duration, row: &[u8], compressed_len: usize) {
        self.rows += 1;
        self.compressed_bytes += compressed_len as u64;
        self.first.get_or_insert(t);
        self.last = Some(t);
        self.saturated += row.iter().map(|&v| u64::from(v == u8::MAX)).sum::<u64>();
        if let Some(i) = peak_bin(row) {
            *self.peak_votes.entry(i).or_insert(0) += 1;
        }
    }

    /// The bin that was the peak in most rows.
    fn peak_bin(&self) -> Option<usize> {
        self.peak_votes
            .iter()
            .max_by_key(|&(_, votes)| *votes)
            .map(|(&i, _)| i)
    }

    fn fields(&self, header: &wfstream::Header, secs: f64) -> Fields {
        let peak_hz = self
            .peak_bin()
            .and_then(|i| u32::try_from(i).ok())
            .map(|i| header.frequency_of_bin(i));
        vec![
            ("rows", self.rows.to_string()),
            ("rows_per_s", format!("{:.2}", per_sec(self.rows, secs))),
            (
                "first_row_ms",
                self.first
                    .map_or_else(|| "none".into(), |t| t.as_millis().to_string()),
            ),
            ("bins", header.bins.to_string()),
            (
                "avg_compressed_row_bytes",
                format!("{:.1}", ratio(self.compressed_bytes, self.rows)),
            ),
            (
                "compressed_bytes_per_s",
                format!("{:.0}", per_sec(self.compressed_bytes, secs)),
            ),
            (
                "raw_bytes_per_s",
                format!("{:.0}", per_sec(self.rows * u64::from(header.bins), secs)),
            ),
            (
                "saturated_bins_per_row",
                format!("{:.1}", ratio(self.saturated, self.rows)),
            ),
            (
                "peak_bin",
                self.peak_bin()
                    .map_or_else(|| "none".into(), |i| i.to_string()),
            ),
            (
                "peak_hz",
                peak_hz.map_or_else(|| "none".into(), |f| format!("{f:.0}")),
            ),
        ]
    }
}

impl WaterfallArgs {
    fn header(&self) -> wfstream::Header {
        wfstream::Header {
            bins: self.bins,
            center_hz: self.center,
            bin_width_hz: f64::from(self.rbw),
            offset_db: WATERFALL_Q.offset_db,
            step_db: WATERFALL_Q.step_db,
        }
    }
}

async fn run_waterfall(client: &Ka9qClient, args: &WaterfallArgs) -> Result<Fields> {
    let params = args.params();
    params.validate(client.config())?;
    let header = args.header();
    let mut writer = match &args.out {
        Some(p) => Some(wfstream::Writer::new(
            BufWriter::new(File::create(p).with_context(|| format!("creating {}", p.display()))?),
            &header,
        )?),
        None => None,
    };
    let start = Instant::now();
    let mut sp = client.open_spectrum_channel(params).await?;
    let ssrc = sp.ssrc();
    eprintln!(
        "opened spectrum channel SSRC {ssrc}: {} bins x {} Hz at {} Hz, {} rows/s",
        args.bins, args.rbw, args.center, args.rate
    );
    let stop = until(start + Duration::from_secs_f64(args.secs));
    tokio::pin!(stop);
    let mut image: Vec<Vec<u8>> = Vec::new();
    let mut rows = RowStats::default();
    loop {
        let frame = tokio::select! {
            () = &mut stop => break,
            f = sp.recv() => f,
        };
        let Some(frame) = frame else { break };
        if frame.bins.len() != args.bins as usize {
            continue;
        }
        let t = start.elapsed();
        let row = requantize(&frame, &WATERFALL_Q);
        let compressed = compress_row(&row, args.zstd_level)?;
        if let Some(w) = writer.as_mut() {
            w.write_row(
                u64::try_from(t.as_micros()).unwrap_or(u64::MAX),
                &compressed,
            )?;
        }
        rows.add(t, &row, compressed.len());
        if args.pgm.is_some() {
            image.push(row);
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let stats = sp.stats();
    sp.close().await?;
    if let Some(w) = writer {
        w.finish()?;
    }
    if let Some(p) = &args.pgm {
        let f = File::create(p).with_context(|| format!("creating {}", p.display()))?;
        wfstream::write_pgm(BufWriter::new(f), args.bins as usize, &image)?;
    }
    let mut fields = vec![
        ("cmd", "waterfall".to_owned()),
        ("ssrc", ssrc.to_string()),
        ("secs", format!("{secs:.2}")),
        ("polls", stats.polls.to_string()),
        ("poll_failures", stats.poll_failures.to_string()),
        ("queue_drops", stats.queue_drops.to_string()),
    ];
    fields.extend(rows.fields(&header, secs));
    Ok(fields)
}

/// Reads a row stream written by `waterfall --out` and summarises it.
fn run_inspect(args: &InspectArgs) -> Result<Fields> {
    let f = File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?;
    let mut r = wfstream::Reader::new(std::io::BufReader::new(f))?;
    let header = r.header;
    let bins = usize::try_from(header.bins)?;
    let mut rows = RowStats::default();
    while let Some(row) = r.next_row()? {
        let data = backend_ka9q::spectrum::decompress_row_bounded(&row.compressed, bins)?;
        rows.add(Duration::from_micros(row.t_us), &data, row.compressed.len());
    }
    // Rows are polled at a fixed interval; the span of n rows is n - 1
    // intervals, so extrapolate by one interval for the rate.
    let secs = match (rows.first, rows.last) {
        (Some(a), Some(b)) if rows.rows > 1 => {
            let span = b.saturating_sub(a).as_secs_f64();
            span * ratio(rows.rows, rows.rows - 1)
        }
        _ => 0.0,
    };
    let mut fields = vec![
        ("cmd", "inspect".to_owned()),
        ("center_hz", format!("{:.0}", header.center_hz)),
        ("bin_width_hz", format!("{}", header.bin_width_hz)),
        ("offset_db", format!("{}", header.offset_db)),
        ("step_db", format!("{}", header.step_db)),
        ("secs", format!("{secs:.2}")),
    ];
    fields.extend(rows.fields(&header, secs));
    Ok(fields)
}

fn describe(s: &ChannelStatus) -> String {
    let mut out = String::new();
    let _ = write!(out, "SSRC {:>10}", s.ssrc.unwrap_or(0));
    if let Some(d) = s.demod_type {
        let _ = write!(out, "  {d}");
    }
    if let Some(p) = &s.preset {
        let _ = write!(out, "  preset {}", p.escape_debug());
    }
    if let Some(f) = s.radio_frequency_hz {
        let _ = write!(out, "  {f:.0} Hz");
    }
    if let Some(r) = s.output_samprate {
        let _ = write!(out, "  {r} S/s");
    }
    if let Some(e) = s.output_encoding {
        let _ = write!(out, "  {e}");
    }
    if let Some(b) = s.bin_count {
        let _ = write!(out, "  {b} bins");
    }
    if let Some(r) = s.resolution_bw_hz {
        let _ = write!(out, " x {r} Hz");
    }
    if let Some(l) = s.lifetime {
        let _ = write!(out, "  lifetime {l}");
    }
    if let Some(snr) = s.snr_db().filter(|v| v.is_finite()) {
        let _ = write!(out, "  SNR {snr:.1} dB");
    }
    if let Some(d) = s.output_data_dest_socket {
        let _ = write!(out, "  -> {d}");
    }
    out
}

fn describe_frontend(s: &ChannelStatus) -> String {
    let mut out = String::from("front end:");
    if let Some(d) = &s.description {
        let _ = write!(out, " {:?}", d.escape_debug().to_string());
    }
    if let Some(r) = s.input_samprate {
        let _ = write!(out, "  {r} S/s");
    }
    if let Some(real) = s.fe_isreal {
        out.push_str(if real { " real" } else { " complex" });
    }
    if let (Some(lo), Some(hi)) = (s.fe_low_edge_hz, s.fe_high_edge_hz) {
        let _ = write!(out, "  {lo:.0}..{hi:.0} Hz");
    }
    if let Some(p) = s.if_power_db {
        let _ = write!(out, "  IF {p:.1} dB");
    }
    if let Some(n) = s.noise_density_db {
        let _ = write!(out, "  N0 {n:.1} dB/Hz");
    }
    out
}

async fn run_status(client: &Ka9qClient, args: &StatusArgs) -> Result<Fields> {
    let all = client
        .dump_all(Duration::from_millis(args.window_ms))
        .await?;
    let probed = if all.is_empty() || args.probe {
        Some(client.probe().await?)
    } else {
        None
    };
    if let Some(fe) = probed.as_ref().or_else(|| all.first()) {
        println!("{}", describe_frontend(fe));
    }
    println!("{} channel(s)", all.len());
    for s in &all {
        println!("  {}", describe(s));
    }
    Ok(vec![
        ("cmd", "status".to_owned()),
        ("channels", all.len().to_string()),
        ("probed", probed.is_some().to_string()),
    ])
}

async fn run(cli: Cli) -> Result<()> {
    let radiod_pid = if cli.report {
        cli.radiod_pid.or_else(|| {
            let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
            cpu::find_radiod_pid(&cpu::radiod_pidfiles(xdg.as_deref()))
        })
    } else {
        None
    };
    let meter = if cli.report {
        Some(cpu::CpuMeter::start(radiod_pid)?)
    } else {
        None
    };
    let client = match cli.cmd {
        Cmd::Inspect(_) => None,
        _ => Some(Ka9qClient::connect(cli.client_config())?),
    };
    let mut fields = match (&cli.cmd, &client) {
        (Cmd::Inspect(i), _) => run_inspect(i)?,
        (Cmd::Audio(a), Some(c)) => run_audio(c, a).await?,
        (Cmd::Waterfall(w), Some(c)) => run_waterfall(c, w).await?,
        (Cmd::Status(s), Some(c)) => run_status(c, s).await?,
        (_, None) => bail!("no radiod client"),
    };
    if let Some(c) = &client {
        let net = c.stats();
        fields.push(("commands_sent", net.commands_sent.to_string()));
        fields.push(("command_timeouts", net.command_timeouts.to_string()));
    }
    if let Some(m) = meter {
        let r = m.finish()?;
        fields.push(("spike_cpu_pct", format!("{:.2}", r.own_percent)));
        match (radiod_pid, r.other_percent) {
            (Some(pid), Some(p)) => {
                fields.push(("radiod_pid", pid.to_string()));
                fields.push(("radiod_cpu_pct", format!("{p:.1}")));
            }
            _ => fields.push(("radiod_cpu_pct", "unknown".into())),
        }
    }
    for (k, v) in &fields {
        println!("{k:>24}: {v}");
    }
    println!("{}", result_line(&fields));
    Ok(())
}

fn validate(cli: &Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Audio(a) => a.params().validate()?,
        Cmd::Waterfall(w) => w.params().validate(&cli.client_config())?,
        Cmd::Status(_) | Cmd::Inspect(_) => {}
    }
    cli.client_config().validate()?;
    if cli.status_addr.port() == 0 {
        bail!("--status-addr needs a port");
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    validate(&cli)?;
    run(cli).await
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "parsed values are exact")]
mod tests {
    use super::*;

    use crate::fake_radiod as support;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("ka9q-spike").chain(args.iter().copied()))
    }

    #[test]
    fn audio_args_and_defaults() {
        let cli = parse(&[
            "audio",
            "--freq",
            "10e6",
            "--mode",
            "am",
            "--secs",
            "10",
            "--bitrate",
            "16000",
            "--samprate",
            "12000",
            "--low",
            "-5000",
            "--high",
            "5000",
            "--out",
            "out.opus",
        ])
        .unwrap();
        assert_eq!(cli.status_addr.to_string(), "239.77.0.6:5006");
        assert_eq!(cli.data_addr.to_string(), "239.77.0.4:5004");
        assert_eq!(cli.interface, Ipv4Addr::LOCALHOST);
        assert!(!cli.report);
        let Cmd::Audio(a) = &cli.cmd else {
            panic!("not audio")
        };
        assert_eq!(a.freq, 10e6);
        assert_eq!(a.secs, 10.0);
        let p = a.params();
        assert_eq!(p.preset.as_deref(), Some("am"));
        assert_eq!((p.samprate, p.opus_bitrate), (Some(12_000), Some(16_000)));
        assert_eq!(
            (p.low_edge_hz, p.high_edge_hz),
            (Some(-5000.0), Some(5000.0))
        );
        validate(&cli).unwrap();
        let cfg = cli.client_config();
        assert_eq!(cfg.command_timeout, Duration::from_millis(250));
        assert_eq!(cfg.lifetime_frames, 500);
    }

    #[test]
    fn waterfall_args_and_global_flags() {
        let cli = parse(&[
            "waterfall",
            "--center",
            "10e6",
            "--bins",
            "1024",
            "--rbw",
            "100",
            "--rate",
            "10",
            "--secs",
            "10",
            "--out",
            "rows.cxwf",
            "--pgm",
            "rows.pgm",
            "--report",
            "--radiod-pid",
            "42",
            "--status-addr",
            "127.0.0.1:5006",
            "--ttl",
            "1",
        ])
        .unwrap();
        assert!(cli.report);
        assert_eq!(cli.radiod_pid, Some(42));
        assert_eq!(cli.ttl, 1);
        let Cmd::Waterfall(w) = &cli.cmd else {
            panic!("not waterfall")
        };
        let p = w.params();
        assert_eq!(p.bin_count, 1024);
        assert_eq!(p.resolution_bw_hz, 100.0);
        assert_eq!(p.row_interval, Duration::from_millis(100));
        assert_eq!(w.zstd_level, 3);
        validate(&cli).unwrap();
    }

    #[test]
    fn invalid_arguments_are_rejected() {
        for args in [
            &["audio", "--freq", "0", "--out", "x"][..],
            &["audio", "--freq", "-5", "--out", "x"],
            &["audio", "--freq", "abc", "--out", "x"],
            &["audio", "--freq", "NaN", "--out", "x"],
            &["audio", "--freq", "10e6"],
            &["audio", "--freq", "10e6", "--secs", "0", "--out", "x"],
            &["audio", "--freq", "10e6", "--secs", "1e9", "--out", "x"],
            &["waterfall", "--center", "10e6", "--bins", "0"],
            &["waterfall", "--center", "10e6", "--bins", "99999"],
            &["waterfall", "--center", "10e6", "--rate", "0"],
            &["waterfall", "--center", "10e6", "--rate", "100"],
            &["waterfall", "--center", "10e6", "--rbw", "0"],
            &["waterfall", "--center", "10e6", "--zstd-level", "40"],
            &["status", "--lifetime-frames", "1"],
            &["status", "--timeout-ms", "0"],
            &["bogus"],
        ] {
            assert!(parse(args).is_err(), "{args:?} should fail");
        }
        // Parses, but fails semantic validation.
        for args in [
            &[
                "audio",
                "--freq",
                "10e6",
                "--samprate",
                "11025",
                "--out",
                "x",
            ][..],
            &["audio", "--freq", "10e6", "--mode", "a m", "--out", "x"],
            &[
                "audio", "--freq", "10e6", "--low", "100", "--high", "-100", "--out", "x",
            ],
            &["waterfall", "--center", "10e6", "--rate", "0.01"],
            &["status", "--status-addr", "127.0.0.1:0"],
            &["status", "--lifetime-frames", "5"],
        ] {
            let cli = parse(args).unwrap();
            assert!(validate(&cli).is_err(), "{args:?} should be invalid");
        }
    }

    #[test]
    fn result_line_format() {
        let f = vec![("a", "1".to_owned()), ("b", "x y".to_owned())];
        assert_eq!(result_line(&f), "RESULT a=1 b=x y");
        assert_eq!(per_sec(10, 0.0), 0.0);
        assert_eq!(per_sec(10, 2.0), 5.0);
        assert_eq!(ratio(10, 4), 2.5);
        assert_eq!(ratio(10, 0), 0.0);
    }

    #[test]
    fn peak_is_the_middle_of_clipped_bins() {
        assert_eq!(peak_bin(&[]), None);
        assert_eq!(peak_bin(&[1, 9, 3]), Some(1));
        assert_eq!(peak_bin(&[255, 0, 255, 0, 255, 1]), Some(2));
        assert_eq!(peak_bin(&[0, 255, 255]), Some(2));
        let mut s = RowStats::default();
        s.add(Duration::ZERO, &[0, 255, 255, 255], 3);
        s.add(Duration::from_millis(100), &[0, 255, 1, 0], 5);
        s.add(Duration::from_millis(200), &[0, 0, 255, 0], 5);
        assert_eq!(s.peak_bin(), Some(2));
        assert_eq!(s.saturated, 5);
    }

    #[test]
    fn status_descriptions() {
        let s = ChannelStatus {
            ssrc: Some(5),
            demod_type: Some(backend_ka9q::tags::DemodType::Spect2),
            preset: Some("am\n".into()),
            radio_frequency_hz: Some(10e6),
            output_samprate: Some(12_000),
            output_encoding: Some(backend_ka9q::tags::Encoding::Opus),
            bin_count: Some(512),
            resolution_bw_hz: Some(1000.0),
            lifetime: Some(499),
            low_edge_hz: Some(-5000.0),
            high_edge_hz: Some(5000.0),
            baseband_power_db: Some(-20.0),
            noise_density_db: Some(-90.0),
            output_data_dest_socket: Some("239.77.0.4:5004".parse().unwrap()),
            description: Some("sig\u{1b}gen".into()),
            input_samprate: Some(24_000_000),
            fe_isreal: Some(true),
            fe_low_edge_hz: Some(0.0),
            fe_high_edge_hz: Some(12e6),
            if_power_db: Some(-20.4),
            ..ChannelStatus::default()
        };
        let d = describe(&s);
        assert!(d.contains("SSRC          5"), "{d}");
        assert!(d.contains("preset am\\n"), "{d}");
        assert!(
            d.contains("10000000 Hz") && d.contains("512 bins x 1000 Hz"),
            "{d}"
        );
        assert!(d.contains("SNR") && d.contains("-> 239.77.0.4:5004"), "{d}");
        let fe = describe_frontend(&s);
        assert!(
            fe.contains("24000000 S/s real") && fe.contains("0..12000000 Hz"),
            "{fe}"
        );
        assert!(
            !fe.contains('\u{1b}'),
            "control characters are escaped: {fe}"
        );
        let empty = ChannelStatus::default();
        assert_eq!(describe(&empty), "SSRC          0");
        assert_eq!(describe_frontend(&empty), "front end:");
    }

    async fn fake_client() -> (support::FakeRadiod, Ka9qClient) {
        let fake = support::FakeRadiod::start().await;
        let client = Ka9qClient::connect(ClientConfig {
            status_addr: fake.addr,
            data_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            command_timeout: Duration::from_millis(100),
            lifetime_frames: 50,
            ..ClientConfig::default()
        })
        .unwrap();
        fake.set_rtp_dest(client.local_data_addr());
        (fake, client)
    }

    fn field<'a>(f: &'a Fields, k: &str) -> &'a str {
        f.iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn audio_against_the_fake_writes_an_ogg_file() {
        let (fake, client) = fake_client().await;
        fake.state().rtp_drop_every = 10;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.opus");
        let args = AudioArgs {
            freq: 10e6,
            mode: "am".into(),
            secs: 2.0,
            bitrate: Some(16_000),
            samprate: Some(12_000),
            low: None,
            high: None,
            out: out.clone(),
        };
        let f = run_audio(&client, &args).await.unwrap();
        let packets: u64 = field(&f, "packets").parse().unwrap();
        assert!((85..=100).contains(&packets), "{packets}");
        let plc: u64 = field(&f, "plc_frames").parse().unwrap();
        assert!(plc >= 8, "{f:?}");
        assert_eq!(field(&f, "lost"), plc.to_string());
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[..4], b"OggS");
        assert!(bytes.windows(8).any(|w| w == b"OpusHead"));
        fake.assert_client_contract();
        assert!(fake.state().channels.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn waterfall_against_the_fake_writes_rows_and_pgm() {
        let (fake, client) = fake_client().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rows.cxwf");
        let pgm = dir.path().join("rows.pgm");
        let args = WaterfallArgs {
            center: 10e6,
            bins: 256,
            rbw: 1000.0,
            avg: Some(2),
            rate: 10.0,
            secs: 3.0,
            zstd_level: 3,
            out: Some(out.clone()),
            pgm: Some(pgm.clone()),
        };
        let f = run_waterfall(&client, &args).await.unwrap();
        let rows: u64 = field(&f, "rows").parse().unwrap();
        assert!((28..=31).contains(&rows), "{f:?}");
        assert_eq!(field(&f, "peak_hz"), "10000000");
        let mut r = wfstream::Reader::new(File::open(&out).unwrap()).unwrap();
        assert_eq!(r.header.bins, 256);
        let mut n = 0;
        while let Some(row) = r.next_row().unwrap() {
            let bins =
                backend_ka9q::spectrum::decompress_row_bounded(&row.compressed, 256).unwrap();
            let peak = bins.iter().enumerate().max_by_key(|&(_, v)| *v).unwrap().0;
            assert_eq!(peak, 128);
            n += 1;
        }
        assert_eq!(n, rows);
        let img = std::fs::read(&pgm).unwrap();
        assert!(img.starts_with(format!("P5\n256 {rows}\n255\n").as_bytes()));
        let inspected = run_inspect(&InspectArgs { file: out.clone() }).unwrap();
        assert_eq!(field(&inspected, "rows"), rows.to_string());
        assert_eq!(field(&inspected, "peak_hz"), "10000000");
        let rate: f64 = field(&inspected, "rows_per_s").parse().unwrap();
        assert!((9.5..=10.5).contains(&rate), "{inspected:?}");
        assert!(run_inspect(&InspectArgs { file: pgm.clone() }).is_err());

        // Without output files.
        let args = WaterfallArgs {
            out: None,
            pgm: None,
            secs: 1.0,
            ..args
        };
        let f = run_waterfall(&client, &args).await.unwrap();
        assert!(field(&f, "rows").parse::<u64>().unwrap() >= 8);
        fake.assert_client_contract();
    }

    #[tokio::test(start_paused = true)]
    async fn status_against_the_fake() {
        let (fake, client) = fake_client().await;
        let args = StatusArgs {
            window_ms: 100,
            probe: false,
        };
        // No channels: probes.
        let f = run_status(&client, &args).await.unwrap();
        assert_eq!(field(&f, "channels"), "0");
        assert_eq!(field(&f, "probed"), "true");
        let ch = client
            .open_audio_channel(AudioChannelParams::new(10e6, "am"))
            .await
            .unwrap();
        let f = run_status(&client, &args).await.unwrap();
        assert_eq!(field(&f, "channels"), "1");
        assert_eq!(field(&f, "probed"), "false");
        ch.close().await.unwrap();
        fake.assert_client_contract();
    }

    #[tokio::test]
    async fn run_end_to_end_against_the_fake_with_report() {
        let fake = support::FakeRadiod::start().await;
        let cli = parse(&[
            "status",
            "--status-addr",
            &fake.addr.to_string(),
            "--data-addr",
            "127.0.0.1:0",
            "--window-ms",
            "50",
            "--report",
            "--radiod-pid",
            &std::process::id().to_string(),
            "--lifetime-frames",
            "50",
            "--timeout-ms",
            "100",
        ])
        .unwrap();
        validate(&cli).unwrap();
        run(cli).await.unwrap();
    }
}
