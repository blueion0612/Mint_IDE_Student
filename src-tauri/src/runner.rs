use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter};

/// Event sent to frontend for each line of output
#[derive(Debug, Clone, Serialize)]
pub struct RunOutputLine {
    pub stream: String, // "stdout", "stderr", "system"
    pub text: String,
}

/// Shared handle to the running process so it can be stopped
pub type RunningProcess = Arc<Mutex<Option<Child>>>;

pub fn new_running_process() -> RunningProcess {
    Arc::new(Mutex::new(None))
}

/// Execute code with real-time streaming output via events.
/// Returns immediately — output comes through "run-output" events.
pub fn execute_code_streaming(
    language: &str,
    code: &str,
    filename: &str,
    workspace_dir: Option<&str>,
    python_path: Option<&str>,
    app_handle: AppHandle,
    process_handle: RunningProcess,
) {
    let work_dir = workspace_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mint-exam-ide"));
    let _ = std::fs::create_dir_all(&work_dir);

    let lang = language.to_string();
    let code = code.to_string();
    let fname = filename.to_string();
    let py_path = python_path.map(|s| s.to_string());
    let dir = work_dir.clone();

    thread::spawn(move || {
        let start = std::time::Instant::now();

        // Write file
        let file_path = dir.join(&fname);
        if let Some(parent) = file_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&file_path, &code).is_err() {
            emit_line(&app_handle, "system", "Failed to write file\n");
            emit_done_with_output(&app_handle, None, 0, "", "");
            return;
        }

        // Build command
        let result = match lang.as_str() {
            "python" => build_python_cmd(&dir, &fname, py_path.as_deref()),
            "javascript" | "typescript" => build_node_cmd(&dir, &fname),
            "c" => build_and_run_c(&dir, &fname, &app_handle),
            "cpp" => build_and_run_cpp(&dir, &fname, &app_handle),
            "java" => build_and_run_java(&dir, &fname, &app_handle),
            _ => {
                emit_line(&app_handle, "stderr", &format!("Unsupported language: {}\n", lang));
                emit_done_with_output(&app_handle, None, 0, "", "");
                return;
            }
        };

        let (cmd, args) = match result {
            Some(v) => v,
            None => return, // compile error already emitted
        };

        // Spawn with piped stdout/stderr
        let mut command = Command::new(&cmd);
        command.args(&args)
            .current_dir(&dir)
            .env("PYTHONUNBUFFERED", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Hide console window on Windows (does NOT affect GUI windows like matplotlib)
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        let child = command.spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                emit_line(&app_handle, "stderr", &format!("Failed to run '{}': {}. Is it installed?\n", cmd, e));
                emit_done_with_output(&app_handle, None, 0, "", "");
                return;
            }
        };

        // Store process handle for Stop button
        {
            let mut guard = process_handle.lock().unwrap();
            *guard = None; // will set after taking stdout/stderr
        }

        // Stream stdout
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Now store the child for stop
        {
            let mut guard = process_handle.lock().unwrap();
            *guard = Some(child);
        }

        let ah1 = app_handle.clone();

        let stdout_collected = Arc::new(Mutex::new(String::new()));
        let stderr_collected = Arc::new(Mutex::new(String::new()));
        let sc1 = stdout_collected.clone();
        let sc2 = stderr_collected.clone();

        // stdout: stream in real-time (line by line)
        let t1 = thread::spawn(move || {
            if let Some(out) = stdout {
                let reader = BufReader::new(out);
                for line in reader.lines() {
                    if let Ok(l) = line {
                        let text = format!("{}\n", l);
                        sc1.lock().unwrap().push_str(&text);
                        emit_line(&ah1, "stdout", &text);
                    }
                }
            }
        });

        // stderr: collect silently, display AFTER stdout finishes
        // This prevents interleaving (traceback mixed into print output)
        let t2 = thread::spawn(move || {
            if let Some(err) = stderr {
                let reader = BufReader::new(err);
                for line in reader.lines() {
                    if let Ok(l) = line {
                        sc2.lock().unwrap().push_str(&format!("{}\n", l));
                    }
                }
            }
        });

        // Wait for stdout to finish first
        t1.join().ok();
        t2.join().ok();

        // Now emit stderr all at once (after stdout)
        {
            let err_str = stderr_collected.lock().unwrap().clone();
            if !err_str.is_empty() {
                emit_line(&app_handle, "stderr", &err_str);
            }
        }

        let exit_code = {
            let mut guard = process_handle.lock().unwrap();
            if let Some(ref mut child) = *guard {
                child.wait().ok().and_then(|s| s.code())
            } else {
                None
            }
        };

        {
            let mut guard = process_handle.lock().unwrap();
            *guard = None;
        }

        let elapsed = start.elapsed().as_millis() as u64;
        let stdout_str = stdout_collected.lock().unwrap().clone();
        let stderr_str = stderr_collected.lock().unwrap().clone();
        emit_done_with_output(&app_handle, exit_code, elapsed, &stdout_str, &stderr_str);
    });
}

/// Stop the currently running process
pub fn stop_process(process_handle: &RunningProcess) -> bool {
    let mut guard = process_handle.lock().unwrap();
    if let Some(ref mut child) = *guard {
        let _ = child.kill();
        *guard = None;
        true
    } else {
        false
    }
}

/// pip install packages
pub fn pip_install(
    packages: &[String],
    python_path: Option<&str>,
    app_handle: AppHandle,
) {
    let py = find_python(python_path);
    let pkgs = packages.to_vec();

    thread::spawn(move || {
        let py_cmd = match py {
            Some(p) => p,
            None => {
                emit_line(&app_handle, "stderr", "Python not found\n");
                return;
            }
        };

        emit_line(&app_handle, "system", &format!("$ {} -m pip install {}\n", py_cmd, pkgs.join(" ")));

        let mut args = vec!["-m".to_string(), "pip".to_string(), "install".to_string(), "--user".to_string()];
        args.extend(pkgs);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let child = Command::new(&py_cmd)
            .args(&arg_refs)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match child {
            Ok(mut c) => {
                if let Some(out) = c.stdout.take() {
                    let reader = BufReader::new(out);
                    for line in reader.lines().flatten() {
                        emit_line(&app_handle, "stdout", &format!("{}\n", line));
                    }
                }
                let status = c.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                emit_line(&app_handle, "system", &format!("[pip exit {}]\n", status));
            }
            Err(e) => {
                emit_line(&app_handle, "stderr", &format!("pip failed: {}\n", e));
            }
        }
    });
}

// ===== Helpers =====

fn emit_line(app: &AppHandle, stream: &str, text: &str) {
    let _ = app.emit("run-output", RunOutputLine {
        stream: stream.to_string(),
        text: text.to_string(),
    });
}

fn emit_done_with_output(app: &AppHandle, exit_code: Option<i32>, duration_ms: u64, stdout: &str, stderr: &str) {
    #[derive(Clone, Serialize)]
    struct RunDone {
        exit_code: Option<i32>,
        duration_ms: u64,
        stdout: String,
        stderr: String,
    }
    let _ = app.emit("run-done", RunDone {
        exit_code, duration_ms,
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    });
}

fn find_python(python_path: Option<&str>) -> Option<String> {
    if let Some(py) = python_path {
        return Some(py.to_string());
    }

    let candidates = if cfg!(target_os = "windows") {
        vec!["python", "python3", "py"]
    } else {
        vec!["python3", "python"]
    };

    for cmd in &candidates {
        if Command::new(cmd).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
            return Some(cmd.to_string());
        }
    }

    // Windows: scan common locations
    #[cfg(target_os = "windows")]
    {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        for dir_name in ["anaconda3", "miniconda3", "Anaconda3", "Miniconda3"] {
            let py = format!("{}\\{}\\python.exe", home, dir_name);
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

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        for p in [
            "/opt/homebrew/bin/python3", "/usr/local/bin/python3", "/usr/bin/python3",
        ] {
            if std::path::Path::new(p).exists() { return Some(p.to_string()); }
        }
        for name in ["anaconda3", "miniconda3", "miniforge3"] {
            let py = format!("{}/{}/bin/python", home, name);
            if std::path::Path::new(&py).exists() { return Some(py); }
        }
    }

    None
}

fn build_python_cmd(dir: &Path, filename: &str, python_path: Option<&str>) -> Option<(String, Vec<String>)> {
    let py = find_python(python_path)?;
    let file = dir.join(filename);
    Some((py, vec![file.to_string_lossy().to_string()]))
}

fn build_node_cmd(dir: &Path, filename: &str) -> Option<(String, Vec<String>)> {
    let file = dir.join(filename);
    Some(("node".to_string(), vec![file.to_string_lossy().to_string()]))
}

fn compile_cmd(compiler: &str, src: &Path, out: &Path, extra_args: &[&str], app: &AppHandle) -> Option<()> {
    let mut args: Vec<String> = vec![src.to_string_lossy().to_string(), "-o".to_string(), out.to_string_lossy().to_string()];
    args.extend(extra_args.iter().map(|s| s.to_string()));

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = Command::new(compiler).args(&arg_refs).output();

    match output {
        Ok(o) => {
            if !o.status.success() {
                emit_line(app, "stderr", "[Compilation Error]\n");
                emit_line(app, "stderr", &String::from_utf8_lossy(&o.stderr));
                emit_done_with_output(app, o.status.code(), 0, "", "");
                return None;
            }
            Some(())
        }
        Err(e) => {
            emit_line(app, "stderr", &format!("Failed to run '{}': {}. Is it installed?\n", compiler, e));
            emit_done_with_output(app, None, 0, "", "");
            None
        }
    }
}

fn build_and_run_c(dir: &Path, filename: &str, app: &AppHandle) -> Option<(String, Vec<String>)> {
    let src = dir.join(filename);
    let out_name = if cfg!(windows) { "a.exe" } else { "a.out" };
    let out = dir.join(out_name);
    compile_cmd("gcc", &src, &out, &["-lm"], app)?;
    Some((out.to_string_lossy().to_string(), vec![]))
}

fn build_and_run_cpp(dir: &Path, filename: &str, app: &AppHandle) -> Option<(String, Vec<String>)> {
    let src = dir.join(filename);
    let out_name = if cfg!(windows) { "a.exe" } else { "a.out" };
    let out = dir.join(out_name);
    compile_cmd("g++", &src, &out, &["-std=c++17"], app)?;
    Some((out.to_string_lossy().to_string(), vec![]))
}

fn build_and_run_java(dir: &Path, filename: &str, app: &AppHandle) -> Option<(String, Vec<String>)> {
    let src = dir.join(filename);
    let src_str = src.to_string_lossy().to_string();
    let dir_str = dir.to_string_lossy().to_string();

    let output = Command::new("javac").arg(&src_str).output();
    match output {
        Ok(o) if !o.status.success() => {
            emit_line(app, "stderr", "[Compilation Error]\n");
            emit_line(app, "stderr", &String::from_utf8_lossy(&o.stderr));
            emit_done_with_output(app, o.status.code(), 0, "", "");
            return None;
        }
        Err(e) => {
            emit_line(app, "stderr", &format!("Failed to run 'javac': {}\n", e));
            emit_done_with_output(app, None, 0, "", "");
            return None;
        }
        _ => {}
    }

    let basename = filename.rsplit('/').next().unwrap_or(filename);
    let class_name = basename.trim_end_matches(".java");
    Some(("java".to_string(), vec!["-cp".to_string(), dir_str, class_name.to_string()]))
}
