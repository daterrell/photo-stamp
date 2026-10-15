#!/usr/bin/env bash
# Make the Windows icons (exe, tray, setup program, Settings > Apps) from the dashboard's logo
# (web/icon.svg: a postmark), drawn white on a filled tile so it shows on light and dark taskbars.
# Blue while running, gray while paused. Needs ImageMagick 7; run in the dev image:
#   ./dev.sh bash scripts/make-icons.sh
# The results are committed in assets/.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# The logo's own drawing (32×32 units), scaled up.
waves='M1 13c4-3 7 3 11 0s7 3 11 0 7 3 9 1M1 19c4-3 7 3 11 0s7 3 11 0 7 3 9 1'
draw() { # color output.png
  magick -size 256x256 xc:none \
    -draw "scale 8,8 fill '$1' stroke none roundrectangle 0,0 31.9,31.9 6,6" \
    -fill none -stroke white \
    -draw "scale 8,8 stroke-width 2.6 circle 16,16 28.2,16" \
    -draw "scale 8,8 stroke-width 1.7 circle 16,16 23.8,16" \
    -draw "scale 8,8 stroke-width 1.9 path '$waves'" \
    "$2"
}
draw '#3346C8' "$tmp/run.png"
draw '#6B7280' "$tmp/paused.png"
mkdir -p assets
for kind in run paused; do
  out=assets/photo-stamp.ico
  [ "$kind" = paused ] && out=assets/photo-stamp-paused.ico
  magick "$tmp/$kind.png" -define icon:auto-resize=256,64,48,40,32,24,20,16 "$out"
done
cp "$tmp/run.png" assets/photo-stamp-256.png
ls -la assets/
