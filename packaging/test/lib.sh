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
# shellcheck source=packaging/suites.sh
. "${STATION}/packaging/suites.sh"
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

# suite_args "$@": sets `suites` from --quick (bookworm only), --suite S or
# nothing (every suite), and `quick`. Sets nothing else.
# shellcheck disable=SC2034 # read by the scripts that call it
suite_args() {
    suites=("${ALL_SUITES[@]}")
    quick=0
    while (($#)); do
        case "$1" in
        --quick) suites=(bookworm); quick=1 ;;
        --suite)
            suite_info "${2:-}" || die "--suite must be one of: ${ALL_SUITES[*]}"
            suites=("$2")
            shift
            ;;
        *) die "usage: $0 [--quick] [--suite ${ALL_SUITES[*]// /|}]" ;;
        esac
        shift
    done
}

# suite_env SUITE: sets SUITE_* (suites.sh) and writes the shell file that
# in-container scripts source: the suite's paths and names.
suite_env() {
    suite_info "$1" || die "unknown suite $1"
    local f
    f="$(mktemp)"
    {
        printf 'CODENAME=%q\n' "$1"
        printf 'SYSLIB=%q\n' "/${SUITE_SYSLIB}"
        printf 'SUFFIX=%q\n' "${SUITE_SUFFIX}"
        printf 'TARGET_NAME=%q\n' "${SUITE_NAME}"
        printf 'OS_IDS=%q\n' "${SUITE_OS_IDS}"
        printf 'BASE_IMAGE=%q\n' "${SUITE_BASE}"
        printf 'SUPPORTED_TEXT=%q\n' "${SUPPORTED_TEXT}"
    } >"${f}"
    SUITE_ENV_FILE="${f}"
}

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
    docker cp "${SUITE_ENV_FILE:?call suite_env first}" "${name}:/test/suite.env" >/dev/null
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
