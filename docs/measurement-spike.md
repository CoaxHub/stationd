<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Measurement spike: the noise floor with radiod

The noise-floor measurement runs one wideband spectrum channel in radiod, outside the listeners' channels: it hops between windows of the station's coverage, and every UTC minute stationd reduces what it saw to a few quantiles per band. While nobody listens, the channel is open only around one minute in five, so radiod's front end runs only then. This spike checks, with radiod at the pinned commit, a real RTL-SDR on a 40 m antenna and the signal generator, what that design depends on: that the channel can hop without stale or misplaced bins, what the statistic looks like on a real band, how the front end behaves when it is started and stopped every five minutes, and what it all costs in CPU. The radiod facts it found are in [radiod notes §11](radiod-notes.md#11-spectrum-channels-for-measurements).

## Summary

| Question | Answer |
|---|---|
| Spectra to discard after a hop | **None.** The retune's own reply is computed from samples taken after the move, at the new frequency. |
| Stale or misplaced bins | **None** in 3,748 hops over 2 h on each front end: the signal generator's carrier was never in a window without it and always in the right bin of a window with it. |
| Frequency mapping | Exact when each window's offset from the LO is a multiple of radiod's placement grid, 1 kHz at 500 Hz bins. Off the grid, one bin closer to the LO. |
| `RESOLUTION_BW` with a fixed `BIN_COUNT` | Safe (420 changes, radiod never failed). Between wideband values the change's own reply is valid; into or within narrowband mode it is wrong once. |
| Leaked channels | None: radiod's channel count stayed at what the spike held, every minute for 2 h, and was 0 after every idle-schedule close. |
| The 40 m statistic | p10 −84.4 dB/Hz (sd 0.19 dB over 125 minutes); 0.1 dB median change from one minute to the next (0.5 dB at most); 1.3 dB below radiod's own noise estimate; unmoved by the strongest signal in the spectrum. |
| Warm-up (create to first valid spectrum) | **68–79 ms** on the RTL-SDR (40 runs, median 69 ms; 15–111 ms on the signal generator), and that first spectrum is already valid. The idle schedule's warm-up drops from 5 s to 2 s. |
| Does the front end stop sampling? | **Yes.** radiod logs every start and stop, the sample counter does not advance between runs, radiod uses 0.000 % CPU in the gaps, and the USB read thread is new on every start. |
| CPU, x86 (% of one core) | Front end 1.7; continuous measuring adds 0.15; a listener adds 0.7; idle duty 0.38 on average. |

## 1. The hopping wideband channel

**Continuous runs** (`measure hop`, 2 h 05 min each, 4 s dwells, 2 polls/s):

| | RTL-SDR, 40 m | Signal generator |
|---|---|---|
| Windows (Hz) | 7,150,000 (40 m), 6,600,000, 7,660,000 | 10,000,000, 11,000,000, 9,600,000, 10,400,000 |
| Hops, polls | 1,874 hops, 15,000 polls | 1,874 hops, 15,000 polls |
| Failed polls, timeouts, re-creations | 0, 0, 0 | 0, 0, 0 |
| Replies without a spectrum, or with another centre | 0 | 0 |
| Reply latency (retune; polls) | 5–59 ms; 0–94 ms | 3–137 ms; 4–231 ms |
| radiod's channels, checked every minute | the spike's 2 (measurement and a listener) | the spike's 1 (2 while another test ran) |
| radiod RSS | 15.4 → 15.6 MB | 39.0 → 39.5 MB |
| radiod CPU | 2.5–3.4 % (with the listener) | 50–75 % (the generator itself) |

- **Stale bins.** On the signal generator, the 10 MHz carrier is in two of the four windows. In the 11,000 spectra of the windows without it, the strongest bin never reached −80 dB/Hz (the carrier is at −47 dB/Hz, the noise near −96); in the 4,000 spectra of the windows with it, the carrier was always in the 10,000,000 Hz bin. That includes every first reply after a hop.
- **On the RTL-SDR**, each spectrum's correlation (in dB/Hz, bin by bin) with the mean of the other late spectra of its dwell was 0.769 for the retune's reply and 0.767–0.775 for the 7 polls after it, and −0.005 to −0.001 with the previous window. Between polls the sample counter advanced by 786,432–1,179,648 (0.38–0.58 s of samples, in 64 ms USB transfers): every spectrum is new data.
- **Placement.** Windows centred at 10.0005 and 10.1005 MHz (off the 1 kHz grid) put the carrier in the bin labelled 10.0005 MHz; on the grid it was always exact (radiod notes §11 has the formula). The window plan snaps every centre to the grid.
- **`RESOLUTION_BW`** (`measure rbw`, 500 → 1000 → 250 → 2000 → 100 → 50 → 500 Hz, 7 polls each): on the signal generator, 60 rounds (420 changes): every wideband reply, first or not, had a median of −96.4 to −95.7 dB/Hz, and the carrier was always in its bin. Entering 100 Hz (narrowband) the first reply was 7.7 dB low and going on to 50 Hz 2.8 dB high, in every round; the next poll was right. The RTL-SDR (2 rounds) behaved the same (6.4 dB low, 2.9 dB high). radiod never failed.

## 2. The statistic on 40 m

From the RTL-SDR run, the 40 m window (7.0–7.3 MHz, 591 bins after the ±2 kHz around 7.1 MHz), 40 spectra a minute (5 of the minute's 15 dwells), on the evening of 2026-10-02, 22:10–00:14 UTC:

| Per minute (125 minutes) | min | median | max | sd |
|---|---|---|---|---|
| p10, dB/Hz | −84.6 | −84.4 | −83.8 | 0.19 |
| p25 | −83.4 | −83.2 | −82.6 | 0.15 |
| p50 | −82.1 | −81.8 | −81.1 | 0.19 |
| p90 | −79.4 | −79.1 | −78.1 | 0.29 |
| p90 − p10 (activity) | 5.2 | 5.5 | 6.0 | 0.17 |
| Change of p10 from one minute to the next | 0.0 | 0.1 | 0.5 | |

- **p10 tracks the noise, not the signals.** A waterfall of the band (the spike's `waterfall` mode, 4 rows/s for 150 s) shows FT8 near 7.075 MHz in its 15 s cycles and a few steady carriers over the noise: they occupy a small minority of the bins. Spectrum by spectrum, the window's 10th percentile does not follow its strongest signal (correlation −0.01 over the 5,000 spectra of the 40 m window; a median of −84.8 dB/Hz both in the spectra with a peak above −55 dB/Hz and in those with none above −60), while it does follow the window's median (0.90): what moves p10 is the floor under everything.
- **Against radiod's own estimate.** A USB channel at 7.0475 MHz, held for the whole run, reported a `NOISE_DENSITY` of −83.0 dB/Hz (median); p10 was 1.1 to 1.7 dB below it in every minute (1.33 dB on average, sd 0.14 dB). Both are uncalibrated, but they move together and sit close, and p10 lies slightly lower, as the 10th percentile of noise does.
- **Variance.** The band was quiet that evening (p90 − p10 about 5.5 dB). p10 moved by 0.1 dB from one minute to the next (median; 0.5 dB at most), as much as the median and less than p90. What a busy band does to p10 is not measured yet; it needs recordings on a busy band such as 20 m.
- **`SPECTRUM_AVG`.** Side by side for 11 minutes on the same window, 10 and 50 FFTs per spectrum gave the same p50 (−81.3 and −81.2 dB/Hz), but p10 was −84.1 at 10 and −83.3 at 50: more averaging narrows the spread of each bin. Both were equally steady (0.1 dB from minute to minute). The method sends `SPECTRUM_AVG` 10 explicitly and keeps it fixed.
- **Overloads.** `AD_OVER` grew in 17 of the 125 minutes, by 1 to 35 samples (of 123 million a minute); the quantiles did not move with it.
- **Whether this floor is the antenna's or the receiver's** can't be told from these data: the floor differs by about 5 dB between the three windows, so it is not the flat floor of the A/D alone, but no measurement was made with the antenna disconnected.
- The fixture [`testdata/noise-40m-sample.csv`](../testdata/noise-40m-sample.csv) holds 60 of these minutes and the 40 idle-duty minutes of the soak.

## 3. The idle schedule

`scripts/measurement-soak.sh` with the RTL-SDR (`measure idle`: period 300 s, the channel opened 5 s before each measuring minute and closed 0.5 s after it, polled every 100 ms during the warm-up, then the 40 m window at 2 polls/s for the minute), and the same on the signal generator:

| | RTL-SDR, 40 m | Signal generator |
|---|---|---|
| Ran | 3 h 21 min, 40 measuring minutes | 3 h 9 min, 38 measuring minutes |
| Front-end starts and stops in radiod's log | 41 and 41: the 40 runs and one at start-up for the readiness probes | 39 and 39: the 38 runs and one at start-up for the readiness probes |
| Failed opens, failed polls, timeouts | 0, 0, 0 | 0, 0, 0 |
| radiod restarts (the soak restarts it if it exits) | 0 | 0 |
| Channels left in radiod 1 s after each close | 0 in every run | 0 in every run |
| Open to first spectrum | 68–79 ms (median 69) | 15–111 ms (median 27) |
| Samples counted across each 234 s gap | 262,144–393,216 (two or three USB transfers) | 0.9–1.8 million (the generator catches up after a start) |
| radiod CPU in the gaps; during a run | 0.000–0.010 %; 1.63–1.75 % | 0.000–0.010 %; 49–71 % |
| radiod CPU averaged over the soak | 0.38 % (0.36 % expected with the 2 s warm-up) | 11.6 % |
| radiod RSS | 9.4–11.1 MB, flat | 34 → 113 MB: `sig_gen.c` leaks 2.9 MB per start (radiod notes §11) |

- **Warm-up.** radiod answers the first poll of a new channel when the front end has delivered its first frame; on the RTL-SDR that is one 64 ms USB transfer after the start, and the reply came 68–79 ms after the open (median 69 ms) in the 40 runs. That spectrum is already valid: compared with the median of its measuring minute, the spectra of the first 250 ms after the open deviate by 0.00 dB (median over the runs), the same as those 4 s later; no start-up transient shows. The idle schedule's warm-up therefore drops from 5 s to 2 s, which leaves margin for a slower start on a Pi.
- **The front end stops sampling.** Four independent signs: radiod logs `rtlsdr running` and `rtlsdr stopped` for every run; radiod's sample counter (`INPUT_SAMPLES`) moves by only 262,144–393,216 samples (two or three USB transfers) between the last poll of one run and the first poll of the next, against 480 million had the dongle kept streaming for the 234 s gap; radiod's CPU in the gaps is 0.000 %; and the RTL-SDR read thread is a new thread on every start (`pidstat -t`). The device stays open: the dongle is claimed and powered between runs, just not streaming. Its idle draw is not measured yet.
- **No stuck device and no leak.** Every close was answered, every next open started the front end again, and radiod's memory stayed flat with the RTL-SDR. The soak never saw the dongle fail; if it does, radiod exits (radiod notes §11) and the soak script restarts it, as systemd does on a station.
- **The minutes themselves.** 120 spectra per measuring minute (one window, 15 dwells); p10 −84.9 dB/Hz (sd 0.20 dB over the 40 minutes, 00:20–03:35 UTC), a median change of 0.0 dB from one measuring minute to the next (0.7 dB at most), the same behaviour as under continuous duty.

The 24-hour soak is the same script: `scripts/measurement-soak.sh OUT_DIR 24` (radiod must not be running already; set `STATUS_GROUP`/`DATA_GROUP` to keep it apart from another radiod). It leaves `runs.csv`, `warmup.csv`, `noise.csv`, `polls.csv`, radiod's logs and `summary.txt` in `OUT_DIR`.

## 4. CPU on x86

`scripts/measurement-cpu.sh` with the RTL-SDR on 40 m, 110 s of `pidstat` per scenario (% of one core of the Ryzen 7 PRO 8840U; a Raspberry Pi 4 costs several times more, and is not measured yet):

| Scenario | radiod | The spike (stationd's share) |
|---|---|---|
| No channel: front end stopped | 0.00 | — |
| Front end + one spectrum channel polled every 2 s | 1.74 | 0.03 |
| Continuous measuring: the channel hopping 3 windows, 2 polls/s | 1.89 | 0.07 |
| One listener (USB, Opus) | 2.47 | 0.12 (writing the Opus file) |
| The listener and continuous measuring | 2.65 | 0.15 |
| Idle duty, averaged over the soak | 0.38 (5 s warm-up) | ≈ 0.02 (estimated from continuous duty) |

Per thread, the front end is radiod's `fft` thread (1.2 %, the 51,200-point forward FFT 50 times a second) and the RTL-SDR read thread (0.5 %); the spectrum channel's own thread stays under 0.05 %.

**The costs, ranked:** the front end (≈ 1.7 %) ≫ a listener's channel (≈ 0.7 %: demodulation and Opus) ≫ the measurement channel with its polls (≈ 0.15 %; 0.2 % on top of a listener) ≫ stationd's reduction (under 0.1 %). Measuring while someone listens costs almost nothing; measuring while nobody listens costs the front end, which the idle duty cuts to about a fifth of the time.

## Set-up

- **Host:** a Debian bookworm container (kernel 7.2) on a Ryzen 7 PRO 8840U, 16 threads. Other builds ran on the host at the same time; CPU figures are radiod's own time (`pidstat`, `/proc/<pid>/stat`), which other load barely affects.
- **radiod** at `384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd`, each instance with its own status and data groups (so no other radiod on the host could answer).
- **RTL-SDR:** RTL2832U + R820T ("RTL2838UHIDIR"), `testdata/radiod-rtlsdr-hf.conf`: Q-branch direct sampling, 2.048 MS/s, centre 7.1 MHz, no AGC. radiod reports a tunable range of 6,137,440–8,062,560 Hz (±47 % of the sample rate); without its outer 5 % the coverage is 6.234–7.966 MHz. **Antenna:** a 40 m inverted-V dipole.
- **Signal generator:** `testdata/radiod-siggen.conf`: 24 MS/s real, a 10 MHz carrier at −20 dBFS over −30 dBFS of white noise. The carrier makes misplaced or stale bins visible.
- **The channel:** spectrum2, `BIN_COUNT` 1024 for its whole life, `RESOLUTION_BW` 500 Hz (a 512 kHz window), radiod's default `SPECTRUM_AVG` 10. Window centres on the 1 kHz placement grid from the front end's LO (radiod notes §11).
- **The cycle:** dwell slots of 4 s aligned to the UTC minute (15 per minute); slot `s` measures window `s mod n` of the `n` windows. In each slot the first command is the retune (`RADIO_FREQUENCY`) and then one poll every 500 ms: 8 spectra per slot, the retune's own reply included.
- **The statistic:** each bin's power (`SPECTRUM_BASE + v × SPECTRUM_STEP`, from that reply's own scale) minus `10·log10(RESOLUTION_BW)`, which is the waterfall's dB/Hz (station protocol §6.2). Every bin of every spectrum whose centre lies in the band (7.0–7.3 MHz for 40 m) and outside ±2 kHz of the front end's centre goes into one histogram per band and UTC minute, with 0.1 dB steps; the minute's row has its 10th, 25th, 50th and 90th percentiles (nearest rank, bin centres).

## The code

- `backend_ka9q::client::PolledSpectrum` (`Ka9qClient::open_polled_spectrum`): a spectrum channel its owner polls, retunes (`retune`) and re-sizes (`set_resolution_bw`), one command at a time, with no background task, so the owner knows which reply follows which retune. Each reply comes with radiod's full status (`INPUT_SAMPLES`, `AD_OVER`, the front end). Tested against the fake radiod.
- `ka9q-spike measure` ([`measure.rs`](../crates/ka9q-spike/src/measure.rs), pure, unit-tested: coverage, the placement grid and radiod's real placement, window planning, dB/Hz, the histogram and its quantiles, the UTC schedule; [`measure_run.rs`](../crates/ka9q-spike/src/measure_run.rs): the modes):
  - `hop`: continuous duty; logs every poll (`polls.csv`), every minute's quantiles per band (`noise.csv`) and radiod's channels, memory and CPU once a minute (`health.csv`). `--listener FREQ:PRESET` also holds an audio channel and records radiod's `NOISE_DENSITY` for it.
  - `idle`: the idle schedule (§3); `runs.csv` and `warmup.csv` as well.
  - `rbw`: steps `RESOLUTION_BW` on a live channel; `hold`: one channel kept alive (a CPU baseline).
- [`scripts/measurement-cpu.sh`](../scripts/measurement-cpu.sh): the CPU scenarios. [`scripts/measurement-soak.sh`](../scripts/measurement-soak.sh): the idle soak, with radiod supervised (restarted if it exits). Both can run on a Raspberry Pi as well.
- [`testdata/noise-40m-sample.csv`](../testdata/noise-40m-sample.csv): per-minute quantiles on 40 m from the RTL-SDR, 60 minutes of continuous duty and 40 measuring minutes of idle duty, a fixture for the noise-floor reduction's tests (a `#` comment line starts it).
