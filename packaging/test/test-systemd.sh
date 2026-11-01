#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Package tests under a real systemd (docs/packaging.md §7): the amd64
# package installed in a container that boots systemd as its init, on
# bookworm and trixie, to test what the static tests cannot: the units
# running under their sandboxes, and coaxhub-helper doing what stationd asks.
#
#   fresh    install; the helper's socket and its owner and mode; stationd.service
#            (its own sandbox, as user coaxhub) asking the helper to start,
#            restart and stop a receiver's unit; hostile requests refused and
#            nothing changed; a peer that is not coaxhub refused (by the
#            socket's mode, and by SO_PEERCRED); a hostile receivers.json
#            refused by the unprivileged renderer; doctor's helper check;
#            remove and purge.
#   upgrade  the published 0.2.3 package (with its polkit rule) upgraded to
#            this one: the rule is gone, the running receiver restarted once,
#            and the helper starts another.
#
# The container recipe: no --privileged, no host device and no udev:
#   --cgroupns=private --cap-add SYS_ADMIN -e container=docker, tmpfs on /run,
#   /run/lock and /tmp, and an entrypoint that remounts /sys/fs/cgroup
#   read-write before exec'ing systemd. --security-opt apparmor=unconfined lets
#   that remount through on AppArmor hosts (Ubuntu, GitHub's runners), whose
#   docker-default profile denies mount; elsewhere it changes nothing. The image has systemd and dbus, no
#   polkit, and no /usr/sbin/policy-rc.d (which would stop the package from
#   starting anything).
#
# Needs the amd64 packages in target/packages (packaging/build-all.sh) and a
# Docker daemon. The 0.2.3 package is downloaded once from the CoaxHub APT
# repository into target/packages-previous/ and checked against its SHA-256.
#
# Usage: packaging/test/test-systemd.sh [--quick] [--suite bookworm|trixie]
#        (--quick: bookworm only, no upgrade)

export TEST_NAME=test-systemd
# shellcheck source-path=SCRIPTDIR source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

readonly PREVIOUS_VERSION="0.2.3-1~deb12"
readonly PREVIOUS_URL="https://apt.coaxhub.com/pool/bookworm/main/coaxhub-stationd_0.2.3-1~deb12_amd64.deb"
readonly PREVIOUS_SHA256="d954f93716d5b25e60983bc28f96d9e0f02859be6815063307cab4f1ad11f6bb"
readonly PREVIOUS_DIR="${STATION}/target/packages-previous"

suites=(bookworm trixie)
upgrade=1
while (($#)); do
    case "$1" in
    --quick) suites=(bookworm); upgrade=0 ;;
    --suite) suites=("${2:?bookworm|trixie}"); shift ;;
    *) die "usage: test-systemd.sh [--quick] [--suite bookworm|trixie]" ;;
    esac
    shift
done
run="coaxhub-systd-$$"

# The image of a suite: systemd, dbus and the tools the tests use.
build_image() {
    local suite="$1" image="coaxhub-systemd-test:$1"
    docker build --quiet --tag "${image}" - >/dev/null <<DOCKERFILE
FROM debian:${suite}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \\
 && apt-get install -y --no-install-recommends systemd systemd-sysv dbus socat ca-certificates \\
      openssl procps util-linux iproute2 kmod \\
 && rm -rf /var/lib/apt/lists/* \\
 && rm -f /usr/sbin/policy-rc.d
STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/bin/sh", "-c", "mount -o remount,rw /sys/fs/cgroup && exec /lib/systemd/systemd"]
DOCKERFILE
}

# start_systemd NAME SUITE: a container with systemd as init, booted.
start_systemd() {
    local name="$1" suite="$2"
    docker run -d --name "${name}" --cgroupns=private --cap-add SYS_ADMIN \
        --security-opt apparmor=unconfined -e container=docker \
        --tmpfs /run --tmpfs /run/lock --tmpfs /tmp "coaxhub-systemd-test:${suite}" >/dev/null
    CONTAINERS+=("${name}")
    for _ in $(seq 1 60); do
        if docker exec "${name}" systemctl is-system-running 2>/dev/null | grep -q '^\(running\|degraded\)$'; then
            return 0
        fi
        sleep 1
    done
    docker logs "${name}" >&2 || true
    die "${name}: systemd did not finish booting"
}

# The published 0.2.3 package, downloaded once and checked.
fetch_previous() {
    local file="${PREVIOUS_DIR}/coaxhub-stationd_${PREVIOUS_VERSION}_amd64.deb"
    if [[ ! -f "${file}" ]]; then
        mkdir -p "${PREVIOUS_DIR}"
        log "downloading ${PREVIOUS_URL}"
        curl --proto '=https' --tlsv1.2 -fsSL --max-time 120 -o "${file}.tmp" "${PREVIOUS_URL}"
        mv "${file}.tmp" "${file}"
    fi
    printf '%s  %s\n' "${PREVIOUS_SHA256}" "${file}" | sha256sum --check --strict --quiet ||
        die "${file}: not the published 0.2.3 package (SHA-256)"
    printf '%s\n' "${file}"
}

fresh_test() {
    local suite="$1" name="${run}-fresh-${1}"
    start_systemd "${name}" "${suite}"
    docker exec "${name}" mkdir -p /test/pkg
    docker cp "$(package_for "${suite}" amd64)" "${name}:/test/pkg/" >/dev/null
    run_in "${name}" "${TEST_DIR}/in-container/systemd.sh" fresh "${suite}"
}

upgrade_test() {
    local suite="$1" name="${run}-upgrade-${1}" previous
    previous="$(fetch_previous)" || return 1
    start_systemd "${name}" "${suite}"
    docker exec "${name}" mkdir -p /test/pkg /test/previous
    docker cp "$(package_for "${suite}" amd64)" "${name}:/test/pkg/" >/dev/null
    docker cp "${previous}" "${name}:/test/previous/" >/dev/null
    run_in "${name}" "${TEST_DIR}/in-container/systemd.sh" upgrade "${suite}"
}

for suite in "${suites[@]}"; do
    build_image "${suite}" || die "cannot build the ${suite} test image"
    record "${suite}: fresh install under systemd" fresh_test "${suite}"
    # The published package is for Debian 12: its upgrade is tested there.
    if ((upgrade)) && [[ "${suite}" == bookworm ]]; then
        record "${suite}: upgrade from ${PREVIOUS_VERSION}" upgrade_test "${suite}"
    fi
done
finish
