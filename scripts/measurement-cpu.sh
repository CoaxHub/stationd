#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# CPU of radiod and of the measurement spike under the measurement scenarios
# §5.4 and §12 (on x86 and on a Raspberry Pi 4). radiod must already be
# running with the front end to measure (for example run-radiod-siggen.sh with
# RADIOD_CONF=testdata/radiod-rtlsdr-hf.conf) and must have no channels.
#
# Usage:
#   measurement-cpu.sh OUT_DIR
#
# Environment:
#   RADIOD_PID     radiod's pid (required)
#   SPIKE          ka9q-spike binary (default target/release/ka9q-spike)
#   STATUS_GROUP   radiod's status/control group (default 239.77.0.6)
#   DATA_GROUP     radiod's data group (default 239.77.0.4)
#   SECS           seconds per scenario (default 120)
#   WINDOWS        measurement windows, Hz (default 7.15e6,6.6e6,7.66e6: 40 m and two more)
#   LISTENER       listener channel FREQ_HZ:PRESET (default 7.074e6:usb)
#
# Scenarios, one after the other (radiod's CPU from pidstat over each, per thread too):
#   none        no channel: radiod's front end is stopped
#   frontend    the front end with one spectrum channel polled every 2 s to keep it alive
#   measure     continuous duty: one wideband channel hopping WINDOWS, polled at 2/s
#   listener    one listener (Opus audio) channel
#   both        the listener and continuous measuring
# The idle duty's average is measured by measurement-soak.sh over many cycles.
# Results: OUT_DIR/cpu.txt (one line per scenario) and OUT_DIR/<scenario>.pidstat.

set -euo pipefail

out="${1:?usage: measurement-cpu.sh OUT_DIR}"
: "${RADIOD_PID:?set RADIOD_PID}"
SPIKE="${SPIKE:-target/release/ka9q-spike}"
STATUS_GROUP="${STATUS_GROUP:-239.77.0.6}"
DATA_GROUP="${DATA_GROUP:-239.77.0.4}"
SECS="${SECS:-120}"
WINDOWS="${WINDOWS:-7.15e6,6.6e6,7.66e6}"
LISTENER="${LISTENER:-7.074e6:usb}"

mkdir -p "${out}"
spike=("${SPIKE}" --status-addr "${STATUS_GROUP}:5006" --data-addr "${DATA_GROUP}:5004"
  --report --radiod-pid "${RADIOD_PID}")
center="${WINDOWS%%,*}"
listen_freq="${LISTENER%%:*}"
listen_mode="${LISTENER##*:}"

log() { printf '[measurement-cpu] %s\n' "$*" >&2; }

# Runs pidstat on radiod for SECS while the given command (if any) runs, then records the
# radiod average and the spike's own CPU from its RESULT line.
scenario() {
  local name="$1"
  shift
  log "${name}: ${SECS} s"
  local spike_pid=""
  if (($# > 0)); then
    "$@" >"${out}/${name}.spike" 2>&1 &
    spike_pid=$!
    sleep 3 # let the channels open and the front end settle
  fi
  pidstat -u -t -p "${RADIOD_PID}" "$((SECS - 10))" 1 >"${out}/${name}.pidstat"
  if [[ -n "${spike_pid}" ]]; then
    wait "${spike_pid}"
  fi
  local radiod spike_cpu
  radiod="$(awk '/^Average:/ && $4 == "-" {print $9; exit}' "${out}/${name}.pidstat")"
  spike_cpu="$(sed -n 's/.*spike_cpu_pct=\([0-9.]*\).*/\1/p' "${out}/${name}.spike" 2>/dev/null || true)"
  printf '%-10s radiod_cpu_pct=%s spike_cpu_pct=%s\n' "${name}" "${radiod:-?}" "${spike_cpu:-n/a}" |
    tee -a "${out}/cpu.txt"
}

: >"${out}/cpu.txt"
scenario none
scenario frontend "${spike[@]}" measure hold --center "${center}" --secs "${SECS}"
scenario measure "${spike[@]}" measure hop --secs "${SECS}" --windows "${WINDOWS}" \
  --out-dir "${out}/measure"
scenario listener "${spike[@]}" audio --freq "${listen_freq}" --mode "${listen_mode}" \
  --secs "${SECS}" --out "${out}/listener.opus"
scenario both "${spike[@]}" measure hop --secs "${SECS}" --windows "${WINDOWS}" \
  --listener "${LISTENER}" --out-dir "${out}/both"
log "done: ${out}/cpu.txt"
