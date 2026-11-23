#!/usr/bin/env bash
# Tag the checked-out merge commit; call from the serialized release workflow.
# Usage: bump-version.sh <patch|minor|major>
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
bump=${1:?Usage: bump-version.sh patch|minor|major}
case "$bump" in patch|minor|major) ;; *) echo "Unknown bump: $bump" >&2; exit 1 ;; esac

git fetch origin --tags
# Ignore snapshots/architecture tags; normalize legacy v-prefixed releases.
releases=$(git tag -l | sed -nE 's/^v?((0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*))$/\1/p' | sort -Vu)
latest=$(printf '%s\n' "$releases" | tail -1)

# A rerun must not allocate a second version for the same merge commit.
existing=$(git tag --points-at HEAD | sed -nE '/^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/p' | head -1)
if [[ -n "$existing" ]]; then
  git push origin "refs/tags/$existing"
  echo "Commit already tagged $existing; retry its image workflow if needed."
  echo "tag=$existing" >> "${GITHUB_OUTPUT:-/dev/stdout}"
  exit 0
fi

if [[ -z "$latest" ]]; then
  tag=0.1.0
else
  # Fail closed if merge events arrive out of order. Never tag an older commit
  # as a newer release. A new labelled merge can release the current main.
  for previous in "$latest" "v$latest"; do
    if git show-ref --verify --quiet "refs/tags/$previous"; then
      git merge-base --is-ancestor "refs/tags/$previous" HEAD || {
        echo "Newest release $previous is not an ancestor of this commit." >&2; exit 1;
      }
    fi
  done
  IFS=. read -r major minor patch <<< "$latest"
  case "$bump" in
    major) tag="$((major + 1)).0.0" ;;
    minor) tag="$major.$((minor + 1)).0" ;;
    patch) tag="$major.$minor.$((patch + 1))" ;;
  esac
fi
git tag "$tag" HEAD
git push origin "refs/tags/$tag"
echo "tag=$tag" >> "${GITHUB_OUTPUT:-/dev/stdout}"
