#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Minimal ka9q-radio control/status probe for verifying radiod behaviour.

Not production code: a stdlib-only helper used to produce the observations in
docs/radiod-notes.md where no upstream tool sends the needed command
(e.g. DEMOD_TYPE 4 spectrum2 polls). Tag numbers are read from the pinned
upstream src/status.h, so nothing is hand-numbered here.

Commands and status use radiod's status group (default 239.77.0.6:5006) on the
loopback interface with multicast TTL 0, exactly as the station client must.

Examples:
  # create/refresh a channel and print the status reply
  ka9q-probe.py cmd --ssrc 5000 DEMOD_TYPE=i:4 RADIO_FREQUENCY=d:10e6 BIN_COUNT=i:512 \
      RESOLUTION_BW=f:1000 LIFETIME=i:500
  # poll it 50 times at 10 Hz and report reply latency; save the last reply raw
  ka9q-probe.py cmd --ssrc 5000 --count 50 --interval 0.1 --raw-out /tmp/spec.bin LIFETIME=i:500
  # passively log every packet on the status group for 5 s
  ka9q-probe.py listen --seconds 5
Value prefixes: i: integer, f: float32, d: float64, s: string.
"""

import argparse
import os
import random
import re
import socket
import struct
import sys
import time

DEFAULT_STATUS_H = os.path.expanduser("~/.cache/coaxhub/ka9q-radio/src/status.h")


def load_tags(path):
    """Parse `enum status_type { ... }` from status.h into {name: value}."""
    text = open(path, encoding="utf-8").read()
    body = re.search(r"enum status_type\s*{(.*?)}", text, re.S).group(1)
    body = re.sub(r"//[^\n]*|/\*.*?\*/", "", body, flags=re.S)
    tags, value = {}, -1
    for item in body.split(","):
        item = item.strip()
        if not item:
            continue
        if "=" in item:
            name, expr = (s.strip() for s in item.split("=", 1))
            value = int(expr, 0)
        else:
            name, value = item, value + 1
        tags[name] = value
    return tags


def enc_len(n):
    if n < 128:
        return bytes([n])
    b = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return bytes([0x80 | len(b)]) + b


def enc_uint(tag, x):
    b = x.to_bytes(8, "big").lstrip(b"\0")  # leading zeros suppressed; 0 -> length 0
    return bytes([tag]) + enc_len(len(b)) + b


def enc_value(tag, spec):
    kind, _, raw = spec.partition(":")
    if kind == "i":
        return enc_uint(tag, int(raw, 0) & 0xFFFFFFFFFFFFFFFF)
    if kind == "f":
        return enc_uint(tag, struct.unpack(">I", struct.pack(">f", float(raw)))[0])
    if kind == "d":
        return enc_uint(tag, struct.unpack(">Q", struct.pack(">d", float(raw)))[0])
    if kind == "s":
        b = raw.encode()
        return bytes([tag]) + enc_len(len(b)) + b
    raise SystemExit(f"bad value spec {spec!r}")


def decode(pkt):
    """Return (pkt_type, {tag: raw bytes}) for one status/command packet."""
    out, i = {}, 1
    while i < len(pkt):
        t = pkt[i]
        i += 1
        if t == 0:
            break
        n = pkt[i]
        i += 1
        if n & 0x80:
            k = n & 0x7F
            n = int.from_bytes(pkt[i : i + k], "big")
            i += k
        out[t] = pkt[i : i + n]
        i += n
    return pkt[0], out


def as_int(b):
    return int.from_bytes(b, "big") if b else 0


def as_float(b):
    """float32, or float64 when longer than 4 bytes (status.c decode_float)."""
    v = as_int(b)
    return struct.unpack(">d", v.to_bytes(8, "big"))[0] if len(b) > 4 else struct.unpack(">f", v.to_bytes(4, "big"))[0]


def as_double(b):
    return struct.unpack(">d", as_int(b).to_bytes(8, "big"))[0] if len(b) > 4 else as_float(b)


def open_socket(group, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    s.bind((group, port))
    lo = socket.inet_aton("127.0.0.1")
    s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(group) + lo)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, lo)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 0)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)
    return s


def summary(tags, fields):
    names = {v: k for k, v in tags.items()}
    parts = []
    for t, b in fields.items():
        name = names.get(t, str(t))
        if name == "BIN_BYTE_DATA":
            parts.append(f"{name}[{len(b)}]={b[:16].hex()}..")
        elif name in ("RADIO_FREQUENCY", "FIRST_LO_FREQUENCY"):
            parts.append(f"{name}={as_double(b):.3f}")
        elif name in ("RESOLUTION_BW", "SPECTRUM_BASE", "SPECTRUM_STEP", "NOISE_BW", "BASEBAND_POWER", "CROSSOVER"):
            parts.append(f"{name}={as_float(b):.4g}")
        elif name in ("OUTPUT_SSRC", "COMMAND_TAG", "LIFETIME", "DEMOD_TYPE", "BIN_COUNT",
                      "SPECTRUM_FFT_N", "SPECTRUM_AVG", "WINDOW_TYPE", "OUTPUT_ENCODING", "CMD_CNT"):
            parts.append(f"{name}={as_int(b)}")
    return " ".join(parts)


def cmd(args, tags):
    s = open_socket(args.group, args.port)
    s.settimeout(args.timeout)
    body = b"".join(enc_value(tags[k], v) for k, v in (a.split("=", 1) for a in args.tlv))
    latencies, last = [], None
    for n in range(args.count):
        tag = random.getrandbits(31) | 1
        pkt = (b"\x01" + enc_uint(tags["OUTPUT_SSRC"], args.ssrc) + enc_uint(tags["COMMAND_TAG"], tag)
               + (body if n == 0 or args.repeat_all else b"") + b"\x00")
        t0 = time.monotonic()
        s.sendto(pkt, (args.group, args.port))
        while True:
            try:
                data = s.recv(65536)
            except socket.timeout:
                print(f"poll {n}: no reply in {args.timeout}s", file=sys.stderr)
                break
            kind, f = decode(data)
            if kind == 0 and as_int(f.get(tags["OUTPUT_SSRC"], b"")) == args.ssrc \
                    and as_int(f.get(tags["COMMAND_TAG"], b"")) == tag:
                latencies.append(time.monotonic() - t0)
                last = data
                if args.verbose or n == args.count - 1:
                    print(f"poll {n}: {len(data)} B {1e3 * latencies[-1]:.1f} ms {summary(tags, f)}")
                break
        next_t = t0 + args.interval
        time.sleep(max(0.0, next_t - time.monotonic()))
    if latencies:
        latencies.sort()
        print(f"replies {len(latencies)}/{args.count}, latency ms min {1e3 * latencies[0]:.1f} "
              f"median {1e3 * latencies[len(latencies) // 2]:.1f} max {1e3 * latencies[-1]:.1f}")
    if args.raw_out and last:
        open(args.raw_out, "wb").write(last)


def listen(args, tags):
    s = open_socket(args.group, args.port)
    s.settimeout(0.2)
    end = time.monotonic() + args.seconds
    t0 = time.monotonic()
    while time.monotonic() < end:
        try:
            data, src = s.recvfrom(65536)
        except socket.timeout:
            continue
        kind, f = decode(data)
        print(f"{time.monotonic() - t0:8.3f} {src[0]}:{src[1]} {'STAT' if kind == 0 else 'CMD '} {len(data)} B "
              f"{summary(tags, f)}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--group", default="239.77.0.6")
    p.add_argument("--port", type=int, default=5006)
    p.add_argument("--status-h", default=DEFAULT_STATUS_H)
    sub = p.add_subparsers(dest="mode", required=True)
    c = sub.add_parser("cmd")
    c.add_argument("--ssrc", type=int, required=True)
    c.add_argument("--count", type=int, default=1)
    c.add_argument("--interval", type=float, default=0.1)
    c.add_argument("--timeout", type=float, default=1.0)
    c.add_argument("--repeat-all", action="store_true", help="send the TLVs on every poll, not just the first")
    c.add_argument("--raw-out")
    c.add_argument("-v", "--verbose", action="store_true")
    c.add_argument("tlv", nargs="*", help="TAG=kind:value")
    li = sub.add_parser("listen")
    li.add_argument("--seconds", type=float, default=5)
    args = p.parse_args()
    tags = load_tags(args.status_h)
    (cmd if args.mode == "cmd" else listen)(args, tags)


if __name__ == "__main__":
    main()
