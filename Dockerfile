FROM rust:1.76.0 AS builder

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends clang cmake build-essential

WORKDIR /app

COPY . .

RUN cargo build --release

ENTRYPOINT ["/bin/bash", "-c", "./target/release/mutinynet-faucet-rs ${FLAGS}"]
