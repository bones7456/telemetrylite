#!/usr/bin/env bash
# Cross-compile a static Linux binary locally (default x86_64); output goes to dist/. Needs zig, cargo-zigbuild and the rustup target.
# 在本机交叉编译 Linux 静态二进制(默认 x86_64),产物在 dist/。依赖: zig、cargo-zigbuild、rustup target。
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET="${TARGET:-x86_64-unknown-linux-musl}"
cargo zigbuild --release --target "$TARGET"
mkdir -p dist
cp "target/$TARGET/release/telemetrylite" dist/telemetrylite
(cd dist && shasum -a 256 telemetrylite | tee telemetrylite.sha256)
ls -lh dist/telemetrylite
