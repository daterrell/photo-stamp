#!/usr/bin/env bash
# Run a command inside the Rust dev container, e.g. `./dev.sh cargo test`.
# Builds the image on first use; caches cargo registry and target dir on the host.
set -euo pipefail
cd "$(dirname "$0")"
IMAGE=photo-stamp-dev:$(sha256sum docker/Dockerfile | cut -c1-12)
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" -f docker/Dockerfile docker >/dev/null
fi
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/photo-stamp-dev"
mkdir -p "$CACHE/cargo"
TTY=(); [ -t 0 ] && TTY=(-it)
exec docker run --rm "${TTY[@]}" \
  -u "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo-home -e HOME=/tmp \
  -v "$CACHE/cargo:/cargo-home" \
  -v "$PWD:/work" -w /work \
  "$IMAGE" "${@:-bash}"
