#!/usr/bin/env bash
# Production build/publication only. No formatter, linter or test rerun.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
source scripts/docker.sh
tag=${1:?Usage: release.sh EXISTING_GIT_TAG}
[[ $tag =~ ^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
  echo 'Expected x.y.z (legacy vx.y.z accepted)' >&2; exit 1;
}
version=${tag#v}
test "$(git rev-parse HEAD)" = "$(git rev-parse "refs/tags/$tag^{commit}")"
git diff --quiet HEAD --
image="zot.endsy.me/library/$APP_NAME"
if docker buildx imagetools inspect "$image:$version" >/dev/null 2>&1; then
  echo "Release $image:$version already exists. Changed inputs need a new version." >&2
  exit 1
fi
sources=()
for platform in "${IMAGE_PLATFORMS[@]}"; do
  case "$platform" in linux/amd64|linux/arm64) ;; *) echo "Unsupported platform: $platform" >&2; exit 1 ;; esac
  arch=${platform#linux/}
  docker buildx build --platform "$platform" "${image_args[@]}" \
    --build-arg "APP_VERSION=$version" \
    --tag "$image:$version-$arch" --push .
  sources+=("$image:$version-$arch")
done
docker buildx imagetools create --tag "$image:$version" "${sources[@]}"
# Metadata validation is publication verification, not a source check rerun.
docker buildx imagetools inspect --raw "$image:$version" | python3 -c '
import json, sys
index = json.load(sys.stdin)
actual = {m.get("platform", {}).get("os", "") + "/" + m.get("platform", {}).get("architecture", "") for m in index.get("manifests", [])}
expected = set(sys.argv[1:])
if not expected <= actual:
    raise SystemExit(f"Missing release platforms: {expected - actual}")
' "${IMAGE_PLATFORMS[@]}"
