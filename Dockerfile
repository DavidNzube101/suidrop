# ---- build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependencies: copy manifests, build a dummy, then the real source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch so cargo rebuilds the real binary (not the cached dummy).
RUN touch src/main.rs && cargo build --release

# ---- runtime stage ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /app/target/release/suidrop /app/suidrop
COPY frontend ./frontend

ENV SUIDROP_PORT=8080
ENV SUIDROP_NETWORK=testnet
EXPOSE 8080

CMD ["/app/suidrop"]
