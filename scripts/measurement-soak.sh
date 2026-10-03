#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# The idle-duty soak of the measurement design §4.1 and §12 (on x86 and on a
# Raspberry Pi 4): radiod's front end is started and stopped around every measuring minute
# for HOURS. `ka9q-spike measure idle` opens the measurement channel LEAD_MS before each
# UTC minute whose number is a multiple of PERIOD_S/60, polls the warm-up fast, measures
# the minute and closes the channel 0.5 s after it. radiod is supervised as systemd would
# do it: if it exits (radiod exits when the dongle stops streaming), it is restarted and
# the restart is logged.
#
# Usage:
#   measurement-soak.sh OUT_DIR [HOURS]     HOURS defaults to 24
#
# Environment:
#   SPIKE          ka9q-spike binary (default target/release/ka9q-spike; build it with
#                  cargo build --release -p ka9q-spike)
#   RADIOD_CONF    radiod config (default testdata/radiod-rtlsdr-hf.conf: RTL-SDR, 40 m)
#   STATUS_GROUP   status/control group written into a copy of the config (default 239.77.0.6)
#   DATA_GROUP     data group written into the copy (default 239.77.0.4)
#   PERIOD_S       period of the measuring minutes, s (default 300)
#   LEAD_MS        warm-up before each measuring minute, ms (default 5000)
#   EXTRA_ARGS     more `measure idle` options, e.g. "--windows 7.15e6,6.6e6"
#
# radiod must not be running already with the same groups, and nothing else may open
# channels on it: the front end only stops when the last channel closes.
# Stop early with Ctrl-C or SIGTERM: the spike finishes its CSV files and radiod stops.
#
# Results in OUT_DIR: runs.csv (one row per measuring minute: open and first-spectrum
# times, the sample counter across the gap, leaked channels, radiod memory and CPU),
# warmup.csv, noise.csv, polls.csv, spike.log, restarts.log, summary.txt, and radiod's
# logs in OUT_DIR/radiod/. See docs/measurement-spike.md.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:?usage: measurement-soak.sh OUT_DIR [HOURS]}"
hours="${2:-24}"
SPIKE="${SPIKE:-${script_dir}/../target/release/ka9q-spike}"
RADIOD_CONF="${RADIOD_CONF:-${script_dir}/../testdata/radiod-rtlsdr-hf.conf}"
STATUS_GROUP="${STATUS_GROUP:-239.77.0.6}"
DATA_GROUP="${DATA_GROUP:-239.77.0.4}"
PERIOD_S="${PERIOD_S:-300}"
LEAD_MS="${LEAD_MS:-5000}"
EXTRA_ARGS="${EXTRA_ARGS:-}"

if ! [[ "${hours}" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo "HOURS must be a number, got ${hours}" >&2
  exit 2
fi
[[ -x "${SPIKE}" ]] || {
  echo "no ${SPIKE}; run: cargo build --release -p ka9q-spike" >&2
  exit 2
}

mkdir -p "${out}/radiod"
run_dir="${out}/radiod"
conf="${run_dir}/radiod.conf"
sed -e "s/^status = .*/status = ${STATUS_GROUP}/" -e "s/^data = .*/data = ${DATA_GROUP}/" \
  "${RADIOD_CONF}" >"${conf}"

log() { printf '%s [measurement-soak] %s\n' "$(date -u +%FT%TZ)" "$*" | tee -a "${out}/restarts.log" >&2; }

radiod_ctl() {
  RADIOD_CONF="${conf}" RADIOD_RUN_DIR="${run_dir}" RADIOD_STATUS="${STATUS_GROUP}" \
    "${script_dir}/run-radiod-siggen.sh" "$@"
}

starts=0
start_radiod() {
  # run-radiod-siggen.sh truncates the log on start; keep the previous one.
  if [[ -s "${run_dir}/radiod.log" ]]; then
    cp "${run_dir}/radiod.log" "${run_dir}/radiod.log.${starts}"
  fi
  radiod_ctl start
  starts=$((starts + 1))
}

spike_pid=""
cleanup() {
  if [[ -n "${spike_pid}" ]] && kill -0 "${spike_pid}" 2>/dev/null; then
    kill -TERM "${spike_pid}" 2>/dev/null || true
    wait "${spike_pid}" 2>/dev/null || true
  fi
  radiod_ctl stop || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

start_radiod
secs="$(awk -v h="${hours}" 'BEGIN { printf "%d", h * 3600 }')"
log "soak for ${hours} h (${secs} s), period ${PERIOD_S} s, lead ${LEAD_MS} ms, out ${out}"
# shellcheck disable=SC2086 # EXTRA_ARGS is split on purpose
"${SPIKE}" --status-addr "${STATUS_GROUP}:5006" --data-addr "${DATA_GROUP}:5004" \
  measure idle --secs "${secs}" --period-s "${PERIOD_S}" --lead-ms "${LEAD_MS}" \
  --radiod-pidfile "${run_dir}/radiod.pid" --out-dir "${out}" ${EXTRA_ARGS} \
  >"${out}/spike.log" 2>&1 &
spike_pid=$!

while kill -0 "${spike_pid}" 2>/dev/null; do
  if ! radiod_ctl status >/dev/null 2>&1; then
    log "radiod is not running; restarting it"
    tail -n 5 "${run_dir}/radiod.log" >>"${out}/restarts.log" 2>/dev/null || true
    start_radiod || log "radiod restart failed"
  fi
  sleep 10
done
wait "${spike_pid}" || log "the spike exited with status $?"
spike_pid=""

{
  echo "soak: ${hours} h, radiod starts: ${starts}"
  grep -h '^RESULT' "${out}/spike.log" || echo "no RESULT line (see spike.log)"
  echo "front-end starts logged by radiod: $(cat "${run_dir}"/radiod.log* | grep -c 'running$' || true)"
  echo "front-end stops logged by radiod: $(cat "${run_dir}"/radiod.log* | grep -c 'stopped$' || true)"
} | tee "${out}/summary.txt"
