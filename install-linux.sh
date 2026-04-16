#!/bin/bash
# MINT Exam IDE — Linux (Ubuntu/Debian) Full Installer
# Usage: curl -sL https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-linux.sh | bash

set -e

echo ""
echo "=============================="
echo "  MINT Exam IDE Installer"
echo "  (Linux/Ubuntu/Debian)"
echo "=============================="
echo ""

check() { command -v "$1" &>/dev/null; }
ok()    { echo "  [OK] $1"; }
miss()  { echo "  [--] $1"; PKGS="$PKGS $2"; }

PKGS=""

echo "[1/3] Checking dependencies..."

check python3 && ok "Python3" || miss "Python3" "python3 python3-pip python-is-python3"
check node    && ok "Node.js" || miss "Node.js" "nodejs npm"
check gcc     && ok "GCC"     || miss "GCC" "build-essential"
check javac   && ok "JDK"     || miss "JDK" "default-jdk"
check ffmpeg  && ok "FFmpeg"  || miss "FFmpeg" "ffmpeg"
echo ""

if [ -n "$PKGS" ]; then
    echo "[2/3] Installing:$PKGS"
    sudo apt update
    sudo apt install -y $PKGS
    echo ""
else
    echo "[2/3] All dependencies installed."
fi

echo "[3/3] Verifying..."
check python3 && ok "python3 ($(python3 --version 2>&1))" || echo "  [WARN] python3 not found"
check node    && ok "node ($(node --version 2>&1))"        || echo "  [WARN] node not found"
check gcc     && ok "gcc"                                   || echo "  [WARN] gcc not found"
check javac   && ok "javac"                                 || echo "  [WARN] javac not found"
check ffmpeg  && ok "ffmpeg"                                || echo "  [WARN] ffmpeg not found"

echo ""
echo "=============================="
echo "  Dependencies ready!"
echo "=============================="
echo ""
echo "  Note: Linux .deb/.AppImage builds are not yet available."
echo "  To build from source:"
echo "    git clone https://github.com/blueion0612/Mint_IDE_Student"
echo "    cd Mint_IDE_Student"
echo "    npm install && npx tauri build"
echo ""
