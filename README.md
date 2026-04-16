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

Download the installer from [Releases](../../releases) and run it. Desktop shortcut is created automatically.
