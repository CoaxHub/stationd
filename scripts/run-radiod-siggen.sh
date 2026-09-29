#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Start/stop a background radiod with the signal-generator test config
# (testdata/radiod-siggen.conf). Runs as the invoking user; no sudo.
# radiod itself only needs the loopback MULTICAST flag, which
# build-radiod.sh sets once per container.
#
# Usage:
#   run-radiod-siggen.sh [start]   start radiod, wait until it answers a command
#   run-radiod-siggen.sh stop      stop it (SIGTERM, then SIGKILL after 5 s)
#   run-radiod-siggen.sh status    exit 0 if running
#
# Environment:
#   KA9Q_PREFIX       install prefix of build-radiod.sh (default ~/.local/opt/ka9q-radio)
#   RADIOD_CONF       config file (default testdata/radiod-siggen.conf)
#   RADIOD_RUN_DIR    pidfile + log directory (default ${XDG_RUNTIME_DIR:-/tmp}/coaxhub-radiod)
#   RADIOD_STATUS     status/control group for the readiness probe (default 239.77.0.6)
#   RADIOD_TIMEOUT    seconds to wait for readiness (default 20)
#
# Readiness: radiod prints "N static demodulators started" once its control socket is
# bound (src/main.c after loadconfig()). radiod sends no status packets until a channel
# exists, so the script then runs upstream `tune` against a probe SSRC (999999999)
# with LIFETIME 50 frames (1 s): a matching status reply proves the multicast path
# end to end. The probe channel self-destructs one second later.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KA9Q_PREFIX="${KA9Q_PREFIX:-${HOME}/.local/opt/ka9q-radio}"
RADIOD_CONF="${RADIOD_CONF:-${script_dir}/../testdata/radiod-siggen.conf}"
RADIOD_RUN_DIR="${RADIOD_RUN_DIR:-${XDG_RUNTIME_DIR:-/tmp}/coaxhub-radiod}"
RADIOD_STATUS="${RADIOD_STATUS:-239.77.0.6}"
RADIOD_TIMEOUT="${RADIOD_TIMEOUT:-20}"

readonly PROBE_SSRC=999999999
pidfile="${RADIOD_RUN_DIR}/radiod.pid"
logfile="${RADIOD_RUN_DIR}/radiod.log"

log() { printf '[run-radiod-siggen] %s\n' "$*" >&2; }

# True if PID is a live (non-zombie) radiod. /proc/PID/stat is "pid (comm) state ...".
# Guards against PID reuse, and treats a zombie as dead: a container's PID 1
# (`sleep infinity`) never reaps orphans, so an exited radiod stays <defunct> and
# `kill -0` on it still succeeds.
is_radiod() {
  local stat
  stat="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
  [[ "${stat}" == "$1 (radiod) "[^Z]* ]]
}

running_pid() {
  local pid
  [[ -f "${pidfile}" ]] || return 1
  pid="$(<"${pidfile}")"
  [[ "${pid}" =~ ^[0-9]+$ ]] && is_radiod "${pid}" || return 1
  printf '%s\n' "${pid}"
}

do_start() {
  local pid
  if pid="$(running_pid)"; then
    log "already running (pid ${pid}), log ${logfile}"
    return 0
  fi
  local radiod="${KA9Q_PREFIX}/sbin/radiod"
  local tune="${KA9Q_PREFIX}/bin/tune"
  if [[ ! -x "${radiod}" || ! -x "${tune}" ]]; then
    log "ERROR: ${radiod} not found; run scripts/build-radiod.sh first"
    return 1
  fi
  if ! ip -o link show lo | grep -q MULTICAST; then
    log "ERROR: lo has no MULTICAST flag; run scripts/build-radiod.sh first"
    return 1
  fi
  mkdir -p "${RADIOD_RUN_DIR}"
  : >"${logfile}"
  # setsid: detach from the caller's process group so Ctrl-C in the caller does not kill it.
  setsid "${radiod}" -v "${RADIOD_CONF}" >>"${logfile}" 2>&1 </dev/null &
  pid=$!
  printf '%s\n' "${pid}" >"${pidfile}"
  log "started radiod pid ${pid}, log ${logfile}"

  local deadline=$((SECONDS + RADIOD_TIMEOUT))
  until grep -q 'static demodulators started' "${logfile}"; do
    if ! is_radiod "${pid}"; then
      log "ERROR: radiod exited during startup:"
      tail -n 20 "${logfile}" >&2
      rm -f "${pidfile}"
      return 1
    fi
    if ((SECONDS >= deadline)); then
      log "ERROR: radiod not ready after ${RADIOD_TIMEOUT}s"
      do_stop
      return 1
    fi
    sleep 0.2
  done
  local remaining=$((deadline - SECONDS))
  ((remaining > 0)) || remaining=1
  if ! timeout "${remaining}" "${tune}" -q -r "${RADIOD_STATUS}" -s "${PROBE_SSRC}" --lifetime 50 -f 10m0 >/dev/null; then
    log "ERROR: no status reply from radiod on ${RADIOD_STATUS}:5006"
    do_stop
    return 1
  fi
  log "ready: status/control ${RADIOD_STATUS}:5006"
}

do_stop() {
  local pid
  if ! pid="$(running_pid)"; then
    log "not running"
    rm -f "${pidfile}"
    return 0
  fi
  kill -TERM "${pid}"
  local i
  # radiod stops the front end, sleeps 1 s and exits (src/main.c closedown()).
  for ((i = 0; i < 50; i++)); do
    is_radiod "${pid}" || break
    sleep 0.1
  done
  if is_radiod "${pid}"; then
    log "radiod did not exit after SIGTERM; sending SIGKILL"
    kill -KILL "${pid}" 2>/dev/null || true
  fi
  rm -f "${pidfile}"
  log "stopped radiod pid ${pid}"
}

do_status() {
  local pid
  if pid="$(running_pid)"; then
    log "running (pid ${pid}), log ${logfile}"
  else
    log "not running"
    return 1
  fi
}

case "${1:-start}" in
  start) do_start ;;
  stop) do_stop ;;
  status) do_status ;;
  *)
    echo "usage: $0 [start|stop|status]" >&2
    exit 2
    ;;
esac
