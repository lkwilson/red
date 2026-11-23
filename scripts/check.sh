#!/usr/bin/env bash
# One workspace preserves dependencies and compilation artifacts between stages.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
bash scripts/format.sh
bash scripts/lint.sh
bash scripts/test.sh
export GATE_TESTS_PASSED=true
bash scripts/build.sh
