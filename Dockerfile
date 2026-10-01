FROM rust:1.88.0 AS builder

# Install build dependencies
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    clang \
    cmake \
    build-essential \
    libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy manifests and lock file first for dependency caching
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies only
RUN mkdir src && echo 'fn main() {}' > src/main.rs

# Build dependencies (this layer is cached unless Cargo.toml/Cargo.lock change)
RUN cargo build --release --locked && rm -rf src

# Copy actual source code
COPY . .

# Touch main.rs so cargo knows it changed (dummy was cached)
RUN touch src/main.rs

# Build the application (only recompiles our crate, not dependencies)
RUN cargo build --release --locked


FROM debian:bookworm-slim

# Runtime dependencies: TLS for reqwest/LND clients, certificates for HTTPS
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app faucet \
    && mkdir -p /app \
    && chown faucet:faucet /app

COPY --from=builder /app/target/release/mutinynet-faucet-rs /usr/local/bin/mutinynet-faucet-rs

# Keep the historical working directory so existing /app volumes and the
# default relative SQLite paths continue to work across image upgrades.
WORKDIR /app
USER faucet

ENTRYPOINT ["/usr/local/bin/mutinynet-faucet-rs"]
