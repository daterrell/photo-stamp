#!/usr/bin/env bash
# Download files of the newest published place data (photo-stamp-places/<YYYY.MM>, built monthly
# by dave/photo-stamp-places) into a folder:
#   scripts/fetch-places.sh data towns.sqlite [places.sqlite]
# Needs a Forgejo token that can read packages: $TOKEN, else ~/.config/forgejo/token.
set -euo pipefail
dest=$1
shift
token=${TOKEN:-$(cat "$HOME/.config/forgejo/token" 2>/dev/null || true)}
[ -n "$token" ] || { echo "fetch-places: set TOKEN (a Forgejo token that can read packages)" >&2; exit 1; }
server=${GITHUB_SERVER_URL:-https://forgejo.terrell.ltd}
owner=${GITHUB_REPOSITORY_OWNER:-dave}
package=photo-stamp-places
version=$(curl -sf -H "Authorization: token $token" "$server/api/v1/packages/$owner?type=generic&q=$package&limit=50" \
  | jq -r --arg p "$package" '[.[] | select(.name == $p) | .version] | sort | last // empty')
[ -n "$version" ] || { echo "fetch-places: no $package published yet" >&2; exit 1; }
mkdir -p "$dest"
for f in "$@"; do
  curl -sf -H "Authorization: token $token" -o "$dest/$f.part" "$server/api/packages/$owner/generic/$package/$version/$f" \
    || { echo "fetch-places: couldn't download $f of $package $version" >&2; exit 1; }
  mv "$dest/$f.part" "$dest/$f"
done
echo "fetched $package $version: $*"
