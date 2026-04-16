# syntax=docker/dockerfile:1.7

FROM rust:1.94-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml ./
COPY .cargo ./.cargo
COPY crates ./crates
COPY apps ./apps

RUN cargo build --release -p polyhft-live-agent -p polyhft-replay-cli -p polyhft-dashboard

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        tzdata \
        dumb-init \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 polyhft

COPY --from=builder /app/target/release/polyhft-live-agent /usr/local/bin/polyhft-live-agent
COPY --from=builder /app/target/release/polyhft-replay-cli /usr/local/bin/polyhft-replay-cli
COPY --from=builder /app/target/release/polyhft-dashboard /usr/local/bin/polyhft-dashboard
COPY config.toml /etc/polyhft/config.toml

USER polyhft
WORKDIR /home/polyhft

ENTRYPOINT ["/usr/bin/dumb-init", "--"]
CMD ["/usr/local/bin/polyhft-live-agent", "--config", "/etc/polyhft/config.toml"]
