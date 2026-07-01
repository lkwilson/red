#!/usr/bin/env bash
# Usage: bump-version.sh <patch|minor|major>
# Computes next x.y.z version (no v prefix), creates git tag, pushes it,
# and writes tag=<tag> to GITHUB_OUTPUT.
set -euo pipefail

BUMP="${1:-patch}"

latest=$(git tag -l '[0-9]*.[0-9]*.[0-9]*' | sort -V | tail -1)
if [ -z "$latest" ]; then
  tag="0.1.0"
else
  IFS='.' read -r maj min pat <<< "$latest"
  case "$BUMP" in
    major) tag="$((maj + 1)).0.0" ;;
    minor) tag="${maj}.$((min + 1)).0" ;;
    patch) tag="${maj}.${min}.$((pat + 1))" ;;
    *) echo "Unknown bump type: $BUMP" >&2; exit 1 ;;
  esac
fi

echo "Bumping $BUMP: ${latest:-none} -> ${tag}"

git config user.email "release-bot@endsy.me"
git config user.name "Release Bot"
git tag "$tag"
git push origin "$tag"

echo "tag=$tag" >> "$GITHUB_OUTPUT"
