#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Runs every cargo-fuzz target of coaxhub-station-proto for N seconds each
# (station-protocol §10: protobuf decoding + hub validator, waterfall
# decompression, certificate identity). CI calls this; it also works locally
# locally.
#
# Usage: scripts/fuzz.sh [seconds-per-target]     (default 60)
#
# Needs the nightly toolchain and cargo-fuzz:
#   rustup toolchain install nightly --profile minimal
#   cargo binstall -y cargo-fuzz        (or: cargo install cargo-fuzz)
#
# Environment:
#   FUZZ_TARGETS      space-separated subset of targets (default: all)
#   FUZZ_TARGET_DIR   build directory (default: $CARGO_TARGET_DIR/fuzz, or
#                     target/fuzz)
#   FUZZ_JOBS         parallel libFuzzer jobs per target (default: 1; with
#                     more, libFuzzer logs to fuzz-<n>.log files instead)
#
# The committed seed corpus (crates/station-proto/fuzz/corpus/<target>) is
# read only; new inputs go to a scratch directory that is deleted on exit.
# Crashes are written to crates/station-proto/fuzz/artifacts/<target>/ and
# make the script exit non-zero.

set -euo pipefail

seconds="${1:-60}"
if ! [[ "$seconds" =~ ^[0-9]+$ ]] || [[ "$seconds" -eq 0 ]]; then
    echo "usage: $0 [seconds-per-target]" >&2
    exit 2
fi

station_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
crate_dir="$station_dir/crates/station-proto"
fuzz_dir="$crate_dir/fuzz"
target_dir="${FUZZ_TARGET_DIR:-${CARGO_TARGET_DIR:-$station_dir/target}/fuzz}"
jobs="${FUZZ_JOBS:-1}"
extra=()
if [[ "$jobs" -gt 1 ]]; then
    extra+=(-jobs="$jobs" -workers="$jobs")
fi
targets="${FUZZ_TARGETS:-station_message waterfall_decompress identity}"

if ! cargo +nightly fuzz --version >/dev/null 2>&1; then
    echo "error: cargo-fuzz on the nightly toolchain is required (see $0 header)" >&2
    exit 2
fi
# cargo-fuzz defaults to the triple it was built for (musl for prebuilt
# binaries); build for the nightly host instead.
host="$(rustc +nightly -vV | sed -n 's/^host: //p')"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

summary=()
status=0
cd "$crate_dir"
for target in $targets; do
    corpus="$fuzz_dir/corpus/$target"
    artifacts="$fuzz_dir/artifacts/$target"
    work="$scratch/$target"
    mkdir -p "$work" "$artifacts"
    log="$scratch/$target.log"
    echo "==> fuzzing $target for ${seconds}s"
    if cargo +nightly fuzz run \
        --fuzz-dir "$fuzz_dir" \
        --target "$host" \
        --target-dir "$target_dir" \
        "$target" "$work" "$corpus" -- \
        -max_total_time="$seconds" \
        -timeout=10 \
        ${extra[@]+"${extra[@]}"} \
        -print_final_stats=1 \
        -artifact_prefix="$artifacts/" 2>&1 | tee "$log"; then
        result="ok"
    else
        result="FAILED (see $artifacts)"
        status=1
    fi
    runs="$(grep -Eo 'Done [0-9]+ runs' "$log" | tail -n 1 | grep -Eo '[0-9]+' || true)"
    summary+=("$target: ${runs:-?} executions, $result")
done

echo
echo "fuzz summary (${seconds}s per target):"
for line in "${summary[@]}"; do
    echo "  $line"
done
exit "$status"
