FROM node:22-bookworm-slim AS node
FROM ubuntu:24.04
ARG RUST_VERSION
COPY --from=node /usr/local/ /usr/local/
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev curl ca-certificates python3 git vim zsh ripgrep \
    && curl -fsS https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain "$RUST_VERSION" \
       --component rustfmt --component clippy
ENV PATH="/root/.cargo/bin:${PATH}"
ENV SHELL=/bin/bash CARGO_TARGET_DIR=/tmp/theseus-target
WORKDIR /work/theseus-shell
