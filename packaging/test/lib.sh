#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Helpers for the package and installer tests (sourced). Test containers
# run on the Docker daemon; files go in and out with `docker cp`, so no bind
# mounts are needed (the tests also work from inside a container,
# whose paths the host daemon cannot see).

set -euo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATION="$(cd "${TEST_DIR}/../.." && pwd)"
PACKAGES="${STATION}/target/packages"
readonly TEST_DIR STATION PACKAGES
# Every container this run starts, removed on exit.
CONTAINERS=()
FAILED=0

log() { printf '[%s] %s\n' "${TEST_NAME:-test}" "$*" >&2; }
die() {
    log "ERROR: $*"
    exit 1
}

cleanup_containers() {
    if ((${#CONTAINERS[@]})); then
        docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1 || true
    fi
}
trap cleanup_containers EXIT

# start_container NAME IMAGE [docker run options...]: a detached container
# that idles until removed.
start_container() {
    local name="$1" image="$2"
    shift 2
    docker run -d --name "${name}" "$@" "${image}" sleep infinity >/dev/null
    CONTAINERS+=("${name}")
}

# The package for SUITE and ARCH (exactly one).
package_for() {
    local suite="$1" arch="$2" debs
    shopt -s nullglob
    debs=("${PACKAGES}/${suite}"/coaxhub-stationd_*_"${arch}".deb)
    shopt -u nullglob
    ((${#debs[@]} == 1)) ||
        die "expected one ${suite}/${arch} package in ${PACKAGES}/${suite}, found ${#debs[@]} (run packaging/build-all.sh)"
    printf '%s\n' "${debs[0]}"
}

# run_in NAME SCRIPT [args...]: copies SCRIPT (and in-container/common.sh)
# into the container and runs it with bash; its TAP-like output is shown.
run_in() {
    local name="$1" script="$2"
    shift 2
    docker exec "${name}" mkdir -p /test
    docker cp "${TEST_DIR}/in-container/common.sh" "${name}:/test/common.sh" >/dev/null
    docker cp "${script}" "${name}:/test/$(basename "${script}")" >/dev/null
    docker exec "${name}" bash "/test/$(basename "${script}")" "$@"
}

# record NAME COMMAND...: runs a test step; counts a failure.
record() {
    local what="$1"
    shift
    if "$@"; then
        log "PASS ${what}"
    else
        log "FAIL ${what}"
        FAILED=$((FAILED + 1))
    fi
}

finish() {
    if ((FAILED)); then
        log "${FAILED} step(s) failed"
        exit 1
    fi
    log "all steps passed"
}
