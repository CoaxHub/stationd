#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Enforces the license boundary that cargo-deny cannot express on its own:
# an MIT-licensed crate in this workspace (station-proto, station-conformance) must never
# have a transitive, non-dev dependency on a workspace crate licensed GPL-*. GPL-3.0-only
# is expected and fine elsewhere in this workspace (stationd, the backend adapters,
# packaging) -- cargo-deny's own deny.toml already allows it. What it can't check is the
# *direction*: that the MIT crates specifically stay free of it, so someone can build an
# alternative, non-GPL station implementation against station-proto alone.
#
# Method: ask `cargo metadata` for the full resolved dependency graph, keep only edges
# between workspace members that are not exclusively dev-dependencies, and walk that graph
# from every MIT-licensed workspace crate. Fails if any reachable workspace crate is
# licensed GPL-*.
#
# Usage: scripts/check-license-boundary.sh
# Run from anywhere; it locates the workspace itself via `cargo metadata`.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${script_dir}/.."

if ! command -v jq >/dev/null 2>&1; then
    echo "check-license-boundary.sh: jq is required but was not found on PATH" >&2
    exit 1
fi

metadata="$(cargo metadata --format-version=1)"
workspace_members_json="$(jq -c '.workspace_members' <<<"${metadata}")"

# id<TAB>name<TAB>license for every workspace member (license is "" if unset).
declare -A name_of
declare -A license_of
while IFS=$'\t' read -r id name license; do
    name_of["${id}"]="${name}"
    license_of["${id}"]="${license}"
done < <(jq -r --argjson wm "${workspace_members_json}" '
    .packages[]
    | select(IN(.id; $wm[]))
    | "\(.id)\t\(.name)\t\(.license // "")"
' <<<"${metadata}")

# Edges src<TAB>dst between workspace members, keeping only non-dev dependency kinds
# (normal or build). A dependency declared only under [dev-dependencies] is excluded.
edges=()
while IFS=$'\t' read -r src dst; do
    edges+=("${src}"$'\t'"${dst}")
done < <(jq -r --argjson wm "${workspace_members_json}" '
    .resolve.nodes[]
    | select(IN(.id; $wm[]))
    | .id as $src
    | .deps[]
    | select(IN(.pkg; $wm[]))
    | select([.dep_kinds[]?.kind] | any(. == null or . == "build"))
    | "\($src)\t\(.pkg)"
' <<<"${metadata}")

# Prints every workspace member id reachable from $1 via `edges` (excluding $1 itself),
# one per line.
reachable_from() {
    local start="$1"
    local -A seen=()
    local -a queue=("${start}")
    seen["${start}"]=1
    while ((${#queue[@]} > 0)); do
        local cur="${queue[0]}"
        queue=("${queue[@]:1}")
        local edge e_src e_dst
        for edge in "${edges[@]+"${edges[@]}"}"; do
            e_src="${edge%%$'\t'*}"
            e_dst="${edge#*$'\t'}"
            if [[ "${e_src}" == "${cur}" && -z "${seen[${e_dst}]+x}" ]]; then
                seen["${e_dst}"]=1
                queue+=("${e_dst}")
            fi
        done
    done
    unset 'seen[$start]'
    local id
    for id in "${!seen[@]}"; do
        printf '%s\n' "${id}"
    done
}

violations=0
for id in "${!license_of[@]}"; do
    [[ "${license_of[${id}]}" == "MIT" ]] || continue
    while IFS= read -r reached; do
        [[ -n "${reached}" ]] || continue
        reached_license="${license_of[${reached}]:-}"
        if [[ "${reached_license}" == GPL* ]]; then
            echo "license boundary violation: MIT crate '${name_of[${id}]}' has a" \
                "transitive, non-dev dependency on GPL crate" \
                "'${name_of[${reached}]}' (${reached_license})" >&2
            violations=$((violations + 1))
        fi
    done < <(reachable_from "${id}")
done

if ((violations > 0)); then
    echo "check-license-boundary.sh: ${violations} violation(s) found" >&2
    exit 1
fi

echo "check-license-boundary.sh: OK — no MIT workspace crate depends on a GPL workspace crate"
