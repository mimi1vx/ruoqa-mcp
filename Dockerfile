# syntax=docker/dockerfile:1

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev cmake make perl clang-dev llvm-dev linux-headers g++
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked && \
    cp target/release/ruoqa-mcp /out
# Pre-owned so a NAMED volume mounted at /var/lib/ruoqa-mcp inherits write
# access for `nonroot` (uid 65532) instead of the root-owned mountpoint
# Docker would otherwise create. A BIND mount ignores this entirely and
# needs the host directory owned by 65532 directly -- see compose.yaml.
RUN install -d -m 0700 -o 65532 -g 65532 /audit

FROM gcr.io/distroless/static-debian13:nonroot
ARG VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/mimi1vx/ruoqa-mcp" \
      org.opencontainers.image.description="MCP server exposing curated, typed tools over the openQA REST API" \
      org.opencontainers.image.licenses="GPL-3.0-or-later" \
      org.opencontainers.image.version="${VERSION}"
COPY --from=build /out /usr/local/bin/ruoqa-mcp
COPY --from=build --chown=65532:65532 /audit /var/lib/ruoqa-mcp
ENV OPENQA_MCP_TRANSPORT=http \
    OPENQA_MCP_HOST=0.0.0.0 \
    OPENQA_MCP_PORT=8000 \
    HOME=/home/nonroot
EXPOSE 8000
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/ruoqa-mcp"]
