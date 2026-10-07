#!/usr/bin/env bash
# Build the signed macOS binaries (arm64 + x64) for a given version. Produces
# metroctl-darwin-arm64 and metroctl-darwin-x64 at the repo root — the asset
# names the installer expects.
set -euo pipefail

VERSION="$1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Inline the version so the binary reports it (env!("CARGO_PKG_VERSION")).
sed -i.bak "s/^version = .*/version = \"${VERSION}\"/" Cargo.toml
rm -f Cargo.toml.bak

rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true

cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

cp target/aarch64-apple-darwin/release/metroctl metroctl-darwin-arm64
cp target/x86_64-apple-darwin/release/metroctl metroctl-darwin-x64

# Ad-hoc sign so Gatekeeper lets the binaries run (same approach as simon).
codesign --force --sign - metroctl-darwin-arm64
codesign --force --sign - metroctl-darwin-x64

echo "Built metroctl ${VERSION} (arm64 + x64)."
