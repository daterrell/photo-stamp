# photo-stamp: watches /photos/inbox and burns place + local time into new photos.

FROM rust:1-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libheif-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY assets assets
COPY src src
# Workspace members (only photo-stamp is built, but cargo needs them all to resolve).
COPY http http
COPY proto proto
COPY server server
COPY ui ui
RUN cargo build --release --locked -p photo-stamp

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        libheif1 libheif-plugin-libde265 libheif-plugin-x265 libheif-plugin-dav1d libheif-plugin-aomenc \
        libimage-exiftool-perl fonts-dejavu-core fonts-noto-core fonts-noto-cjk ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/photo-stamp /usr/local/bin/photo-stamp
# The small towns database (scripts/make-towns-db.py); set PHOTO_STAMP_SERVER_URL and
# PHOTO_STAMP_SERVER_KEY to name places with your place server.
COPY data/towns.sqlite /usr/share/photo-stamp/towns.sqlite
# World-writable so the container can run as any user (see `user:` in compose.yaml).
RUN mkdir -p /photos /cache && chmod 1777 /photos /cache
ENV XDG_CACHE_HOME=/cache HOME=/tmp
VOLUME ["/photos", "/cache"]
ENTRYPOINT ["photo-stamp"]
CMD ["watch", "/photos"]
