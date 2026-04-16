#!/bin/bash
# MINT Exam IDE — macOS Full Installer
# Installs all dependencies + the IDE app
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

# ─── Helper ───
check() { command -v "$1" &>/dev/null; }
ok()    { echo "  [OK] $1"; }
miss()  { echo "  [--] $1 — will install"; NEED_INSTALL=1; }

NEED_INSTALL=0

# ─── 1. Check all dependencies ───
echo "[1/5] Checking dependencies..."

check python3 && ok "Python3 ($(python3 --version 2>&1))" || miss "Python3"
check node    && ok "Node.js ($(node --version 2>&1))"     || miss "Node.js"
check gcc     && ok "GCC ($(gcc --version 2>&1 | head -1))" || miss "GCC (Xcode Command Line Tools)"
check javac   && ok "JDK ($(javac -version 2>&1))"         || miss "JDK"
check ffmpeg  && ok "FFmpeg"                                 || miss "FFmpeg"
echo ""

# ─── 2. Install missing dependencies ───
if [ "$NEED_INSTALL" -eq 1 ]; then
    echo "[2/5] Installing missing dependencies..."

    # Xcode Command Line Tools (provides gcc, g++, make, git)
    if ! check gcc; then
        echo "  Installing Xcode Command Line Tools (gcc, g++, git)..."
        echo "  A popup may appear — click 'Install' and wait."
        xcode-select --install 2>/dev/null || true
        # Wait for installation
        echo "  Waiting for Xcode CLT installation..."
        until check gcc; do sleep 5; done
        echo "  Xcode CLT installed!"
    fi

    # Homebrew
    if ! check brew; then
        echo "  Installing Homebrew..."
        /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
        # Add to PATH
        if [ -f "/opt/homebrew/bin/brew" ]; then
            eval "$(/opt/homebrew/bin/brew shellenv)"
        elif [ -f "/usr/local/bin/brew" ]; then
            eval "$(/usr/local/bin/brew shellenv)"
        fi
    fi

    # Install missing packages via Homebrew
    BREW_PKGS=""
    check python3 || BREW_PKGS="$BREW_PKGS python"
    check node    || BREW_PKGS="$BREW_PKGS node"
    check javac   || BREW_PKGS="$BREW_PKGS openjdk"
    check ffmpeg  || BREW_PKGS="$BREW_PKGS ffmpeg"

    if [ -n "$BREW_PKGS" ]; then
        echo "  brew install$BREW_PKGS"
        brew install $BREW_PKGS
    fi

    # Java symlink (Homebrew openjdk needs this)
    if [ -d "$(brew --prefix openjdk 2>/dev/null)/libexec/openjdk.jdk" ] 2>/dev/null; then
        sudo ln -sfn "$(brew --prefix openjdk)/libexec/openjdk.jdk" /Library/Java/JavaVirtualMachines/openjdk.jdk 2>/dev/null || true
    fi

    echo ""
    echo "  Dependencies installed!"
else
    echo "[2/5] All dependencies already installed. Skipping."
fi

# ─── 3. Verify ───
echo "[3/5] Verifying..."
WARNINGS=""
check python3 && ok "python3" || WARNINGS="$WARNINGS python3"
check node    && ok "node"    || WARNINGS="$WARNINGS node"
check gcc     && ok "gcc"     || WARNINGS="$WARNINGS gcc"
check javac   && ok "javac"   || WARNINGS="$WARNINGS javac"
check ffmpeg  && ok "ffmpeg"  || WARNINGS="$WARNINGS ffmpeg"

if [ -n "$WARNINGS" ]; then
    echo ""
    echo "  [WARNING] These tools failed to install:$WARNINGS"
    echo "  Some languages may not work. You can install them manually later."
fi
echo ""

# ─── 4. Download & Install App ───
echo "[4/5] Downloading MINT Exam IDE..."

ARCH=$(uname -m)
if [ "$ARCH" = "arm64" ]; then
    DMG_PATTERN="aarch64.dmg"
else
    DMG_PATTERN="x64.dmg"
fi

DMG_URL=$(curl -sL "https://api.github.com/repos/$REPO/releases/latest" | \
    grep "browser_download_url.*${DMG_PATTERN}" | \
    grep -v "Lite" | \
    head -1 | \
    cut -d '"' -f 4)

if [ -z "$DMG_URL" ]; then
    echo "  Error: Could not find download URL"
    exit 1
fi

TMPDIR=$(mktemp -d)
DMG_PATH="$TMPDIR/mint-ide.dmg"
curl -L "$DMG_URL" -o "$DMG_PATH" --progress-bar

echo "[5/5] Installing app..."
MOUNT_POINT=$(hdiutil attach "$DMG_PATH" -nobrowse -quiet | tail -1 | awk '{print $NF}')

APP_FOUND=$(find "$MOUNT_POINT" -name "*.app" -maxdepth 1 | head -1)
if [ -n "$APP_FOUND" ]; then
    APP_NAME=$(basename "$APP_FOUND" .app)
    rm -rf "$INSTALL_DIR/$APP_NAME.app"
    cp -R "$APP_FOUND" "$INSTALL_DIR/"
else
    echo "  Error: No .app found"
    hdiutil detach "$MOUNT_POINT" -quiet
    exit 1
fi

hdiutil detach "$MOUNT_POINT" -quiet
xattr -cr "$INSTALL_DIR/$APP_NAME.app"
rm -rf "$TMPDIR"

# ─── Done ───
echo ""
echo "=============================="
echo "  Installation complete!"
echo "=============================="
echo ""
echo "  App:     $INSTALL_DIR/$APP_NAME.app"
echo "  Python:  $(python3 --version 2>&1 || echo 'not found')"
echo "  Node.js: $(node --version 2>&1 || echo 'not found')"
echo "  GCC:     $(gcc --version 2>&1 | head -1 || echo 'not found')"
echo "  Java:    $(javac -version 2>&1 || echo 'not found')"
echo "  FFmpeg:  $(ffmpeg -version 2>&1 | head -1 || echo 'not found')"
echo ""

open "$INSTALL_DIR/$APP_NAME.app"
