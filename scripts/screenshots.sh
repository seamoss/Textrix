#!/bin/sh
# Draws site/screenshots (in the README and on the website) from the real
# interface and made-up rooms: no account, no network. Run it after
# changing how anything looks, or adding a theme.
set -e
cd "$(dirname "$0")/.."
rm -f site/screenshots/*.svg site/screenshots/themes/*.svg
cargo test --locked -q -- --ignored --exact app::shots::screenshots --nocapture 2>&1 | grep -E '^wrote|panicked|error'
