<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Spike results: the first measurements

The spike is a Rust client that drives a local ka9q-radio `radiod`, pinned at `384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd`. These are its first measurements. It covers:
- TLV codegen;
- Opus audio to a `.opus` file;
- spectrum rows to uint8 waterfall rows at 10 rows/s;
- CPU measurement.

**Test environment.** All numbers below come from `scripts/e2e.sh` and the spike, run in a Debian bookworm container on 2026-09-29:
- x86-64 on a Ryzen 7 PRO 8840U;
- radiod with the signal-generator config `testdata/radiod-siggen.conf`: 24 MS/s real, a 10 MHz carrier at −20 dBFS, AM-modulated 50 % by 1 kHz, over −30 dBFS white noise.

**No Raspberry Pi was available.** None of these numbers is a Pi measurement. The procedure for the Pi is at the end of this document.

A real **RTL-SDR** (RTL2832U + R820T, no antenna) was later run through the same client and spike in the container via USB passthrough. See [Real hardware: RTL-SDR on x86](#real-hardware-rtl-sdr-on-x86).

## What was checked

| Check | Status | Evidence |
|---|---|---|
| TLV codegen is in place | **Done** | `backend-ka9q/build.rs` generates the tag enums from the vendored, SHA-256-pinned `status.h` and `rtp.h`. |
| Audio plays from a spike-created channel | **Done** (signal generator) | `ka9q-spike audio` produced an Ogg Opus file. `opusinfo` accepts it: 5.013 s, 20 ms packets. `opusdec` decodes it. The decoded audio has a **1000.0 Hz** dominant tone at −24.4 to −24.7 dBFS, 62–68 dB above 700 and 1300 Hz (three runs). |
| Waterfall rows are dumped at 10 rows/s | **Done** (signal generator) | 100 rows in 10.00 s (**10.00 rows/s**), 0 poll failures. The peak is at bin 512 of 1024, which is exactly **10.000 MHz**. |
| CPU on a Raspberry Pi | **Not done** | No Pi was available. Measured on x86 with the signal generator ([CPU](#cpu)) and with a real RTL-SDR ([below](#real-hardware-rtl-sdr-on-x86)). Procedure: [below](#procedure-cpu-on-a-raspberry-pi-4-with-an-rtl-sdr). |

## What the end-to-end tests check

`crates/ka9q-spike/tests/e2e.rs` is run by `scripts/e2e.sh`. All five tests passed.

| Test | Checks |
|---|---|
| `audio_records_a_playable_opus_file_with_the_1khz_tone` | ≥ 240 packets in 5 s, ≤ 2 lost; `opusinfo` accepts the file with 20 ms packets; `opusdec` produces 48 kHz WAV; a Goertzel scan from 100 Hz to 4 kHz finds the dominant tone within 5 Hz of 1 kHz and ≥ 20 dB above 700/1300 Hz. |
| `waterfall_dumps_ten_rows_per_second_with_the_carrier_at_10mhz` | ≥ 95 rows in 10 s; `inspect` of the row file matches the row count; the peak bin is 511–513 (10 MHz ± 1 bin); the PGM image is written. |
| `closed_dropped_and_expired_channels_stop_sending` | After `close()` and after `drop`: 0 RTP and 0 status packets for the SSRC in 1.5 s, observed passively. Before close: 50 packets/s. A raw create with `LIFETIME 25` that is never refreshed sends RTP at first, then nothing after it expires. `dump_all` does not re-create it. |
| `spectrum_bin_count_change_keeps_radiod_alive` | Changing 512 → 1024 bins creates a new SSRC, not an in-place `BIN_COUNT` change, which would crash radiod. Rows of 1024 bins follow and radiod still answers. `SPECTRUM_BASE` differs on each of 5 rows, confirming the per-poll auto-scaling. |
| `status_shows_the_signal_generator` | `status --probe` shows the `coaxhub-test sig_gen` front end at 24 MS/s real. |

## Measurements

### Waterfall (spectrum2 → our rows → zstd level 3, 10 rows/s unless noted)

Rows in this first run used the station quantization −160 dB + 0.5 dB × v. The rows have since been normalised to dB/Hz (see [the waterfall scale](#waterfall-scale-finding)). "Upload" is the compressed row payload only, without framing.

| Bins × RBW | Rows/s | Compressed row | Upload | Raw |
|---|---|---|---|---|
| 512 × 1 kHz | 10.00 | 281 B | 2.8 kB/s (22 kbit/s) | 5.1 kB/s |
| **1024 × 1 kHz** | **10.00** | **514 B** | **5.1 kB/s (41 kbit/s)** | 10.2 kB/s |
| 1024 × 100 Hz (narrowband) | 10.00 | 520 B | 5.2 kB/s (42 kbit/s) | 10.2 kB/s |
| 2048 × 1 kHz | 10.00 | 996 B | 10.0 kB/s (80 kbit/s) | 20.5 kB/s |
| 2048 × 1 kHz, 20 rows/s | 19.99 | 994 B | 19.9 kB/s (159 kbit/s) | 40.9 kB/s |

Polling behaviour:
- The first row arrives 24–36 ms after the channel opens.
- No poll ever timed out, with the default 250 ms timeout.
- The 4096 × 1 kHz run is left out because its span runs past the 12 MHz Nyquist edge of `sig_gen`, so it compresses unrealistically well.

The signal generator produces white Gaussian noise, which is close to the worst case for zstd. Real band noise with structure should compress at least as well. The results sit at the low end of an estimate of 50–150 kbit/s for one full-span waterfall.

### Audio (Opus over RTP, 20 ms frames)

| Case | Packets/s | Opus payload | RTP + UDP + IPv4 on the wire |
|---|---|---|---|
| AM, 12 kHz, `--bitrate 16000` | 50.1–50.2 | 14.0–16.2 kbit/s (1.75–2.0 kB/s) | 30–32 kbit/s |
| AM, template rate, encoder default bit rate | 50.2 | 13.7 kbit/s | 30 kbit/s |
| USB, 12 kHz, default bit rate | 50.3 | 13.1 kbit/s | 29 kbit/s |

- No packets were lost, duplicated or late in any run.
- Header overhead is 40 bytes per 20 ms packet, which is 16 kbit/s. That is as large as the payload. The station protocol therefore does not carry per-packet RTP-sized framing to the hub.

### CPU

CPU is given as a percentage of one core. The spike figures come from `--report`, which reads `/proc/<pid>/stat` utime+stime at `USER_HZ` 100. The radiod figures come from `pidstat`.

| Scenario | Spike | radiod |
|---|---|---|
| radiod, no channels (front end stopped) | — | **0.0 %** |
| 1 AM Opus channel, 5 s (e2e runs) | 0.0–0.2 % | 50.9–51.7 % |
| 1 spectrum channel, 1024 × 1 kHz at 10 rows/s, 10 s | 0.1–0.3 % | 50.0–50.4 % |
| Both at once, 10 s, one spike process each (two runs) | 0.1 % + 0.2–0.3 % | **51.5 % / 50.8 %** (`pidstat`); 50.8–51.6 % (`--report`) |

Per-thread breakdown of radiod in the "both at once" run (`pidstat -t`):

| Thread | CPU |
|---|---|
| `proc_siggen`: synthesizes 24 MS/s of noise and AM | **40.9–41.0 %** |
| `fft`: front-end forward FFT | 8.1–8.6 % |
| Both channel threads (demod + Opus encode; spectrum) | 1.6 % |

Most of radiod's CPU goes to the signal generator, not to the channels, so this number says little about a Pi.
- On real hardware, `proc_siggen` is replaced by the SDR driver's USB transfer and sample conversion.
- The `fft` cost scales with the input sample rate. An RTL-SDR runs at 1.8–2.4 MS/s instead of 24 MS/s, so roughly 10× less FFT work.

The spike itself is negligible: ≤ 0.3 % of one x86 core for Opus-to-Ogg plus requantize and zstd at 10 rows/s.

### Cross-compile

`cargo build --release --target aarch64-unknown-linux-gnu -p ka9q-spike`, with `CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc` and `AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar` set, builds in about 11 s.

| Property | Value |
|---|---|
| Binary size | 4.4 MB, or 3.2 MB stripped |
| Machine | AArch64 |
| Shared libraries needed | `libc`, `libm`, `libgcc_s` |
| Highest glibc symbol | `GLIBC_2.34` |

## Real hardware: RTL-SDR on x86

Run on 2026-09-29 in the container, with the dongle passed through (`/dev/bus/usb`). librtlsdr detached the host's `dvb_usb_rtl28xxu` driver by itself; nothing was needed on the host.
- Hardware: RTL2832U + Rafael Micro R820T. **No antenna connected.**
- Config: `testdata/radiod-rtlsdr.conf` (2.048 MS/s complex, fixed 98 MHz centre, gain 30 dB, no AGC).
- Commands:

```sh
RADIOD_CONF=testdata/radiod-rtlsdr.conf scripts/run-radiod-siggen.sh start
ka9q-spike --report audio --freq 98.3e6 --mode am --secs 10 --out am.opus
ka9q-spike --report waterfall --center 98e6 --bins 1024 --rbw 1800 --rate 10 --secs 10 --out rows.cxwf --pgm rows.pgm
```

| Check | Result |
|---|---|
| radiod with the `rtlsdr` front end | Starts, locks the tuner at 98 MHz, answers commands. |
| AM audio, 10 s | 503 packets (50.3/s), **0 lost**, 0 late, 12.0 kbit/s Opus payload. There is no signal to check (no antenna), so there is no tone assertion. |
| Waterfall, 1024 × 1.8 kHz, 10 s | **100 rows in 10.00 s**, 0 poll failures, 525 B/row compressed (5.2 kB/s). Peak at bin 512 = the dongle's DC spike at the tuner centre (expected). **0 saturated bins per row** with the default quantization (the signal generator gave 3). |
| WFM | 0 packets: radiod forces SNR squelch on for WFM (`wfm.c`), and there is no signal to open it. This exposed a client bug, now fixed: see below. |

**CPU** (`--report`, % of one x86 core; Ryzen 7 PRO 8840U):

| Scenario | Spike | radiod |
|---|---|---|
| 1 AM Opus channel, 10 s | 0.1 % | **2.8 %** |
| 1 spectrum channel, 1024 × 1.8 kHz at 10 rows/s, 10 s | 0.1 % | **1.8 %** |
| 1 WFM channel (squelched), 10 s | 0.0 % | 2.3–2.4 % |

With a real 2 MS/s front end radiod needs about 2–3 % of one x86 core, versus about 51 % with the 24 MS/s signal generator. This supports the expectation that a Pi 4 has plenty of headroom, but the Pi number still has to be measured.

**Client bug found and fixed.** radiod overrides the output sample rate for some modes (WFM always runs at 48 kHz). The client treated the mismatch as a lost channel and re-sent the create at every keepalive, which restarted the demodulator each time. The client now adopts the rate radiod reports and re-creates only on a real loss (wrong frequency or not Opus). Regression test: `radiod_sample_rate_override_does_not_restart_the_channel` (the fake radiod now models the WFM override). Verified on the dongle: 1 demodulator start across 2 keepalives, versus a restart at every keepalive before.

### HF with an antenna: 40 m via direct sampling

Same dongle, now on a 40 m antenna. It was run on 2026-09-29 around 22:22 UTC, and the band was quiet.
- Config: `testdata/radiod-rtlsdr-hf.conf`: Q-branch direct sampling (the RTL-SDR Blog V3 HF path), 2.048 MS/s, centre 7.1 MHz, covering about 6.08–8.12 MHz.
- Three spike processes ran at once for 30 s.

| Run | Result |
|---|---|
| USB audio at 7.074 MHz (FT8) | 1501 packets (50.0/s), **0 lost**, 13.5 kbit/s. The decoded spectrogram shows a clean 0–3 kHz passband, with a weak signal near 800 Hz (8–20 s) and a strong one near 1.45 kHz (from 23 s). |
| Wide waterfall, 1024 × 1.8 kHz around 7.1 MHz | 300 rows, **10.00 rows/s**, 0 poll failures, 684 B/row compressed (6.8 kB/s). **3.1 saturated bins per row** (0.3 % of values). |
| FT8 zoom, 512 × 6.25 Hz around 7.0755 MHz | 300 rows, **10.00 rows/s**, 299 B/row (3.0 kB/s). Peak at **7,074,812 Hz**, inside the FT8 audio passband. No saturation. |
| radiod CPU (all three at once) | **3.0 %** of one x86 core. |

### Waterfall scale finding

The finding led to rows normalised to dB/Hz, with a default of −130 dB/Hz and 0.5 dB steps (station protocol §6.2). radiod's spectrum values are **power per bin, not power density**: they shift by 10·log10(bin width).

| View | Noise floor (median) | Top of range | Values used |
|---|---|---|---|
| Wide (1.8 kHz bins) | −50.0 dB | radiod reported base −57.8 dB, step 0.143 dB, so the strongest bin is ≈ **−21 dB**. Our fixed scale clips at −32.5 dB. | 60 of 256 levels |
| Narrow (6.25 Hz bins) | −72.5 dB | about −42 dB | 148 levels |

The two noise floors differ by 22.5 dB, close to the expected 10·log10(1800/6.25) = 25 dB. Converted to dB/Hz the floors agree: about −82.6 and −80.5 dB/Hz. So a fixed per-subscription `Quantization` works across zoom levels only if the rows are **normalised to dB/Hz**. The fixed −160 dB/0.5 dB scale clips the wide view and wastes range in the narrow one.

**After the change** (rows normalised to dB/Hz, default −130 dB/Hz with 0.5 dB steps), the same antenna 15 s later:

| View | Noise floor (median) | Strongest | Saturated values |
|---|---|---|---|
| Wide (1.8 kHz bins) | **−82.5 dB/Hz** | −51.5 dB/Hz | **0** |
| FT8 zoom (6.25 Hz bins) | **−81.0 dB/Hz** | −69.0 dB/Hz | **0** |

Both views now share one scale, and the floors agree within 1.5 dB.

## Findings

1. **The first waterfall quantization clipped strong signals from radiod, and was replaced.**
   - radiod's spectrum2 dB scale puts the `sig_gen` noise floor at about −71 dB. The −20 dBFS carrier lands at about −19 dB.
   - The first scale, −160 dB + 0.5 dB × v, saturates at −32.5 dB.
   - Measured: 3 bins/row clip at 255 at 1 kHz RBW (carrier plus Hann leakage), and 5 at 100 Hz RBW (carrier plus both AM sidebands). Values below v ≈ 178 are never used.
   - The spike's peak detector therefore takes the middle of a run of clipped bins.
   - The fix is the one above: rows normalised to dB/Hz, with the offset at −130 dB/Hz.
2. **The spectrum create reply is enough to confirm a spectrum channel.** radiod's immediate reply to the create command already reports `DEMOD_TYPE 4` and the requested `BIN_COUNT`, although it carries no bin data yet. The client relies on this.
3. **Opus bit rate varies.** With `--bitrate 16000`, the payload measured 14.0–16.2 kbit/s across runs; with the default bit rate it was about 13–14 kbit/s. Budget about 16 kbit/s per channel plus station-protocol framing.
4. **Poll-rate headroom.** 2048-bin rows at 20 rows/s came back without failures, so the station protocol's 10 rows/s default has headroom. This agrees with [radiod-notes.md](radiod-notes.md) §6.

## Procedure: CPU on a Raspberry Pi 4 with an RTL-SDR

This measures the one thing the spike has not: the CPU cost on a Pi. Use a Pi 4 (4 GB or more) running Raspberry Pi OS bookworm 64-bit, with an RTL-SDR v3 or v4 and an antenna.

### 1. Build radiod on the Pi

Use the same pinned commit, with the RTL-SDR driver:

```sh
sudo apt install git build-essential pkg-config libbsd-dev libfftw3-dev libiniparser-dev \
  libopus-dev libogg-dev libsamplerate0-dev libusb-1.0-0-dev libncurses-dev librtlsdr-dev \
  sysstat opus-tools
git clone https://github.com/ka9q/ka9q-radio.git && cd ka9q-radio
git checkout 384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd
make -C src -j4 prefix=$HOME/.local/opt/ka9q-radio ENABLE_AIRSPY=0 ENABLE_AIRSPYHF=0 \
  ENABLE_BLADERF=0 ENABLE_FOBOS=0 ENABLE_FUNCUBE=0 ENABLE_HACKRF=0 ENABLE_HYDRASDR=0 \
  ENABLE_RX888=0 ENABLE_SDRPLAY=0 ENABLE_SOAPY=0 ENABLE_SIG_GEN=0 ENABLE_RTLSDR=1 \
  radiod rtlsdr.so tune metadump
```

Install `radiod`, `rtlsdr.so` and `share/presets.conf` into the prefix, as `scripts/build-radiod.sh` does. `radiod -V` must report the pinned hash.

### 2. Configure radiod

Create `radiod-rtlsdr.conf`. It is the test config with a real front end. Set `frequency` to the centre of a band with a known signal: here the 2 m band, or pick an airband or broadcast frequency.

```ini
[global]
hardware = rtlsdr
description = "coaxhub-pi rtlsdr"
status = 239.77.0.6
data = 239.77.0.4
ttl = 0
preset = am
advertise = no

[rtlsdr]
device = rtlsdr
description = "coaxhub-pi rtlsdr"
samprate = 1800000
frequency = 145m0
agc = yes
```

Before starting radiod:
- Run `sudo ip link set dev lo multicast on` once per boot.
- Unload the DVB driver if it is loaded: `sudo rmmod dvb_usb_rtl28xxu`.
- Record the Pi's state: `vcgencmd get_throttled` and `vcgencmd measure_temp`. Use the official PSU; undervoltage throttles the CPU and invalidates the numbers.

Start radiod:

```sh
~/.local/opt/ka9q-radio/sbin/radiod -v radiod-rtlsdr.conf &
```

### 3. Copy the cross-compiled spike

On the development machine (the README's cross-compiling section):

```sh
scp target/aarch64-unknown-linux-gnu/release/ka9q-spike pi@<pi>:
```

### 4. Measure

Run each scenario for at least 60 s. For each, record `pidstat -u -t -p $(pidof radiod) 60 1` and the spike's `RESULT` lines.

1. **radiod idle, no channels.** Run `pidstat` only.
2. **One audio channel.** Pick `F` inside 145 MHz ± 0.8 MHz, on a known signal:
   ```sh
   ./ka9q-spike --report audio --freq F --mode fm --secs 60 --out fm.opus
   ```
3. **One waterfall.** 1024 bins × 1 kHz spans 1.024 MHz, which fits in 1.8 MS/s:
   ```sh
   ./ka9q-spike --report waterfall --center 145e6 --bins 1024 --rbw 1000 --rate 10 --secs 60 --out rows.cxwf
   ```
4. **Scenarios 2 and 3 at the same time.**
5. **Optional, to find the limit:** 4 and 8 audio channels plus the waterfall, one spike process each.

For every scenario, record:
- radiod total and per-thread CPU (`rtlsdr`/USB, `fft`, channel threads);
- the spike's `spike_cpu_pct`, `rows_per_s` and upload bytes/s;
- the `vcgencmd get_throttled` value after the run.

Check that the audio plays (`opusinfo fm.opus`, then listen). Check the waterfall with `./ka9q-spike inspect rows.cxwf`.

### 5. Record the results

Add a "Raspberry Pi 4" column to the CPU table above and mark the CPU check done.
