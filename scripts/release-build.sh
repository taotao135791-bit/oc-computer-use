#!/usr/bin/env bash
# Build the release artifacts into dist/:
#
#   dist/macos-arm64/   cu, cubridge  (arm64; `cu daemon run` IS the daemon)
#   dist/macos-x64/     cu, cubridge  (x86_64)
#   dist/universal/     cu, cubridge  (lipo'd fat binaries)
#   dist/npm/           tarballs of the TypeScript packages (pnpm pack)
#   dist/checksums.txt  sha256 of every artifact (relative paths)
#
# Prereqs: rustup targets aarch64-apple-darwin + x86_64-apple-darwin,
# swiftc (macOS), pnpm. Idempotent: dist/ is rebuilt from scratch.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
DIST="dist"
echo "== building $VERSION"
rm -rf "$DIST"
mkdir -p "$DIST/macos-arm64" "$DIST/macos-x64" "$DIST/universal" "$DIST/npm"

echo "== Rust release builds (aarch64 + x86_64)"
# cu-daemon is a library crate; the daemon runs in-process as `cu daemon run`,
# so the CLI binary is the only Rust artifact.
cargo build --release --target aarch64-apple-darwin -p cu-cli
cargo build --release --target x86_64-apple-darwin -p cu-cli

echo "== Swift bridge (aarch64 + x86_64, macOS 14+)"
SWIFT_SRC="crates/cu-driver-macos/swift/CUBridge/Sources/CUBridge/main.swift"
FRAMEWORKS="-framework ScreenCaptureKit -framework CoreGraphics -framework AppKit -framework ApplicationServices -framework CoreImage"
swiftc -O -target arm64-apple-macosx14.0 -o "$DIST/macos-arm64/cubridge" "$SWIFT_SRC" $FRAMEWORKS
swiftc -O -target x86_64-apple-macosx14.0 -o "$DIST/macos-x64/cubridge" "$SWIFT_SRC" $FRAMEWORKS

echo "== Assembling per-arch dirs"
cp target/aarch64-apple-darwin/release/cu "$DIST/macos-arm64/"
cp target/x86_64-apple-darwin/release/cu "$DIST/macos-x64/"

echo "== Universal fat binaries (lipo)"
for bin in cu cubridge; do
  lipo -create -output "$DIST/universal/$bin" "$DIST/macos-arm64/$bin" "$DIST/macos-x64/$bin"
done
lipo -info "$DIST/universal"/*

echo "== npm tarballs (packages must be built first)"
pnpm -r build
for pkg in packages/sdk-typescript packages/mcp-server packages/pi-extension packages/opencode-adapter; do
  (cd "$pkg" && pnpm pack --pack-destination "../../$DIST/npm" >/dev/null)
done
ls -1 "$DIST/npm"

echo "== checksums"
# Hash artifacts into a temp file, then rename: the file being written must
# not appear in its own listing (a tee'd file would be hashed while empty).
( cd "$DIST" && find . -type f ! -name "checksums.txt*" -print0 | sort -z | xargs -0 shasum -a 256 ) > "$DIST/checksums.txt.tmp"
mv "$DIST/checksums.txt.tmp" "$DIST/checksums.txt"
cat "$DIST/checksums.txt"
echo "== done: $(find "$DIST" -type f | wc -l | tr -d ' ') artifacts in $DIST/"
