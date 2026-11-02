#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Runs inside a container whose init is a real systemd (test-systemd.sh):
# installs the package and checks what only a running systemd shows: the
# helper's socket, stationd.service's sandbox asking the helper to start,
# restart and stop a receiver's unit, hostile requests, peers that are not
# coaxhub, a hostile receivers.json, doctor, remove and purge; or the upgrade
# from the published 0.2.3 package with its polkit rule.
#
# Usage: systemd.sh fresh|upgrade SUITE
#
# A stub radiod (`sleep infinity`) stands in for radiod through a drop-in of
# the receiver template: the point is that the unit starts, runs as
# coaxhub-radiod under the template's sandbox and stops. The template's
# ExecStartPre (the unprivileged renderer) is tested apart, against a hostile
# receivers.json.

# shellcheck source-path=SCRIPTDIR source=common.sh
. /test/common.sh
# SYSLIB (/usr/lib or /lib), SUFFIX, TARGET_NAME ... of the suite under test, from the host.
# shellcheck source=/dev/null
. /test/suite.env

readonly MODE="${1:?fresh|upgrade}" SUITE="${2:?suite}"
printf "# %s test on %s\n" "${MODE}" "${SUITE}"
readonly SOCK=/run/coaxhub-helper.sock
readonly SB_OUT=/var/lib/coaxhub/stationd/sandbox.out
readonly DROPIN_RADIOD=/run/systemd/system/coaxhub-radiod@.service.d/zz-test.conf
readonly DROPIN_STATIOND=/run/systemd/system/stationd.service.d/zz-test.conf

rm -f /usr/sbin/policy-rc.d
apt_quiet update || exit 1

# ---------------------------------------------------------------- helpers
# wait_for SECONDS COMMAND...: polls until COMMAND succeeds.
wait_for() {
    local limit=$(($1 * 5)) i
    shift
    for ((i = 0; i < limit; i++)); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.2
    done
    return 1
}
prop() { systemctl show -P "$2" "$1" 2>&1; }
is_active() { [[ "$(prop "$1" ActiveState)" == active ]]; }
is_inactive() { [[ "$(prop "$1" ActiveState)" == inactive ]]; }
is_failed() { [[ "$(prop "$1" ActiveState)" == failed ]]; }
# runs_stub UNIT: its main process has exec'd the stub. A Type=simple unit is
# active from the fork, while the child is still systemd's "(sleep)".
runs_stub() { [[ "$(ps -o comm= -p "$(prop "$1" MainPID)")" == sleep ]]; }
invocation() { prop "$1" InvocationID; }
# The receivers' units that exist, and their states.
radiod_units() { systemctl list-units 'coaxhub-radiod@*' --all --plain --no-legend 2>&1 | awk '{print $1, $3, $4}'; }

# ask USER LINE: sends the bytes of the printf format LINE to the helper as USER.
ask() {
    local user="$1" line="$2"
    # shellcheck disable=SC2059 # the request is a printf format on purpose
    printf "${line}" | runuser -u "${user}" -- socat -t 5 - "UNIX-CONNECT:${SOCK}" 2>&1
}

stub_radiod() {
    mkdir -p "$(dirname "${DROPIN_RADIOD}")"
    cat >"${DROPIN_RADIOD}" <<'EOF'
[Service]
ExecCondition=
ExecStartPre=
ExecStart=
ExecStart=/bin/sleep infinity
EOF
    systemctl daemon-reload
}

# The unit as shipped, for the renderer: its ExecCondition and the renderer as
# ExecStartPre stay; only `+ip link` (the container has no CAP_NET_ADMIN) and
# radiod (no SDR) are replaced. Restart=on-failure and RestartSec=5 are the
# unit's own: nothing here hides a restart loop.
real_renderer() {
    mkdir -p "$(dirname "${DROPIN_RADIOD}")"
    cat >"${DROPIN_RADIOD}" <<'EOF'
[Service]
ExecStartPre=
ExecStartPre=/usr/bin/stationd backend-config --receiver %i
ExecStart=
ExecStart=/bin/sleep infinity
EOF
    systemctl daemon-reload
}

# fake_enrollment: throwaway credentials, so that `stationd run` starts (there
# is no hub to reach; it starts its receivers first and retries the tunnel).
fake_enrollment() {
    local d=/var/lib/coaxhub/stationd uuid work
    work="$(mktemp -d)"
    uuid="$(cat /proc/sys/kernel/random/uuid)"
    (
        set -e
        cd "${work}"
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout ca.key -out ca.pem \
            -subj /CN=test-ca -days 2 -addext basicConstraints=critical,CA:TRUE \
            -addext keyUsage=critical,keyCertSign,cRLSign 2>/dev/null
        openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout key.pem -subj /CN=station \
            -out station.csr 2>/dev/null
        printf 'subjectAltName=URI:urn:coaxhub:station:%s\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n' \
            "${uuid}" >ext
        openssl x509 -req -in station.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 1 -out cert.pem \
            -extfile ext 2>/dev/null
        install -o coaxhub -g coaxhub -m 0600 key.pem "${d}/key.pem"
        install -o coaxhub -g coaxhub -m 0644 cert.pem "${d}/cert.pem"
        install -o coaxhub -g coaxhub -m 0644 /dev/null "${d}/chain.pem"
        install -o coaxhub -g coaxhub -m 0644 ca.pem "${d}/gateway-ca.pem"
        printf '{"hub_id":"sa1","gateway_url":"https://gw.sa1.coaxhub.test"}\n' >assignment.json
        install -o coaxhub -g coaxhub -m 0644 assignment.json "${d}/assignment.json"
        printf '%s\n' "${uuid}" >station-id
        install -o coaxhub -g coaxhub -m 0644 station-id "${d}/station-id"
    )
    check "throwaway credentials are in place (stationd status exits 0)" test "$(exit_code stationd status)" = 0
}

# put_receivers_json: stdin becomes receivers.json, written as stationd does.
put_receivers_json() {
    # shellcheck disable=SC2016 # expanded by the inner sh
    runuser -u coaxhub -- sh -c 'f=/var/lib/coaxhub/receivers/receivers.json &&
        cat >"$f.tmp" && chmod 0640 "$f.tmp" && mv "$f.tmp" "$f"'
}

# The container has no NTP: systemd-time-wait-sync, which stationd.service
# waits for, would never finish. It is made to finish at once; stationd.service
# itself is left as the package ships it.
fast_time_sync() {
    mkdir -p /run/systemd/system/systemd-time-wait-sync.service.d
    cat >/run/systemd/system/systemd-time-wait-sync.service.d/zz-test.conf <<'EOF'
[Service]
ExecStart=
ExecStart=/bin/true
EOF
    systemctl daemon-reload
}

# sandbox_run: runs the script on stdin inside stationd.service's own sandbox
# (user coaxhub, no capability, the unit's whole hardening, its Wants= of the
# helper's socket): only the unit's command is replaced for the run. Prints
# what the script wrote.
sandbox_run() {
    mkdir -p "$(dirname "${DROPIN_STATIOND}")"
    cat >"${DROPIN_STATIOND}" <<'EOF'
[Service]
ExecStart=
ExecStart=/bin/bash /test/sb.sh
Restart=no
EOF
    {
        cat <<EOF
exec >"${SB_OUT}.tmp" 2>&1
sock=${SOCK}
# ask NAME FORMAT: one request, whatever the helper answers.
ask() {
    echo "### \$1"
    # shellcheck disable=SC2059
    printf "\$2" | socat -t 5 - "UNIX-CONNECT:\$sock"
    echo "[exit \$?]"
}
EOF
        cat
        echo "mv ${SB_OUT}.tmp ${SB_OUT}"
    } >/test/sb.sh
    chmod 0755 /test/sb.sh
    rm -f "${SB_OUT}" "${SB_OUT}.tmp"
    systemctl daemon-reload
    systemctl start --wait stationd.service >/dev/null 2>&1 || true
    systemctl reset-failed stationd.service 2>/dev/null || true
    cat "${SB_OUT}" 2>/dev/null
}

# section NAME TEXT: the lines of TEXT after "### NAME" up to the next "###".
section() { awk -v n="### $1" '$0 == n { on = 1; next } /^### / { on = 0 } on' <<<"$2"; }

drop_test_files() {
    rm -f "${DROPIN_RADIOD}" "${DROPIN_STATIOND}"
    systemctl daemon-reload
}

# ---------------------------------------------------------------- fresh
fresh() {
    deb="$(ls /test/pkg/coaxhub-stationd_*_amd64.deb)"
    check_eq "the image has no polkit (the tests prove the package needs none)" "" \
        "$(dpkg-query -W -f='${Status}' polkitd policykit-1 2>/dev/null | grep 'ok installed')"
    check "systemd is the init" test "$(cat /proc/1/comm)" = systemd
    check "apt-get install resolves every dependency" apt_quiet install --no-install-recommends "${deb}"
    check_eq "installing the package pulled no polkit" "" \
        "$(dpkg-query -W -f='${Status}\n' polkitd policykit-1 2>/dev/null | grep 'ok installed')"
    check_eq "stationd.service is not enabled" disabled "$(systemctl is-enabled stationd.service 2>&1)"
    check_eq "the socket is not enabled either (nobody enables it)" "no-enable-link" \
        "$([[ -z "$(find /etc/systemd -name 'coaxhub-helper*' 2>/dev/null)" ]] && echo no-enable-link)"
    check_eq "the socket unit is loaded" loaded "$(prop coaxhub-helper.socket LoadState)"
    check_eq "the socket is not listening before stationd wants it" inactive "$(prop coaxhub-helper.socket ActiveState)"
    check_eq "the socket pulls in no stationd" inactive "$(prop stationd.service ActiveState)"
    check_contains "modprobe blacklists the DVB driver (the units' paths are the suite's)" \
        "blacklist dvb_usb_rtl28xxu" "$(modprobe --showconfig 2>&1)"

    for unit in stationd.service coaxhub-radiod@1.service coaxhub-helper.socket coaxhub-helper@0-1.service; do
        check "systemd-analyze verify ${unit}" systemd-analyze verify "${unit}"
    done
    for limit in "stationd.service 2.5" "coaxhub-radiod@1.service 4.5" "coaxhub-helper@0-1.service 3.0"; do
        read -r unit max <<<"${limit}"
        score="$(systemd-analyze security "${unit}" 2>&1 |
            sed -n 's/.*Overall exposure level for .*: \([0-9.]*\) .*/\1/p')"
        check "${unit} exposure ${score:-?} <= ${max}" \
            awk -v s="${score:-99}" -v m="${max}" 'BEGIN { exit !(s + 0 <= m + 0) }'
    done

    stub_radiod
    fast_time_sync
    coaxhub_uid="$(id -u coaxhub)"
    before="$(radiod_units)"

    # ------------------------------------------------ stationd's own sandbox
    # One run of stationd.service's sandbox does the valid requests; the helper's
    # socket is started because the unit wants it.
    out="$(
        sandbox_run <<'SB'
echo "### identity"
id -un
grep -E '^(NoNewPrivs|CapBnd|Seccomp):' /proc/self/status
echo "### socket-visible"
test -S /run/coaxhub-helper.sock && echo yes
echo "### direct-start-refused"
systemctl --no-ask-password --no-block start coaxhub-radiod@11.service; echo "[exit $?]"
echo "### state-readable"
systemctl show -P ActiveState coaxhub-radiod@11.service
ask ping 'ping\n'
ask start-11 'start 11\n'
SB
    )"
    check_eq "the sandbox ran as coaxhub" coaxhub "$(section identity "${out}" | head -n1)"
    check_contains "the sandbox: no new privileges" "NoNewPrivs:	1" "${out}"
    check_contains "the sandbox: no capability" "CapBnd:	0000000000000000" "${out}"
    check_contains "the sandbox: a seccomp filter" "Seccomp:	2" "${out}"
    check_eq "coaxhub-helper.socket was pulled in by stationd.service (Wants=)" active \
        "$(prop coaxhub-helper.socket ActiveState)"
    check_eq "the helper's socket is root:coaxhub 0660" "660 root coaxhub" "$(mode_owner "${SOCK}")"
    check_eq "the socket's trigger limit: 200 ..." 200 "$(prop coaxhub-helper.socket TriggerLimitBurst)"
    check_eq "the socket's trigger limit: ... in 2 s" 2s "$(prop coaxhub-helper.socket TriggerLimitIntervalUSec)"
    check_eq "the socket serves at most 4 connections at once" 4 "$(prop coaxhub-helper.socket MaxConnections)"
    check_eq "the sandbox sees the socket" yes "$(section socket-visible "${out}")"
    check_contains "without the helper, coaxhub cannot start a unit (no polkit, no rule)" \
        "Failed to start coaxhub-radiod@11.service: Access denied" "$(section direct-start-refused "${out}")"
    check_eq "the sandbox reads a unit's state without any authorization" inactive \
        "$(section state-readable "${out}" | head -n1)"
    check_eq "ping answered ok" $'ok\n[exit 0]' "$(section ping "${out}")"
    check_eq "start 11 answered ok" $'ok\n[exit 0]' "$(section start-11 "${out}")"
    check "receiver 11's unit is active" wait_for 10 is_active coaxhub-radiod@11.service
    check_eq "receiver 11 runs as coaxhub-radiod" coaxhub-radiod "$(prop coaxhub-radiod@11.service User)"
    check "receiver 11's main process is the stub" wait_for 10 runs_stub coaxhub-radiod@11.service
    inv1="$(invocation coaxhub-radiod@11.service)"
    check "receiver 11 has an invocation" test -n "${inv1}"

    out="$(
        sandbox_run <<'SB'
ask start-again 'start 11\n'
SB
    )"
    check_eq "start of a running unit answers ok" $'ok\n[exit 0]' "$(section start-again "${out}")"
    check_eq "start of a running unit leaves it alone" "${inv1}" "$(invocation coaxhub-radiod@11.service)"
    out="$(
        sandbox_run <<'SB'
ask restart-11 'restart 11\n'
ask start-12 'start 12\n'
SB
    )"
    check_eq "restart 11 answered ok" $'ok\n[exit 0]' "$(section restart-11 "${out}")"
    check "restart gives receiver 11 a new invocation" wait_for 10 bash -c \
        "[[ \"\$(systemctl show -P InvocationID coaxhub-radiod@11.service)\" != '${inv1}' && -n \"\$(systemctl show -P InvocationID coaxhub-radiod@11.service)\" ]]"
    check "receiver 11 is active after the restart" wait_for 10 is_active coaxhub-radiod@11.service
    check "receiver 12 is active" wait_for 10 is_active coaxhub-radiod@12.service
    out="$(
        sandbox_run <<'SB'
ask stop-11 'stop 11\n'
SB
    )"
    check_eq "stop 11 answered ok" $'ok\n[exit 0]' "$(section stop-11 "${out}")"
    check "stop makes receiver 11 inactive" wait_for 10 is_inactive coaxhub-radiod@11.service
    check "stop leaves receiver 12 alone" is_active coaxhub-radiod@12.service
    out="$(
        sandbox_run <<'SB'
ask stop-12 'stop 12\n'
ask stop-unknown 'stop 77\n'
SB
    )"
    check "receiver 12 is inactive" wait_for 10 is_inactive coaxhub-radiod@12.service
    # systemctl stop of a unit that never ran is fine; it is ok.
    check_eq "stop of a unit that never ran answers ok" $'ok\n[exit 0]' "$(section stop-unknown "${out}")"
    check "the helper's instances are gone (one per connection)" wait_for 20 test -z \
        "$(systemctl list-units 'coaxhub-helper@*' --all --plain --no-legend)"

    # ------------------------------------------------ hostile requests
    before="$(radiod_units)"
    out="$(
        sandbox_run <<'SB'
ask enable 'enable 11\n'
ask zero 'start 0\n'
ask leading-zero 'start 011\n'
ask two-ids 'start 11 12\n'
ask unit-name 'start coaxhub-radiod@11\n'
ask too-big 'start 4294967296\n'
ask negative 'start -1\n'
ask nul 'start 1\0\n'
ask non-utf8 'start \xff\xfe\n'
ask crlf 'start 1\r\n'
ask upper 'START 1\n'
ask daemon-reload 'daemon-reload\n'
ask ping-id 'ping 1\n'
echo "### one-mib"
head -c 1048576 /dev/zero | tr '\0' a | socat -t 5 - "UNIX-CONNECT:$sock"; echo "[exit $?]"
echo "### no-newline"
(printf 'start 1'; sleep 4) | socat -t 6 - "UNIX-CONNECT:$sock"; echo "[exit $?]"
SB
    )"
    for name in enable zero leading-zero two-ids unit-name too-big negative nul non-utf8 crlf upper \
        daemon-reload ping-id one-mib no-newline; do
        got="$(section "${name}" "${out}" | grep -v '^\[exit')"
        # A refusal says "bad request"; a 1 MiB sender may be cut off before it reads it.
        if [[ "${name}" == one-mib ]]; then
            # The sender is cut off while it still writes: socat and tr say so; the
            # helper answers a bad request, or the connection is reset first.
            if [[ "${got}" != ok ]]; then ok "hostile: ${name} refused"; else not_ok "hostile: ${name} refused" "${got}"; fi
        else
            check_eq "hostile: ${name} is refused as a bad request" "error bad request" "${got}"
        fi
    done
    check_eq "no receiver unit changed (nothing was started, stopped or enabled)" "${before}" "$(radiod_units)"
    check_eq "nothing was enabled" "" "$(find /etc/systemd/system -name 'coaxhub-radiod@*' 2>/dev/null)"
    check_contains "the journal names the helper's instances by the peer's UID" \
        "-${coaxhub_uid}.service" "$(journalctl -u 'coaxhub-helper@*' -o verbose --no-pager 2>&1)"
    check_contains "the journal records the valid request" "start 11: ok" \
        "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"
    check_contains "the journal records a refusal without its bytes" \
        "refused: bad request (unknown verb)" "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"
    check_lacks "the journal never holds a request's bytes" "daemon-reload" \
        "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"

    # ------------------------------------------------ stationd, by itself
    # The real `stationd run` (enrolled with throwaway credentials, no hub to
    # reach) starts the applied receivers at boot, before any tunnel, whose
    # SDRs it sees in a fake sysfs: tmpfs over /sys/bus/usb/devices, which
    # CAP_SYS_ADMIN allows. The requests come from stationd's own process.
    fake_enrollment
    mount -t tmpfs tmpfs /sys/bus/usb/devices
    for entry in "1-1.3 A0000031" "1-1.4 A0000032"; do
        read -r port serial <<<"${entry}"
        mkdir -p "/sys/bus/usb/devices/${port}"
        printf '0bda\n' >"/sys/bus/usb/devices/${port}/idVendor"
        printf '2838\n' >"/sys/bus/usb/devices/${port}/idProduct"
        printf '%s\n' "${serial}" >"/sys/bus/usb/devices/${port}/serial"
        printf 'RTL2838UHIDIR\n' >"/sys/bus/usb/devices/${port}/product"
        printf 'Realtek\n' >"/sys/bus/usb/devices/${port}/manufacturer"
    done
    put_receivers_json <<'JSON'
{"receivers": [
  {"receiver_id": 31, "slot": 1, "sdr_serial": "A0000031", "model": "rtlsdr_other",
   "profile": "rtlsdr", "center_frequency_hz": 98000000},
  {"receiver_id": 32, "slot": 2, "sdr_serial": "A0000032", "model": "rtlsdr_other",
   "profile": "rtlsdr", "center_frequency_hz": 98000000}
]}
JSON
    real_renderer
    systemctl stop coaxhub-helper.socket
    check "the helper's socket is stopped before stationd starts" is_inactive coaxhub-helper.socket
    cat >"${DROPIN_STATIOND}" <<'EOF'
[Service]
Restart=no
EOF
    systemctl daemon-reload
    check "systemctl start stationd.service (the unit as shipped)" systemctl start stationd.service
    check_eq "stationd.service wants the helper's socket: it is up with stationd" active \
        "$(prop coaxhub-helper.socket ActiveState)"
    check "stationd started receiver 31 through the helper" wait_for 20 is_active coaxhub-radiod@31.service
    check "stationd started receiver 32 through the helper" wait_for 20 is_active coaxhub-radiod@32.service
    check_eq "stationd runs as coaxhub" coaxhub "$(prop stationd.service User)"
    check_contains "the journal: the helper started 31 for stationd's user" "start 31: ok" \
        "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"
    check_contains "the journal: the helper started 32 for stationd's user" "start 32: ok" \
        "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"
    check_contains "the journal: those requests came from coaxhub's UID" "peer uid ${coaxhub_uid}: start 31: ok" \
        "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"
    check_contains "stationd logged that it started the applied receivers" "starting the applied receivers" \
        "$(journalctl -u stationd.service -o cat --no-pager 2>&1)"
    check_contains "doctor sees both receivers" "receiver 31" "$(stationd doctor 2>&1)"
    check_contains "doctor: the helper answers while stationd runs" "pass  helper" "$(stationd doctor 2>&1)"
    systemctl stop stationd.service
    check "stationd stopped" is_inactive stationd.service
    check "stopping stationd leaves the receivers running (the next start finds them)" \
        is_active coaxhub-radiod@31.service
    systemctl stop coaxhub-radiod@31.service coaxhub-radiod@32.service
    systemctl reset-failed 2>/dev/null
    umount /sys/bus/usb/devices
    stub_radiod
    rm -f "${DROPIN_STATIOND}"
    systemctl daemon-reload
    systemctl start coaxhub-helper.socket

    # ------------------------------------------------ who may connect
    check "a user outside the coaxhub group cannot connect" bash -c \
        "! printf 'ping\n' | runuser -u nobody -- socat -t 3 - UNIX-CONNECT:${SOCK} 2>&1 | grep -qx ok"
    check_contains "nobody gets 'Permission denied' at connect" "Permission denied" \
        "$(ask nobody 'ping\n')"
    useradd --system --no-create-home --shell /usr/sbin/nologin --gid nogroup --groups coaxhub intruder
    check_eq "a user in the coaxhub group with another UID is refused by SO_PEERCRED" \
        "error not permitted" "$(ask intruder 'start 11\n')"
    check_eq "that user's start changed nothing" "${before}" "$(radiod_units)"
    check_eq "root may ping" ok "$(printf 'ping\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}")"
    check_eq "coaxhub may ping" ok "$(ask coaxhub 'ping\n')"
    userdel intruder 2>/dev/null

    # ------------------------------------------------ doctor
    doctor="$(stationd doctor 2>&1)"
    check_contains "doctor: the helper answers" "pass  helper" "${doctor}"
    check_contains "doctor as nobody: warns, with the fix" "warn  helper" \
        "$(runuser -u nobody -- stationd doctor 2>&1)"
    systemctl stop coaxhub-helper.socket
    doctor="$(stationd doctor 2>&1)"
    check_contains "doctor: no socket fails" "FAIL  helper" "${doctor}"
    check_contains "doctor: names the cause" "cannot reach coaxhub-helper (${SOCK})" "${doctor}"
    check_contains "doctor: names the socket unit's state" "coaxhub-helper.socket is inactive" "${doctor}"
    check_contains "doctor: names the fix" "sudo systemctl restart stationd" "${doctor}"
    systemctl start coaxhub-helper.socket
    check_contains "doctor passes again" "pass  helper" "$(stationd doctor 2>&1)"

    # ------------------------------------------------ the number of receiver units
    # A start or restart needs room: fewer than 16 other receiver units that
    # are running or starting. Systemd is asked; no file of stationd's is read.
    # A failed unit runs nothing and is not counted (stop does not clear it).
    mkdir -p "$(dirname "${DROPIN_RADIOD}")"
    cat >"${DROPIN_RADIOD}" <<'EOF'
[Service]
ExecCondition=
ExecStartPre=
ExecStart=
ExecStart=/bin/false
Restart=no
EOF
    systemctl daemon-reload
    printf 'start 118\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}" >/dev/null
    check "a unit whose radiod fails is failed" wait_for 10 is_failed coaxhub-radiod@118.service
    printf 'stop 118\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}" >/dev/null
    sleep 1
    check_eq "stop does not clear a failed unit" failed "$(prop coaxhub-radiod@118.service ActiveState)"
    stub_radiod
    for n in $(seq 101 116); do
        printf 'start %s\n' "${n}" | socat -t 5 - "UNIX-CONNECT:${SOCK}" >/dev/null
    done
    check "sixteen receiver units run (the failed one did not count against them)" wait_for 20 bash -c \
        "[[ \$(systemctl list-units 'coaxhub-radiod@1*' --state=active --plain --no-legend | grep -vc '@118') -ge 16 ]]"
    check_eq "the failed unit is still failed" failed "$(prop coaxhub-radiod@118.service ActiveState)"
    answer="$(printf 'start 117\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}")"
    check_eq "a seventeenth unit is refused, with the reason" \
        "error too many receivers: 16 other coaxhub-radiod units are running (at most 16)" "${answer}"
    check_eq "the refused unit did not start" inactive "$(prop coaxhub-radiod@117.service ActiveState)"
    answer="$(printf 'restart 117\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}")"
    check_contains "restart of a new id is refused too" "error too many receivers" "${answer}"
    inv101="$(invocation coaxhub-radiod@101.service)"
    check_eq "restart of one of the sixteen is fine (the others are fifteen)" ok \
        "$(printf 'restart 101\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}")"
    check "that restart gave it a new invocation" wait_for 10 bash -c \
        "[[ \"\$(systemctl show -P InvocationID coaxhub-radiod@101.service)\" != '${inv101}' ]]"
    check_eq "stop is never refused" ok "$(printf 'stop 116\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}")"
    check "receiver 116 is inactive" wait_for 10 is_inactive coaxhub-radiod@116.service
    check_eq "with room again, a new unit starts" ok "$(printf 'start 117\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}")"
    check "receiver 117 is active" wait_for 10 is_active coaxhub-radiod@117.service
    check_contains "the journal records the refusal by its numbers" \
        "start 117: refused, 16 other receiver units are running" \
        "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"
    for n in $(seq 101 118); do
        printf 'stop %s\n' "${n}" | socat -t 5 - "UNIX-CONNECT:${SOCK}" >/dev/null
    done
    check "the sixteen are stopped again" wait_for 20 test -z \
        "$(systemctl list-units 'coaxhub-radiod@*' --state=active,activating --plain --no-legend)"
    systemctl reset-failed 2>/dev/null
    stub_radiod

    # ------------------------------------------------ hostile receivers.json
    # Root reads nothing of it: the unit's ExecCondition, the unprivileged
    # renderer run as coaxhub-radiod, refuses it, and the unit does not start
    # and is not retried (Restart=on-failure, RestartSec=5, as shipped).
    real_renderer
    check_eq "the shipped restart policy is on" on-failure "$(prop coaxhub-radiod@1.service Restart)"
    check_eq "the shipped restart delay is 5 s" 5s "$(prop coaxhub-radiod@1.service RestartUSec)"
    # ID: how many times the unit was skipped by its ExecCondition, from the journal.
    skipped() { journalctl -u "coaxhub-radiod@$1.service" -o cat --no-pager 2>&1 | grep -c "Skipped due to 'exec-condition'"; }
    skipped_once() { [[ "$(skipped "$1")" == 1 ]]; }
    wrote_config() { journalctl -u "coaxhub-radiod@$1.service" -o cat --no-pager 2>&1 | grep -q "radiod config written"; }
    put_receivers_json <<'JSON'
{"receivers": [{"receiver_id": 21, "slot": 2, "sdr_serial": "A0000021", "model": "rtlsdr_other",
  "profile": "rtlsdr", "center_frequency_hz": 98000000}]}
JSON
    printf 'start 21\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}" >/dev/null
    check "a valid receiver runs (its condition passed)" wait_for 10 is_active coaxhub-radiod@21.service
    check "a valid receiver's renderer wrote its config" wait_for 10 wrote_config 21
    check_eq "a valid receiver was not skipped" 0 "$(skipped 21)"
    check_eq "the renderer ran as coaxhub-radiod (its unit's user)" coaxhub-radiod "$(prop coaxhub-radiod@21.service User)"
    check_eq "the check writes nothing" "" "$(stationd backend-config --receiver 21 --check 2>&1)"
    systemctl stop coaxhub-radiod@21.service 2>/dev/null
    systemctl reset-failed 2>/dev/null
    hostile_ids=()
    hostile() { # WHAT ID JSON
        printf '%s\n' "$3" | put_receivers_json
        printf 'start %s\n' "$2" | socat -t 5 - "UNIX-CONNECT:${SOCK}" >/dev/null
        check "hostile receivers.json (${1}): the condition skips the unit" wait_for 10 skipped_once "$2"
        check_eq "hostile receivers.json (${1}): the unit did not start" inactive \
            "$(prop "coaxhub-radiod@$2.service" ActiveState)"
        check "hostile receivers.json (${1}): the renderer proper never wrote a config" \
            bash -c "! journalctl -u coaxhub-radiod@$2.service -o cat --no-pager | grep -q 'radiod config written'"
        hostile_ids+=("$2")
    }
    hostile "an id the file lacks" 99 '{"receivers": [{"receiver_id": 21, "slot": 2, "sdr_serial": "A", "model": "rtlsdr_other", "profile": "rtlsdr", "center_frequency_hz": 98000000}]}'
    hostile "a key outside the schema" 22 '{"receivers": [{"receiver_id": 22, "slot": 2, "sdr_serial": "A", "model": "rtlsdr_other", "profile": "rtlsdr", "center_frequency_hz": 98000000, "config": "hardware = x"}]}'
    hostile "the development siggen profile" 23 '{"receivers": [{"receiver_id": 23, "slot": 2, "sdr_serial": "", "model": "rtlsdr_other", "profile": "siggen", "center_frequency_hz": 98000000}]}'
    hostile "not JSON" 24 'not json'
    # Two restart delays later, nothing was retried: skipped once, no restart.
    sleep 12
    for id in "${hostile_ids[@]}"; do
        check_eq "refused receiver ${id}: not retried (skipped once)" 1 "$(skipped "${id}")"
        check_eq "refused receiver ${id}: no restart" 0 "$(prop "coaxhub-radiod@${id}.service" NRestarts)"
        check_eq "refused receiver ${id}: still inactive" inactive "$(prop "coaxhub-radiod@${id}.service" ActiveState)"
    done
    check_eq "refused receivers are not counted as running (they are inactive)" "" \
        "$(systemctl list-units 'coaxhub-radiod@*' --state=active,activating,deactivating,failed --plain --no-legend)"
    stub_radiod

    # ------------------------------------------------ remove and purge
    systemctl start coaxhub-helper.socket
    printf 'start 11\n' | socat -t 5 - "UNIX-CONNECT:${SOCK}" >/dev/null
    check "receiver 11 runs before the removal" wait_for 10 is_active coaxhub-radiod@11.service
    drop_test_files
    check "apt-get remove" apt_quiet remove coaxhub-stationd
    check_eq "remove: the socket is stopped" inactive "$(prop coaxhub-helper.socket ActiveState)"
    check "remove: the socket file is gone" test ! -e "${SOCK}"
    check "remove: the receivers are stopped" test -z \
        "$(systemctl list-units 'coaxhub-radiod@*' --state=active,activating --plain --no-legend)"
    check "remove: the helper is gone" test ! -e /usr/lib/coaxhub/coaxhub-helper
    check_eq "remove: the socket unit is not found" not-found "$(prop coaxhub-helper.socket LoadState)"
    check "apt-get purge" apt_quiet purge coaxhub-stationd
    check "purge: no helper unit file is left" test -z "$(find "${SYSLIB}"/systemd /etc/systemd -name 'coaxhub-helper*' 2>/dev/null)"
    check "purge: no socket file is left" test ! -e "${SOCK}"
    check "purge: no link to our units is left" test -z \
        "$(find /etc/systemd -name 'stationd.service' -o -name 'coaxhub-*' 2>/dev/null)"
    check_not "purge: user coaxhub is gone" getent passwd coaxhub
}

# ---------------------------------------------------------------- upgrade
upgrade() {
    old="$(ls /test/previous/coaxhub-stationd_*_amd64.deb)"
    new="$(ls /test/pkg/coaxhub-stationd_*_amd64.deb)"
    check "install the published 0.2.3 package (it brings polkit)" apt_quiet install --no-install-recommends "${old}"
    check_eq "0.2.3 is installed" "0.2.3-1~deb12" "$(dpkg-query -W -f='${Version}' coaxhub-stationd)"
    check "0.2.3 shipped the polkit rule" test -f /usr/share/polkit-1/rules.d/coaxhub-stationd.rules
    check_eq "polkit came with 0.2.3" "install ok installed" "$(dpkg-query -W -f='${Status}' polkitd 2>&1)"
    stub_radiod
    # stationd.service stands in as running (a sleep), as on a station that is enrolled.
    mkdir -p "$(dirname "${DROPIN_STATIOND}")"
    cat >"${DROPIN_STATIOND}" <<'EOF'
[Service]
ExecStart=
ExecStart=/bin/sleep infinity
Restart=no
EOF
    fast_time_sync
    check "stationd.service is running (the stand-in)" bash -c 'systemctl start stationd.service && systemctl is-active --quiet stationd.service'
    # What 0.2.3 did: coaxhub starts a unit through polkit's rule, with no helper.
    check "0.2.3: coaxhub starts receiver 12 through the polkit rule" \
        runuser -u coaxhub -- systemctl --no-ask-password --no-block start coaxhub-radiod@12.service
    check "receiver 12 is active" wait_for 15 is_active coaxhub-radiod@12.service
    check "receiver 11 (started by root) is active" bash -c 'systemctl start coaxhub-radiod@11.service && sleep 1'
    check "receiver 11 is active" wait_for 15 is_active coaxhub-radiod@11.service
    inv11="$(invocation coaxhub-radiod@11.service)"
    inv12="$(invocation coaxhub-radiod@12.service)"
    inv_stationd="$(invocation stationd.service)"

    # --reinstall: a package built from this tree has the published version's
    # number until a release bumps it, and apt would call that "already the
    # newest"; dpkg still runs the new postinst with the old version as $2, as
    # for any upgrade.
    check "apt-get install the new package over it" apt_quiet install --reinstall --no-install-recommends "${new}"
    check "the new package's files are in: the helper" test -x /usr/lib/coaxhub/coaxhub-helper
    check "the old polkit rule file is gone" test ! -e /usr/share/polkit-1/rules.d/coaxhub-stationd.rules
    check_eq "polkit itself stays until autoremove" "install ok installed" "$(dpkg-query -W -f='${Status}' polkitd 2>&1)"
    check_eq "the helper's socket is active (stationd was running)" active "$(prop coaxhub-helper.socket ActiveState)"
    check_eq "the helper's socket is root:coaxhub 0660" "660 root coaxhub" "$(mode_owner "${SOCK}")"
    check "stationd was restarted" test "$(invocation stationd.service)" != "${inv_stationd}"
    check "receiver 11 was restarted (once)" wait_for 15 bash -c \
        "[[ \"\$(systemctl show -P InvocationID coaxhub-radiod@11.service)\" != '${inv11}' ]] && systemctl is-active --quiet coaxhub-radiod@11.service"
    check "receiver 12 was restarted (once)" wait_for 15 bash -c \
        "[[ \"\$(systemctl show -P InvocationID coaxhub-radiod@12.service)\" != '${inv12}' ]] && systemctl is-active --quiet coaxhub-radiod@12.service"
    after11="$(invocation coaxhub-radiod@11.service)"
    after12="$(invocation coaxhub-radiod@12.service)"
    sleep 3
    check_eq "receiver 11 was not restarted again" "${after11}" "$(invocation coaxhub-radiod@11.service)"
    check_eq "receiver 12 was not restarted again" "${after12}" "$(invocation coaxhub-radiod@12.service)"
    check_eq "without the rule, coaxhub can no longer start a unit by itself" "refused" \
        "$(runuser -u coaxhub -- systemctl --no-ask-password --no-block start coaxhub-radiod@14.service >/dev/null 2>&1 && echo started || echo refused)"
    check_eq "receiver 14 did not start" inactive "$(prop coaxhub-radiod@14.service ActiveState)"
    check_eq "the helper starts another receiver for coaxhub" ok "$(ask coaxhub 'start 13\n')"
    check "receiver 13 is active" wait_for 15 is_active coaxhub-radiod@13.service
    check_eq "the helper restarts receiver 12 for coaxhub" ok "$(ask coaxhub 'restart 12\n')"
    check "receiver 12 has a new invocation" wait_for 15 bash -c \
        "[[ \"\$(systemctl show -P InvocationID coaxhub-radiod@12.service)\" != '${after12}' ]]"
    check_eq "the helper stops receiver 13 for coaxhub" ok "$(ask coaxhub 'stop 13\n')"
    check "receiver 13 is inactive" wait_for 15 is_inactive coaxhub-radiod@13.service
    # An upgrade from a version that already has the helper restarts its socket,
    # so that a changed socket unit applies at once.
    ts_before="$(prop coaxhub-helper.socket ActiveEnterTimestampMonotonic)"
    sleep 1
    check "apt-get install the package again (an upgrade from a version with the helper)" \
        apt_quiet install --reinstall --no-install-recommends "${new}"
    check_eq "the socket is active after it" active "$(prop coaxhub-helper.socket ActiveState)"
    check "the socket was restarted by the upgrade" test \
        "$(prop coaxhub-helper.socket ActiveEnterTimestampMonotonic)" != "${ts_before}"
    check_eq "the socket is still root:coaxhub 0660" "660 root coaxhub" "$(mode_owner "${SOCK}")"
    check_eq "the helper answers after it" ok "$(ask coaxhub 'ping\n')"
    drop_test_files
    check "apt-get purge" apt_quiet purge coaxhub-stationd
    check "purge: no helper file, unit or socket is left" test -z \
        "$(find "${SYSLIB}"/systemd /etc/systemd /run -name 'coaxhub-helper*' 2>/dev/null)"
}

# ---------------------------------------------------------------- owner
# The jammy owner: 0.2.3 for bookworm on Ubuntu 22.04, as an altered installer
# left it; then the published installer against a repository with the jammy
# package. Also a bookworm system with the same installer is covered by fresh.
owner() {
    old="$(ls /test/previous/coaxhub-stationd_*_amd64.deb)"
    readonly URL="http://127.0.0.1:28081" REPO=/srv/repo
    check_eq "this is the jammy case" jammy "${CODENAME}"
    # The altered installer had to force it: 0.2.3 for bookworm needs libfftw3-single3 >= 3.3.10
    # and jammy has 3.3.8 (the same soname, so radiod runs). Its other dependencies, polkit
    # among them, are installed first.
    apt_quiet install --no-install-recommends dbus polkitd libbsd0 libfftw3-single3 libiniparser1 \
        libopus0 libusb-1.0-0 adduser iproute2 || exit 1
    # The tools of the test (and curl, which the owner's machine had): apt is broken
    # once the package is forced in, until the jammy package replaces it.
    apt_quiet install --no-install-recommends gpg gpg-agent apt-utils python3 curl ca-certificates || exit 1
    check "install the published 0.2.3 package for bookworm, forcing its dependencies as the altered installer did" \
        dpkg --force-depends -i "${old}"
    check_eq "0.2.3 is installed" "0.2.3-1~deb12" "$(dpkg-query -W -f='${Version}' coaxhub-stationd)"
    check_eq "polkit is the jammy one (0.105: .pkla, not .rules)" "0.105" \
        "$(dpkg-query -W -f='${Version}' polkitd | cut -d- -f1)"
    stub_radiod
    # What the owner saw: no receiver can start.
    fail="$(runuser -u coaxhub -- systemctl --no-ask-password --no-block start coaxhub-radiod@12.service 2>&1)"
    check_contains "the reported bug: the rule is ignored, \"Interactive authentication required\"" \
        "Interactive authentication required" "${fail}"
    check_eq "and the receiver did not start" inactive "$(prop coaxhub-radiod@12.service ActiveState)"
    systemctl stop coaxhub-radiod@12.service 2>/dev/null

    # A repository with the jammy package, signed with a key of the test, and the
    # installer filled in as apt-repo.sh does; a station that is enrolled (throwaway
    # credentials) with two receivers whose SDRs (a fake sysfs) are plugged in.
    export GNUPGHOME=/root/repo-gnupg
    mkdir -p "${GNUPGHOME}" && chmod 0700 "${GNUPGHOME}"
    gpg --batch --quiet --passphrase '' --quick-gen-key 'CoaxHub test archive <archive@coaxhub.test>' ed25519 sign never
    fpr="$(gpg --batch --with-colons --list-keys | awk -F: '$1 == "fpr" { print $10; exit }')"
    mkdir -p "${REPO}/pool/main"
    cp /test/pkg/*.deb "${REPO}/pool/main/"
    for arch in amd64 arm64; do
        mkdir -p "${REPO}/dists/${CODENAME}/main/binary-${arch}"
        (cd "${REPO}" && apt-ftparchive --arch "${arch}" packages pool/main) >"${REPO}/dists/${CODENAME}/main/binary-${arch}/Packages"
        gzip -9kn "${REPO}/dists/${CODENAME}/main/binary-${arch}/Packages"
    done
    apt-ftparchive -o "APT::FTPArchive::Release::Suite=${CODENAME}" -o "APT::FTPArchive::Release::Codename=${CODENAME}" \
        -o "APT::FTPArchive::Release::Components=main" -o "APT::FTPArchive::Release::Architectures=amd64 arm64" \
        release "${REPO}/dists/${CODENAME}" >"${REPO}/dists/${CODENAME}/Release"
    gpg --batch --yes --quiet -u "${fpr}" --clearsign -o "${REPO}/dists/${CODENAME}/InRelease" "${REPO}/dists/${CODENAME}/Release"
    gpg --batch --yes --quiet -u "${fpr}" -abs -o "${REPO}/dists/${CODENAME}/Release.gpg" "${REPO}/dists/${CODENAME}/Release"
    gpg --batch --armor --export "${fpr}" >"${REPO}/coaxhub-archive-keyring.asc"
    (cd "${REPO}" && exec python3 -m http.server 28081 --bind 127.0.0.1 >/tmp/http.log 2>&1) &
    for _ in $(seq 50); do
        python3 -c "import urllib.request; urllib.request.urlopen('${URL}/dists/${CODENAME}/Release')" 2>/dev/null && break
        sleep 0.1
    done
    sed -e "s|@COAXHUB_APT_URL@|${URL}|g" -e "s|@COAXHUB_KEY_FINGERPRINT@|${fpr}|g" /test/station.sh >/tmp/station.sh

    fake_enrollment
    mount -t tmpfs tmpfs /sys/bus/usb/devices
    for entry in "1-1.3 A0000031" "1-1.4 A0000032"; do
        read -r port serial <<<"${entry}"
        mkdir -p "/sys/bus/usb/devices/${port}"
        printf '0bda\n' >"/sys/bus/usb/devices/${port}/idVendor"
        printf '2838\n' >"/sys/bus/usb/devices/${port}/idProduct"
        printf '%s\n' "${serial}" >"/sys/bus/usb/devices/${port}/serial"
        printf 'RTL2838UHIDIR\n' >"/sys/bus/usb/devices/${port}/product"
    done
    put_receivers_json <<'JSON'
{"receivers": [
  {"receiver_id": 31, "slot": 1, "sdr_serial": "A0000031", "model": "rtlsdr_other",
   "profile": "rtlsdr", "center_frequency_hz": 98000000},
  {"receiver_id": 32, "slot": 2, "sdr_serial": "A0000032", "model": "rtlsdr_other",
   "profile": "rtlsdr", "center_frequency_hz": 98000000}
]}
JSON
    fast_time_sync
    # The old stationd as the owner ran it: it asks polkit and every receiver fails.
    mkdir -p "$(dirname "${DROPIN_STATIOND}")"
    printf '[Service]\nRestart=no\n' >"${DROPIN_STATIOND}"
    real_renderer
    systemctl daemon-reload
    systemctl start stationd.service
    check "the old stationd cannot start the receivers (no receiver unit runs)" bash -c \
        "sleep 6; test -z \"\$(systemctl list-units 'coaxhub-radiod@*' --state=active,activating --plain --no-legend)\""
    check_contains "its journal says why" "Interactive authentication required" \
        "$(journalctl -u stationd.service -o cat --no-pager 2>&1)"

    # The published installer, once the repository has the jammy package.
    rc=0
    sh /tmp/station.sh -y --skip-time-sync </dev/null >/tmp/installer.log 2>&1 || rc=$?
    check_eq "the installer succeeds on jammy" 0 "${rc}"
    check_contains "it writes the jammy suite" "Suites: jammy" "$(cat /etc/apt/sources.list.d/coaxhub.sources)"
    check_contains "the package is now the jammy one" "~ubuntu22.04" "$(dpkg-query -W -f='${Version}' coaxhub-stationd)"
    check_eq "the polkit rule file is gone" "" "$(ls /usr/share/polkit-1/rules.d/coaxhub-stationd.rules 2>/dev/null)"
    check_eq "the helper's socket is active" active "$(prop coaxhub-helper.socket ActiveState)"
    check "the receivers start" wait_for 30 is_active coaxhub-radiod@31.service
    check "both receivers start" wait_for 30 is_active coaxhub-radiod@32.service
    check_contains "the helper started them for stationd's user" "start 31: ok" \
        "$(journalctl -u 'coaxhub-helper@*' -o cat --no-pager 2>&1)"
    check_contains "the DVB blacklist is where jammy's kmod reads it" "blacklist dvb_usb_rtl28xxu" "$(modprobe --showconfig 2>&1)"
    systemctl stop stationd.service coaxhub-radiod@31.service coaxhub-radiod@32.service
    umount /sys/bus/usb/devices
    drop_test_files
}

case "${MODE}" in
fresh) fresh ;;
upgrade) upgrade ;;
owner) owner ;;
*) echo "unknown mode ${MODE}" >&2; exit 2 ;;
esac
done_testing
