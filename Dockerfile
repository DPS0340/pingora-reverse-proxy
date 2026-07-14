# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

FROM rust:1.85-bookworm@sha256:e51d0265072d2d9d5d320f6a44dde6b9ef13653b035098febd68cce8fa7c0bc4 AS builder

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        clang cmake libclang-dev \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_TOOLCHAIN=1.85.1

WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY vendor ./vendor
COPY src ./src
RUN --mount=type=cache,id=pingora-release-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-release-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=pingora-release-target,target=/build/target,sharing=locked \
    cargo build --locked --release \
    && install -D -m 0755 target/release/pingora-reverse-proxy /out/pingora-reverse-proxy \
    && strip /out/pingora-reverse-proxy

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818 AS runtime

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && find / -xdev -type d -perm -0002 ! -path /tmp -exec chmod o-w {} + \
    && find / -xdev -type f -perm -0002 -exec chmod o-w {} + \
    && rm -rf \
        /usr/local/src /usr/src \
        /usr/share/doc/bash /usr/share/menu/bash \
        /usr/share/debianutils/shells.d/bash \
    && rm -f /usr/bin/bash /usr/bin/dash /usr/bin/sh

COPY --from=builder /out/pingora-reverse-proxy /usr/local/bin/pingora-reverse-proxy

ENV HOME=/tmp \
    TMPDIR=/tmp

EXPOSE 8000 8001 8002
USER 65532:65532
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/pingora-reverse-proxy"]
CMD ["--ip","0.0.0.0","--port","8000","--api-ip","0.0.0.0","--api-port","8001","--metrics-ip","0.0.0.0","--metrics-port","8002"]
