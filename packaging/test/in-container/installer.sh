#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Runs inside a clean debian:<suite> amd64 container: builds a throwaway
# signed APT repository from /test/pkg/*.deb (a key generated here), serves
# it on 127.0.0.1:28080 inside the container, and runs the installer
# (/test/station.sh, the unfilled template) against it (contract A).
#
# There is no terminal (docker exec without -t) and no control plane, so the
# happy path ends at enrollment, which must fail cleanly.
#
# Usage: installer.sh SUITE

# shellcheck source-path=SCRIPTDIR source=common.sh
. /test/common.sh

readonly SUITE="${1:?suite}"
readonly PORT=28080
readonly URL="http://127.0.0.1:${PORT}"
readonly REPO=/srv/repo
readonly TEMPLATE=/test/station.sh
readonly KEYRING=/usr/share/keyrings/coaxhub-archive-keyring.gpg
readonly SOURCES=/etc/apt/sources.list.d/coaxhub.sources
readonly CONFIG=/etc/coaxhub/stationd.toml
# A well-formed token (chs_enr_ + 32 Crockford base32 characters).
readonly TOKEN_BODY=0123456789ABCDEFGHJKMNPQRSTVWXYZ
export GNUPGHOME=/root/repo-gnupg

# ---------------------------------------------------------------- setup
# No curl or wget: the installer must fetch its own prerequisites.
apt_quiet update && apt_quiet install --no-install-recommends gpg gpg-agent apt-utils python3 openssl ||
    exit 1

mkdir -p "${GNUPGHOME}" && chmod 0700 "${GNUPGHOME}"
gen_key() {
    gpg --batch --quiet --passphrase '' --quick-gen-key "$1" ed25519 sign never
    gpg --batch --with-colons --list-keys "$1" | awk -F: '$1 == "fpr" { print $10; exit }'
}
FPR="$(gen_key 'CoaxHub test archive <archive@coaxhub.test>')"
OTHER_FPR="$(gen_key 'Someone else <other@example.invalid>')"
[[ ${#FPR} -eq 40 && ${#OTHER_FPR} -eq 40 ]] || { echo "key generation failed"; exit 1; }

mkdir -p "${REPO}/pool/main"
cp /test/pkg/*.deb "${REPO}/pool/main/"
for arch in amd64 arm64; do
    mkdir -p "${REPO}/dists/${SUITE}/main/binary-${arch}"
    (cd "${REPO}" && apt-ftparchive --arch "${arch}" packages pool/main) \
        >"${REPO}/dists/${SUITE}/main/binary-${arch}/Packages"
    gzip -9kn "${REPO}/dists/${SUITE}/main/binary-${arch}/Packages"
done
apt-ftparchive -o "APT::FTPArchive::Release::Suite=${SUITE}" \
    -o "APT::FTPArchive::Release::Codename=${SUITE}" \
    -o "APT::FTPArchive::Release::Components=main" \
    -o "APT::FTPArchive::Release::Architectures=amd64 arm64" \
    release "${REPO}/dists/${SUITE}" >"${REPO}/dists/${SUITE}/Release"
gpg --batch --yes --quiet -u "${FPR}" --clearsign -o "${REPO}/dists/${SUITE}/InRelease" "${REPO}/dists/${SUITE}/Release"
gpg --batch --yes --quiet -u "${FPR}" -abs -o "${REPO}/dists/${SUITE}/Release.gpg" "${REPO}/dists/${SUITE}/Release"
gpg --batch --armor --export "${FPR}" >"${REPO}/coaxhub-archive-keyring.asc"
cp "${REPO}/coaxhub-archive-keyring.asc" /root/good-key.asc

(cd "${REPO}" && exec python3 -m http.server "${PORT}" --bind 127.0.0.1 >/tmp/http.log 2>&1) &
for _ in $(seq 50); do
    python3 -c "import urllib.request; urllib.request.urlopen('${URL}/dists/${SUITE}/Release')" 2>/dev/null && break
    sleep 0.1
done

# installer FINGERPRINT: the installer as apt-repo.sh publishes it.
installer() {
    sed -e "s|@COAXHUB_APT_URL@|${URL}|g" -e "s|@COAXHUB_KEY_FINGERPRINT@|$1|g" "${TEMPLATE}" >"/tmp/station-$1.sh"
    printf '/tmp/station-%s.sh' "$1"
}

# run LOG ARGS...: runs `sh ARGS...` with stdin closed, like `curl | sh`
# after the script was read; stdout+stderr to LOG; prints the exit code.
run() {
    local log="$1" rc=0
    shift
    sh "$@" </dev/null >"${log}" 2>&1 || rc=$?
    printf '%s' "${rc}"
}

nothing_written() {
    check "$1: no keyring" test ! -e "${KEYRING}"
    check "$1: no sources file" test ! -e "${SOURCES}"
    check_eq "$1: the package is not installed" "" \
        "$(dpkg-query -W -f='${Status}' coaxhub-stationd 2>/dev/null | grep 'ok installed')"
}

printf 'chs_enr_%s\n' "${TOKEN_BODY}" >/root/token
chmod 0600 /root/token
# A fake RTL-SDR for detection.
mkdir -p /tmp/usb/1-1
printf '0bda\n' >/tmp/usb/1-1/idVendor
printf '2838\n' >/tmp/usb/1-1/idProduct
printf 'RTL2838UHIDIR\n' >/tmp/usb/1-1/product
mkdir -p /tmp/nousb
good="$(installer "${FPR}")"
common=(--skip-time-sync --no-start)

# ---------------------------------------------------------------- refusals
rc="$(run /tmp/t.log "${TEMPLATE}" -y "${common[@]}")"
check_eq "template: refuses to run" 1 "${rc}"
check_contains "template: says why" "installer template" "$(cat /tmp/t.log)"
nothing_written "template"

rc="$(run /tmp/t.log "${good}" --token "chs_enr_${TOKEN_BODY}" "${common[@]}")"
check_eq "--token argument: refused" 1 "${rc}"
check_lacks "--token argument: the token is not echoed" "${TOKEN_BODY}" "$(cat /tmp/t.log)"

rc=0
runuser -u nobody -- sh "${good}" "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
check_eq "not root: refused" 1 "${rc}"
check_contains "not root: says why" "as root" "$(cat /tmp/t.log)"

rc="$(run /tmp/t.log "${good}" --profile airspy "${common[@]}")"
check_eq "unknown profile: refused" 1 "${rc}"

# Other systems: /etc/os-release is swapped for a while.
cp -P /etc/os-release /root/os-release.orig
for os in "debian bullseye" "ubuntu noble" "fedora 40"; do
    read -r os_id os_version <<<"${os}"
    rm -f /etc/os-release
    case "${os_id}" in
    debian) printf 'ID=debian\nVERSION_CODENAME=%s\nPRETTY_NAME="Debian (%s)"\n' "${os_version}" "${os_version}" ;;
    ubuntu) printf 'ID=ubuntu\nID_LIKE=debian\nVERSION_CODENAME=%s\n' "${os_version}" ;;
    fedora) printf 'ID=fedora\nVERSION_ID=%s\nPRETTY_NAME="Fedora %s"\n' "${os_version}" "${os_version}" ;;
    esac >/etc/os-release
    rc="$(run /tmp/t.log "${good}" -y "${common[@]}")"
    check_eq "${os}: refused" 1 "${rc}"
    check_contains "${os}: says it is unsupported" "support" "$(cat /tmp/t.log)"
done
rm -f /etc/os-release
cp -P /root/os-release.orig /etc/os-release
nothing_written "unsupported systems"

mkdir -p /tmp/shim
for fake in armhf i386; do
    # shellcheck disable=SC2016 # the shim's own "$1" and "$@"
    printf '#!/bin/sh\n[ "$1" = --print-architecture ] && { echo %s; exit 0; }\nexec /usr/bin/dpkg "$@"\n' \
        "${fake}" >/tmp/shim/dpkg
    chmod +x /tmp/shim/dpkg
    rc=0
    PATH="/tmp/shim:${PATH}" sh "${good}" -y "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
    check_eq "${fake}: refused" 1 "${rc}"
    check_contains "${fake}: names the architecture" "${fake}" "$(cat /tmp/t.log)"
done
rm -f /tmp/shim/dpkg

rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/nousb sh "${good}" "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
check_eq "no SDR, no terminal, no --yes: refused" 1 "${rc}"
check_contains "no SDR: says so" "No RTL-SDR found" "$(cat /tmp/t.log)"
nothing_written "no SDR"

# A wrong fingerprint, and a key file with a second primary key added.
wrong="$(printf '%s' "${FPR}" | tr '0-9A-F' '1-9A-F0')"
rc="$(run /tmp/t.log "$(installer "${wrong}")" -y --token-file /root/token "${common[@]}")"
check_eq "wrong fingerprint: refused" 1 "${rc}"
check_contains "wrong fingerprint: says so" "fingerprint" "$(cat /tmp/t.log)"
nothing_written "wrong fingerprint"
gpg --batch --armor --export "${FPR}" "${OTHER_FPR}" >"${REPO}/coaxhub-archive-keyring.asc"
rc="$(run /tmp/t.log "${good}" -y --token-file /root/token "${common[@]}")"
check_eq "an extra key in the key file: refused" 1 "${rc}"
nothing_written "extra key"
cp /root/good-key.asc "${REPO}/coaxhub-archive-keyring.asc"

# ---------------------------------------------------------------- happy path
# Piped into sh, as `curl -fsSL …/station | sudo sh -s -- …` runs it.
rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh -s -- --profile rtlsdr-hf --token-file /root/token \
    "${common[@]}" <"${good}" >/tmp/install.log 2>&1 || rc=$?
log="$(cat /tmp/install.log)"
printf '# installer output:\n%s\n' "$(sed 's/^/#   /' /tmp/install.log)"
check_eq "happy path: exits 1 (no control plane to enroll with)" 1 "${rc}"
check_contains "happy path: detects the SDR" "Found an RTL-SDR (0bda:2838, RTL2838UHIDIR)" "${log}"
check_contains "happy path: reports the failed enrollment" "enrollment failed" "${log}"
check_contains "happy path: stationd says why" "cannot reach the enrollment service at https://enroll." "${log}"
check_lacks "happy path: the token is not in the output" "${TOKEN_BODY}" "${log}"
check_eq "keyring mode" "644 root root" "$(mode_owner "${KEYRING}")"
check_contains "the keyring holds the pinned key" "${FPR}" \
    "$(gpg --batch --show-keys --with-colons "${KEYRING}" 2>&1)"
sources="$(cat "${SOURCES}")"
check_contains "sources: deb822 with signed-by" "Signed-By: ${KEYRING}" "${sources}"
check_contains "sources: the suite" "Suites: ${SUITE}" "${sources}"
check_contains "sources: the URL" "URIs: ${URL}" "${sources}"
check_eq "the package is installed" "install ok installed" \
    "$(dpkg-query -W -f='${Status}' coaxhub-stationd)"
check_eq "the profile is written once" 'profile = "rtlsdr-hf"' "$(grep '^profile' "${CONFIG}")"
check_contains "the rest of stationd.toml is kept" "max_waterfall_subscriptions = 4" "$(cat "${CONFIG}")"
check "stationd.toml stays valid" stationd check-config
check_eq "not enrolled after the failure" 3 "$(exit_code stationd status)"
check_eq "the credentials directory is untouched" "" "$(ls -A /var/lib/coaxhub/stationd)"

# ---------------------------------------------------------------- on a terminal
# As a person runs it: questions and the token prompt on the terminal.
interactive() { python3 /test/interactive.py "$@"; }
rc=0
interactive /tmp/tty.log "Choose [1]:" 2 \
    "Enrollment token (input hidden):" "chs_enr_BADBAD" \
    "Enrollment token (input hidden):" "chs_enr_${TOKEN_BODY}" \
    -- sh -c "cat '${good}' | COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh -s -- ${common[*]}" || rc=$?
tty_log="$(tr -d '\r' </tmp/tty.log)"
printf '# terminal:\n%s\n' "$(grep -v '^\(Get\|Hit\|Reading\|Building\|Selecting\|Preparing\|Unpacking\|Setting\)' \
    <<<"${tty_log}" | sed 's/^/#   /')"
check_eq "terminal: exits 1 (no control plane)" 1 "${rc}"
check_contains "terminal: asks for the profile" "HF by direct sampling" "${tty_log}"
check_eq "terminal: answer 2 is rtlsdr-hf" 'profile = "rtlsdr-hf"' "$(grep '^profile' "${CONFIG}")"
check_contains "terminal: a refused token asks again" "Try again (2 of 3)" "${tty_log}"
check_contains "terminal: then reports the failed enrollment" "enrollment failed" "${tty_log}"
check_lacks "terminal: the refused token is not echoed" "BADBAD" "${tty_log}"
check_lacks "terminal: the token is not echoed" "${TOKEN_BODY}" "${tty_log}"

rc=0
interactive /tmp/tty.log "Enrollment token (input hidden):" x1 \
    "Enrollment token (input hidden):" x2 "Enrollment token (input hidden):" x3 \
    -- env COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" --profile rtlsdr "${common[@]}" || rc=$?
check_eq "terminal: three refused tokens: exits 1" 1 "${rc}"
check_contains "terminal: three refused tokens: says so" "not accepted 3 times" "$(cat /tmp/tty.log)"

rc=0
interactive /tmp/tty.log "Continue anyway? [y/N]" n \
    -- env COAXHUB_INSTALL_USB_DEVICES=/tmp/nousb sh "${good}" "${common[@]}" || rc=$?
check_eq "terminal: no SDR, answer no: exits 1" 1 "${rc}"
check_contains "terminal: no SDR, answer no: stops" "stopped" "$(cat /tmp/tty.log)"

# ---------------------------------------------------------------- again
sum_before="$(md5sum "${KEYRING}" "${SOURCES}")"
rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" --profile rtlsdr --token-file /root/token \
    "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
check_eq "re-run: same result" 1 "${rc}"
check_eq "re-run: keyring and sources unchanged" "${sum_before}" "$(md5sum "${KEYRING}" "${SOURCES}")"
check_eq "re-run: one sources file" 1 "$(grep -rl "${URL}" /etc/apt/sources.list.d | wc -l)"
check_eq "re-run: the new profile, once" 'profile = "rtlsdr"' "$(grep '^profile' "${CONFIG}")"

printf 'chs_enr_TOOSHORT\n' >/root/bad-token
rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" --profile rtlsdr --token-file /root/bad-token \
    "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
check_eq "malformed token: exits 1" 1 "${rc}"
check_contains "malformed token: says the token was refused" "was not accepted" "$(cat /tmp/t.log)"
check_lacks "malformed token: not echoed" "TOOSHORT" "$(cat /tmp/t.log)"

# ---------------------------------------------------------------- enrolled
# Fake credentials, so `stationd status` reports the station as enrolled.
(
    set -e
    d=/var/lib/coaxhub/stationd
    cd "$(mktemp -d)"
    uuid="$(cat /proc/sys/kernel/random/uuid)"
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
check_eq "fake enrollment: stationd status exits 0" 0 "$(exit_code stationd status)"
key_before="$(md5sum /var/lib/coaxhub/stationd/key.pem)"

rc=0
PATH="/tmp/shim:${PATH}" COAXHUB_INSTALL_USB_DEVICES=/tmp/nousb sh "${good}" -y --no-start </dev/null \
    >/tmp/t.log 2>&1 || rc=$?
check_eq "enrolled, no timedatectl, no --skip-time-sync: refused" 1 "${rc}"
check_contains "no timedatectl: says so" "timedatectl" "$(cat /tmp/t.log)"

printf '#!/bin/sh\n[ "$*" = "show -p NTPSynchronized --value" ] && echo yes\n' >/tmp/shim/timedatectl
chmod +x /tmp/shim/timedatectl
rc=0
PATH="/tmp/shim:${PATH}" COAXHUB_INSTALL_USB_DEVICES=/tmp/nousb sh "${good}" -y --no-start </dev/null \
    >/tmp/t.log 2>&1 || rc=$?
log="$(cat /tmp/t.log)"
check_eq "enrolled re-run: exits 0" 0 "${rc}"
check_contains "enrolled re-run: waits for NTP" "The clock is synchronised" "${log}"
check_contains "enrolled re-run: skips enrollment" "already enrolled" "${log}"
check_contains "enrolled re-run: --yes takes the default profile" "Using the rtlsdr profile" "${log}"
check_contains "enrolled re-run: prints stationd status" "enrolled" "${log}"
check_eq "enrolled re-run: the key is untouched" "${key_before}" "$(md5sum /var/lib/coaxhub/stationd/key.pem)"

rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" --profile rtlsdr --reenroll --token-file /root/token \
    "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
check_eq "--reenroll: enrolls again, and fails without a control plane" 1 "${rc}"
check_contains "--reenroll: reports it" "enrollment failed" "$(cat /tmp/t.log)"
check_eq "--reenroll: a failed enrollment keeps the old credentials" "${key_before}" \
    "$(md5sum /var/lib/coaxhub/stationd/key.pem)"

done_testing
