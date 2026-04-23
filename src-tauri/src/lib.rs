mod monitor;
mod recorder;
mod runner;
mod setup;
mod workspace;

use monitor::{ActivityEvent, ActivityLog, KnownWrites, new_known_writes, mark_known_write};
use recorder::{RecorderState, ScreenRecorder};
use setup::SetupConfig;
use workspace::{FileNode, Workspace, WorkspaceState};
use std::sync::Mutex;
use tauri::{Emitter, State};

struct AppState {
    activity_log: Mutex<ActivityLog>,
}

use serde::Serialize;

// ===== Activity Log Commands =====

#[tauri::command]
fn get_activity_log(state: State<AppState>) -> Vec<ActivityEvent> {
    state.activity_log.lock().unwrap().get_events()
}

#[tauri::command]
fn clear_activity_log(state: State<AppState>) {
    state.activity_log.lock().unwrap().clear();
}

#[tauri::command]
fn export_activity_log(state: State<AppState>) -> String {
    let events = state.activity_log.lock().unwrap().get_events();
    serde_json::to_string_pretty(&events).unwrap_or_default()
}

#[tauri::command]
fn log_editor_event(
    state: State<AppState>,
    event_type: String,
    detail: String,
    char_count: Option<u32>,
    time_delta_ms: Option<f64>,
) {
    let event = ActivityEvent::new(&event_type, &detail, char_count, time_delta_ms);
    state.activity_log.lock().unwrap().add_event(event);
}

// ===== Code Execution =====

#[tauri::command]
fn run_code(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    ws: State<WorkspaceState>,
    kw: State<KnownWrites>,
    process: State<runner::RunningProcess>,
    language: String,
    code: String,
    filename: String,
    python_path: Option<String>,
) -> Result<(), String> {
    // Save file to workspace so imports work.
    // Register the write with known_writes BEFORE write_file so the integrity
    // monitor's next polling pass doesn't flag our own auto-save as tampering.
    if let Ok(guard) = ws.lock() {
        if let Some(ref workspace) = *guard {
            mark_known_write(&kw, &filename);
            let _ = workspace.write_file(&filename, &code);
        }
    }

    let event = ActivityEvent::new(
        "code_run",
        &format!("Running {} ({})", filename, language),
        Some(code.len() as u32),
        None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

    let cwd = ws.lock().ok()
        .and_then(|g| g.as_ref().map(|w| w.root_path()));

    runner::execute_code_streaming(
        &language, &code, &filename,
        cwd.as_deref(),
        python_path.as_deref(),
        app_handle,
        (*process).clone(),
    );

    Ok(())
}

/// Blocking code execution — for notebooks. Returns stdout+stderr directly.
#[tauri::command]
async fn run_code_sync(
    ws: State<'_, WorkspaceState>,
    language: String,
    code: String,
    filename: String,
    python_path: Option<String>,
) -> Result<(String, String, Option<i32>), String> {
    let cwd = ws.lock().ok().and_then(|g| g.as_ref().map(|w| w.root_path()));
    let work_dir = cwd.clone().unwrap_or_else(|| std::env::temp_dir().to_string_lossy().to_string());
    let hidden_name = format!(".{}", filename);
    let file_path = std::path::PathBuf::from(&work_dir).join(&hidden_name);
    std::fs::write(&file_path, &code).map_err(|e| e.to_string())?;

    let py = runner::find_python_cached(python_path.as_deref())
        .unwrap_or("python".to_string());
    let mut command = std::process::Command::new(&py);
    command.arg(&file_path);
    command.current_dir(&work_dir);
    command.env("PYTHONUNBUFFERED", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .env("TF_CPP_MIN_LOG_LEVEL", "3");

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let output = command.output().map_err(|e| e.to_string())?;

    // Cleanup temp file
    let _ = std::fs::remove_file(&file_path);

    Ok((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code(),
    ))
}

#[tauri::command]
fn stop_code(process: State<runner::RunningProcess>) -> bool {
    runner::stop_process(&process)
}

#[tauri::command]
fn pip_install_packages(
    app_handle: tauri::AppHandle,
    packages: Vec<String>,
    python_path: Option<String>,
) {
    runner::pip_install(&packages, python_path.as_deref(), app_handle);
}

// ===== Screen Recording =====

#[tauri::command]
fn start_recording(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    recorder: State<RecorderState>,
    output_dir: Option<String>,
) -> Result<String, String> {
    let mut rec = recorder.lock().map_err(|e| e.to_string())?;
    let dir = output_dir.unwrap_or_else(|| setup::recordings_dir().to_string_lossy().to_string());
    let path = rec.start(&dir)?;
    let strategy = rec.last_strategy().unwrap_or_else(|| "unknown".to_string());

    let event = ActivityEvent::new(
        "recording_start",
        &format!("Screen recording started [{}]: {}", strategy, path),
        None, None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

    Ok(path)
}

#[tauri::command]
fn stop_recording(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    recorder: State<RecorderState>,
) -> Result<String, String> {
    let mut rec = recorder.lock().map_err(|e| e.to_string())?;
    let path = rec.stop()?;

    let event = ActivityEvent::new("recording_stop", &format!("Screen recording saved: {}", path), None, None);
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

    Ok(path)
}

#[tauri::command]
fn is_recording(recorder: State<RecorderState>) -> bool {
    recorder.lock().map(|r| r.is_recording()).unwrap_or(false)
}

#[tauri::command]
fn get_home_dir() -> Result<String, String> {
    dirs::home_dir()
        .or_else(|| std::env::var("USERPROFILE").ok().map(std::path::PathBuf::from))
        .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from))
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "Cannot determine home directory".to_string())
}

#[tauri::command]
fn get_recordings_dir() -> String {
    setup::recordings_dir().to_string_lossy().to_string()
}

#[tauri::command]
fn get_workspaces_dir() -> String {
    setup::workspaces_dir().to_string_lossy().to_string()
}

// ===== Setup Config =====

#[tauri::command]
fn read_setup_config() -> SetupConfig {
    setup::load_config()
}

#[tauri::command]
fn write_setup_config(config: SetupConfig) -> Result<(), String> {
    setup::save_config(&config)
}

#[tauri::command]
fn package_list_for_profile(profile: String, custom: Vec<String>) -> Vec<String> {
    setup::package_list_for_profile(&profile, &custom)
}

// ===== Pip Package Management =====

/// Install packages with smart routing for torch / tensorflow.
/// Streams output via run-output events; emits run-done when finished.
#[tauri::command]
fn install_packages_smart(
    app_handle: tauri::AppHandle,
    packages: Vec<String>,
    python_path: Option<String>,
) {
    runner::pip_install_smart(&packages, python_path.as_deref(), app_handle);
}

#[tauri::command]
fn uninstall_packages(
    app_handle: tauri::AppHandle,
    packages: Vec<String>,
    python_path: Option<String>,
) {
    runner::pip_uninstall(&packages, python_path.as_deref(), app_handle);
}

#[tauri::command]
fn list_installed_packages(python_path: Option<String>) -> Vec<String> {
    runner::pip_list(python_path.as_deref())
}

// ===== Sample Code Helpers =====

#[tauri::command]
fn delete_sample_files(ws: State<WorkspaceState>) -> Result<u32, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized")?;
    let root = std::path::PathBuf::from(workspace.root_path());
    let sample_names = [
        "test_all.py", "test_import.py", "test_popup.py", "test_notebook.ipynb",
        "main.py", "utils",
    ];
    let mut removed = 0u32;
    for name in sample_names {
        let p = root.join(name);
        if p.exists() {
            if p.is_dir() {
                if std::fs::remove_dir_all(&p).is_ok() { removed += 1; }
            } else if std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

// ===== Python Interpreter Detection =====

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PythonInfo {
    path: String,
    version: String,
    label: String, // "System Python 3.12", "venv: myenv", etc.
}

#[tauri::command]
fn detect_pythons() -> Result<Vec<PythonInfo>, String> {
    let mut results = Vec::new();

    // 1. System pythons
    for cmd in ["python3", "python"] {
        if let Some(info) = probe_python(cmd) {
            results.push(info);
        }
    }

    // 2. Conda base installs
    let home = dirs::home_dir().unwrap_or_default();
    let conda_bases = [
        home.join("anaconda3"),
        home.join("miniconda3"),
        home.join("Anaconda3"),
        home.join("Miniconda3"),
        home.join("miniforge3"),
        home.join("mambaforge"),
    ];

    for base in &conda_bases {
        let py = if cfg!(windows) {
            base.join("python.exe")
        } else {
            base.join("bin").join("python")
        };
        if py.exists() {
            let name = base.file_name().unwrap().to_string_lossy().to_string();
            if let Some(mut info) = probe_python(&py.to_string_lossy()) {
                info.label = format!("conda: {} (base)", name);
                results.push(info);
            }
        }
    }

    // 3. Conda envs + virtualenvs
    let search_dirs = [
        home.join("envs"),
        home.join(".virtualenvs"),
        home.join("anaconda3").join("envs"),
        home.join("miniconda3").join("envs"),
        home.join("Anaconda3").join("envs"),
        home.join("Miniconda3").join("envs"),
        home.join("miniforge3").join("envs"),
        home.join("mambaforge").join("envs"),
    ];

    for dir in &search_dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_dir() { continue; }
                let py = if cfg!(windows) {
                    p.join("python.exe")  // conda envs on Windows: envs/name/python.exe
                } else {
                    p.join("bin").join("python")
                };
                // Also check Scripts/ for conda on Windows
                let py = if py.exists() { py } else if cfg!(windows) {
                    p.join("Scripts").join("python.exe")
                } else { py };

                if py.exists() {
                    let name = p.file_name().unwrap().to_string_lossy().to_string();
                    if let Some(mut info) = probe_python(&py.to_string_lossy()) {
                        info.label = format!("env: {}", name);
                        results.push(info);
                    }
                }
            }
        }
    }

    Ok(results)
}

fn silent_cmd(cmd: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut command = std::process::Command::new(cmd);
    command.args(args);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command.output().ok()
}

fn probe_python(cmd: &str) -> Option<PythonInfo> {
    let output = silent_cmd(cmd, &["--version"])?;
    let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let ver = if ver.is_empty() { String::from_utf8_lossy(&output.stderr).trim().to_string() } else { ver };
    if ver.is_empty() { return None; }

    let path_output = silent_cmd(cmd, &["-c", "import sys; print(sys.executable)"])?;
    let real_path = String::from_utf8_lossy(&path_output.stdout).trim().to_string();

    Some(PythonInfo {
        path: if real_path.is_empty() { cmd.to_string() } else { real_path },
        label: format!("System {}", ver),
        version: ver,
    })
}

// ===== Save Code Edit History =====

#[tauri::command]
fn save_code_history(ws: State<WorkspaceState>, history_json: String) -> Result<(), String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace")?;
    workspace.write_file("_log_code_history.json", &history_json)
}

// ===== Exam Python Environment =====

fn venv_dir_from_config() -> std::path::PathBuf {
    let cfg = setup::load_config();
    match cfg.custom_venv_path {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => setup::default_venv_path(),
    }
}

fn python_exe_in_venv(venv_dir: &std::path::Path) -> std::path::PathBuf {
    if cfg!(windows) {
        venv_dir.join("Scripts").join("python.exe")
    } else {
        venv_dir.join("bin").join("python")
    }
}

fn try_create_venv(target: &std::path::Path, sys_python: &str) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let target_str = target.to_string_lossy().to_string();
    let out = silent_cmd(sys_python, &["-m", "venv", &target_str]);
    if out.is_none() || !out.as_ref().unwrap().status.success() {
        let err = out.and_then(|o| String::from_utf8(o.stderr).ok()).unwrap_or_default();
        return Err(if err.is_empty() { "unknown error".to_string() } else { err });
    }
    if !python_exe_in_venv(target).exists() {
        return Err("python executable missing after venv creation".to_string());
    }
    Ok(())
}

/// Ensure the exam venv exists and return its python executable path.
/// Honors `custom_venv_path` from SetupConfig if set. Auto-falls-back to
/// `C:\ProgramData\MINT_Exam_IDE\exam-venv` (ASCII) if default path fails —
/// covers Windows users whose `%LOCALAPPDATA%` contains non-ASCII characters.
#[tauri::command]
fn setup_exam_python(app_handle: tauri::AppHandle) -> Result<String, String> {
    let venv_dir = venv_dir_from_config();
    let py_exe = python_exe_in_venv(&venv_dir);

    if py_exe.exists() {
        return Ok(py_exe.to_string_lossy().to_string());
    }

    let sys_python = find_system_python()
        .ok_or("Python not found. Please install Python first.")?;

    let _ = app_handle.emit("run-output", runner::RunOutputLine {
        stream: "system".to_string(),
        text: format!("Creating exam Python venv at {}...\nUsing: {}\n", venv_dir.display(), sys_python),
    });

    match try_create_venv(&venv_dir, &sys_python) {
        Ok(()) => {
            let _ = app_handle.emit("run-output", runner::RunOutputLine {
                stream: "system".to_string(),
                text: "Exam venv ready.\n".to_string(),
            });
            Ok(python_exe_in_venv(&venv_dir).to_string_lossy().to_string())
        }
        Err(primary_err) => {
            // Fallback: ASCII-guaranteed path (covers non-ASCII Windows usernames).
            #[cfg(target_os = "windows")]
            {
                let fallback = std::path::PathBuf::from(r"C:\ProgramData\MINT_Exam_IDE\exam-venv");
                if fallback != venv_dir {
                    let _ = app_handle.emit("run-output", runner::RunOutputLine {
                        stream: "system".to_string(),
                        text: format!(
                            "Primary venv creation failed ({}). Retrying at ASCII path {}...\n",
                            primary_err.trim(),
                            fallback.display()
                        ),
                    });
                    if let Ok(()) = try_create_venv(&fallback, &sys_python) {
                        let fallback_str = fallback.to_string_lossy().to_string();
                        let mut cfg = setup::load_config();
                        cfg.custom_venv_path = Some(fallback_str.clone());
                        let _ = setup::save_config(&cfg);
                        let _ = app_handle.emit("run-output", runner::RunOutputLine {
                            stream: "system".to_string(),
                            text: format!("Fallback venv ready at {}.\n", fallback.display()),
                        });
                        return Ok(python_exe_in_venv(&fallback).to_string_lossy().to_string());
                    }
                }
            }
            Err(format!("Failed to create Python venv at {}: {}", venv_dir.display(), primary_err))
        }
    }
}

#[tauri::command]
fn get_current_venv_path() -> String {
    venv_dir_from_config().to_string_lossy().to_string()
}

#[tauri::command]
fn get_default_venv_path() -> String {
    setup::default_venv_path().to_string_lossy().to_string()
}

/// Delete existing venv (if any) and create a new one at `path` (or default).
/// Saves the resulting path to SetupConfig.custom_venv_path.
#[tauri::command]
fn recreate_venv(app_handle: tauri::AppHandle, path: Option<String>) -> Result<String, String> {
    let target_dir = match path.as_deref() {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => setup::default_venv_path(),
    };

    // Wipe existing venv at target (if exists)
    if target_dir.exists() {
        std::fs::remove_dir_all(&target_dir)
            .map_err(|e| format!("Failed to remove existing venv: {}", e))?;
    }

    // Ensure parent directory exists
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create parent directory: {}", e))?;
    }

    let sys_python = find_system_python()
        .ok_or("Python not found. Please install Python first.")?;

    let _ = app_handle.emit("run-output", runner::RunOutputLine {
        stream: "system".to_string(),
        text: format!("Creating new venv at {}...\n", target_dir.display()),
    });

    let target_str = target_dir.to_string_lossy().to_string();
    let output = silent_cmd(&sys_python, &["-m", "venv", &target_str]);
    if output.is_none() || !output.as_ref().unwrap().status.success() {
        let err_detail = output
            .and_then(|o| String::from_utf8(o.stderr).ok())
            .unwrap_or_else(|| "unknown".to_string());
        return Err(format!("venv creation failed at {}: {}", target_dir.display(), err_detail));
    }

    let py_exe = python_exe_in_venv(&target_dir);
    if !py_exe.exists() {
        return Err(format!("venv created but python.exe not found at {}", py_exe.display()));
    }

    // Save path to config (None if it's the default)
    let mut cfg = setup::load_config();
    let default = setup::default_venv_path();
    cfg.custom_venv_path = if target_dir == default {
        None
    } else {
        Some(target_str.clone())
    };
    setup::save_config(&cfg)?;

    Ok(py_exe.to_string_lossy().to_string())
}

fn install_exam_packages(py_str: &str, app_handle: &tauri::AppHandle) -> Result<String, String> {
    let _ = app_handle.emit("run-output", runner::RunOutputLine {
        stream: "system".to_string(),
        text: "Installing exam packages...\nThis may take a few minutes on first run.\n".to_string(),
    });

    let packages = [
        "numpy", "pandas", "matplotlib", "seaborn",
        "scikit-learn", "scipy", "sympy",
        "Pillow", "opencv-python-headless",
        "openpyxl", "requests",
    ];

    let mut args: Vec<&str> = vec!["-m", "pip", "install"];
    for pkg in &packages {
        args.push(pkg);
    }
    let _ = silent_cmd(py_str, &args);

    // PyTorch CPU
    let _ = silent_cmd(py_str, &[
        "-m", "pip", "install",
        "torch", "torchvision", "torchaudio",
        "--index-url", "https://download.pytorch.org/whl/cpu",
    ]);

    // TensorFlow CPU (may fail on some platforms — that's OK)
    let _ = silent_cmd(py_str, &["-m", "pip", "install", "tensorflow-cpu"]);

    let _ = app_handle.emit("run-output", runner::RunOutputLine {
        stream: "system".to_string(),
        text: "Exam Python environment ready!\n".to_string(),
    });

    Ok(py_str.to_string())
}

fn find_system_python() -> Option<String> {
    // Priority 1: MINT-dedicated Python (installed by install-windows.ps1 — ASCII path + tcl/tk verified).
    #[cfg(target_os = "windows")]
    {
        let mint_py = "C:\\ProgramData\\MINT_Python\\Python312\\python.exe";
        if std::path::Path::new(mint_py).exists() {
            return Some(mint_py.to_string());
        }
    }

    let candidates = if cfg!(target_os = "windows") {
        vec!["python", "python3", "py"]
    } else {
        vec!["python3", "python"]
    };
    for cmd in &candidates {
        if let Some(out) = silent_cmd(cmd, &["--version"]) {
            if out.status.success() {
                return Some(cmd.to_string());
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        for name in ["anaconda3", "miniconda3", "Anaconda3", "Miniconda3"] {
            let py = format!("{}\\{}\\python.exe", home, name);
            if std::path::Path::new(&py).exists() { return Some(py); }
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let base = std::path::PathBuf::from(local).join("Programs").join("Python");
            if let Ok(entries) = std::fs::read_dir(&base) {
                for entry in entries.flatten() {
                    let py = entry.path().join("python.exe");
                    if py.exists() { return Some(py.to_string_lossy().to_string()); }
                }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // GUI-launched apps on macOS have a stripped PATH; search known locations.
        let home = std::env::var("HOME").unwrap_or_default();
        let mut candidates: Vec<String> = vec![
            "/opt/homebrew/bin/python3".to_string(),
            "/usr/local/bin/python3".to_string(),
            "/usr/bin/python3".to_string(),
            format!("{}/anaconda3/bin/python", home),
            format!("{}/miniconda3/bin/python", home),
            format!("{}/miniforge3/bin/python", home),
            format!("{}/mambaforge/bin/python", home),
        ];
        if let Some(pyenv_root) = std::env::var("PYENV_ROOT").ok() {
            candidates.push(format!("{}/shims/python3", pyenv_root));
        }
        for p in &candidates {
            if std::path::Path::new(p).exists() { return Some(p.clone()); }
        }
    }

    None
}

/// Log when student changes Python environment
#[tauri::command]
fn log_python_change(
    state: State<AppState>,
    app_handle: tauri::AppHandle,
    from_env: String,
    to_env: String,
) {
    let event = ActivityEvent::new(
        "python_env_changed",
        &format!("Python environment changed: {} → {}", from_env, to_env),
        None, None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);
}

// ===== Import File from outside =====

#[derive(serde::Serialize)]
struct ImportResult {
    dest_path: String,
    original_path: String,
    size_bytes: u64,
}

#[tauri::command]
fn ws_import_file(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    ws: State<WorkspaceState>,
    kw: State<KnownWrites>,
    source_path: String,
    dest_dir: String,
) -> Result<ImportResult, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized".to_string())?;

    let src = std::path::Path::new(&source_path);
    if !src.exists() {
        return Err(format!("Source file not found: {}", source_path));
    }
    let filename = src.file_name()
        .ok_or("Invalid source path")?
        .to_string_lossy().to_string();
    let size_bytes = src.metadata().map(|m| m.len()).unwrap_or(0);

    let rel_dest = if dest_dir.is_empty() {
        filename.clone()
    } else {
        format!("{}/{}", dest_dir, filename)
    };

    // Read source and write into workspace (goes through resolve_safe)
    let content = std::fs::read(src)
        .map_err(|e| format!("Failed to read source: {}", e))?;
    mark_known_write(&kw, &rel_dest);
    let full_dest = workspace.resolve_safe_for_write(&rel_dest)?;
    if let Some(parent) = full_dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&full_dest, &content)
        .map_err(|e| format!("Failed to write imported file: {}", e))?;

    // Log the import
    let detail = format!(
        "Imported external file: {} ({} bytes) from {}",
        rel_dest, size_bytes, source_path
    );
    let event = ActivityEvent::new("file_import", &detail, Some(size_bytes as u32), None);
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

    Ok(ImportResult {
        dest_path: rel_dest,
        original_path: source_path,
        size_bytes,
    })
}

// ===== Workspace Commands =====

#[tauri::command]
fn init_workspace(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    ws: State<WorkspaceState>,
    kw: State<KnownWrites>,
    session_name: String,
) -> Result<String, String> {
    let base = setup::workspaces_dir();

    let workspace = Workspace::init(&base, &session_name)?;
    let root = workspace.root_path();

    // Start integrity monitor
    let log_handle = state.activity_log.lock().unwrap().get_handle();
    monitor::start_integrity_monitor(
        root.clone(),
        log_handle,
        app_handle,
        (*kw).clone(),
    );

    *ws.lock().map_err(|e| e.to_string())? = Some(workspace);
    Ok(root)
}

#[tauri::command]
fn ws_list_tree(ws: State<WorkspaceState>) -> Result<Vec<FileNode>, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.list_tree()
}

#[tauri::command]
fn ws_read_file(ws: State<WorkspaceState>, path: String) -> Result<String, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.read_file(&path)
}

#[tauri::command]
fn ws_xlsx_to_csv(ws: State<WorkspaceState>, path: String) -> Result<String, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized")?;
    let full_path = workspace.resolve_safe_for_write(&path)?;
    let full_str = full_path.to_string_lossy().to_string();

    // Use Python pandas to convert xlsx to CSV string
    let py = find_system_python().ok_or("Python not found")?;
    let script = format!(
        "import pandas as pd; df = pd.read_excel(r'{}'); print(df.to_csv(index=False))",
        full_str
    );
    let output = silent_cmd(&py, &["-c", &script]);
    match output {
        Some(o) if o.status.success() => {
            Ok(String::from_utf8_lossy(&o.stdout).to_string())
        }
        _ => Err("Failed to read Excel file".to_string()),
    }
}

#[tauri::command]
fn ws_read_file_base64(ws: State<WorkspaceState>, path: String) -> Result<String, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized".to_string())?;
    let full_path = workspace.resolve_safe_for_write(&path)?;
    let data = std::fs::read(&full_path).map_err(|e| e.to_string())?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&data))
}

#[tauri::command]
fn ws_write_file(ws: State<WorkspaceState>, kw: State<KnownWrites>, path: String, content: String) -> Result<(), String> {
    mark_known_write(&kw, &path);
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.write_file(&path, &content)
}

#[tauri::command]
fn ws_create_dir(ws: State<WorkspaceState>, kw: State<KnownWrites>, path: String) -> Result<(), String> {
    mark_known_write(&kw, &path);
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.create_dir(&path)
}

#[tauri::command]
fn ws_rename(ws: State<WorkspaceState>, kw: State<KnownWrites>, old_path: String, new_path: String) -> Result<(), String> {
    mark_known_write(&kw, &old_path);
    mark_known_write(&kw, &new_path);
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.rename(&old_path, &new_path)
}

#[tauri::command]
fn ws_delete(ws: State<WorkspaceState>, kw: State<KnownWrites>, path: String) -> Result<(), String> {
    mark_known_write(&kw, &path);
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.delete(&path)
}

#[tauri::command]
fn ws_root_path(ws: State<WorkspaceState>) -> Result<String, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())
        .map(|w| w.root_path())
}

// ===== Move (drag-and-drop) =====

#[tauri::command]
fn ws_move(ws: State<WorkspaceState>, src_path: String, dest_dir: String) -> Result<String, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized".to_string())?;

    let filename = src_path.rsplit('/').next().unwrap_or(&src_path);
    let new_path = if dest_dir.is_empty() {
        filename.to_string()
    } else {
        format!("{}/{}", dest_dir, filename)
    };

    if src_path == new_path {
        return Ok(new_path);
    }

    workspace.rename(&src_path, &new_path)?;
    Ok(new_path)
}

// ===== Submit Exam =====

#[derive(serde::Serialize)]
struct SubmitResult {
    folder_path: String,
    code_zip: String,
    video_zip: String,
}

/// Hash the student ID with SHA-256 to produce the zip encryption password.
/// The grading tool uses the same hash to decrypt.
fn hash_student_id(student_id: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    // Salt with a fixed prefix so raw student ID alone can't open it
    hasher.update(b"MINT_EXAM_2026_");
    hasher.update(student_id.as_bytes());
    hex::encode(hasher.finalize())
}

/// SHA-256 of the currently running executable. Used for tamper detection.
fn compute_self_hash() -> Option<String> {
    use sha2::{Sha256, Digest};
    use std::io::Read;

    let exe_path = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&exe_path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(hex::encode(hasher.finalize()))
}

#[tauri::command]
fn get_build_info() -> serde_json::Value {
    serde_json::json!({
        "commit_sha": env!("MINT_GIT_SHA"),
        "build_time": env!("MINT_BUILD_TIME"),
        "exe_hash": compute_self_hash().unwrap_or_else(|| "unavailable".to_string()),
    })
}

#[tauri::command]
fn submit_exam(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    recorder: State<RecorderState>,
    ws: State<WorkspaceState>,
    student_id: String,
) -> Result<SubmitResult, String> {
    // 1. Stop recording
    {
        let mut rec = recorder.lock().map_err(|e| e.to_string())?;
        if rec.is_recording() {
            let _ = rec.stop();
        }
    }

    // 2. Workspace root
    let ws_root = {
        let guard = ws.lock().map_err(|e| e.to_string())?;
        guard.as_ref().ok_or("No workspace initialized".to_string())?.root_path()
    };

    // 3. Save activity logs as separate files
    {
        let events = state.activity_log.lock().unwrap().get_events();

        let focus_log: Vec<_> = events.iter()
            .filter(|e| matches!(e.event_type.as_str(),
                "focus_lost" | "focus_returned" | "session_start" | "exam_submitted"))
            .cloned().collect();

        let background_log: Vec<_> = events.iter()
            .filter(|e| matches!(e.event_type.as_str(),
                "clipboard_internal" | "clipboard_external" | "recording_start" | "recording_stop" | "file_import" |
                "tamper_detected" | "tamper_new_file" | "tamper_deleted"))
            .cloned().collect();

        let editor_log: Vec<_> = events.iter()
            .filter(|e| matches!(e.event_type.as_str(),
                "paste" | "paste_large" | "input_burst" | "typing_summary" |
                "code_run" | "code_run_result" | "copy" | "cut" |
                "terminal_stdout" | "terminal_stderr"))
            .cloned().collect();

        let ws_path = std::path::PathBuf::from(&ws_root);
        let _ = std::fs::write(ws_path.join("_log_app_focus.json"),
            serde_json::to_string_pretty(&focus_log).unwrap_or_default());
        let _ = std::fs::write(ws_path.join("_log_background.json"),
            serde_json::to_string_pretty(&background_log).unwrap_or_default());
        let _ = std::fs::write(ws_path.join("_log_editor_activity.json"),
            serde_json::to_string_pretty(&editor_log).unwrap_or_default());
        let _ = std::fs::write(ws_path.join("_log_complete.json"),
            serde_json::to_string_pretty(&events).unwrap_or_default());
    }

    // 4. Create submission folder on Desktop
    let desktop = dirs::desktop_dir()
        .ok_or("Cannot find Desktop directory")?;
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let folder_name = format!("MINT_Exam_{}_{}", timestamp, student_id);
    let submit_dir = desktop.join(&folder_name);
    std::fs::create_dir_all(&submit_dir)
        .map_err(|e| format!("Failed to create submission folder: {}", e))?;

    // 5. Encryption password
    let password = hash_student_id(&student_id);
    let password_bytes = password.as_bytes().to_vec();

    // 6. Code + Logs → AES-256 zip (small files, fast)
    let code_zip_path = submit_dir.join("submission_code.zip");
    create_encrypted_zip(&ws_root, &code_zip_path, &password)?;

    // 7. Video → copy + obfuscate headers (instant, no re-encoding/zipping)
    //    Much faster than zipping 300MB+ video files
    let video_dir = submit_dir.join("video");
    let _ = std::fs::create_dir_all(&video_dir);
    let rec_dir = setup::recordings_dir();
    let mut video_count = 0u32;

    if rec_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&rec_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext == "mp4" || ext == "mov" {
                    let dest = video_dir.join(path.file_name().unwrap());
                    // Move (not copy) for speed — if same filesystem, instant
                    if std::fs::rename(&path, &dest).is_err() {
                        // Different filesystem: fall back to copy
                        let _ = std::fs::copy(&path, &dest);
                        let _ = std::fs::remove_file(&path);
                    }
                    // Obfuscate header so student can't play it
                    let _ = recorder::obfuscate_video(&dest, &password_bytes);
                    video_count += 1;
                }
            }
        }
    }

    // 8. Manifest (includes IDE integrity fingerprint)
    let ide_exe_hash = compute_self_hash().unwrap_or_else(|| "unavailable".to_string());
    let manifest = serde_json::json!({
        "student_id": student_id,
        "timestamp": timestamp,
        "hash_check": &password[..16],
        "video_count": video_count,
        "video_obfuscated": true,
        "ide_commit_sha": env!("MINT_GIT_SHA"),
        "ide_build_time": env!("MINT_BUILD_TIME"),
        "ide_exe_hash": ide_exe_hash,
    });
    let _ = std::fs::write(
        submit_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    );

    let folder_str = submit_dir.to_string_lossy().to_string();
    let code_str = code_zip_path.to_string_lossy().to_string();
    let video_str = video_dir.to_string_lossy().to_string();

    let event = ActivityEvent::new(
        "exam_submitted",
        &format!("Submitted by {}: {}", student_id, folder_str),
        None, None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

    Ok(SubmitResult { folder_path: folder_str, code_zip: code_str, video_zip: video_str })
}

fn create_encrypted_zip(workspace_root: &str, zip_path: &std::path::Path, password: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    use zip::write::SimpleFileOptions;
    use zip::AesMode;

    let file = std::fs::File::create(zip_path)
        .map_err(|e| format!("Failed to create zip: {}", e))?;
    let mut zip = zip::ZipWriter::new(file);

    let root = std::path::Path::new(workspace_root);
    add_dir_to_zip_encrypted(&mut zip, root, root, password)?;

    zip.finish().map_err(|e| format!("Failed to finish zip: {}", e))?;
    Ok(())
}

fn add_dir_to_zip_encrypted<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &std::path::Path,
    root: &std::path::Path,
    password: &str,
) -> Result<(), String> {
    use std::io::{Read, Write};
    use zip::write::SimpleFileOptions;
    use zip::AesMode;

    if !dir.is_dir() { return Ok(()); }

    const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "mkv", "webm"];

    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let relative = path.strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if path.is_dir() {
            let dir_options = SimpleFileOptions::default()
                .with_aes_encryption(AesMode::Aes256, password);
            zip.add_directory(&format!("{}/", relative), dir_options).map_err(|e| e.to_string())?;
            add_dir_to_zip_encrypted(zip, &path, root, password)?;
        } else {
            let ext = path.extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            if VIDEO_EXTS.contains(&ext.as_str()) {
                continue;
            }
            let file_options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .with_aes_encryption(AesMode::Aes256, password);
            zip.start_file(&relative, file_options).map_err(|e| e.to_string())?;
            let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            zip.write_all(&buf).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn create_encrypted_video_zip(rec_dir: &std::path::Path, zip_path: &std::path::Path, password: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    use zip::write::SimpleFileOptions;
    use zip::AesMode;

    let file = std::fs::File::create(zip_path)
        .map_err(|e| format!("Failed to create video zip: {}", e))?;
    let mut zip = zip::ZipWriter::new(file);

    if rec_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(rec_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "mp4" || e == "mov").unwrap_or(false) {
                    let options = SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored)
                        .with_aes_encryption(AesMode::Aes256, password);
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    zip.start_file(&name, options).map_err(|e| e.to_string())?;
                    let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
                    zip.write_all(&buf).map_err(|e| e.to_string())?;
                }
            }
        }
    }

    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

fn add_dir_to_zip<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &std::path::Path,
    root: &std::path::Path,
    options: &zip::write::SimpleFileOptions,
) -> Result<(), String> {
    use std::io::{Read, Write};

    if !dir.is_dir() { return Ok(()); }

    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let relative = path.strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if path.is_dir() {
            zip.add_directory(&format!("{}/", relative), *options).map_err(|e| e.to_string())?;
            add_dir_to_zip(zip, &path, root, options)?;
        } else {
            zip.start_file(&relative, *options).map_err(|e| e.to_string())?;
            let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            zip.write_all(&buf).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}



// ===== App Entry =====

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let activity_log = ActivityLog::new();
    let log_handle = activity_log.get_handle();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .manage(AppState {
            activity_log: Mutex::new(activity_log),
        })
        .manage(Mutex::new(ScreenRecorder::new()) as RecorderState)
        .manage(Mutex::new(None::<Workspace>) as WorkspaceState)
        .manage(new_known_writes())
        .manage(runner::new_running_process())
        .invoke_handler(tauri::generate_handler![
            get_activity_log,
            clear_activity_log,
            export_activity_log,
            log_editor_event,
            run_code,
            run_code_sync,
            stop_code,
            pip_install_packages,
            start_recording,
            stop_recording,
            is_recording,
            get_home_dir,
            get_recordings_dir,
            get_workspaces_dir,
            get_current_venv_path,
            get_default_venv_path,
            recreate_venv,
            get_build_info,
            read_setup_config,
            write_setup_config,
            package_list_for_profile,
            install_packages_smart,
            uninstall_packages,
            list_installed_packages,
            delete_sample_files,
            init_workspace,
            ws_list_tree,
            ws_read_file,
            ws_read_file_base64,
            ws_xlsx_to_csv,
            ws_write_file,
            ws_create_dir,
            ws_rename,
            ws_delete,
            ws_move,
            ws_root_path,
            ws_import_file,
            detect_pythons,
            setup_exam_python,
            log_python_change,
            save_code_history,
            submit_exam,
        ])
        .setup(move |app| {
            let app_handle = app.handle().clone();
            monitor::start_clipboard_monitor(log_handle.clone(), app_handle.clone());
            monitor::start_focus_monitor(log_handle.clone(), app_handle);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running MINT Exam IDE");
}
