#!/bin/sh
# What CI checks, to run before committing: formatting, clippy (warnings
# are errors) and the tests, with the lock file as committed.
set -e
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets --locked -q -- -D warnings
# Not piped straight into grep: that would pass on grep's success, even
# when tests fail (it let a failing test through once).
out=$(cargo test --locked -q 2>&1) || { printf '%s\n' "$out"; exit 1; }
printf '%s\n' "$out" | grep -E '^test result'
