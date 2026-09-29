#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Build and install ka9q-radio `radiod` (signal-generator and RTL-SDR front ends) and
# the upstream client tools we use for testing, inside a development container.
#
# Idempotent: re-running with the same pinned commit and an existing install
# only re-checks the apt packages and the loopback multicast flag.
#
# Layout (override with environment variables):
#   KA9Q_SRC     source clone      default: ~/.cache/coaxhub/ka9q-radio
#   KA9Q_PREFIX  install prefix    default: ~/.local/opt/ka9q-radio
#                  $KA9Q_PREFIX/sbin/radiod
#                  $KA9Q_PREFIX/bin/{control,metadump,tune,pcmrecord,powers}
#                  $KA9Q_PREFIX/lib/ka9q-radio/{sig_gen,rtlsdr}.so (compiled-in PKGLIBDIR)
#                  $KA9Q_PREFIX/share/ka9q-radio/presets.conf (compiled-in PKGDATADIR)
#                  $KA9Q_PREFIX/var/lib/ka9q-radio            (FFTW wisdom, STATEDIR)
# The install is user-owned; sudo is only used for apt-get and, if needed,
# to set the MULTICAST flag on the loopback interface (see ensure_lo_multicast).
#
# Usage: scripts/build-radiod.sh [--force-rebuild]

set -euo pipefail

readonly KA9Q_REPO="https://github.com/ka9q/ka9q-radio.git"
readonly KA9Q_COMMIT="384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd"
KA9Q_SRC="${KA9Q_SRC:-${HOME}/.cache/coaxhub/ka9q-radio}"
KA9Q_PREFIX="${KA9Q_PREFIX:-${HOME}/.local/opt/ka9q-radio}"

# Minimal Debian (bookworm) packages to build radiod + sig_gen.so + tools.
# radiod links -lopus -lfftw3f(_threads) -liniparser -lusb-1.0 -lbsd;
# sig_gen.so links -lsamplerate; rtlsdr.so links -lrtlsdr; pcmrecord links -lopus -logg;
# control links -lncursesw.
readonly BUILD_PKGS=(
  git build-essential pkg-config
  libbsd-dev libfftw3-dev libiniparser-dev libopus-dev libogg-dev
  libsamplerate0-dev libusb-1.0-0-dev libncurses-dev librtlsdr-dev
)
# Runtime/test tools: sox generates the AM test tone (see testdata/radiod-siggen.conf)
# and analyses recordings; iproute2 for `ip`; tcpdump/opus-tools/sysstat for verification;
# the shellcheck package lints these scripts.
# rtl-sdr provides rtl_test for checking USB access to a dongle.
readonly TOOL_PKGS=(sox iproute2 tcpdump opus-tools sysstat shellcheck rtl-sdr)

# Upstream programs to build (radiod is in src/Makefile DAEMONS; the rest in EXECS).
readonly DAEMONS=(radiod)
readonly EXECS=(control metadump tune pcmrecord powers)
readonly DRIVERS=(sig_gen.so rtlsdr.so)

force_rebuild=false
case "${1:-}" in
  --force-rebuild) force_rebuild=true ;;
  "") ;;
  -h | --help)
    sed -n '3,21p' "$0"
    exit 0
    ;;
  *)
    echo "unknown argument: $1" >&2
    exit 2
    ;;
esac

log() { printf '[build-radiod] %s\n' "$*" >&2; }

install_packages() {
  local missing=()
  local pkg
  for pkg in "${BUILD_PKGS[@]}" "${TOOL_PKGS[@]}"; do
    if ! dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q 'install ok installed'; then
      missing+=("$pkg")
    fi
  done
  if ((${#missing[@]} == 0)); then
    log "apt packages already installed"
    return
  fi
  log "installing apt packages: ${missing[*]}"
  sudo apt-get update -qq >/dev/null
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "${missing[@]}" >/dev/null
}

fetch_source() {
  if [[ ! -d "${KA9Q_SRC}/.git" ]]; then
    log "cloning ${KA9Q_REPO} into ${KA9Q_SRC}"
    mkdir -p "$(dirname "${KA9Q_SRC}")"
    git clone --quiet "${KA9Q_REPO}" "${KA9Q_SRC}"
  fi
  if [[ "$(git -C "${KA9Q_SRC}" rev-parse HEAD)" != "${KA9Q_COMMIT}" ]]; then
    if ! git -C "${KA9Q_SRC}" cat-file -e "${KA9Q_COMMIT}^{commit}" 2>/dev/null; then
      log "fetching ${KA9Q_COMMIT}"
      git -C "${KA9Q_SRC}" fetch --quiet origin
    fi
    log "checking out ${KA9Q_COMMIT}"
    git -C "${KA9Q_SRC}" -c advice.detachedHead=false checkout --quiet --force "${KA9Q_COMMIT}"
  fi
}

build_and_install() {
  local stamp="${KA9Q_PREFIX}/.coaxhub-ka9q-commit"
  # The stamp records the commit and the driver set, so adding a driver triggers a rebuild.
  local want="${KA9Q_COMMIT} ${DRIVERS[*]}"
  if [[ "${force_rebuild}" == false && -f "${stamp}" && "$(cat "${stamp}")" == "${want}" \
    && -x "${KA9Q_PREFIX}/sbin/radiod" ]]; then
    log "ka9q-radio ${KA9Q_COMMIT:0:12} already installed in ${KA9Q_PREFIX}"
    return
  fi
  # Only the sig_gen and RTL-SDR front ends: every other hardware driver is disabled
  # so no other SDR vendor libraries are needed. The paths below are compiled into config_paths.h.
  local make_vars=(
    "prefix=${KA9Q_PREFIX}"
    "confdir=${KA9Q_PREFIX}/etc/radio"
    "statedir=${KA9Q_PREFIX}/var/lib/ka9q-radio"
    ENABLE_AIRSPY=0 ENABLE_AIRSPYHF=0 ENABLE_BLADERF=0 ENABLE_FOBOS=0
    ENABLE_FUNCUBE=0 ENABLE_HACKRF=0 ENABLE_HYDRASDR=0 ENABLE_RTLSDR=1
    ENABLE_RX888=0 ENABLE_SDRPLAY=0 ENABLE_SOAPY=0 ENABLE_SIG_GEN=1
  )
  log "building ${DAEMONS[*]} ${EXECS[*]} ${DRIVERS[*]}"
  make -C "${KA9Q_SRC}/src" clean >/dev/null
  make -C "${KA9Q_SRC}/src" -j"$(nproc)" "${make_vars[@]}" \
    "${DAEMONS[@]}" "${EXECS[@]}" "${DRIVERS[@]}" >/dev/null

  log "installing into ${KA9Q_PREFIX}"
  install -d "${KA9Q_PREFIX}/sbin" "${KA9Q_PREFIX}/bin" "${KA9Q_PREFIX}/lib/ka9q-radio" \
    "${KA9Q_PREFIX}/share/ka9q-radio" "${KA9Q_PREFIX}/etc/radio" "${KA9Q_PREFIX}/var/lib/ka9q-radio"
  local f
  for f in "${DAEMONS[@]}"; do install -m 0755 "${KA9Q_SRC}/src/${f}" "${KA9Q_PREFIX}/sbin/"; done
  for f in "${EXECS[@]}"; do install -m 0755 "${KA9Q_SRC}/src/${f}" "${KA9Q_PREFIX}/bin/"; done
  for f in "${DRIVERS[@]}"; do install -m 0755 "${KA9Q_SRC}/src/${f}" "${KA9Q_PREFIX}/lib/ka9q-radio/"; done
  install -m 0644 "${KA9Q_SRC}/share/presets.conf" "${KA9Q_PREFIX}/share/ka9q-radio/"
  printf '%s\n' "${want}" >"${stamp}"
}

# radiod needs the MULTICAST flag on the loopback interface: its TTL-0 sockets send
# with IP_MULTICAST_IF=lo, and if the flag is missing it tries SIOCSIFFLAGS itself and
# exits (EX_NOPERM) without CAP_NET_ADMIN (src/multicast.c loopback_index()).
# Docker's default `lo` has no MULTICAST flag and a container may lack CAP_NET_ADMIN,
# so plain `sudo` is not enough. Order of attempts:
#   1. flag already set (e.g. container started with cap_add: NET_ADMIN and set before)
#   2. sudo ip link (works when the container has CAP_NET_ADMIN)
#   3. docker exec --privileged into this container via the docker-outside-of-docker socket
# The flag lives in the container's network namespace: re-run this script after the
# container is restarted or recreated (the other steps are then no-ops).
lo_is_multicast() { ip -o link show lo | grep -q 'MULTICAST'; }

ensure_lo_multicast() {
  if lo_is_multicast; then
    log "loopback already has the MULTICAST flag"
    return
  fi
  if sudo ip link set dev lo multicast on 2>/dev/null && lo_is_multicast; then
    log "set MULTICAST on lo with sudo"
    return
  fi
  local self
  self="$(hostname)"
  if command -v docker >/dev/null 2>&1 \
    && docker exec --privileged -u root "${self}" ip link set dev lo multicast on 2>/dev/null \
    && lo_is_multicast; then
    log "set MULTICAST on lo via 'docker exec --privileged ${self}' (no CAP_NET_ADMIN in container)"
    return
  fi
  log "ERROR: cannot enable multicast on lo. From the host run:"
  log "  docker exec --privileged -u root <container> ip link set dev lo multicast on"
  log "or give the container NET_ADMIN (cap_add) and rebuild."
  return 1
}

verify() {
  local out
  out="$("${KA9Q_PREFIX}/sbin/radiod" -V 2>&1 || true)"
  if ! grep -q "${KA9Q_COMMIT}" <<<"${out}"; then
    log "ERROR: radiod -V does not report ${KA9Q_COMMIT}:"
    printf '%s\n' "${out}" >&2
    return 1
  fi
  log "radiod OK: $(head -n1 <<<"${out}")"
  log "binaries: ${KA9Q_PREFIX}/sbin ${KA9Q_PREFIX}/bin"
}

main() {
  install_packages
  fetch_source
  build_and_install
  ensure_lo_multicast
  verify
}

main "$@"
