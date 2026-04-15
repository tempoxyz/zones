ARG CHEF_IMAGE=chef

FROM ${CHEF_IMAGE} AS builder

ARG RUST_PROFILE=profiling
ARG VERGEN_GIT_SHA
ARG VERGEN_GIT_SHA_SHORT
ARG EXTRA_RUSTFLAGS=""

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked,id=cargo-registry \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked,id=cargo-git \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked,id=sccache \
    RUSTFLAGS="-C link-arg=-fuse-ld=mold ${EXTRA_RUSTFLAGS}" \
    cargo build --profile ${RUST_PROFILE} \
        --bin tempo-zone --features "jemalloc"

FROM debian:bookworm-slim@sha256:4724b8cc51e33e398f0e2e15e18d5ec2851ff0c2280647e1310bc1642182655d AS base

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /data

# tempo-zone
FROM base AS tempo-zone
ARG RUST_PROFILE=profiling
COPY --from=builder /app/target/${RUST_PROFILE}/tempo-zone /usr/local/bin/tempo-zone
ENTRYPOINT ["/usr/local/bin/tempo-zone"]
