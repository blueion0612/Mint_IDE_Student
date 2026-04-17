# MINT Exam IDE — Windows Full Installer
# Run in PowerShell (as Administrator):
#   Set-ExecutionPolicy Bypass -Scope Process -Force; irm https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-windows.ps1 | iex

$ErrorActionPreference = "Stop"

try {

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  MINT Exam IDE Installer" -ForegroundColor Cyan
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

function Test-Cmd($cmd) { $null -ne (Get-Command $cmd -ErrorAction SilentlyContinue) }

# ─── 1. Check dependencies ───
Write-Host "[1/4] Checking dependencies..." -ForegroundColor Yellow

$missing = @()

if (Test-Cmd "python") {
    Write-Host "  [OK] Python ($((python --version 2>&1)))" -ForegroundColor Green
} else {
    Write-Host "  [--] Python" -ForegroundColor Red
    $missing += "Python.Python.3.12"
}

if (Test-Cmd "node") {
    Write-Host "  [OK] Node.js ($(node --version))" -ForegroundColor Green
} else {
    Write-Host "  [--] Node.js" -ForegroundColor Red
    $missing += "OpenJS.NodeJS.LTS"
}

if (Test-Cmd "gcc") {
    Write-Host "  [OK] GCC" -ForegroundColor Green
} else {
    Write-Host "  [--] GCC (optional, for C/C++)" -ForegroundColor DarkYellow
}

if (Test-Cmd "javac") {
    Write-Host "  [OK] JDK" -ForegroundColor Green
} else {
    Write-Host "  [--] JDK" -ForegroundColor Red
    $missing += "EclipseAdoptium.Temurin.21.JDK"
}

if (Test-Cmd "ffmpeg") {
    Write-Host "  [OK] FFmpeg" -ForegroundColor Green
} else {
    Write-Host "  [--] FFmpeg" -ForegroundColor Red
    $missing += "Gyan.FFmpeg"
}

Write-Host ""

# ─── 2. Install missing ───
if ($missing.Count -gt 0) {
    Write-Host "[2/4] Installing $($missing.Count) packages via winget..." -ForegroundColor Yellow

    foreach ($pkg in $missing) {
        Write-Host "  Installing $pkg..."
        try {
            winget install -e --id $pkg --accept-source-agreements --accept-package-agreements 2>&1 | Out-Null
        } catch {
            Write-Host "    Warning: $pkg install may have failed" -ForegroundColor Yellow
        }
    }

    # Refresh PATH so newly installed tools are found
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")
    Write-Host ""
} else {
    Write-Host "[2/4] All dependencies installed." -ForegroundColor Green
}

# ─── 3. Download IDE ───
Write-Host "[3/4] Downloading MINT Exam IDE..." -ForegroundColor Yellow

# Find the release that has .exe assets (latest with assets)
$releases = Invoke-RestMethod "https://api.github.com/repos/blueion0612/Mint_IDE_Student/releases?per_page=10"
$exeAsset = $null

foreach ($rel in $releases) {
    $found = $rel.assets | Where-Object { $_.name -match "x64-setup\.exe$" -and $_.name -notmatch "Lite" } | Select-Object -First 1
    if ($found) {
        $exeAsset = $found
        Write-Host "  Found: $($rel.tag_name) — $($found.name)" -ForegroundColor Green
        break
    }
}

if (-not $exeAsset) {
    Write-Host "  No installer found in releases. Building from source..." -ForegroundColor Yellow
    Write-Host ""
    Write-Host "  Please run these commands manually:" -ForegroundColor Cyan
    Write-Host "    git clone https://github.com/blueion0612/Mint_IDE_Student" -ForegroundColor White
    Write-Host "    cd Mint_IDE_Student" -ForegroundColor White
    Write-Host "    npm install && npx tauri build" -ForegroundColor White
    Write-Host ""
    Read-Host "Press Enter to exit"
    exit 0
}

$tmpPath = "$env:TEMP\mint-ide-setup.exe"
Write-Host "  Downloading..."
Invoke-WebRequest -Uri $exeAsset.browser_download_url -OutFile $tmpPath -UseBasicParsing

# ─── 4. Install ───
Write-Host "[4/4] Running installer..." -ForegroundColor Yellow
Start-Process -FilePath $tmpPath -Wait

Remove-Item $tmpPath -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  Installation complete!" -ForegroundColor Cyan
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

} catch {
    Write-Host ""
    Write-Host "Error: $_" -ForegroundColor Red
    Write-Host ""
}

Read-Host "Press Enter to close"
