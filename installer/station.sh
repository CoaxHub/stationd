#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
#
# CoaxHub station installer.
#
#   curl -fsSL https://get.<domain>/station | sudo sh
#
# What it does, in order:
#   1. checks that it runs as root on Debian or Raspberry Pi OS (bookworm or
#      trixie), on amd64 or arm64;
#   2. lists the SDRs it sees on USB, as information: it asks nothing about
#      them. The owner sets up each receiver (its SDR, model and band) in the
#      web app, from the list the station reports;
#   3. downloads the CoaxHub APT repository key, checks its fingerprint
#      against the one pinned below, and adds the repository;
#   4. installs (or upgrades) the coaxhub-stationd package;
#   5. waits for the clock to be synchronised by NTP (TLS needs it);
#   6. enrolls the station: stationd asks for the enrollment token on the
#      terminal, with the input hidden. The token is never a command-line
#      argument, so it stays out of sudo's log, the process list and your
#      shell history;
#   7. enables and starts stationd.service and shows `stationd status`.
#      stationd starts the receivers' radiod services itself.
#
# Running it again upgrades the package and keeps the enrollment and the
# receivers.
#
# Options:
#   --token-file PATH           read the enrollment token from PATH
#                               (scripted installs) instead of the terminal
#   --reenroll                  enroll again even if already enrolled
#   --no-start                  do not enable or start stationd
#   --skip-time-sync            do not wait for NTP (containers without systemd)
#   -y, --yes                   take the default of any confirmation the
#                               installer asks (scripted installs)
#   -h, --help                  this help

set -eu

# Filled in when the installer is published (both must be replaced).
APT_URL='@COAXHUB_APT_URL@'
KEY_FINGERPRINT='@COAXHUB_KEY_FINGERPRINT@'

KEYRING=/usr/share/keyrings/coaxhub-archive-keyring.gpg
SOURCES=/etc/apt/sources.list.d/coaxhub.sources
PACKAGE=coaxhub-stationd
# RTL2832U dongles (vendor:product).
RTLSDR_IDS="0bda:2838 0bda:2832"
# Where USB devices are listed (overridable for tests only).
USB_DEVICES="${COAXHUB_INSTALL_USB_DEVICES:-/sys/bus/usb/devices}"
TIME_SYNC_TIMEOUT=120
TOKEN_ATTEMPTS=3

token_file=""
reenroll=false
start=true
wait_time_sync=true
tmp=""
tty_state=""

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}
usage() {
	cat <<-'USAGE'
		CoaxHub station installer: installs coaxhub-stationd, enrolls the station
		and starts it. You set up its receivers in the CoaxHub web app.

		  curl -fsSL https://get.<domain>/station | sudo sh
		  curl -fsSL https://get.<domain>/station | sudo sh -s -- [options]

		Options:
		  --token-file PATH           read the enrollment token from PATH
		  --reenroll                  enroll again even if already enrolled
		  --no-start                  do not enable or start stationd
		  --skip-time-sync            do not wait for NTP (containers without systemd)
		  -y, --yes                   take the default of any confirmation
	USAGE
}

cleanup() {
	# A Ctrl-C during the hidden token prompt must not leave echo off.
	[ -z "$tty_state" ] || stty "$tty_state" </dev/tty 2>/dev/null || true
	[ -z "$tmp" ] || rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---------------------------------------------------------------- arguments
while [ $# -gt 0 ]; do
	case "$1" in
	--token-file)
		[ $# -ge 2 ] || die "--token-file needs a path"
		token_file="$2"
		shift 2
		;;
	--reenroll) reenroll=true && shift ;;
	--no-start) start=false && shift ;;
	--skip-time-sync) wait_time_sync=false && shift ;;
	-y | --yes) shift ;; # no step asks a yes/no question; accepted for scripts that pass it
	-h | --help)
		usage
		exit 0
		;;
	--token | --token=*) die "the token is never passed as an argument: stationd asks for it (or use --token-file)" ;;
	*) die "unknown option: $1 (see --help)" ;;
	esac
done

if [ -n "$token_file" ] && [ ! -r "$token_file" ]; then
	die "cannot read the token file $token_file"
fi

# ---------------------------------------------------------------- terminal
# stdin is this script when piped from curl, so the token prompt uses /dev/tty.
have_tty() { (: </dev/tty) 2>/dev/null; }

# ---------------------------------------------------------------- checks
check_placeholders() {
	case "$APT_URL$KEY_FINGERPRINT" in
	*@COAXHUB_*) die "this is the installer template: its repository URL and key fingerprint were not filled in" ;;
	esac
	case "$APT_URL" in
	http://* | https://*) ;;
	*) die "bad repository URL in the installer: $APT_URL" ;;
	esac
	KEY_FINGERPRINT=$(printf '%s' "$KEY_FINGERPRINT" | tr 'a-f' 'A-F')
	if [ "${#KEY_FINGERPRINT}" -ne 40 ] || [ -n "$(printf '%s' "$KEY_FINGERPRINT" | tr -d '0-9A-F')" ]; then
		die "bad key fingerprint in the installer: $KEY_FINGERPRINT"
	fi
}

check_system() {
	[ "$(id -u)" -eq 0 ] || die "run this as root, e.g.: curl -fsSL <installer URL> | sudo sh"
	[ -r /etc/os-release ] || die "cannot identify this system (no /etc/os-release)"
	# shellcheck disable=SC1091 # the system's own file
	. /etc/os-release
	case " ${ID:-} ${ID_LIKE:-} " in
	*" debian "*) ;;
	*) die "${PRETTY_NAME:-${ID:-this system}} is not supported: CoaxHub stations run on Debian or Raspberry Pi OS" ;;
	esac
	codename="${VERSION_CODENAME:-}"
	case "$codename" in
	bookworm | trixie) ;;
	*) die "Debian ${codename:-${VERSION_ID:-unknown}} is not supported; use Debian or Raspberry Pi OS bookworm (12) or trixie (13)" ;;
	esac
	command -v dpkg >/dev/null 2>&1 || die "dpkg not found"
	arch=$(dpkg --print-architecture)
	case "$arch" in
	amd64 | arm64) ;;
	armhf | armel) die "this is a 32-bit system ($arch); install the 64-bit Raspberry Pi OS (arm64)" ;;
	*) die "the $arch architecture is not supported; use amd64 or arm64" ;;
	esac
}

# ---------------------------------------------------------------- SDR
list_sdrs() {
	found=0
	for dir in "$USB_DEVICES"/*; do
		if [ ! -r "$dir/idVendor" ] || [ ! -r "$dir/idProduct" ]; then
			continue
		fi
		id="$(cat "$dir/idVendor"):$(cat "$dir/idProduct")"
		for want in $RTLSDR_IDS; do
			if [ "$id" = "$want" ]; then
				product=$(cat "$dir/product" 2>/dev/null || true)
				say "Found an RTL-SDR ($id${product:+, $product})."
				found=$((found + 1))
			fi
		done
	done
	if [ "$found" -eq 0 ]; then
		say "No RTL-SDR found on USB (looked for $RTLSDR_IDS); you can plug it in later."
	fi
	say "You choose each receiver's SDR and band in the CoaxHub web app, after the station is enrolled."
}

# ---------------------------------------------------------------- repository
download() {
	# download URL FILE
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --retry 3 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		die "neither curl nor wget is installed"
	fi
}

install_prerequisites() {
	missing=""
	command -v gpg >/dev/null 2>&1 || missing="$missing gpg"
	command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || missing="$missing curl"
	case "$APT_URL" in
	https://*) [ -e /etc/ssl/certs/ca-certificates.crt ] || missing="$missing ca-certificates" ;;
	esac
	[ -n "$missing" ] || return 0
	say "Installing:$missing"
	apt-get update -q >/dev/null
	# shellcheck disable=SC2086 # a list of packages
	DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends $missing >/dev/null
}

add_repository() {
	say "Adding the CoaxHub APT repository ($APT_URL)."
	download "$APT_URL/coaxhub-archive-keyring.asc" "$tmp/key.asc" ||
		die "cannot download the repository key from $APT_URL"
	# Exactly one primary key, with the pinned fingerprint. Nothing is
	# written outside $tmp before this check passes.
	mkdir -m 0700 "$tmp/gnupg"
	keys=$(GNUPGHOME="$tmp/gnupg" gpg --batch --quiet --show-keys --with-colons "$tmp/key.asc" 2>/dev/null) ||
		die "the downloaded repository key is not a valid OpenPGP key"
	primaries=$(printf '%s\n' "$keys" | grep -c '^pub:' || true)
	actual=$(printf '%s\n' "$keys" | awk -F: '$1 == "pub" { p = 1; next } p && $1 == "fpr" { print $10; exit }')
	if [ "$primaries" -ne 1 ] || [ "$actual" != "$KEY_FINGERPRINT" ]; then
		die "the repository key's fingerprint is ${actual:-missing}, expected $KEY_FINGERPRINT: not installing (is $APT_URL the right repository?)"
	fi
	GNUPGHOME="$tmp/gnupg" gpg --batch --quiet --dearmor <"$tmp/key.asc" >"$tmp/keyring.gpg"
	install -D -m 0644 "$tmp/keyring.gpg" "$KEYRING"
	cat >"$tmp/coaxhub.sources" <<-SOURCES
		Types: deb
		URIs: $APT_URL
		Suites: $codename
		Components: main
		Architectures: $arch
		Signed-By: $KEYRING
	SOURCES
	install -D -m 0644 "$tmp/coaxhub.sources" "$SOURCES"
}

install_package() {
	say "Installing $PACKAGE."
	apt-get update -q >/dev/null || die "apt-get update failed"
	DEBIAN_FRONTEND=noninteractive apt-get install -y -q "$PACKAGE" ||
		die "installing $PACKAGE failed"
	command -v stationd >/dev/null 2>&1 || die "$PACKAGE is installed but stationd is missing"
}

# The kernel's DVB driver holds the dongle until it is unloaded; the package
# blacklists it for later boots.
release_dvb_driver() {
	[ -d /sys/module/dvb_usb_rtl28xxu ] || return 0
	if modprobe -r dvb_usb_rtl28xxu 2>/dev/null; then
		say "Unloaded the kernel's DVB driver, which was holding the RTL-SDR."
	else
		warn "the kernel's DVB driver (dvb_usb_rtl28xxu) holds the RTL-SDR; reboot to release it"
	fi
}

# ---------------------------------------------------------------- clock
wait_for_time_sync() {
	[ "$wait_time_sync" = true ] || return 0
	command -v timedatectl >/dev/null 2>&1 ||
		die "timedatectl not found: cannot check the clock (use --skip-time-sync only where the clock is known to be right)"
	waited=0
	while [ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null || true)" != yes ]; do
		if [ "$waited" -eq 0 ]; then
			say "Waiting for the clock to be synchronised by NTP (up to $TIME_SYNC_TIMEOUT s)..."
		fi
		if [ "$waited" -ge "$TIME_SYNC_TIMEOUT" ]; then
			die "the clock is not synchronised after $TIME_SYNC_TIMEOUT s. Check the network and 'timedatectl status' (enable NTP with 'timedatectl set-ntp true'), then run the installer again"
		fi
		sleep 2
		waited=$((waited + 2))
	done
	say "The clock is synchronised."
}

# ---------------------------------------------------------------- enrollment
# stationd's exit codes (stationd --help, README): 0 enrolled, 3 not
# enrolled, 4 the token was refused or malformed.
enroll() {
	status=0
	stationd status >/dev/null 2>&1 || status=$?
	case "$status" in
	0)
		if [ "$reenroll" = false ]; then
			say "This station is already enrolled (use --reenroll to enroll it again)."
			return 0
		fi
		;;
	3) ;;
	*) die "stationd cannot read its configuration or credentials; run 'stationd status' to see why" ;;
	esac
	force=""
	[ "$reenroll" = false ] || force=--force

	if [ -n "$token_file" ]; then
		say "Enrolling with the token in $token_file."
		# stationd runs as the service user and reads the token on stdin.
		rc=0
		runuser -u coaxhub -- stationd enroll ${force:+"$force"} <"$token_file" || rc=$?
		case "$rc" in
		0) return 0 ;;
		4) die "the token in $token_file was not accepted (see the message above); create a new one in the web app" ;;
		*) die "enrollment failed (see the message above; stationd exit code $rc)" ;;
		esac
	fi

	have_tty || die "no terminal to ask for the enrollment token on; use --token-file"
	say "Enroll this station: create an enrollment token for it in the CoaxHub web app"
	say "and paste it below (the input is hidden)."
	tty_state=$(stty -g </dev/tty 2>/dev/null || true)
	attempt=1
	while :; do
		rc=0
		# stationd prompts on the terminal with echo off and keeps the token to itself.
		runuser -u coaxhub -- stationd enroll ${force:+"$force"} </dev/tty || rc=$?
		case "$rc" in
		0) return 0 ;;
		4)
			[ "$attempt" -lt "$TOKEN_ATTEMPTS" ] ||
				die "the token was not accepted $TOKEN_ATTEMPTS times; create a new one and run the installer again"
			attempt=$((attempt + 1))
			say "Try again ($attempt of $TOKEN_ATTEMPTS)."
			;;
		*) die "enrollment failed (see the message above; stationd exit code $rc). Fix it and run the installer again" ;;
		esac
	done
}

# ---------------------------------------------------------------- services
# Only stationd is enabled: it starts the receivers' radiod instances
# (coaxhub-radiod@<receiver_id>.service) itself.
start_services() {
	if [ "$start" = false ]; then
		say "Not starting stationd (--no-start). Start it with:"
		say "  sudo systemctl enable --now stationd.service"
		return 0
	fi
	systemctl enable --quiet stationd.service
	# restart: a station that is already running picks up a new package.
	if ! systemctl restart stationd.service; then
		warn "stationd did not start; see: journalctl -u stationd"
	fi
	sleep 3
}

main() {
	# Under `curl | sudo sh` stdin is this script: no command may read it.
	# Questions and the token prompt use /dev/tty.
	exec </dev/null
	check_placeholders
	check_system
	list_sdrs
	tmp=$(mktemp -d)
	install_prerequisites
	add_repository
	install_package
	release_dvb_driver
	wait_for_time_sync
	enroll
	start_services
	say ""
	stationd status || true
}

main
