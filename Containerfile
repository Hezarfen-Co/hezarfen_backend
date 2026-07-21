# Build stage. Edition 2024 needs Rust >= 1.85; pinned to the current stable.
FROM docker.io/library/rust:1.97-slim AS builder

# utoipa-swagger-ui's build script downloads the Swagger UI bundle with curl.
RUN apt-get update && \
    apt-get install -y --no-install-recommends curl ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Cache mounts keep the registry and incremental build artifacts between
# builds. The binary must be copied out of /app/target inside the same RUN,
# because cache mounts are not part of the image layers.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked && \
    cp target/release/hezarfen_backend /usr/local/bin/hezarfen_backend

# Runtime stage. curl is included for the compose healthcheck.
FROM docker.io/library/debian:trixie-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends curl ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --uid 10001 hezarfen && \
    mkdir /data && chown hezarfen:hezarfen /data

COPY --from=builder /usr/local/bin/hezarfen_backend /usr/local/bin/hezarfen_backend

USER hezarfen

# HOST must be 0.0.0.0 so the port mapping can reach the listener. DB_URL
# points at the SurrealDB server (the compose service); /data holds uploads.
ENV HOST=0.0.0.0 \
    PORT=8080 \
    DB_URL=ws://surrealdb:8000

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/hezarfen_backend"]
