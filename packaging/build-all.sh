#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Builds coaxhub-stationd for every supported target: {bookworm, trixie} ×
# {amd64, arm64}, one after the other, and prints a summary with build times
# and package sizes. Takes the same environment as build-deb.sh
# (COAXHUB_DOMAIN and COAXHUB_ROOT_CA are required).
#
# Usage: packaging/build-all.sh

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
packages="${here}/../target/packages"
summary=()
for suite in bookworm trixie; do
    for arch in amd64 arm64; do
        start="$(date +%s)"
        "${here}/build-deb.sh" --suite "${suite}" --arch "${arch}"
        deb="$(ls "${packages}/${suite}"/coaxhub-stationd_*_"${arch}".deb)"
        summary+=("$(printf '%-9s %-6s %5d s %7d KiB  %s' "${suite}" "${arch}" \
            "$(($(date +%s) - start))" "$(($(stat -c %s "${deb}") / 1024))" "${deb##*/}")")
    done
done
printf '\n%-9s %-6s %7s %11s  %s\n' suite arch time size package
printf '%s\n' "${summary[@]}"
