# Multi-stage build for model-switcher with vLLM
#
# This Dockerfile builds the model-switcher binary and packages it with vLLM,
# enabling zero-reload model switching for multiple models on shared GPU.

# =============================================================================
# Stage 1: Build the Rust binary
# =============================================================================
FROM rust:1.83-bookworm AS builder

WORKDIR /build

# Install dependencies for building
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy the workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build the model-switcher binary in release mode
RUN cargo build --release --package model-switcher --bin model-switcher

# =============================================================================
# Stage 2: Create the final image with vLLM
# =============================================================================
FROM vllm/vllm-openai:latest

# Copy the model-switcher binary from builder
COPY --from=builder /build/target/release/model-switcher /usr/local/bin/model-switcher

# Create config directory
RUN mkdir -p /etc/model-switcher

# Default config location
ENV MODEL_SWITCHER_CONFIG=/etc/model-switcher/config.json

# Expose the proxy port (default 3000) and metrics port (default 9090)
EXPOSE 3000 9090

# The model-switcher will spawn vLLM processes internally
# It expects a config file at $MODEL_SWITCHER_CONFIG
ENTRYPOINT ["/usr/local/bin/model-switcher"]
CMD ["--config", "/etc/model-switcher/config.json"]
