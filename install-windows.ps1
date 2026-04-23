# MINT Exam IDE — Windows Installer
# PowerShell (관리자):
#   Set-ExecutionPolicy Bypass -Scope Process -Force; irm https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-windows.ps1 | iex

$ErrorActionPreference = "Continue"

# Check admin
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host ""
    Write-Host "  [!] Not running as Administrator." -ForegroundColor Yellow
    Write-Host "  Re-run as Administrator for the dedicated Python install step." -ForegroundColor Yellow
    Write-Host '  Right-click PowerShell > "Run as Administrator"' -ForegroundColor Cyan
    Write-Host ""
    $continue = Read-Host "Continue anyway? (y/N)"
    if ($continue -ne "y" -and $continue -ne "Y") { exit 0 }
}

try {

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  MINT Exam IDE Installer" -ForegroundColor Cyan
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

function Test-Cmd($cmd) { $null -ne (Get-Command $cmd -ErrorAction SilentlyContinue) }

# ─── Configuration (hardcoded for reproducibility) ───
$MINT_PY_VERSION = "3.12.8"
$MINT_PY_URL     = "https://www.python.org/ftp/python/$MINT_PY_VERSION/python-$MINT_PY_VERSION-amd64.exe"
$MINT_PY_ROOT    = "C:\ProgramData\MINT_Python\Python312"
$MINT_PY_EXE     = "$MINT_PY_ROOT\python.exe"

# ─── 1. Dedicated Python install (ASCII path, tcl/tk included) ───
Write-Host "[1/4] Installing dedicated Python $MINT_PY_VERSION (with tcl/tk)..." -ForegroundColor Yellow

if (Test-Path $MINT_PY_EXE) {
    $ver = & $MINT_PY_EXE --version 2>&1
    Write-Host "  [OK] Already installed: $ver at $MINT_PY_ROOT" -ForegroundColor Green
} else {
    Write-Host "  Downloading $MINT_PY_URL ..."
    $tmpInstaller = "$env:TEMP\mint-python-$MINT_PY_VERSION.exe"
    try {
        Invoke-WebRequest -Uri $MINT_PY_URL -OutFile $tmpInstaller -UseBasicParsing
    } catch {
        Write-Host "  [FAIL] Download failed: $_" -ForegroundColor Red
        Write-Host "  Check network or try again later." -ForegroundColor Yellow
        Read-Host "Press Enter to close"
        exit 1
    }

    Write-Host "  Installing to $MINT_PY_ROOT (silent, tcl/tk included)..."
    $pyArgs = @(
        "/quiet",
        "InstallAllUsers=1",
        "TargetDir=$MINT_PY_ROOT",
        "PrependPath=0",
        "Include_tcltk=1",
        "Include_pip=1",
        "Include_launcher=0",
        "Include_test=0",
        "Include_doc=0",
        "AssociateFiles=0",
        "Shortcuts=0",
        "CompileAll=0"
    )
    Start-Process -FilePath $tmpInstaller -Wait -ArgumentList $pyArgs
    Remove-Item $tmpInstaller -ErrorAction SilentlyContinue

    if (Test-Path $MINT_PY_EXE) {
        $ver = & $MINT_PY_EXE --version 2>&1
        Write-Host "  [OK] Installed: $ver" -ForegroundColor Green

        # Verify tkinter loads
        $tkCheck = & $MINT_PY_EXE -c "import tkinter; tkinter.Tk().destroy(); print('tkinter OK')" 2>&1
        if ($tkCheck -match "tkinter OK") {
            Write-Host "  [OK] tkinter/TCL verified" -ForegroundColor Green
        } else {
            Write-Host "  [WARN] tkinter self-check failed: $tkCheck" -ForegroundColor Yellow
        }
    } else {
        Write-Host "  [FAIL] Python install did not produce $MINT_PY_EXE" -ForegroundColor Red
        Read-Host "Press Enter to close"
        exit 1
    }
}

Write-Host ""

# ─── 2. Other system deps via winget (Node, JDK, FFmpeg) ───
Write-Host "[2/4] Checking Node.js / JDK / FFmpeg..." -ForegroundColor Yellow

$missing = @()
if (Test-Cmd "node")  { Write-Host "  [OK] Node.js" -ForegroundColor Green } else { Write-Host "  [--] Node.js" -ForegroundColor Red; $missing += "OpenJS.NodeJS.LTS" }
if (Test-Cmd "javac") { Write-Host "  [OK] JDK" -ForegroundColor Green }     else { Write-Host "  [--] JDK" -ForegroundColor Red;     $missing += "EclipseAdoptium.Temurin.21.JDK" }
if (Test-Cmd "ffmpeg"){ Write-Host "  [OK] FFmpeg" -ForegroundColor Green }  else { Write-Host "  [--] FFmpeg" -ForegroundColor Red;  $missing += "Gyan.FFmpeg" }

if ($missing.Count -gt 0) {
    Write-Host "  Installing $($missing.Count) via winget..."
    foreach ($pkg in $missing) {
        Write-Host "    Installing $pkg..."
        try { winget install -e --id $pkg --accept-source-agreements --accept-package-agreements 2>&1 | Out-Null } catch {}
    }
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")
}

Write-Host ""

# ─── 3. Download IDE installer ───
Write-Host "[3/4] Downloading MINT Exam IDE..." -ForegroundColor Yellow

$releases = Invoke-RestMethod "https://api.github.com/repos/blueion0612/Mint_IDE_Student/releases?per_page=10"
$exeAsset = $null
foreach ($rel in $releases) {
    $found = $rel.assets | Where-Object { $_.name -match "x64-setup\.exe$" -and $_.name -notmatch "Lite" } | Select-Object -First 1
    if ($found) { $exeAsset = $found; Write-Host "  Found: $($rel.tag_name)" -ForegroundColor Green; break }
}

if ($exeAsset) {
    $tmpPath = "$env:TEMP\mint-ide-setup.exe"
    Write-Host "  Downloading $($exeAsset.name)..."
    Invoke-WebRequest -Uri $exeAsset.browser_download_url -OutFile $tmpPath -UseBasicParsing

    Write-Host ""
    Write-Host "[4/4] Running IDE installer..." -ForegroundColor Yellow
    Start-Process -FilePath $tmpPath -Wait
    Remove-Item $tmpPath -ErrorAction SilentlyContinue
} else {
    Write-Host "  No installer found in recent releases." -ForegroundColor Yellow
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
