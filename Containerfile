# syntax=docker/dockerfile:1
# Two images from one build: `--target runtime` and `--target session`.

FROM rust:1.99.1-trixie AS chef
ARG CARGO_CHEF_VERSION=0.1.78
RUN cargo install cargo-chef --locked --version ${CARGO_CHEF_VERSION}
WORKDIR /src

FROM chef AS plan
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Dependencies build in their own layer, which only changes with the manifests and lockfile, so CI's layer cache
# keeps it across commits.
FROM chef AS build
ARG CARGO_BUILD_JOBS=4
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked -p sloppod-runtime -p sloppod-supervisor \
    && mkdir /out && cp target/release/sloppod target/release/sloppod-supervisor /out/

FROM node:24.22.1-trixie-slim AS ui
WORKDIR /ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM debian:trixie-slim AS runtime-headless
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/sloppod /usr/local/bin/sloppod
USER 65532:65532
ENTRYPOINT ["sloppod"]

FROM runtime-headless AS runtime
COPY --from=ui /ui/build /usr/share/sloppod/ui

FROM debian:trixie-slim AS memini
ARG MEMINI_VERSION=v0.7.37
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git \
    && git clone --depth 1 --branch ${MEMINI_VERSION} https://github.com/eleboucher/memini /memini

FROM node:24.22.1-trixie-slim AS session
ARG MISE_VERSION=2026.10.18
ARG MISE_SHA256=e97c38973336aa2ff350fea50f47b412b266fe263b102b7200ff1e9b118c6942
ARG ADAPTER_VERSION=0.97.0
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git gh jq less procps ripgrep unzip xz-utils zstd \
        build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL -o /usr/local/bin/mise \
        https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-x64 \
    && echo "${MISE_SHA256}  /usr/local/bin/mise" | sha256sum -c - \
    && chmod +x /usr/local/bin/mise
RUN npm install -g @agentclientprotocol/claude-agent-acp@${ADAPTER_VERSION} && npm cache clean --force
COPY --from=memini /memini/plugin /opt/sloppod/plugins/memini
COPY --from=build /out/sloppod-supervisor /usr/local/bin/sloppod-supervisor
# The node image's `node` user is uid 1000, which session pods run as.
USER 1000:1000
WORKDIR /workspace
ENTRYPOINT ["sloppod-supervisor"]
