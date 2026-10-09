<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Operator guide

CoaxHub puts SDR receivers on the web: a station owner shares a receiver, and anyone can listen to it in a browser. During the beta the service is by invitation.

## What a station is

A station is a computer with an SDR receiver, running `stationd`. stationd dials out to CoaxHub through one encrypted tunnel (mutual TLS). It opens no port, so it works behind NAT, CGNAT and a changing IP address. It drives a bundled copy of [ka9q-radio](https://github.com/ka9q/ka9q-radio)'s `radiod`, which is private to the package and not on `PATH`.

## What leaves the machine

- **Audio and waterfall rows**, only while a listener listens. A channel nobody uses sends nothing.
- **Noise-floor measurements**, when the owner enables them (`[measurements] enabled`, the default). stationd measures a few statistics per band and per minute (the 10th, 25th, 50th and 90th percentiles of the noise density), never audio. It measures on a channel of its own, never retunes the receiver and never takes a channel from a listener. With no listener it runs the SDR about one minute in five.
- **Status**: the station's health (versions, uptime, backend state, receiver state), so the owner can see it.

Nothing else is sent. The hub gets no shell on the station and cannot push configuration to it.

## What the owner controls

The owner's policy is in `/etc/coaxhub/stationd.toml`. The hub can ask for less than these limits and never for more.

```toml
allow_frontend_control = true   # may the hub change the receiver's settings (gain, AGC, antenna port, centre frequency)
allowed_admin_ops = []          # restart_stationd, restart_backend, update, reboot_host: all refused unless listed
max_channels = 16               # how many channels it serves at once (at most 64)
max_waterfall_subscriptions = 4 # at most 16
[measurements]
enabled = true                  # false: the station measures nothing
[sdr]
profile = "rtlsdr"              # rtlsdr | rtlsdr-hf
# center_frequency_hz = 98000000
```

After editing, run `stationd check-config`, then restart: `sudo systemctl restart coaxhub-radiod stationd`.

| Profile | For | Starts at |
|---|---|---|
| `rtlsdr` (the default) | VHF and UHF, with the dongle's tuner | 98 MHz |
| `rtlsdr-hf` | HF by direct sampling, for dongles that support it (such as the RTL-SDR Blog V3) | 7.1 MHz, the 40 m band |

An RTL-SDR covers about 2 MHz around its centre frequency, so a station hears one band at a time. To choose another centre, set `center_frequency_hz`.

## Requirements

- **A computer that stays on**: Debian 12 (bookworm) or 13 (trixie), or Raspberry Pi OS based on them, **arm64** or **amd64**. A Raspberry Pi 3B+, 4 or 5 needs the 64-bit image.
- **An RTL-SDR** on USB, with an antenna. Other receivers are not supported yet.
- **Outbound internet access.** No port forwarding, no public IP, no dynamic DNS.
- **A clock synchronised by NTP.** A Raspberry Pi has no clock battery, and TLS fails with the wrong time. The installer waits for the sync.
- **A CoaxHub account**, by invitation during the beta.

## Install

Create the station in the CoaxHub web app. The page shows an install command and, separately, an **enrollment token**. The token is valid for 15 minutes and works once. Run the command on the station's computer:

```sh
curl -fsSL https://get.coaxhub.com/station | sudo sh
```

The installer ([installer/station.sh](../installer/station.sh), short and readable):
1. checks the system (root, Debian bookworm or trixie, amd64 or arm64);
2. looks for the RTL-SDR (`0bda:2838`, `0bda:2832`) and asks whether it covers VHF/UHF (profile `rtlsdr`, the default) or HF by direct sampling (`rtlsdr-hf`);
3. adds the CoaxHub APT repository, after checking its key against the fingerprint pinned in the script, and installs `coaxhub-stationd`;
4. writes the profile to `/etc/coaxhub/stationd.toml` and waits for the clock to be synchronised by NTP;
5. runs `stationd enroll`: stationd asks for the enrollment token on the terminal with the input hidden. Paste the token from the web app. The token is never on a command line, so it stays out of `sudo`'s log, the process list and your shell history;
6. enables and starts `coaxhub-radiod` and `stationd`, then prints `stationd status`.

On the station's page in the web app, "waiting for your station" turns to online within seconds.

Options go after `sh -s --`, for example `curl -fsSL https://get.coaxhub.com/station | sudo sh -s -- --profile rtlsdr-hf`:

| Option | Does |
|---|---|
| `--profile rtlsdr\|rtlsdr-hf` | the SDR profile, without asking |
| `--token-file PATH` | read the token from a file (scripted installs) |
| `--reenroll` | enroll again even if already enrolled |
| `--no-start` | do not enable or start the services |
| `--skip-time-sync` | do not wait for NTP (containers without systemd only) |
| `-y`, `--yes` | continue without an SDR; the default profile when there is no terminal |

The package blacklists the kernel's DVB-T driver (`dvb_usb_rtl28xxu`), which otherwise claims the dongle. The installer unloads it, or reboot once.

## Upgrade

Updates come with the system's: `sudo apt upgrade`. Running the installer again also upgrades the package, and keeps the enrollment and your `stationd.toml`.

## Uninstall

```sh
sudo apt remove coaxhub-stationd   # stops the services; keeps the configuration and the enrollment
sudo apt purge coaxhub-stationd    # removes both; the station must be enrolled again
```

Delete the station in the web app too, if you do not plan to bring it back. To cut a station off at once, for example if its computer is lost, revoke it in the web app. To use it again, get a new token and run the installer with `--reenroll`.

## Where the token comes from

The web app creates the enrollment token when you create the station, or when you ask for a new one. It is shown to you only. stationd sends it, with a certificate request, to `enroll.<domain>` and gets back the station's certificate. The station's private key is generated on the station and never leaves it.

## After the install

```sh
stationd status                            # enrolled?, tunnel, backend, certificate expiry, last error
stationd doctor                            # checks the receiver, radiod, DNS, TLS and the clock
journalctl -u stationd -u coaxhub-radiod   # the logs
```

`stationd doctor` prints pass, warn or fail per check and exits non-zero on a failure. It checks the configuration, the credentials, the SDR on USB, radiod, DNS and the TLS handshake to each CoaxHub endpoint, and the clock. When something is wrong, run it first, then read the log. Set `RUST_LOG=debug` in the service environment (`sudo systemctl edit stationd`) for more detail. Tokens and key material are never logged.

`stationd status` exits 3 when the station is not enrolled or was revoked: enroll it again. The service does not restart on that code.

## Package signing key

The installer checks the repository's signing key against a fingerprint pinned in the script. To check it yourself, compare the fingerprint of `https://apt.<domain>/coaxhub-archive-keyring.asc` with the one published here.

Fingerprint: not published yet. It is added here before the first release.

How the package is built and why each service setting is there: [packaging.md](packaging.md). How a release is made and verified: [releasing.md](releasing.md). The security model: [SECURITY.md](../SECURITY.md).
