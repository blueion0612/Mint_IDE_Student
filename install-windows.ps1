# MINT Exam IDE — Windows Full Installer
# PowerShell (관리자):
#   Set-ExecutionPolicy Bypass -Scope Process -Force; irm https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-windows.ps1 | iex

$ErrorActionPreference = "Continue"

# Check admin
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host ""
    Write-Host "  [!] Not running as Administrator." -ForegroundColor Yellow
    Write-Host "  Some installations may fail. Please re-run as Administrator:" -ForegroundColor Yellow
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

# ─── 1. Check dependencies ───
Write-Host "[1/5] Checking dependencies..." -ForegroundColor Yellow

$missing = @()

if (Test-Cmd "python") {
    Write-Host "  [OK] Python ($((python --version 2>&1)))" -ForegroundColor Green
} else {
    Write-Host "  [--] Python" -ForegroundColor Red
    $missing += "Python.Python.3.12"
}

if (Test-Cmd "node") {
    Write-Host "  [OK] Node.js" -ForegroundColor Green
} else {
    Write-Host "  [--] Node.js" -ForegroundColor Red
    $missing += "OpenJS.NodeJS.LTS"
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

# ─── 2. Install missing system deps ───
if ($missing.Count -gt 0) {
    Write-Host "[2/5] Installing $($missing.Count) system packages..." -ForegroundColor Yellow
    foreach ($pkg in $missing) {
        Write-Host "  Installing $pkg..."
        try { winget install -e --id $pkg --accept-source-agreements --accept-package-agreements 2>&1 | Out-Null } catch {}
    }
    $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path", "User")
    Write-Host ""
} else {
    Write-Host "[2/5] All system deps installed." -ForegroundColor Green
}

# ─── 3. Create exam Python venv + install packages ───
Write-Host "[3/5] Setting up exam Python environment..." -ForegroundColor Yellow

$venvDir = "$env:LOCALAPPDATA\MINT_Exam_IDE\exam-venv"
$venvPy = "$venvDir\Scripts\python.exe"

# Find Python
$pyCmd = $null
if (Test-Cmd "python") { $pyCmd = "python" }
elseif (Test-Cmd "py") { $pyCmd = "py" }

if ($pyCmd) {
    if (-not (Test-Path $venvPy)) {
        Write-Host "  Creating venv..."
        & $pyCmd -m venv $venvDir
    }

    Write-Host "  Installing packages (this may take 5-10 minutes)..."
    Write-Host "    numpy, pandas, matplotlib, seaborn, scikit-learn, scipy, sympy," -ForegroundColor DarkGray
    Write-Host "    Pillow, opencv, openpyxl, requests, torch, tensorflow" -ForegroundColor DarkGray

    # All pip/python commands go through cmd /c ... 2>NUL
    # to prevent TensorFlow's C++ stderr from leaking into PowerShell
    cmd /c "`"$venvPy`" -m pip install --upgrade pip 2>NUL" | Out-Null
    cmd /c "`"$venvPy`" -m pip install numpy pandas matplotlib seaborn scikit-learn scipy sympy Pillow opencv-python-headless openpyxl requests 2>NUL" | Out-Null
    Write-Host "  [OK] Core packages" -ForegroundColor Green

    Write-Host "  Installing PyTorch (CPU)..."
    cmd /c "`"$venvPy`" -m pip install torch torchvision torchaudio --index-url https://download.pytorch.org/whl/cpu 2>NUL" | Out-Null
    Write-Host "  [OK] PyTorch" -ForegroundColor Green

    Write-Host "  Installing TensorFlow (CPU)..."
    cmd /c "`"$venvPy`" -m pip install tensorflow-cpu 2>NUL" | Out-Null
    Write-Host "  [OK] TensorFlow" -ForegroundColor Green

    # Verify
    $check = cmd /c "`"$venvPy`" -c `"import numpy,pandas,matplotlib,seaborn,sklearn,scipy,sympy,cv2,torch; print('ALL OK')`" 2>NUL"
    if ($check -match "ALL OK") {
        Write-Host "  All packages verified!" -ForegroundColor Green
    } else {
        Write-Host "  Warning: some packages may have failed" -ForegroundColor Yellow
    }
} else {
    Write-Host "  [SKIP] Python not found — exam venv not created" -ForegroundColor Yellow
}

Write-Host ""

# ─── 4. Download & Install IDE ───
Write-Host "[4/5] Downloading MINT Exam IDE..." -ForegroundColor Yellow

$releases = Invoke-RestMethod "https://api.github.com/repos/blueion0612/Mint_IDE_Student/releases?per_page=10"
$exeAsset = $null
foreach ($rel in $releases) {
    $found = $rel.assets | Where-Object { $_.name -match "x64-setup\.exe$" -and $_.name -notmatch "Lite" } | Select-Object -First 1
    if ($found) { $exeAsset = $found; Write-Host "  Found: $($rel.tag_name)" -ForegroundColor Green; break }
}

if ($exeAsset) {
    $tmpPath = "$env:TEMP\mint-ide-setup.exe"
    Write-Host "  Downloading..."
    Invoke-WebRequest -Uri $exeAsset.browser_download_url -OutFile $tmpPath -UseBasicParsing

    Write-Host "[5/5] Running installer..." -ForegroundColor Yellow
    Start-Process -FilePath $tmpPath -Wait
    Remove-Item $tmpPath -ErrorAction SilentlyContinue
} else {
    Write-Host "  No installer found. Install from source:" -ForegroundColor Yellow
    Write-Host "    git clone https://github.com/blueion0612/Mint_IDE_Student" -ForegroundColor White
    Write-Host "    cd Mint_IDE_Student && npm install && npx tauri build" -ForegroundColor White
}

Write-Host ""
Write-Host "==============================" -ForegroundColor Cyan
Write-Host "  Installation complete!" -ForegroundColor Cyan
Write-Host "==============================" -ForegroundColor Cyan
Write-Host ""

} catch {
    Write-Host "Error: $_" -ForegroundColor Red
}

Read-Host "Press Enter to close"
