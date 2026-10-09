<!-- SPDX-License-Identifier: GPL-3.0-only -->

# radiod notes: test setup and client contract

These are observed facts about ka9q-radio `radiod` at the pinned commit `384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd` (2026-09-29). The station ka9q backend is coded against this document.

- Source references are `file:line` in upstream `src/` at the pinned commit.
- Observations were made in a Debian bookworm container: kernel 7.2, Ryzen 7 PRO 8840U, 16 threads.
- `P=scripts/ka9q-probe.py` is a stdlib-only Python helper. It sends CMD packets and decodes STATUS packets over loopback with TTL 0, the same way the station client must. Its tag numbers are parsed from the pinned `status.h`. It exists only because no upstream tool sends `DEMOD_TYPE 4`. Everything else was checked with upstream tools: `tune`, `metadump`, `pcmrecord`, plus `tcpdump`, `opusinfo` and `sox`.

## 1. Build and run

```sh
scripts/build-radiod.sh                  # idempotent; ~15 s first time, 0.1 s after
scripts/run-radiod-siggen.sh             # start in background, wait until ready (~0.3 s)
scripts/run-radiod-siggen.sh status
scripts/run-radiod-siggen.sh stop        # SIGTERM; radiod exits after ~1 s
```

### What `build-radiod.sh` does

1. It installs the apt packages with sudo.
   - **Build:** `git build-essential pkg-config libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libogg-dev libsamplerate0-dev libusb-1.0-0-dev libncurses-dev librtlsdr-dev`.
   - **Tools:** `sox iproute2 tcpdump opus-tools sysstat shellcheck rtl-sdr`.
   - Why `libusb-1.0-dev` is needed: `radiod` links `-lusb-1.0` unconditionally (`src/Makefile`, `radiod:` rule).
   - No SDR vendor library is needed for the signal generator. Every `ENABLE_*` driver except `ENABLE_SIG_GEN` and `ENABLE_RTLSDR` is set to 0.
2. It clones the repo into `~/.cache/coaxhub/ka9q-radio` and checks out the pinned commit.
3. It builds only `radiod`, `control`, `metadump`, `tune`, `pcmrecord`, `powers`, `sig_gen.so` and `rtlsdr.so`, then installs them **user-owned** into `~/.local/opt/ka9q-radio`.
   - The prefix is compiled in (`config_paths.h`): `PKGLIBDIR=$prefix/lib/ka9q-radio`, which holds `sig_gen.so` and `rtlsdr.so`, and `PKGDATADIR=$prefix/share/ka9q-radio`, which holds `presets.conf`.
   - Override the locations with `KA9Q_SRC` and `KA9Q_PREFIX`.
   - `sudo make install` is not used. It would create a `radio` user and systemd/udev files we don't need.
   - `opusd` and `monitor` are not built. `monitor` needs portaudio and a sound card. `opusd` isn't needed.
4. It sets the **MULTICAST flag on `lo`**. This is required, see §2.
5. It checks that `radiod -V` reports the pinned hash.

### What `run-radiod-siggen.sh` does

- It runs `radiod -v testdata/radiod-siggen.conf` as the calling user, with no sudo.
- It writes the log to `${XDG_RUNTIME_DIR:-/tmp}/coaxhub-radiod/radiod.log` and the pidfile to `radiod.pid` in the same directory.
- Readiness takes two steps:
  1. It waits for the log line `0 static demodulators started`. `radiod` prints it after the control socket is bound (`main.c` after `loadconfig()`; `radio.c:484`).
  2. radiod sends no status until a channel exists. So the script runs `tune --lifetime 50` on the probe SSRC 999999999 and waits for its status reply. The probe channel self-destructs after 1 s.
- `stop` treats a zombie as dead. See the §9 note on zombies.

## 2. Multicast addresses, ports, TTL, interface

| What | Address | Source |
|---|---|---|
| Status + control group | `239.77.0.6:5006` | `[global] status = 239.77.0.6`; numeric, used verbatim (`radio.c:356`), port `DEFAULT_STAT_PORT` (`rtp.h:11`) |
| RTP data (all dynamic channels) | `239.77.0.4:5004` | `[global] data = 239.77.0.4` (`modes.c:474`), port `DEFAULT_RTP_PORT` (`rtp.h:9`) |
| Per-channel status copy | `239.77.0.4:5006` | status socket = data address with port 5006 (`modes.c:499`) |

Addressing rules:
- Numeric IPv4 and IPv6 are accepted for `status` and `data`.
- A name (not numeric) is not resolved unless `dns = yes`. Instead it is hashed into 239.x.y.z with `make_maddr()` (`radio.c:374`, `multicast.c:831`). No avahi is needed either way.
- `advertise = no` stops radiod from calling avahi.
- `data =` must not include a `:port`. A value like `239.77.0.4:5004` fails `inet_pton` and gets hashed.

Interface and TTL:
- **TTL 0 means loopback only.** `ttl = 0` is set in `[global]`; it is also the default (`defaults.h:14`).
- Channel output and status go through `Output_fd0`. That socket has `IP_MULTICAST_IF=lo` and joins its groups on `lo` (`multicast.c:161-167`, `radio_status.c:125`).
- Observed with `tcpdump -i any`: every radiod packet was on `lo` from `127.0.0.1`. Nothing radiod sent appeared on `eth0`.
- **`lo` must have the MULTICAST flag.** If the flag is missing, radiod tries `SIOCSIFFLAGS` itself and exits with `EX_NOPERM` when it lacks CAP_NET_ADMIN (`multicast.c:577-592`).
  - A stock Linux `lo`, and Docker's in particular, has no MULTICAST flag. `sudo ip link set dev lo multicast on` sets it; a container needs `NET_ADMIN` for that.
  - `scripts/build-radiod.sh` checks the flag and sets it with sudo. `coaxhub-radiod.service` sets it before every start.
- **No route is needed.** Nothing uses `ip route add 224.0.0.0/4 dev lo`. The sockets pick `lo` explicitly.
- radiod's control socket joins the status group on the default-route interface and also on `lo` (`multicast.c:240-244`). Commands arrive whichever way they are sent. Observed memberships: `ip maddr` shows 239.77.0.6 on both `lo` and `eth0`.
- Upstream `tune` and `metadump` send commands out `eth0` with TTL 1 or 5 (seen in tcpdump as `eth0 Out 172.20.0.3 > 239.77.0.6.5006`). That leaks commands onto the network. `tune -i lo` keeps them on `lo`.

**Client socket recipe.** Implemented and verified by `ka9q-probe.py`:
1. Create a UDP socket and set `SO_REUSEADDR` and `SO_REUSEPORT`. radiod has the same port bound (`multicast.c:247-250`).
2. `bind(239.77.0.6:5006)`.
3. `IP_ADD_MEMBERSHIP` with group 239.77.0.6 and interface 127.0.0.1.
4. `IP_MULTICAST_IF=127.0.0.1`, `IP_MULTICAST_TTL=0`, `IP_MULTICAST_LOOP=1`.
5. Send commands with `sendto(239.77.0.6:5006)`.
6. For RTP, do the same on `239.77.0.4:5004`.

This also works when the client runs under a different UID than radiod: the probe ran as root against radiod running as an unprivileged user.

The client receives its own CMD packets, other clients' commands, and all channels' status on the group. It must filter on `pkt[0] == 0` (STATUS), then on `OUTPUT_SSRC`, then on `COMMAND_TAG`.

## 3. Test config (`testdata/radiod-siggen.conf`)

Front end (`hardware = sig_gen`):
- 24 MS/s **real** samples. The front end covers 0 to 12 MHz; the LO is fixed at 0 and cannot be tuned (`sig_gen.c`, `sig_gen_tune`).
- One carrier at **10.000 MHz, −20 dBFS, AM-modulated 50 % by a 1 kHz sine**.
- White noise at −30 dBFS total across the band. radiod reports N0 ≈ −91 dB/Hz and AM SNR ≈ 31 dB in 10 kHz.

The modulation source:
- `sig_gen` `popen()`s the configured `source` and reads 48 kHz s16 mono from it (`sig_gen.c:234`).
- The config uses `sox -q -n -t raw -r 48000 -e signed-integer -b 16 -c 1 - synth 1000000 sine 1000 vol 0.5`.
- Upstream's example uses `wwvsim`, which is not in the repo.
- Without a source, the modulation silently falls back to an unmodulated carrier.

Dynamic channels:
- They are enabled because `data =` is set.
- The template preset is `preset = am`: linear demod, 12 kHz, ±5 kHz, envelope detector, AGC.
- No `lifetime` is set, so the template keeps upstream's default: **0 = never expires** (`defaults.h:30`).
- The config has no static channels. The front end runs only while at least one channel exists: it starts on the first channel (`radio.c:912-915`) and stops when the last one closes (`radio.c:1100`). So with no channels, radiod uses 0 % CPU and sends no packets.

FFT planning:
- There is no FFTW wisdom file, so plans fall back to `FFTW_ESTIMATE` (`filter.c:109-112`).
- Startup takes < 1 s. `fft-plan-level` has no effect without wisdom.

## 4. Command and status flow

### Packet framing

The TLV framing assumed at the start is confirmed. Additional rules:

- **Packet validity:**
  - A command is accepted only if its length is ≥ 3 and `pkt[0] == 1` (`radio_status.c:47`).
  - The SSRC is taken from the first `OUTPUT_SSRC` TLV found anywhere in the packet (`decode_status.c:375`).
  - A missing SSRC, or SSRC 0, is ignored (`radio_status.c:53`).
- **SSRC 0xffffffff means all channels.** Every running channel then sends one status to the status group. The replies are staggered at four channels per 20 ms frame (`radio_status.c:56-66`).
- **Any other SSRC finds or creates a channel** from the template (`radio.c` `lookup_or_create_chan`). Consequences:
  - **Any** command to an unknown SSRC creates a channel, including a bare poll or a "delete" command.
  - A refresh or poll sent after the channel expired silently re-creates it from the template, with LIFETIME 0 (immortal) at 0 Hz.
  - Always send `LIFETIME` in **every** command.
  - The channel table holds 2000 entries (`defaults.h:10`).
- **Per-channel command queue depth is 2** (`radio.h:302` `CQLEN`).
  - A command that arrives while two are queued is **silently dropped** (`radio_status.c:93-104`).
  - The channel thread takes one command per 20 ms frame.
  - Observed: 6 back-to-back polls produced 2 replies, for both a linear and a spectrum channel.
  - So wait for the reply (or ~40 ms) before sending the next command to the same SSRC.
- **Parsing:**
  - `PRESET` is applied in a first pass (`radio_status.c:144-184`).
  - All other TLVs are applied in TLV order in the second pass.
  - The parser stops at the first TLV for which `cp + optlen >= end` (`radio_status.c:161,204`), so the last TLV must be followed by `EOL`.
  - Every accepted command resets the lifetime timer (`radio_status.c:141`).

### Status destinations

Status is always sent to the **status group** `239.77.0.6:5006` (`frontend->metadata_dest_socket`) in reply to a command (`radio.c:1340-1357`). The reply carries the echoed `COMMAND_TAG` and `CMD_CNT` (the number of commands received). Linear, FM and WFM channels also send:
- a copy of each command reply to `239.77.0.4:5006`;
- unsolicited status to `239.77.0.4:5006` every `update` frames, default 25 frames = 0.5 s (`defaults.h:16`). Observed: status at t = 0.44, 0.94, 1.44 … s.

The periodic status goes **only** to the data group, not to the status group.

### Reply latency

A command is executed by the channel's own thread, once per 20 ms frame.

| Case | Observed latency |
|---|---|
| New channel (`PRESET am`, Opus) | 17.7–20.2 ms |
| New channel with `DEMOD_TYPE 4` | 0.5 ms, but the reply has no bin data yet |
| Existing linear channel, 30 polls at 10 Hz | 24.5 / 30.1 / 35.7 ms (min / median / max) |
| Existing spectrum channel | 18–45 ms (see §6) |

Use a reply timeout of at least 100 ms, and retry.

### Creating an Opus channel

Upstream `tune`, sending on `lo`:

```
$ tune -i lo -r 239.77.0.6 -s 1234 -m am -f 10m0 -e opus --lifetime 1500
SSRC 1234
Lifetime 1499
Preset am
Sample rate 12000 Hz
Destination socket: 239.77.0.4:5004
Opus mode audio
Frequency 10000000.000 Hz
...
Baseband power -19.5 dB
N0 -90.8 dB/Hz
SNR 31.3 dB
```

Upstream `tune` quirks:
- `-t` is missing from its `Optstring` (`tune.c:58`). Only `--lifetime` works.
- Without `--lifetime` it sends `LIFETIME 0` (`tune.c:50,267`), which makes the channel immortal.

Opus sample rate:
- `OUTPUT_ENCODING = 3` (Opus) keeps the preset's 12 kHz rate, because 12 kHz is a legal Opus rate.
- Illegal rates are forced to 48 kHz (`radio_status.c:506`).

### Status packet dump

A 298-byte status reply, decoded with upstream `metadump -c 2 -s 1234 239.77.0.6` and abridged:

```
127.0.0.1:58858 STAT [18] SSRC 1234 [1] cmd tag b0c5103c [2] commands 2 [4] coaxhub-test sig_gen
[117] lifetime 499 frames [5] status dest 239.77.0.6:5006 [10] in samprate 24000000 Hz
[102] fe produces real samples [100] fe filt low 0 Hz [101] fe filt high 1.2e+07 Hz [33] RF 10000000.000 Hz
[34] first LO 0.000 Hz [45] IF pwr -20.4 dB [47] N0 -91.5 dB/Hz [48] demod 0 (linear) [85] preset am
[39] filt low -5000 Hz [40] filt high 5000 Hz [20] samprate 12000 Hz [46] baseband pwr -19.5 dB
[49] out channels 1 ... [22] data pkts 3 [69] output level -13.4 dBFS [71] opus bitrate 0 Hz
[16] data src 0.0.0.0:58858 [17] data dst 239.77.0.4:5004 [19] TTL 0 [105] RTP PT 111
[106] status interval 25 [107] encoding 3 (opus) [21] metadata pkts 3 [23] output errors 0
```

## 5. RTP / Opus

Observed with a Python multicast receiver on `239.77.0.4:5004`, SSRC 8001, AM, Opus:

| Field | Observed value |
|---|---|
| Source | `127.0.0.1:<ephemeral>`; the same source port carries the status copy |
| RTP header | V=2, P=0, X=0, CC=0, M=0 |
| Payload type | **PT 111**; dynamic, also reported in status tag `RTP_PT` (105) |
| Packet rate | 50 packets/s, one 20 ms Opus frame each |
| Sequence number | +1 per packet |
| **Timestamp** | **+960 per packet: a 48 kHz clock**, although the channel rate is 12 kHz. It is based on radiod uptime (`radio.c` `demod_thread`), not 0. |
| Payload | 22–50 bytes (≈ 14 kbit/s, `OPUS_BIT_RATE 0` = auto) |
| Opus TOC | `0x28`: config 5 (SILK MB 20 ms), mono, code 0 |

Recording with upstream tools:

```
$ timeout 8 pcmrecord -v -d rec -S 1234 -x 5 239.77.0.4
coaxhub-test sig_gen creating '1234k2026-09-29T17:24:05.7Z.opus' 12000 s/s mono opus 10000000.000 Hz linear from 127.0.0.1:58858
$ opusinfo rec/1234k*.opus
  Packet duration:   20.0ms (max),   20.0ms (avg),   20.0ms (min)
  Playback length: 0m:07.713s   Average bitrate: 14.79 kbit/s
$ opusdec --rate 48000 rec/1234k*.opus out.wav; sox out.wav -n remix 1 trim 1 stat
  RMS     amplitude:     0.041632
  Rough   frequency:          995
```

- pcmrecord writes an Ogg header with 2 channels. That is its own choice; the RTP stream is mono.
- A Goertzel check on the decoded audio found a 1000 Hz tone at −30.5 dBFS. The level at 900 Hz and at 1100 Hz was below −72 dBFS. The audio is the signal generator's 1 kHz AM tone.

## 6. Spectrum2 (`DEMOD_TYPE 4`)

### Creating the channel

Send one command to a **new** SSRC:

```
$P cmd --ssrc 5000 DEMOD_TYPE=i:4 RADIO_FREQUENCY=d:10e6 BIN_COUNT=i:512 RESOLUTION_BW=f:1000 LIFETIME=i:1500
```

- The channel starts as the template (linear), and the `DEMOD_TYPE` change restarts it as spectrum2.
- The immediate reply has no bin data (`SPECTRUM_FFT_N=0`). Bin data starts with the next poll.

### Cadence: poll only

- Spectrum channels set `status.output_interval = 0` (`spectrum.c:44`).
- They send status only in reply to a command, and only to the status group (`radio.c:1353`).
- Observed: `listen` on both `239.77.0.6:5006` and `239.77.0.4:5006` for 3 s without polling showed **0 packets**.
- Each poll computes a **fresh** spectrum:
  1. The command is decoded.
  2. The thread waits for the next frame.
  3. It runs `wideband_poll` or `narrowband_poll` (`spectrum.c:156-178`).
  4. The reply goes out at the top of the next loop.
- A poll only needs `OUTPUT_SSRC`, `COMMAND_TAG` and `LIFETIME`. It also refreshes the lifetime.

### Poll rate

Measured with synchronous polls, the next poll sent only after the reply:

| Target interval | Replies | Latency min / median / max (ms) |
|---|---|---|
| 100 ms (10 Hz) | 50/50, and 120/120 in a later run | 17.7 / 36.3 / 42.3 |
| 50 ms (20 Hz) | 100/100 | 18.1 / 30.5 / 44.2 |
| 20 ms, 10 ms | 100/100 | 33 / 42 / 45 (limited by latency to ≈ 23 Hz) |

**10 rows/s is comfortable.** The ceiling is about one reply per two frames (≈ 20–25 Hz) per channel. Do not pipeline more than 2 polls, because of `CQLEN` (§4).

### Geometry

**Bin spacing is `RESOLUTION_BW` in Hz.** `BIN_COUNT` sets the number of bins. The span is `BIN_COUNT × RESOLUTION_BW`, centred on `RADIO_FREQUENCY`. In wideband mode both hold exactly only when `RESOLUTION_BW` divides the sample rate and the centre is on radiod's placement grid; otherwise the bins are slightly wider or narrower and up to one bin away from where `RADIO_FREQUENCY` puts them (§11).

**Wideband mode** applies when `RESOLUTION_BW > CROSSOVER`; the default crossover is 200 Hz.
- It FFTs the A/D stream directly: `SPECTRUM_FFT_N = lrint(samprate / rbw)` (`spectrum.c:612`). At 24 MS/s and 1 kHz that is 24000.
- It averages `SPECTRUM_AVG` FFTs. The template default is **10**.
- `NOISE_BW` is 1500 Hz with the default Hann window (`WINDOW_TYPE 5`).

**Narrowband mode** applies when `RESOLUTION_BW ≤ CROSSOVER`.
- It FFTs the downconverted baseband.
- Observed: `BIN_COUNT 1024`, `RESOLUTION_BW 100`, `SPECTRUM_AVG 1` gave `SPECTRUM_FFT_N 1040`. The carrier landed in bin 512 and the AM sidebands in bins 502 and 522 (±1 kHz).

Tested geometries:

| Bins | RBW | Span | Reply size | Carrier bin |
|---|---|---|---|---|
| 512 | 1 kHz | 512 kHz | 731 B | 256 |
| 1024 | 100 Hz | 102.4 kHz | 1255 B | 512 |
| 2048 | 1 kHz | 2.048 MHz | 2267 B | 1024 |

The frequency of byte `i` of N bins is **`RADIO_FREQUENCY + (i − ⌈N/2⌉) × RESOLUTION_BW`** (in wideband mode, with the real centre and spacing of §11).
- Check at 10.0 MHz: the carrier is at bin 256 of 512.
- Check with the centre moved to 10.1 MHz: the carrier moved to bin 156.
- **An odd count has one more bin below the centre than above.** Both modes fill a DC-first array whose first `⌊N/2⌋` entries are offsets 0, 1, … and whose other `⌈N/2⌉` are the negative offsets, and `encode_byte_data` starts at entry `⌊N/2⌋`, so byte 0 is offset `−⌈N/2⌉`. Observed on a complex signal generator (2.048 MS/s, LO 1.024 MHz, carrier 100 kHz): centred at the LO with 2 kHz bins, 1023 and 1024 bins both put the carrier in bin 50 and 1025 bins in bin 51; centred at 102 kHz with 100 Hz bins (narrowband), 1023 and 1024 bins both put it in bin 492.

### `BIN_BYTE_DATA` format

`BIN_BYTE_DATA` is tag 9. It is encoded as a string TLV of `BIN_COUNT` bytes; for ≥ 128 bytes that means `0x82 hi lo`.

- **Order is lowest frequency to highest** (`spectrum.c:529-541`).
- `byte = clamp(round((P_dB − SPECTRUM_BASE) / SPECTRUM_STEP), 0, 255)`, so **`P_dB ≈ SPECTRUM_BASE + byte × SPECTRUM_STEP`**.
- **BASE and STEP are auto-scaled on every poll** (`spectrum.c:515-520`): `BASE = min bin dB` and `STEP = (max − min) / 256` dB. So byte 0 is the weakest bin, and the strongest bin clamps to 255.
- Values sent by the client in `SPECTRUM_BASE` or `SPECTRUM_STEP` are accepted but overwritten at the next poll.
  - Observed: after sending BASE = −100 and STEP = 0.5, the replies showed −70.21 / 0.1967, then −71.09 / 0.1902.
  - To produce station-quantized rows, decode to dB with each packet's own BASE and STEP, then re-quantize.
- `SPECTRUM_BASE` (tag 11) and `SPECTRUM_STEP` (tag 115) are float32 TLVs sent just before the bytes. They are sent only when finite (`radio_status.c:797-808`).
- Upstream `metadump` does not know tags 9, 11 and 115; it prints "unknown type".

A captured 512-bin reply (731 B), decoded by `ka9q-probe.py` and upstream `metadump -t`:

```
OUTPUT_SSRC=5000 CMD_CNT=54 LIFETIME=1499 RADIO_FREQUENCY=10000000.000 DEMOD_TYPE=4 WINDOW_TYPE=5
RESOLUTION_BW=1000 BIN_COUNT=512 CROSSOVER=200 SPECTRUM_FFT_N=24000 NOISE_BW=1500 SPECTRUM_AVG=10
SPECTRUM_BASE=-71.15 SPECTRUM_STEP=0.2055 BIN_BYTE_DATA[512]=131b1a1d0907061c19201e1a1b1b1e0f..
metadump -t: ... [48] demod 4 (spectrum v2) [14] fft window type 5 Hann [93] resolution bw 1000.0 Hz
  [94] bins 512 [95] crossover 200 [76] fft N 24000 [15] bin noise bw 1500.0 Hz [12] fft avg 10
  [116] fft overlap 0.000 [11] unknown type 11 length 4 [115] unknown type 115 length 4 [9] unknown type 9 length 512
bins 250..262: [27, 26, 20, 26, 161, 234, 255, 234, 161, 7, 20, 22, 27]   median 26
```

### Warning: never change `BIN_COUNT` on a live spectrum channel

It **crashes radiod**.
- The local `bin_count` is updated at re-setup (`spectrum.c:103`) before the realloc check (`spectrum.c:158`), so `bin_data` is never grown. The next poll then overflows the heap.
- Reproduced twice (512 → 8192 and 2048 → 4096 bins): the log shows `malloc(): unsorted double linked list corrupted` and radiod aborts.
- To change the geometry, close the channel (§7) and create a new SSRC.
- Changing `RADIO_FREQUENCY` on a live spectrum channel is safe. Changing `RESOLUTION_BW` with the same bin count also reuses the buffer.

## 7. Channel lifetime and teardown

- **`LIFETIME` is in frames** (20 ms). It is a countdown, decremented once per frame in `downconvert()` (`radio.c:1244`) or `demod_idle` (`radio.c:1025`). At 0 the channel terminates and closes.
- `LIFETIME = 0` means **never expires**. This is the template default (`defaults.h:30`). A `lifetime =` config key is in **seconds** (`modes.c:242`).
- Every accepted command to the SSRC resets the countdown to the last value set with `LIFETIME` (`lifestart`, `radio_status.c:141,581-582`). A bare poll (SSRC + tag) is enough.
- Status reports the remaining frames. Right after a reply it shows `LIFETIME = set − 1`.

Observed, with verbosity raised by `kill -USR1 <radiod>` so that timeouts are logged:

| Case | Result |
|---|---|
| 7001 created without `LIFETIME` | status `LIFETIME=0`; still present in the all-channel poll after 3+ s |
| 7002 `LIFETIME=100`, never refreshed | log `linear 7002 timeout` / `chan 7002 exiting` after 2 s |
| 7003 `LIFETIME=100`, bare poll every 1 s for 4 s | stayed alive, every reply `LIFETIME=99` |

`RADIO_FREQUENCY = 0` **does not destroy a channel.** This contradicts the assumption we started with.
- It sets the frequency to 0. The front end is not retuned (`radio.c:1122`).
- A linear channel is muted (`linear.c:337`). tcpdump showed no RTP on 5004 for 2 s after the command, only status.
- The channel stays alive until its lifetime expires. With `LIFETIME=0`, observed at 3+ s, it stays forever.

**To destroy a channel, send `LIFETIME=1`**, optionally with `RADIO_FREQUENCY=0` to mute it at once.
- The reply shows `LIFETIME=0`, and the channel exits within one frame. Observed: gone from the all-channel poll 0.5 s later.
- Because any command re-creates a missing SSRC, a `LIFETIME=1` sent to an already-expired SSRC creates a channel that dies within one frame. The command is therefore idempotent and harmless (seen in the log as a second `7003 timeout`).

Client rules:
- Always set `LIFETIME` explicitly, for example 5–10 s worth of frames.
- Refresh it with every command or poll.
- Close with `LIFETIME=1`.

## 8. CPU (radiod on x86, signal generator)

Measured with `pidstat -u [-t] -p <radiod> 10 1` (10 s average; 100 % = one core):

| Scenario | radiod CPU |
|---|---|
| No channels (front end stopped) | 0.0 % |
| 1 Opus AM channel | 50.6 % |
| 1 Opus AM + 1 spectrum2 (1024 bins × 1 kHz) polled at 10 Hz | 52.0 % (a second run: 51.6 %) |

Per-thread breakdown for the last row:

| Thread | CPU |
|---|---|
| `proc_siggen` (synthesising 24 MS/s of Gaussian noise + AM) | 41.2 % |
| `fft` (600k-point real forward FFT, 50/s) | 8.8 % |
| `linear 8001` (AM demod + Opus encode) | 1.2 % |
| `spectrum2 8002` | 0.5 % |

Most of this is the synthetic front end. The channels themselves cost ≈ 2 %. On real hardware the `proc_siggen` share is replaced by the driver's USB and unpacking cost. Other observations:
- RSS is ≈ 34 MB.
- radiod's own log line `CPU usage: … 49.7% in last 10.0 sec` agrees with pidstat.
- `sched_setscheduler` fails without CAP_SYS_NICE, so radiod runs at normal priority. This is only a warning.

## 9. Gotchas summary

1. `lo` needs the MULTICAST flag. radiod exits if the flag is missing and it lacks CAP_NET_ADMIN (§2).
2. With no channels, radiod sends nothing. The front end starts with the first channel.
3. A command to an unknown SSRC creates a channel, even a poll or a delete. Always include `LIFETIME` (§4, §7).
4. `RADIO_FREQUENCY=0` mutes a channel but does not delete it. Delete with `LIFETIME=1`.
5. Per-channel command queue depth is 2. Extra commands are dropped silently.
6. Spectrum2 status is poll-only, and BASE/STEP are auto-scaled per reply.
7. **Never change `BIN_COUNT` on a live spectrum channel: radiod crashes.**
8. The Opus RTP timestamp runs at 48 kHz whatever the channel sample rate.
9. `data =` must be a bare address with no port. `status =` is the same.
10. Zombies: in a container whose PID 1 does not reap orphans (`sleep infinity`, for example), a stopped radiod stays `<defunct>` and `kill -0` still succeeds on it. Run the container with an init process (`docker run --init`). `run-radiod-siggen.sh` also checks the process state in `/proc/PID/stat`, for containers without one.
11. Upstream `tune`: use `--lifetime`, not `-t`. Without it the channel is immortal.
12. **radiod overrides the output sample rate for some modes.** WFM always outputs 48 kHz (`wfm.c` sets `output.samprate = Audio_samprate`), and Opus forces 48 kHz for rates it cannot encode. The status reports the rate actually used. Don't treat a mismatch as a lost channel: re-sending the requested rate restarts the demodulator (`decode_radio_commands` flags a restart on any sample-rate change). Observed on an RTL-SDR.
    - The WFM override happens when the demodulator (re)starts, which is **after** radiod replies to the creating command: that reply still shows the requested rate (observed: 24000 in the reply, 48000 from the next status on). The backend therefore reports 48 kHz for `wfm` in `ChannelOpened` without waiting (§10).
13. **FM squelch is always on.** `wfm.c` sets `squelch.snr_enable = true` at demodulator start, whatever the preset says (`snr-squelch = no` in `[wfm]`). `fm.c` (NBFM) squelches on the channel SNR against `SQUELCH_OPEN`/`SQUELCH_CLOSE` even with `SNR_SQUELCH` off. With no signal, such a channel sends no RTP at all. To keep one open, set both thresholds very low (§10).
14. **RTL-SDR in a container.** A dongle is reachable through a `/dev/bus/usb` bind mount, if the user has access to the device node. librtlsdr detaches the host's `dvb_usb_rtl28xxu` driver on open and reattaches it on close. Config: `testdata/radiod-rtlsdr.conf`; a fixed `frequency =` locks the tuner.
15. **A wideband spectrum is placed on radiod's grid, not exactly at `RADIO_FREQUENCY`.** Keep the window's offset from `FIRST_LO_FREQUENCY` a multiple of the placement grid, and `RESOLUTION_BW` a divisor of the sample rate (§11).
16. **The first reply after a `RESOLUTION_BW` change into or within narrowband mode is wrong** (its ring buffer is partly empty); discard it. Wideband changes and retunes are valid at once (§11).
17. **A channel's create reply has `INPUT_SAMPLES` 0.** The front end's sample count comes from later replies (§11).
18. **The signal generator leaks about 3 MB per front-end start** (`sig_gen.c`, §11): long start/stop tests on it grow radiod.
19. **radiod exits when the RTL-SDR stops streaming** (`rtlsdr_read_async` fails: `exit(EX_NOINPUT)`); its service must restart it (§11).

## 10. The station backend

`backend_ka9q::backend::Ka9qBackend` maps the station protocol onto radiod (station protocol §9). These are the radiod facts it relies on beyond §1–§9, checked at the pinned commit and end to end with `scripts/e2e-station.sh` (signal generator).

### Modes and presets

From the pinned `presets.conf`:

| Protocol mode | Preset | Demodulator | Rate | Passband |
|---|---|---|---|---|
| AM | `am` | linear, envelope | 12 kHz | ±5 kHz |
| FM (broadcast) | `wfm` | wfm | **always 48 kHz** (§9 gotcha 12) | ±110 kHz |
| NFM | `fm` | fm, no de-emphasis | 24 kHz | ±8 kHz |
| USB | `usb` | linear | 12 kHz | +50 .. +3000 Hz |
| LSB | `lsb` | linear | 12 kHz | −3000 .. −50 Hz |
| CW | `cwu` | linear, 500 Hz tone | 12 kHz | ±200 Hz |

- There is also an `nfm` preset (±6.25 kHz). `fm` is used for NFM because it is upstream's "standard flat narrow band FM", and the hub always sends its own filter edges anyway.
- Every preset sets `mono = yes`. The backend also sends `OUTPUT_CHANNELS 1`, which for `wfm` turns FM stereo decoding off (`radio_status.c`, `OUTPUT_CHANNELS`), so every channel is mono Opus.
- Presets do not set `encoding`, so a `PRESET` on a live channel keeps Opus. A preset does reset the sample rate, passband and demodulator, so a mode change re-sends the channel's full configuration after `PRESET` (which radiod applies first).
- `loadpreset()` also resets the lifetime timer to its current value; the `LIFETIME` sent in the same command then sets it again (second pass), so a mode change keeps the channel's lifetime.

### Squelch

- `SNR_SQUELCH` on, with `SQUELCH_OPEN` = the hub's threshold and `SQUELCH_CLOSE` 1 dB below (radiod's defaults are 8 and 7 dB, `defaults.h`).
- Squelch off: `SNR_SQUELCH` off **and** both thresholds at −100 dB SNR. Linear modes only squelch with `SNR_SQUELCH` on; the FM demodulators always compare the SNR with the thresholds (§9 gotcha 13), and −100 dB keeps them open for any positive SNR. `fm.c` replaces a threshold of exactly 0 (linear) with its default, which −100 dB is not. Observed: a WFM channel with squelch off on the signal generator sent 100 frames in 2 s.

### S-meter

- Linear, FM and WFM channels send unsolicited status to the **data group on port 5006** every 25 frames (§4), plus a copy of each command reply. The client listens there (`ClientConfig::channel_status_addr`), so `ChannelSignal` needs no polling: the backend checks every 200 ms for a new status (≤ 5/s, station protocol §5).
- SNR: `FM_SNR` for FM channels, `PLL_SNR` when a linear channel runs its PLL (sent only then), otherwise upstream `tune`'s formula `BASEBAND_POWER / (NOISE_DENSITY × |HIGH_EDGE − LOW_EDGE|) − 1` (`tune.c`). Power: `BASEBAND_POWER`.
- Observed on the 10 MHz carrier with ±5 kHz: about 38 dB SNR, −19.5 dB power.

### Front end and capabilities

- Tunable range: `FIRST_LO_FREQUENCY + FE_LOW_EDGE .. FIRST_LO_FREQUENCY + FE_HIGH_EDGE` (`radio_status.c` sends `min_IF`/`max_IF` relative to the LO). Signal generator: 0 .. 12 MHz.
- Every status carries the front-end fields, so any fresh status (≤ 5 s old) answers `GetCapabilities`; otherwise the backend probes (a temporary channel with `LIFETIME` 50, destroyed right after).
- radiod reports neither its tuner's range, nor whether the tuner is locked, nor its driver's runtime controls:
  - `RF_GAIN` and `RF_ATTEN` commands call the driver's optional `<driver>_gain`/`<driver>_atten` symbols (`radio.c` `dlsym`); neither `rtlsdr.c` nor `sig_gen.c` has them, and the command is then ignored silently. `RF_AGC` is not a command at all (status only).
  - `FIRST_LO_FREQUENCY` calls `<driver>_tune`; `sig_gen_tune` ignores it, and a fixed `frequency =` locks an RTL-SDR's tuner.
  - So capabilities report no front-end controls and `hardware_range` = the tunable range. `SetFrontend` answers `UNSUPPORTED` for gain, AGC, attenuation and antenna; a retune is sent and accepted only if the reply's `FIRST_LO_FREQUENCY` moved (within 1 kHz).
- Overloads: `AD_OVER` counts A/D overranges since radiod started. The backend counts each second in which it grew and sends `Event{FRONTEND_OVERLOAD}` at most every 10 s.

### Waterfall geometry

`map::geometry` asks radiod for a spectrum whose real bins (§6, §11) tile a range that the backend reports in `WaterfallSubscribed.range`: a listener labels bin `i` of `N` at `start + (i + ½) × (end − start) / N`, and that is where radiod's bin `i` is, within a hertz. The model is `backend::placement`.

- **Wideband** (`(end − start) / N` above the 200 Hz crossover; the front end's status gives the LO, sample rate, `L` and `M`): radiod's bin centres are `LO + j × samprate / fft_n`, so the backend picks
  - the widest bin radiod can produce, `samprate / fft_n` for a whole `fft_n`, no wider than `(end − start) / N`, and sends it as `RESOLUTION_BW` (as a float, `lrint(samprate / RESOLUTION_BW)` gives back `fft_n`);
  - an analysis shift `k` (the window's centre bin, `LO + k × samprate / fft_n`) whose `N` bins lie inside the range, nearest its middle, and that radiod's loop fills (`wideband_poll`): for a real front end `k − ⌈N/2⌉ ≥ 0` and `k + ⌊N/2⌋ ≤ ⌊fft_n/2⌋` (§11); for a complex one, every bin within `−⌊fft_n/2⌋ .. ⌈fft_n/2⌉ − 1`;
  - a `RADIO_FREQUENCY` that radiod maps to exactly `k`: a whole number `s` of front-end bins from the LO, rounded away from zero (`s = ⌈k × points / fft_n⌉` for `k ≥ 0`), so that `compute_tuning`'s `lrint` returns `s` and `trunc(s × fft_n / points)` is `k`.

  When `fft_n` takes the next larger value the span shrinks by `(end − start) / samprate` of a bin, and a whole shift costs up to a bin, so the reported range is inside the request and short of it by less than two bins.
- **Narrowband** (200 Hz or less): radiod downconverts with fine tuning, so `RESOLUTION_BW = (end − start) / N` and `RADIO_FREQUENCY = start + (⌈N/2⌉ + ½) × RESOLUTION_BW` tile the range exactly.
- The reported range comes from the channel's own status (`map::covered_range`: its `RADIO_FREQUENCY`, `RESOLUTION_BW` and front-end fields through the same model), rounded to whole hertz; rows are normalised to dB/Hz with the `RESOLUTION_BW` sent, which is the real bin width.
- A nominal request (`RESOLUTION_BW = (end − start) / N`, the range's centre at bin `⌊N/2⌋`) is mislabelled in wideband mode. On an RTL-SDR at 2.048 MS/s, the full span 6,137,440 .. 8,062,560 Hz in 1024 bins asks for 1880 Hz bins centred at 7,100,940 Hz; radiod runs `fft_n` 1089 (1880.62 Hz bins) centred at 7,100,000 Hz, so labels from the range are 1259 Hz high at the bottom of the span, 940 Hz at the centre and 621 Hz at the top. `map::geometry` runs that span at 1091 points (1877.18 Hz bins), inside it.
- Observed with `crates/backend-ka9q/tests/e2e_waterfall.rs` (run by `scripts/e2e-station.sh`): for the full span and spans of 2 MHz, 512 kHz, 150 kHz, 100 kHz, 40 kHz and 20 kHz in 64 to 4096 bins (odd counts included), with the carrier at five places in each, the carrier's bin is the one its label says, on the real 24 MS/s signal generator (33 requests) and on a complex 2.048 MS/s one (LO 1.024 MHz, carrier at 1,234,567 Hz; 28 requests). The bin is checked exactly unless the carrier is within a tenth of a bin of a labelled edge, where either neighbour may win. Nominal requests fail 16 and 14 of them, from both causes: wideband placement (2048, 1023, 701 and 64 bins) and the odd-count centre (1023 bins, narrowband included).
- Full span on the signal generator: 0 .. 12 MHz in 1024 bins is reported as 5,854 .. 11,994,146 Hz (2050 points, 11,707.3 Hz bins: 11,718.75 Hz bins would need the bin at DC, half below 0 Hz, or the window's top past Nyquist); the 10 MHz carrier is in bin 853 in every row; rows at 10.0/s; noise floor −96 dB/Hz after density normalisation; 542 bytes per zstd row.

### Measurement channels (station protocol 1.1)

stationd measures the noise floor on spectrum2 channels of its own (`backend::measure`), outside the hub's channels and subscriptions:

- One `PolledSpectrum` per plan (the client's owner-polled spectrum channel, no background task), created with the plan's `BIN_COUNT` (1024) and `RESOLUTION_BW` (500 Hz) and `SPECTRUM_AVG` 10, which never change while it lives (§6, §9, §11). A hop sends `RADIO_FREQUENCY` only, on the same SSRC; its reply is the dwell's first spectrum, and stationd then polls it 2/s itself, one command at a time.
- Window centres sit on the placement grid of §11 (`backend::measure::grid_of`: the LO and `placement_grid_hz` from the front end's status), so every bin is where stationd labels it.
- Rows are decoded with their own `SPECTRUM_BASE`/`SPECTRUM_STEP` into per-bin power; stationd normalises them to dB/Hz with `RESOLUTION_BW`, as for the waterfall.
- Under idle duty the channel is destroyed (`LIFETIME` 1) between measuring minutes, so radiod's front end stops when nothing else is open (§3) and starts again 2 s before the next one. Nothing is discarded after an open or a hop (§11; stationd's `WARMUP_MS` and `HOP_SETTLE_POLLS` are 0).

### Addresses by name

A non-numeric `status =` (or `data =`) in radiod's config is not resolved with the default `dns = no`: radiod appends `.local` unless the name already ends with it (`ensure_suffix()`, `misc.c`, which checks only the first occurrence), then hashes it into `239.x.y.z` with `make_maddr()` (`multicast.c`: 32-bit FNV-1, the low 24 bits, avoiding `239.0.0.0/24` and `239.128.0.0/24`). stationd's `status_group = "<name>"` does the same, so both reach the same group without DNS or mDNS.

### Measured end to end (signal generator)

stationd in process with the ka9q backend, serving station-conformance's fake hub over mTLS on loopback:

| What | Observed |
|---|---|
| `GetCapabilities` round trip (includes a probe channel) | 40 ms |
| `OpenChannel` round trip | 7–15 ms |
| `UpdateChannel` (retune) round trip | 18–61 ms (one radiod reply, 1–3 frames) |
| Audio | 50.0–50.3 frames/s, no sequence gap across 4 retunes |
| Waterfall | 10.0 rows/s at `row_interval_ms` 100 |


## 11. Spectrum channels for measurements

Facts behind the noise-floor measurement (one wideband spectrum channel that hops between windows, and an idle schedule that opens it only around the measuring minutes), checked at the pinned commit with `ka9q-spike measure` on the signal generator and on an RTL-SDR (RTL2832U + R820T, Q-branch direct sampling at 2.048 MS/s around 7.1 MHz, on a 40 m dipole). Procedure and numbers: [measurement-spike.md](measurement-spike.md).

### Retuning a live wideband channel

- A command is decoded at the top of the channel's loop, before `downconvert()` and the poll (`spectrum.c` `demod_spectrum`). `downconvert()` recomputes the bin shift from the new `RADIO_FREQUENCY` every frame, and `wideband_poll` FFTs the newest A/D samples. So **the reply to the `RADIO_FREQUENCY` command already carries a spectrum of the new window, computed from fresh samples.** Nothing is averaged across polls.
- Observed on the signal generator, hopping between windows with and without the 10 MHz carrier: no reply after a hop, the retune's own reply included, ever showed the carrier in a window without it, and every reply of a window with it put the carrier in the right bin (§ placement below). On the RTL-SDR, the first reply after a hop correlates with the rest of its dwell as well as they correlate with each other, and not with the previous window.
- `RADIO_FREQUENCY` replies took 1–70 ms (they wait for the next frame, and an RTL-SDR delivers samples in 64 ms USB transfers of 131,072 samples).

### Where radiod places a wideband spectrum

In wideband mode (`RESOLUTION_BW` > `CROSSOVER`) the bins are FFT bins of the A/D stream:
- `SPECTRUM_FFT_N = lrint(samprate / RESOLUTION_BW)` (`setup_wideband`), so the **real bin spacing is `samprate / SPECTRUM_FFT_N`**, which equals `RESOLUTION_BW` only when it divides the sample rate. `RESOLUTION_BW` in status is the requested value.
- The window's offset from the front end's LO is first rounded to the front end's own FFT bin, `samprate / (L + M − 1)` (`compute_tuning`, `lrint`, ties to even), then scaled to the analysis FFT with C integer division, which truncates toward zero (`wideband_poll`: `bin_shift * fft_n / master->points`). The real centre is `LO + trunc(lrint(offset / fe_bin) × fft_n / points) × samprate / fft_n`.
- So the spectrum sits exactly where `RADIO_FREQUENCY` says only when the offset from `FIRST_LO_FREQUENCY` is a multiple of the **placement grid**, the least common multiple of the bin spacing and the front end's bin. The front end's bin is 40 Hz on every config seen (20 ms blocks, overlap 5: `L` 40,960 and `M` 10,241 on the RTL-SDR at 2.048 MS/s; 480,000 and 120,001 on the signal generator at 24 MS/s), so with 500 Hz bins the grid is **1 kHz**.
- **A real front end loses the window's lower half past Nyquist.** For a real front end `wideband_poll` walks the positive offsets first and stops its loop when the FFT bin passes `fft_n/2`, before it wraps to the negative offsets: the window needs `k + ⌊N/2⌋ ≤ ⌊fft_n/2⌋` (`k` the analysis shift), and bins below 0 Hz stay empty. Observed on the signal generator (2 kHz bins, `fft_n` 12,000, 1024 bins): centred at 10.976 MHz (`k` 5488) the 10 MHz carrier is in bin 24; at 10.978 MHz (`k` 5489) it is gone and the peak wanders.
- Off the grid the spectrum is up to one bin closer to the LO than reported. Observed on the signal generator (LO 0, 500 Hz bins): windows centred at 10.0005 and 10.1005 MHz put the 10 MHz carrier in the bin labelled 10.0005 MHz (the real centres are 10.0000 and 10.1000 MHz, as the formula predicts); windows at 10.0, 10.2, 10.25 MHz and 9.9995 MHz (rounded away from zero, so exact) were exact. On a complex signal generator (2.048 MS/s, LO 1.024 MHz, carrier 100 kHz), 127 windows centred 6 kHz either side of the LO in steps of 173 and 211 Hz, with 1880 Hz bins (`fft_n` 1089, 1880.62 Hz real) and 2730 Hz bins (`fft_n` 750) and 1024 and 701 bins, all put the carrier in the bin the formula predicts; the nominal labels were right in 61. On the real one (24 MS/s), 88 windows around 9.6 and 10 MHz agreed too.

### How much signal a spectrum integrates

- A wideband poll FFTs the `SPECTRUM_AVG × SPECTRUM_FFT_N` newest samples (no overlap by default), that is `SPECTRUM_AVG / RESOLUTION_BW` seconds: **20 ms** at the default `SPECTRUM_AVG` 10 and 500 Hz bins. Each bin is the mean of 10 periodograms, so its dB value has a spread of about ±1.4 dB on noise.
- `SPECTRUM_AVG` is capped silently by the input ring: on the RTL-SDR at 500 Hz bins, a request for 51 or 100 is answered with 50 (100 ms; the ring holds 4 × 51,200 samples).
- On 40 m, side by side on the same window for 11 minutes: the median of the pooled dB/Hz values agreed within 0.1 dB at 10 and 50 FFTs, and the 10th percentile was 0.8 dB lower at 10 (a wider spread per bin). The low percentiles therefore depend on `SPECTRUM_AVG`, which must be set explicitly and kept fixed.

### `RESOLUTION_BW` changes with a fixed `BIN_COUNT`

- Between wideband values (500 → 1000 → 250 → 2000 → 500 Hz; 60 rounds, 420 changes, on the signal generator and 2 rounds on the RTL-SDR), the change's own reply is valid: on the signal generator the median noise density (dB per bin minus `10·log10(RBW)`) of every reply, first or not, was −96.4 to −95.7 dB/Hz at every width, and the carrier was always in the 10 MHz bin. `SPECTRUM_FFT_N` follows (`samprate / RBW`), the bin buffer is reused, and radiod stays up.
- Into narrowband mode (≤ 200 Hz), or between narrowband values, **the change's own reply is wrong**: the narrowband ring buffer is reallocated and zero-filled, and the first reply's spectrum is computed partly from zeros (on the signal generator, in all 60 rounds: 7.7 dB low on entering 100 Hz, 2.8 dB high on going on to 50 Hz; on the RTL-SDR 6.4 dB low and 2.9 dB high). The next poll is valid: the ring holds `SPECTRUM_AVG / RESOLUTION_BW` seconds of baseband (0.1 s at 100 Hz).

### `INPUT_SAMPLES`

- For a wideband spectrum channel, `INPUT_SAMPLES` is the front end's sample counter at the poll (complex front ends; real ones subtract the samples the poll analysed). The counter runs since radiod started, **keeps its value while the front end is stopped**, and is not reset when it starts again.
- The reply to the command that creates a channel reports **0**: the new channel has not run a frame yet.
- On the RTL-SDR it advances in steps of 131,072 samples (64 ms): 1,048,576 or 917,504 between polls 500 ms apart.

### The front end stops when the last channel closes (RTL-SDR)

- The first channel starts the front end (`radio.c` `lookup_or_create_chan`: `Frontend.start`) and the last one to close stops it (`Frontend.shutdown`). For the RTL-SDR, start creates a read thread that runs `rtlsdr_read_async`, and shutdown calls `rtlsdr_cancel_async` and joins it (`rtlsdr.c`). The device stays open (claimed) between runs; only the USB streaming stops.
- How we know it stops sampling, not just processing:
  - radiod logs `rtlsdr running` and `rtlsdr stopped` on every start and stop;
  - the sample counter (`INPUT_SAMPLES`) does not advance between runs (the first poll of a run shows at most a few USB transfers more than the last poll of the previous one, against 2.048 million per second of gap);
  - radiod's CPU between runs is 0.000 % of a core, and the read thread has a new thread id on every start.

### The first spectrum after the front end starts

- A command to a new channel is answered at once (1 ms); the first poll is answered only when the front end has delivered a frame (about 70 ms on the RTL-SDR, the first USB transfer included), and its spectrum is computed from samples taken after the start.

### The signal generator leaks on every front-end start

- `sig_gen.c`'s processing thread, started on every front-end start, `malloc`s its modulation buffer (`dac_modulation`, 1.5 frames of samples: 2.88 MB at 24 MS/s) and never frees it. Observed in an idle-schedule soak on the signal generator: radiod's RSS grew by about 3 MB per measuring minute (34 MB to 48 MB over six). This is the test front end only; tests that start and stop it for hours should expect the growth.

### RTL-SDR failure

- `rtlsdr_read_thread` calls `exit(EX_NOINPUT)` when `rtlsdr_read_async` returns an error (the dongle was unplugged or stopped streaming): radiod exits rather than hang, and its service restarts it (`coaxhub-radiod.service`: `Restart=on-failure`, `RestartSec=5`). A client sees its commands time out, then its next poll re-creates the channel from the template, which the backend client detects (`DEMOD_TYPE` 0) and re-creates.
