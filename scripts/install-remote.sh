#!/usr/bin/env sh
set -eu

REPO="${REMIAFT_REPO:-Angelhellwolf/RemiaftServerManager}"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
TMP_DIR="${TMPDIR:-/tmp}/remiaft-install-$$"

need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required command: $1" >&2
        exit 1
    fi
}

asset_name() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os:$arch" in
        Linux:x86_64 | Linux:amd64)
            echo "remiaft-linux-x86_64"
            ;;
        Darwin:x86_64 | Darwin:amd64)
            echo "remiaft-macos-x86_64"
            ;;
        *)
            echo "unsupported platform: $os $arch" >&2
            echo "install from source with: cargo install --git https://github.com/$REPO.git" >&2
            exit 1
            ;;
    esac
}

cleanup() {
    rm -rf "$TMP_DIR"
}

need curl
need chmod
need mkdir
trap cleanup EXIT INT TERM

mkdir -p "$TMP_DIR" "$BIN_DIR"
asset="$(asset_name)"
url="https://github.com/$REPO/releases/latest/download/$asset"

echo "Installing remiaft from $url"
if ! curl --retry 5 --retry-delay 2 --retry-all-errors -fL "$url" -o "$TMP_DIR/remiaft"; then
    echo "failed to download release asset" >&2
    echo "if this repository has no release yet, build from source:" >&2
    echo "  cargo install --git https://github.com/$REPO.git" >&2
    exit 1
fi

chmod +x "$TMP_DIR/remiaft"
mv "$TMP_DIR/remiaft" "$BIN_DIR/remiaft"

echo "Installed remiaft to $BIN_DIR/remiaft"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Add $BIN_DIR to PATH if the remiaft command is not found." ;;
esac
