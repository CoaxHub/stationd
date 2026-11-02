#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Builds coaxhub-stationd for every supported target: {bookworm, trixie, jammy,
# noble, resolute} × {amd64, arm64}, one after the other, and prints a summary
# with build times and package sizes. Takes the same environment as
# build-deb.sh (COAXHUB_DOMAIN and COAXHUB_ROOT_CA are required).
#
# ARCHES (default "amd64 arm64") limits the architectures built, SUITES
# (default: every suite, packaging/suites.sh) the suites.
#
# Usage: packaging/build-all.sh

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
packages="${here}/../target/packages"
# shellcheck source=packaging/suites.sh
. "${here}/suites.sh"
summary=()
# shellcheck disable=SC2086 # SUITES is a list
for suite in ${SUITES:-${ALL_SUITES[*]}}; do
    for arch in ${ARCHES:-amd64 arm64}; do
        start="$(date +%s)"
        "${here}/build-deb.sh" --suite "${suite}" --arch "${arch}"
        deb="$(ls "${packages}/${suite}"/coaxhub-stationd_*_"${arch}".deb)"
        summary+=("$(printf '%-9s %-6s %5d s %7d KiB  %s' "${suite}" "${arch}" \
            "$(($(date +%s) - start))" "$(($(stat -c %s "${deb}") / 1024))" "${deb##*/}")")
    done
done
printf '\n%-9s %-6s %7s %11s  %s\n' suite arch time size package
printf '%s\n' "${summary[@]}"
