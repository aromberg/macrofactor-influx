# ── Build stage ──────────────────────────────────────────────────────────────
FROM rust:1.87-slim AS builder

WORKDIR /app

# Install build dependencies (pkg-config + OpenSSL headers required by openssl-sys).
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Cache dependencies by compiling a stub binary first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Now build the real source.
COPY src ./src
# Touch main.rs so cargo detects the change.
RUN touch src/main.rs && cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Install CA certificates (needed for HTTPS to Firebase/InfluxDB).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Install supercronic.
ADD https://github.com/aptible/supercronic/releases/download/v0.2.33/supercronic-linux-amd64 \
    /usr/local/bin/supercronic
RUN chmod +x /usr/local/bin/supercronic

COPY --from=builder /app/target/release/macrofactor-influx /usr/local/bin/app
COPY crontab /etc/crontab

ENTRYPOINT ["/usr/local/bin/supercronic", "/etc/crontab"]
