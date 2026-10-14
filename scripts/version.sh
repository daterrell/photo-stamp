#!/usr/bin/env bash
# Print the version of the checked-out commit, from git tags (the tag *is* the version):
#
#   on tag v0.7.0                      -> 0.7.0
#   7 commits after v0.6.0, at 1b37dbe -> 0.6.1-dev.7+g1b37dbe   (after 0.6.0, before 0.6.1)
#   uncommitted changes                -> ...+g1b37dbe.dirty
#   no tags / no git                   -> 0.0.0-dev
#
# Used by http/build.rs (which bakes it into every program) and by CI for Docker builds, which
# can't see .git. A PHOTO_STAMP_VERSION already in the environment wins.
set -euo pipefail
if [ -n "${PHOTO_STAMP_VERSION:-}" ]; then
  echo "${PHOTO_STAMP_VERSION#v}"
  exit 0
fi
cd "$(dirname "$0")/.."
desc=$(git describe --tags --match 'v[0-9]*.[0-9]*.[0-9]*' --long --dirty 2>/dev/null) || { echo "0.0.0-dev"; exit 0; }
# v<major>.<minor>.<patch>-<commits>-g<sha>[-dirty]
if [[ $desc =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)-([0-9]+)-(g[0-9a-f]+)(-dirty)?$ ]]; then
  major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]} patch=${BASH_REMATCH[3]}
  commits=${BASH_REMATCH[4]} sha=${BASH_REMATCH[5]} dirty=${BASH_REMATCH[6]:+.dirty}
  if [ "$commits" = 0 ] && [ -z "$dirty" ]; then
    echo "$major.$minor.$patch"
  else
    echo "$major.$minor.$((patch + 1))-dev.$commits+$sha$dirty"
  fi
else
  echo "0.0.0-dev"
fi
