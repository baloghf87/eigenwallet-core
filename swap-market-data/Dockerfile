# Builds the `market-data` binary: a headless collector that serves the current
# XMR/BTC network orderbook snapshot over REST.
#
# NOTE: `swap-market-data` depends on `swap-p2p`, which transitively pulls in
# `monero-sys` (a C++ build). That is why this image mirrors `swap-asb/Dockerfile`
# (Ubuntu + monero system deps + cargo-chef) rather than the lean
# `libp2p-rendezvous-node/Dockerfile`. The build context must include the
# `monero-sys` git submodules (checkout with `--recursive`).

# Latest Ubuntu 24.04 image as of Tue, 05 Aug 2025 15:34:08 GMT
FROM ubuntu:24.04@sha256:a08e551cb33850e4740772b38217fc1796a66da2506d312abe51acda354ff061 AS toolchain

# Install system dependencies required by monero-sys
# See .github/actions/set-monero-env/action.yml for required dependencies
ENV DEBIAN_FRONTEND=noninteractive
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && \
    apt-get install -y --allow-change-held-packages \
    autoconf \
    build-essential \
    pkg-config \
    libtool \
    ccache \
    make \
    cmake \
    gcc \
    g++ \
    git \
    curl \
    lbzip2 \
    gperf \
    libglib2.0-dev \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# See ./rust-toolchain.toml for the Rust version
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN set -eu; \
    RUST_VERSION=$(awk -F\" '/^[[:space:]]*channel[[:space:]]*=/ {print $2; exit}' /tmp/rust-toolchain.toml); \
    echo "Using Rust $RUST_VERSION"; \
    curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain "$RUST_VERSION" --profile complete

ENV PATH="/root/.cargo/bin:${PATH}"

# cargo-chef caches the dependency graph
RUN cargo install cargo-chef --locked

# Probe that the monero submodule is present in the build context
FROM toolchain AS prober
RUN --mount=type=bind,source=.,target=/src,ro \
    test -f /src/monero-sys/monero/CMakeLists.txt

# Analyze the workspace to compute the dependency recipe
FROM toolchain AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build
FROM toolchain AS builder

# Enable debug info in release builds for stack traces and perf.
ENV CARGO_PROFILE_RELEASE_DEBUG=1

COPY --from=planner /recipe.json recipe.json

# Build just the dependencies of swap-market-data first (cached layer)
RUN --mount=type=cache,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,target=/root/.cargo/git,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json --package swap-market-data

# Act as if we are in a GitHub Actions environment (monero build expects this)
ENV DOCKER_BUILD=true

COPY . .

RUN cargo build --release --locked -p swap-market-data --bin market-data

# Runtime image
FROM ubuntu:24.04@sha256:a08e551cb33850e4740772b38217fc1796a66da2506d312abe51acda354ff061 AS runner

ENV DEBIAN_FRONTEND=noninteractive
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && \
    apt-get install -y ca-certificates && \
    update-ca-certificates

COPY --from=builder /target/release/market-data /bin/market-data

EXPOSE 8080

ENTRYPOINT ["market-data"]
