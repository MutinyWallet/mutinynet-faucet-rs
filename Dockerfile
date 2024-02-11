# Build Stage
FROM rust:1.67.0 AS builder

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends clang cmake build-essential

WORKDIR /app

COPY . .

RUN cargo build --release

# Run Stage
FROM debian:buster-slim

# Install OpenSSL
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libssl1.1

WORKDIR /app

COPY --from=builder /app/target/release/mutinynet-faucet-rs /app/mutinynet-faucet-rs

ENTRYPOINT ["/bin/bash", "-c", "./mutinynet-faucet-rs ${FLAGS}"]