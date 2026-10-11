#!/usr/bin/env bash
# Build the Windows release folder and zip:
#
#   dist/photo-stamp-windows/
#     photo-stamp.exe
#     photo-stamp-files/exiftool.exe, photo-stamp-files/exiftool_files/
#     README.txt
#   dist/photo-stamp-windows.zip
#
# Needs only Docker. The exe is cross-compiled with HEIC support linked in statically.
set -euo pipefail
cd "$(dirname "$0")/.."

EXIFTOOL_VERSION=13.59
EXIFTOOL_SHA256=44b512b25af500724ba579d0a53c8fc5851628b692dd5e5d94ae4a15c2cba9ec
EXIFTOOL_URL="https://sourceforge.net/projects/exiftool/files/exiftool-${EXIFTOOL_VERSION}_64.zip/download"

DIST=dist/photo-stamp-windows
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/photo-stamp-dev/downloads"
mkdir -p "$CACHE"

echo "building photo-stamp.exe…"
docker build -q --target build -t photo-stamp-win-build -f docker/Dockerfile.windows . >/dev/null

rm -rf "$DIST" dist/photo-stamp-windows.zip
mkdir -p "$DIST/photo-stamp-files"
container=$(docker create photo-stamp-win-build)
trap 'docker rm -f "$container" >/dev/null' EXIT
docker cp "$container:/src/target/x86_64-pc-windows-gnu/release/photo-stamp.exe" "$DIST/photo-stamp.exe"

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

cp scripts/windows-README.txt "$DIST/README.txt"
(cd dist && python3 -m zipfile -c photo-stamp-windows.zip photo-stamp-windows/)
echo "built $DIST ($(du -sh "$DIST" | cut -f1)) and dist/photo-stamp-windows.zip"
