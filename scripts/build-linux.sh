#!/bin/sh
# Builds static Linux binaries into dist/, from macOS or Linux. They link
# against musl, so they run on any distro with nothing installed.
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
for arch in x86_64 aarch64; do
    target="$arch-unknown-linux-musl"
    rustup target add "$target" >/dev/null
    linker="CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER"
    env "$linker=rust-lld" cargo build --release --target "$target"
    cp "target/$target/release/agentmon" "dist/agentmon-linux-$arch"
done
ls -l dist
