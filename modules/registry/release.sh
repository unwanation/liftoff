#!/usr/bin/env bash
# Signs one module release and adds it to the registry's index.json. Run by the modules workflow
# (.github/workflows/modules.yml) on a tag `modules/<crate-dir>/vX.Y.Z`
# of this repository, after `just modules` built the component; the
# workflow publishes the out-dir as a release of `liftoff-modules`.
#
#   modules/registry/release.sh <crate-dir> <component.wasm> <download-base-url> <out-dir> [<index.json>]
#
# <crate-dir>          the module's crate under modules/: its Cargo.toml has
#                      `[package.metadata.liftoff] liftoffd_min = "X.Y.Z"`.
# <component.wasm>     the built component. Its own manifest (what its
#                      `manifest()` returns, read and checked by the
#                      `module-manifest` example of crates/wasm) names the
#                      module, its version, what it requires and its
#                      config_schema.
# <download-base-url>  where the release's files will be downloadable, e.g.
#                      https://github.com/unwanation/liftoff-modules/releases/download/tenants-v0.1.0
# <out-dir>            receives <name>-<version>.wasm, its .minisig and the
#                      new index.json: the assets of the release.
# <index.json>         the index of the previous release (default: empty);
#                      a version it lists already is refused.
#
# Environment: MINISIGN_SECRET_KEY, the path of the registry's minisign
# secret key (made with `minisign -G -W`, no password); its public key is
# the one `liftoffd` pins (crates/liftoffd/src/modules.rs,
# REGISTRY_PUBLIC_KEY). EXPECT_VERSION, when set, is the version the
# release must have (the tag's): a component of another version is
# refused.
#
# Needs: jq, sha256sum, minisign, cargo.
#
# index.json (crates/core/src/module/registry_remote.rs documents it):
#   {"modules": {"<name>": {"<version>": {"url", "sha256", "signature",
#     "wit_version", "liftoffd_min", "requires", "config_schema"}}}}
# Only the .wasm is signed; the daemon checks its sha256 and signature
# against the pinned key and reads the component's own manifest again.
set -euo pipefail

die() {
  echo "release.sh: $*" >&2
  exit 1
}

[[ $# -ge 4 && $# -le 5 ]] || die "usage: release.sh <crate-dir> <component.wasm> <download-base-url> <out-dir> [<index.json>]"
crate_dir=$1
component=$2
base_url=${3%/}
out_dir=$4
previous=${5:-}

for tool in jq sha256sum minisign cargo; do
  command -v "$tool" >/dev/null || die "$tool is not installed"
done
[[ -n ${MINISIGN_SECRET_KEY:-} && -f $MINISIGN_SECRET_KEY ]] || die "MINISIGN_SECRET_KEY names no key file"
[[ -f $crate_dir/Cargo.toml ]] || die "$crate_dir/Cargo.toml is missing"
[[ -f $component ]] || die "$component is missing"

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
manifest=$work/manifest.json
cargo run --quiet --locked --manifest-path "$repo/Cargo.toml" -p liftoff-wasm \
  --example module-manifest -- "$component" >"$manifest" ||
  die "$component has no manifest the daemon accepts"
name=$(jq -er '.name' "$manifest")
version=$(jq -er '.version' "$manifest")
if [[ -n ${EXPECT_VERSION:-} && $version != "$EXPECT_VERSION" ]]; then
  die "$component is $name $version, the tag says $EXPECT_VERSION"
fi
# The world the component was built against: the package line of the WIT.
wit_version=$(sed -nE 's/^package liftoff:module@([0-9][^;]*);.*/\1/p' "$repo/wit/liftoff-module.wit")
[[ -n $wit_version ]] || die "no package line in wit/liftoff-module.wit"
liftoffd_min=$(cargo metadata --format-version 1 --no-deps --manifest-path "$crate_dir/Cargo.toml" |
  jq -er --arg dir "$(cd "$crate_dir" && pwd)/Cargo.toml" \
    '.packages[] | select(.manifest_path == $dir) | .metadata.liftoff.liftoffd_min') ||
  die "$crate_dir/Cargo.toml has no [package.metadata.liftoff] liftoffd_min"

mkdir -p "$out_dir"
file=$name-$version.wasm
cp "$component" "$out_dir/$file"
sha256=$(sha256sum "$out_dir/$file" | cut -d' ' -f1)
minisign -S -s "$MINISIGN_SECRET_KEY" -m "$out_dir/$file" -x "$out_dir/$file.minisig" \
  -t "liftoff module $name $version" >/dev/null
signature=$(cat "$out_dir/$file.minisig")

if [[ -n $previous ]]; then
  index=$(cat "$previous")
else
  index='{"modules":{}}'
fi
# A published version is never replaced: daemons may hold its hash.
if jq -e --arg name "$name" --arg version "$version" '.modules[$name][$version] != null' <<<"$index" >/dev/null; then
  die "$name $version is in the index already"
fi
jq --arg name "$name" --arg version "$version" --arg url "$base_url/$file" \
  --arg sha256 "$sha256" --arg signature "$signature" --arg wit "$wit_version" \
  --arg min "$liftoffd_min" --slurpfile manifest "$manifest" \
  '.modules[$name][$version] = {
     url: $url,
     sha256: $sha256,
     signature: $signature,
     wit_version: $wit,
     liftoffd_min: $min,
     requires: ($manifest[0].requires // []),
     config_schema: ($manifest[0].config_schema // null)
   }' <<<"$index" >"$out_dir/index.json"
echo "release.sh: $name $version -> $out_dir/index.json" >&2
