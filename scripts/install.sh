#!/bin/sh
# Installs (or upgrades) liftoff from a release:
#
#   curl -fsSL https://github.com/unwanation/liftoff/releases/latest/download/install.sh | sudo sh
#
# Downloads liftoffd and liftoff for this machine, checks the signature of
# the release's SHA256SUMS (`ssh-keygen -Y verify` against the release key
# pinned below) and the binaries against it, puts them in /usr/local/bin
# with the `lof` symlink, and tells what comes next. `lof server upgrade`
# runs this same script. Anything unsigned, mismatched or fetched over
# anything but https:// (or an explicit file://) is refused, and the
# installed binaries stay as they were.
#
# Environment:
#   LIFTOFF_RELEASE_URL  where the release files are (https:// or file://);
#                        default: the latest release on the forge
#   LIFTOFF_VERSION      the tag to install instead of the latest release
#   LIFTOFF_FORGE        the forge: https://github.com (the default) or a
#                        Forgejo/Gitea host
#   LIFTOFF_REPO         the repository (default unwanation/liftoff)
#   LIFTOFF_BIN_DIR      where the binaries go (default /usr/local/bin)
#   LIFTOFF_RELEASE_KEY  the public key (`ssh-ed25519 AAAA…`) that signs the
#                        releases, instead of the pinned one
set -eu

# The public key of the liftoff releases (`ssh-keygen -Y sign -n
# liftoff-release`, see scripts/package-release.sh), pinned here by the
# release process before v2.0.0; LIFTOFF_RELEASE_KEY
# overrides it, and nothing is installed without a key.
RELEASE_KEY="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE200D9ViVv3jd3GO0gKjK43na0QKj5kTI8ZImrVdHpu"
# The signature namespace of the releases.
NAMESPACE=liftoff-release

forge="${LIFTOFF_FORGE:-https://github.com}"
repo="${LIFTOFF_REPO:-unwanation/liftoff}"
bin_dir="${LIFTOFF_BIN_DIR:-/usr/local/bin}"

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "needs $1"
}

need curl
need sha256sum
need ssh-keygen
need uname
[ "$(id -u)" -eq 0 ] || [ -n "${LIFTOFF_BIN_DIR:-}" ] || fail "run as root (sudo sh)"

release_key="${LIFTOFF_RELEASE_KEY:-$RELEASE_KEY}"
[ -n "$release_key" ] || fail "no release key is pinned in this install.sh; set LIFTOFF_RELEASE_KEY"

# Only https:// (redirects included, TLS 1.2+) or an explicit file://.
fetch() {
  case "$1" in
    https://*) curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL "$@" ;;
    file://*) curl --proto '=file' -fsSL "$@" ;;
    *) fail "refusing $1: only https:// (or file://) URLs" ;;
  esac
}

case "$(uname -m)" in
  x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
  aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
  *) fail "no release for $(uname -m)" ;;
esac

release="${LIFTOFF_RELEASE_URL:-}"
case "$forge" in
  https://*) ;;
  *) fail "refusing forge $forge: only https://" ;;
esac
case "$release" in
  "" | https://* | file://*) ;;
  *) fail "refusing release $release: only https:// (or file://)" ;;
esac
if [ -z "$release" ]; then
  tag="${LIFTOFF_VERSION:-}"
  if [ -z "$tag" ]; then
    # GitHub answers on api.github.com; a Forgejo or Gitea host under /api/v1.
    case "$forge" in
      https://github.com) api="https://api.github.com/repos/${repo}" ;;
      *) api="${forge}/api/v1/repos/${repo}" ;;
    esac
    tag="$(fetch "${api}/releases/latest" \
      | tr ',' '\n' | sed -n 's/^.*"tag_name": *"\([^"]*\)".*$/\1/p' | head -n 1)"
    [ -n "$tag" ] || fail "cannot find the latest release of ${repo} on ${forge}"
  fi
  release="${forge}/${repo}/releases/download/${tag}"
fi
release="${release%/}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

sums="SHA256SUMS-${target}.txt"
for file in "liftoffd-${target}" "liftoff-${target}" "$sums" "${sums}.sig"; do
  fetch "${release}/${file}" -o "${work}/${file}" || fail "cannot download ${release}/${file}"
done

# The sums are signed by the release key; then package-release.sh's
# `<hash>  ./<file>` lines must list both binaries and match them.
(
  cd "$work"
  printf '%s %s\n' "$NAMESPACE" "$release_key" > allowed_signers
  ssh-keygen -Y verify -f allowed_signers -I "$NAMESPACE" -n "$NAMESPACE" \
    -s "${sums}.sig" < "$sums" > /dev/null 2>&1 \
    || fail "${sums} does not carry the signature of the release key: refusing to install"
  grep -E "^[0-9a-f]{64}  (\./)?(liftoffd|liftoff)-${target}\$" "$sums" > checked.txt || true
  [ "$(wc -l < checked.txt)" -eq 2 ] || fail "${sums} does not list both binaries"
  sha256sum -c --quiet checked.txt || fail "checksum mismatch: refusing to install"
)

mkdir -p "$bin_dir"
for bin in liftoffd liftoff; do
  # A copy next to the target, then a rename: a running binary is replaced,
  # never written over.
  cp "${work}/${bin}-${target}" "${bin_dir}/.${bin}.new"
  chmod 0755 "${bin_dir}/.${bin}.new"
  mv -f "${bin_dir}/.${bin}.new" "${bin_dir}/${bin}"
done
ln -sfn liftoff "${bin_dir}/lof"

echo "installed $("${bin_dir}/liftoffd" --version) and $("${bin_dir}/liftoff" --version) in ${bin_dir}" >&2
if [ ! -f /etc/liftoff/liftoffd.toml ]; then
  echo "next: sudo liftoffd setup" >&2
fi
