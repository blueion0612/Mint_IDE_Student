#!/bin/bash
# MINT Exam IDE — macOS one-line installer
# Usage: curl -sL https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-mac.sh | bash

set -e

APP_NAME="MINT Exam IDE"
REPO="blueion0612/Mint_IDE_Student"
INSTALL_DIR="/Applications"

echo ""
echo "=== MINT Exam IDE Installer ==="
echo ""

# Detect architecture
ARCH=$(uname -m)
if [ "$ARCH" = "arm64" ]; then
    DMG_PATTERN="aarch64.dmg"
    echo "Detected: Apple Silicon (M1/M2/M3/M4)"
else
    DMG_PATTERN="x64.dmg"
    echo "Detected: Intel Mac"
fi

# Get latest release DMG URL
echo "Finding latest release..."
DMG_URL=$(curl -sL "https://api.github.com/repos/$REPO/releases/latest" | \
    grep "browser_download_url.*${DMG_PATTERN}" | \
    grep -v "Lite" | \
    head -1 | \
    cut -d '"' -f 4)

if [ -z "$DMG_URL" ]; then
    echo "Error: Could not find DMG download URL"
    exit 1
fi

echo "Downloading: $(basename "$DMG_URL")"
TMPDIR=$(mktemp -d)
DMG_PATH="$TMPDIR/mint-ide.dmg"

curl -L "$DMG_URL" -o "$DMG_PATH" --progress-bar

# Mount DMG
echo "Installing..."
MOUNT_POINT=$(hdiutil attach "$DMG_PATH" -nobrowse -quiet | tail -1 | awk '{print $NF}')

# Copy app
if [ -d "$MOUNT_POINT/$APP_NAME.app" ]; then
    rm -rf "$INSTALL_DIR/$APP_NAME.app"
    cp -R "$MOUNT_POINT/$APP_NAME.app" "$INSTALL_DIR/"
else
    # Try finding .app in mount point
    APP_FOUND=$(find "$MOUNT_POINT" -name "*.app" -maxdepth 1 | head -1)
    if [ -n "$APP_FOUND" ]; then
        rm -rf "$INSTALL_DIR/$(basename "$APP_FOUND")"
        cp -R "$APP_FOUND" "$INSTALL_DIR/"
        APP_NAME=$(basename "$APP_FOUND" .app)
    else
        echo "Error: No .app found in DMG"
        hdiutil detach "$MOUNT_POINT" -quiet
        exit 1
    fi
fi

# Unmount
hdiutil detach "$MOUNT_POINT" -quiet

# Remove quarantine attribute — this is the key step
xattr -cr "$INSTALL_DIR/$APP_NAME.app"

# Cleanup
rm -rf "$TMPDIR"

echo ""
echo "=== Installation complete! ==="
echo "App installed to: $INSTALL_DIR/$APP_NAME.app"
echo ""
echo "You can now open '$APP_NAME' from Applications or Launchpad."
echo ""

# Open the app
open "$INSTALL_DIR/$APP_NAME.app"
