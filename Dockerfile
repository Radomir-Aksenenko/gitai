# ==============================================================================
# Build stage
# ==============================================================================
FROM rust:bookworm AS builder

WORKDIR /build

# Copy manifest, source files, prompts and example config required at compile-time
COPY Cargo.toml Cargo.lock gitai.example.toml ./
COPY prompts ./prompts
COPY crates ./crates

# Build release binary
RUN cargo build --release --locked --package gitai-cli

# ==============================================================================
# Runtime stage
# ==============================================================================
FROM debian:bookworm-slim AS runtime

# Install runtime dependencies:
# - ca-certificates: TLS verification for HTTPS model APIs & git remotes
# - git: host-side checkout, clone, diff and branch management
# - docker.io: docker CLI for sandbox container management (DooD)
# - curl: container health checks and utilities
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
    docker.io \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /build/target/release/gitai /usr/local/bin/gitai

# Copy bundled prompts and config templates
COPY prompts /app/prompts
COPY deploy /app/deploy
COPY gitai.example.toml /app/gitai.example.toml
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Persistent data volume
VOLUME /data
ENV GITAI_DATA_DIR=/data
ENV GITAI_CONFIG=/data/gitai.toml

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -fs http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
