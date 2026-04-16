mod monitor;
mod recorder;
mod runner;
mod workspace;

use monitor::{ActivityEvent, ActivityLog, KnownWrites, new_known_writes, mark_known_write};
use recorder::{RecorderState, ScreenRecorder};
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
    process: State<runner::RunningProcess>,
    language: String,
    code: String,
    filename: String,
    python_path: Option<String>,
) -> Result<(), String> {
    // Save file to workspace so imports work
    if let Ok(guard) = ws.lock() {
        if let Some(ref workspace) = *guard {
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
    output_dir: String,
) -> Result<String, String> {
    let mut rec = recorder.lock().map_err(|e| e.to_string())?;
    let path = rec.start(&output_dir)?;

    let event = ActivityEvent::new("recording_start", &format!("Screen recording started: {}", path), None, None);
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

fn probe_python(cmd: &str) -> Option<PythonInfo> {
    let output = std::process::Command::new(cmd)
        .args(["--version"])
        .output()
        .ok()?;
    let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let ver = if ver.is_empty() { String::from_utf8_lossy(&output.stderr).trim().to_string() } else { ver };
    if ver.is_empty() { return None; }

    // Get the real path
    let path_output = std::process::Command::new(cmd)
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .ok()?;
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
    let base = dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let base = base.join("MINT_Exam_Workspaces");

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
                "code_run" | "code_run_result"))
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

    // 5. Encryption password = SHA-256 hash of student ID
    let password = hash_student_id(&student_id);

    // 6. ZIP 1: Code + Log (AES-256 encrypted)
    let code_zip_path = submit_dir.join("submission_code.zip");
    create_encrypted_zip(&ws_root, &code_zip_path, &password)?;

    // 7. ZIP 2: Video recordings (AES-256 encrypted)
    let video_zip_path = submit_dir.join("submission_video.zip");
    let rec_dir = dirs::home_dir().unwrap_or_default().join("MINT_Exam_Recordings");
    create_encrypted_video_zip(&rec_dir, &video_zip_path, &password)?;

    // 8. Write a manifest with student info (unencrypted, for identification)
    let manifest = serde_json::json!({
        "student_id": student_id,
        "timestamp": timestamp,
        "hash_check": &password[..16], // first 16 chars for verification, not the full key
    });
    let _ = std::fs::write(
        submit_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    );

    let folder_str = submit_dir.to_string_lossy().to_string();
    let code_str = code_zip_path.to_string_lossy().to_string();
    let video_str = video_zip_path.to_string_lossy().to_string();

    // 9. Log
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
            stop_code,
            pip_install_packages,
            start_recording,
            stop_recording,
            is_recording,
            get_home_dir,
            init_workspace,
            ws_list_tree,
            ws_read_file,
            ws_write_file,
            ws_create_dir,
            ws_rename,
            ws_delete,
            ws_move,
            ws_root_path,
            ws_import_file,
            detect_pythons,
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
