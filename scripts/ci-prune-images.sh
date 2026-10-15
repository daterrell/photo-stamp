#!/usr/bin/env bash
# Remove all but the newest $KEEP_BUILDS `sha-*` tags of a container image in Forgejo's registry
# (version tags, :main and :latest are kept). Used by CI after pushing an image.
#   TOKEN=<package write token> scripts/ci-prune-images.sh photo-stamp
set -euo pipefail
name=$1
keep=${KEEP_BUILDS:-10}
server=${GITHUB_SERVER_URL:-https://forgejo.terrell.ltd}
owner=${GITHUB_REPOSITORY_OWNER:-dave}
api="$server/api/v1/packages/$owner"
curl -sf -H "Authorization: token $TOKEN" "$api?type=container&q=$name&limit=200" \
  | jq -r --arg n "$name" --argjson keep "$keep" \
      '[.[] | select(.name == $n and (.version | startswith("sha-")))] | sort_by(.created_at) | reverse | .[$keep:] | .[].version' \
  | while read -r v; do
      curl -sf -X DELETE -H "Authorization: token $TOKEN" "$api/container/$name/$v" && echo "removed $name:$v"
    done
