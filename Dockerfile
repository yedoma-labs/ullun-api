# Multi-stage build for minimal final image

# Build stage
FROM rust:1.96-slim as builder

WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples

# Build for release
RUN cargo build --release --examples

# Runtime stage
FROM debian:bookworm-slim

# Install CA certificates for HTTPS
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Copy the built examples
COPY --from=builder /app/target/release/examples/hello_world /usr/local/bin/hello_world
COPY --from=builder /app/target/release/examples/full_api /usr/local/bin/full_api

# Expose port
EXPOSE 3000

# Default to hello_world example
CMD ["hello_world"]
