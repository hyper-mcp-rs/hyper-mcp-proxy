# ------ Builder Stage --------------
FROM rust:1.97 AS builder
WORKDIR /app
RUN cargo install cargo-auditable

COPY Cargo.toml Cargo.lock ./
RUN cargo fetch
COPY src ./src
# tests/ is copied so the `mock-mcp-child` [[bin]] target path declared in
# Cargo.toml resolves during manifest parsing. We restrict the build to the
# production binary so the mock isn't actually compiled into the image.
COPY tests ./tests
RUN cargo auditable build --release --locked --bin hyper-mcp-proxy

# ------- Cosign Stage ---------------

FROM ghcr.io/sigstore/cosign/cosign:v3.1.3 AS cosign

# ------- Production Stage -----------
FROM debian:13-slim

LABEL org.opencontainers.image.authors="joseph.wortmann@gmail.com" \
    org.opencontainers.image.url="https://github.com/hyper-mcp-rs/hyper-mcp-proxy" \
    org.opencontainers.image.source="https://github.com/hyper-mcp-rs/hyper-mcp-proxy" \
    org.opencontainers.image.vendor="github.com/hyper-mcp-rs/hyper-mcp-proxy" \
    io.modelcontextprotocol.server.name="io.github.hyper-mcp-rs/hyper-mcp-proxy"

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=cosign /ko-app/cosign /usr/local/bin/cosign

WORKDIR /app
COPY --from=builder /app/target/release/hyper-mcp-proxy /usr/local/bin/hyper-mcp-proxy
ENTRYPOINT ["/usr/local/bin/hyper-mcp-proxy"]
