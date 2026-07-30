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
        --bin tempo-zone --features "jemalloc" \
    && RUSTFLAGS="-C link-arg=-fuse-ld=mold ${EXTRA_RUSTFLAGS}" \
    cargo build --profile ${RUST_PROFILE} \
        --bin tempo-xtask

# Solidity ref-impls compiled for shared runtimes, routers, and zone genesis artifacts.
# Requires the specs/ref-impls/lib submodules to be checked out.
FROM debian:bookworm-slim@sha256:4724b8cc51e33e398f0e2e15e18d5ec2851ff0c2280647e1310bc1642182655d AS solidity
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*
ARG FOUNDRY_CACHE_BUST=2026-07-30
RUN echo "Foundry cache bust: ${FOUNDRY_CACHE_BUST}" \
    && curl -fsSL https://foundry.paradigm.xyz | bash \
    && /root/.foundry/bin/foundryup
ENV PATH="/root/.foundry/bin:${PATH}"
WORKDIR /app/specs/ref-impls
COPY specs/ref-impls .
RUN forge build --skip test

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

# tempo-zone-xtask: zone provisioning tooling (create-zone, zone-info, deploy-router).
# Ships the compiled ref-impls artifacts used by provisioning and router deployment.
FROM base AS tempo-zone-xtask
ARG RUST_PROFILE=profiling
RUN apt-get update && apt-get install -y --no-install-recommends \
    jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/${RUST_PROFILE}/tempo-xtask /usr/local/bin/tempo-xtask
COPY --from=solidity /root/.foundry/bin/cast /usr/local/bin/cast
COPY --from=solidity /app/specs/ref-impls/out /app/specs/ref-impls/out
WORKDIR /app
ENTRYPOINT ["/usr/local/bin/tempo-xtask"]
