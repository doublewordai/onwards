# Build stage
FROM rust:1.92.0-slim AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
  pkg-config \
  libssl-dev \
  && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy source code and dependencies
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build the application
RUN cargo build --release

# Runtime stage - use Ubuntu for better compatibility
FROM ubuntu:22.04

# Copy the binary from builder stage
COPY --from=builder /app/target/release/onwards /app/onwards

# Set working directory
WORKDIR /app

# Expose port (adjust if your app uses a different port)
EXPOSE 3000

# Run the application
ENTRYPOINT ["./onwards"]
CMD []
