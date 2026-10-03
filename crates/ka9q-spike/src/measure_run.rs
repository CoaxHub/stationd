// SPDX-License-Identifier: GPL-3.0-only
//! `ka9q-spike measure`: the noise-floor measurement spike. Drives one hopping wideband spectrum channel in radiod with
//! [`backend_ka9q::client::PolledSpectrum`] and reduces it with
//! [`crate::measure`].
//!
//! Modes:
//! - `hop`: continuous duty. The channel stays open and hops between
//!   windows on dwell slots aligned to the UTC minute; every poll is logged
//!   (`polls.csv`) and every minute's quantiles per band are written
//!   (`noise.csv`), with radiod's channel count, memory and CPU
//!   (`health.csv`).
//! - `idle`: the idle duty. The channel is opened `lead` before each
//!   measuring minute (which starts radiod's front end) and closed `tail`
//!   after it (which stops it). The warm-up polls are logged
//!   (`warmup.csv`), and each run's timings, sample counter, leaked channels
//!   and radiod memory and CPU go to `runs.csv`.
//! - `rbw`: `RESOLUTION_BW` changes on a live channel with a fixed bin count.
//! - `hold`: the front end with one channel that is polled only to keep it
//!   alive, for CPU baselines.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use backend_ka9q::client::{
    AudioChannel, AudioChannelParams, Ka9qClient, PolledSpectrum, SpectrumParams, SpectrumReply,
};
use backend_ka9q::spectrum::SpectrumFrame;
use backend_ka9q::status::ChannelStatus;
use clap::{Args, Subcommand};

use crate::cpu;
use crate::measure::{
    self, FreqRange, Histogram, band_bins, correlation, densities, is_measuring_minute,
    minute_start, next_measuring_minute, peak_index, quantile_of, utc_string,
};

/// Measurement spike commands.
#[derive(Debug, Subcommand)]
pub enum MeasureCmd {
    /// Continuous duty: hop the channel and log every poll and minute.
    Hop(HopArgs),
    /// Idle duty: open the channel only around the measuring minutes.
    Idle(IdleArgs),
    /// Change `RESOLUTION_BW` on a live channel with a fixed bin count.
    Rbw(RbwArgs),
    /// One channel polled only to keep it alive (CPU baseline).
    Hold(HoldArgs),
}

/// Options shared by the measuring modes.
#[derive(Debug, Args, Clone)]
pub struct Common {
    /// Bins per spectrum, fixed for the channel's life.
    #[arg(long, default_value_t = 1024)]
    pub bins: u32,
    /// Resolution bandwidth (bin spacing), Hz.
    #[arg(long, default_value_t = 500.0)]
    pub rbw: f32,
    /// FFTs radiod averages per spectrum (`SPECTRUM_AVG`; radiod's default
    /// when omitted).
    #[arg(long)]
    pub avg: Option<u32>,
    /// Poll interval, ms.
    #[arg(long, default_value_t = 500)]
    pub poll_ms: u64,
    /// Dwell per window, ms; slots are aligned to the UTC minute.
    #[arg(long, default_value_t = 4000)]
    pub dwell_ms: u64,
    /// Spectra discarded after each retune (0 = the retune's own reply counts).
    #[arg(long, default_value_t = 0)]
    pub discard: u32,
    /// A band to report, `NAME:LO_HZ:HI_HZ` (repeatable).
    #[arg(long = "band", value_parser = parse_band, default_values_t = [Band::forty()])]
    pub bands: Vec<Band>,
    /// Window centres, Hz, comma-separated (default: planned over --range).
    #[arg(long, value_delimiter = ',', value_parser = parse_hz)]
    pub windows: Vec<f64>,
    /// Range the windows tile, `LO_HZ:HI_HZ` (default: the 40 m band).
    #[arg(long, value_parser = parse_range)]
    pub range: Option<FreqRange>,
    /// Fraction of the tunable range cut at each end (anti-alias roll-off).
    #[arg(long, default_value_t = 0.05)]
    pub rolloff: f64,
    /// Half-width of the exclusion around the front end's centre, Hz.
    #[arg(long, default_value_t = 2000.0)]
    pub dc_hz: f64,
    /// Output directory for the CSV logs.
    #[arg(long)]
    pub out_dir: PathBuf,
    /// radiod pidfile, re-read for memory and CPU (radiod may restart).
    #[arg(long)]
    pub radiod_pidfile: Option<PathBuf>,
}

/// `measure hop`.
#[derive(Debug, Args)]
pub struct HopArgs {
    #[command(flatten)]
    pub common: Common,
    /// Run length, seconds.
    #[arg(long, default_value_t = 600.0, value_parser = parse_secs)]
    pub secs: f64,
    /// Also hold a listener audio channel, `FREQ_HZ:PRESET` (e.g. `7.0475e6:usb`).
    #[arg(long, value_parser = parse_listener)]
    pub listener: Option<(f64, String)>,
}

/// `measure idle`.
#[derive(Debug, Args)]
pub struct IdleArgs {
    #[command(flatten)]
    pub common: Common,
    /// Run length, seconds.
    #[arg(long, default_value_t = 3600.0, value_parser = parse_secs)]
    pub secs: f64,
    /// Period of the measuring minutes, seconds (a multiple of 60).
    #[arg(long, default_value_t = 300)]
    pub period_s: i64,
    /// The channel opens this long before the measuring minute, ms.
    #[arg(long, default_value_t = 5000)]
    pub lead_ms: i64,
    /// The channel closes this long after the measuring minute ends, ms.
    #[arg(long, default_value_t = 500)]
    pub tail_ms: i64,
    /// Poll interval during the warm-up, ms.
    #[arg(long, default_value_t = 100)]
    pub warmup_poll_ms: u64,
}

/// `measure rbw`.
#[derive(Debug, Args)]
pub struct RbwArgs {
    #[command(flatten)]
    pub common: Common,
    /// Window centre, Hz.
    #[arg(long, value_parser = parse_hz)]
    pub center: f64,
    /// Resolution bandwidths to step through, Hz.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "500,1000,250,2000,100,50,500"
    )]
    pub sequence: Vec<f32>,
    /// Polls after each change.
    #[arg(long, default_value_t = 8)]
    pub polls: u32,
    /// Times to run the sequence.
    #[arg(long, default_value_t = 1)]
    pub repeat: u32,
}

/// `measure hold`.
#[derive(Debug, Args)]
pub struct HoldArgs {
    /// Window centre, Hz.
    #[arg(long, value_parser = parse_hz)]
    pub center: f64,
    /// Bins.
    #[arg(long, default_value_t = 1024)]
    pub bins: u32,
    /// Resolution bandwidth, Hz.
    #[arg(long, default_value_t = 500.0)]
    pub rbw: f32,
    /// Keepalive poll interval, ms.
    #[arg(long, default_value_t = 2000)]
    pub poll_ms: u64,
    /// Run length, seconds.
    #[arg(long, default_value_t = 60.0, value_parser = parse_secs)]
    pub secs: f64,
    /// Also hold a listener audio channel, `FREQ_HZ:PRESET`.
    #[arg(long, value_parser = parse_listener)]
    pub listener: Option<(f64, String)>,
}

/// A named band.
#[derive(Debug, Clone, PartialEq)]
pub struct Band {
    /// Name, e.g. `40m`.
    pub name: String,
    /// Range.
    pub range: FreqRange,
}

impl Band {
    fn forty() -> Self {
        Self {
            name: "40m".into(),
            range: FreqRange {
                lo_hz: 7.0e6,
                hi_hz: 7.3e6,
            },
        }
    }
}

impl std::fmt::Display for Band {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.name, self.range.lo_hz, self.range.hi_hz)
    }
}

fn parse_hz(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("not a number: {s:?}"))?;
    if v.is_finite() && v > 0.0 && v < 1e12 {
        Ok(v)
    } else {
        Err(format!("frequency out of range: {s}"))
    }
}

/// Longest run, seconds: a week.
const MAX_RUN_SECS: f64 = 7.0 * 86_400.0;

fn parse_secs(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("not a number: {s:?}"))?;
    if v.is_finite() && v > 0.0 && v <= MAX_RUN_SECS {
        Ok(v)
    } else {
        Err(format!(
            "seconds must be > 0 and <= {MAX_RUN_SECS}, got {s}"
        ))
    }
}

fn parse_range(s: &str) -> Result<FreqRange, String> {
    let (lo, hi) = s.split_once(':').ok_or("expected LO:HI")?;
    FreqRange::new(parse_hz(lo)?, parse_hz(hi)?).ok_or_else(|| format!("empty range {s}"))
}

fn parse_band(s: &str) -> Result<Band, String> {
    let (name, range) = s.split_once(':').ok_or("expected NAME:LO:HI")?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("bad band name {name:?}"));
    }
    Ok(Band {
        name: name.to_owned(),
        range: parse_range(range)?,
    })
}

fn parse_listener(s: &str) -> Result<(f64, String), String> {
    let (f, mode) = s.split_once(':').ok_or("expected FREQ:PRESET")?;
    if mode.is_empty() || !mode.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("bad preset {mode:?}"));
    }
    Ok((parse_hz(f)?, mode.to_owned()))
}

/// Unix time now, ms.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Set by SIGINT or SIGTERM.
#[derive(Debug, Clone, Default)]
struct Stop(Arc<AtomicBool>);

impl Stop {
    fn install() -> Self {
        let stop = Self::default();
        let flag = Arc::clone(&stop.0);
        tokio::spawn(async move {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("no SIGTERM handler: {e}");
                        let _ = tokio::signal::ctrl_c().await;
                        flag.store(true, Ordering::Relaxed);
                        return;
                    }
                };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            flag.store(true, Ordering::Relaxed);
        });
        stop
    }

    fn requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Sleeps until Unix time `t_ms`, waking early on a stop. Returns false
    /// if stopped.
    async fn sleep_until(&self, t_ms: i64) -> bool {
        loop {
            if self.requested() {
                return false;
            }
            let left = t_ms - now_ms();
            if left <= 0 {
                return true;
            }
            #[allow(clippy::cast_sign_loss, reason = "positive here")]
            tokio::time::sleep(Duration::from_millis(left.min(200) as u64)).await;
        }
    }
}

/// A CSV file with a header, flushed on demand.
#[derive(Debug)]
struct Csv(BufWriter<File>);

impl Csv {
    fn create(dir: &Path, name: &str, header: &str) -> Result<Self> {
        let path = dir.join(name);
        let exists = path.exists();
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut w = BufWriter::new(f);
        if !exists {
            writeln!(w, "{header}")?;
        }
        Ok(Self(w))
    }

    fn row(&mut self, fields: &[String]) -> Result<()> {
        writeln!(self.0, "{}", fields.join(","))?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush()?;
        Ok(())
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(String::new, |v| v.to_string())
}

fn f1(v: Option<f32>) -> String {
    v.map_or_else(String::new, |v| format!("{v:.1}"))
}

/// What the front end looks like, from a status packet.
#[derive(Debug, Clone, Copy)]
struct FrontEnd {
    lo_hz: f64,
    range: FreqRange,
    samprate: u32,
    blocksize: u32,
    fir_length: u32,
}

impl FrontEnd {
    fn from_status(st: &ChannelStatus) -> Result<Self> {
        let lo_hz = st.first_lo_hz.context("no FIRST_LO_FREQUENCY")?;
        let lo = st.fe_low_edge_hz.context("no FE_LOW_EDGE")?;
        let hi = st.fe_high_edge_hz.context("no FE_HIGH_EDGE")?;
        Ok(Self {
            lo_hz,
            range: FreqRange::new(lo_hz + lo, lo_hz + hi).context("empty front-end range")?,
            samprate: st.input_samprate.context("no INPUT_SAMPRATE")?,
            blocksize: st.filter_blocksize.context("no FILTER_BLOCKSIZE")?,
            fir_length: st.filter_fir_length.context("no FILTER_FIR_LENGTH")?,
        })
    }
}

/// The measurement plan: windows, bands and exclusions.
#[derive(Debug, Clone)]
struct Plan {
    windows: Vec<f64>,
    bands: Vec<(Band, FreqRange)>,
    exclude: Vec<FreqRange>,
    grid_hz: Option<f64>,
}

fn make_plan(c: &Common, fe: FrontEnd) -> Result<Plan> {
    let usable = measure::usable_range(fe.range.lo_hz, fe.range.hi_hz, c.rolloff)
        .context("no usable range")?;
    let grid_hz =
        measure::placement_grid_hz(f64::from(c.rbw), fe.samprate, fe.blocksize, fe.fir_length);
    let span = f64::from(c.bins) * f64::from(c.rbw);
    let windows = if c.windows.is_empty() {
        let range = c.range.unwrap_or(Band::forty().range);
        let range = range.intersect(&usable).context("range outside coverage")?;
        measure::plan_windows(range, span, grid_hz.unwrap_or(f64::from(c.rbw)), fe.lo_hz)
    } else {
        c.windows.clone()
    };
    if windows.is_empty() {
        bail!("no windows");
    }
    for &w in &windows {
        let actual = measure::wideband_center(
            w,
            fe.lo_hz,
            fe.samprate,
            fe.blocksize,
            fe.fir_length,
            f64::from(c.rbw),
        );
        if actual.is_some_and(|a| (a - w).abs() > 1e-3) {
            eprintln!(
                "warning: window {w} Hz is off radiod's placement grid; radiod centres it at {:.1} Hz",
                actual.unwrap_or(w)
            );
        }
    }
    let bands = c
        .bands
        .iter()
        .filter_map(|b| b.range.intersect(&usable).map(|r| (b.clone(), r)))
        .collect::<Vec<_>>();
    let exclude =
        vec![FreqRange::new(fe.lo_hz - c.dc_hz, fe.lo_hz + c.dc_hz).context("bad --dc-hz")?];
    eprintln!(
        "front end: LO {:.0} Hz, tunable {:.0}..{:.0} Hz, usable {:.0}..{:.0} Hz, {} S/s, \
         L {} M {}, placement grid {:?} Hz",
        fe.lo_hz,
        fe.range.lo_hz,
        fe.range.hi_hz,
        usable.lo_hz,
        usable.hi_hz,
        fe.samprate,
        fe.blocksize,
        fe.fir_length,
        grid_hz
    );
    eprintln!("windows: {windows:?}");
    for (b, r) in &bands {
        eprintln!(
            "band {}: measured {:.0}..{:.0} Hz",
            b.name, r.lo_hz, r.hi_hz
        );
    }
    Ok(Plan {
        windows,
        bands,
        exclude,
        grid_hz,
    })
}

/// radiod's memory (`VmRSS`, kB) and CPU ticks, if it is running.
fn radiod_usage(pid: Option<u32>) -> (Option<u64>, Option<u64>) {
    let Some(pid) = pid else {
        return (None, None);
    };
    let rss = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        });
    let ticks = cpu::read_stat(Some(pid))
        .ok()
        .filter(|(comm, st)| comm == "radiod" && st.state != 'Z')
        .map(|(_, st)| st.cpu_ticks);
    (rss, ticks)
}

fn radiod_pid(pidfile: Option<&Path>, fallback: Option<u32>) -> Option<u32> {
    pidfile
        .and_then(|p| cpu::find_radiod_pid(&[p.to_path_buf()]))
        .or(fallback)
}

/// Per-minute accumulation of the band histograms.
#[derive(Debug)]
struct MinuteAcc {
    minute: i64,
    hists: Vec<Histogram>,
    spectra: Vec<u32>,
    measured: Vec<Option<(f64, f64)>>,
    ad_over_start: Option<u64>,
    ad_over_last: Option<u64>,
    listener_n0: Vec<f64>,
}

impl MinuteAcc {
    fn new(minute: i64, bands: usize) -> Self {
        Self {
            minute,
            hists: vec![Histogram::default(); bands],
            spectra: vec![0; bands],
            measured: vec![None; bands],
            ad_over_start: None,
            ad_over_last: None,
            listener_n0: Vec::new(),
        }
    }

    fn add(&mut self, plan: &Plan, frame: &SpectrumFrame, ad_over: Option<u64>) {
        if let Some(a) = ad_over {
            self.ad_over_start.get_or_insert(a);
            self.ad_over_last = Some(a);
        }
        for (k, (_, range)) in plan.bands.iter().enumerate() {
            let idx = band_bins(frame, range, &plan.exclude);
            if idx.is_empty() {
                continue;
            }
            self.hists[k].add_frame(frame, &idx);
            self.spectra[k] += 1;
            let half = frame.bin_width_hz / 2.0;
            let lo = frame.frequency_of_bin(idx[0]) - half;
            let hi = frame.frequency_of_bin(idx[idx.len() - 1]) + half;
            let m = self.measured[k].get_or_insert((lo, hi));
            *m = (m.0.min(lo), m.1.max(hi));
        }
    }

    fn write(&self, plan: &Plan, duty: &str, csv: &mut Csv) -> Result<()> {
        let n0 = quantile_of(
            &self
                .listener_n0
                .iter()
                .map(|&v| {
                    #[allow(clippy::cast_possible_truncation, reason = "dB values")]
                    let v = v as f32;
                    v
                })
                .collect::<Vec<_>>(),
            0.5,
        );
        for (k, (band, _)) in plan.bands.iter().enumerate() {
            let Some(q) = self.hists[k].quantiles() else {
                continue;
            };
            let (lo, hi) = self.measured[k].unwrap_or((0.0, 0.0));
            let over = match (self.ad_over_start, self.ad_over_last) {
                (Some(a), Some(b)) => Some(b.saturating_sub(a)),
                _ => None,
            };
            csv.row(&[
                utc_string(self.minute),
                (self.minute / 1000).to_string(),
                duty.to_owned(),
                band.name.clone(),
                format!("{lo:.0}"),
                format!("{hi:.0}"),
                self.spectra[k].to_string(),
                self.hists[k].total().to_string(),
                format!("{:.1}", q[0]),
                format!("{:.1}", q[1]),
                format!("{:.1}", q[2]),
                format!("{:.1}", q[3]),
                opt(over),
                f1(n0),
            ])?;
        }
        csv.flush()
    }
}

const NOISE_HEADER: &str = "minute_utc,minute_unix_s,duty,band,measured_lo_hz,measured_hi_hz,\
spectra,samples,p10_db_hz,p25_db_hz,p50_db_hz,p90_db_hz,ad_over,listener_n0_db_hz";
const POLLS_HEADER: &str = "t_utc,t_unix_ms,phase,slot,window_hz,poll,latency_ms,frame,\
reported_center_hz,input_samples,samples_since_prev,step_db,median_db_hz,p10_db_hz,peak_hz,\
peak_db_hz,corr_ref,corr_prev_ref";

/// One reply of a dwell, kept until the dwell ends.
#[derive(Debug)]
struct PollRec {
    t_ms: i64,
    latency_ms: i64,
    reply: Option<SpectrumReply>,
    dens: Option<Vec<f32>>,
}

/// The reference spectrum of a dwell for poll `skip`: the mean density per
/// bin of the dwell's polls from index 3 on, leaving out poll `skip` itself.
fn dwell_reference(polls: &[PollRec], skip: usize) -> Option<Vec<f32>> {
    let late: Vec<&Vec<f32>> = polls
        .iter()
        .enumerate()
        .skip(3)
        .filter(|&(k, _)| k != skip)
        .filter_map(|(_, p)| p.dens.as_ref())
        .collect();
    let first = late.first()?;
    if late.iter().any(|d| d.len() != first.len()) {
        return None;
    }
    let mut mean = vec![0f32; first.len()];
    for d in &late {
        for (m, v) in mean.iter_mut().zip(d.iter()) {
            *m += v;
        }
    }
    #[allow(clippy::cast_precision_loss, reason = "a few polls")]
    let n = late.len() as f32;
    for m in &mut mean {
        *m /= n;
    }
    Some(mean)
}

/// The hopping state shared by `hop` and `idle`.
#[derive(Debug)]
struct Hopper {
    plan: Plan,
    common: Common,
    polls_csv: Csv,
    noise_csv: Csv,
    acc: Option<MinuteAcc>,
    prev_ref: Option<(f64, Vec<f32>)>,
    last_samples: Option<u64>,
    polls: u64,
    failures: u64,
    stale_suspects: u64,
}

impl Hopper {
    fn new(plan: Plan, common: Common) -> Result<Self> {
        std::fs::create_dir_all(&common.out_dir)?;
        Ok(Self {
            polls_csv: Csv::create(&common.out_dir, "polls.csv", POLLS_HEADER)?,
            noise_csv: Csv::create(&common.out_dir, "noise.csv", NOISE_HEADER)?,
            plan,
            common,
            acc: None,
            prev_ref: None,
            last_samples: None,
            polls: 0,
            failures: 0,
            stale_suspects: 0,
        })
    }

    fn polls_per_dwell(&self) -> u64 {
        (self.common.dwell_ms / self.common.poll_ms.max(1)).max(1)
    }

    fn dwell_ms(&self) -> i64 {
        i64::try_from(self.common.dwell_ms).unwrap_or(4000)
    }

    /// The window of the dwell slot starting at `t_ms`.
    fn window_of_slot(&self, t_ms: i64) -> f64 {
        let slot = (t_ms - minute_start(t_ms)) / self.dwell_ms();
        let n = i64::try_from(self.plan.windows.len()).unwrap_or(1);
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "mod n"
        )]
        self.plan.windows[slot.rem_euclid(n) as usize]
    }

    /// Runs one dwell slot from `slot_ms`: retune (or poll) at once, then
    /// poll every `poll_ms`. Spectra in `[pool_from, pool_to)` are pooled.
    async fn dwell(
        &mut self,
        sp: &mut PolledSpectrum,
        slot_ms: i64,
        phase: &str,
        pool: (i64, i64),
        duty: &str,
        stop: &Stop,
    ) -> Result<()> {
        let window = self.window_of_slot(slot_ms);
        let poll_ms = i64::try_from(self.common.poll_ms).unwrap_or(500);
        let mut records = Vec::new();
        for k in 0..self.polls_per_dwell() {
            let k = i64::try_from(k).unwrap_or(0);
            if !stop.sleep_until(slot_ms + k * poll_ms).await {
                break;
            }
            let t0 = now_ms();
            let res = if k == 0 && (sp.params().center_hz - window).abs() > 0.5 {
                sp.retune(window).await
            } else {
                sp.poll().await
            };
            let t1 = now_ms();
            self.polls += 1;
            let reply = match res {
                Ok(r) => Some(r),
                Err(e) => {
                    self.failures += 1;
                    tracing::warn!("poll failed: {e}");
                    None
                }
            };
            let dens = reply.as_ref().and_then(|r| r.frame.as_ref()).map(densities);
            records.push(PollRec {
                t_ms: t1,
                latency_ms: t1 - t0,
                reply,
                dens,
            });
        }
        for (k, rec) in records.iter().enumerate() {
            let reference = dwell_reference(&records, k);
            self.log_poll(phase, slot_ms, window, k, rec, reference.as_deref())?;
            let Some(frame) = rec.reply.as_ref().and_then(|r| r.frame.as_ref()) else {
                continue;
            };
            let in_pool = rec.t_ms >= pool.0 && rec.t_ms < pool.1;
            if k < self.common.discard as usize || !in_pool || frame.step_db <= 0.0 {
                continue;
            }
            let minute = minute_start(rec.t_ms);
            if self.acc.as_ref().is_some_and(|a| a.minute != minute) {
                self.flush_minute(duty)?;
            }
            let bands = self.plan.bands.len();
            let acc = self
                .acc
                .get_or_insert_with(|| MinuteAcc::new(minute, bands));
            let ad_over = rec.reply.as_ref().and_then(|r| r.status.ad_over);
            acc.add(&self.plan, frame, ad_over);
        }
        if let Some(r) = dwell_reference(&records, usize::MAX) {
            self.prev_ref = Some((window, r));
        }
        self.polls_csv.flush()
    }

    fn log_poll(
        &mut self,
        phase: &str,
        slot_ms: i64,
        window: f64,
        k: usize,
        rec: &PollRec,
        reference: Option<&[f32]>,
    ) -> Result<()> {
        let status = rec.reply.as_ref().map(|r| &r.status);
        let frame = rec.reply.as_ref().and_then(|r| r.frame.as_ref());
        let samples = status.and_then(|s| s.input_samples);
        let since_prev = match (samples, self.last_samples) {
            (Some(a), Some(b)) => Some(i128::from(a) - i128::from(b)),
            _ => None,
        };
        if samples.is_some() {
            self.last_samples = samples;
        }
        let dens = rec.dens.as_deref();
        let (peak_hz, peak_db) = match (frame, dens.and_then(peak_index)) {
            (Some(f), Some(i)) => (Some(f.frequency_of_bin(i)), dens.map(|d| d[i])),
            _ => (None, None),
        };
        let corr_ref = dens.zip(reference).and_then(|(d, r)| correlation(d, r));
        let corr_prev = dens
            .zip(
                self.prev_ref
                    .as_ref()
                    .filter(|(w, _)| (w - window).abs() > 0.5),
            )
            .and_then(|(d, (_, r))| correlation(d, r));
        if k < 3
            && corr_ref
                .zip(corr_prev)
                .is_some_and(|(same, prev)| prev > 0.5 && prev > same)
        {
            self.stale_suspects += 1;
        }
        self.polls_csv.row(&[
            utc_string(rec.t_ms),
            rec.t_ms.to_string(),
            phase.to_owned(),
            ((slot_ms - minute_start(slot_ms)) / self.dwell_ms()).to_string(),
            format!("{window:.0}"),
            k.to_string(),
            rec.latency_ms.to_string(),
            u8::from(frame.is_some()).to_string(),
            opt(status
                .and_then(|s| s.radio_frequency_hz)
                .map(|f| format!("{f:.0}"))),
            opt(samples),
            opt(since_prev),
            opt(frame.map(|f| format!("{:.4}", f.step_db))),
            f1(dens.and_then(|d| quantile_of(d, 0.5))),
            f1(dens.and_then(|d| quantile_of(d, 0.1))),
            opt(peak_hz.map(|f| format!("{f:.0}"))),
            f1(peak_db),
            opt(corr_ref.map(|c| format!("{c:.3}"))),
            opt(corr_prev.map(|c| format!("{c:.3}"))),
        ])
    }

    fn flush_minute(&mut self, duty: &str) -> Result<()> {
        if let Some(acc) = self.acc.take() {
            acc.write(&self.plan, duty, &mut self.noise_csv)?;
        }
        Ok(())
    }
}

/// Probes the front end.
async fn front_end(client: &Ka9qClient) -> Result<FrontEnd> {
    let st = client.probe().await.context("probing radiod")?;
    FrontEnd::from_status(&st)
}

fn spectrum_params(c: &Common, center: f64) -> SpectrumParams {
    SpectrumParams {
        avg: c.avg,
        ..SpectrumParams::new(center, c.bins, c.rbw)
    }
}

async fn open_listener(
    client: &Ka9qClient,
    l: Option<&(f64, String)>,
) -> Result<Option<AudioChannel>> {
    let Some((freq, mode)) = l else {
        return Ok(None);
    };
    let ch = client
        .open_audio_channel(AudioChannelParams::new(*freq, mode))
        .await
        .context("opening the listener channel")?;
    eprintln!("listener channel SSRC {} at {freq} Hz {mode}", ch.ssrc());
    // Drain its audio so the queue never matters.
    let rx = ch.receiver();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    Ok(Some(ch))
}

/// `measure hop`.
pub async fn run_hop(
    client: &Ka9qClient,
    args: &HopArgs,
    pid_fallback: Option<u32>,
) -> Result<Vec<(&'static str, String)>> {
    let stop = Stop::install();
    let c = &args.common;
    let plan = make_plan(c, front_end(client).await?)?;
    let mut hop = Hopper::new(plan.clone(), c.clone())?;
    let mut health = Csv::create(
        &c.out_dir,
        "health.csv",
        "minute_utc,radiod_channels,radiod_rss_kb,radiod_cpu_pct,polls,failures,recreations,stale_suspects",
    )?;
    let listener = open_listener(client, args.listener.as_ref()).await?;
    let (mut sp, _) = client
        .open_polled_spectrum(spectrum_params(c, plan.windows[0]))
        .await?;
    eprintln!("measurement channel SSRC {}", sp.ssrc());
    let start = now_ms();
    #[allow(clippy::cast_possible_truncation, reason = "bounded by MAX_RUN_SECS")]
    let end = start + (args.secs * 1000.0) as i64;
    let dwell = hop.dwell_ms();
    let mut slot = (start / dwell + 1) * dwell;
    let mut last_health = (
        now_ms(),
        radiod_usage(radiod_pid(c.radiod_pidfile.as_deref(), pid_fallback)).1,
    );
    let mut cur_minute = minute_start(slot);
    while slot < end && !stop.requested() {
        hop.dwell(
            &mut sp,
            slot,
            "hop",
            (i64::MIN, i64::MAX),
            "continuous",
            &stop,
        )
        .await?;
        let n0 = listener
            .as_ref()
            .and_then(AudioChannel::status)
            .and_then(|st| st.noise_density_db);
        if let (Some(n0), Some(acc)) = (n0, hop.acc.as_mut()) {
            acc.listener_n0.push(n0);
        }
        slot += dwell;
        if minute_start(slot) != cur_minute {
            hop.flush_minute("continuous")?;
            // Health once a minute: channels in radiod, its memory and CPU.
            let chans = client.dump_all(Duration::from_millis(300)).await?.len();
            let pid = radiod_pid(c.radiod_pidfile.as_deref(), pid_fallback);
            let (rss, ticks) = radiod_usage(pid);
            let now = now_ms();
            let cpu_pct = match (ticks, last_health.1) {
                (Some(a), Some(b)) => Some(cpu::percent_of_one_core(
                    a.saturating_sub(b),
                    Duration::from_millis(u64::try_from(now - last_health.0).unwrap_or(1)),
                )),
                _ => None,
            };
            last_health = (now, ticks);
            health.row(&[
                utc_string(cur_minute),
                chans.to_string(),
                opt(rss),
                opt(cpu_pct.map(|p| format!("{p:.2}"))),
                hop.polls.to_string(),
                hop.failures.to_string(),
                sp.recreations().to_string(),
                hop.stale_suspects.to_string(),
            ])?;
            health.flush()?;
            cur_minute = minute_start(slot);
        }
    }
    hop.flush_minute("continuous")?;
    let recreations = sp.recreations();
    sp.close().await?;
    if let Some(l) = listener {
        l.close().await?;
    }
    Ok(vec![
        ("cmd", "measure-hop".into()),
        ("windows", plan.windows.len().to_string()),
        ("polls", hop.polls.to_string()),
        ("poll_failures", hop.failures.to_string()),
        ("recreations", recreations.to_string()),
        ("stale_suspects", hop.stale_suspects.to_string()),
        ("grid_hz", opt(plan.grid_hz)),
        ("out_dir", c.out_dir.display().to_string()),
    ])
}

const RUNS_HEADER: &str = "minute_utc,open_utc,create_ms,first_poll_ms,first_poll_fresh_samples,\
gap_ms,first_frame_ms,first_frame_fresh_samples,warmup_polls,polls,failures,closed_utc,close_ok,\
channels_after_close,radiod_rss_kb,radiod_cpu_pct_run,radiod_cpu_pct_gap,ad_over";
const WARMUP_HEADER: &str = "minute_utc,t_since_open_ms,fresh_samples,frame,step_db,\
median_db_hz,p10_db_hz,fft_n,avg";

/// `measure idle`.
#[allow(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    reason = "one schedule loop, logged step by step"
)]
pub async fn run_idle(
    client: &Ka9qClient,
    args: &IdleArgs,
    pid_fallback: Option<u32>,
) -> Result<Vec<(&'static str, String)>> {
    let stop = Stop::install();
    let c = &args.common;
    if args.period_s < 60 || args.period_s % 60 != 0 {
        bail!("--period-s must be a multiple of 60");
    }
    let plan = make_plan(c, front_end(client).await?)?;
    let mut hop = Hopper::new(plan.clone(), c.clone())?;
    let mut runs = Csv::create(&c.out_dir, "runs.csv", RUNS_HEADER)?;
    let mut warm = Csv::create(&c.out_dir, "warmup.csv", WARMUP_HEADER)?;
    let start = now_ms();
    #[allow(clippy::cast_possible_truncation, reason = "bounded by MAX_RUN_SECS")]
    let end = start + (args.secs * 1000.0) as i64;
    let pid = || radiod_pid(c.radiod_pidfile.as_deref(), pid_fallback);
    let (mut n_ok, mut n_failed, mut leaked) = (0u64, 0u64, 0u64);
    let mut last_close: Option<(i64, Option<u64>, Option<u64>)> = None; // (t, samples, ticks)
    let soak_ticks0 = radiod_usage(pid()).1;
    let warmup_poll = i64::try_from(args.warmup_poll_ms.max(20)).unwrap_or(100);
    loop {
        let m = next_measuring_minute(now_ms(), args.period_s, args.lead_ms);
        if m + measure::MINUTE_MS > end || stop.requested() {
            break;
        }
        debug_assert!(is_measuring_minute(m, args.period_s));
        if !stop.sleep_until(m - args.lead_ms).await {
            break;
        }
        let (_, ticks_open) = radiod_usage(pid());
        let t_open = now_ms();
        let opened = client
            .open_polled_spectrum(spectrum_params(c, hop.window_of_slot(m)))
            .await;
        let t_created = now_ms();
        let (mut sp, created) = match opened {
            Ok(v) => v,
            Err(e) => {
                n_failed += 1;
                eprintln!("{}: open failed: {e}", utc_string(m));
                runs.row(&[utc_string(m), utc_string(t_open), String::new()])?;
                runs.flush()?;
                continue;
            }
        };
        // radiod's create reply carries INPUT_SAMPLES 0; the front end's sample
        // counter is read from the polls. Samples counted since the previous
        // run's last poll are fresh: the front end stopped in between.
        let s_prev = last_close.and_then(|l| l.1);
        let mut first_poll: Option<(i64, Option<i128>)> = None;
        let gap_ms = last_close.map(|l| t_open - l.0);
        let cpu_gap = match (ticks_open, last_close.and_then(|l| l.2), gap_ms) {
            (Some(a), Some(b), Some(g)) if g > 0 => Some(cpu::percent_of_one_core(
                a.saturating_sub(b),
                Duration::from_millis(u64::try_from(g).unwrap_or(1)),
            )),
            _ => None,
        };
        // Warm-up: poll fast until the minute starts.
        let mut first_frame: Option<(i64, Option<i128>)> = None;
        let mut warm_polls = 0u32;
        let mut t = t_created;
        while t < m - warmup_poll && !stop.requested() {
            let res = sp.poll().await;
            let tr = now_ms();
            warm_polls += 1;
            if let Ok(r) = &res {
                let ds = r
                    .status
                    .input_samples
                    .zip(s_prev)
                    .map(|(a, b)| i128::from(a) - i128::from(b));
                first_poll.get_or_insert((tr - t_open, ds));
                let dens = r.frame.as_ref().map(densities);
                if r.frame.as_ref().is_some_and(|f| f.step_db > 0.0) && first_frame.is_none() {
                    first_frame = Some((tr - t_open, ds));
                }
                warm.row(&[
                    utc_string(m),
                    (tr - t_open).to_string(),
                    opt(ds),
                    u8::from(r.frame.is_some()).to_string(),
                    opt(r.frame.as_ref().map(|f| format!("{:.4}", f.step_db))),
                    f1(dens.as_deref().and_then(|d| quantile_of(d, 0.5))),
                    f1(dens.as_deref().and_then(|d| quantile_of(d, 0.1))),
                    opt(r.status.spectrum_fft_n),
                    opt(r.status.spectrum_avg),
                ])?;
            }
            t += warmup_poll;
            stop.sleep_until(t).await;
        }
        warm.flush()?;
        let polls_before = hop.polls;
        let failures_before = hop.failures;
        let ad_over_start = created.status.ad_over;
        // The measuring minute: dwell slots from m to m + 60 s.
        let dwell = hop.dwell_ms();
        let mut slot = m;
        while slot < m + measure::MINUTE_MS && !stop.requested() {
            hop.dwell(
                &mut sp,
                slot,
                "idle",
                (m, m + measure::MINUTE_MS),
                "idle",
                &stop,
            )
            .await?;
            slot += dwell;
        }
        hop.flush_minute("idle")?;
        // Close after the tail.
        stop.sleep_until(m + measure::MINUTE_MS + args.tail_ms)
            .await;
        let last = sp.poll().await.ok();
        let last_samples = last.as_ref().and_then(|r| r.status.input_samples);
        let ad_over = last
            .as_ref()
            .and_then(|r| r.status.ad_over)
            .zip(ad_over_start)
            .map(|(a, b)| a.saturating_sub(b));
        let close_ok = sp.close().await.is_ok();
        let t_close = now_ms();
        let (_, ticks_close) = radiod_usage(pid());
        // radiod needs a frame or two to tear the channel down.
        stop.sleep_until(t_close + 1000).await;
        let chans = client.dump_all(Duration::from_millis(500)).await?.len();
        if chans != 0 {
            leaked += 1;
        }
        let (rss, _) = radiod_usage(pid());
        let cpu_run = match (ticks_close, ticks_open) {
            (Some(a), Some(b)) => Some(cpu::percent_of_one_core(
                a.saturating_sub(b),
                Duration::from_millis(u64::try_from(t_close - t_open).unwrap_or(1)),
            )),
            _ => None,
        };
        runs.row(&[
            utc_string(m),
            utc_string(t_open),
            (t_created - t_open).to_string(),
            opt(first_poll.map(|f| f.0)),
            opt(first_poll.and_then(|f| f.1)),
            opt(gap_ms),
            opt(first_frame.map(|f| f.0)),
            opt(first_frame.and_then(|f| f.1)),
            warm_polls.to_string(),
            (hop.polls - polls_before).to_string(),
            (hop.failures - failures_before).to_string(),
            utc_string(t_close),
            u8::from(close_ok).to_string(),
            chans.to_string(),
            opt(rss),
            opt(cpu_run.map(|p| format!("{p:.2}"))),
            opt(cpu_gap.map(|p| format!("{p:.3}"))),
            opt(ad_over),
        ])?;
        runs.flush()?;
        n_ok += 1;
        last_close = Some((t_close, last_samples, ticks_close));
    }
    let soak_ticks1 = radiod_usage(pid()).1;
    let wall = Duration::from_millis(u64::try_from(now_ms() - start).unwrap_or(0));
    let avg_cpu = soak_ticks0
        .zip(soak_ticks1)
        .map(|(a, b)| cpu::percent_of_one_core(b.saturating_sub(a), wall));
    Ok(vec![
        ("cmd", "measure-idle".into()),
        ("runs", n_ok.to_string()),
        ("failed_opens", n_failed.to_string()),
        ("runs_with_leaked_channels", leaked.to_string()),
        ("poll_failures", hop.failures.to_string()),
        ("stale_suspects", hop.stale_suspects.to_string()),
        ("wall_s", wall.as_secs().to_string()),
        (
            "radiod_cpu_pct_avg",
            opt(avg_cpu.map(|p| format!("{p:.3}"))),
        ),
        ("out_dir", c.out_dir.display().to_string()),
    ])
}

/// `measure rbw`.
pub async fn run_rbw(client: &Ka9qClient, args: &RbwArgs) -> Result<Vec<(&'static str, String)>> {
    let c = &args.common;
    std::fs::create_dir_all(&c.out_dir)?;
    let fe = front_end(client).await?;
    let mut csv = Csv::create(
        &c.out_dir,
        "rbw.csv",
        "t_utc,round,rbw_req,poll,latency_ms,frame,reported_rbw,fft_n,avg,span_hz,step_db,\
median_db_hz,p10_db_hz,peak_hz,expected_center_hz",
    )?;
    let (mut sp, _) = client
        .open_polled_spectrum(spectrum_params(c, args.center))
        .await?;
    let poll = Duration::from_millis(c.poll_ms);
    let mut failures = 0u32;
    for round in 0..args.repeat {
        for &rbw in &args.sequence {
            for k in 0..=args.polls {
                if k > 0 {
                    tokio::time::sleep(poll).await;
                }
                let t0 = now_ms();
                let res = if k == 0 {
                    sp.set_resolution_bw(rbw).await
                } else {
                    sp.poll().await
                };
                let t1 = now_ms();
                let Ok(r) = res else {
                    failures += 1;
                    eprintln!("rbw {rbw}, poll {k}: {}", res.unwrap_err());
                    continue;
                };
                let dens = r.frame.as_ref().map(densities);
                let peak = r
                    .frame
                    .as_ref()
                    .zip(dens.as_deref().and_then(peak_index))
                    .map(|(f, i)| f.frequency_of_bin(i));
                let expected = measure::wideband_center(
                    args.center,
                    fe.lo_hz,
                    fe.samprate,
                    fe.blocksize,
                    fe.fir_length,
                    f64::from(rbw),
                );
                csv.row(&[
                    utc_string(t1),
                    round.to_string(),
                    rbw.to_string(),
                    k.to_string(),
                    (t1 - t0).to_string(),
                    u8::from(r.frame.is_some()).to_string(),
                    opt(r.status.resolution_bw_hz),
                    opt(r.status.spectrum_fft_n),
                    opt(r.status.spectrum_avg),
                    opt(r.status.resolution_bw_hz.map(|w| w * f64::from(c.bins))),
                    opt(r.frame.as_ref().map(|f| format!("{:.4}", f.step_db))),
                    f1(dens.as_deref().and_then(|d| quantile_of(d, 0.5))),
                    f1(dens.as_deref().and_then(|d| quantile_of(d, 0.1))),
                    opt(peak.map(|p| format!("{p:.0}"))),
                    opt(expected.map(|e| format!("{e:.0}"))),
                ])?;
            }
        }
    }
    csv.flush()?;
    // radiod is still alive and answers.
    let alive = sp.poll().await.is_ok();
    sp.close().await?;
    Ok(vec![
        ("cmd", "measure-rbw".into()),
        ("failures", failures.to_string()),
        ("radiod_alive", alive.to_string()),
        ("out_dir", c.out_dir.display().to_string()),
    ])
}

/// `measure hold`.
pub async fn run_hold(client: &Ka9qClient, args: &HoldArgs) -> Result<Vec<(&'static str, String)>> {
    let stop = Stop::install();
    let listener = open_listener(client, args.listener.as_ref()).await?;
    let (mut sp, _) = client
        .open_polled_spectrum(SpectrumParams::new(args.center, args.bins, args.rbw))
        .await?;
    let start = now_ms();
    #[allow(clippy::cast_possible_truncation, reason = "bounded by MAX_RUN_SECS")]
    let end = start + (args.secs * 1000.0) as i64;
    let every = i64::try_from(args.poll_ms).unwrap_or(2000);
    let mut t = start;
    let mut polls = 0u32;
    while t < end && stop.sleep_until(t).await {
        sp.poll().await?;
        polls += 1;
        t += every;
    }
    sp.close().await?;
    if let Some(l) = listener {
        l.close().await?;
    }
    Ok(vec![
        ("cmd", "measure-hold".into()),
        ("polls", polls.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        assert_eq!(parse_band("40m:7e6:7.3e6").unwrap(), Band::forty());
        assert!(parse_band("4 0m:7e6:7.3e6").is_err());
        assert!(parse_band("40m:7.3e6:7e6").is_err());
        assert!(parse_band("40m").is_err());
        assert_eq!(
            parse_listener("7.0475e6:usb").unwrap(),
            (7.0475e6, "usb".into())
        );
        assert!(parse_listener("7e6:u sb").is_err());
        assert!(parse_hz("-1").is_err());
        assert_eq!(Band::forty().to_string(), "40m:7000000:7300000");
    }

    fn rec(dens: Option<Vec<f32>>) -> PollRec {
        PollRec {
            t_ms: 0,
            latency_ms: 0,
            reply: None,
            dens,
        }
    }

    #[test]
    fn the_dwell_reference_averages_the_late_polls() {
        let polls = vec![
            rec(Some(vec![100.0, 100.0])),
            rec(None),
            rec(Some(vec![100.0, 100.0])),
            rec(Some(vec![1.0, 3.0])),
            rec(Some(vec![3.0, 5.0])),
        ];
        assert_eq!(dwell_reference(&polls, usize::MAX), Some(vec![2.0, 4.0]));
        // Leave-one-out: poll 3 is compared with poll 4 only.
        assert_eq!(dwell_reference(&polls, 3), Some(vec![3.0, 5.0]));
        assert_eq!(dwell_reference(&polls[..3], usize::MAX), None);
    }
}
