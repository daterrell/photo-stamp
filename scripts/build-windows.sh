#!/usr/bin/env bash
# Build the Windows release folder and zip:
#
#   dist/photo-stamp-windows/
#     photo-stamp.exe (watches the drop folder, serves the dashboard, tray icon)
#     photo-stamp-files/exiftool.exe, photo-stamp-files/exiftool_files/, photo-stamp-files/towns.sqlite
#     README.txt
#   dist/photo-stamp-windows.zip   (what auto-update installs)
#   dist/photo-stamp-setup.exe     (what people download: installer/photo-stamp.nsi)
#
# Needs only Docker (makensis is used directly if it's installed). The exe is cross-compiled with HEIC support linked in statically.
# Set PHOTO_STAMP_EXE to package an exe that was already built, PHOTO_STAMP_TOWNS_DB for the towns database
# (default data/towns.sqlite, downloaded with scripts/fetch-places.sh if missing).
set -euo pipefail
cd "$(dirname "$0")/.."

EXIFTOOL_VERSION=13.59
EXIFTOOL_SHA256=44b512b25af500724ba579d0a53c8fc5851628b692dd5e5d94ae4a15c2cba9ec
EXIFTOOL_URL="https://sourceforge.net/projects/exiftool/files/exiftool-${EXIFTOOL_VERSION}_64.zip/download"

DIST=dist/photo-stamp-windows
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/photo-stamp-dev/downloads"
mkdir -p "$CACHE"

rm -rf "$DIST" dist/photo-stamp-windows.zip
mkdir -p "$DIST/photo-stamp-files"

if [ -n "${PHOTO_STAMP_EXE:-}" ]; then
  # Already built (CI builds it with a layer cache, see .forgejo/workflows/ci.yml).
  cp "$PHOTO_STAMP_EXE" "$DIST/photo-stamp.exe"
else
  echo "building photo-stamp.exe…"
  docker build -q --target build -t photo-stamp-win-build -f docker/Dockerfile.windows \
    --build-arg PHOTO_STAMP_VERSION="$(scripts/version.sh)" . >/dev/null
  container=$(docker create photo-stamp-win-build)
  trap 'docker rm -f "$container" >/dev/null' EXIT
  docker cp "$container:/src/target/x86_64-pc-windows-gnu/release/photo-stamp.exe" "$DIST/photo-stamp.exe"
fi

zip="$CACHE/exiftool-${EXIFTOOL_VERSION}_64.zip"
if [ ! -f "$zip" ]; then
  echo "downloading exiftool ${EXIFTOOL_VERSION}…"
  curl -fsSL -o "$zip.part" "$EXIFTOOL_URL"
  mv "$zip.part" "$zip"
fi
echo "$EXIFTOOL_SHA256  $zip" | sha256sum -c --quiet -

# The zip holds exiftool-<ver>_64/{exiftool(-k).exe, exiftool_files/}; "(-k)" makes it pause for a
# keypress, so it's renamed to plain exiftool.exe.
python3 - "$zip" "$DIST/photo-stamp-files" <<'EOF'
import sys, zipfile, os
src, dest = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(src) as z:
    for info in z.infolist():
        parts = info.filename.split("/", 1)
        if len(parts) < 2 or not parts[1] or info.is_dir():
            continue
        rel = parts[1].replace("exiftool(-k).exe", "exiftool.exe")
        out = os.path.join(dest, rel)
        os.makedirs(os.path.dirname(out), exist_ok=True)
        with z.open(info) as r, open(out, "wb") as w:
            w.write(r.read())
EOF

# Bundled place data: the small towns database (towns, regions, countries); the place server has the rest.
towns="${PHOTO_STAMP_TOWNS_DB:-data/towns.sqlite}"
[ -f "$towns" ] || scripts/fetch-places.sh "$towns"
cp "$towns" "$DIST/photo-stamp-files/towns.sqlite"

cp scripts/windows-README.txt "$DIST/README.txt"
(cd dist && python3 -m zipfile -c photo-stamp-windows.zip photo-stamp-windows/)

# The setup program. Windows wants a four-number version for its file properties: 0.7.1-dev.3+g1b37dbe → 0.7.1.0.
version=$(scripts/version.sh)
numeric=$(echo "$version" | sed -E 's/^([0-9]+)\.([0-9]+)\.([0-9]+).*/\1.\2.\3.0/')
nsis_args=(-V2 -WX "-DVERSION=$version" "-DVIVERSION=$numeric" "-DDIST=../$DIST" "-DOUTFILE=../dist/photo-stamp-setup.exe" installer/photo-stamp.nsi)
rm -f dist/photo-stamp-setup.exe
if command -v makensis >/dev/null; then
  makensis "${nsis_args[@]}"
else
  docker build -q -t photo-stamp-nsis - >/dev/null <<'DOCKERFILE'
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends nsis && rm -rf /var/lib/apt/lists/*
DOCKERFILE
  docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/w" -w /w photo-stamp-nsis makensis "${nsis_args[@]}"
fi
echo "built $DIST ($(du -sh "$DIST" | cut -f1)), dist/photo-stamp-windows.zip and dist/photo-stamp-setup.exe ($(du -sh dist/photo-stamp-setup.exe | cut -f1))"
