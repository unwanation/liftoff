# Task runner. `just` lists recipes; `just ci` runs everything CI runs.

set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load := true

export RUSTDOCFLAGS := "-D warnings"
export CARGO_TERM_COLOR := "always"
# Where the host tests (crates/wasm, crates/liftoffd, crates/liftoff) find the built
# modules: `just modules`
# puts every component of the `modules/` workspace here. Not `LIFTOFF_*`:
# liftoffd reads those as config keys and refuses unknown ones.
export WASM_MODULES_DIR := justfile_directory() / "modules/target/wasm32-wasip2/release"
# The componentize-py of `just functions` (`fn_python`).
componentize_py := "0.25.1"

default:
    @just --list --unsorted

# One-time developer setup: tools, git hooks, first build.
setup:
    mise install
    lefthook install
    cargo fetch --locked
    @echo "ready: run 'just ci' to verify the toolchain"

# --- Formatting -------------------------------------------------------------

fmt:
    cargo fmt --all
    cargo fmt --all --manifest-path modules/Cargo.toml
    taplo fmt

fmt-check:
    cargo fmt --all -- --check
    cargo fmt --all --manifest-path modules/Cargo.toml -- --check
    taplo fmt --check
    typos

# --- Static analysis --------------------------------------------------------

lint: modules-lint
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# Every feature combination must compile on its own.
features:
    cargo hack check --workspace --feature-powerset --no-dev-deps

machete:
    cargo machete --with-metadata

doc:
    cargo doc --workspace --all-features --no-deps --locked
    cargo doc --manifest-path modules/Cargo.toml --workspace --target wasm32-wasip2 --no-deps --locked

doc-open: doc
    cargo doc --workspace --all-features --no-deps --open

# --- Supply chain -----------------------------------------------------------

deny:
    cargo deny --workspace --all-features check
    cargo deny --manifest-path modules/Cargo.toml --workspace --all-features --config deny.toml check

audit:
    cargo audit --deny warnings
    cargo audit --deny warnings --file modules/Cargo.lock

# Lint GitHub Actions (syntax + security), shell scripts and the Dockerfile.
lint-infra:
    actionlint .github/workflows/*.yml
    zizmor --persona=pedantic .github/workflows/*.yml
    shellcheck --severity=warning scripts/*.sh modules/registry/*.sh
    hadolint Dockerfile
    gitleaks git --redact --no-banner

# --- Modules -----------------------------------------------------
# `modules/` is a workspace of its own targeting wasm32-wasip2; the native
# target links components directly (no cargo-component). The host tests in
# crates/wasm and crates/liftoffd load what `just modules` builds
# (WASM_MODULES_DIR above).

# Builds every module and contract component for wasm32-wasip2.
modules:
    cargo build --manifest-path modules/Cargo.toml --workspace --target wasm32-wasip2 --release --locked

# Builds the contract functions of modules/testing/functions/ for
# wasm32-wasip2: `fn_rust` and
# `fn_misbehave` with cargo, `fn_ts` with componentize-js (through `jco
# componentize`, both pinned in its package.json and bun.lock) under bun,
# `fn_python` with componentize-py under uv (bun and uv pinned in
# mise.toml). The function runtime's tests in crates/wasm load them from
# WASM_MODULES_DIR, like the modules. It then builds each function
# template (templates/fn-*) with its own `[function] build` in a copy
# under target/templates/ (`tpl_fn_*`), which
# crates/wasm/tests/templates.rs checks and serves.
functions:
    cargo build --manifest-path modules/Cargo.toml -p liftoff-function-rust -p liftoff-function-misbehave --target wasm32-wasip2 --release --locked
    mkdir -p "$WASM_MODULES_DIR"
    cd modules/testing/functions/ts && bun install --frozen-lockfile && bun --bun run build && cp dist/fn_ts.wasm "$WASM_MODULES_DIR/fn_ts.wasm"
    cd modules/testing/functions/python && uvx componentize-py@{{componentize_py}} -d ../../../../wit/function -w liftoff:function/function componentize app -o "$WASM_MODULES_DIR/fn_python.wasm"
    scripts/build-templates.sh "$WASM_MODULES_DIR"

modules-lint:
    cargo clippy --manifest-path modules/Cargo.toml --workspace --all-targets --target wasm32-wasip2 --locked -- -D warnings

# A dry run of the module releases (modules.yml): every releasable
# module's manifest read from its component, signed with a throwaway
# minisign key and added to an index in a temporary directory.
modules-release-check: modules
    modules/registry/check.sh

# wit/deps/ is the WASI WIT of the locked wasmtime-wasi-http; refresh it
# after a wasmtime bump. The check also holds templates/fn-*/wit/deps/
# equal to wit/function/deps/.
wit-deps:
    scripts/wit-deps.sh

wit-deps-check:
    scripts/wit-deps.sh --check

# --- Tests ------------------------------------------------------------------

test: modules functions
    cargo nextest run --workspace --all-features --locked
    cargo test --workspace --all-features --locked --doc

test-watch:
    cargo watch -x 'nextest run --workspace --all-features'

coverage: modules functions
    cargo llvm-cov nextest --workspace --all-features --locked --lcov --output-path lcov.info
    cargo llvm-cov report --summary-only

coverage-html: modules functions
    cargo llvm-cov nextest --workspace --all-features --locked --html --open

# --- Everything CI runs -------------------------------------------------------

ci: fmt-check wit-deps-check lint machete features test modules-release-check doc deny

# --- Build & run --------------------------------------------------------------

build:
    cargo build --workspace --all-features --locked

# Release binaries (liftoffd and liftoff).
release:
    cargo build --release --locked --workspace --bins
    @ls -la target/release/liftoffd target/release/liftoff

# Release binary with an embedded, auditable dependency list (cargo-auditable).
release-auditable:
    cargo auditable build --release --locked --workspace --bins

serve *args:
    LIFTOFF_LOG__FORMAT=pretty cargo run --locked -p liftoffd -- --config config/local.toml serve {{args}}

docker tag="liftoffd:local":
    docker build --tag {{tag}} .

clean:
    cargo clean
    cargo clean --manifest-path modules/Cargo.toml
    rm -rf lcov.info coverage
