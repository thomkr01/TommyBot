# Build with `docker compose build`. Pick the image with --target: bot, dashboard or updater.

FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p tommybot -p tommybot-dashboard

FROM rust:1-bookworm AS ui
RUN rustup target add wasm32-unknown-unknown \
 && cargo install trunk --version 0.21.14 --locked
WORKDIR /src/crates/dashboard/ui
COPY assets /src/assets
COPY crates/dashboard/ui ./
# Trunk's prebuilt wasm-bindgen needs glibc 2.38 on arm64 (bookworm has 2.36), so build the CLI that matches the locked crate.
RUN cargo generate-lockfile \
 && cargo install wasm-bindgen-cli --locked --version "$(cargo pkgid wasm-bindgen | sed 's/.*@//')" \
 && trunk build --release

FROM debian:bookworm-slim AS base
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV RUST_LOG=info
WORKDIR /app
# The commit the image was built from; the dashboard compares it with the newest build.
ARG GIT_SHA=dev
LABEL org.opencontainers.image.revision=$GIT_SHA
ENV TOMMYBOT_SHA=$GIT_SHA

FROM base AS bot
COPY --from=builder /src/target/release/tommybot /usr/local/bin/tommybot
CMD ["tommybot"]

FROM base AS dashboard
COPY --from=builder /src/target/release/tommybot-dashboard /usr/local/bin/tommybot-dashboard
COPY --from=ui /src/crates/dashboard/ui/dist /app/ui/dist
ENV DASHBOARD_DIST=/app/ui/dist BIND=0.0.0.0:8080
EXPOSE 8080
CMD ["tommybot-dashboard"]

# Carries out updates started from the dashboard; see scripts/updater.sh.
# psql from the Postgres image, the Docker CLI and compose plugin from Docker's: no package installs.
FROM postgres:16-alpine AS updater
COPY --from=docker:28-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker:28-cli /usr/local/libexec/docker/cli-plugins/docker-compose /usr/local/libexec/docker/cli-plugins/docker-compose
COPY scripts/updater.sh /usr/local/bin/updater
ENTRYPOINT ["updater"]
