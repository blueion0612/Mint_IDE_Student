# MINT Exam IDE — Windows Full Installer
# Run in PowerShell (as Administrator recommended):
#   irm https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-windows.ps1 | iex

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  MINT Exam IDE Installer" -ForegroundColor Cyan
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

function Test-Command($cmd) { $null -ne (Get-Command $cmd -ErrorAction SilentlyContinue) }

# ─── 1. Check dependencies ───
Write-Host "[1/4] Checking dependencies..." -ForegroundColor Yellow

$missing = @()

if (Test-Command "python") {
    Write-Host "  [OK] Python ($((python --version 2>&1)))" -ForegroundColor Green
} else {
    Write-Host "  [--] Python — will install" -ForegroundColor Red
    $missing += "Python.Python.3.12"
}

if (Test-Command "node") {
    Write-Host "  [OK] Node.js ($(node --version))" -ForegroundColor Green
} else {
    Write-Host "  [--] Node.js — will install" -ForegroundColor Red
    $missing += "OpenJS.NodeJS.LTS"
}

if (Test-Command "gcc") {
    Write-Host "  [OK] GCC ($(gcc --version 2>&1 | Select-Object -First 1))" -ForegroundColor Green
} else {
    Write-Host "  [--] GCC — will guide manual install" -ForegroundColor Red
}

if (Test-Command "javac") {
    Write-Host "  [OK] JDK ($(javac -version 2>&1))" -ForegroundColor Green
} else {
    Write-Host "  [--] JDK — will install" -ForegroundColor Red
    $missing += "EclipseAdoptium.Temurin.21.JDK"
}

if (Test-Command "ffmpeg") {
    Write-Host "  [OK] FFmpeg" -ForegroundColor Green
} else {
    Write-Host "  [--] FFmpeg — will install" -ForegroundColor Red
    $missing += "Gyan.FFmpeg"
}

Write-Host ""

# ─── 2. Install missing ───
if ($missing.Count -gt 0) {
    Write-Host "[2/4] Installing missing dependencies via winget..." -ForegroundColor Yellow

    foreach ($pkg in $missing) {
        Write-Host "  Installing $pkg..."
        winget install -e --id $pkg --accept-source-agreements --accept-package-agreements 2>&1 | Out-Null
    }

    # GCC needs special handling (MinGW)
    if (-not (Test-Command "gcc")) {
        Write-Host ""
        Write-Host "  [NOTE] GCC (C/C++ compiler) requires MinGW-w64." -ForegroundColor Yellow
        Write-Host "  To install: winget install MSYS2.MSYS2" -ForegroundColor Yellow
        Write-Host "  Then in MSYS2: pacman -S mingw-w64-ucrt-x86_64-gcc" -ForegroundColor Yellow
        Write-Host "  Add C:\msys64\ucrt64\bin to PATH" -ForegroundColor Yellow
        Write-Host "  (C/C++ is optional — Python/JS/Java work without it)" -ForegroundColor Yellow
    }

    # Refresh PATH
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")

    Write-Host ""
} else {
    Write-Host "[2/4] All dependencies already installed." -ForegroundColor Green
}

# ─── 3. Download & Install IDE ───
Write-Host "[3/4] Downloading MINT Exam IDE..." -ForegroundColor Yellow

$release = Invoke-RestMethod "https://api.github.com/repos/blueion0612/Mint_IDE_Student/releases/latest"
$exeAsset = $release.assets | Where-Object { $_.name -match "x64-setup\.exe$" -and $_.name -notmatch "Lite" } | Select-Object -First 1

if (-not $exeAsset) {
    Write-Host "  Error: Could not find installer" -ForegroundColor Red
    exit 1
}

$tmpPath = "$env:TEMP\mint-ide-setup.exe"
Write-Host "  Downloading $($exeAsset.name)..."
Invoke-WebRequest -Uri $exeAsset.browser_download_url -OutFile $tmpPath

Write-Host "[4/4] Running installer..." -ForegroundColor Yellow
Start-Process -FilePath $tmpPath -Wait

Remove-Item $tmpPath -ErrorAction SilentlyContinue

# ─── Done ───
Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  Installation complete!" -ForegroundColor Cyan
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Desktop shortcut should be created."
Write-Host "  You can also find it in Start Menu."
Write-Host ""
