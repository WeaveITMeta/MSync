# Build stage
FROM rust:1.75-slim as builder

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
    cargo build --release && \
    rm -rf src

# Copy source code
COPY src ./src
COPY migrations ./migrations

# Build application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
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
