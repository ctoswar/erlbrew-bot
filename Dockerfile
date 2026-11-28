# Build stage: compile the Rust binary
FROM rust:1-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy Cargo files first for layer caching
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build in release mode
RUN cargo build --release

# ==========================
# Runtime stage
# ==========================
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Data directory for the SQLite database (bind-mounted in docker-compose)
RUN mkdir -p /app/data

# Copy the compiled binary from the builder stage
COPY --from=builder /app/target/release/erlbrew-bot /usr/local/bin/bot

# Copy editable content (live-editable via bind-mount in docker-compose)
COPY faq.yaml knowledge.md ./

# Expose the port
EXPOSE 8000

# Health check
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=2 \
    CMD ["/usr/local/bin/bot", "--health-check"]

# Run the bot
CMD ["/usr/local/bin/bot"]
