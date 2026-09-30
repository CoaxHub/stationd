#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Station end to end: stationd with the ka9q backend against a real radiod,
# serving station-conformance's fake hub (which validates every message with the
# station-protocol §5 rules). radiod is always stopped on exit.
#
# Steps:
#   1. build-radiod.sh   build/install radiod if needed, set lo MULTICAST
#   2. run-radiod-siggen.sh     radiod with the signal generator (testdata/radiod-siggen.conf)
#   3. crates/stationd/tests/e2e_ka9q.rs (release, COAXHUB_E2E=1): capabilities, a
#      channel at 50 frames/s, retunes with a continuous sequence, WFM at 48 kHz,
#      waterfall rows at 10/s with the 10 MHz carrier in the right bin, health
#   4. if an RTL-SDR is plugged in (USB 0bda:2838 or 0bda:2832) and radiod has the
#      rtlsdr driver: the same test against testdata/radiod-rtlsdr.conf (98 MHz).
#      Skipped, not failed, when absent; RTLSDR=0 skips it, RTLSDR=1 makes it required.
#
# Timings are printed as "E2E key=value" lines and saved in ${E2E_RESULTS}.
#
# Usage: scripts/e2e-station.sh
#
# Environment:
#   E2E_RESULTS   output directory (default ${TMPDIR:-/tmp}/coaxhub-e2e-station)
#   RTLSDR        auto (default), 0 or 1 (see step 4)
#   KA9Q_PREFIX   radiod install prefix (default ~/.local/opt/ka9q-radio)
#   plus everything run-radiod-siggen.sh accepts (RADIOD_RUN_DIR, ...)

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
station_dir="$(cd "${script_dir}/.." && pwd)"
results="${E2E_RESULTS:-${TMPDIR:-/tmp}/coaxhub-e2e-station}"
rtlsdr="${RTLSDR:-auto}"
prefix="${KA9Q_PREFIX:-${HOME}/.local/opt/ka9q-radio}"

log() { printf '[e2e-station] %s\n' "$*" >&2; }

stop_radiod() {
  "${script_dir}/run-radiod-siggen.sh" stop || true
}

run_test() {
  local radiod="$1"
  log "end-to-end test against radiod (${radiod})"
  (
    cd "${station_dir}"
    COAXHUB_E2E=1 COAXHUB_E2E_RADIOD="${radiod}" \
      cargo test --release -p stationd --test e2e_ka9q -- --nocapture --test-threads=1
  ) 2>&1 | tee "${results}/e2e-${radiod}.log"
  grep '^E2E ' "${results}/e2e-${radiod}.log" >"${results}/e2e-${radiod}.values" || true
}

rtlsdr_present() {
  local d
  for d in /sys/bus/usb/devices/*; do
    [[ -f "${d}/idVendor" ]] || continue
    if [[ "$(<"${d}/idVendor")" == 0bda ]] && [[ "$(<"${d}/idProduct")" =~ ^283[28]$ ]]; then
      return 0
    fi
  done
  return 1
}

main() {
  mkdir -p "${results}"
  "${script_dir}/build-radiod.sh"
  trap stop_radiod EXIT

  (cd "${station_dir}" && cargo build --release -p stationd --tests)

  "${script_dir}/run-radiod-siggen.sh" start
  run_test siggen
  stop_radiod

  if [[ "${rtlsdr}" == 0 ]]; then
    log "RTL-SDR run skipped (RTLSDR=0)"
  elif rtlsdr_present && [[ -f "${prefix}/lib/ka9q-radio/rtlsdr.so" ]]; then
    RADIOD_CONF="${station_dir}/testdata/radiod-rtlsdr.conf" "${script_dir}/run-radiod-siggen.sh" start
    run_test rtlsdr
    stop_radiod
  elif [[ "${rtlsdr}" == 1 ]]; then
    log "ERROR: RTLSDR=1 but no RTL-SDR (or no rtlsdr driver in ${prefix})"
    return 1
  else
    log "no RTL-SDR (or no rtlsdr driver in ${prefix}): RTL-SDR run skipped"
  fi
  log "passed; results in ${results}"
}

main "$@"
