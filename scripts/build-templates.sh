#!/usr/bin/env bash
# Builds every function template (templates/fn-*) the way `lof create`
# and `lof deploy` would: its tracked files
# copied into target/templates/<name>/, `{{project}}` substituted in the
# `[substitute] files`, then its own `[function] build` run there and its
# `[function] component` copied to <out-dir>/tpl_<name>.wasm (`-` as `_`),
# where crates/wasm/tests/templates.rs checks and serves it.
#
#   scripts/build-templates.sh <out-dir>     # part of `just functions`

set -euo pipefail

out=${1:?usage: build-templates.sh <out-dir>}
root=$(cd "$(dirname "$0")/.." && pwd)
work="$root/target/templates"
project="tpl-check"

# The string value of `key` in the table `[table]` of a liftoff.toml.
value() {
  awk -v table="[$2]" -v key="$3" '
    /^\[/ { inside = ($0 == table); next }
    inside && $1 == key && $2 == "=" {
      sub(/^[^=]*=[ \t]*"/, ""); sub(/"[ \t]*$/, ""); print; exit
    }' "$1"
}

# The `[substitute] files` of a liftoff.toml, one per line.
substituted() {
  awk '
    /^\[/ { inside = ($0 == "[substitute]"); next }
    inside && $1 == "files" {
      sub(/^[^\[]*\[/, ""); sub(/\].*$/, ""); gsub(/[" ]/, ""); gsub(/,/, "\n"); print; exit
    }' "$1"
}

mkdir -p "$out"
for template in "$root"/templates/fn-*/; do
  name=$(basename "$template")
  copy="$work/$name"
  rm -rf "$copy"
  mkdir -p "$copy"
  (cd "$template" && git ls-files -z . | xargs -0 cp --parents -t "$copy")
  manifest="$copy/liftoff.toml"
  while IFS= read -r file; do
    [ -n "$file" ] && sed -i "s/{{project}}/$project/g" "$copy/$file"
  done < <(substituted "$manifest")
  build=$(value "$manifest" function build)
  component=$(value "$manifest" function component)
  if [ -z "$build" ] || [ -z "$component" ]; then
    echo "$name: liftoff.toml has no [function] build and component" >&2
    exit 1
  fi
  echo "$name: $build" >&2
  (cd "$copy" && sh -c "$build")
  cp "$copy/$component" "$out/tpl_${name//-/_}.wasm"
done
