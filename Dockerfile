FROM docker:28-cli AS docker-cli
COPY scripts/docker-client-notices.py /tmp/docker-client-notices.py
RUN apk add --no-cache python3 \
    && python3 /tmp/docker-client-notices.py --self-test \
    && python3 /tmp/docker-client-notices.py --output /tmp/DOCKER-CLI-NOTICES.txt

FROM node:24-alpine AS dashboard-builder
WORKDIR /dashboard
COPY dashboard/package.json dashboard/package-lock.json ./
RUN npm ci
COPY dashboard/ ./
RUN npm run build

FROM rust:1.96-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY --from=dashboard-builder /dashboard/dist ./dashboard/dist
RUN rustup component add rust-docs \
    && cargo build --locked --release \
    && cp "$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html" /build/RUST-STANDARD-LIBRARY.html

FROM debian:bookworm-slim AS base
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git bash curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --home-dir /data ylcc \
    && mkdir -p /data && chown 10001:10001 /data
ENV HOME=/data YLCC_CONTAINERIZED=1
COPY LICENSE THIRD_PARTY_NOTICES.txt /usr/local/share/licenses/ylcc/
WORKDIR /data
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/ylcc"]
CMD ["--help"]

# Release CI supplies the already tested, native architecture binary.
FROM base AS runtime
COPY --chmod=755 dist/ylcc /usr/local/bin/ylcc
COPY dist/LICENSE dist/THIRD_PARTY_NOTICES.txt /usr/local/share/licenses/ylcc/

# Optional sibling-container control; contains a client, never a daemon.
FROM runtime AS runtime-client
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker-cli /tmp/DOCKER-CLI-NOTICES.txt /usr/local/share/licenses/docker-cli/

# The default target also supports a normal `docker build .` from source.
FROM base AS source
COPY --from=builder /build/target/release/ylcc /usr/local/bin/ylcc
COPY --from=builder /build/RUST-STANDARD-LIBRARY.html /usr/local/share/licenses/ylcc/
