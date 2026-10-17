#!/bin/sh
# Print the Homebrew formula for a release.
#   packaging/homebrew/formula.sh <version> <dir with the release's .sha256 files>
# e.g. packaging/homebrew/formula.sh 0.1.0 dist > textrix.rb
set -eu
version="$1"
dir="$2"
template="$(dirname "$0")/textrix.rb.in"
sha() {
    file="$dir/textrix-v$version-$1.tar.gz.sha256"
    [ -f "$file" ] || { echo "missing $file" >&2; exit 1; }
    cut -d' ' -f1 "$file"
}
mac_arm=$(sha aarch64-apple-darwin)
mac_intel=$(sha x86_64-apple-darwin)
linux_intel=$(sha x86_64-unknown-linux-gnu)
sed -e "s/@VERSION@/$version/g" \
    -e "s/@SHA_MACOS_ARM@/$mac_arm/" \
    -e "s/@SHA_MACOS_INTEL@/$mac_intel/" \
    -e "s/@SHA_LINUX_INTEL@/$linux_intel/" \
    "$template"
