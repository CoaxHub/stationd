<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Security

## Reporting a vulnerability

Email **security@coaxhub.com**. Please do not open a public issue for a vulnerability.

Include:
- what you found and which component it affects (stationd, the protocol, the package, the installer, a script);
- the version (`stationd --version`) or the commit;
- steps to reproduce it, or a proof of concept;
- what an attacker gains, and what they need to be able to do first;
- whether you plan to publish it, and when.

CoaxHub is a one-person project. You get an acknowledgement within a week. After that, expect a first assessment and a plan for a fix, and a note when the fix is released. Reports are credited when the fix is published, unless you prefer otherwise.

Supported versions: the latest release.

## The station-side security model

**The hub is not trusted by the station, and the station is not trusted by the hub.**
- The hub treats stations as untrusted and validates everything a station sends: sizes, rates, counts and sequence numbers, against the limits in the station protocol. A station that breaks them is throttled, then disconnected.
- The station answers requests within the limits fixed in its package, whatever the hub asks.

**The station trusts only the CoaxHub root.**
- The gateway and the station-api are verified against the gateway CA bundle that enrollment stored (`gateway-ca.pem`). Enrollment is verified against the root certificate compiled into stationd. No other root is trusted.
- stationd never uses the system's certificate store and does not depend on `webpki-roots` or `rustls-native-certs`.
- TLS 1.3 only, with mutual TLS: the station presents a client certificate whose only URI name is its station id.
- The root certificate is public. Release packages trust the one committed under `packaging/roots/`, so anyone can check what a package trusts.

**The hub gets no shell and no configuration push.**
- The protocol has no request that runs a command or writes a file on the station.
- A packaged station has no settings file. It keeps its credentials and state in `/var/lib/coaxhub/stationd` and the receiver set the hub last applied in `/var/lib/coaxhub/receivers/receivers.json`; everything the owner chooses lives in the web app and reaches the station as typed requests ([station protocol §7](docs/station-protocol.md#7-what-the-station-keeps)).
- Those requests carry enums, numbers and an SDR's serial, never configuration text, paths or commands. stationd checks each against rules fixed in the package: the SDR table and the profiles' limits, at most 16 receivers, no overlapping receivers, and its capacity claims (channels per receiver, waterfall subscriptions). A request that breaks one is refused and changes nothing. stationd writes radiod's configuration only from the package's templates, with values it has checked.
- stationd starts, restarts and stops the receivers' radiod units, `coaxhub-radiod@<number>.service`, through a polkit rule that allows the `coaxhub` user those three verbs on those units and nothing else. It never runs as root.
- Administrative requests (restart stationd, restart the backend, update, reboot the host) are answered `UNSUPPORTED`: stationd implements none of them.
- Someone who controls the hub, the control plane or the owner's account can use this interface and no more: retune, reconfigure, stop or restart the station's receivers. [docs/architecture.md](docs/architecture.md#what-the-hub-can-and-cannot-do) lists what the hub can and cannot do.
- radiod runs as its own user with no access to the station's key, and with the system call, device and network restrictions in [docs/packaging.md](docs/packaging.md).

**Credentials come only from enrollment.**
- The station generates its ECDSA P-256 key itself. The key is never sent. Enrollment sends a certificate request and a one-time token.
- The credentials live in `/var/lib/coaxhub/stationd`, a directory of mode 0700 owned by the service user `coaxhub`. The key file is mode 0600.
- Certificates are short-lived. stationd renews them on its own after two thirds of their lifetime, and swaps the new key and certificate in as one atomic step. The control plane can revoke a station, which ends its connection and stops it from reconnecting.

**The enrollment token never passes through argv, the environment or the log.**
- stationd reads it from a terminal with echo off, or from standard input. It is zeroed in memory after use. The installer has no `--token` option and refuses one.
- It stays out of `sudo`'s log, the process list, the environment and your shell history, and no log line contains it.
- A token is valid for 15 minutes and works once.

**The clock must be NTP-synchronised.**
- Certificate validation and enrollment need the right time. A Raspberry Pi has no clock battery.
- The service waits for time synchronisation before it starts, and the installer waits for it before enrolling. `stationd doctor` checks the clock against the certificate's validity, and stationd warns in the log when its clock differs from the hub's by more than 30 seconds.

## What a self-built package gets

You can build the package yourself ([docs/packaging.md](docs/packaging.md)). A self-built package is built with the domain and root certificate you give it.
- It trusts that root and that domain, and enrolls only against that service. It does not trust CoaxHub's root unless you gave it that one.
- It has the same unit hardening, file permissions and token handling as a release package, because they come from the same sources.
- It has no CoaxHub signature. Only packages from the signed CoaxHub APT repository carry one, and [docs/releasing.md](docs/releasing.md) says how to verify them. Nothing in this repository holds a signing key.
- A build without `--features packaged`, which is what a plain `cargo build` makes, is a development build. It accepts static credentials, custom endpoints and a fake backend, so it is not for a station on the network.

The installer and its checks are described at [https://coaxhub.com/docs/station](https://coaxhub.com/docs/station). The fingerprint of the repository's signing key is published at [https://coaxhub.com/docs/station-security](https://coaxhub.com/docs/station-security).
