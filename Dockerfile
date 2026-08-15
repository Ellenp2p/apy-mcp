FROM rust:1.77-slim as builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/apy-mcp /usr/local/bin/

# Create data directory for SQLite
RUN mkdir -p /app/data
WORKDIR /app

EXPOSE 3000

CMD ["apy-mcp", "http", "--addr", "0.0.0.0:3000", "--pool-id", "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD"]
