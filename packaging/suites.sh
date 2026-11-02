#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# The systems a station package is built for (docs/packaging.md §1), in one
# place: sourced by build-deb.sh, build-all.sh and the package tests.
#
#   ALL_SUITES    every suite, in the order they are built and tested
#   suite_info S  sets, for suite S (the system's VERSION_CODENAME):
#     SUITE_BASE      the Docker image the package is built and tested on
#     SUITE_SUFFIX    the version suffix: <version>-<revision>~<suffix>
#     SUITE_OS_IDS    the os-release IDs the package installs on (the package
#                     refuses every other system in its preinst)
#     SUITE_NAME      the system in words, for messages
#     SUITE_FLOOR     the first base-files version of the release: the package
#                     pre-depends on it, so that during an in-place release
#                     upgrade /etc/os-release already names the new release
#                     when the preinst reads it
#     SUITE_SYSLIB    where the package puts units, udev rules and modprobe
#                     files: jammy's kmod reads /lib/modprobe.d and not
#                     /usr/lib/modprobe.d, and a jammy that is not merged-/usr
#                     has them apart; every other suite keeps /usr/lib
#   SUPPORTED_TEXT  the supported systems, as the package and the installer
#                   list them in their refusals

# shellcheck disable=SC2034 # the variables are read by the scripts that source this
ALL_SUITES=(bookworm trixie jammy noble resolute)

# shellcheck disable=SC2034
SUPPORTED_TEXT="Ubuntu 22.04, 24.04 or 26.04 LTS, or Debian or Raspberry Pi OS 12 or 13 (64-bit)"

# suite_info SUITE: returns 1 for a suite that is not a target.
# shellcheck disable=SC2034
suite_info() {
    case "$1" in
    bookworm)
        SUITE_BASE=debian:bookworm SUITE_SUFFIX=deb12 SUITE_OS_IDS="debian raspbian"
        SUITE_NAME="Debian 12 (bookworm)" SUITE_FLOOR=12 SUITE_SYSLIB=usr/lib
        ;;
    trixie)
        SUITE_BASE=debian:trixie SUITE_SUFFIX=deb13 SUITE_OS_IDS="debian raspbian"
        SUITE_NAME="Debian 13 (trixie)" SUITE_FLOOR=13 SUITE_SYSLIB=usr/lib
        ;;
    jammy)
        SUITE_BASE=ubuntu:22.04 SUITE_SUFFIX=ubuntu22.04 SUITE_OS_IDS=ubuntu
        SUITE_NAME="Ubuntu 22.04 LTS (jammy)" SUITE_FLOOR=12ubuntu1 SUITE_SYSLIB=lib
        ;;
    noble)
        SUITE_BASE=ubuntu:24.04 SUITE_SUFFIX=ubuntu24.04 SUITE_OS_IDS=ubuntu
        SUITE_NAME="Ubuntu 24.04 LTS (noble)" SUITE_FLOOR=13ubuntu1 SUITE_SYSLIB=usr/lib
        ;;
    resolute)
        SUITE_BASE=ubuntu:26.04 SUITE_SUFFIX=ubuntu26.04 SUITE_OS_IDS=ubuntu
        SUITE_NAME="Ubuntu 26.04 LTS (resolute)" SUITE_FLOOR=14ubuntu1 SUITE_SYSLIB=usr/lib
        ;;
    *) return 1 ;;
    esac
}
