# Build with `docker compose build`. Pick the image with --target: bot or dashboard.

FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p tommybot -p tommybot-dashboard

FROM rust:1-bookworm AS ui
RUN rustup target add wasm32-unknown-unknown \
 && cargo install trunk --version 0.21.14 --locked
WORKDIR /src/crates/dashboard/ui
COPY crates/dashboard/ui ./
RUN trunk build --release

FROM debian:bookworm-slim AS base
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV RUST_LOG=info
WORKDIR /app

FROM base AS bot
COPY --from=builder /src/target/release/tommybot /usr/local/bin/tommybot
CMD ["tommybot"]

FROM base AS dashboard
COPY --from=builder /src/target/release/tommybot-dashboard /usr/local/bin/tommybot-dashboard
COPY --from=ui /src/crates/dashboard/ui/dist /app/ui/dist
ENV DASHBOARD_DIST=/app/ui/dist BIND=0.0.0.0:8080
EXPOSE 8080
CMD ["tommybot-dashboard"]
