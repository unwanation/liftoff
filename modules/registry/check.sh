#!/usr/bin/env bash
# A dry run of the module releases: what
# .github/workflows/modules.yml does on a tag, for every releasable
# module at once, signed with a throwaway key into a temporary directory.
# Fails when a module's component has no manifest the daemon accepts, its
# Cargo.toml lacks `liftoffd_min`, or the index cannot be built. `just ci`
# runs it after `just modules`.
#
#   modules/registry/check.sh [<wasm-dir>]
#
# <wasm-dir>  where `just modules` put the components (default:
#             $WASM_MODULES_DIR, else modules/target/wasm32-wasip2/release).
#
# A releasable module is a crate of the modules/ workspace with
# `[package.metadata.liftoff]`; the contract components under
# modules/testing/ have none.
#
# Needs: jq, sha256sum, minisign, cargo.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
wasm_dir=${1:-${WASM_MODULES_DIR:-$repo/modules/target/wasm32-wasip2/release}}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
minisign -G -W -p "$work/key.pub" -s "$work/key" >/dev/null

# <crate-dir> <lib name> of every releasable module.
releasable=$(cargo metadata --format-version 1 --no-deps --locked --manifest-path "$repo/modules/Cargo.toml" |
  jq -r '.packages[] | select(.metadata.liftoff != null)
    | "\(.manifest_path | rtrimstr("/Cargo.toml")) \(.targets[] | select(.kind | index("cdylib")) | .name)"')
[[ -n $releasable ]] || {
  echo "check.sh: no module has [package.metadata.liftoff]" >&2
  exit 1
}

previous=()
count=0
while read -r crate_dir lib; do
  out=$work/$(basename "$crate_dir")
  MINISIGN_SECRET_KEY=$work/key "$repo/modules/registry/release.sh" "$crate_dir" \
    "$wasm_dir/$lib.wasm" "https://registry.invalid/$(basename "$crate_dir")" "$out" "${previous[@]}"
  minisign -V -q -p "$work/key.pub" -m "$out"/*.wasm
  previous=("$out/index.json")
  count=$((count + 1))
done <<<"$releasable"

modules=$(jq '.modules | length' "${previous[0]}")
[[ $modules -eq $count ]] || {
  echo "check.sh: the index lists $modules modules, $count were released" >&2
  exit 1
}
released=$(jq -r '.modules | to_entries[] | "\(.key) \(.value | keys | join(","))"' "${previous[0]}" | paste -sd ';' - | sed 's/;/; /g')
echo "check.sh: $count modules sign and index: $released" >&2
