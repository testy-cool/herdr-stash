#!/bin/sh
# Fetch the released binary for this platform, or build it from source.
#
# Runs at `herdr plugin install` time, after the confirmation preview and before
# Herdr registers the plugin. It must leave an executable at ./bin/herdr-stash,
# because that is the path every entry in herdr-plugin.toml runs.
#
# Downloading is the default so a normal install needs no Rust toolchain. The
# release assets are public, so there is no credential in this path at all.
set -eu

REPO="victor-software-house/herdr-stash"
BINARY="herdr-stash"

# The manifest and Cargo.toml carry the same version, and the release tag is
# v<version>. Reading it here rather than hardcoding is what keeps an install of
# an older tag fetching that tag's asset.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
if [ -z "$VERSION" ]; then
  echo "could not read version from Cargo.toml" >&2
  exit 1
fi

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
  Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
  # Anything else has no asset. Not an error yet: cargo may still be present.
  *) TARGET="" ;;
esac

mkdir -p bin

if [ -n "$TARGET" ]; then
  URL="https://github.com/$REPO/releases/download/v$VERSION/$BINARY-$TARGET"
  # -f matters: without it a 404's HTML body lands in bin/ and gets marked
  # executable, and the failure surfaces later as a plugin that does nothing.
  if curl -fsSL --retry 2 -o "bin/$BINARY" "$URL"; then
    chmod +x "bin/$BINARY"
    echo "fetched $BINARY $VERSION for $TARGET"
    exit 0
  fi
  echo "no release asset at $URL — falling back to building from source" >&2
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "no release asset for $(uname -s)-$(uname -m), and cargo is not installed" >&2
  echo "install Rust (https://rustup.rs) and reinstall this plugin" >&2
  exit 1
fi

# --locked so an install builds the dependency versions this release was tested
# with, rather than whatever resolves today.
cargo build --release --locked
cp "target/release/$BINARY" "bin/$BINARY"
echo "built $BINARY $VERSION from source"
