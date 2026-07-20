# MINT Exam IDE (Student)

Anti-cheat coding environment for programming exams.

## Features

- Code editor with syntax highlighting (Python, JS, TS, Java, C, C++)
- File tree with drag-and-drop, folders, rename, import
- Code execution with output panel
- Python venv selector (system or custom virtual environment)
- **Monitoring**: clipboard source detection, keystroke analysis, window focus tracking, file integrity (TAMPER detection), screen recording
- **Full edit history**: every insert/delete logged with type/paste/undo classification
- **Encrypted submission**: AES-256 zip locked with hashed student ID

## Build

```bash
npm install
npx tauri build
```

## Prerequisites

- Node.js >= 18, Rust >= 1.70, FFmpeg
- Language runtimes: Python, Node.js, GCC/G++, JDK (as needed)

## Install

### Windows (관리자 PowerShell)

```powershell
Set-ExecutionPolicy Bypass -Scope Process -Force
irm https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-windows.ps1 | iex
```

Portable Python 3.12.13 (Astral python-build-standalone) + Node + JDK +
FFmpeg + WebView2 + IDE를 한 번에 설치. MSI 충돌 없음 — 기존 Python
3.12.x가 있어도 그대로 같이 두고 우리 전용 portable Python을 별도
경로(`C:\ProgramData\MINT_Python\Python312`)에 압축 해제.
한글 username PC도 자동 처리 (LongPathsEnabled + venv ASCII fallback).

### macOS (source build — Apple Developer cert 없음)

```bash
curl -sL https://raw.githubusercontent.com/blueion0612/Mint_IDE_Student/main/install-mac.sh | bash
```

Xcode CLT + Homebrew + Python 3.12 + Rust + Node + JDK + FFmpeg 설치 후 소스
clone → `npm run tauri build` → `/Applications`에 복사. 5~10분 소요, ~500MB 다운로드.

**첫 실행 권한**: 다이얼로그가 뜨면 **화면 기록(Screen Recording)** + **자동화(Automation →
System Events)** 둘 다 허용 필수. 화면 기록은 허용하면 15초 내 자동으로 녹화가 시작되고,
자동화는 허용 후 IDE 재시작 필요. 다이얼로그를 놓쳤다면 시스템 설정 > 개인정보 보호 및
보안 > 화면 기록 / 자동화 에서 'MINT Exam IDE'를 켜고 IDE를 재시작.
