#!/usr/bin/env bash
# Collect every workspace binary built for TARGET, an archive of them
# with the `lof` symlink, scripts/install.sh and the CycloneDX SBOM into
# OUT_DIR, with a SHA256SUMS file and its signature. Used by the release
# workflows.
#
#   scripts/package-release.sh x86_64-unknown-linux-musl dist
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <target-triple> <out-dir>" >&2
  exit 64
fi
target="$1"
out="$2"
mkdir -p "$out"

# Binaries declared by workspace members (not dependencies).
mapfile -t bins < <(cargo metadata --format-version 1 --no-deps \
  | jq -r '.packages[].targets[] | select(.kind[] == "bin") | .name' | sort -u)
if [[ ${#bins[@]} -eq 0 ]]; then
  echo "error: no binaries in the workspace" >&2
  exit 65
fi
for bin in "${bins[@]}"; do
  cp "target/${target}/release/${bin}" "${out}/${bin}-${target}"
done

# The CLI answers to `lof` as well. A release asset cannot
# be a symlink (an upload would copy the binary), so the symlink travels
# in an archive of the binaries for manual installs; install.sh fetches
# the loose binaries and makes the symlink itself.
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
archive="liftoff-${target}"
mkdir "${stage}/${archive}"
for bin in "${bins[@]}"; do
  cp "target/${target}/release/${bin}" "${stage}/${archive}/${bin}"
done
if [[ -f "${stage}/${archive}/liftoff" ]]; then
  ln -s liftoff "${stage}/${archive}/lof"
fi
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "${out}/${archive}.tar.gz" "$archive"

# The installer of the release: `curl -fsSL
# <release>/install.sh | sudo sh`.
cp scripts/install.sh "${out}/install.sh"

# cargo cyclonedx writes next to the crate manifests; gather whatever it made.
find . -path ./target -prune -o -name "*-${target}.cdx.json" -print0 \
  | xargs -0 -r -I{} cp {} "$out/"

(cd "$out" && sha256sum ./* > "SHA256SUMS-${target}.txt")

# install.sh installs nothing whose sums are not signed by the release key
#: with LIFTOFF_SIGNING_KEY (a private ed25519 key file) the sums
# get their `.sig`.
if [[ -n "${LIFTOFF_SIGNING_KEY:-}" ]]; then
  ssh-keygen -q -Y sign -n liftoff-release -f "$LIFTOFF_SIGNING_KEY" "${out}/SHA256SUMS-${target}.txt"
else
  echo "warning: LIFTOFF_SIGNING_KEY is not set; install.sh refuses an unsigned release" >&2
fi
ls -l "$out"
