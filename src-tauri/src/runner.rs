use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter};

// Cached Python path — found once, reused forever
static CACHED_PYTHON: Mutex<Option<String>> = Mutex::new(None);

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
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1")
            .env("TF_CPP_MIN_LOG_LEVEL", "3")      // suppress TensorFlow warnings
            .env("TF_ENABLE_ONEDNN_OPTS", "0")      // suppress oneDNN messages
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

/// Run a pip command with streaming output. Returns final exit code.
fn run_pip_streaming(py_cmd: &str, args: &[&str], app: &AppHandle) -> i32 {
    let mut command = Command::new(py_cmd);
    command.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let child = command.spawn();
    match child {
        Ok(mut c) => {
            let stdout = c.stdout.take();
            let stderr = c.stderr.take();
            let app2 = app.clone();
            let t1 = thread::spawn(move || {
                if let Some(out) = stdout {
                    for line in BufReader::new(out).lines().flatten() {
                        emit_line(&app2, "stdout", &format!("{}\n", line));
                    }
                }
            });
            let app3 = app.clone();
            let t2 = thread::spawn(move || {
                if let Some(err) = stderr {
                    for line in BufReader::new(err).lines().flatten() {
                        emit_line(&app3, "stderr", &format!("{}\n", line));
                    }
                }
            });
            t1.join().ok();
            t2.join().ok();
            c.wait().ok().and_then(|s| s.code()).unwrap_or(-1)
        }
        Err(e) => {
            emit_line(app, "stderr", &format!("pip failed: {}\n", e));
            -1
        }
    }
}

/// Smart install: routes torch / tensorflow through their proper indexes.
pub fn pip_install_smart(
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
                emit_line(&app_handle, "system", "[INSTALL_DONE:fail]\n");
                return;
            }
        };

        let mut torch_pkgs: Vec<String> = Vec::new();
        let mut tf_pkgs: Vec<String> = Vec::new();
        let mut other_pkgs: Vec<String> = Vec::new();
        for pkg in &pkgs {
            let lower = pkg.to_lowercase();
            if lower.starts_with("torch") {
                torch_pkgs.push(pkg.clone());
            } else if lower.starts_with("tensorflow") {
                tf_pkgs.push(pkg.clone());
            } else {
                other_pkgs.push(pkg.clone());
            }
        }

        let mut overall_ok = true;

        if !other_pkgs.is_empty() {
            emit_line(&app_handle, "system",
                &format!("Installing: {}\n", other_pkgs.join(", ")));
            let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into(), "--upgrade".into()];
            args.extend(other_pkgs);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
            if code != 0 { overall_ok = false; }
            emit_line(&app_handle, "system", &format!("[core exit {}]\n", code));
        }

        if !torch_pkgs.is_empty() {
            emit_line(&app_handle, "system",
                &format!("Installing PyTorch (CPU): {}\n", torch_pkgs.join(", ")));
            let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into()];
            args.extend(torch_pkgs);
            args.push("--index-url".into());
            args.push("https://download.pytorch.org/whl/cpu".into());
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
            if code != 0 { overall_ok = false; }
            emit_line(&app_handle, "system", &format!("[torch exit {}]\n", code));
        }

        if !tf_pkgs.is_empty() {
            emit_line(&app_handle, "system",
                &format!("Installing TensorFlow: {}\n", tf_pkgs.join(", ")));
            let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into()];
            args.extend(tf_pkgs);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
            if code != 0 { overall_ok = false; }
            emit_line(&app_handle, "system", &format!("[tensorflow exit {}]\n", code));
        }

        let marker = if overall_ok { "[INSTALL_DONE:ok]\n" } else { "[INSTALL_DONE:partial]\n" };
        emit_line(&app_handle, "system", marker);
    });
}

pub fn pip_uninstall(
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
                emit_line(&app_handle, "system", "[UNINSTALL_DONE:fail]\n");
                return;
            }
        };
        emit_line(&app_handle, "system",
            &format!("Uninstalling: {}\n", pkgs.join(", ")));
        let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "uninstall".into(), "-y".into()];
        args.extend(pkgs);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
        let marker = if code == 0 { "[UNINSTALL_DONE:ok]\n" } else { "[UNINSTALL_DONE:fail]\n" };
        emit_line(&app_handle, "system", marker);
    });
}

pub fn pip_list(python_path: Option<&str>) -> Vec<String> {
    let py = match find_python(python_path) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut command = Command::new(&py);
    command.args(["-m", "pip", "list", "--format=freeze"]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = match command.output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|l| l.split("==").next().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
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

pub fn find_python_cached(python_path: Option<&str>) -> Option<String> {
    find_python(python_path)
}

fn find_python(python_path: Option<&str>) -> Option<String> {
    // User-selected path takes priority
    if let Some(py) = python_path {
        return Some(py.to_string());
    }

    // Return cached path if available (instant)
    if let Ok(cache) = CACHED_PYTHON.lock() {
        if let Some(ref cached) = *cache {
            return Some(cached.clone());
        }
    }

    // First-time discovery
    let found = discover_python();
    if let Some(ref py) = found {
        if let Ok(mut cache) = CACHED_PYTHON.lock() {
            *cache = Some(py.clone());
        }
    }
    found
}

fn discover_python() -> Option<String> {
    // Fast: check PATH first (no subprocess spawn for exists-check)
    #[cfg(target_os = "windows")]
    {
        // Check well-known Windows paths by file existence (no process spawn = instant)
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();

        let direct_paths = [
            format!("{}\\AppData\\Local\\Programs\\Python\\Python312\\python.exe", home),
            format!("{}\\AppData\\Local\\Programs\\Python\\Python311\\python.exe", home),
            format!("{}\\AppData\\Local\\Programs\\Python\\Python310\\python.exe", home),
            format!("{}\\anaconda3\\python.exe", home),
            format!("{}\\miniconda3\\python.exe", home),
            "C:\\Python312\\python.exe".to_string(),
            "C:\\Python311\\python.exe".to_string(),
        ];

        for p in &direct_paths {
            if std::path::Path::new(p).exists() {
                return Some(p.clone());
            }
        }

        // Fallback: try PATH commands (slower, spawns process)
        use std::os::windows::process::CommandExt;
        for cmd in ["python", "py"] {
            if let Ok(out) = Command::new(cmd)
                .arg("--version")
                .stdout(Stdio::null()).stderr(Stdio::null())
                .creation_flags(0x08000000)
                .status()
            {
                if out.success() { return Some(cmd.to_string()); }
            }
        }

        // Scan LOCALAPPDATA
        let base = std::path::PathBuf::from(&local).join("Programs").join("Python");
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let py = entry.path().join("python.exe");
                if py.exists() { return Some(py.to_string_lossy().to_string()); }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // macOS/Linux: check common paths by file existence
        let home = std::env::var("HOME").unwrap_or_default();
        let paths = [
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
            "/usr/bin/python3",
            &format!("{}/anaconda3/bin/python", home),
            &format!("{}/miniconda3/bin/python", home),
            &format!("{}/miniforge3/bin/python", home),
        ];
        for p in paths {
            if std::path::Path::new(p).exists() { return Some(p.to_string()); }
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
