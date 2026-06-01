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

# ─── 0. System policy: enable long paths (manifest alone is not enough) ───
# Windows 10 1607+ requires BOTH a process manifest with longPathAware=true
# AND HKLM\SYSTEM\CCS\Control\FileSystem\LongPathsEnabled=1. Otherwise
# Korean usernames + nested workspace paths over 260 chars break workspace.rs
# operations with ERROR_FILENAME_EXCED_RANGE. We already have admin here.
Write-Host "[0/5] Enabling Windows long path support..." -ForegroundColor Yellow
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
Write-Host "[1/5] Setting up portable Python $MINT_PY_VERSION..." -ForegroundColor Yellow

# Already extracted from a previous run? Skip download.
if (Test-Path $MINT_PY_EXE) {
    $ver = & $MINT_PY_EXE --version 2>&1
    Write-Host "  [OK] Already present: $ver at $MINT_PY_ROOT" -ForegroundColor Green
} else {
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
Write-Host "[2/5] Checking Node.js / JDK / FFmpeg / WebView2..." -ForegroundColor Yellow

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
} else {
    $missing = @()
    if (Test-Cmd "node")  { Write-Host "  [OK] Node.js" -ForegroundColor Green } else { Write-Host "  [--] Node.js" -ForegroundColor Red; $missing += "OpenJS.NodeJS.LTS" }
    if (Test-Cmd "javac") { Write-Host "  [OK] JDK" -ForegroundColor Green }     else { Write-Host "  [--] JDK" -ForegroundColor Red;     $missing += "EclipseAdoptium.Temurin.21.JDK" }
    if (Test-Cmd "ffmpeg"){ Write-Host "  [OK] FFmpeg" -ForegroundColor Green }  else { Write-Host "  [--] FFmpeg" -ForegroundColor Red;  $missing += "Gyan.FFmpeg" }

    if ($missing.Count -gt 0) {
        Write-Host "  Installing $($missing.Count) via winget..."
        foreach ($pkg in $missing) {
            Write-Host "    Installing $pkg..."
            try {
                $wingetOut = winget install -e --id $pkg --accept-source-agreements --accept-package-agreements 2>&1
                if ($LASTEXITCODE -ne 0) {
                    Write-Host "    [WARN] $pkg install exit code $LASTEXITCODE" -ForegroundColor Yellow
                    Write-Host "      $($wingetOut | Out-String)" -ForegroundColor DarkGray
                }
            } catch {
                Write-Host "    [WARN] $pkg install threw: $_" -ForegroundColor Yellow
            }
        }
        $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")

        # Post-install verification — if FFmpeg specifically didn't land,
        # recording won't work mid-exam. Make sure the student knows.
        if (-not (Test-Cmd "ffmpeg")) {
            Write-Host "  [WARN] ffmpeg still missing after winget install." -ForegroundColor Yellow
            Write-Host "         Screen recording will not work until this is fixed." -ForegroundColor Yellow
        }
    }
}

Write-Host ""

# ─── 3. Download IDE installer ───
Write-Host "[3/5] Downloading MINT Exam IDE..." -ForegroundColor Yellow

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
    Write-Host "[4/5] Running IDE installer..." -ForegroundColor Yellow
    $ideProc = Start-Process -FilePath $tmpPath -Wait -PassThru
    Remove-Item $tmpPath -ErrorAction SilentlyContinue
    if ($ideProc.ExitCode -ne 0) {
        Write-Host "  [WARN] IDE installer exit code $($ideProc.ExitCode) — install may be incomplete." -ForegroundColor Yellow
    }
} else {
    Write-Host "  [FAIL] No installer found in recent releases." -ForegroundColor Red
    Write-Host "         Manual download: https://github.com/blueion0612/Mint_IDE_Student/releases/latest" -ForegroundColor Cyan
    Read-Host "Press Enter to close"
    exit 1
}

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  Installation complete!" -ForegroundColor Cyan
Write-Host "  Python:    $MINT_PY_EXE" -ForegroundColor Gray
Write-Host "  Launch the IDE from Start Menu. First run opens the setup wizard." -ForegroundColor Gray
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

} catch {
    Write-Host "Error: $_" -ForegroundColor Red
}

Read-Host "Press Enter to close"
