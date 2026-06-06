#!/usr/bin/env sh
set -e

REPO="DavidNzube101/suidrop"
INSTALL_DIR="/usr/local/bin"

BINARY="${1:-suidrop-cli}"
VERSION_ARG="${2:-latest}"

get_latest_version() {
    curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' \
        | sed 's/.*"tag_name": "\(.*\)".*/\1/'
}

if [ "$VERSION_ARG" = "latest" ]; then
    VERSION=$(get_latest_version)
else
    case "$VERSION_ARG" in
        v*) VERSION="$VERSION_ARG" ;;
        *)  VERSION="v$VERSION_ARG" ;;
    esac
fi

get_target() {
    OS=$(uname -s)
    ARCH=$(uname -m)

    case "$OS" in
        Linux)
            case "$ARCH" in
                x86_64)  echo "suidrop-cli-linux-x86_64" ;;
                aarch64) echo "suidrop-cli-linux-arm64" ;;
                i686|i386) echo "suidrop-cli-linux-i686" ;;
                *)       echo "Unsupported architecture: $ARCH" >&2 && exit 1 ;;
            esac
            ;;
        Darwin)
            case "$ARCH" in
                x86_64) echo "suidrop-cli-macos-x86_64" ;;
                arm64)  echo "suidrop-cli-macos-arm64" ;;
                *)      echo "Unsupported architecture: $ARCH" >&2 && exit 1 ;;
            esac
            ;;
        *)
            echo "Unsupported OS: $OS. On Windows, download the .exe from the releases page." >&2
            exit 1
            ;;
    esac
}

is_our_binary() {
    path="$1"
    [ -x "$path" ] || return 1
    "$path" --version 2>&1 | grep -q "^suidrop-cli [0-9]" || return 1
    help_text=$("$path" --help 2>&1)
    echo "$help_text" | grep -q "Walrus" || return 1
    echo "$help_text" | grep -q "send" || return 1
    echo "$help_text" | grep -q "get" || return 1
    return 0
}

if [ -e "$INSTALL_DIR/$BINARY" ]; then
    if is_our_binary "$INSTALL_DIR/$BINARY"; then
        echo "SuiDrop CLI detected at $INSTALL_DIR/$BINARY. Upgrading to $VERSION..."
    else
        echo "Error: A file named '$BINARY' already exists in $INSTALL_DIR and is not the SuiDrop CLI."
        echo "Install under a different name by passing it as an argument:"
        echo "  curl -fsSL https://suidrop.xyz/install.sh | sh -s -- your-custom-name"
        exit 1
    fi
fi

TARGET=$(get_target)
URL="https://github.com/$REPO/releases/download/$VERSION/$TARGET"

echo "Installing SuiDrop CLI $VERSION as '$BINARY'..."
echo "Downloading $TARGET..."

TMP_BIN="/tmp/suidrop_cli_$(date +%s)"
if ! curl -fsSL "$URL" -o "$TMP_BIN"; then
    echo "Error: could not download $VERSION for your platform. Check the releases page." >&2
    exit 1
fi
chmod +x "$TMP_BIN"

if [ -w "$INSTALL_DIR" ]; then
    mv -f "$TMP_BIN" "$INSTALL_DIR/$BINARY"
else
    echo "Sudo privileges are required to install to $INSTALL_DIR"
    sudo mv -f "$TMP_BIN" "$INSTALL_DIR/$BINARY"
fi

echo "Installed to $INSTALL_DIR/$BINARY"
echo "Run: $BINARY --help"
