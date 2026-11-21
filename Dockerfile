# Build stage
# sqlx 0.9 needs Rust 1.94 or newer. This matches the toolchain CI runs; raise both together.
# bookworm here so the binary links the same glibc and OpenSSL the runtime stage ships.
FROM rust:1.100-slim-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /usr/src/msync

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Build dependencies (this is cached as long as Cargo.toml doesn't change)
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release --locked && \
    rm -rf src

# Copy source code
COPY build.rs ./
COPY src ./src
COPY migrations ./migrations
# msync init embeds the templates (include_str!), so they are needed at build time.
COPY templates ./templates
# Example CRAs and one doc are embedded the same way.
COPY examples ./examples
COPY docs ./docs

# Build application. The touch makes cargo see the real sources as newer than the stub above.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies. curl is what the HEALTHCHECK below runs.
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 msync

# Copy binary from builder
COPY --from=builder /usr/src/msync/target/release/msync /usr/local/bin/msync

# Copy migrations
COPY --from=builder /usr/src/msync/migrations /app/migrations

# Set working directory
WORKDIR /app

# Change ownership
RUN chown -R msync:msync /app

# Switch to non-root user
USER msync

# Expose port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Run the binary
CMD ["msync"]
