# Build stage
FROM rust:1.82-slim as builder

WORKDIR /app

# Copy manifests first for better caching
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release 2>/dev/null || true && \
    rm -rf src

# Copy actual source
COPY src ./src

# Build the application
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary
COPY --from=builder /app/target/release/apy-mcp /usr/local/bin/apy-mcp

# Create non-root user
RUN useradd -m -s /bin/bash mcp
USER mcp

# Expose port
EXPOSE 3000

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:3000/health || exit 1

# Default to HTTP mode
ENTRYPOINT ["apy-mcp"]
CMD ["http", "--addr", "0.0.0.0:3000"]
