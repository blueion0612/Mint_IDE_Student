#!/bin/bash
# MINT Exam IDE — macOS Source Build Installer
#
# Apple Developer cert가 없어 signed binary 배포 불가. 학생이 직접 빌드.
# Usage:
#   curl -sL https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-mac.sh | bash

set -e

REPO="blueion0612/Mint_IDE_Student"
BUILD_DIR="$HOME/MINT_IDE_Source"
INSTALL_DIR="/Applications"

echo ""
echo "=============================="
echo "  MINT Exam IDE — Source Build"
echo "=============================="
echo ""

check() { command -v "$1" &>/dev/null; }

# ─── 1. Xcode Command Line Tools (gcc, git) ───
if ! check gcc; then
    echo "[1/6] Installing Xcode Command Line Tools..."
    echo "       (Popup appears — click 'Install' and wait ~5 min.)"
    xcode-select --install 2>/dev/null || true
    until check gcc; do sleep 5; done
fi
echo "[1/6] Xcode CLT: OK"

# ─── 2. Homebrew ───
if ! check brew; then
    echo "[2/6] Installing Homebrew..."
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
fi
# Ensure brew is on PATH for both Apple Silicon and Intel layouts.
if [ -x "/opt/homebrew/bin/brew" ]; then
    eval "$(/opt/homebrew/bin/brew shellenv)"
elif [ -x "/usr/local/bin/brew" ]; then
    eval "$(/usr/local/bin/brew shellenv)"
fi
echo "[2/6] Homebrew: OK ($(brew --prefix))"

# ─── 3. Brew packages (Python 3.12 + Node + JDK + FFmpeg + Rust) ───
echo "[3/6] Installing build tools (Python 3.12 / Node / JDK / FFmpeg / Rust)..."
NEED=""
check python3.12 || NEED="$NEED python@3.12"
check node       || NEED="$NEED node"
check javac      || NEED="$NEED openjdk@21"
check ffmpeg     || NEED="$NEED ffmpeg"
check rustc      || NEED="$NEED rust"
if [ -n "$NEED" ]; then
    brew install $NEED
fi
# Java symlink for IDE auto-detection
if [ -d "$(brew --prefix openjdk@21 2>/dev/null)/libexec/openjdk.jdk" ]; then
    sudo ln -sfn "$(brew --prefix openjdk@21)/libexec/openjdk.jdk" \
        /Library/Java/JavaVirtualMachines/openjdk-21.jdk 2>/dev/null || true
fi
echo "[3/6] Tools: OK"

# ─── 4. Source clone (idempotent — wipe + reclone for clean retry) ───
echo "[4/6] Cloning source to $BUILD_DIR ..."
rm -rf "$BUILD_DIR"
git clone "https://github.com/$REPO.git" "$BUILD_DIR"
cd "$BUILD_DIR"

# ─── 5. Build (npm install + tauri build) ───
echo "[5/6] Building (5~10 minutes, downloads ~500MB of Rust crates)..."
npm install
npm run tauri build

# ─── 6. Install to /Applications + remove quarantine attr ───
APP_PATH=$(find "$BUILD_DIR/src-tauri/target/release/bundle/macos" -maxdepth 1 -name "*.app" | head -1)
if [ -z "$APP_PATH" ]; then
    echo "  [FAIL] Build produced no .app at expected location."
    exit 1
fi
APP_NAME=$(basename "$APP_PATH" .app)
echo "[6/6] Installing $APP_NAME.app to $INSTALL_DIR ..."
rm -rf "$INSTALL_DIR/$APP_NAME.app"
cp -R "$APP_PATH" "$INSTALL_DIR/"
# Strip Gatekeeper quarantine — unsigned app would otherwise show
# "damaged and can't be opened".
xattr -cr "$INSTALL_DIR/$APP_NAME.app"

echo ""
echo "=============================="
echo "  Build complete!"
echo "=============================="
echo ""
echo "  App:    $INSTALL_DIR/$APP_NAME.app"
echo "  Source: $BUILD_DIR"
echo ""
echo "  ⚠ 첫 실행 시 권한 다이얼로그가 뜹니다. 모두 [허용] 누르세요:"
echo "    1. Screen Recording  — 시험 영상 녹화에 필수"
echo "    2. Automation        — 클립보드/포커스 모니터링에 필수"
echo ""
echo "  거부 시: 시스템 설정 > 개인정보 보호 및 보안 에서 수동 허용."
echo ""

open "$INSTALL_DIR/$APP_NAME.app"
