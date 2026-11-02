#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Runs inside a clean container of the suite under test (amd64): the package
# refuses a system it was not built for, in its preinst, before anything is
# unpacked (docs/packaging.md §5).
#
#   - a package for another system is refused by dpkg, naming this system and
#     the package's, listing what is supported, and leaving nothing: no file,
#     no user, no package. It is made from this suite's own package (dpkg-deb
#     -R, the preinst's CODENAME, OS_IDS and TARGET_NAME set to another
#     suite's as assemble.sh fills them, dpkg-deb -b), so that the real
#     preinst text the build produced is what refuses and only this suite's
#     packages are needed; when that other suite's real package was built too
#     (/test/wrong/*.deb) it is tried as well;
#   - this suite's package (/test/pkg/*.deb) is refused on systems that only
#     look like it: a derivative (another ID that names this one in ID_LIKE),
#     a release that is not a target (an old Debian or a non-LTS Ubuntu), no
#     /etc/os-release at all;
#   - it installs where it should, and on Raspberry Pi OS (ID=raspbian) for
#     the Debian suites, and nowhere else (Ubuntu's package refuses raspbian).
#
# Usage: refusal.sh

# shellcheck source-path=SCRIPTDIR source=common.sh
. /test/common.sh
# shellcheck source=/dev/null
. /test/suite.env

# shellcheck source=/dev/null
. /test/other.env
right="$(ls /test/pkg/coaxhub-stationd_*_amd64.deb)"
# The package for another system, from this suite's own: only the three filled-in
# lines of the preinst differ. Its Pre-Depends is this suite's, which is met here.
rm -rf /root/derived
dpkg-deb -R "${right}" /root/derived
sed -i -e "s|^OS_IDS=.*|OS_IDS=\"${OTHER_OS_IDS}\"|" -e "s|^CODENAME=.*|CODENAME=\"${OTHER_CODENAME}\"|" \
    -e "s|^TARGET_NAME=.*|TARGET_NAME=\"${OTHER_TARGET_NAME}\"|" /root/derived/DEBIAN/preinst
derived=/root/derived.deb
dpkg-deb --root-owner-group -b /root/derived "${derived}" >/dev/null
changed="$(diff <(dpkg-deb -I "${right}" preinst) <(dpkg-deb -I "${derived}" preinst) | grep '^[<>]')"
check_eq "the derived package differs from the real one in the preinst's filled-in lines only" "" \
    "$(grep -vE '^[<>] (OS_IDS|CODENAME|TARGET_NAME)=' <<<"${changed}")"
check_contains "and it names the other system" "CODENAME=\"${OTHER_CODENAME}\"" "${changed}"
cp -P /etc/os-release /root/os-release.orig
real_pretty="$(. /etc/os-release && echo "${PRETTY_NAME}")"

nothing_installed() {
    ! dpkg-query -W -f='${Status}' coaxhub-stationd 2>/dev/null | grep -q 'ok installed' &&
        [[ ! -e /usr/bin/stationd && ! -e /usr/lib/coaxhub ]] && ! getent passwd coaxhub >/dev/null
}

os_release() { # ID VERSION_CODENAME [PRETTY_NAME] [ID_LIKE]
    rm -f /etc/os-release
    printf 'ID=%s\nVERSION_CODENAME=%s\nPRETTY_NAME="%s"\n' "$1" "$2" "${3:-$1 $2}" >/etc/os-release
    [[ -n "${4:-}" ]] && printf 'ID_LIKE=%s\n' "$4" >>/etc/os-release
    return 0
}
# dpkg --remove with a system that does not match: the preinst is not run.
remove_on_fedora() {
    os_release fedora 40 "Fedora Linux 40"
    dpkg --remove coaxhub-stationd
    local rc=$?
    restore_os
    return "${rc}"
}
restore_os() { rm -f /etc/os-release; cp -P /root/os-release.orig /etc/os-release; }

# ------------------------------------------------ a package for another system
wrong_checks() { # LABEL DEB TARGET_NAME
    local out rc
    out="$(dpkg -i "$2" 2>&1)"
    rc=$?
    check_eq "$1: dpkg refuses it" 1 "${rc}"
    check_contains "$1: it says what the package is for" "this package is for $3" "${out}"
    check_contains "$1: it names this system" "${real_pretty}" "${out}"
    check_contains "$1: it says nothing was installed" "nothing was installed" "${out}"
    check_contains "$1: it lists what is supported" "${SUPPORTED_TEXT}" "${out}"
    check "$1: nothing of it is left (no package, no file, no user)" nothing_installed
}
wrong_checks "the package for ${OTHER_TARGET_NAME}, derived from this suite's" "${derived}" "${OTHER_TARGET_NAME}"
for real in /test/wrong/coaxhub-stationd_*_amd64.deb; do
    [[ -e "${real}" ]] || continue
    wrong_checks "the real package ${real##*/}" "${real}" "$(dpkg-deb --field "${real}" Description | sed -n 's/.*Built for \(.*\) and the CoaxHub service.*/\1/p')"
done
check_contains "the package carries the suffix it was built with" "~" "$(dpkg-deb --field "${right}" Version)"

# ------------------------------------------------ this suite's package on systems that only look like it
refused() { # WHAT: dpkg -i of the right package is refused, naming this fake system
    local out rc
    out="$(dpkg -i "${right}" 2>&1)"
    rc=$?
    check_eq "$1: refused" 1 "${rc}"
    check_contains "$1: names the package's system" "this package is for ${TARGET_NAME}" "${out}"
    check "$1: nothing is left" nothing_installed
}
# shellcheck disable=SC2086 # OS_IDS is a list: its first word is a real ID of this suite
first_id="${OS_IDS%% *}"
os_release linuxmint "${CODENAME}" "Linux Mint (derived from ${CODENAME})" "${first_id}"
refused "a derivative that names ${first_id} in ID_LIKE"
os_release "${first_id}" xenial "Some Old Release"
refused "a release that is not a target (${first_id} xenial)"
os_release "${first_id}" bullseye "Debian GNU/Linux 11 (bullseye)"
refused "an end-of-life release (bullseye)"
os_release "${first_id}" mantic "Ubuntu 23.10"
refused "a non-LTS release (mantic)"
os_release fedora 40 "Fedora Linux 40"
refused "Fedora"
rm -f /etc/os-release
out="$(dpkg -i "${right}" 2>&1)"
rc=$?
check_eq "no /etc/os-release: refused" 1 "${rc}"
check_contains "no /etc/os-release: says so" "no /etc/os-release" "${out}"
check "no /etc/os-release: nothing is left" nothing_installed
restore_os

# remove and purge of a package that was never installed never check anything
os_release fedora 40 "Fedora Linux 40"
check_eq "purge of the absent package is not blocked" 0 "$(exit_code dpkg --purge coaxhub-stationd)"
restore_os

# ------------------------------------------------ Raspberry Pi OS
apt_quiet update || exit 1
if [[ " ${OS_IDS} " == *" raspbian "* ]]; then
    os_release raspbian "${CODENAME}" "Raspbian GNU/Linux (${CODENAME})" debian
    check "Raspberry Pi OS (ID=raspbian) is accepted by the ${CODENAME} package" apt_quiet install --no-install-recommends "${right}"
    check_eq "and it is installed" "install ok installed" "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>&1)"
    # An upgrade is checked too: the same package over itself (--reinstall runs the
    # preinst with the old version as $2).
    check "an upgrade on Raspberry Pi OS is accepted" apt_quiet install --reinstall --no-install-recommends "${right}"
    os_release fedora 40 "Fedora Linux 40"
    out="$(dpkg -i "${right}" 2>&1)"
    rc=$?
    check_eq "an upgrade on a system that does not match is refused" 1 "${rc}"
    check_eq "a refused upgrade leaves the installed version in place" "install ok installed" \
        "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>&1)"
    check "and its files" test -x /usr/bin/stationd
    restore_os
    check "remove is not blocked on a system that does not match" remove_on_fedora
else
    os_release raspbian "${CODENAME}" "Raspbian GNU/Linux (${CODENAME})" debian
    out="$(dpkg -i "${right}" 2>&1)"
    rc=$?
    check_eq "Raspberry Pi OS is refused by the ${CODENAME} package" 1 "${rc}"
    check "Raspberry Pi OS: nothing is left" nothing_installed
    restore_os
    check "the package installs on ${real_pretty}" apt_quiet install --no-install-recommends "${right}"
    check_eq "and it is installed" "install ok installed" "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>&1)"
    os_release linuxmint "${CODENAME}" "Linux Mint" ubuntu
    out="$(dpkg -i "${right}" 2>&1)"
    rc=$?
    check_eq "an upgrade on a derivative is refused" 1 "${rc}"
    check_eq "a refused upgrade leaves the installed version in place" "install ok installed" \
        "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>&1)"
    restore_os
fi
check "apt-get purge" apt_quiet purge coaxhub-stationd

done_testing
