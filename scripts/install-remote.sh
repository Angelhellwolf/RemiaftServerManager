#!/usr/bin/env sh
set -eu

REPO="${REMIAFT_REPO:-Angelhellwolf/RemiaftServerManager}"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
BIN_PATH="$BIN_DIR/remiaft"
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

old_version=""
if [ -x "$BIN_PATH" ]; then
    old_version="$("$BIN_PATH" --version 2>/dev/null || true)"
fi

echo "Installing remiaft from $url"
if ! curl --retry 5 --retry-delay 2 --retry-all-errors -fL "$url" -o "$TMP_DIR/remiaft"; then
    echo "failed to download release asset" >&2
    echo "if this repository has no release yet, build from source:" >&2
    echo "  cargo install --git https://github.com/$REPO.git" >&2
    exit 1
fi

chmod +x "$TMP_DIR/remiaft"
new_version="$("$TMP_DIR/remiaft" --version 2>/dev/null || true)"
mv -f "$TMP_DIR/remiaft" "$BIN_PATH"

if [ -n "$old_version" ] && [ -n "$new_version" ]; then
    if [ "$old_version" = "$new_version" ]; then
        echo "remiaft is already up to date: $new_version"
    else
        echo "Updated remiaft: $old_version -> $new_version"
    fi
else
    echo "Installed remiaft${new_version:+: $new_version}"
fi

echo "Binary path: $BIN_PATH"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Add $BIN_DIR to PATH if the remiaft command is not found." ;;
esac
