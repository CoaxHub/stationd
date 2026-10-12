#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Runs inside a debian:<suite> container: checks a built package without
# installing it (so it works for arm64 on an amd64 host): control fields,
# the file list, the ELF machine of every binary, the private librtlsdr and
# the source tarballs.
#
# Usage: artefacts.sh SUITE ARCH   (the package is /test/pkg/*_ARCH.deb)

# shellcheck source-path=SCRIPTDIR source=common.sh
. /test/common.sh

readonly SUITE="${1:?suite}" ARCH="${2:?arch}"
readonly LIB=usr/lib/coaxhub/radiod
deb="$(ls /test/pkg/coaxhub-stationd_*_"${ARCH}".deb)"
case "${SUITE}" in bookworm) debv=12 ;; trixie) debv=13 ;; esac
case "${ARCH}" in amd64) machine="x86-64" ;; arm64) machine="ARM aarch64" ;; esac

apt_quiet update && apt_quiet install --no-install-recommends file binutils || exit 1

info="$(dpkg-deb --info "${deb}")"
field() { dpkg-deb --field "${deb}" "$1"; }
check_eq "Package" coaxhub-stationd "$(field Package)"
check_eq "Architecture" "${ARCH}" "$(field Architecture)"
check_contains "Version ends in ~deb${debv}" "~deb${debv}" "$(field Version)"
check_contains "file name matches the version" "_$(field Version)_${ARCH}.deb" "${deb}"
depends="$(field Depends)"
for dep in libc6 libopus0 libfftw3-single3 libiniparser libusb-1.0-0 adduser iproute2 polkitd; do
    check_contains "Depends: ${dep}" "${dep}" "${depends}"
done
check_lacks "Depends: no librtlsdr (the package bundles its own)" "librtlsdr" "${depends}"
check_contains "maintainer scripts" "postinst" "${info}"
check_lacks "no conffiles" "conffiles" "${info}"
check_eq "no conffiles file in the control archive" "" "$(dpkg-deb --ctrl-tarfile "${deb}" | tar -t | grep conffiles)"

contents="$(dpkg-deb --contents "${deb}")"
for path in ./usr/bin/stationd ./${LIB}/radiod ./${LIB}/rtlsdr.so ./${LIB}/librtlsdr.so.0 \
    ./${LIB}/presets.conf ./usr/share/coaxhub/radiod-templates/rtlsdr.conf \
    ./usr/share/coaxhub/radiod-templates/rtlsdr-hf.conf \
    ./usr/share/coaxhub/radiod-templates/rtlsdr-v4.conf \
    ./usr/lib/systemd/system/stationd.service ./usr/lib/systemd/system/coaxhub-radiod@.service \
    ./usr/share/polkit-1/rules.d/coaxhub-stationd.rules \
    ./usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules ./usr/lib/modprobe.d/coaxhub-stationd.conf \
    ./usr/share/doc/coaxhub-stationd/copyright; do
    check_contains "contains ${path}" " ${path}"$'\n' "${contents}"$'\n'
done
check_lacks "no siggen template" "siggen" "${contents}"
check_lacks "no coaxhub-radiod.service (only the template)" " ./usr/lib/systemd/system/coaxhub-radiod.service" "${contents}"
check_lacks "nothing under /etc" " ./etc/" "${contents}"
check_eq "every file is owned by root" "" "$(awk '$2 != "root/root"' <<<"${contents}")"

x="$(mktemp -d)"
dpkg-deb -x "${deb}" "${x}"
# The corresponding source of radiod, next to the package (GPL).
tarball=/test/pkg/ka9q-radio-384b269a.tar.gz
check "the ka9q-radio source tarball is next to the package" test -f "${tarball}"
check_contains "copyright names the tarball's SHA-256" "$(sha256sum "${tarball}" | cut -d' ' -f1)" \
    "$(cat "${x}/usr/share/doc/coaxhub-stationd/copyright")"
check_contains "the tarball holds radiod's source" "ka9q-radio-384b269a/src/radio.c" \
    "$(tar -tzf "${tarball}")"
# The corresponding source of the bundled librtlsdr (GPL).
tarball=/test/pkg/rtl-sdr-619ac318.tar.gz
check "the rtl-sdr source tarball is next to the package" test -f "${tarball}"
check_contains "copyright names the rtl-sdr tarball's SHA-256" "$(sha256sum "${tarball}" | cut -d' ' -f1)" \
    "$(cat "${x}/usr/share/doc/coaxhub-stationd/copyright")"
check_contains "the tarball holds librtlsdr's source" "rtl-sdr-619ac318/src/librtlsdr.c" \
    "$(tar -tzf "${tarball}")"
# rtlsdr.so links the private librtlsdr, and finds it beside itself.
dynamic="$(readelf -d "${x}/${LIB}/rtlsdr.so")"
check_contains "rtlsdr.so needs librtlsdr.so.0" "[librtlsdr.so.0]" "${dynamic}"
check_contains "rtlsdr.so: RUNPATH /${LIB}" "Library runpath: [/${LIB}]" "${dynamic}"
dynamic="$(readelf -d "${x}/${LIB}/librtlsdr.so.0")"
check_contains "librtlsdr.so.0: its SONAME" "Library soname: [librtlsdr.so.0]" "${dynamic}"
check_contains "librtlsdr.so.0 needs libusb" "[libusb-1.0.so.0]" "${dynamic}"
check "librtlsdr.so.0 knows the RTL-SDR Blog V4" grep -q 'Blog V4' "${x}/${LIB}/librtlsdr.so.0"
for f in usr/bin/stationd ${LIB}/radiod ${LIB}/rtlsdr.so ${LIB}/librtlsdr.so.0; do
    desc="$(file -b "${x}/${f}")"
    check_contains "${f##*/} is ${machine}" "ELF 64-bit LSB" "${desc}"
    check_contains "${f##*/} machine: ${machine}" "${machine}" "${desc}"
    check_contains "${f##*/} is stripped" ", stripped" "${desc}"
done
check_contains "stationd is dynamically linked" "dynamically linked" "$(file -b "${x}/usr/bin/stationd")"
check_eq "no PEM file is shipped" "" "$(find "${x}" -name '*.pem')"

done_testing
