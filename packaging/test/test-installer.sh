#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Installer tests (contract A): installer/station.sh against a throwaway
# signed APT repository built and served inside a clean debian:<suite>
# container (in-container/installer.sh). Needs the amd64 and arm64 packages
# in target/packages (packaging/build-all.sh); no bind mounts, no host ports.
#
# Usage: packaging/test/test-installer.sh [--quick]   (--quick: bookworm only)

export TEST_NAME=test-installer
# shellcheck source-path=SCRIPTDIR source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

suites=(bookworm trixie)
[[ "${1:-}" == --quick ]] && suites=(bookworm)
run="coaxhub-insttest-$$"

installer_test() {
    local suite="$1" name="${run}-${1}" arch
    start_container "${name}" "debian:${suite}"
    docker exec "${name}" mkdir -p /test/pkg
    for arch in amd64 arm64; do
        docker cp "$(package_for "${suite}" "${arch}")" "${name}:/test/pkg/" >/dev/null
    done
    docker cp "${STATION}/installer/station.sh" "${name}:/test/station.sh" >/dev/null
    docker cp "${TEST_DIR}/in-container/interactive.py" "${name}:/test/interactive.py" >/dev/null
    run_in "${name}" "${TEST_DIR}/in-container/installer.sh" "${suite}"
}

for suite in "${suites[@]}"; do
    record "${suite}: installer against a local signed repository" installer_test "${suite}"
done
finish
