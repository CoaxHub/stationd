#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Drives the installer on a pseudo-terminal, as a person would (stdlib only).

Usage: interactive.py TRANSCRIPT EXPECT SEND [EXPECT SEND ...] -- COMMAND...

Starts COMMAND with a new controlling terminal (so /dev/tty works), waits for
each EXPECT text in its output and then types SEND and Enter. SEND is typed
only after a short pause, like a person, so a program that turns echo off
right after its prompt has done so. Everything the terminal showed, echo
included, is written to TRANSCRIPT. Exits with COMMAND's exit code, or 124
when an EXPECT text does not appear within 90 s.
"""

import os
import pty
import select
import sys
import time


def main() -> int:
    args = sys.argv[1:]
    split = args.index("--")
    transcript_path, pairs, command = args[0], args[1:split], args[split + 1 :]
    steps = list(zip(pairs[0::2], pairs[1::2]))

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(command[0], command)

    seen = b""

    def pump(timeout: float) -> bool:
        """Reads what is available; False at end of output."""
        nonlocal seen
        ready, _, _ = select.select([fd], [], [], timeout)
        if not ready:
            return True
        try:
            data = os.read(fd, 4096)
        except OSError:
            return False
        seen += data
        return bool(data)

    status = 0
    offset = 0
    for expect, send in steps:
        deadline = time.monotonic() + 90
        while expect.encode() not in seen[offset:]:
            if time.monotonic() > deadline or not pump(0.5):
                status = 124
                break
        if status:
            break
        offset = seen.index(expect.encode(), offset) + len(expect)
        time.sleep(0.5)
        os.write(fd, send.encode() + b"\r")
    deadline = time.monotonic() + 120
    while pump(1.0) and time.monotonic() < deadline:
        pass
    _, wait_status = os.waitpid(pid, 0)
    with open(transcript_path, "wb") as f:
        f.write(seen)
    return status or os.waitstatus_to_exitcode(wait_status)


if __name__ == "__main__":
    sys.exit(main())
