#!/bin/sh
# Build the binary where herdr-plugin.toml expects it (./bin/agent-relay).
set -eu
cd "$(dirname "$0")/.."
cargo build --release
mkdir -p bin
cp target/release/agent-relay bin/agent-relay
