mod monitor;
mod recorder;
mod runner;
mod setup;
mod toolchain;
mod workspace;

use monitor::{ActivityEvent, ActivityLog, KnownWrites, new_known_writes, mark_known_write, mark_known_write_hash, content_fingerprint};
use recorder::{RecorderState, ScreenRecorder};
use setup::SetupConfig;
use workspace::{FileNode, Workspace, WorkspaceState};
use std::sync::Mutex;
use tauri::{Emitter, Manager, State};

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
    stdin_state: State<runner::RunningStdin>,
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
            // Pin the exact bytes we're about to write so an external overwrite
            // of this file within the grace window is still flagged as tamper.
            mark_known_write_hash(&kw, &filename, &content_fingerprint(code.as_bytes()));
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
        runner::events_from_handle(app_handle),
        (*process).clone(),
        (*stdin_state).clone(),
        (*kw).clone(),
    );

    Ok(())
}

/// PID of the currently-running notebook cell child (0 = none). Published by
/// run_code_sync so the focus monitor can exempt the student's own program
/// window (matplotlib/tkinter) — which lives in this python process — from
/// focus_lost, just like the streaming runner does via RunningProcess.
pub static NOTEBOOK_CHILD_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Blocking code execution — for notebooks. Returns stdout+stderr directly.
///
/// The blocking work is pushed onto the BLOCKING thread pool, not run inline on
/// the async runtime. A notebook cell can run for the entire exam (`while True`
/// until the student hits Stop); occupying an async worker for that long
/// starves every other `#[tauri::command(async)]` — and on a 1-core machine
/// (num_cpus = 1 worker) it would make **submit_exam unable to run at all**
/// while a runaway cell is active.
#[tauri::command]
async fn run_code_sync(
    ws: State<'_, WorkspaceState>,
    known_writes: State<'_, KnownWrites>,
    language: String,
    code: String,
    filename: String,
    python_path: Option<String>,
) -> Result<(String, String, Option<i32>), String> {
    let _ = language; // currently unused — notebook supplies python only

    let cwd = ws.lock().ok().and_then(|g| g.as_ref().map(|w| w.root_path()));
    let kw = (*known_writes).clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_notebook_blocking(cwd, kw, code, filename, python_path)
    })
    .await
    .map_err(|e| format!("notebook run task failed: {}", e))?
}

fn run_notebook_blocking(
    cwd: Option<String>,
    known_writes: KnownWrites,
    code: String,
    filename: String,
    python_path: Option<String>,
) -> Result<(String, String, Option<i32>), String> {
    let work_dir = cwd.clone().unwrap_or_else(|| std::env::temp_dir().to_string_lossy().to_string());
    let hidden_name = format!(".{}", filename);
    let file_path = std::path::PathBuf::from(&work_dir).join(&hidden_name);
    std::fs::write(&file_path, &code).map_err(|e| e.to_string())?;

    // Snapshot the workspace BEFORE running so we can register any new files
    // the student's notebook cell creates (plt.savefig, df.to_csv, …) as
    // known writes — otherwise the integrity monitor flags them as TAMPER.
    let work_path = std::path::PathBuf::from(&work_dir);
    let pre_snapshot = runner::snapshot_workspace_files(&work_path);

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
        command.env("MPLBACKEND", "TkAgg");
    }

    // Own process group on Unix so stop_notebook can SIGKILL the whole tree
    // (a cell using multiprocessing/subprocess otherwise leaves grandchildren
    // running after Stop). Mirrors the streaming runner.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    // Spawn (instead of .output()) so we can publish the child PID for the
    // focus monitor's window-exemption.
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    // Notebook cells have no input box (the .py/.c/.cpp runner does), so give
    // them an empty stdin rather than the inherited one. A GUI process has no
    // console, so the inherited handle is invalid: `input()` in a cell failed
    // with an OS-level error instead of the plain EOFError a student can read.
    command.stdin(std::process::Stdio::null());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    NOTEBOOK_CHILD_PID.store(child.id(), std::sync::atomic::Ordering::SeqCst);

    // Chunked, CAPPED collection + wait on the CHILD, not on pipe EOF.
    // `wait_with_output()` had the same two flaws the streaming runner already
    // fixed: (1) a grandchild inheriting the pipe (multiprocessing/subprocess)
    // kept EOF from ever arriving → the notebook stayed "Running..." forever
    // for a cell that had finished; (2) it buffered UNBOUNDED output — one
    // `print` of a giant list OOM'd low-RAM laptops.
    const NB_STDOUT_CAP: usize = 8 * 1024 * 1024;
    const NB_STDERR_CAP: usize = 2 * 1024 * 1024;
    fn spawn_capped_reader<R: std::io::Read + Send + 'static>(
        pipe: Option<R>,
        cap: usize,
    ) -> (std::sync::Arc<Mutex<String>>, Option<std::thread::JoinHandle<()>>) {
        let buf = std::sync::Arc::new(Mutex::new(String::new()));
        let Some(mut pipe) = pipe else { return (buf, None); };
        let b2 = buf.clone();
        let handle = std::thread::spawn(move || {
            let mut chunk = [0u8; 65536];
            let mut pending: Vec<u8> = Vec::new();
            loop {
                let n = match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                pending.extend_from_slice(&chunk[..n]);
                runner::push_capped(&b2, cap, &runner::drain_utf8_lossy(&mut pending));
            }
            if !pending.is_empty() {
                runner::push_capped(&b2, cap, &String::from_utf8_lossy(&pending));
            }
        });
        (buf, Some(handle))
    }
    let (out_buf, t_out) = spawn_capped_reader(child.stdout.take(), NB_STDOUT_CAP);
    let (err_buf, t_err) = spawn_capped_reader(child.stderr.take(), NB_STDERR_CAP);

    let wait_result = child.wait();
    NOTEBOOK_CHILD_PID.store(0, std::sync::atomic::Ordering::SeqCst);
    let status = wait_result.map_err(|e| e.to_string())?;
    // Drain the readers briefly, then detach (grandchild-held pipe case).
    if let Some(t) = t_out {
        runner::join_bounded(t, std::time::Duration::from_secs(3));
    }
    if let Some(t) = t_err {
        runner::join_bounded(t, std::time::Duration::from_secs(3));
    }
    let stdout_text = out_buf.lock().map(|b| b.clone()).unwrap_or_default();
    let stderr_text = err_buf.lock().map(|b| b.clone()).unwrap_or_default();

    // Cleanup temp file
    let _ = std::fs::remove_file(&file_path);

    // Register ALL files the cell touched as known writes — both newly-
    // created (set difference) AND modified-in-place files (e.g. the student
    // running `with open('data.csv','a') as f: f.write(...)`). The earlier
    // difference()-only approach left in-place modifications unprotected, so
    // the integrity monitor flagged every notebook append as TAMPER.
    // Pin each touched file to its post-run content hash (content-aware grace),
    // and give a time-only deletion grace to files the cell removed — same
    // policy as the streaming runner. (Marking the pre/post UNION time-only
    // previously laundered the whole workspace for 8s after every cell run.)
    // Pin each touched file to its post-run content hash; deleted files get a
    // time-only deletion grace. (Generated-output file types are excluded from
    // monitoring entirely in integrity.rs, so a notebook cell's plot/data
    // outputs are never flagged; source files stay monitored.)
    let post_paths = runner::snapshot_workspace_paths(&work_path);
    for (rel, path) in &post_paths {
        match monitor::hash_file_retry(path) {
            Some(h) => mark_known_write_hash(&known_writes, rel, &h),
            None => mark_known_write(&known_writes, rel),
        }
    }
    let post_rels: std::collections::HashSet<String> = post_paths.into_keys().collect();
    for rel in pre_snapshot.difference(&post_rels) {
        mark_known_write(&known_writes, rel);
    }

    Ok((stdout_text, stderr_text, status.code()))
}

/// Kill the currently-running notebook cell child (published in
/// NOTEBOOK_CHILD_PID by run_code_sync). Notebook runs are blocking and were
/// otherwise unstoppable — a `while True:` cell would wedge the notebook for
/// the whole exam with app-restart the only recovery (which strands the
/// student's files in the old session workspace). Returns true if a child was
/// targeted.
#[tauri::command]
fn stop_notebook() -> bool {
    use std::sync::atomic::Ordering;
    let pid = NOTEBOOK_CHILD_PID.load(Ordering::SeqCst);
    if pid == 0 {
        return false;
    }
    // taskkill /T takes the child's tree on Windows; on Unix the child is a
    // process-group leader (process_group(0) at spawn), so `-<pid>` kills the
    // whole group including multiprocessing/subprocess grandchildren.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{}", pid)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
    }
    true
}

#[tauri::command]
fn stop_code(process: State<runner::RunningProcess>, stdin_state: State<runner::RunningStdin>) -> bool {
    // Close stdin first. A program parked in `cin >>` / `input()` is woken by
    // the EOF and can exit cleanly; without it the kill below is the only way
    // out and a graceful shutdown path is lost.
    runner::close_stdin(&stdin_state);
    // Record the stop against the current run generation BEFORE touching the
    // slot. A compiled language has a brief window where no child is in the
    // slot (the compiler has exited, the program is not yet spawned); without
    // this the Stop would be swallowed and the program would start anyway.
    runner::note_stop_request();
    // Take the child OUT of the shared handle immediately, under the lock.
    // The slot is now free for a fresh Run — even if it arrives during the
    // background kill that follows. Without this synchronous take, the kill
    // thread races with the next run and may SIGKILL the new child.
    let child = match process.lock().ok().and_then(|mut g| g.take().map(|(_, c)| c)) {
        Some(c) => c,
        // Nothing to kill, but the stop was still recorded above — a run that is
        // mid-compile will honour it.
        None => return false,
    };
    // taskkill /F /T can take 100ms+. Do it off the IPC thread.
    std::thread::spawn(move || runner::stop_taken_child(child));
    true
}

/// Send a chunk to the running program's standard input.
///
/// The frontend appends the newline for a normal "Enter"; a paste of several
/// lines arrives as one call so ordering is preserved even if the student
/// pastes faster than the program consumes.
#[tauri::command]
fn send_stdin(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    stdin_state: State<runner::RunningStdin>,
    text: String,
) -> Result<bool, String> {
    let delivered = runner::write_stdin(&stdin_state, &text)?;
    if delivered {
        // Input a student typed is part of the exam record: an answer that
        // hardcodes the expected input reads very differently from one that
        // parses it, and the grader can only see that if the input is logged.
        let preview: String = text.chars().take(200).collect();
        let event = ActivityEvent::new(
            "stdin_input",
            &format!("stdin: {}", preview.trim_end()),
            Some(text.len() as u32),
            None,
        );
        state.activity_log.lock().unwrap().add_event(event.clone());
        let _ = app_handle.emit("activity-event", &event);
    }
    Ok(delivered)
}

/// Close standard input, signalling EOF.
///
/// `while (std::cin >> x)` and `while (getline(std::cin, line))` end only at
/// EOF, so this is what lets a student finish an input-loop program at all.
#[tauri::command]
fn close_stdin(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    stdin_state: State<runner::RunningStdin>,
) -> bool {
    let closed = runner::close_stdin(&stdin_state);
    if closed {
        let event = ActivityEvent::new("stdin_eof", "stdin closed (EOF)", None, None);
        state.activity_log.lock().unwrap().add_event(event.clone());
        let _ = app_handle.emit("activity-event", &event);
    }
    closed
}

/// Every C++ compiler on this machine, for the settings selector.
#[tauri::command(async)]
fn detect_compilers(cpp: Option<bool>) -> Vec<toolchain::CompilerInfo> {
    toolchain::detect_compilers(cpp.unwrap_or(true))
}

/// Compile, link and run a probe that also reads stdin. Mirrors
/// `verify_exam_environment` for Python: the point is to fail in the wizard,
/// where it can be fixed, rather than on the first Run of an exam.
#[tauri::command(async)]
fn verify_cpp_environment(compiler_path: Option<String>, standard: Option<String>) -> toolchain::CppVerifyResult {
    // A settings change may have installed or switched toolchains; discovery is
    // cached for the process lifetime, so drop it before verifying.
    toolchain::clear_compiler_cache();
    toolchain::verify_cpp(
        compiler_path.as_deref(),
        standard.as_deref().unwrap_or(toolchain::DEFAULT_CPP_STANDARD),
    )
}

/// The compiler a Run would actually use right now, for the status bar.
#[tauri::command(async)]
fn current_compiler() -> Option<toolchain::CompilerInfo> {
    let cfg = setup::load_config();
    let path = toolchain::find_compiler(cfg.cpp_compiler_path.as_deref(), true)?;
    let version = toolchain::probe_version(&path).unwrap_or_default();
    Some(toolchain::CompilerInfo {
        kind: if version.to_ascii_lowercase().contains("clang") { "clang".into() } else { "gcc".into() },
        is_mint: false,
        path,
        version,
    })
}

// ===== Screen Recording =====

// Monotonic recording generation. Bumped on every start AND stop so the
// health watchdog thread for a given recording exits as soon as that recording
// stops or a new one starts (it never holds the recorder lock).
static RECORDING_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// `async`-flagged so Tauri runs it OFF the main (UI) thread: start-up does
// multi-second blocking work (device enumeration + per-strategy 1.5s spawn
// probes + the fresh-process TCC preflight on macOS) that would otherwise
// beach-ball the whole window, and the frontend re-invokes it every 15s while
// permission is pending.
#[tauri::command(async)]
fn start_recording(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    recorder: State<RecorderState>,
    output_dir: Option<String>,
) -> Result<String, String> {
    let mut rec = recorder.lock().map_err(|e| e.to_string())?;
    let dir = output_dir.unwrap_or_else(|| setup::recordings_dir().to_string_lossy().to_string());
    // Ensure the recordings dir exists and is HIDDEN. It lives on the LOCAL
    // (non-OneDrive) %LOCALAPPDATA% drive so ffmpeg's live write can't stall on
    // OneDrive sync/locking, and a casual student doesn't stumble onto it.
    {
        let p = std::path::PathBuf::from(&dir);
        let _ = std::fs::create_dir_all(&p);
        #[cfg(target_os = "windows")]
        crate::workspace::hide_directory(&p);
    }
    let path = rec.start(&dir)?;
    let recording_epoch =
        RECORDING_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let strategy = rec.last_strategy().unwrap_or_else(|| "unknown".to_string());

    let event = ActivityEvent::new(
        "recording_start",
        &format!("Screen recording started [{}]: {}", strategy, path),
        None, None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

    // Health check — confirms the capture process actually wrote frames.
    // Covers two silent-failure modes:
    //   1. macOS: Screen Recording permission denied → dead process / 0-byte file.
    //   2. Windows: gdigrab session disconnected (RDP)、antivirus quarantine.
    // After 3 seconds a healthy ffmpeg capture is ≥ tens of KB; a sub-1KB
    // file means nothing reached disk. The student gets a clear alert
    // instead of finding out post-exam that the video is unplayable.
    //
    // STRATEGY-AWARE: the macOS `screencapture` fallback may produce its .mov
    // only at STOP time, so for that strategy file size proves nothing — the
    // probe and watchdog key off process LIVENESS instead. ffmpeg strategies
    // (all platforms) write progressively, so both size and liveness apply.
    {
        let path_for_check = std::path::PathBuf::from(&path);
        let ah = app_handle.clone();
        let my_epoch = recording_epoch;
        let size_based = !strategy.contains("screencapture");
        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            // Liveness snapshot via the managed recorder state. Reaps a dead
            // child (setting last_error) under a briefly-held lock.
            let probe_recorder = |ah: &tauri::AppHandle| -> (bool, Option<String>) {
                let st = ah.state::<RecorderState>();
                let result = match st.lock() {
                    Ok(mut r) => {
                        let alive = r.is_recording();
                        (alive, r.last_error())
                    }
                    Err(_) => (true, None), // poisoned — don't false-alarm
                };
                result
            };

            // 1. Fast-failure probe. A LIVE encoder with a tiny file at 3s is
            //    NORMAL — at 2fps, x264's frame delay plus the mp4 muxer's
            //    write buffering keeps the on-disk file under 1KB for the
            //    first several seconds (field-confirmed: a healthy recording
            //    averaged ~1.3KB/s, so the 3s check false-alarmed on real
            //    exams). Alarm at 3s only if the capture process is DEAD;
            //    a small-but-alive capture gets re-checked at ~12s and alarms
            //    only if the file is STILL under 1KB.
            std::thread::sleep(std::time::Duration::from_secs(3));
            if RECORDING_EPOCH.load(Ordering::SeqCst) != my_epoch {
                return; // already stopped / superseded
            }
            let mut bytes = std::fs::metadata(&path_for_check)
                .map(|m| m.len()).unwrap_or(0);
            let (alive, mut last_err) = probe_recorder(&ah);
            // Re-check the epoch: stop_recording may hold the recorder lock
            // through its (bounded) graceful stop, so the probe could have
            // blocked above while this recording was being stopped.
            if RECORDING_EPOCH.load(Ordering::SeqCst) != my_epoch {
                return;
            }
            let mut start_failed = if size_based { !alive && bytes < 1024 } else { !alive };
            let mut probe_window = "3s";
            if !start_failed && size_based && bytes < 1024 {
                // Alive but nothing on disk yet — grace period, then re-verify.
                std::thread::sleep(std::time::Duration::from_secs(9));
                if RECORDING_EPOCH.load(Ordering::SeqCst) != my_epoch {
                    return;
                }
                bytes = std::fs::metadata(&path_for_check)
                    .map(|m| m.len()).unwrap_or(0);
                let (alive2, err2) = probe_recorder(&ah);
                if RECORDING_EPOCH.load(Ordering::SeqCst) != my_epoch {
                    return;
                }
                last_err = err2;
                // Dead OR still no real data after 12s — genuine start failure
                // (permission denied / capture device error).
                start_failed = !alive2 || bytes < 1024;
                probe_window = "12s";
            }
            if start_failed {
                let msg = if cfg!(target_os = "macos") {
                    format!(
                        "녹화가 시작되지 않았습니다 ({} bytes after {}{}). \
                         시스템 설정 > 개인정보 보호 및 보안 > 화면 기록에서 \
                         MINT Exam IDE 권한을 허용하고 IDE를 재시작하세요.",
                        bytes,
                        probe_window,
                        last_err.as_deref().map(|e| format!(", {}", e)).unwrap_or_default()
                    )
                } else {
                    format!(
                        "녹화가 시작되지 않았습니다 ({} bytes after {}). \
                         FFmpeg가 설치되어 있는지, RDP/원격 데스크탑 세션에 \
                         있지 않은지 확인하세요.",
                        bytes,
                        probe_window
                    )
                };
                let event = ActivityEvent::new(
                    "recording_health_fail",
                    &msg,
                    Some(bytes as u32),
                    None,
                );
                let _ = ah.emit("activity-event", &event);
            }

            // 2. Periodic watchdog: a capture that dies/freezes mid-exam (RDP
            //    disconnect, display sleep, GPU encoder hang) leaves ffmpeg
            //    resident while the file simply stops growing. The one-shot 3s
            //    probe already passed, so without this nothing re-checks.
            // Require SEVERAL consecutive no-growth samples before alarming so
            // normal ffmpeg write-buffering (size can stay flat for a sample or
            // two even while healthy) doesn't trip a false "stalled" alert. A
            // genuine freeze stays flat indefinitely and still gets caught.
            let mut last_size = bytes;
            let mut flat_samples: u32 = 0;
            let mut stalled_reported = false;
            const FLAT_SAMPLES_BEFORE_ALARM: u32 = 3; // 3 × 20s ≈ 60s of no growth
            loop {
                std::thread::sleep(std::time::Duration::from_secs(20));
                if RECORDING_EPOCH.load(Ordering::SeqCst) != my_epoch {
                    return; // recording stopped or a new one started
                }
                // Dead capture process is definitive on every strategy — alert
                // once and stop watching (is_recording() also emits nothing on
                // its own; a dead child otherwise goes unnoticed for the rest
                // of the exam while "● REC" keeps showing).
                let (alive, last_err) = probe_recorder(&ah);
                if RECORDING_EPOCH.load(Ordering::SeqCst) != my_epoch {
                    return;
                }
                if !alive {
                    let event = ActivityEvent::new(
                        "recording_health_fail",
                        &format!(
                            "녹화 프로세스가 예기치 않게 종료되었습니다{}. 녹화가 더 이상 진행되지 않습니다 — 감독관에게 알리세요.",
                            last_err.as_deref().map(|e| format!(" ({})", e)).unwrap_or_default()
                        ),
                        None,
                        None,
                    );
                    let _ = ah.emit("activity-event", &event);
                    return;
                }
                if !size_based {
                    continue; // screencapture: size proves nothing until stop
                }
                let now_size = std::fs::metadata(&path_for_check).map(|m| m.len()).unwrap_or(0);
                if now_size > last_size {
                    flat_samples = 0;
                    stalled_reported = false; // growing again — re-arm
                } else if now_size > 1024 {
                    flat_samples += 1;
                    if flat_samples >= FLAT_SAMPLES_BEFORE_ALARM && !stalled_reported {
                        let event = ActivityEvent::new(
                            "recording_health_fail",
                            "녹화 파일이 약 1분간 커지지 않았습니다. 화면 캡처가 중단되었을 수 있습니다(RDP 연결 해제 / 디스플레이 절전 / 인코더 오류). 녹화 상태를 확인하세요.",
                            Some(now_size as u32),
                            None,
                        );
                        let _ = ah.emit("activity-event", &event);
                        stalled_reported = true;
                    }
                }
                last_size = now_size;
            }
        });
    }

    Ok(path)
}

// `async`-flagged: graceful_stop_recorder blocks up to 15s waiting for ffmpeg
// to finalize the moov atom — must not run on the main/UI thread.
#[tauri::command(async)]
fn stop_recording(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    recorder: State<RecorderState>,
) -> Result<String, String> {
    let mut rec = recorder.lock().map_err(|e| e.to_string())?;
    let path = rec.stop()?;
    // Invalidate the health watchdog for the recording we just stopped.
    RECORDING_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let event = ActivityEvent::new("recording_stop", &format!("Screen recording saved: {}", path), None, None);
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

    Ok(path)
}

#[tauri::command]
fn is_recording(recorder: State<RecorderState>) -> bool {
    // `is_recording` now mutates (reaps dead ffmpeg children). Take a mut lock.
    recorder.lock().map(|mut r| r.is_recording()).unwrap_or(false)
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
    let mut results: Vec<PythonInfo> = Vec::new();
    // `python3` and `python` frequently resolve to the SAME interpreter (and
    // conda base can be rediscovered via the envs scan) — dedupe on the
    // resolved sys.executable path so the selector doesn't list twins.
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut push_unique = |results: &mut Vec<PythonInfo>, info: PythonInfo| {
        if seen_paths.insert(info.path.clone()) {
            results.push(info);
        }
    };

    // 1. System pythons
    for cmd in ["python3", "python"] {
        if let Some(info) = probe_python(cmd) {
            push_unique(&mut results, info);
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
                push_unique(&mut results, info);
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
                        push_unique(&mut results, info);
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
    // The Windows Store `python.exe` alias stub exits non-zero and prints
    // "Python was not found; run without arguments to install..." to stderr —
    // without these checks it showed up in the interpreter selector as a
    // garbage "System Python was not found..." entry.
    if !output.status.success() {
        return None;
    }
    let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let ver = if ver.is_empty() { String::from_utf8_lossy(&output.stderr).trim().to_string() } else { ver };
    if !ver.starts_with("Python ") { return None; }

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

/// Verify that the given python can import tkinter + matplotlib and produce
/// a figure with the TkAgg backend. Used after `install_packages_smart` to
/// surface broken environments BEFORE the student tries `plt.show()` mid-exam.
#[derive(Debug, Clone, Serialize)]
pub struct EnvVerifyResult {
    pub ok: bool,
    pub tkinter_ok: bool,
    pub matplotlib_ok: bool,
    pub backend: String,
    pub errors: Vec<String>,
}

#[tauri::command]
fn verify_exam_environment(python_path: String) -> EnvVerifyResult {
    // Single-line Python scripts (no triple-quotes — passed via -c).
    //
    // Windows probe: tkinter + matplotlib FORCED to TkAgg — that is exactly
    // what runner.rs forces at run time there, so the probe must match.
    const PROBE_TK: &str = "\
import json,sys\n\
r={'tkinter_ok':False,'matplotlib_ok':False,'backend':'','errors':[]}\n\
try:\n\
 import tkinter\n\
 t=tkinter.Tk();t.withdraw();t.destroy()\n\
 r['tkinter_ok']=True\n\
except Exception as e:\n\
 r['errors'].append('tkinter: '+type(e).__name__+': '+str(e))\n\
try:\n\
 import matplotlib\n\
 matplotlib.use('TkAgg',force=True)\n\
 from matplotlib import pyplot as plt\n\
 fig=plt.figure();plt.close(fig)\n\
 r['matplotlib_ok']=True\n\
 r['backend']=matplotlib.get_backend()\n\
except Exception as e:\n\
 r['errors'].append('matplotlib: '+type(e).__name__+': '+str(e))\n\
sys.stdout.write(json.dumps(r))\n";

    // macOS/Linux probe: let matplotlib auto-select its backend (macOS uses
    // the native MacOSX backend; runner.rs deliberately does NOT force TkAgg
    // there). Probing TkAgg used to FAIL on every Mac — brew's python@3.12
    // ships without tkinter — even though plt.show() works fine at exam time.
    // tkinter status is still reported for information but does not gate `ok`.
    const PROBE_DEFAULT: &str = "\
import json,sys\n\
r={'tkinter_ok':False,'matplotlib_ok':False,'backend':'','errors':[]}\n\
try:\n\
 import tkinter\n\
 r['tkinter_ok']=True\n\
except Exception as e:\n\
 r['errors'].append('tkinter (optional on this OS): '+type(e).__name__+': '+str(e))\n\
try:\n\
 import matplotlib\n\
 from matplotlib import pyplot as plt\n\
 fig=plt.figure();plt.close(fig)\n\
 r['matplotlib_ok']=True\n\
 r['backend']=matplotlib.get_backend()\n\
except Exception as e:\n\
 r['errors'].append('matplotlib: '+type(e).__name__+': '+str(e))\n\
sys.stdout.write(json.dumps(r))\n";

    let windows = cfg!(target_os = "windows");
    let probe = if windows { PROBE_TK } else { PROBE_DEFAULT };

    let mut command = std::process::Command::new(&python_path);
    command
        .args(["-c", probe])
        .env("PYTHONIOENCODING", "utf-8")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if windows {
        command.env("MPLBACKEND", "TkAgg");
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let fail = |msg: String| EnvVerifyResult {
        ok: false, tkinter_ok: false, matplotlib_ok: false,
        backend: String::new(), errors: vec![msg],
    };

    let output = match command.output() {
        Ok(o) => o,
        Err(e) => return fail(format!("Failed to launch python at {}: {}", python_path, e)),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let last = stdout.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("");

    let parsed: serde_json::Value = match serde_json::from_str(last) {
        Ok(v) => v,
        Err(_) => return fail(format!(
            "Verification probe did not emit JSON. stdout={:?} stderr={:?}",
            stdout.chars().take(200).collect::<String>(),
            stderr.chars().take(200).collect::<String>()
        )),
    };

    let tkinter_ok = parsed.get("tkinter_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let matplotlib_ok = parsed.get("matplotlib_ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let backend = parsed.get("backend").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let errors: Vec<String> = parsed.get("errors").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // Windows requires tkinter (TkAgg is forced at run time there); macOS/Linux
    // only need matplotlib itself to work with its auto-selected backend.
    let ok = if windows { tkinter_ok && matplotlib_ok } else { matplotlib_ok };
    EnvVerifyResult {
        ok,
        tkinter_ok, matplotlib_ok, backend, errors,
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

fn find_system_python() -> Option<String> {
    // Priority 1: MINT-dedicated Python (installed by install-windows.ps1 — ASCII path + tcl/tk verified).
    #[cfg(target_os = "windows")]
    {
        let mint_py = "C:\\ProgramData\\MINT_Python\\Python312\\python.exe";
        if std::path::Path::new(mint_py).exists() {
            return Some(mint_py.to_string());
        }
    }

    // Priority 1 (macOS/Linux): KNOWN LOCATIONS, pinned python3.12 first —
    // BEFORE probing bare `python3` on PATH. A GUI-launched app on macOS gets
    // the minimal launchd PATH (/usr/bin:...), where `python3` is the Xcode
    // CLT's Python 3.9 — a venv built from it can't install the 3.12-pinned
    // package set (numpy==2.1.3 / matplotlib==3.10.0 / scipy==1.14.1 all
    // require ≥3.10), so every profile install used to fail on Macs. Mirrors
    // runner::discover_python's ordering.
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut candidates: Vec<String> = vec![
            "/opt/homebrew/bin/python3.12".to_string(),
            "/opt/homebrew/opt/python@3.12/bin/python3.12".to_string(),
            "/usr/local/bin/python3.12".to_string(),
            "/usr/local/opt/python@3.12/bin/python3.12".to_string(),
            "/opt/homebrew/bin/python3".to_string(),
            "/usr/local/bin/python3".to_string(),
            format!("{}/anaconda3/bin/python", home),
            format!("{}/miniconda3/bin/python", home),
            format!("{}/miniforge3/bin/python", home),
            format!("{}/mambaforge/bin/python", home),
        ];
        if let Some(pyenv_root) = std::env::var("PYENV_ROOT").ok() {
            candidates.push(format!("{}/shims/python3", pyenv_root));
        }
        // /usr/bin/python3 (CLT 3.9) is the LAST resort, after the PATH probe
        // below has also failed — it exists on every Mac but is the wrong env.
        for p in &candidates {
            if std::path::Path::new(p).exists() { return Some(p.clone()); }
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
        // Absolute last resort: the always-present CLT python (3.9 on macOS).
        if std::path::Path::new("/usr/bin/python3").exists() {
            return Some("/usr/bin/python3".to_string());
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
    // Content-pin the imported bytes (not a blind time grace) so the file's
    // appearance isn't flagged, and only THIS content is excused at that path.
    // (content_fingerprint — a >32MB import must pin the same `meta:` scheme
    // the scanner computes, or the pin never matches.)
    mark_known_write_hash(&kw, &rel_dest, &content_fingerprint(&content));
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
    sb: State<monitor::SharedBaseline>,
    session_name: String,
) -> Result<String, String> {
    // Sweep stale build directories once per session, off the startup path: a
    // semester of exams would otherwise leave one build tree per workspace,
    // each holding statically-linked binaries, in a directory no student ever
    // sees. Threaded so a slow disk cannot delay the window appearing.
    std::thread::spawn(|| {
        toolchain::prune_old_build_dirs(toolchain::BUILD_DIR_MAX_AGE);
    });

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
        (*sb).clone(),
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

/// File size without reading content — lets the frontend refuse to mount a
/// giant file into CodeMirror / the CSV table parser (UI freeze on low-spec
/// machines) without first pulling the whole file over IPC to find out.
#[tauri::command]
fn ws_file_size(ws: State<WorkspaceState>, path: String) -> Result<u64, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized".to_string())?;
    let full = workspace.resolve_safe_for_write(&path)?;
    full.metadata().map(|m| m.len()).map_err(|e| e.to_string())
}

#[tauri::command]
fn ws_xlsx_to_csv(
    ws: State<WorkspaceState>,
    path: String,
    python_path: Option<String>,
) -> Result<String, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized")?;
    let full_path = workspace.resolve_safe_for_write(&path)?;
    let full_str = full_path.to_string_lossy().to_string();

    // Convert xlsx to CSV via pandas. Pass the path through sys.argv, NOT string
    // interpolation into an r'...' literal — a filename containing a single quote
    // (legal on macOS/Linux) would otherwise break out of the literal, so a legit
    // data file fails to open (and it would run injected code).
    //
    // Use the EXAM VENV python the frontend passes — pandas is installed there,
    // not in the base MINT/system python (find_system_python), so the old
    // base-python call made every .xlsx preview fail on a fresh install.
    let py = python_path
        .filter(|p| !p.is_empty())
        .or_else(find_system_python)
        .ok_or("Python not found")?;
    let script = "import sys, pandas as pd; print(pd.read_excel(sys.argv[1]).to_csv(index=False))";
    let output = silent_cmd(&py, &["-c", script, &full_str]);
    match output {
        Some(o) if o.status.success() => {
            Ok(String::from_utf8_lossy(&o.stdout).to_string())
        }
        _ => Err("Failed to read Excel file (pandas/openpyxl 필요)".to_string()),
    }
}

#[tauri::command]
fn ws_read_file_base64(ws: State<WorkspaceState>, path: String) -> Result<String, String> {
    // Hard ceiling: the result is base64 (≈1.33×) embedded in a JSON IPC
    // message and then in a data: URI. A 100MB image would materialize the
    // bytes, the base64 String, the JSON, and the webview copy all at once —
    // enough to OOM or freeze a low-RAM laptop.
    const MAX_BASE64_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized".to_string())?;
    let full_path = workspace.resolve_safe_for_write(&path)?;
    let len = full_path.metadata().map(|m| m.len()).unwrap_or(0);
    if len > MAX_BASE64_SOURCE_BYTES {
        return Err(format!(
            "파일이 너무 큽니다 ({:.1} MB) — 미리보기를 표시할 수 없습니다.",
            len as f64 / (1024.0 * 1024.0)
        ));
    }
    let data = std::fs::read(&full_path).map_err(|e| e.to_string())?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&data))
}

#[tauri::command]
fn ws_write_file(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    ws: State<WorkspaceState>,
    kw: State<KnownWrites>,
    path: String,
    content: String,
) -> Result<(), String> {
    // Content-pinned: an external overwrite of this path within the grace
    // window (different bytes) is still flagged as tampering.
    mark_known_write_hash(&kw, &path, &content_fingerprint(content.as_bytes()));
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.write_file(&path, &content)?;
    let event = ActivityEvent::new(
        "file_save",
        &format!("Saved {} ({} bytes)", path, content.len()),
        Some(content.len() as u32),
        None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);
    Ok(())
}

#[tauri::command]
fn ws_create_dir(ws: State<WorkspaceState>, kw: State<KnownWrites>, path: String) -> Result<(), String> {
    mark_known_write(&kw, &path);
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.create_dir(&path)
}

/// Register known-writes for an IDE-initiated rename/move of `src` (absolute
/// path, still at its OLD location) from workspace-relative `old_rel` to
/// `new_rel`. A directory is handled PER CHILD FILE (dirs themselves aren't
/// tracked by the integrity scanner): each old child path gets a time-only
/// deletion grace and each dest child is pinned via `pin_authorized_dest`.
/// Without this, renaming a folder raises a spurious tamper_new_file +
/// tamper_deleted for every file inside.
fn pin_rename_sources(
    kw: &KnownWrites,
    shared: &monitor::SharedBaseline,
    src: &std::path::Path,
    old_rel: &str,
    new_rel: &str,
) {
    if src.is_dir() {
        let old_base = old_rel.trim_end_matches('/');
        let new_base = new_rel.trim_end_matches('/');
        for (child_rel, child_path) in runner::snapshot_workspace_paths(src) {
            let old_child = format!("{}/{}", old_base, child_rel);
            let new_child = format!("{}/{}", new_base, child_rel);
            mark_known_write(kw, &old_child); // source child disappears — deletion grace
            pin_authorized_dest(kw, shared, &child_path, &old_child, &new_child);
        }
    } else {
        // Single file: the caller already set the old-path deletion grace.
        pin_authorized_dest(kw, shared, src, old_rel, new_rel);
    }
}

/// Content-pin `new_rel` as a known write ONLY when the source file's current
/// bytes equal the integrity baseline's last-known-good hash for `old_rel`.
/// Honest renames (content unchanged since the last poll) stay event-free; a
/// file an external process modified or created since the last poll does NOT
/// match the baseline, so its dest is left unpinned and the monitor flags it at
/// the new path.
///
/// Authorization is deliberately gated on the SIGNED BASELINE ALONE — NOT on the
/// live known_writes pins. Those pins are set for every file a run touches
/// (runner.rs pins the whole post-run snapshot), so trusting them here would let
/// a student externally edit a file, trigger any run to pin the tampered bytes,
/// then rename it to launder the edit with no tamper event. The baseline can
/// only hold content a poll already accepted (and any divergence from baseline
/// was itself flagged as tamper_detected at that poll), so it is the one source
/// of truth an attacker cannot poison inside the sub-poll window.
///
/// TRADE-OFF: a file SAVED through the IDE and renamed within the same ~2s poll
/// gap (before the baseline absorbs the new bytes) is momentarily not baseline-
/// known, so it raises a one-off tamper_new_file at the destination. This fails
/// SAFE (over-reports an honest action; a grader sees the paired file_rename)
/// and is far preferable to the laundering the known_writes branch would open.
fn pin_authorized_dest(
    kw: &KnownWrites,
    shared: &monitor::SharedBaseline,
    src_child: &std::path::Path,
    old_rel: &str,
    new_rel: &str,
) {
    let Some(live) = monitor::hash_file_retry(src_child) else { return; };
    if monitor::shared_baseline_hash(shared, old_rel).as_deref() == Some(live.as_str()) {
        mark_known_write_hash(kw, new_rel, &live);
    }
}

#[tauri::command]
fn ws_rename(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    ws: State<WorkspaceState>,
    kw: State<KnownWrites>,
    sb: State<monitor::SharedBaseline>,
    old_path: String,
    new_path: String,
) -> Result<(), String> {
    mark_known_write(&kw, &old_path); // old path disappears — deletion grace
    let guard = ws.lock().map_err(|e| e.to_string())?;
    let workspace = guard.as_ref().ok_or("No workspace initialized".to_string())?;
    // Pin the destination from the SOURCE bytes BEFORE renaming, but ONLY if
    // those bytes match the signed baseline (see pin_authorized_dest) — so the
    // new path is content-known the instant it appears (no unpinned window for
    // the 2s poll) while an externally-tampered source is still flagged at the
    // destination. For a DIRECTORY, do the same per child file (renaming a
    // folder otherwise raises tamper_new_file + tamper_deleted for every child).
    if let Ok(src) = workspace.resolve_safe_for_write(&old_path) {
        pin_rename_sources(&kw, &sb, &src, &old_path, &new_path);
    }
    workspace.rename(&old_path, &new_path)?;
    let event = ActivityEvent::new(
        "file_rename",
        &format!("Renamed: {} → {}", old_path, new_path),
        None,
        None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);
    Ok(())
}

#[tauri::command]
fn ws_delete(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    ws: State<WorkspaceState>,
    kw: State<KnownWrites>,
    path: String,
) -> Result<(), String> {
    mark_known_write(&kw, &path);
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())?.delete(&path)?;
    let event = ActivityEvent::new(
        "file_delete",
        &format!("Deleted: {}", path),
        None,
        None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);
    Ok(())
}

#[tauri::command]
fn ws_root_path(ws: State<WorkspaceState>) -> Result<String, String> {
    let guard = ws.lock().map_err(|e| e.to_string())?;
    guard.as_ref().ok_or("No workspace initialized".to_string())
        .map(|w| w.root_path())
}

// ===== Move (drag-and-drop) =====

#[tauri::command]
fn ws_move(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    ws: State<WorkspaceState>,
    kw: State<KnownWrites>,
    sb: State<monitor::SharedBaseline>,
    src_path: String,
    dest_dir: String,
) -> Result<String, String> {
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

    mark_known_write(&kw, &src_path); // source disappears — deletion grace
    // Pin the dest from the SOURCE bytes BEFORE moving, gated on the baseline
    // (see pin_authorized_dest): no unpinned window, but an externally-tampered
    // source is still flagged at the destination. Directory moves pin every
    // child file the same way.
    if let Ok(src) = workspace.resolve_safe_for_write(&src_path) {
        pin_rename_sources(&kw, &sb, &src, &src_path, &new_path);
    }
    workspace.rename(&src_path, &new_path)?;

    let event = ActivityEvent::new(
        "file_move",
        &format!("Moved: {} → {}", src_path, new_path),
        None,
        None,
    );
    state.activity_log.lock().unwrap().add_event(event.clone());
    let _ = app_handle.emit("activity-event", &event);

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

/// App version from tauri.conf.json — shown in the toolbar/title so a student
/// (and proctor) can SEE at a glance whether the latest build is installed.
#[tauri::command]
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

/// True when the exam venv's python actually exists on disk. The first-launch
/// flow shows the setup wizard when this is false EVEN IF setup_done=true —
/// a stale config from an earlier install otherwise skipped the wizard on a
/// machine whose venv/packages are gone, silently leaving the student with a
/// package-less environment.
#[tauri::command]
fn exam_venv_ready() -> bool {
    python_exe_in_venv(&venv_dir_from_config()).exists()
}

#[tauri::command]
fn get_build_info() -> serde_json::Value {
    serde_json::json!({
        "commit_sha": env!("MINT_GIT_SHA"),
        "build_time": env!("MINT_BUILD_TIME"),
        "exe_hash": compute_self_hash().unwrap_or_else(|| "unavailable".to_string()),
    })
}

// Reentrancy guard for submit_exam. A double-click or rapid keyboard
// re-trigger of the Submit button would otherwise truncate the half-written
// zip and silently destroy the student's submission.
static SUBMITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct SubmitGuard;
impl SubmitGuard {
    fn try_acquire() -> Option<Self> {
        use std::sync::atomic::Ordering;
        if SUBMITTING.swap(true, Ordering::SeqCst) { None } else { Some(Self) }
    }
}
impl Drop for SubmitGuard {
    fn drop(&mut self) {
        SUBMITTING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

// `async`-flagged: submit stops recording (up to 15s graceful finalize), then
// zips + obfuscates video — seconds of blocking work that must stay off the
// main/UI thread so the window doesn't freeze during submission.
#[tauri::command(async)]
fn submit_exam(
    app_handle: tauri::AppHandle,
    state: State<AppState>,
    recorder: State<RecorderState>,
    ws: State<WorkspaceState>,
    student_id: String,
) -> Result<SubmitResult, String> {
    let _guard = SubmitGuard::try_acquire()
        .ok_or_else(|| "이미 제출이 진행 중입니다. 잠시 기다려 주세요.".to_string())?;

    // Reject malformed student_id early — it's interpolated into folder
    // names (Desktop) and SHA-256 password.
    let id_trim = student_id.trim();
    if id_trim.is_empty() || id_trim.len() > 32
        || id_trim.chars().any(|c| !c.is_ascii_alphanumeric())
    {
        return Err("학번은 영문/숫자 1~32자만 사용 가능합니다.".to_string());
    }
    let student_id = id_trim.to_string();

    // 1. Stop recording. Bump RECORDING_EPOCH WHILE STILL HOLDING the recorder
    //    lock (before the guard drops) — the health watchdog re-checks the epoch
    //    immediately after it acquires the same lock, so bumping under the lock
    //    guarantees it observes the invalidation and never fires a false
    //    "recording process died" alert during a legitimate submit. (Bumping
    //    after releasing the lock left a window where the watchdog could acquire
    //    the freed lock, see the just-stopped process as not-alive, and alarm.)
    {
        let mut rec = recorder.lock().map_err(|e| e.to_string())?;
        if rec.is_recording() {
            let _ = rec.stop();
        }
        RECORDING_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    // 2. Workspace root
    let ws_root = {
        let guard = ws.lock().map_err(|e| e.to_string())?;
        guard.as_ref().ok_or("No workspace initialized".to_string())?.root_path()
    };

    // 3. Create submission folder on Desktop FIRST, so the exam_submitted
    //    marker recorded below is captured in the zipped logs.
    let desktop = dirs::desktop_dir()
        .ok_or("Cannot find Desktop directory")?;
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let folder_name = format!("MINT_Exam_{}_{}", timestamp, student_id);
    let submit_dir = desktop.join(&folder_name);
    std::fs::create_dir_all(&submit_dir)
        .map_err(|e| format!("Failed to create submission folder: {}", e))?;

    // 4. Record the submission event BEFORE serializing logs so it actually
    //    appears in the zipped _log_app_focus.json / _log_complete.json (the
    //    focus filter lists "exam_submitted"; previously it was added after the
    //    zip was built, so the filter claimed an event that was never present).
    {
        let event = ActivityEvent::new(
            "exam_submitted",
            &format!("Submitted by {}: {}", student_id, submit_dir.to_string_lossy()),
            None, None,
        );
        state.activity_log.lock().unwrap().add_event(event.clone());
        let _ = app_handle.emit("activity-event", &event);
    }

    // 5. Save activity logs as separate files. Propagate write errors — these
    //    files ARE the on-disk anti-cheat evidence zipped at the next step, so
    //    silently swallowing a failure would ship a submission with missing or
    //    stale evidence while reporting success. Returning Err lets the student
    //    retry while the in-memory log is still intact.
    //
    //    ONE snapshot of the event log is taken here and reused for the video
    //    scoping + manifest below (it was cloned three separate times, each a
    //    full deep copy of every event in the session).
    let events = state.activity_log.lock().unwrap().get_events();
    {
        let focus_log: Vec<_> = events.iter()
            .filter(|e| matches!(e.event_type.as_str(),
                "focus_lost" | "focus_returned" | "session_start" | "exam_submitted"))
            .cloned().collect();

        let background_log: Vec<_> = events.iter()
            .filter(|e| matches!(e.event_type.as_str(),
                "clipboard_internal" | "clipboard_external" | "recording_start" | "recording_stop" | "file_import" |
                "tamper_detected" | "tamper_new_file" | "tamper_deleted" |
                // Monitoring/recording health failures are anti-cheat-relevant
                // evidence (they say WHY a signal may be missing) — they were
                // only in the complete log, not the focused background one.
                "recording_health_fail" | "monitor_health_fail"))
            .cloned().collect();

        let editor_log: Vec<_> = events.iter()
            .filter(|e| matches!(e.event_type.as_str(),
                "paste" | "paste_large" | "input_burst" | "typing_summary" |
                "code_run" | "code_run_result" | "copy" | "cut" |
                "terminal_stdout" | "terminal_stderr"))
            .cloned().collect();

        let ws_path = std::path::PathBuf::from(&ws_root);
        let write_log = |name: &str, body: String| -> Result<(), String> {
            std::fs::write(ws_path.join(name), body)
                .map_err(|e| format!("활동 로그 저장 실패({}): {}", name, e))
        };
        write_log("_log_app_focus.json", serde_json::to_string_pretty(&focus_log).unwrap_or_default())?;
        write_log("_log_background.json", serde_json::to_string_pretty(&background_log).unwrap_or_default())?;
        write_log("_log_editor_activity.json", serde_json::to_string_pretty(&editor_log).unwrap_or_default())?;
        write_log("_log_complete.json", serde_json::to_string_pretty(&events).unwrap_or_default())?;
    }

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
    let mut video_source_count = 0u32;  // mp4/mov files we tried to copy
    let mut video_errors: Vec<String> = Vec::new();

    // Recording-start epoch for THIS session — used both to scope which
    // recordings belong to this student and (in the manifest below) to compute
    // video offsets. If recording never started this session, there are no
    // recordings of ours to collect.
    let rec_start_ms: Option<i64> = events.iter()
        .find(|e| e.event_type == "recording_start")
        .map(|e| e.epoch_ms);
    // mtime floor (secs), 60s margin for clock/mtime skew. Recordings older
    // than this belong to a previous/other session (shared exam PC) and must
    // not be bundled into — or counted toward — this student's submission.
    let session_start_secs: Option<u64> =
        rec_start_ms.map(|ms| ((ms / 1000) as u64).saturating_sub(60));

    if rec_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&rec_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_default();
                if ext != "mp4" && ext != "mov" {
                    continue;
                }

                let meta = match std::fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                // Skip truncated/empty captures (failed-strategy leftovers,
                // 0-byte permission failures): neither count nor ship them, so
                // the silent-loss guard below stays meaningful.
                if meta.len() < 1024 {
                    continue;
                }
                // Scope to this session by mtime.
                let mtime_secs = meta.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                match session_start_secs {
                    Some(floor) => { if mtime_secs < floor { continue; } }
                    None => continue, // recording disabled / never started this session
                }

                video_source_count += 1;
                let dest = video_dir.join(path.file_name().unwrap());
                // COPY before delete — if anything downstream fails (ENOSPC,
                // OneDrive lock), the originals in Recordings/ are intact and
                // the student can retry.
                if let Err(e) = std::fs::copy(&path, &dest) {
                    video_errors.push(format!(
                        "{}: copy failed ({})",
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                        e
                    ));
                    continue;
                }
                // Obfuscation MUST succeed before we count it. On failure, remove
                // the plaintext copy (original stays in Recordings/ for retry)
                // and DON'T count it — never ship a plaintext recording that the
                // manifest claims is obfuscated (the grader would otherwise
                // double-XOR and corrupt it).
                match recorder::obfuscate_video(&dest, &password_bytes) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&path);
                        video_count += 1;
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&dest);
                        video_errors.push(format!(
                            "{}: obfuscate failed ({})",
                            path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                            e
                        ));
                    }
                }
            }
        }
    }

    // SILENT VIDEO LOSS GUARD: if there ARE source recordings but NONE made
    // it into the submission folder, refuse to claim "submit complete". The
    // student gets a clear error and can retry (originals are still in
    // Recordings/ thanks to copy-before-delete).
    if video_source_count > 0 && video_count == 0 {
        return Err(format!(
            "녹화 파일 {}개를 제출 폴더로 복사하지 못했습니다. 디스크 여유 공간을 확인하세요.\n원본은 {}에 남아 있으니 재시도 가능합니다.\n\n상세:\n{}",
            video_source_count,
            rec_dir.display(),
            video_errors.join("\n")
        ));
    }

    // 8. Manifest (IDE integrity + student setup + suspicious event timestamps)
    let ide_exe_hash = compute_self_hash().unwrap_or_else(|| "unavailable".to_string());
    let cfg = setup::load_config();

    // (rec_start_ms was computed above, before the video loop.)

    // Suspicious events with seconds-from-recording-start (helps grader jump to video).
    let suspect_types = [
        "tamper_detected","tamper_new_file","tamper_deleted",
        "clipboard_external","paste_large","focus_lost","file_import",
    ];
    let mut suspect_events: Vec<serde_json::Value> = Vec::new();
    if let Some(start) = rec_start_ms {
        for e in events.iter() {
            if !suspect_types.contains(&e.event_type.as_str()) { continue; }
            let offset_s = ((e.epoch_ms - start) as f64) / 1000.0;
            if offset_s < 0.0 { continue; }
            suspect_events.push(serde_json::json!({
                "t": e.timestamp,
                "type": e.event_type,
                "video_offset_s": (offset_s * 10.0).round() / 10.0,
                "detail": e.detail,
            }));
        }
    }

    let manifest = serde_json::json!({
        "student_id": student_id,
        "timestamp": timestamp,
        "hash_check": &password[..16],
        "video_count": video_count,
        "video_obfuscated": video_count > 0,
        "ide_commit_sha": env!("MINT_GIT_SHA"),
        "ide_build_time": env!("MINT_BUILD_TIME"),
        "ide_exe_hash": ide_exe_hash,
        "setup_config": {
            "package_profile": cfg.package_profile,
            "custom_packages": cfg.custom_packages,
            "recording_enabled": cfg.recording_enabled,
            "include_sample_code": cfg.include_sample_code,
            "custom_venv_path": cfg.custom_venv_path,
        },
        "recording_start_epoch_ms": rec_start_ms,
        "suspect_events": suspect_events,
        // Partial video loss (some copied, some failed) previously vanished —
        // only TOTAL loss aborted the submit. Record per-file failures so the
        // grader can see a submission shipped with fewer recordings than the
        // session produced.
        "video_errors": video_errors,
    });
    // Atomic + error-propagating: a missing/empty manifest makes the whole
    // submission invisible to the grader (scan_submissions skips manifest-less
    // folders). Write to a temp file then rename, and return Err on failure so
    // the student retries (the code zip & videos are already written, originals
    // intact) instead of being told "제출 완료" over a broken submission.
    {
        let manifest_str = serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("manifest 직렬화 실패: {}", e))?;
        let manifest_path = submit_dir.join("manifest.json");
        let manifest_tmp = submit_dir.join("manifest.json.tmp");
        std::fs::write(&manifest_tmp, manifest_str.as_bytes())
            .map_err(|e| format!("manifest 저장 실패: {}", e))?;
        std::fs::rename(&manifest_tmp, &manifest_path)
            .map_err(|e| format!("manifest 저장(rename) 실패: {}", e))?;
    }

    let folder_str = submit_dir.to_string_lossy().to_string();
    let code_str = code_zip_path.to_string_lossy().to_string();
    let video_str = video_dir.to_string_lossy().to_string();

    // (The exam_submitted activity event is recorded earlier, before the logs
    //  are serialized, so it is present in the zipped evidence.)

    Ok(SubmitResult { folder_path: folder_str, code_zip: code_str, video_zip: video_str })
}

fn create_encrypted_zip(workspace_root: &str, zip_path: &std::path::Path, password: &str) -> Result<(), String> {
    let file = std::fs::File::create(zip_path)
        .map_err(|e| format!("Failed to create zip: {}", e))?;
    let mut zip = zip::ZipWriter::new(file);

    let root = std::path::Path::new(workspace_root);
    add_dir_to_zip_encrypted(&mut zip, root, root, password)?;

    zip.finish().map_err(|e| format!("Failed to finish zip: {}", e))?;
    Ok(())
}

/// True if a directory entry is a symlink (any OS) or a Windows reparse point
/// (junction / mount point). Mirrors the guards in integrity.rs / runner.rs so
/// the submit zip walker doesn't follow a workspace junction out of bounds or
/// into an infinite loop.
fn entry_is_link(entry: &std::fs::DirEntry) -> bool {
    if let Ok(ft) = entry.file_type() {
        if ft.is_symlink() {
            return true;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if let Ok(md) = entry.metadata() {
            if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return true;
            }
        }
    }
    false
}

fn add_dir_to_zip_encrypted<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &std::path::Path,
    root: &std::path::Path,
    password: &str,
) -> Result<(), String> {
    use zip::write::SimpleFileOptions;
    use zip::AesMode;

    if !dir.is_dir() { return Ok(()); }

    const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "mkv", "webm"];

    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        // Never follow symlinks / junctions. A workspace-local junction loop
        // (`mklink /J`, no admin needed) would otherwise make this recursion
        // spin until the stack overflows and ABORTS the whole submit — no code
        // zip, no manifest, submission destroyed — and a junction pointing OUT
        // of the workspace would bundle external files under a workspace path.
        // The integrity scanner and workspace snapshot already skip reparse
        // points; this walker must match them.
        if entry_is_link(&entry) {
            continue;
        }
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
            // STREAM the file into the zip. read_to_end buffered each member
            // whole: a student who imported a large dataset spiked RAM by that
            // file's full size at submit time — the worst possible moment to
            // OOM on an 8GB exam laptop.
            let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            std::io::copy(&mut f, zip).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

// ===== App Entry =====

/// Gate for process exit. The frontend's close guard / submit flow set this
/// (via allow_exit) right before calling exit(0); a window that is truly gone
/// (Destroyed) also sets it so a dead-webview close can't leave a headless
/// zombie. Any OTHER exit request — most importantly macOS Cmd+Q / menu Quit,
/// which never passes through the JS onCloseRequested guard — is vetoed, so a
/// student cannot skip the unsaved-work confirm + edit-history flush by
/// quitting from the menu.
static EXIT_ALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[tauri::command]
fn allow_exit() {
    EXIT_ALLOWED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // macOS: if re-launched by screen_capture_authorized_fresh() to read the
    // current Screen Recording TCC verdict, print it and exit BEFORE any GUI is
    // created. No-op on other platforms / normal launches.
    recorder::run_tcc_preflight_and_exit_if_requested();

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
        .manage(monitor::new_shared_baseline())
        .manage(runner::new_running_process())
        .manage(runner::new_running_stdin())
        .invoke_handler(tauri::generate_handler![
            get_activity_log,
            clear_activity_log,
            export_activity_log,
            log_editor_event,
            run_code,
            run_code_sync,
            stop_code,
            stop_notebook,
            send_stdin,
            close_stdin,
            detect_compilers,
            verify_cpp_environment,
            current_compiler,
            allow_exit,
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
            get_app_version,
            exam_venv_ready,
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
            ws_file_size,
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
            verify_exam_environment,
            log_python_change,
            save_code_history,
            submit_exam,
        ])
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let running = app.state::<runner::RunningProcess>().inner().clone();
            monitor::start_clipboard_monitor(log_handle.clone(), app_handle.clone());
            monitor::start_focus_monitor(log_handle.clone(), app_handle, running);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running MINT Exam IDE")
        .run(|app_handle, event| {
            use std::sync::atomic::Ordering;
            match event {
                // The window is really gone (close guard already ran, or the
                // webview died) — from here on, exiting is always legitimate.
                tauri::RunEvent::WindowEvent {
                    event: tauri::WindowEvent::Destroyed, ..
                } => {
                    EXIT_ALLOWED.store(true, Ordering::SeqCst);
                }
                // Last-chance cleanup. The exit that follows is
                // std::process::exit — NO destructors run, so without this an
                // active ffmpeg child was ORPHANED and kept recording the
                // desktop indefinitely after the IDE closed (and a running
                // python child kept executing). Stopping the recorder here also
                // finalizes the mp4's moov atom so the last segment stays
                // playable even on a quit-without-submit.
                tauri::RunEvent::Exit => {
                    RECORDING_EPOCH.fetch_add(1, Ordering::SeqCst);
                    if let Some(rec) = app_handle.try_state::<RecorderState>() {
                        if let Ok(mut r) = rec.lock() {
                            let _ = r.stop();
                        }
                    }
                    if let Some(sin) = app_handle.try_state::<runner::RunningStdin>() {
                        runner::close_stdin(&sin);
                    }
                    if let Some(proc) = app_handle.try_state::<runner::RunningProcess>() {
                        if let Some(child) =
                            proc.lock().ok().and_then(|mut g| g.take().map(|(_, c)| c))
                        {
                            runner::stop_taken_child(child);
                        }
                    }
                    let _ = stop_notebook();
                }
                // Cmd+Q / menu Quit / stray app.exit: veto unless the frontend
                // explicitly authorized it (allow_exit before exit(0)).
                tauri::RunEvent::ExitRequested { api, .. } => {
                    if !EXIT_ALLOWED.load(Ordering::SeqCst) {
                        api.prevent_exit();
                    }
                }
                _ => {}
            }
        });
}
