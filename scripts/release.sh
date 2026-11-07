#!/usr/bin/env bash
# 在本机交叉编译 Linux x86_64 静态二进制,产物在 dist/。依赖: zig、cargo-zigbuild、rustup target。
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET="${TARGET:-x86_64-unknown-linux-musl}"
cargo zigbuild --release --target "$TARGET"
mkdir -p dist
cp "target/$TARGET/release/telemetrylite" dist/telemetrylite
(cd dist && shasum -a 256 telemetrylite | tee telemetrylite.sha256)
ls -lh dist/telemetrylite
