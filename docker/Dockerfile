# Dev toolchain image: stable Rust plus clippy/rustfmt and the native deps photo-stamp uses.
FROM rust:1-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libheif-dev libheif-plugin-x265 libheif-plugin-libde265 \
        libimage-exiftool-perl fonts-dejavu-core fonts-noto-core fonts-noto-cjk imagemagick \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy rustfmt
