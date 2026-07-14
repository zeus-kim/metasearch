# Multi-stage build for the Orgos search engine.
#
# Build:
#   docker build -t orgos .
#
# Run:
#   docker run --rm -p 8889:8889 \
#     -v "$PWD/settings.yml:/app/settings.yml:ro" \
#     orgos
#
# Optional cargo features (Reddit/Marginalia engines, Redis cache):
#   docker build --build-arg FEATURES="reddit marginalia redis" -t orgos .

# -----------------------------------------------------------------------------
# Stage 1: Build
# -----------------------------------------------------------------------------
FROM rust:1.75-slim AS build

WORKDIR /src

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

ARG FEATURES=""

# Cache dependencies by building a dummy project first
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
RUN mkdir -p src/bin && \
    echo "fn main(){}" > src/main.rs && \
    echo "fn main(){}" > src/bin/ask.rs && \
    echo "fn main(){}" > src/bin/mcp.rs && \
    echo "" > src/lib.rs && \
    if [ -n "$FEATURES" ]; then \
        cargo build --release --features "$FEATURES" 2>/dev/null || true; \
    else \
        cargo build --release 2>/dev/null || true; \
    fi && \
    rm -rf src

# Copy actual source code and static files
COPY src ./src
COPY static ./static

# Build the actual binary
RUN if [ -n "$FEATURES" ]; then \
        cargo build --release --features "$FEATURES"; \
    else \
        cargo build --release; \
    fi

# -----------------------------------------------------------------------------
# Stage 2: Runtime
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim

# Install runtime dependencies and create non-root user
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app --create-home orgos

WORKDIR /app

# Copy binary from build stage
COPY --from=build /src/target/release/metasearch /usr/local/bin/metasearch

# Copy static files (HTML, CSS, JS, etc.)
COPY --from=build /src/static /app/static

# Copy default settings (can be overridden with volume mount)
COPY settings.yml /app/settings.yml

# Environment configuration
ENV METASEARCH_SETTINGS=/app/settings.yml \
    METASEARCH_LOG=info \
    METASEARCH_BIND=0.0.0.0 \
    METASEARCH_PORT=8889

# Switch to non-root user
USER orgos

# Expose the default port
EXPOSE 8889

# Health check using the built-in /health endpoint
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -sf http://127.0.0.1:8889/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/metasearch"]
