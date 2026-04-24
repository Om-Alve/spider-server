# syntax=docker/dockerfile:1.7

FROM rust:1.87-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/spider-server /usr/local/bin/spider-server

ENV HOST=0.0.0.0 \
    PORT=8080 \
    HTTP_CONCURRENCY_LIMIT=1024 \
    MAX_CONCURRENT_CRAWLS=8 \
    DEFAULT_MAX_DEPTH=2 \
    MAX_ALLOWED_DEPTH=6 \
    DEFAULT_MAX_PAGES=100 \
    MAX_ALLOWED_PAGES=5000 \
    DEFAULT_CRAWL_CONCURRENCY=16 \
    MAX_ALLOWED_CRAWL_CONCURRENCY=256 \
    DEFAULT_REQUEST_TIMEOUT_SECS=10 \
    MAX_REQUEST_TIMEOUT_SECS=60 \
    DEFAULT_CRAWL_TIMEOUT_SECS=30 \
    MAX_CRAWL_TIMEOUT_SECS=300 \
    REQUEST_BODY_LIMIT_MB=2 \
    DEFAULT_CONTENT_CHARS=4000 \
    MAX_CONTENT_CHARS=100000

EXPOSE 8080

CMD ["spider-server"]
