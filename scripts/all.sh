#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
# Prepare pinned tools/dependencies, then run every stage on this host.
bash scripts/setup.sh
exec bash scripts/check.sh
