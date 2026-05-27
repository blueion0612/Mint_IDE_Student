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
$MINT_PY_VERSION = "3.12.8"
$MINT_PY_URL     = "https://www.python.org/ftp/python/$MINT_PY_VERSION/python-$MINT_PY_VERSION-amd64.exe"
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

# ─── 1. Dedicated Python install (ASCII path, tcl/tk included) ───
Write-Host "[1/5] Installing dedicated Python $MINT_PY_VERSION (with tcl/tk)..." -ForegroundColor Yellow

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
        Write-Host "  Manual download (then double-click to install with same options):" -ForegroundColor Yellow
        Write-Host "    $MINT_PY_URL" -ForegroundColor Cyan
        Write-Host "  Or pick another Python $MINT_PY_VERSION installer from:" -ForegroundColor Yellow
        Write-Host "    https://www.python.org/downloads/release/python-3128/" -ForegroundColor Cyan
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
    # Capture exit code — silent installer otherwise hides UAC rejection,
    # antivirus blocks, MSI rollback. Test-Path is necessary but not sufficient.
    $pyProc = Start-Process -FilePath $tmpInstaller -Wait -ArgumentList $pyArgs -PassThru
    Remove-Item $tmpInstaller -ErrorAction SilentlyContinue

    # MSI exit 1638 = "another version of this product is already installed".
    # python.org installer enforces a single Python 3.12.x system-wide regardless
    # of TargetDir. Fall back to that existing install if it has tcl/tk.
    if ($pyProc.ExitCode -eq 1638) {
        Write-Host "  [INFO] Another Python 3.12.x is already installed on this PC." -ForegroundColor Yellow
        Write-Host "  Searching for an existing usable Python 3.12..." -ForegroundColor Yellow

        $existingPy = $null
        # 1) py launcher
        if (Test-Cmd "py") {
            $pyPath = & py -3.12 -c "import sys; print(sys.executable)" 2>$null
            if ($LASTEXITCODE -eq 0 -and $pyPath -and (Test-Path $pyPath)) {
                $existingPy = $pyPath
            }
        }
        # 2) Common install locations
        if (-not $existingPy) {
            $candidates = @(
                "C:\Python312\python.exe",
                "C:\Program Files\Python312\python.exe",
                "$env:LOCALAPPDATA\Programs\Python\Python312\python.exe"
            )
            foreach ($c in $candidates) {
                if (Test-Path $c) { $existingPy = $c; break }
            }
        }

        if (-not $existingPy) {
            Write-Host "  [FAIL] 1638 reported but Python 3.12 was not found at known paths." -ForegroundColor Red
            Write-Host "         Open Settings > Apps and uninstall 'Python 3.12.x'," -ForegroundColor Yellow
            Write-Host "         then re-run this installer." -ForegroundColor Yellow
            Read-Host "Press Enter to close"
            exit 1
        }

        Write-Host "  Found: $existingPy" -ForegroundColor Cyan
        $tkCheck = & $existingPy -c "import tkinter; tkinter.Tk().destroy(); print('tkinter OK')" 2>&1
        if ($tkCheck -notmatch "tkinter OK") {
            Write-Host "  [FAIL] Existing Python 3.12 lacks tkinter:" -ForegroundColor Red
            Write-Host "         $tkCheck" -ForegroundColor DarkGray
            Write-Host "         matplotlib plt.show() will not work at exam time." -ForegroundColor Yellow
            Write-Host "         Fix: Settings > Apps > uninstall 'Python 3.12.x'," -ForegroundColor Yellow
            Write-Host "              then re-run this installer (it will reinstall with tcl/tk)." -ForegroundColor Yellow
            Read-Host "Press Enter to close"
            exit 1
        }
        Write-Host "  [OK] Existing Python 3.12 has tkinter — using it." -ForegroundColor Green
        # Redirect the rest of the script to the existing Python. IDE's
        # find_system_python will still pick up MINT_PY_EXE if it exists
        # later; here we just verify the environment is usable.
        $MINT_PY_EXE = $existingPy
        # Skip the rest of the install-block; the verified Python is good.
    }
    elseif ($pyProc.ExitCode -ne 0) {
        Write-Host "  [FAIL] Python installer exited with code $($pyProc.ExitCode)." -ForegroundColor Red
        Write-Host "  Common causes: antivirus blocked the installer, UAC denied, low disk space." -ForegroundColor Yellow
        Read-Host "Press Enter to close"
        exit 1
    }

    if (Test-Path $MINT_PY_EXE) {
        $ver = & $MINT_PY_EXE --version 2>&1
        Write-Host "  [OK] Installed: $ver" -ForegroundColor Green

        # Verify tkinter loads — matplotlib GUI (plt.show) depends on it.
        # Fail fast here rather than letting the student discover it mid-exam.
        $tkCheck = & $MINT_PY_EXE -c "import tkinter; tkinter.Tk().destroy(); print('tkinter OK')" 2>&1
        if ($tkCheck -match "tkinter OK") {
            Write-Host "  [OK] tkinter/TCL verified" -ForegroundColor Green
        } else {
            Write-Host "  [FAIL] tkinter self-check failed: $tkCheck" -ForegroundColor Red
            Write-Host "  This blocks matplotlib plt.show() at exam time." -ForegroundColor Red
            Write-Host "  Likely cause: python.org installer ran without Include_tcltk=1." -ForegroundColor Yellow
            Write-Host "  Remove $MINT_PY_ROOT and re-run this script." -ForegroundColor Yellow
            Read-Host "Press Enter to close"
            exit 1
        }
    } else {
        Write-Host "  [FAIL] Python install did not produce $MINT_PY_EXE" -ForegroundColor Red
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
            Write-Host "    IDE first launch may show a blank window." -ForegroundColor Yellow
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
