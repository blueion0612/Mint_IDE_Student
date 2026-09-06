# MINT Exam IDE — Windows Installer
# PowerShell (관리자):
#   Set-ExecutionPolicy Bypass -Scope Process -Force; irm https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-windows.ps1 | iex

$ErrorActionPreference = "Continue"
# Suppress PowerShell 5.1 progress bar — Invoke-WebRequest is ~10x faster
# without it, and we have our own status lines.
$ProgressPreference = "SilentlyContinue"

# Check admin — Python InstallAllUsers=1 + winget HKLM writes both require it.
# Without admin the script will SILENTLY install half the environment.
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host ""
    Write-Host "  [STOP] Administrator privileges required." -ForegroundColor Red
    Write-Host "  This installer writes to C:\ProgramData and uses winget" -ForegroundColor Yellow
    Write-Host "  to install JDK/Node/FFmpeg — both need admin." -ForegroundColor Yellow
    Write-Host ""
    Write-Host "  How to fix:" -ForegroundColor Cyan
    Write-Host "    1. Close this window." -ForegroundColor Cyan
    Write-Host '    2. Start menu > "Windows PowerShell" > right-click > Run as Administrator.' -ForegroundColor Cyan
    Write-Host "    3. Paste the install command again." -ForegroundColor Cyan
    Write-Host ""
    Read-Host "Press Enter to close"
    exit 1
}

try {

# Set at each warn-and-continue site so the final banner tells the truth
# instead of unconditionally claiming "complete".
$script:hadWarnings = $false

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  MINT Exam IDE Installer" -ForegroundColor Cyan
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

function Test-Cmd($cmd) { $null -ne (Get-Command $cmd -ErrorAction SilentlyContinue) }

# ─── Configuration (hardcoded for reproducibility) ───
# Astral python-build-standalone — fully portable CPython distribution.
# Avoids python.org MSI installer entirely (no more 1638 conflicts with
# existing Python 3.12.x on the student's PC). bit-identical across all
# student machines.
$MINT_PY_VERSION = "3.12.13"
$MINT_PY_BUILD   = "20260510"
$MINT_PY_URL     = "https://github.com/astral-sh/python-build-standalone/releases/download/$MINT_PY_BUILD/cpython-$MINT_PY_VERSION%2B$MINT_PY_BUILD-x86_64-pc-windows-msvc-install_only.tar.gz"
$MINT_PY_ROOT    = "C:\ProgramData\MINT_Python\Python312"
$MINT_PY_EXE     = "$MINT_PY_ROOT\python.exe"

# Portable MinGW-w64 (WinLibs, GCC + UCRT runtime). Same reasoning as the
# portable Python: pinned to one build so every student compiles with an
# identical toolchain, unpacked to an ASCII path so a Korean username cannot
# break it, and no installer/registry involvement so it cannot collide with a
# compiler the student already has.
#
# Windows is the only platform that needs this. macOS gets clang from the Xcode
# command line tools and Linux gets g++ from the distro, both of which the other
# install scripts already ensure.
$MINT_GCC_VERSION = "15.3.0"
$MINT_GCC_TAG     = "15.3.0posix-14.0.0-ucrt-r1"
$MINT_GCC_ASSET   = "winlibs-x86_64-posix-seh-gcc-15.3.0-mingw-w64ucrt-14.0.0-r1.7z"
$MINT_GCC_URL     = "https://github.com/brechtsanders/winlibs_mingw/releases/download/$MINT_GCC_TAG/$MINT_GCC_ASSET"
$MINT_GCC_SHA256  = "7bd06101e7a472b41506b13f79b2c56e3369da13a72165cbe8fd8a18b6e9d116"
$MINT_GCC_ROOT    = "C:\ProgramData\MINT_MinGW"
$MINT_GXX_EXE     = "$MINT_GCC_ROOT\mingw64\bin\g++.exe"

# ─── 0. System policy: enable long paths (manifest alone is not enough) ───
# Windows 10 1607+ requires BOTH a process manifest with longPathAware=true
# AND HKLM\SYSTEM\CCS\Control\FileSystem\LongPathsEnabled=1. Otherwise
# Korean usernames + nested workspace paths over 260 chars break workspace.rs
# operations with ERROR_FILENAME_EXCED_RANGE. We already have admin here.
Write-Host "[0/6] Enabling Windows long path support..." -ForegroundColor Yellow
try {
    Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem" `
                     -Name "LongPathsEnabled" -Type DWord -Value 1 -ErrorAction Stop
    Write-Host "  [OK] LongPathsEnabled = 1 (HKLM)" -ForegroundColor Green
} catch {
    Write-Host "  [WARN] Could not enable LongPathsEnabled: $_" -ForegroundColor Yellow
    Write-Host "         Korean usernames + long workspace paths may break." -ForegroundColor Yellow
}
Write-Host ""

# ─── 1. Portable Python (extracted from Astral python-build-standalone) ───
Write-Host "[1/6] Setting up portable Python $MINT_PY_VERSION..." -ForegroundColor Yellow

# Already extracted from a previous run? Skip download — but ONLY if it is the
# pinned version AND tkinter loads. A prior release shipping e.g. 3.12.10 would
# otherwise be kept forever, silently breaking the "byte-identical env across
# all students" invariant; a half-extracted python.exe with broken tcl would
# also pass. If either check fails, fall through to a clean re-extract (the
# extract path deletes the stale dir first, so it is re-entrant).
$pyReuse = $false
if (Test-Path $MINT_PY_EXE) {
    $ver = & $MINT_PY_EXE --version 2>&1
    $tkOk = (& $MINT_PY_EXE -c "import tkinter; tkinter.Tk().destroy(); print('OK')" 2>&1) -match "OK"
    if (("$ver" -match [regex]::Escape($MINT_PY_VERSION)) -and $tkOk) {
        Write-Host "  [OK] Already present: $ver at $MINT_PY_ROOT" -ForegroundColor Green
        $pyReuse = $true
    } else {
        Write-Host "  [..] Present but not pinned $MINT_PY_VERSION (found '$ver', tkinter=$tkOk) — re-extracting." -ForegroundColor Yellow
    }
}
if (-not $pyReuse) {
    # Sanity check: tar.exe ships with Windows 10 1803+. Older builds will
    # not have it. Fall back is manual download instructions.
    if (-not (Test-Cmd "tar")) {
        Write-Host "  [FAIL] tar.exe not found. Need Windows 10 1803+ or newer." -ForegroundColor Red
        Write-Host "         Manual: download $MINT_PY_URL and extract its 'python\' folder" -ForegroundColor Yellow
        Write-Host "         to $MINT_PY_ROOT (rename 'python' to 'Python312')." -ForegroundColor Yellow
        Read-Host "Press Enter to close"
        exit 1
    }

    $tarPath = "$env:TEMP\mint-cpython-$MINT_PY_VERSION.tar.gz"
    Write-Host "  Downloading portable Python (~45 MB) ..."
    Write-Host "  $MINT_PY_URL" -ForegroundColor DarkGray
    try {
        Invoke-WebRequest -Uri $MINT_PY_URL -OutFile $tarPath -UseBasicParsing
    } catch {
        Write-Host "  [FAIL] Download failed: $_" -ForegroundColor Red
        Write-Host "         Check internet, or download manually from:" -ForegroundColor Yellow
        Write-Host "         https://github.com/astral-sh/python-build-standalone/releases/tag/$MINT_PY_BUILD" -ForegroundColor Cyan
        Read-Host "Press Enter to close"
        exit 1
    }

    # Extract — the tarball top-level contains a single 'python\' directory.
    # We extract into ProgramData\MINT_Python\, then rename python → Python312
    # so the rest of the script and the IDE can keep using $MINT_PY_ROOT.
    $extractParent = "C:\ProgramData\MINT_Python"
    $stagingDir    = "$extractParent\python"
    if (Test-Path $stagingDir) { Remove-Item $stagingDir -Recurse -Force }
    if (Test-Path $MINT_PY_ROOT) { Remove-Item $MINT_PY_ROOT -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $extractParent | Out-Null

    Write-Host "  Extracting to $extractParent ..."
    tar -xzf $tarPath -C $extractParent
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path "$stagingDir\python.exe")) {
        Write-Host "  [FAIL] Extraction produced no python.exe at $stagingDir" -ForegroundColor Red
        Write-Host "         tar exit code: $LASTEXITCODE" -ForegroundColor Yellow
        Remove-Item $tarPath -ErrorAction SilentlyContinue
        Read-Host "Press Enter to close"
        exit 1
    }
    Rename-Item -Path $stagingDir -NewName "Python312"
    Remove-Item $tarPath -ErrorAction SilentlyContinue

    if (-not (Test-Path $MINT_PY_EXE)) {
        Write-Host "  [FAIL] Expected python.exe missing after rename: $MINT_PY_EXE" -ForegroundColor Red
        Read-Host "Press Enter to close"
        exit 1
    }

    $ver = & $MINT_PY_EXE --version 2>&1
    Write-Host "  [OK] Extracted: $ver" -ForegroundColor Green

    # Verify tkinter loads — matplotlib GUI (plt.show) depends on it.
    # python-build-standalone install_only ships tcl/tk by default, so this
    # is sanity-check only. If it fails, the asset on Astral changed.
    $tkCheck = & $MINT_PY_EXE -c "import tkinter; tkinter.Tk().destroy(); print('tkinter OK')" 2>&1
    if ($tkCheck -match "tkinter OK") {
        Write-Host "  [OK] tkinter/TCL verified" -ForegroundColor Green
    } else {
        Write-Host "  [FAIL] tkinter self-check failed in portable Python:" -ForegroundColor Red
        Write-Host "         $tkCheck" -ForegroundColor DarkGray
        Write-Host "         The python-build-standalone asset may have changed structure." -ForegroundColor Yellow
        Write-Host "         Report to https://github.com/blueion0612/Mint_IDE_Student/issues" -ForegroundColor Cyan
        Read-Host "Press Enter to close"
        exit 1
    }
}

Write-Host ""

# ─── 2. Other system deps via winget (Node, JDK, FFmpeg, WebView2) ───
# ─── 2. Portable C/C++ toolchain (WinLibs MinGW-w64) ───
# Without this every C++ Run failed with "Failed to run 'g++'": nothing on a
# stock Windows install provides a compiler, and the IDE has no way to conjure
# one. macOS and Linux get theirs from the CLT / distro packages.
Write-Host "[2/6] Setting up portable C/C++ toolchain (GCC $MINT_GCC_VERSION)..." -ForegroundColor Yellow

$gccReuse = $false
if (Test-Path $MINT_GXX_EXE) {
    $gccVer = & $MINT_GXX_EXE --version 2>&1 | Select-Object -First 1
    if ("$gccVer" -match [regex]::Escape($MINT_GCC_VERSION)) {
        Write-Host "  [OK] Already present: $gccVer" -ForegroundColor Green
        $gccReuse = $true
    } else {
        Write-Host "  [..] Present but not pinned $MINT_GCC_VERSION (found '$gccVer') - re-extracting." -ForegroundColor Yellow
    }
}

if (-not $gccReuse) {
    if (-not (Test-Cmd "tar")) {
        Write-Host "  [WARN] tar.exe not found (needs Windows 10 1803+). Skipping C/C++ toolchain." -ForegroundColor Yellow
        Write-Host "         Python/Java will still work; C and C++ Run will not." -ForegroundColor Yellow
        $script:hadWarnings = $true
    } else {
        $gccArchive = "$env:TEMP\mint-mingw-$MINT_GCC_VERSION.7z"
        Write-Host "  Downloading MinGW-w64 GCC $MINT_GCC_VERSION (~102 MB download, ~900 MB installed) ..."
        Write-Host "  $MINT_GCC_URL" -ForegroundColor DarkGray
        $gccOk = $true
        try {
            Invoke-WebRequest -Uri $MINT_GCC_URL -OutFile $gccArchive -UseBasicParsing
        } catch {
            Write-Host "  [WARN] Download failed: $_" -ForegroundColor Yellow
            $gccOk = $false
        }

        # Verify the pinned digest. A truncated or substituted archive would
        # otherwise be extracted and produce a compiler that miscompiles or
        # simply fails at link time, mid-exam.
        if ($gccOk) {
            $actual = (Get-FileHash $gccArchive -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $MINT_GCC_SHA256) {
                Write-Host "  [WARN] SHA-256 mismatch - refusing to install this toolchain." -ForegroundColor Yellow
                Write-Host "         expected $MINT_GCC_SHA256" -ForegroundColor DarkGray
                Write-Host "         actual   $actual" -ForegroundColor DarkGray
                $gccOk = $false
            }
        }

        if ($gccOk) {
            if (Test-Path $MINT_GCC_ROOT) { Remove-Item $MINT_GCC_ROOT -Recurse -Force -ErrorAction SilentlyContinue }
            New-Item -ItemType Directory -Force -Path $MINT_GCC_ROOT | Out-Null
            Write-Host "  Extracting to $MINT_GCC_ROOT (takes ~10 s) ..."
            # Windows' bundled tar is bsdtar/libarchive, which reads 7-Zip.
            & "$env:SystemRoot\System32\tar.exe" -xf $gccArchive -C $MINT_GCC_ROOT
            if ($LASTEXITCODE -ne 0 -or -not (Test-Path $MINT_GXX_EXE)) {
                Write-Host "  [WARN] Extraction did not produce $MINT_GXX_EXE (tar exit $LASTEXITCODE)." -ForegroundColor Yellow
                $gccOk = $false
            }
        }

        Remove-Item $gccArchive -ErrorAction SilentlyContinue

        if ($gccOk) {
            # Prove it can actually build and run something before declaring
            # success — an extracted-but-broken toolchain looks identical to a
            # working one until the first Run of an exam.
            $probeDir = "$env:TEMP\mint-gcc-probe"
            if (Test-Path $probeDir) { Remove-Item $probeDir -Recurse -Force -ErrorAction SilentlyContinue }
            New-Item -ItemType Directory -Force -Path $probeDir | Out-Null
            $probeSrc = "$probeDir\probe.cpp"
            Set-Content -Path $probeSrc -Encoding utf8 -Value @'
#include <iostream>
#include <vector>
#include <string>
int main(){ std::vector<std::string> v{"MINT","GCC","OK"}; for(auto&s:v) std::cout<<s<<" "; std::cout<<std::endl; }
'@
            & $MINT_GXX_EXE -std=c++17 -O2 -static "$probeSrc" -o "$probeDir\probe.exe" 2>&1 | Out-Null
            if ((Test-Path "$probeDir\probe.exe") -and ((& "$probeDir\probe.exe" 2>&1) -match "MINT GCC OK")) {
                Write-Host "  [OK] GCC $MINT_GCC_VERSION installed and verified (compile + link + run)" -ForegroundColor Green
            } else {
                Write-Host "  [WARN] Toolchain extracted but the compile probe failed." -ForegroundColor Yellow
                Write-Host "         C and C++ Run may not work. Python/Java are unaffected." -ForegroundColor Yellow
                $script:hadWarnings = $true
            }
            Remove-Item $probeDir -Recurse -Force -ErrorAction SilentlyContinue
        } else {
            Write-Host "  [WARN] C/C++ toolchain not installed - C and C++ Run will not work." -ForegroundColor Yellow
            Write-Host "         Re-run this installer, or point the IDE at an existing" -ForegroundColor Yellow
            Write-Host "         compiler in Settings > C++ compiler." -ForegroundColor Yellow
            $script:hadWarnings = $true
        }
    }
}
Write-Host ""

Write-Host "[3/6] Checking Node.js / JDK / FFmpeg / WebView2..." -ForegroundColor Yellow

# WebView2 Runtime — Tauri IDE renders into this. Without it, IDE first
# launch shows a blank/black window and the student can't take the exam.
# Windows 11 ships it; some Windows 10 / LTSC / IoT SKUs don't. The Tauri
# bundler's downloadBootstrapper option also fails on locked-down school
# networks. Installing explicitly via winget is reliable.
if (Test-Cmd "winget") {
    $wv2Installed = $false
    # Check via registry — WebView2 doesn't expose a CLI Test-Cmd target.
    $wv2Keys = @(
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
    )
    foreach ($k in $wv2Keys) {
        if (Test-Path $k) { $wv2Installed = $true; break }
    }
    if ($wv2Installed) {
        Write-Host "  [OK] WebView2 Runtime" -ForegroundColor Green
    } else {
        Write-Host "  [--] WebView2 Runtime — installing..." -ForegroundColor Red
        try {
            winget install -e --id Microsoft.EdgeWebView2Runtime --accept-source-agreements --accept-package-agreements 2>&1 | Out-Null
        } catch {
            Write-Host "    [WARN] WebView2 install failed: $_" -ForegroundColor Yellow
        }
        if ($LASTEXITCODE -ne 0) {
            Write-Host "    [WARN] WebView2 install exit code $LASTEXITCODE" -ForegroundColor Yellow
        }
        # winget reports failure via $LASTEXITCODE, not an exception — re-probe
        # the registry to confirm WebView2 actually landed.
        $wv2Installed = $false
        foreach ($k in $wv2Keys) {
            if (Test-Path $k) { $wv2Installed = $true; break }
        }
        if (-not $wv2Installed) {
            Write-Host "    [WARN] WebView2 still missing after winget install." -ForegroundColor Yellow
            Write-Host "    IDE first launch may show a blank window. Install manually:" -ForegroundColor Yellow
            Write-Host "    https://developer.microsoft.com/microsoft-edge/webview2/" -ForegroundColor Cyan
        }
    }
}

# Confirm winget itself is available — Windows 10 LTSC / Server SKUs lack it.
if (-not (Test-Cmd "winget")) {
    Write-Host "  [WARN] winget not available on this system." -ForegroundColor Yellow
    Write-Host "  Install 'App Installer' from Microsoft Store, or manually install:" -ForegroundColor Yellow
    Write-Host "    - Node.js LTS:  https://nodejs.org/" -ForegroundColor Cyan
    Write-Host "    - Temurin JDK:  https://adoptium.net/" -ForegroundColor Cyan
    Write-Host "    - FFmpeg:       https://www.gyan.dev/ffmpeg/builds/" -ForegroundColor Cyan
    Write-Host "  Continuing without auto-install — Java/C++ run + recording may not work." -ForegroundColor Yellow
    $script:hadWarnings = $true
} else {
    $missing = @()
    if (Test-Cmd "node")  { Write-Host "  [OK] Node.js" -ForegroundColor Green } else { Write-Host "  [--] Node.js" -ForegroundColor Red; $missing += "OpenJS.NodeJS.LTS" }
    if (Test-Cmd "javac") { Write-Host "  [OK] JDK" -ForegroundColor Green }     else { Write-Host "  [--] JDK" -ForegroundColor Red;     $missing += "EclipseAdoptium.Temurin.21.JDK" }
    if (Test-Cmd "ffmpeg"){ Write-Host "  [OK] FFmpeg" -ForegroundColor Green }  else { Write-Host "  [--] FFmpeg" -ForegroundColor Red;  $missing += "Gyan.FFmpeg" }

    if ($missing.Count -gt 0) {
        Write-Host "  Installing $($missing.Count) via winget..."
        foreach ($pkg in $missing) {
            Write-Host "    Installing $pkg..."
            # FFmpeg (Gyan.FFmpeg) is a winget PORTABLE package — its default
            # scope is USER even in an elevated shell, so when an admin elevates
            # a DIFFERENT account than the student's, it lands in the admin
            # profile + admin PATH and the student's session never sees it
            # (recording fails at exam time while this script's own re-probe
            # passes). Force machine scope so it goes under %ProgramFiles%\WinGet
            # with a system PATH entry. Node/JDK are perMachine MSIs already.
            $scopeArg = @()
            if ($pkg -eq "Gyan.FFmpeg") { $scopeArg = @("--scope", "machine") }
            try {
                $wingetOut = winget install -e --id $pkg @scopeArg --accept-source-agreements --accept-package-agreements 2>&1
                if ($LASTEXITCODE -ne 0) {
                    Write-Host "    [WARN] $pkg install exit code $LASTEXITCODE" -ForegroundColor Yellow
                    Write-Host "      $($wingetOut | Out-String)" -ForegroundColor DarkGray
                    $script:hadWarnings = $true
                }
            } catch {
                Write-Host "    [WARN] $pkg install threw: $_" -ForegroundColor Yellow
                $script:hadWarnings = $true
            }
        }
        $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")

        # Post-install verification. Node/JDK/FFmpeg each break a feature if
        # they didn't actually land, so re-probe all three (not just ffmpeg).
        if (-not (Test-Cmd "ffmpeg")) {
            Write-Host "  [WARN] ffmpeg still missing after winget install — screen recording will not work." -ForegroundColor Yellow
            $script:hadWarnings = $true
        }
        if (-not (Test-Cmd "node")) {
            Write-Host "  [WARN] node still missing after winget install — JavaScript/TypeScript run will not work." -ForegroundColor Yellow
            $script:hadWarnings = $true
        }
        if (-not (Test-Cmd "javac")) {
            Write-Host "  [WARN] javac still missing after winget install — Java run will not work." -ForegroundColor Yellow
            $script:hadWarnings = $true
        }
    }
}

Write-Host ""

# ─── 3. Download IDE installer ───
Write-Host "[4/6] Downloading MINT Exam IDE..." -ForegroundColor Yellow

# GitHub API rate limit (60/hr unauthenticated). A shared exam-room IP hits
# this fast. Catch the 403 and tell the student what to do instead of dying
# with a generic "exception" message.
$releases = $null
try {
    $releases = Invoke-RestMethod "https://api.github.com/repos/blueion0612/Mint_IDE_Student/releases?per_page=10"
} catch {
    $errMsg = $_.Exception.Message
    if ($errMsg -match "rate limit|403") {
        Write-Host "  [FAIL] GitHub API rate limit reached (shared IP?)." -ForegroundColor Red
        Write-Host "         Wait 30~60 minutes or download manually:" -ForegroundColor Yellow
        Write-Host "         https://github.com/blueion0612/Mint_IDE_Student/releases/latest" -ForegroundColor Cyan
    } else {
        Write-Host "  [FAIL] Could not reach GitHub: $errMsg" -ForegroundColor Red
        Write-Host "         Check your internet connection, or download manually:" -ForegroundColor Yellow
        Write-Host "         https://github.com/blueion0612/Mint_IDE_Student/releases/latest" -ForegroundColor Cyan
    }
    Read-Host "Press Enter to close"
    exit 1
}

$exeAsset = $null
foreach ($rel in $releases) {
    $found = $rel.assets | Where-Object { $_.name -match "x64-setup\.exe$" -and $_.name -notmatch "Lite" } | Select-Object -First 1
    if ($found) { $exeAsset = $found; Write-Host "  Found: $($rel.tag_name)" -ForegroundColor Green; break }
}

if ($exeAsset) {
    $tmpPath = "$env:TEMP\mint-ide-setup.exe"
    Write-Host "  Downloading $($exeAsset.name)..."
    try {
        Invoke-WebRequest -Uri $exeAsset.browser_download_url -OutFile $tmpPath -UseBasicParsing
    } catch {
        Write-Host "  [FAIL] Download failed: $($_.Exception.Message)" -ForegroundColor Red
        Read-Host "Press Enter to close"
        exit 1
    }

    Write-Host ""
    Write-Host "[5/6] Running IDE installer (silent)..." -ForegroundColor Yellow
    # /S = NSIS silent install (Tauri's NSIS bundler supports it). Without it the
    # student must click through the Next/Install/Finish wizard — an extra manual
    # step the one-liner install is supposed to avoid — and cancelling would still
    # fall through to the "complete!" banner below. Already elevated + perMachine
    # means no UAC re-prompt.
    $ideProc = Start-Process -FilePath $tmpPath -ArgumentList "/S" -Wait -PassThru
    Remove-Item $tmpPath -ErrorAction SilentlyContinue
    if ($ideProc.ExitCode -ne 0) {
        Write-Host "  [WARN] IDE installer exit code $($ideProc.ExitCode) — install may be incomplete." -ForegroundColor Yellow
        $script:hadWarnings = $true
    }

    # Re-arm the setup wizard: a config left by an EARLIER install has
    # setup_done=true, so after this reinstall the IDE would skip the wizard
    # entirely. Reset ONLY that flag — custom_venv_path and the other choices
    # are preserved as wizard defaults. (BOM-less write: the IDE's JSON parser
    # rejects a UTF-8 BOM and would fall back to a blank default config,
    # losing custom_venv_path.)
    $cfgPath = Join-Path $env:LOCALAPPDATA "MINT_Exam_IDE\setup_config.json"
    if (Test-Path $cfgPath) {
        try {
            $cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
            $cfg.setup_done = $false
            $json = $cfg | ConvertTo-Json -Depth 8
            [System.IO.File]::WriteAllText($cfgPath, $json, (New-Object System.Text.UTF8Encoding($false)))
            Write-Host "  [OK] Setup wizard will run on next IDE launch" -ForegroundColor Green
        } catch {
            Write-Host "  [WARN] Could not re-arm setup wizard: $_" -ForegroundColor Yellow
        }
    }
} else {
    Write-Host "  [FAIL] No installer found in recent releases." -ForegroundColor Red
    Write-Host "         Manual download: https://github.com/blueion0612/Mint_IDE_Student/releases/latest" -ForegroundColor Cyan
    Read-Host "Press Enter to close"
    exit 1
}

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
if ($script:hadWarnings) {
    Write-Host "  설치가 완료되었지만 경고가 있었습니다 — 위 [WARN] 항목을 확인하세요." -ForegroundColor Yellow
    Write-Host "  (WebView2 / FFmpeg 등이 빠지면 IDE 화면이 안 뜨거나 녹화가 안 될 수 있습니다.)" -ForegroundColor Yellow
} else {
    Write-Host "  Installation complete!" -ForegroundColor Cyan
}
Write-Host "  Python:    $MINT_PY_EXE" -ForegroundColor Gray
if (Test-Path $MINT_GXX_EXE) {
    Write-Host "  C/C++:     $MINT_GXX_EXE" -ForegroundColor Gray
} else {
    Write-Host "  C/C++:     (not installed - C/C++ Run unavailable)" -ForegroundColor Yellow
}
Write-Host "  Launch the IDE from Start Menu. First run opens the setup wizard." -ForegroundColor Gray
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

} catch {
    Write-Host "Error: $_" -ForegroundColor Red
}

Read-Host "Press Enter to close"
