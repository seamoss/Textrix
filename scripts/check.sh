#!/bin/sh
# What CI checks, to run before committing: formatting, clippy (warnings
# are errors) and the tests, with the lock file as committed.
set -e
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets --locked -q -- -D warnings
cargo test --locked -q 2>&1 | grep -E '^test result'
