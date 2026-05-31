#!/usr/bin/env bash
set -euo pipefail

echo "=== cargo fmt ==="
cargo fmt --check 2>&1

echo "=== cargo clippy ==="
cargo clippy --all-targets -- -D warnings 2>&1

echo "=== cargo audit ==="
cargo audit 2>&1

echo "=== cargo deny ==="
cargo deny check 2>&1

echo "=== cargo test ==="
cargo test 2>&1

echo "=== build release ==="
cargo build --release 2>&1

echo "=== integration tests ==="
python3 tests/fixtures/build_test_db.py
python3 tests/mcp_integration.py

echo ""
echo "All checks passed."
