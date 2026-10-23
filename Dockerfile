# syntax=docker/dockerfile:1.7
# Static, non-root, distroless image. Build with BuildKit (default in Docker 23+):
#   docker build -t liftoffd .

# --- build stage -------------------------------------------------------------
# Keep the tag in sync with rust-toolchain.toml. Digest-pinned; Dependabot bumps it.
FROM rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS builder

ARG TARGET=x86_64-unknown-linux-musl
# Use the toolchain baked into the image; skip rust-toolchain.toml's extra components.
ENV RUSTUP_TOOLCHAIN=1.98.1 \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=10

WORKDIR /src
# musl-gcc: aws-lc-sys (reqwest's TLS) compiles C for the musl target.
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "${TARGET}"

COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p liftoffd --target "${TARGET}" \
    && mkdir -p /out \
    && cp "target/${TARGET}/release/liftoffd" /out/liftoffd

# --- runtime stage -----------------------------------------------------------
# Distroless static: no shell, no package manager, runs as uid 65532.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

COPY --from=builder /out/liftoffd /usr/local/bin/liftoffd
COPY config/default.toml /etc/liftoff/config.toml

# Inside a container the listeners must bind all interfaces; TLS and auth
# belong to the ingress in front of this pod.
ENV LIFTOFF_API__BIND=0.0.0.0:8080 \
    LIFTOFF_LOG__FORMAT=json

EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/liftoffd"]
CMD ["--config", "/etc/liftoff/config.toml", "serve"]
