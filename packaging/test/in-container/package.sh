#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Runs inside a clean debian:<suite> amd64 container: installs the package
# from /test/pkg/*.deb, checks what docs/packaging.md says it contains and
# that stationd renders a receiver's radiod config from receivers.json, then
# removes and purges it and checks that nothing is left.
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
# stationd starts the receivers' units over the system bus, which polkit authorizes.
check "polkit is installed with it (Depends: polkitd | policykit-1)" \
    bash -c "dpkg-query -W -f='\${Status}' polkitd policykit-1 2>/dev/null | grep -q 'ok installed'"

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
check_eq "/var/lib/coaxhub/receivers (set-group-ID)" "2750 coaxhub coaxhub-radiod" "$(mode_owner /var/lib/coaxhub/receivers)"
check_eq "/var/lib/coaxhub/receivers is empty on a new station" "" "$(ls -A /var/lib/coaxhub/receivers)"
check_eq "/var/lib/coaxhub-radiod" "755 coaxhub-radiod coaxhub-radiod" "$(mode_owner /var/lib/coaxhub-radiod)"

# Files and modes.
for f in "755 /usr/bin/stationd" "755 /usr/lib/coaxhub/radiod/radiod" \
    "644 /usr/lib/coaxhub/radiod/rtlsdr.so" "644 /usr/lib/coaxhub/radiod/librtlsdr.so.0" \
    "644 /usr/lib/coaxhub/radiod/presets.conf" \
    "644 /usr/share/coaxhub/radiod-templates/rtlsdr.conf" \
    "644 /usr/share/coaxhub/radiod-templates/rtlsdr-hf.conf" \
    "644 /usr/share/coaxhub/radiod-templates/rtlsdr-v4.conf" \
    "644 /usr/lib/systemd/system/stationd.service" \
    "644 /usr/lib/systemd/system/coaxhub-radiod@.service" \
    "644 /usr/share/polkit-1/rules.d/coaxhub-stationd.rules" \
    "644 /usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules" \
    "644 /usr/lib/modprobe.d/coaxhub-stationd.conf" \
    "644 /usr/share/doc/coaxhub-stationd/copyright"; do
    check_eq "${f#* } (mode ${f%% *}, root)" "${f%% *} root root" "$(mode_owner "${f#* }")"
done
check "no development-only template (siggen)" test ! -e /usr/share/coaxhub/radiod-templates/siggen.conf
check_not "radiod is not on PATH" command -v radiod
check "no coaxhub-radiod.service (only the template)" test ! -e /usr/lib/systemd/system/coaxhub-radiod.service
check_eq "the package has no conffile" "" "$(dpkg-query -W -f='${Conffiles}' coaxhub-stationd)"
check "the package ships nothing under /etc/coaxhub" test ! -e /etc/coaxhub
check_contains "the blacklist names dvb_usb_rtl28xxu" "blacklist dvb_usb_rtl28xxu" \
    "$(cat /usr/lib/modprobe.d/coaxhub-stationd.conf)"
copyright="$(cat /usr/share/doc/coaxhub-stationd/copyright)"
check_contains "copyright names the ka9q-radio commit" "${KA9Q_COMMIT}" "${copyright}"
check_contains "copyright names the source tarball" "ka9q-radio-${KA9Q_COMMIT:0:8}.tar.gz" "${copyright}"
check_contains "copyright names radiod's patches" "  ka9q-radio-${KA9Q_COMMIT:0:8}-0001-" "${copyright}"
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
# The packaged build reads no configuration file.
check_eq "the packaged build has no check-config" 2 "$(exit_code stationd check-config)"
check_eq "the packaged build takes no --config" 2 "$(exit_code stationd --config /nonexistent status)"
doctor="$(stationd doctor 2>&1)"
check_contains "doctor: not enrolled" "not enrolled" "${doctor}"
check_contains "doctor: no SDR here" "no RTL-SDR on USB (looked for 0bda:2838, 0bda:2832)" "${doctor}"
check_eq "stationd status exits 3 when not enrolled" 3 "$(exit_code stationd status)"
check_eq "stationd run exits 3 when not enrolled (as coaxhub)" 3 \
    "$(exit_code runuser -u coaxhub -- stationd run)"

# ---------------------------------------------------------------- polkit
# stationd (user coaxhub) starts, restarts and stops coaxhub-radiod@<digits>.service
# over D-Bus, and nothing else. The rule is JavaScript run by polkitd, so it
# is checked as text here (the container has no system bus).
rule="$(cat /usr/share/polkit-1/rules.d/coaxhub-stationd.rules)"
rule_code="$(grep -v '^[[:space:]]*//' <<<"${rule}")"
check_eq "polkit rule: one rule" 1 "$(grep -c 'polkit.addRule' <<<"${rule_code}")"
check_contains "polkit rule: the user coaxhub" 'subject.user == "coaxhub"' "${rule_code}"
check_contains "polkit rule: the manage-units action" 'action.id == "org.freedesktop.systemd1.manage-units"' "${rule_code}"
check_contains "polkit rule: the unit pattern" '/^coaxhub-radiod@[0-9]+\.service$/.test(action.lookup("unit"))' "${rule_code}"
check_contains "polkit rule: exactly the three verbs" '["start", "stop", "restart"].indexOf(action.lookup("verb")) >= 0' "${rule_code}"
check_eq "polkit rule: allows (YES) once" 1 "$(grep -c 'polkit.Result.YES' <<<"${rule_code}")"
check_eq "polkit rule: no other result" 0 "$(grep -c 'polkit.Result.\(AUTH\|NO\|NOT_HANDLED\)\|return true' <<<"${rule_code}")"
for other in reload kill enable disable "reload-or-restart" "try-restart" "reset-failed"; do
    check_lacks "polkit rule: not the verb ${other}" "\"${other}\"" "${rule_code}"
done

# ---------------------------------------------------------------- backend-config
# The applied receiver set, as stationd writes it after ConfigureReceivers
# (station protocol §7): three receivers, one per profile, in slots that are
# not 0 (so the groups cannot be 239.77.0.x by accident).
# put_receivers_json: stdin becomes receivers.json, written as stationd writes
# it: by the user coaxhub, who can't chown, mode 0640; the group comes from
# the set-group-ID directory.
put_receivers_json() {
    # shellcheck disable=SC2016 # expanded by the inner sh
    runuser -u coaxhub -- sh -c 'f=/var/lib/coaxhub/receivers/receivers.json &&
        cat >"$f.tmp" && chmod 0640 "$f.tmp" && mv "$f.tmp" "$f"'
}
write_receivers_json() {
    put_receivers_json <<'JSON'
{
  "receivers": [
    {"receiver_id": 11, "slot": 3, "sdr_serial": "A0000011", "model": "rtlsdr_other",
     "profile": "rtlsdr", "center_frequency_hz": 98000000},
    {"receiver_id": 12, "slot": 7, "sdr_serial": "A0000012", "model": "rtlsdr_direct_sampling",
     "profile": "rtlsdr_hf", "center_frequency_hz": 7100000},
    {"receiver_id": 13, "slot": 12, "sdr_serial": "A0000013", "model": "rtlsdr_blog_v4",
     "profile": "rtlsdr_v4", "center_frequency_hz": 14200000}
  ]
}
JSON
}
write_receivers_json
check_eq "receivers.json written by coaxhub takes the directory's group" "640 coaxhub coaxhub-radiod" \
    "$(mode_owner /var/lib/coaxhub/receivers/receivers.json)"
check "coaxhub-radiod reads receivers.json" \
    runuser -u coaxhub-radiod -- cat /var/lib/coaxhub/receivers/receivers.json

# render ID: stationd's rendering of that receiver's radiod config, to stdout.
render() { stationd backend-config --receiver "$1" --out - 2>&1; }
for entry in "11 3 A0000011" "12 7 A0000012" "13 12 A0000013"; do
    read -r id slot serial <<<"${entry}"
    rendered="$(render "${id}")"
    check_contains "receiver ${id}: hardware" "hardware = rtlsdr" "${rendered}"
    check_contains "receiver ${id}: its SDR's serial" "serial = ${serial}" "${rendered}"
    check_contains "receiver ${id}: the status group of slot ${slot}" "status = 239.77.${slot}.6" "${rendered}"
    check_contains "receiver ${id}: the data group of slot ${slot}" "data = 239.77.${slot}.4" "${rendered}"
    check_lacks "receiver ${id}: no other slot's groups" "239.77.0." "${rendered}"
    check_contains "receiver ${id}: ttl 0" "ttl = 0" "${rendered}"
done
rendered="$(render 11)"
check_contains "rtlsdr: its template" "/usr/share/coaxhub/radiod-templates/rtlsdr.conf" "${rendered}"
check_contains "rtlsdr: the centre" "frequency = 98000000" "${rendered}"
rendered="$(render 12)"
check_contains "rtlsdr-hf: direct sampling" "direct_sampling = 2" "${rendered}"
check_contains "rtlsdr-hf: the centre" "frequency = 7100000" "${rendered}"
rendered="$(render 13)"
check_contains "rtlsdr-v4: its template" "/usr/share/coaxhub/radiod-templates/rtlsdr-v4.conf" "${rendered}"
check_lacks "rtlsdr-v4: no direct sampling" "direct_sampling =" "${rendered}"
check_contains "rtlsdr-v4: the tuner's gain" "gain = 30" "${rendered}"
check_contains "rtlsdr-v4: the centre" "frequency = 14200000" "${rendered}"
check_contains "rtlsdr-v4: its description" 'description = "RTL-SDR Blog V4"' "${rendered}"
check_eq "a receiver that is not in receivers.json is refused (exit 2)" 2 \
    "$(exit_code stationd backend-config --receiver 99 --out -)"
check_eq "receiver 0 is refused (exit 2)" 2 "$(exit_code stationd backend-config --receiver 0 --out -)"
# A packaged build refuses the development-only siggen profile.
put_receivers_json <<'JSON'
{"receivers": [{"receiver_id": 14, "slot": 1, "sdr_serial": "", "model": "rtlsdr_other",
  "profile": "siggen", "center_frequency_hz": 98000000}]}
JSON
check_eq "a packaged build refuses the siggen profile (exit 2)" 2 \
    "$(exit_code stationd backend-config --receiver 14 --out -)"
# stationd's schema: an unknown key is an error.
put_receivers_json <<'JSON'
{"receivers": [{"receiver_id": 15, "slot": 2, "sdr_serial": "A0000015", "model": "rtlsdr_other",
  "profile": "rtlsdr", "center_frequency_hz": 98000000, "config": "hardware = x"}]}
JSON
check_eq "receivers.json with an unknown key is refused (exit 2)" 2 \
    "$(exit_code stationd backend-config --receiver 15 --out -)"
# The optional front-end settings.
put_receivers_json <<'JSON'
{"receivers": [{"receiver_id": 16, "slot": 4, "sdr_serial": "A0000016", "model": "rtlsdr_blog_v4",
  "profile": "rtlsdr_v4", "center_frequency_hz": 145000000, "gain_db": 20.7}]}
JSON
check_contains "a receiver's gain replaces the template's" "gain = 20.7" "$(render 16)"
write_receivers_json
# As coaxhub-radiod@ID.service runs it: its user, no --out, into its runtime
# directory (which systemd creates: RuntimeDirectory=coaxhub-radiod/%i).
install -d -o coaxhub-radiod -g coaxhub-radiod /run/coaxhub-radiod /run/coaxhub-radiod/11
check "backend-config as coaxhub-radiod into /run/coaxhub-radiod/11" \
    runuser -u coaxhub-radiod -- stationd backend-config --receiver 11
check_eq "radiod.conf mode" "644 coaxhub-radiod coaxhub-radiod" "$(mode_owner /run/coaxhub-radiod/11/radiod.conf)"
check_contains "radiod.conf is receiver 11's" "status = 239.77.3.6" "$(cat /run/coaxhub-radiod/11/radiod.conf)"

# ---------------------------------------------------------------- radiod
out="$(/usr/lib/coaxhub/radiod/radiod -V 2>&1)"
check_contains "radiod -V reports the pinned commit" "${KA9Q_COMMIT}" "${out}"
check_contains "radiod -V names our patches" "(coaxhub-stationd, patches 0001" "${out}"
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
check "set the loopback MULTICAST flag (as coaxhub-radiod@.service does)" ip link set dev lo multicast on
out="$(cd / && timeout 20 runuser -u coaxhub-radiod -- /usr/lib/coaxhub/radiod/radiod \
    /run/coaxhub-radiod/11/radiod.conf 2>&1)"
check_contains "radiod loads presets from /usr/lib/coaxhub/radiod" \
    "/usr/lib/coaxhub/radiod/presets.conf" "${out}"
check_lacks "radiod finds the rtlsdr driver" "dlopen" "${out}"
check_contains "radiod reaches the device probe (no SDR here)" "rtlsdr" "$(tr '[:upper:]' '[:lower:]' <<<"${out}")"
printf '# radiod without an SDR said:\n%s\n' "$(tail -n 5 <<<"${out}" | sed 's/^/#   /')"

# ---------------------------------------------------------------- systemd
if [[ "${MODE}" == with-systemd ]]; then
    # The template unit is verified through an instance of it.
    for unit in stationd.service coaxhub-radiod@1.service; do
        check "systemd-analyze verify ${unit}" systemd-analyze verify "${unit}"
    done
    # systemd's own exposure score (0 best, 10 worst); both are "OK" today.
    for limit in "stationd.service 2.5" "coaxhub-radiod@1.service 4.5"; do
        read -r unit max <<<"${limit}"
        score="$(systemd-analyze security --offline=true "${unit}" 2>&1 |
            sed -n 's/.*Overall exposure level for .*: \([0-9.]*\) .*/\1/p')"
        check "${unit} exposure ${score:-?} <= ${max} (systemd-analyze security)" \
            awk -v s="${score:-99}" -v m="${max}" 'BEGIN { exit !(s + 0 <= m + 0) }'
    done
    check "postinst enabled systemd-time-wait-sync" \
        systemctl is-enabled --quiet systemd-time-wait-sync.service
    unit="$(cat /usr/lib/systemd/system/stationd.service)"
    check_contains "stationd.service: RestartPreventExitStatus=3" "RestartPreventExitStatus=3" "${unit}"
    check_contains "stationd.service: after time-sync.target" "time-sync.target" "${unit}"
    check_lacks "stationd.service: does not depend on radiod" "coaxhub-radiod" "$(grep -v '^#' <<<"${unit}")"
    check_contains "stationd.service: AF_UNIX stays for the system bus" \
        "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK" "${unit}"
    unit="$(cat /usr/lib/systemd/system/coaxhub-radiod@.service)"
    check_contains "coaxhub-radiod@.service: a runtime directory per receiver" \
        "RuntimeDirectory=coaxhub-radiod/%i" "${unit}"
    check_contains "coaxhub-radiod@.service: renders the receiver's config first" \
        "ExecStartPre=/usr/bin/stationd backend-config --receiver %i" "${unit}"
    check_contains "coaxhub-radiod@.service: starts radiod on that config" \
        "ExecStart=/usr/lib/coaxhub/radiod/radiod /run/coaxhub-radiod/%i/radiod.conf" "${unit}"
    check_contains "coaxhub-radiod@.service: the privileged multicast ExecStartPre" \
        "ExecStartPre=+ip link set dev lo multicast on" "${unit}"
    check_lacks "coaxhub-radiod@.service: no [Install] (stationd starts the instances)" "[Install]" "${unit}"
    if udevadm verify --help >/dev/null 2>&1; then
        check "udevadm verify the rule" udevadm verify /usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules
    fi
    # As the installer does, so purge has something to clean up. Only
    # stationd is enabled.
    check "systemctl enable stationd.service (offline)" systemctl enable stationd.service
fi

# ---------------------------------------------------------------- reinstall
# A fake enrollment must survive a reinstall and a remove.
echo "marker" >/var/lib/coaxhub/stationd/key.pem
chown coaxhub:coaxhub /var/lib/coaxhub/stationd/key.pem
receivers_before="$(cat /var/lib/coaxhub/receivers/receivers.json)"
check "reinstall (postinst configure with the old version)" apt_quiet install --reinstall "${deb}"
check_eq "reinstall keeps the credentials" marker "$(cat /var/lib/coaxhub/stationd/key.pem)"
check_eq "reinstall keeps the state directory's mode" "700 coaxhub coaxhub" \
    "$(mode_owner /var/lib/coaxhub/stationd)"
check_eq "reinstall keeps receivers.json" "${receivers_before}" "$(cat /var/lib/coaxhub/receivers/receivers.json)"
check_eq "reinstall keeps receivers.json's mode" "640 coaxhub coaxhub-radiod" \
    "$(mode_owner /var/lib/coaxhub/receivers/receivers.json)"
check_eq "reinstall keeps the receivers directory's mode" "2750 coaxhub coaxhub-radiod" \
    "$(mode_owner /var/lib/coaxhub/receivers)"

# ---------------------------------------------------------------- remove
check "apt-get remove" apt_quiet remove coaxhub-stationd
check "remove: stationd is gone" test ! -e /usr/bin/stationd
check "remove: /usr/lib/coaxhub is gone" test ! -e /usr/lib/coaxhub
check "remove: the units are gone" test ! -e /usr/lib/systemd/system/stationd.service
check "remove: the polkit rule is gone" test ! -e /usr/share/polkit-1/rules.d/coaxhub-stationd.rules
check_eq "remove: the credentials stay" marker "$(cat /var/lib/coaxhub/stationd/key.pem 2>&1)"
check_eq "remove: the receiver set stays" "${receivers_before}" "$(cat /var/lib/coaxhub/receivers/receivers.json 2>&1)"
check "remove: the users stay" getent passwd coaxhub

# ---------------------------------------------------------------- purge
check "apt-get purge" apt_quiet purge coaxhub-stationd
check_eq "purge: dpkg forgets the package" "" "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>/dev/null)"
for path in /etc/coaxhub /var/lib/coaxhub /var/lib/coaxhub/receivers /var/lib/coaxhub-radiod \
    /usr/lib/coaxhub /usr/share/coaxhub /usr/share/doc/coaxhub-stationd /run/coaxhub-radiod \
    /usr/lib/udev/rules.d/72-coaxhub-rtlsdr.rules /usr/lib/modprobe.d/coaxhub-stationd.conf \
    /usr/share/polkit-1/rules.d/coaxhub-stationd.rules; do
    check "purge: ${path} is gone" test ! -e "${path}"
done
for name in coaxhub coaxhub-radiod; do
    check_not "purge: user ${name} is gone" getent passwd "${name}"
    check_not "purge: group ${name} is gone" getent group "${name}"
done
check_eq "purge: no systemd link to our units is left" "" \
    "$(find /etc/systemd -name 'stationd.service' -o -name 'coaxhub-radiod@*' 2>/dev/null)"
check_eq "purge: no file mentions coaxhub under /etc" "" \
    "$(grep -rl coaxhub /etc 2>/dev/null | grep -v '^/etc/\(passwd\|group\|shadow\|gshadow\)-$')"
if [[ "${MODE}" == with-systemd ]]; then
    check_not "purge: systemd-time-wait-sync is disabled again" \
        systemctl is-enabled --quiet systemd-time-wait-sync.service
fi

done_testing
