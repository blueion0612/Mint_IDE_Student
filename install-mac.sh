#!/bin/bash
# MINT Exam IDE — macOS one-line installer
# Usage: curl -sL https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-mac.sh | bash

set -e

APP_NAME="MINT Exam IDE"
REPO="blueion0612/Mint_IDE_Student"
INSTALL_DIR="/Applications"

echo ""
echo "=============================="
echo "  MINT Exam IDE Installer"
echo "=============================="
echo ""

# --- 1. Check & Install FFmpeg ---
echo "[1/4] Checking FFmpeg..."

if command -v ffmpeg &>/dev/null; then
    echo "  FFmpeg found: $(which ffmpeg)"
else
    echo "  FFmpeg not found. Installing..."

    if command -v brew &>/dev/null; then
        echo "  Installing via Homebrew..."
        brew install ffmpeg
    else
        echo "  Homebrew not found. Installing Homebrew first..."
        /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

        # Add brew to PATH for this session
        if [ -f "/opt/homebrew/bin/brew" ]; then
            eval "$(/opt/homebrew/bin/brew shellenv)"
        elif [ -f "/usr/local/bin/brew" ]; then
            eval "$(/usr/local/bin/brew shellenv)"
        fi

        echo "  Installing FFmpeg..."
        brew install ffmpeg
    fi

    if command -v ffmpeg &>/dev/null; then
        echo "  FFmpeg installed successfully!"
    else
        echo ""
        echo "  [WARNING] FFmpeg installation failed."
        echo "  Screen recording will not work."
        echo "  You can install it later: brew install ffmpeg"
        echo "  (or download the Lite version which doesn't need FFmpeg)"
        echo ""
    fi
fi

# --- 2. Detect architecture ---
echo "[2/4] Detecting system..."

ARCH=$(uname -m)
if [ "$ARCH" = "arm64" ]; then
    DMG_PATTERN="aarch64.dmg"
    echo "  Apple Silicon (M1/M2/M3/M4)"
else
    DMG_PATTERN="x64.dmg"
    echo "  Intel Mac"
fi

# --- 3. Download & Install ---
echo "[3/4] Downloading latest version..."

DMG_URL=$(curl -sL "https://api.github.com/repos/$REPO/releases/latest" | \
    grep "browser_download_url.*${DMG_PATTERN}" | \
    grep -v "Lite" | \
    head -1 | \
    cut -d '"' -f 4)

if [ -z "$DMG_URL" ]; then
    echo "  Error: Could not find download URL"
    exit 1
fi

echo "  $(basename "$DMG_URL")"
TMPDIR=$(mktemp -d)
DMG_PATH="$TMPDIR/mint-ide.dmg"

curl -L "$DMG_URL" -o "$DMG_PATH" --progress-bar

echo "[4/4] Installing..."
MOUNT_POINT=$(hdiutil attach "$DMG_PATH" -nobrowse -quiet | tail -1 | awk '{print $NF}')

# Find and copy .app
APP_FOUND=$(find "$MOUNT_POINT" -name "*.app" -maxdepth 1 | head -1)
if [ -n "$APP_FOUND" ]; then
    APP_NAME=$(basename "$APP_FOUND" .app)
    rm -rf "$INSTALL_DIR/$APP_NAME.app"
    cp -R "$APP_FOUND" "$INSTALL_DIR/"
else
    echo "  Error: No .app found in DMG"
    hdiutil detach "$MOUNT_POINT" -quiet
    exit 1
fi

hdiutil detach "$MOUNT_POINT" -quiet

# Remove quarantine — bypasses Gatekeeper
xattr -cr "$INSTALL_DIR/$APP_NAME.app"

rm -rf "$TMPDIR"

# --- Done ---
echo ""
echo "=============================="
echo "  Installation complete!"
echo "=============================="
echo ""
echo "  App: $INSTALL_DIR/$APP_NAME.app"
if command -v ffmpeg &>/dev/null; then
    echo "  FFmpeg: $(which ffmpeg)"
    echo "  Screen recording: enabled"
else
    echo "  FFmpeg: not installed"
    echo "  Screen recording: disabled (install FFmpeg to enable)"
fi
echo ""
echo "  Opening $APP_NAME..."
echo ""

open "$INSTALL_DIR/$APP_NAME.app"
