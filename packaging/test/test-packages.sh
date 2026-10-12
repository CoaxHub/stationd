#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Package tests (contract B). Needs the four packages in target/packages
# (packaging/build-all.sh) and a Docker daemon; no bind mounts, no ports.
#
#   1. every package, without installing it (arm64 included, on an amd64
#      host): control fields, file list, ELF machine of each binary, the
#      private librtlsdr and rtlsdr.so's RUNPATH to it, the source tarballs;
#   2. amd64 on bookworm and trixie, each in a clean container, once on the
#      bare image and once with systemd installed: install, users, modes,
#      paths, stationd and radiod checks, systemd-analyze verify, reinstall,
#      remove and purge.
#
# Usage: packaging/test/test-packages.sh [--quick]   (--quick: bookworm only)

export TEST_NAME=test-packages
# shellcheck source-path=SCRIPTDIR source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

suites=(bookworm trixie)
[[ "${1:-}" == --quick ]] && suites=(bookworm)
run="coaxhub-pkgtest-$$"

artefacts() {
    local suite="$1" name="${run}-artefacts-${1}"
    start_container "${name}" "debian:${suite}"
    docker exec "${name}" mkdir -p /test/pkg
    local arch
    for arch in amd64 arm64; do
        docker cp "$(package_for "${suite}" "${arch}")" "${name}:/test/pkg/" >/dev/null
    done
    docker cp "${PACKAGES}/${suite}/ka9q-radio-384b269a.tar.gz" "${name}:/test/pkg/" >/dev/null
    local patch
    for patch in "${PACKAGES}/${suite}"/ka9q-radio-384b269a-*.patch; do
        [[ -e "${patch}" ]] && docker cp "${patch}" "${name}:/test/pkg/" >/dev/null
    done
    docker cp "${PACKAGES}/${suite}/rtl-sdr-619ac318.tar.gz" "${name}:/test/pkg/" >/dev/null
    run_in "${name}" "${TEST_DIR}/in-container/artefacts.sh" "${suite}" amd64 &&
        run_in "${name}" "${TEST_DIR}/in-container/artefacts.sh" "${suite}" arm64
}

install_test() {
    local suite="$1" mode="$2" name="${run}-install-${1}-${2}"
    # NET_ADMIN (in the container's own network namespace) to set the
    # loopback MULTICAST flag before running radiod.
    start_container "${name}" "debian:${suite}" --cap-add NET_ADMIN
    docker exec "${name}" mkdir -p /test/pkg
    docker cp "$(package_for "${suite}" amd64)" "${name}:/test/pkg/" >/dev/null
    run_in "${name}" "${TEST_DIR}/in-container/package.sh" "${mode}"
}

for suite in "${suites[@]}"; do
    record "${suite}: amd64 and arm64 artefacts" artefacts "${suite}"
    record "${suite}: amd64 install/remove/purge (bare image)" install_test "${suite}" without-systemd
    record "${suite}: amd64 install/remove/purge (with systemd)" install_test "${suite}" with-systemd
done
finish
