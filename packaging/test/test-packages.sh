#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Package tests (contract B). Needs the packages in target/packages
# (packaging/build-all.sh: five suites, two architectures) and a Docker
# daemon; no bind mounts, no ports.
#
#   1. every package, without installing it (arm64 included, on an amd64
#      host): control fields, file list, ELF machine of each binary, the
#      private librtlsdr and rtlsdr.so's RUNPATH to it, the source tarballs;
#   2. amd64 on every suite (Debian bookworm and trixie, Ubuntu jammy, noble
#      and resolute), each in a clean container of that system, once on the
#      bare image and once with systemd installed: install, users, modes,
#      paths, stationd and radiod checks, systemd-analyze verify, reinstall,
#      remove and purge;
#   3. the package refusing a system it was not built for (refusal.sh).
#
# The same package under a real systemd is test-systemd.sh.
#
# Usage: packaging/test/test-packages.sh [--quick] [--suite bookworm|trixie|jammy|noble|resolute]
#        (--quick: bookworm only)

export TEST_NAME=test-packages
# shellcheck source-path=SCRIPTDIR source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

suite_args "$@"
run="coaxhub-pkgtest-$$"

artefacts() {
    local suite="$1" name="${run}-artefacts-${1}"
    suite_env "${suite}"
    start_container "${name}" "${SUITE_BASE}"
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
    suite_env "${suite}"
    # NET_ADMIN (in the container's own network namespace) to set the
    # loopback MULTICAST flag before running radiod.
    start_container "${name}" "${SUITE_BASE}" --cap-add NET_ADMIN
    docker exec "${name}" mkdir -p /test/pkg
    docker cp "$(package_for "${suite}" amd64)" "${name}:/test/pkg/" >/dev/null
    run_in "${name}" "${TEST_DIR}/in-container/package.sh" "${mode}"
}

# The suite whose system the "wrong" package is for. The test needs only this
# job's own suite's packages (CI builds one suite per job): refusal.sh makes the
# wrong package from the suite's own .deb, with the preinst's filled-in system
# changed to this suite's, as assemble.sh would have filled it, and keeps this
# suite's Pre-Depends, which is met, so that it is the preinst that refuses.
# When the real package of that suite is in target/packages as well (a full local
# build), it is tried too: an older release's, or another family's with a lower
# floor, so that its own Pre-Depends is met on this system.
wrong_package_suite() {
    case "$1" in
    bookworm) echo jammy ;;
    trixie) echo bookworm ;;
    jammy) echo bookworm ;; # the jammy owner's case
    noble) echo jammy ;;
    resolute) echo noble ;;
    esac
}

refusal_test() {
    local suite="$1" name="${run}-refusal-${1}" other
    other="$(wrong_package_suite "${suite}")"
    suite_env "${suite}"
    start_container "${name}" "${SUITE_BASE}"
    docker exec "${name}" mkdir -p /test/pkg /test/wrong
    docker cp "$(package_for "${suite}" amd64)" "${name}:/test/pkg/" >/dev/null
    # The other suite's real package, when it was built.
    local real
    for real in "${PACKAGES}/${other}"/coaxhub-stationd_*_amd64.deb; do
        [[ -e "${real}" ]] && docker cp "${real}" "${name}:/test/wrong/" >/dev/null
    done
    # The system the derived package is for.
    local f
    f="$(mktemp)"
    suite_info "${other}"
    {
        printf 'OTHER_CODENAME=%q\n' "${other}"
        printf 'OTHER_OS_IDS=%q\n' "${SUITE_OS_IDS}"
        printf 'OTHER_TARGET_NAME=%q\n' "${SUITE_NAME}"
    } >"${f}"
    docker cp "${f}" "${name}:/test/other.env" >/dev/null
    run_in "${name}" "${TEST_DIR}/in-container/refusal.sh"
}

for suite in "${suites[@]}"; do
    record "${suite}: amd64 and arm64 artefacts" artefacts "${suite}"
    record "${suite}: amd64 install/remove/purge (bare image)" install_test "${suite}" without-systemd
    record "${suite}: amd64 install/remove/purge (with systemd)" install_test "${suite}" with-systemd
    record "${suite}: the package refuses a system it was not built for" refusal_test "${suite}"
done
finish
