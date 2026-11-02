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
# SYSLIB (/usr/lib or /lib), SUFFIX, TARGET_NAME ... of the suite under test, from the host.
# shellcheck source=/dev/null
. /test/suite.env

readonly SUITE="${1:?suite}"
readonly PORT=28080
readonly URL="http://127.0.0.1:${PORT}"
readonly REPO=/srv/repo
readonly TEMPLATE=/test/station.sh
readonly KEYRING=/usr/share/keyrings/coaxhub-archive-keyring.gpg
readonly SOURCES=/etc/apt/sources.list.d/coaxhub.sources
readonly RECEIVERS=/var/lib/coaxhub/receivers
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
# A fake RTL-SDR Blog V4: the same IDs, its own USB strings.
mkdir -p /tmp/usbv4/1-1.3
printf '0bda\n' >/tmp/usbv4/1-1.3/idVendor
printf '2838\n' >/tmp/usbv4/1-1.3/idProduct
printf 'Blog V4\n' >/tmp/usbv4/1-1.3/product
printf 'RTLSDRBlog\n' >/tmp/usbv4/1-1.3/manufacturer
mkdir -p /tmp/nousb
good="$(installer "${FPR}")"
common=(--skip-time-sync --no-start)

# ---------------------------------------------------------------- no control plane
# The package has its domain built in (a throwaway one in CI, the production
# one in a release), and the installer's enrollment calls https://enroll.<domain>.
# Whatever the domain, this test must never reach a real service: the names
# stationd and the installer use are pointed at the container's own loopback,
# where nothing listens on port 443, so every connection is refused at once.
# The domain is read from the package itself (Maintainer: CoaxHub <packages@DOMAIN>).
maintainer="$(dpkg-deb -f /test/pkg/*_amd64.deb Maintainer)"
DOMAIN="${maintainer##*packages@}"
DOMAIN="${DOMAIN%>}"
if [[ ! "${DOMAIN}" =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$ ]]; then
    echo "cannot read the package's domain from its Maintainer field: ${maintainer}"
    exit 1
fi
{
    for sub in enroll station-api get apt api app admin www; do
        printf '127.0.0.1 %s.%s\n::1 %s.%s\n' "${sub}" "${DOMAIN}" "${sub}" "${DOMAIN}"
    done
    printf '127.0.0.1 %s\n::1 %s\n' "${DOMAIN}" "${DOMAIN}"
} >>/etc/hosts
# unreachable HOST: resolves to loopback only, and a connection to port 443 is
# refused (or fails some other way within 5 s); prints what it saw.
unreachable() {
    python3 - "$1" <<'PY'
import socket, sys
host = sys.argv[1]
addrs = {a[4][0] for a in socket.getaddrinfo(host, 443, proto=socket.IPPROTO_TCP)}
if not addrs or not addrs <= {"127.0.0.1", "::1"}:
    print(f"{host} resolves to {sorted(addrs)}")
    sys.exit(1)
try:
    socket.create_connection((host, 443), timeout=5).close()
except OSError as e:
    print(f"{host} -> {sorted(addrs)}: {e.__class__.__name__}")
    sys.exit(0)
print(f"{host} accepted a connection")
sys.exit(1)
PY
}
check "the enrollment endpoint (enroll.${DOMAIN}) is unreachable from this test" unreachable "enroll.${DOMAIN}"
check "the station API (station-api.${DOMAIN}) is unreachable from this test" unreachable "station-api.${DOMAIN}"

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

# There is no --profile: the owner sets up receivers in the web app.
for profile in rtlsdr rtlsdr-hf rtlsdr-v4 airspy; do
    rc="$(run /tmp/t.log "${good}" --profile "${profile}" "${common[@]}")"
    check_eq "--profile ${profile}: refused" 1 "${rc}"
    check_contains "--profile ${profile}: an unknown option" "unknown option: --profile" "$(cat /tmp/t.log)"
done
nothing_written "--profile"

# Other systems: /etc/os-release is swapped for a while. Each refusal names the
# system (PRETTY_NAME) and lists what is supported, and writes nothing.
cp -P /etc/os-release /root/os-release.orig
supported_line="CoaxHub stations run on ${SUPPORTED_TEXT}"
fake_os() { # NAME ID CODENAME PRETTY [ID_LIKE [UBUNTU_CODENAME]]
    rm -f /etc/os-release
    {
        printf 'ID=%s\nPRETTY_NAME="%s"\n' "$2" "$4"
        [[ -n "$3" ]] && printf 'VERSION_CODENAME=%s\n' "$3"
        [[ -n "${5:-}" ]] && printf 'ID_LIKE="%s"\n' "$5"
        [[ -n "${6:-}" ]] && printf 'UBUNTU_CODENAME=%s\n' "$6"
    } >/etc/os-release
    rc="$(run /tmp/t.log "${good}" -y "${common[@]}")"
    check_eq "$1: refused" 1 "${rc}"
    check_contains "$1: names the system" "$4 is not supported" "$(cat /tmp/t.log)"
    check_contains "$1: lists what is supported" "${supported_line}" "$(cat /tmp/t.log)"
}
fake_os "Debian 11" debian bullseye "Debian GNU/Linux 11 (bullseye)" ""
fake_os "Ubuntu 23.10 (not an LTS)" ubuntu mantic "Ubuntu 23.10" debian
fake_os "Ubuntu 20.04 (past standard support)" ubuntu focal "Ubuntu 20.04.6 LTS" debian
fake_os "Linux Mint (a derivative)" linuxmint wilma "Linux Mint 22" "ubuntu debian" noble
fake_os "Pop!_OS (a derivative)" pop noble "Pop!_OS 24.04 LTS" "ubuntu debian" noble
fake_os "Fedora" fedora "" "Fedora Linux 40 (Workstation Edition)" ""
fake_os "a Debian that claims an Ubuntu codename" debian jammy "Odd Debian (jammy)" ""
check_lacks "never says 'Debian jammy'" "Debian jammy" "$(cat /tmp/t.log)"
fake_os "an Ubuntu that claims a Debian codename" ubuntu bookworm "Odd Ubuntu (bookworm)" debian
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

# ---------------------------------------------------------------- Raspberry Pi OS and needrestart
# Raspberry Pi OS (ID=raspbian) on the Debian suites is accepted by the
# installer and by the package; on Ubuntu it is refused by the installer.
if [[ " ${OS_IDS} " == *" raspbian "* ]]; then
    rm -f /etc/os-release
    printf 'ID=raspbian\nID_LIKE=debian\nVERSION_CODENAME=%s\nPRETTY_NAME="Raspbian GNU/Linux (%s)"\n' "${SUITE}" "${SUITE}" >/etc/os-release
    rc="$(run /tmp/t.log "${good}" -y --token-file /root/token "${common[@]}")"
    check_eq "Raspberry Pi OS: the installer goes through to enrollment (exits 1, no control plane)" 1 "${rc}"
    check_contains "Raspberry Pi OS: reaches enrollment" "enrollment failed" "$(cat /tmp/t.log)"
    check_eq "Raspberry Pi OS: the package is installed" "install ok installed" \
        "$(dpkg-query -W -f='${Status}' coaxhub-stationd)"
    apt_quiet purge coaxhub-stationd
    rm -f "${KEYRING}" "${SOURCES}"
    rm -f /etc/os-release
    cp -P /root/os-release.orig /etc/os-release
else
    rm -f /etc/os-release
    printf 'ID=raspbian\nID_LIKE=debian\nVERSION_CODENAME=%s\nPRETTY_NAME="Raspbian GNU/Linux"\n' "${SUITE}" >/etc/os-release
    rc="$(run /tmp/t.log "${good}" -y "${common[@]}")"
    check_eq "Raspberry Pi OS on an Ubuntu suite: refused" 1 "${rc}"
    check_contains "Raspberry Pi OS on an Ubuntu suite: names the system" "Raspbian GNU/Linux is not supported" "$(cat /tmp/t.log)"
    rm -f /etc/os-release
    cp -P /root/os-release.orig /etc/os-release
    nothing_written "raspbian on Ubuntu"
    # needrestart (Ubuntu's, on by default) runs after every apt-get and would stop
    # `curl | sudo sh` with a dialog unless NEEDRESTART_SUSPEND is set: its apt hook is
    # a shim that records what it was given.
    mkdir -p /usr/lib/needrestart
    # shellcheck disable=SC2016 # the shim's own $
    printf '#!/bin/sh\necho "suspend=${NEEDRESTART_SUSPEND:-unset}" >>/tmp/needrestart.seen\n' >/usr/lib/needrestart/apt-pinvoke
    chmod 0755 /usr/lib/needrestart/apt-pinvoke
    printf 'DPkg::Post-Invoke { "[ -x /usr/lib/needrestart/apt-pinvoke ] && /usr/lib/needrestart/apt-pinvoke || true"; };\n' \
        >/etc/apt/apt.conf.d/99needrestart-shim
fi

# ---------------------------------------------------------------- happy path
# Piped into sh, as `curl -fsSL …/station | sudo sh -s -- …` runs it.
rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh -s -- --token-file /root/token \
    "${common[@]}" <"${good}" >/tmp/install.log 2>&1 || rc=$?
log="$(cat /tmp/install.log)"
printf '# installer output:\n%s\n' "$(sed 's/^/#   /' /tmp/install.log)"
check_eq "happy path: exits 1 (no control plane to enroll with)" 1 "${rc}"
check_contains "happy path: lists the SDR as information" "Found an RTL-SDR (0bda:2838, RTL2838UHIDIR)" "${log}"
check_contains "happy path: says the web app sets up receivers" "web app" "${log}"
check_contains "happy path: reports the failed enrollment" "enrollment failed" "${log}"
check_contains "happy path: stationd says why" "cannot reach the enrollment service at https://enroll.${DOMAIN}" "${log}"
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
if [[ " ${OS_IDS} " != *" raspbian "* ]]; then
    check_contains "needrestart's hook ran after the installer's apt-get" "suspend=" "$(cat /tmp/needrestart.seen)"
    check_lacks "needrestart was always suspended (no dialog on a terminal)" "suspend=unset" "$(cat /tmp/needrestart.seen)"
fi
check "the installer writes no configuration" test ! -e /etc/coaxhub
check_eq "the installer writes no receivers.json" "" "$(ls -A "${RECEIVERS}")"
check_lacks "the installer never mentions a profile" "profile" "${log}"
check_eq "not enrolled after the failure" 3 "$(exit_code stationd status)"
check_eq "the credentials directory is untouched" "" "$(ls -A /var/lib/coaxhub/stationd)"

# ---------------------------------------------------------------- on a terminal
# As a person runs it: the token prompt is the only question on the terminal.
interactive() { python3 /test/interactive.py "$@"; }
rc=0
interactive /tmp/tty.log \
    "Enrollment token (input hidden):" "chs_enr_BADBAD" \
    "Enrollment token (input hidden):" "chs_enr_${TOKEN_BODY}" \
    -- sh -c "cat '${good}' | COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh -s -- ${common[*]}" || rc=$?
tty_log="$(tr -d '\r' </tmp/tty.log)"
printf '# terminal:\n%s\n' "$(grep -v '^\(Get\|Hit\|Reading\|Building\|Selecting\|Preparing\|Unpacking\|Setting\)' \
    <<<"${tty_log}" | sed 's/^/#   /')"
check_eq "terminal: exits 1 (no control plane)" 1 "${rc}"
check_contains "terminal: lists the SDR" "Found an RTL-SDR (0bda:2838, RTL2838UHIDIR)" "${tty_log}"
check_lacks "terminal: no profile question" "Choose" "${tty_log}"
check_lacks "terminal: no band question" "bands" "${tty_log}"
check_lacks "terminal: no profile at all" "profile" "${tty_log}"
check_contains "terminal: a refused token asks again" "Try again (2 of 3)" "${tty_log}"
check_contains "terminal: then reports the failed enrollment" "enrollment failed" "${tty_log}"
check_lacks "terminal: the refused token is not echoed" "BADBAD" "${tty_log}"
check_lacks "terminal: the token is not echoed" "${TOKEN_BODY}" "${tty_log}"

rc=0
interactive /tmp/tty.log "Enrollment token (input hidden):" x1 \
    "Enrollment token (input hidden):" x2 "Enrollment token (input hidden):" x3 \
    -- env COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" "${common[@]}" || rc=$?
check_eq "terminal: three refused tokens: exits 1" 1 "${rc}"
check_contains "terminal: three refused tokens: says so" "not accepted 3 times" "$(cat /tmp/tty.log)"

# ---------------------------------------------------------------- no SDR
# The installer says it sees none and carries on (a station can be installed
# before its SDR arrives), with no question, on a terminal and without one.
rc="$(COAXHUB_INSTALL_USB_DEVICES=/tmp/nousb run /tmp/t.log "${good}" --token-file /root/token "${common[@]}")"
check_eq "no SDR, no terminal: carries on (exits 1, no control plane)" 1 "${rc}"
check_contains "no SDR, no terminal: says it sees none" "No RTL-SDR found" "$(cat /tmp/t.log)"
check_contains "no SDR, no terminal: reaches enrollment" "enrollment failed" "$(cat /tmp/t.log)"
check_lacks "no SDR, no terminal: asks nothing" "Continue anyway" "$(cat /tmp/t.log)"
rc=0
interactive /tmp/tty.log \
    -- env COAXHUB_INSTALL_USB_DEVICES=/tmp/nousb sh "${good}" --token-file /root/token "${common[@]}" || rc=$?
tty_log="$(tr -d '\r' </tmp/tty.log)"
check_eq "no SDR, terminal: carries on (exits 1, no control plane)" 1 "${rc}"
check_contains "no SDR, terminal: says it sees none" "No RTL-SDR found" "${tty_log}"
check_contains "no SDR, terminal: reaches enrollment" "enrollment failed" "${tty_log}"
check_lacks "no SDR, terminal: asks nothing" "Continue anyway" "${tty_log}"

# ---------------------------------------------------------------- RTL-SDR Blog V4
# Listed by its USB product string like any other SDR: no question, and no
# choice made for the owner (the web app offers the V4 its own profile).
rc=0
interactive /tmp/tty.log \
    -- env COAXHUB_INSTALL_USB_DEVICES=/tmp/usbv4 sh "${good}" --token-file /root/token "${common[@]}" || rc=$?
tty_log="$(tr -d '\r' </tmp/tty.log)"
check_eq "V4 on a terminal: exits 1 (no control plane)" 1 "${rc}"
check_contains "V4 on a terminal: lists it" "Found an RTL-SDR (0bda:2838, Blog V4)" "${tty_log}"
check_lacks "V4 on a terminal: asks no question" "Choose" "${tty_log}"
check_lacks "V4 on a terminal: names no profile" "profile" "${tty_log}"

rc="$(COAXHUB_INSTALL_USB_DEVICES=/tmp/usbv4 run /tmp/t.log "${good}" --token-file /root/token "${common[@]}")"
check_eq "V4, no terminal: exits 1 (no control plane)" 1 "${rc}"
check_contains "V4, no terminal: lists it" "Found an RTL-SDR (0bda:2838, Blog V4)" "$(cat /tmp/t.log)"
check_lacks "V4, no terminal: names no profile" "profile" "$(cat /tmp/t.log)"
check "nothing under /etc/coaxhub after any of it" test ! -e /etc/coaxhub
check_eq "no receivers.json after any of it" "" "$(ls -A "${RECEIVERS}")"

# ---------------------------------------------------------------- again
sum_before="$(md5sum "${KEYRING}" "${SOURCES}")"
rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" --token-file /root/token \
    "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
check_eq "re-run: same result" 1 "${rc}"
check_eq "re-run: keyring and sources unchanged" "${sum_before}" "$(md5sum "${KEYRING}" "${SOURCES}")"
check_eq "re-run: one sources file" 1 "$(grep -rl "${URL}" /etc/apt/sources.list.d | wc -l)"

printf 'chs_enr_TOOSHORT\n' >/root/bad-token
rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" --token-file /root/bad-token \
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
check_contains "enrolled re-run: -y asks nothing and carries on without an SDR" "No RTL-SDR found" "${log}"
check_contains "enrolled re-run: --no-start names stationd alone" \
    "sudo systemctl enable --now stationd.service" "${log}"
check_lacks "enrolled re-run: no radiod unit is enabled or started" "coaxhub-radiod" "${log}"
check_contains "enrolled re-run: prints stationd status" "enrolled" "${log}"
check_eq "enrolled re-run: the key is untouched" "${key_before}" "$(md5sum /var/lib/coaxhub/stationd/key.pem)"

rc=0
PATH="/tmp/shim:${PATH}" COAXHUB_INSTALL_USB_DEVICES=/tmp/usbv4 sh "${good}" -y --no-start </dev/null \
    >/tmp/t.log 2>&1 || rc=$?
check_eq "V4 with --yes: exits 0" 0 "${rc}"
check_contains "V4 with --yes: lists it" "Found an RTL-SDR (0bda:2838, Blog V4)" "$(cat /tmp/t.log)"
check "V4 with --yes: nothing under /etc/coaxhub" test ! -e /etc/coaxhub
check_eq "V4 with --yes: no receivers.json" "" "$(ls -A "${RECEIVERS}")"

rc=0
COAXHUB_INSTALL_USB_DEVICES=/tmp/usb sh "${good}" --reenroll --token-file /root/token \
    "${common[@]}" </dev/null >/tmp/t.log 2>&1 || rc=$?
check_eq "--reenroll: enrolls again, and fails without a control plane" 1 "${rc}"
check_contains "--reenroll: reports it" "enrollment failed" "$(cat /tmp/t.log)"
check_eq "--reenroll: a failed enrollment keeps the old credentials" "${key_before}" \
    "$(md5sum /var/lib/coaxhub/stationd/key.pem)"

done_testing
