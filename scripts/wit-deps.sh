#!/usr/bin/env bash
# Keeps wit/deps/ a verbatim copy of wit/deps/ of the wasmtime-wasi-http
# crate in Cargo.lock: the WASI WIT (wasi:http, wasi:io, ...) the module
# host links, which the world wit/liftoff-module.wit imports. The check
# also holds the function templates' wit/deps/ (templates/fn-*/wit/deps/,
# plain files: a template is a repository of its own) equal to
# wit/function/deps/, the WIT of the function world.
#
#   scripts/wit-deps.sh          # refresh wit/deps/ (after a wasmtime bump)
#   scripts/wit-deps.sh --check  # fail when wit/deps/ drifted (just ci)
set -euo pipefail

cd "$(dirname "$0")/.."
cargo fetch --locked --quiet
version="$(cargo pkgid --locked wasmtime-wasi-http | sed 's/.*@//')"
src=""
for dir in "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/"wasmtime-wasi-http-$version"/wit/deps; do
  [[ -d "$dir" ]] && src="$dir" && break
done
if [[ -z "$src" ]]; then
  echo "wit-deps: wasmtime-wasi-http $version is not in the cargo registry" >&2
  exit 1
fi

if [[ "${1:-}" == "--check" ]]; then
  if ! diff -r "$src" wit/deps; then
    echo "wit-deps: wit/deps differs from wasmtime-wasi-http $version; run scripts/wit-deps.sh" >&2
    exit 1
  fi
  for template in templates/fn-*/wit/deps; do
    if ! diff -r wit/function/deps "$template"; then
      echo "wit-deps: $template differs from wit/function/deps; copy it with cp -rL" >&2
      exit 1
    fi
  done
  exit 0
fi

rm -rf wit/deps
cp -r "$src" wit/deps
echo "wit/deps: copied from wasmtime-wasi-http $version"
for template in templates/fn-*/wit; do
  rm -rf "$template/deps"
  cp -rL wit/function/deps "$template/deps"
  echo "$template/deps: copied from wit/function/deps"
done
