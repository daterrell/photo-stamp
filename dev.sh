#!/usr/bin/env bash
# Run a command inside the Rust dev container, e.g. `./dev.sh cargo test`.
# Builds the image on first use; caches cargo registry and target dir on the host.
set -euo pipefail
cd "$(dirname "$0")"
IMAGE=photo-stamp-dev:$(sha256sum docker/Dockerfile | cut -c1-12)
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" -f docker/Dockerfile docker >/dev/null
fi
# The dev image has no git: work the version out here (see scripts/version.sh).
VERSION=$(scripts/version.sh)
if [ -n "${DEV_SH_COPY_SOURCE:-}" ]; then
  # CI's Docker daemon (Forgejo's docker-in-docker) can't see this checkout, so bind mounts would
  # be empty. Copy the source in instead, and keep cargo's cache and the target dir in named
  # volumes, which persist on that daemon between runs.
  container=$(docker create -u "$(id -u):$(id -g)" \
    -e CARGO_HOME=/cargo-home -e HOME=/tmp -e CARGO_TARGET_DIR=/target -e PHOTO_STAMP_VERSION="$VERSION" \
    -v photo-stamp-dev-cargo:/cargo-home -v photo-stamp-dev-target:/target -w /work \
    "$IMAGE" "${@:-bash}")
  trap 'docker rm -f "$container" >/dev/null' EXIT
  tar -c --exclude=./target --transform='s,^\.,work,' . | docker cp - "$container:/"
  docker start -a "$container"
  exit
fi
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/photo-stamp-dev"
mkdir -p "$CACHE/cargo"
TTY=(); [ -t 0 ] && TTY=(-it)
exec docker run --rm "${TTY[@]}" \
  -u "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo-home -e HOME=/tmp -e PHOTO_STAMP_VERSION="$VERSION" \
  -v "$CACHE/cargo:/cargo-home" \
  -v "$PWD:/work" -w /work \
  "$IMAGE" "${@:-bash}"
