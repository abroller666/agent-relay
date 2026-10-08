#!/bin/sh
# Build the binary where herdr-plugin.toml expects it (./bin/pane-relay).
set -eu
cd "$(dirname "$0")/.."
cargo build --release
mkdir -p bin
cp target/release/pane-relay bin/pane-relay
