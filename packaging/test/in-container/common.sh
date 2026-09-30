#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Assertions for the scripts that run inside test containers (sourced).
# Output is TAP-like: `ok N - what` / `not ok N - what`; the script exits
# non-zero if any check failed.

set -uo pipefail
export DEBIAN_FRONTEND=noninteractive

CHECKS=0
FAILURES=0

ok() {
    CHECKS=$((CHECKS + 1))
    printf 'ok %d - %s\n' "${CHECKS}" "$1"
}
not_ok() {
    CHECKS=$((CHECKS + 1))
    FAILURES=$((FAILURES + 1))
    printf 'not ok %d - %s\n' "${CHECKS}" "$1"
    if [[ -n "${2:-}" ]]; then
        printf '%s\n' "$2" | sed 's/^/#   /'
    fi
}

# check WHAT COMMAND...: passes when COMMAND succeeds.
check() {
    local what="$1" out
    shift
    if out="$("$@" 2>&1)"; then ok "${what}"; else not_ok "${what}" "${out}"; fi
}

# check_not WHAT COMMAND...: passes when COMMAND fails.
check_not() {
    local what="$1" out
    shift
    if out="$("$@" 2>&1)"; then not_ok "${what}" "${out}"; else ok "${what}"; fi
}

# check_eq WHAT EXPECTED ACTUAL
check_eq() {
    if [[ "$2" == "$3" ]]; then ok "$1"; else not_ok "$1" "expected: $2"$'\n'"actual:   $3"; fi
}

# check_contains WHAT NEEDLE HAYSTACK
check_contains() {
    if [[ "$3" == *"$2"* ]]; then ok "$1"; else not_ok "$1" "missing: $2"$'\n'"in: $3"; fi
}

# check_lacks WHAT NEEDLE HAYSTACK
check_lacks() {
    if [[ "$3" == *"$2"* ]]; then not_ok "$1" "unexpected: $2"; else ok "$1"; fi
}

# mode_owner PATH: "<octal mode> <user> <group>"
mode_owner() { stat -c '%a %U %G' "$1" 2>&1; }

# exit_code COMMAND...: prints the exit code.
exit_code() {
    local rc=0
    "$@" >/dev/null 2>&1 || rc=$?
    printf '%s' "${rc}"
}

apt_quiet() { apt-get -q -y -o Dpkg::Use-Pty=0 "$@" >/tmp/apt.log 2>&1 || { tail -n 30 /tmp/apt.log; return 1; }; }

done_testing() {
    printf '1..%d\n' "${CHECKS}"
    if ((FAILURES)); then
        printf '# %d of %d checks failed\n' "${FAILURES}" "${CHECKS}"
        exit 1
    fi
    printf '# all %d checks passed\n' "${CHECKS}"
}
