# Multi-stage build for minimal final image

# Build stage
FROM rust:1.99-slim AS builder

WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
# Cargo validates all manifest targets, so benches must exist too
COPY benches ./benches

# Build for release
RUN cargo build --release --example hello_world --example full_api --example microservice

# Runtime stage
FROM debian:bookworm-slim

# Install CA certificates for HTTPS
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Copy the built examples
COPY --from=builder /app/target/release/examples/hello_world /usr/local/bin/hello_world
COPY --from=builder /app/target/release/examples/full_api /usr/local/bin/full_api
COPY --from=builder /app/target/release/examples/microservice /usr/local/bin/microservice

# Expose port
EXPOSE 3000

# microservice binds 0.0.0.0:$PORT (default 3000), unlike the loopback-only examples
ENV PORT=3000
CMD ["microservice"]
