#!/usr/bin/env bash
# Download a file of the newest published place data (photo-stamp-places/<YYYY.MM>, built monthly
# by dave/photo-stamp-places) to OUT:
#   scripts/fetch-places.sh OUT [FILE]    (FILE defaults to towns.sqlite)
# Needs a Forgejo token that can read packages: $TOKEN, else ~/.config/forgejo/token.
set -euo pipefail
out=$1
file=${2:-towns.sqlite}
token=${TOKEN:-$(cat "$HOME/.config/forgejo/token" 2>/dev/null || true)}
[ -n "$token" ] || { echo "fetch-places: set TOKEN (a Forgejo token that can read packages)" >&2; exit 1; }
server=${GITHUB_SERVER_URL:-https://forgejo.terrell.ltd}
# The producer's owner, not this repo's.
owner=dave
package=photo-stamp-places
version=$(curl -sf -H "Authorization: token $token" "$server/api/v1/packages/$owner?type=generic&q=$package&limit=50" \
  | jq -r --arg p "$package" '[.[] | select(.name == $p) | .version] | sort | last // empty')
[ -n "$version" ] || { echo "fetch-places: no $package published yet" >&2; exit 1; }
mkdir -p "$(dirname "$out")"
curl -sf -H "Authorization: token $token" -o "$out.part" "$server/api/packages/$owner/generic/$package/$version/$file" \
  || { echo "fetch-places: couldn't download $file of $package $version" >&2; exit 1; }
mv "$out.part" "$out"
echo "fetched $file of $package $version to $out"
