#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Runs inside a clean debian:<suite> amd64 container: installs the package
# from /test/pkg/*.deb, checks what contract B says it contains, then removes
# and purges it and checks that nothing is left.
#
# Usage: package.sh without-systemd|with-systemd
#   without-systemd  the bare image: maintainer scripts must cope without
#                    systemctl
#   with-systemd     systemd installed first (not running), as on a real
#                    system: systemd-analyze verify, time-wait-sync enabled
#                    and disabled again on purge

# shellcheck source-path=SCRIPTDIR source=common.sh
. /test/common.sh

readonly MODE="${1:?without-systemd|with-systemd}"
readonly KA9Q_COMMIT=384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd
readonly RTLSDR_COMMIT=619ac3186ea0ffc092615e1f59f7397e5e6f668c
deb="$(ls /test/pkg/coaxhub-stationd_*_amd64.deb)"

apt_quiet update || exit 1
if [[ "${MODE}" == with-systemd ]]; then
    # systemd-timesyncd ships systemd-time-wait-sync.service on trixie.
    apt_quiet install --no-install-recommends systemd systemd-timesyncd udev || exit 1
fi

# ---------------------------------------------------------------- install
check "apt-get install resolves every dependency" apt_quiet install --no-install-recommends "${deb}"
check_eq "the package is installed" "install ok installed" \
    "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>&1)"
check "dpkg --verify: files match md5sums" test -z "$(dpkg --verify coaxhub-stationd 2>&1)"
check_lacks "the suite's librtlsdr is not installed with it" "ok installed" \
    "$(dpkg-query -W -f='${Status}\n' 'librtlsdr*' 2>&1)"

# Users.
for user in coaxhub coaxhub-radiod; do
    entry="$(getent passwd "${user}")"
    check "user ${user} exists" test -n "${entry}"
    uid="$(cut -d: -f3 <<<"${entry}")"
    check "user ${user} is a system user (uid ${uid})" test "${uid:-1000}" -lt 1000
    check_eq "user ${user} has no login shell" /usr/sbin/nologin "$(cut -d: -f7 <<<"${entry}")"
    check_eq "user ${user}'s primary group is ${user}" "${user}" "$(id -gn "${user}" 2>&1)"
done
check_eq "coaxhub is not in the coaxhub-radiod group (no SDR access)" "coaxhub" "$(id -Gn coaxhub)"

# State directories.
check_eq "/var/lib/coaxhub" "755 root root" "$(mode_owner /var/lib/coaxhub)"
check_eq "/var/lib/coaxhub/stationd" "700 coaxhub coaxhub" "$(mode_owner /var/lib/coaxhub/stationd)"
check_eq "/var/lib/coaxhub-radiod" "755 coaxhub-radiod coaxhub-radiod" "$(mode_owner /var/lib/coaxhub-radiod)"

# Files and modes.
for f in "755 /usr/bin/stationd" "755 /usr/lib/coaxhub/radiod/radiod" \
    "644 /usr/lib/coaxhub/radiod/rtlsdr.so" "644 /usr/lib/coaxhub/radiod/librtlsdr.so.0" \
    "644 /usr/lib/coaxhub/radiod/presets.conf" \
    "644 /usr/share/coaxhub/radiod-templates/rtlsdr.conf" \
    "644 /usr/share/coaxhub/radiod-templates/rtlsdr-hf.conf" \
    "644 /usr/share/coaxhub/radiod-templates/rtlsdr-v4.conf" \
    "644 /etc/coaxhub/stationd.toml" \
    "644 /usr/lib/systemd/system/stationd.service" \
    "644 /usr/lib/systemd/system/coaxhub-radiod.service" \
    "644 /usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules" \
    "644 /usr/lib/modprobe.d/coaxhub-stationd.conf" \
    "644 /usr/share/doc/coaxhub-stationd/copyright"; do
    check_eq "${f#* } (mode ${f%% *}, root)" "${f%% *} root root" "$(mode_owner "${f#* }")"
done
check "no development-only template (siggen)" test ! -e /usr/share/coaxhub/radiod-templates/siggen.conf
check_not "radiod is not on PATH" command -v radiod
check_contains "stationd.toml is a conffile" /etc/coaxhub/stationd.toml \
    "$(dpkg-query -W -f='${Conffiles}' coaxhub-stationd)"
check_contains "the default profile is rtlsdr" 'profile = "rtlsdr"' "$(cat /etc/coaxhub/stationd.toml)"
check_contains "the blacklist names dvb_usb_rtl28xxu" "blacklist dvb_usb_rtl28xxu" \
    "$(cat /usr/lib/modprobe.d/coaxhub-stationd.conf)"
copyright="$(cat /usr/share/doc/coaxhub-stationd/copyright)"
check_contains "copyright names the ka9q-radio commit" "${KA9Q_COMMIT}" "${copyright}"
check_contains "copyright names the source tarball" "ka9q-radio-${KA9Q_COMMIT:0:8}.tar.gz" "${copyright}"
check_contains "copyright: GPL-3.0-only" "License: GPL-3.0-only" "${copyright}"
check_contains "copyright names the rtl-sdr commit" "${RTLSDR_COMMIT}" "${copyright}"
check_contains "copyright names the rtl-sdr tarball" "rtl-sdr-${RTLSDR_COMMIT:0:8}.tar.gz" "${copyright}"
check_contains "copyright: librtlsdr is GPL-2.0-or-later" \
    $'Files: usr/lib/coaxhub/radiod/librtlsdr.so.0\nCopyright: 2012-2014 Steve Markgraf' "${copyright}"
check_contains "copyright: the GPL-2.0-or-later text" "License: GPL-2.0-or-later"$'\n' "${copyright}"
check_contains "copyright lists the Rust crates" "rustls v" "${copyright}"
check_eq "copyright has no unfilled field" "" "$(grep -E '@[A-Z0-9_]+@' <<<"${copyright}")"

# ---------------------------------------------------------------- stationd
version="$(stationd --version 2>&1)"
check_contains "stationd --version" "stationd 0." "${version}"
out="$(stationd check-config 2>&1)"
rc=$?
check_eq "stationd check-config exits 0 when not enrolled" 0 "${rc}"
check_contains "check-config: not enrolled" "not enrolled yet" "${out}"
check_contains "check-config: packaged build" "packaged build" "${out}"
check_eq "stationd status exits 3 when not enrolled" 3 "$(exit_code stationd status)"
check_eq "stationd run exits 3 when not enrolled (as coaxhub)" 3 \
    "$(exit_code runuser -u coaxhub -- stationd run)"

# backend-config renders both profiles.
rendered="$(stationd backend-config --out - 2>&1)"
check_contains "rtlsdr: hardware" "hardware = rtlsdr" "${rendered}"
check_contains "rtlsdr: fixed status group" "status = 239.77.0.6" "${rendered}"
check_contains "rtlsdr: fixed data group" "data = 239.77.0.4" "${rendered}"
check_contains "rtlsdr: ttl 0" "ttl = 0" "${rendered}"
check_contains "rtlsdr: default frequency" "frequency = 98000000" "${rendered}"
check_contains "rtlsdr: packaged template dir" "/usr/share/coaxhub/radiod-templates/rtlsdr.conf" "${rendered}"
hf="$(mktemp -d)"
printf '[sdr]\nprofile = "rtlsdr-hf"\n' >"${hf}/stationd.toml"
rendered="$(stationd backend-config --config "${hf}/stationd.toml" --out - 2>&1)"
check_contains "rtlsdr-hf: direct sampling" "direct_sampling = 2" "${rendered}"
check_contains "rtlsdr-hf: default frequency" "frequency = 7100000" "${rendered}"
printf '[sdr]\nprofile = "rtlsdr-v4"\n' >"${hf}/stationd.toml"
rendered="$(stationd backend-config --config "${hf}/stationd.toml" --out - 2>&1)"
check_contains "rtlsdr-v4: its template" "/usr/share/coaxhub/radiod-templates/rtlsdr-v4.conf" "${rendered}"
check_lacks "rtlsdr-v4: no direct sampling" "direct_sampling =" "${rendered}"
check_contains "rtlsdr-v4: the tuner's gain" "gain = 30" "${rendered}"
check_contains "rtlsdr-v4: default frequency" "frequency = 7100000" "${rendered}"
check_contains "rtlsdr-v4: its description" 'description = "RTL-SDR Blog V4"' "${rendered}"
printf '[sdr]\nprofile = "siggen"\n' >"${hf}/stationd.toml"
check_eq "a packaged build refuses the siggen profile" 2 \
    "$(exit_code stationd backend-config --config "${hf}/stationd.toml" --out -)"
printf '[sdr]\nprofile = "rtlsdr"\n[backend]\nkind = "fake"\n' >"${hf}/stationd.toml"
check_eq "a packaged build refuses [backend]" 2 \
    "$(exit_code stationd check-config --config "${hf}/stationd.toml")"
# As coaxhub-radiod.service runs it: its user, into its runtime directory.
install -d -o coaxhub-radiod -g coaxhub-radiod /run/coaxhub-radiod
check "backend-config as coaxhub-radiod into /run/coaxhub-radiod" \
    runuser -u coaxhub-radiod -- stationd backend-config --out /run/coaxhub-radiod/radiod.conf
check_eq "radiod.conf mode" "644 coaxhub-radiod coaxhub-radiod" "$(mode_owner /run/coaxhub-radiod/radiod.conf)"

# ---------------------------------------------------------------- radiod
out="$(/usr/lib/coaxhub/radiod/radiod -V 2>&1)"
check_contains "radiod -V reports the pinned commit" "${KA9Q_COMMIT}" "${out}"
for f in /usr/lib/coaxhub/radiod/radiod /usr/lib/coaxhub/radiod/rtlsdr.so \
    /usr/lib/coaxhub/radiod/librtlsdr.so.0 /usr/bin/stationd; do
    check_lacks "${f##*/}: every shared library resolves" "not found" "$(ldd "${f}" 2>&1)"
done
check_contains "rtlsdr.so resolves the private librtlsdr" \
    "librtlsdr.so.0 => /usr/lib/coaxhub/radiod/librtlsdr.so.0" "$(ldd /usr/lib/coaxhub/radiod/rtlsdr.so 2>&1)"
# Run it on the rendered config with no SDR attached: it must find its
# presets and driver at the compiled-in private paths, then fail on the
# missing device (not on a path or a config error). The loopback MULTICAST
# flag is what the unit's privileged ExecStartPre sets (the test container
# has CAP_NET_ADMIN in its own network namespace).
check "set the loopback MULTICAST flag (as coaxhub-radiod.service does)" ip link set dev lo multicast on
out="$(cd / && timeout 20 runuser -u coaxhub-radiod -- /usr/lib/coaxhub/radiod/radiod \
    /run/coaxhub-radiod/radiod.conf 2>&1)"
check_contains "radiod loads presets from /usr/lib/coaxhub/radiod" \
    "/usr/lib/coaxhub/radiod/presets.conf" "${out}"
check_lacks "radiod finds the rtlsdr driver" "dlopen" "${out}"
check_contains "radiod reaches the device probe (no SDR here)" "rtlsdr" "$(tr '[:upper:]' '[:lower:]' <<<"${out}")"
printf '# radiod without an SDR said:\n%s\n' "$(tail -n 5 <<<"${out}" | sed 's/^/#   /')"

# ---------------------------------------------------------------- systemd
if [[ "${MODE}" == with-systemd ]]; then
    for unit in stationd.service coaxhub-radiod.service; do
        check "systemd-analyze verify ${unit}" \
            systemd-analyze verify "/usr/lib/systemd/system/${unit}"
    done
    # systemd's own exposure score (0 best, 10 worst); both are "OK" today.
    for limit in "stationd 2.5" "coaxhub-radiod 4.5"; do
        read -r unit max <<<"${limit}"
        score="$(systemd-analyze security --offline=true "/usr/lib/systemd/system/${unit}.service" 2>&1 |
            sed -n 's/.*Overall exposure level for .*: \([0-9.]*\) .*/\1/p')"
        check "${unit}.service exposure ${score:-?} <= ${max} (systemd-analyze security)" \
            awk -v s="${score:-99}" -v m="${max}" 'BEGIN { exit !(s + 0 <= m + 0) }'
    done
    check "postinst enabled systemd-time-wait-sync" \
        systemctl is-enabled --quiet systemd-time-wait-sync.service
    unit="$(cat /usr/lib/systemd/system/stationd.service)"
    check_contains "stationd.service: RestartPreventExitStatus=3" "RestartPreventExitStatus=3" "${unit}"
    check_contains "stationd.service: after time-sync.target" "time-sync.target" "${unit}"
    if udevadm verify --help >/dev/null 2>&1; then
        check "udevadm verify the rule" udevadm verify /usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules
    fi
    # As the installer does, so purge has something to clean up.
    check "systemctl enable the units (offline)" \
        systemctl enable coaxhub-radiod.service stationd.service
fi

# ---------------------------------------------------------------- reinstall
# A fake enrollment must survive a reinstall and a remove.
echo "marker" >/var/lib/coaxhub/stationd/key.pem
chown coaxhub:coaxhub /var/lib/coaxhub/stationd/key.pem
sed -i 's/^max_channels = 16/max_channels = 8/' /etc/coaxhub/stationd.toml
check "reinstall (postinst configure with the old version)" \
    apt_quiet install --reinstall -o Dpkg::Options::=--force-confold "${deb}"
check_eq "reinstall keeps the credentials" marker "$(cat /var/lib/coaxhub/stationd/key.pem)"
check_eq "reinstall keeps the state directory's mode" "700 coaxhub coaxhub" \
    "$(mode_owner /var/lib/coaxhub/stationd)"
check_contains "reinstall keeps the owner's stationd.toml" "max_channels = 8" "$(cat /etc/coaxhub/stationd.toml)"

# ---------------------------------------------------------------- remove
check "apt-get remove" apt_quiet remove coaxhub-stationd
check "remove: stationd is gone" test ! -e /usr/bin/stationd
check "remove: /usr/lib/coaxhub is gone" test ! -e /usr/lib/coaxhub
check "remove: the units are gone" test ! -e /usr/lib/systemd/system/stationd.service
check "remove: stationd.toml stays" test -f /etc/coaxhub/stationd.toml
check_eq "remove: the credentials stay" marker "$(cat /var/lib/coaxhub/stationd/key.pem 2>&1)"
check "remove: the users stay" getent passwd coaxhub

# ---------------------------------------------------------------- purge
check "apt-get purge" apt_quiet purge coaxhub-stationd
check_eq "purge: dpkg forgets the package" "" "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>/dev/null)"
for path in /etc/coaxhub /var/lib/coaxhub /var/lib/coaxhub-radiod /usr/lib/coaxhub /usr/share/coaxhub \
    /usr/share/doc/coaxhub-stationd /run/coaxhub-radiod /usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules \
    /usr/lib/modprobe.d/coaxhub-stationd.conf; do
    check "purge: ${path} is gone" test ! -e "${path}"
done
for name in coaxhub coaxhub-radiod; do
    check_not "purge: user ${name} is gone" getent passwd "${name}"
    check_not "purge: group ${name} is gone" getent group "${name}"
done
check_eq "purge: no systemd link to our units is left" "" \
    "$(find /etc/systemd -name 'stationd.service' -o -name 'coaxhub-radiod.service' 2>/dev/null)"
check_eq "purge: no file mentions coaxhub under /etc" "" \
    "$(grep -rl coaxhub /etc 2>/dev/null | grep -v '^/etc/\(passwd\|group\|shadow\|gshadow\)-$')"
if [[ "${MODE}" == with-systemd ]]; then
    check_not "purge: systemd-time-wait-sync is disabled again" \
        systemctl is-enabled --quiet systemd-time-wait-sync.service
fi

done_testing
