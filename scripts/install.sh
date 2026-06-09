#!/usr/bin/env sh
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"

cargo build --release
mkdir -p "$BIN_DIR"
cp target/release/remiaft "$BIN_DIR/remiaft"

echo "Installed remiaft to $BIN_DIR/remiaft"
echo "Add $BIN_DIR to PATH if the remiaft command is not found."
