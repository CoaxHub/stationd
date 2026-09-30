#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Assembles coaxhub-stationd_<VERSION>_<ARCH>.deb inside the package build
# container (packaging/deb/Dockerfile, stage `package`).
#
# Usage: assemble.sh IN PKGSRC OUT
#   IN      radiod/{radiod,rtlsdr.so,presets.conf,ARCHOPTS} and
#           stationd/{stationd,rust-crates.txt}, built by the earlier stages
#   PKGSRC  packaging
#   OUT     where the .deb goes
# Environment: ARCH, SUITE, VERSION, COAXHUB_DOMAIN, KA9Q_COMMIT,
#   KA9Q_TARBALL_SHA256, SOURCE_DATE_EPOCH.

set -euo pipefail

in="$1"
src="$2"
out="$3"
: "${ARCH:?}" "${SUITE:?}" "${VERSION:?}" "${COAXHUB_DOMAIN:?}" "${KA9Q_COMMIT:?}"
: "${KA9Q_TARBALL_SHA256:?}" "${SOURCE_DATE_EPOCH:?}"

readonly PACKAGE=coaxhub-stationd
work="$(mktemp -d)"
root="${work}/root"
doc="${root}/usr/share/doc/${PACKAGE}"

# ---------------------------------------------------------------- files
install -D -m 0755 "${in}/stationd/stationd" "${root}/usr/bin/stationd"
install -D -m 0755 "${in}/radiod/radiod" "${root}/usr/lib/coaxhub/radiod/radiod"
install -m 0644 "${in}/radiod/rtlsdr.so" "${in}/radiod/presets.conf" "${root}/usr/lib/coaxhub/radiod/"
# Production SDR profiles only; siggen is for development builds.
install -d "${root}/usr/share/coaxhub/radiod-templates"
install -m 0644 "${src}/radiod-templates/rtlsdr.conf" "${src}/radiod-templates/rtlsdr-hf.conf" \
    "${root}/usr/share/coaxhub/radiod-templates/"
install -D -m 0644 "${src}/deb/stationd.toml" "${root}/etc/coaxhub/stationd.toml"
install -d "${root}/usr/lib/systemd/system"
install -m 0644 "${src}/systemd/stationd.service" "${src}/systemd/coaxhub-radiod.service" \
    "${root}/usr/lib/systemd/system/"
install -D -m 0644 "${src}/udev/72-coaxhub-rtlsdr.rules" "${root}/usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules"
install -D -m 0644 "${src}/modprobe/coaxhub-stationd.conf" "${root}/usr/lib/modprobe.d/coaxhub-stationd.conf"

install -d "${doc}"
short="${KA9Q_COMMIT:0:8}"
archopts="$(cat "${in}/radiod/ARCHOPTS")"
sed -e "s|@COAXHUB_DOMAIN@|${COAXHUB_DOMAIN}|g" -e "s|@KA9Q_COMMIT@|${KA9Q_COMMIT}|g" \
    -e "s|@KA9Q_SHORT@|${short}|g" -e "s|@KA9Q_TARBALL_SHA256@|${KA9Q_TARBALL_SHA256}|g" \
    -e "s|@ARCHOPTS@|${archopts}|g" -e '/^#/d' "${src}/deb/copyright.in" |
    awk -v crates="${in}/stationd/rust-crates.txt" '
        $0 == "@RUST_CRATES@" { while ((getline line < crates) > 0) print "  " line; next }
        { print }' >"${doc}/copyright"
chmod 0644 "${doc}/copyright"

# ---------------------------------------------------------------- control
install -d -m 0755 "${root}/DEBIAN"
install -m 0755 "${src}/deb/postinst" "${src}/deb/prerm" "${src}/deb/postrm" "${root}/DEBIAN/"
printf '/etc/coaxhub/stationd.toml\n' >"${root}/DEBIAN/conffiles"

# Shared-library dependencies of the three ELF objects, for the target
# architecture (dpkg-shlibdeps wants a debian/control to exist).
mkdir -p "${work}/shlibs/debian"
printf 'Source: stationd\n\nPackage: %s\nArchitecture: any\n' "${PACKAGE}" \
    >"${work}/shlibs/debian/control"
(
    cd "${work}/shlibs"
    # radiod exports its symbols (-rdynamic) for the driver it dlopen()s, so
    # rtlsdr.so has symbols no library defines: --warnings=0 silences that.
    dpkg-architecture -a"${ARCH}" -c dpkg-shlibdeps -O --warnings=0 \
        "${root}/usr/bin/stationd" \
        "${root}/usr/lib/coaxhub/radiod/radiod" \
        "${root}/usr/lib/coaxhub/radiod/rtlsdr.so"
) >"${work}/shlibs.out"
shlib_depends="$(sed -n 's/^shlibs:Depends=//p' "${work}/shlibs.out")"
[[ -n "${shlib_depends}" ]] || { echo "dpkg-shlibdeps found no dependencies" >&2; exit 1; }

installed_size="$(du -k -s --apparent-size --exclude=DEBIAN "${root}" | cut -f1)"
cat >"${root}/DEBIAN/control" <<CONTROL
Package: ${PACKAGE}
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: CoaxHub <packages@${COAXHUB_DOMAIN}>
Installed-Size: ${installed_size}
Depends: ${shlib_depends}, adduser, iproute2
Recommends: systemd-timesyncd | time-daemon
Section: hamradio
Priority: optional
Homepage: https://github.com/CoaxHub/stationd
Description: CoaxHub station daemon with its bundled SDR receiver
 stationd connects this receiver to its CoaxHub hub over an outbound,
 mutually authenticated TLS tunnel, so listeners can use it from a web
 browser. Nothing is exposed to the internet.
 .
 The package bundles a private build of ka9q-radio's radiod (commit
 ${short}) as the receiver's DSP backend, under /usr/lib/coaxhub/radiod.
 It is run as its own sandboxed service (coaxhub-radiod), configured from
 the SDR profile in /etc/coaxhub/stationd.toml, and not put on PATH.
 .
 Built for Debian ${SUITE} and the CoaxHub service at ${COAXHUB_DOMAIN}.
CONTROL

(cd "${root}" && find usr -type f -print0 | sort -z | xargs -0 md5sum) >"${root}/DEBIAN/md5sums"
chmod 0644 "${root}/DEBIAN/control" "${root}/DEBIAN/conffiles" "${root}/DEBIAN/md5sums"

# ---------------------------------------------------------------- build
find "${root}" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +
mkdir -p "${out}"
deb="${out}/${PACKAGE}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group -Zxz --build "${root}" "${deb}"
dpkg-deb --info "${deb}"
