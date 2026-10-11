#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Builds the coaxhub-stationd .deb for one Debian suite and architecture
# in a debian:<suite> container on the Docker
# daemon, with BuildKit. arm64 is cross-compiled: no emulation is needed.
#
# Usage: packaging/build-deb.sh --suite bookworm|trixie --arch amd64|arm64
#
# Environment:
#   COAXHUB_DOMAIN     required: the CoaxHub domain built into stationd
#   COAXHUB_ROOT_CA    required: PEM file with the CoaxHub root certificate
#                      built into stationd
#   PACKAGE_REVISION   Debian revision (default 1): version <crate>-<rev>~deb<N>
#   RUST_VERSION       rustup toolchain for the build image (default: the channel
#                      in rust-toolchain.toml)
#   RADIOD_ARCHOPTS    override radiod's ARCHOPTS (default: x86-64-v2 for amd64,
#                      armv8-a tuned for Cortex-A53 for arm64; never -march=native)
#   DOCKER_PROGRESS    docker build --progress (default auto)
#
# Output (target/packages/<suite>/):
#   coaxhub-stationd_<version>_<arch>.deb
#   ka9q-radio-<commit8>.tar.gz   the radiod corresponding source (GPL)
#   rtl-sdr-<commit8>.tar.gz      the librtlsdr corresponding source (GPL)
#
# Caches: the ka9q-radio and rtl-sdr commits in target/packaging/ka9q-radio.git
# and target/packaging/rtl-sdr.git; the cargo registry and target directories
# in BuildKit cache mounts; the build toolchain as image layers.

set -euo pipefail

readonly KA9Q_REPO="https://github.com/ka9q/ka9q-radio.git"
readonly KA9Q_COMMIT="384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd"
# osmocom librtlsdr, tag v2.0.2: the RTL-SDR Blog V4 needs 2.0.0 or newer.
readonly RTLSDR_REPO="https://gitea.osmocom.org/sdr/rtl-sdr.git"
readonly RTLSDR_VERSION="2.0.2"
readonly RTLSDR_COMMIT="619ac3186ea0ffc092615e1f59f7397e5e6f668c"

station="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly station
readonly packages="${station}/target/packages"
readonly cache="${station}/target/packaging"

log() { printf '[build-deb] %s\n' "$*" >&2; }
die() {
    log "ERROR: $*"
    exit 1
}
usage() {
    sed -n '3,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

suite=""
arch=""
while (($#)); do
    case "$1" in
    --suite) suite="${2:-}"; shift 2 ;;
    --arch) arch="${2:-}"; shift 2 ;;
    -h | --help) usage 0 ;;
    *) log "unknown argument: $1"; usage 2 >&2 ;;
    esac
done

case "${suite}" in
bookworm) debian_version=12 ;;
trixie) debian_version=13 ;;
*) die "--suite must be bookworm or trixie" ;;
esac
case "${arch}" in
amd64 | arm64) ;;
*) die "--arch must be amd64 or arm64" ;;
esac
: "${COAXHUB_DOMAIN:?set COAXHUB_DOMAIN (e.g. coaxhub.test)}"
: "${COAXHUB_ROOT_CA:?set COAXHUB_ROOT_CA to the CoaxHub root certificate (PEM)}"
[[ -f "${COAXHUB_ROOT_CA}" ]] || die "COAXHUB_ROOT_CA=${COAXHUB_ROOT_CA}: no such file"
grep -q -- '-----BEGIN CERTIFICATE-----' "${COAXHUB_ROOT_CA}" ||
    die "COAXHUB_ROOT_CA=${COAXHUB_ROOT_CA}: no PEM certificate"
if grep -q 'PRIVATE KEY' "${COAXHUB_ROOT_CA}"; then
    die "COAXHUB_ROOT_CA=${COAXHUB_ROOT_CA} holds a private key; give the certificate only"
fi
revision="${PACKAGE_REVISION:-1}"
[[ "${revision}" =~ ^[0-9][0-9A-Za-z.+]*$ ]] || die "PACKAGE_REVISION=${revision}: not a Debian revision"

crate_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "${station}/crates/stationd/Cargo.toml" | head -n1)"
[[ -n "${crate_version}" ]] || die "cannot read stationd's version from its Cargo.toml"
readonly version="${crate_version}-${revision}~deb${debian_version}"

# fetch_source NAME REPO COMMIT DIR: the pinned COMMIT of NAME, fetched once
# into target/packaging/NAME.git, and its source tarball
# target/packaging/DIR/NAME-<commit8>.tar.gz (`git archive`, gzip without a
# timestamp: the same bytes on every run): what the package's binaries are
# built from (DIR is their named build context), and what is published next
# to the package. Sets SOURCE_TIME, SOURCE_SUMMARY and SOURCE_SHA256.
fetch_source() {
    local name="$1" repo="$2" commit="$3" dir="$4"
    local short="${commit:0:8}" git_dir="${cache}/${name}.git"
    if [[ ! -d "${git_dir}" ]]; then
        mkdir -p "${cache}"
        git init --quiet --bare "${git_dir}"
    fi
    if ! git -C "${git_dir}" cat-file -e "${commit}^{commit}" 2>/dev/null; then
        log "fetching ${name} ${short} from ${repo}"
        git -C "${git_dir}" fetch --quiet --depth 1 "${repo}" "${commit}"
    fi
    git -C "${git_dir}" cat-file -e "${commit}^{commit}" ||
        die "${name} commit ${commit} not found"
    local tarball="${cache}/${dir}/${name}-${short}.tar.gz"
    if [[ ! -f "${tarball}" ]]; then
        mkdir -p "${cache}/${dir}"
        git -C "${git_dir}" archive --format=tar --prefix="${name}-${short}/" "${commit}" |
            gzip -n -9 >"${tarball}.tmp"
        mv "${tarball}.tmp" "${tarball}"
    fi
    # Next to the packages of the suite, where scripts/apt-repo.sh looks.
    mkdir -p "${packages}/${suite}"
    cp -p "${tarball}" "${packages}/${suite}/"
    SOURCE_TIME="$(git -C "${git_dir}" show -s --format=%ci "${commit}")"
    SOURCE_SUMMARY="$(git -C "${git_dir}" show -s --format=%s "${commit}")"
    SOURCE_SHA256="$(sha256sum "${tarball}" | cut -d' ' -f1)"
}

main() {
    local start
    start="$(date +%s)"
    fetch_source ka9q-radio "${KA9Q_REPO}" "${KA9Q_COMMIT}" ka9q-src
    local KA9Q_COMMIT_TIME="${SOURCE_TIME}" KA9Q_SUMMARY="${SOURCE_SUMMARY}"
    local KA9Q_TARBALL_SHA256="${SOURCE_SHA256}"
    fetch_source rtl-sdr "${RTLSDR_REPO}" "${RTLSDR_COMMIT}" rtl-sdr-src
    local RTLSDR_TARBALL_SHA256="${SOURCE_SHA256}"

    local tmp
    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064 # expand tmp now
    trap "rm -rf '${tmp}'" EXIT
    # Only the certificate goes into the build, never its directory.
    mkdir "${tmp}/rootca" "${tmp}/out"
    cp "${COAXHUB_ROOT_CA}" "${tmp}/rootca/root.pem"

    local epoch
    epoch="$(git -C "${station}" log -1 --format=%ct 2>/dev/null || date +%s)"

    log "building coaxhub-stationd ${version} for ${suite}/${arch} (domain ${COAXHUB_DOMAIN})"
    local args=(
        build
        --progress "${DOCKER_PROGRESS:-auto}"
        --file "${station}/packaging/deb/Dockerfile"
        --target export
        --output "type=local,dest=${tmp}/out"
        --build-context "ka9q=${cache}/ka9q-src"
        --build-context "rtlsdr=${cache}/rtl-sdr-src"
        --build-context "rootca=${tmp}/rootca"
        --build-arg "SUITE=${suite}"
        --build-arg "ARCH=${arch}"
        --build-arg "VERSION=${version}"
        --build-arg "COAXHUB_DOMAIN=${COAXHUB_DOMAIN}"
        --build-arg "RUST_VERSION=${RUST_VERSION:-$(sed -n 's/^channel = "\(.*\)"$/\1/p' "${station}/rust-toolchain.toml")}"
        --build-arg "RADIOD_ARCHOPTS=${RADIOD_ARCHOPTS:-}"
        --build-arg "KA9Q_COMMIT=${KA9Q_COMMIT}"
        --build-arg "KA9Q_COMMIT_TIME=${KA9Q_COMMIT_TIME}"
        --build-arg "KA9Q_SUMMARY=${KA9Q_SUMMARY}"
        --build-arg "KA9Q_TARBALL_SHA256=${KA9Q_TARBALL_SHA256}"
        --build-arg "RTLSDR_VERSION=${RTLSDR_VERSION}"
        --build-arg "RTLSDR_COMMIT=${RTLSDR_COMMIT}"
        --build-arg "RTLSDR_TARBALL_SHA256=${RTLSDR_TARBALL_SHA256}"
        --build-arg "SOURCE_DATE_EPOCH=${epoch}"
        "${station}"
    )
    docker "${args[@]}"

    local deb="coaxhub-stationd_${version}_${arch}.deb"
    [[ -f "${tmp}/out/${deb}" ]] || die "the build produced no ${deb}"
    mkdir -p "${packages}/${suite}"
    rm -f "${packages}/${suite}"/coaxhub-stationd_*_"${arch}".deb
    mv "${tmp}/out/${deb}" "${packages}/${suite}/${deb}"

    local size
    size="$(stat -c %s "${packages}/${suite}/${deb}")"
    log "built target/packages/${suite}/${deb} ($((size / 1024)) KiB) in $(($(date +%s) - start)) s"
}

main
