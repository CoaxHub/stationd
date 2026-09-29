#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# End-to-end test of the ka9q backend and the spike against a real radiod
# (signal-generator config, testdata/radiod-siggen.conf) inside the
# development container. radiod is always stopped on exit, pass or fail.
#
# Steps:
#   1. build-radiod.sh     build/install radiod if needed, set lo MULTICAST
#   2. run-radiod-siggen.sh       start radiod in the background
#   3. cargo test (release) of crates/ka9q-spike/tests/e2e.rs with COAXHUB_E2E=1
#   4. measurements: radiod idle CPU, then one Opus channel and one 10 rows/s
#      spectrum channel side by side for MEASURE_SECS, with --report and
#      pidstat's per-thread breakdown of radiod
#
# Usage: scripts/e2e.sh
#
# Environment:
#   E2E_RESULTS       output directory (default ${TMPDIR:-/tmp}/coaxhub-e2e)
#   MEASURE_SECS      length of the measurement run (default 10)
#   CARGO_TARGET_DIR  honoured for locating the release binary
#   plus everything run-radiod-siggen.sh accepts (RADIOD_RUN_DIR, ...)

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
station_dir="$(cd "${script_dir}/.." && pwd)"
results="${E2E_RESULTS:-${TMPDIR:-/tmp}/coaxhub-e2e}"
measure_secs="${MEASURE_SECS:-10}"
target_dir="${CARGO_TARGET_DIR:-${station_dir}/target}"
spike="${target_dir}/release/ka9q-spike"
radiod_run_dir="${RADIOD_RUN_DIR:-${XDG_RUNTIME_DIR:-/tmp}/coaxhub-radiod}"

log() { printf '[e2e] %s\n' "$*" >&2; }

stop_radiod() {
  "${script_dir}/run-radiod-siggen.sh" stop || true
}

main() {
  mkdir -p "${results}"
  "${script_dir}/build-radiod.sh"
  trap stop_radiod EXIT
  "${script_dir}/run-radiod-siggen.sh" start
  local pid
  pid="$(<"${radiod_run_dir}/radiod.pid")"

  cd "${station_dir}"
  log "building the release spike"
  cargo build --release -p ka9q-spike

  log "end-to-end tests"
  COAXHUB_E2E=1 cargo test --release -p ka9q-spike --test e2e -- --nocapture --test-threads=1 \
    2>&1 | tee "${results}/e2e-tests.log"

  log "radiod CPU with no channels (3 s)"
  pidstat -u -p "${pid}" 3 1 >"${results}/pidstat-idle.txt" || true

  log "measurement: 1 Opus AM channel + 1 spectrum channel (1024 x 1 kHz, 10 rows/s) for ${measure_secs} s"
  pidstat -u -t -p "${pid}" "${measure_secs}" 1 >"${results}/pidstat-busy.txt" &
  local pidstat_pid=$!
  "${spike}" --report audio --freq 10e6 --mode am --secs "${measure_secs}" \
    --bitrate 16000 --samprate 12000 --out "${results}/am.opus" >"${results}/audio.txt" &
  local audio_pid=$!
  "${spike}" --report waterfall --center 10e6 --bins 1024 --rbw 1000 --rate 10 \
    --secs "${measure_secs}" --out "${results}/rows.cxwf" --pgm "${results}/rows.pgm" \
    >"${results}/waterfall.txt"
  wait "${audio_pid}"
  wait "${pidstat_pid}" || true

  log "summary (files in ${results})"
  grep -h '^test result' "${results}/e2e-tests.log"
  grep -o 'E2E .*' "${results}/e2e-tests.log"
  grep -h '^RESULT' "${results}/audio.txt" "${results}/waterfall.txt"
  printf 'radiod idle:\n'
  grep -E 'Average|%CPU' "${results}/pidstat-idle.txt" || true
  printf 'radiod busy (per thread):\n'
  grep -E 'Average|%CPU' "${results}/pidstat-busy.txt" || true
}

main "$@"
