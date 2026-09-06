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
# g++ is checked SEPARATELY from gcc. A distro can ship gcc without g++
# (gcc-core-only images do), and the IDE's C++ Run needs the C++ driver, not
# just the C one — a machine with gcc alone passed this check and then failed
# every C++ compile.
check gcc     && ok "GCC"     || miss "GCC" "build-essential"
check g++     && ok "G++"     || miss "G++" "build-essential g++"
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

# Prove the C++ toolchain COMPILES, LINKS and RUNS. "g++ exists" is not the
# same claim: a broken libstdc++ or a missing linker passes a `command -v` check
# and fails at the first Run of an exam.
if check g++; then
    CPP_PROBE="$(mktemp -d)"
    cat > "$CPP_PROBE/probe.cpp" <<'CPPEOF'
#include <iostream>
#include <vector>
#include <string>
int main(){ std::vector<std::string> v{"MINT","CPP","OK"}; for(auto&s:v) std::cout<<s<<" "; std::cout<<std::endl; }
CPPEOF
    if g++ -std=c++17 -O2 "$CPP_PROBE/probe.cpp" -o "$CPP_PROBE/probe" 2>/dev/null \
       && "$CPP_PROBE/probe" 2>/dev/null | grep -q "MINT CPP OK"; then
        ok "g++ ($(g++ --version 2>&1 | head -1)) — compile + link + run verified"
    else
        echo "  [WARN] g++ is present but the compile probe failed — C++ Run may not work."
    fi
    rm -rf "$CPP_PROBE"
else
    echo "  [WARN] g++ not found — C++ Run will not work."
fi
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
