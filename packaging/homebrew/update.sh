#!/usr/bin/env bash
# Writes Formula/crossdrop.rb and Casks/crossdrop-app.rb for a release.
#
#   packaging/homebrew/update.sh v0.4.0 path/to/downloaded/assets
#
# The asset folder must hold the release's
#   crossdrop-cli-<tag>-universal-apple-darwin.tar.gz
#   crossdrop-cli-<tag>-x86_64-unknown-linux-gnu.tar.gz
#   crossdrop-cli-<tag>-aarch64-unknown-linux-gnu.tar.gz
#   crossdrop_<version>_universal.dmg
set -euo pipefail

tag="$1"
assets="$2"
version="${tag#v}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

sha() {
  local f="$assets/$1"
  [ -f "$f" ] || { echo "missing asset: $1" >&2; exit 1; }
  if command -v sha256sum >/dev/null; then sha256sum "$f" | cut -d' ' -f1; else shasum -a 256 "$f" | cut -d' ' -f1; fi
}

sha_mac=$(sha "crossdrop-cli-$tag-universal-apple-darwin.tar.gz")
sha_linux=$(sha "crossdrop-cli-$tag-x86_64-unknown-linux-gnu.tar.gz")
sha_linux_arm=$(sha "crossdrop-cli-$tag-aarch64-unknown-linux-gnu.tar.gz")
sha_dmg=$(sha "crossdrop_${version}_universal.dmg")

render() {
  sed -e "s/@VERSION@/$version/g" \
      -e "s/@SHA_MAC@/$sha_mac/g" \
      -e "s/@SHA_LINUX@/$sha_linux/g" \
      -e "s/@SHA_LINUX_ARM@/$sha_linux_arm/g" \
      -e "s/@SHA_DMG@/$sha_dmg/g" "$1"
}

mkdir -p "$root/Formula" "$root/Casks"
render "$here/crossdrop.rb.in" > "$root/Formula/crossdrop.rb"
render "$here/crossdrop-app.rb.in" > "$root/Casks/crossdrop-app.rb"
echo "Formula/crossdrop.rb and Casks/crossdrop-app.rb updated to $version"
