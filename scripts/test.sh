#!/usr/bin/env bash
# Full test suite: optimized tests without LTO (profile test-opt) and a fresh index cache.
#   scripts/test.sh                     # everything
#   scripts/test.sh --test php_refactor_cli_tests
set -euo pipefail
cd "$(dirname "$0")/.."
rm -rf target/test-cache
exec cargo test --profile test-opt --workspace "$@"
