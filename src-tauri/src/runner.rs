use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

/// Runs code and returns the output.
/// If workspace_dir is provided, files are written there (so imports work).
/// Otherwise uses a temp directory.
pub fn execute_code(language: &str, code: &str, filename: &str, workspace_dir: Option<&str>, python_path: Option<&str>) -> RunResult {
    let work_dir = workspace_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mint-exam-ide"));
    let _ = std::fs::create_dir_all(&work_dir);

    let start = Instant::now();

    let result = match language {
        "python" => run_python(&work_dir, code, filename, python_path),
        "javascript" => run_node(&work_dir, code, filename),
        "typescript" => run_node(&work_dir, code, filename),
        "c" => run_c(&work_dir, code, filename),
        "cpp" => run_cpp(&work_dir, code, filename),
        "java" => run_java(&work_dir, code, filename),
        _ => Err(format!("Unsupported language: {}", language)),
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok((stdout, stderr, code)) => RunResult { stdout, stderr, exit_code: code, duration_ms },
        Err(e) => RunResult { stdout: String::new(), stderr: e, exit_code: None, duration_ms },
    }
}

fn write_file(dir: &Path, name: &str, content: &str) -> Result<std::path::PathBuf, String> {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = std::fs::File::create(&path)
        .map_err(|e| format!("Failed to create file: {}", e))?;
    file.write_all(content.as_bytes())
        .map_err(|e| format!("Failed to write file: {}", e))?;
    Ok(path)
}

fn run_cmd(cmd: &str, args: &[&str], cwd: &Path) -> Result<(String, String, Option<i32>), String> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .env("PYTHONUNBUFFERED", "1")  // real-time output for Python
        .output()
        .map_err(|e| format!("Failed to run '{}': {}. Is it installed?", cmd, e))?;

    Ok((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code(),
    ))
}

fn run_python(dir: &Path, code: &str, filename: &str, python_path: Option<&str>) -> Result<(String, String, Option<i32>), String> {
    let path = write_file(dir, filename, code)?;
    let path_str = path.to_string_lossy().to_string();

    // If user explicitly selected a Python path, use it
    if let Some(py) = python_path {
        return run_cmd(py, &[&path_str], dir);
    }

    // Try common command names (works if Python is in PATH)
    // "py" is the Windows Python Launcher — handles version selection
    let candidates = if cfg!(target_os = "windows") {
        vec!["python", "python3", "py"]
    } else {
        vec!["python3", "python"]
    };

    for cmd in &candidates {
        if let Ok(r) = run_cmd(cmd, &[&path_str], dir) {
            return Ok(r);
        }
    }

    // Windows: search common install locations + Conda
    #[cfg(target_os = "windows")]
    {
        let home = std::env::var("USERPROFILE").unwrap_or_default();

        // Direct conda base installs
        let conda_directs: Vec<std::path::PathBuf> = [
            format!("{}\\anaconda3\\python.exe", home),
            format!("{}\\miniconda3\\python.exe", home),
            format!("{}\\Anaconda3\\python.exe", home),
            format!("{}\\Miniconda3\\python.exe", home),
        ].into_iter().map(std::path::PathBuf::from).collect();

        for py in &conda_directs {
            if py.exists() {
                let py_str = py.to_string_lossy().to_string();
                if let Ok(r) = run_cmd(&py_str, &[&path_str], dir) {
                    return Ok(r);
                }
            }
        }

        // Standard Python installs
        let search_bases: Vec<std::path::PathBuf> = [
            std::env::var("LOCALAPPDATA").ok().map(|v| std::path::PathBuf::from(v).join("Programs").join("Python")),
            std::env::var("PROGRAMFILES").ok().map(|v| std::path::PathBuf::from(v).join("Python")),
            Some(std::path::PathBuf::from("C:\\Python3")),
        ].into_iter().flatten().collect();

        for base in &search_bases {
            if let Ok(entries) = std::fs::read_dir(base) {
                let mut dirs: Vec<_> = entries.flatten().collect();
                dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
                for entry in dirs {
                    let py = entry.path().join("python.exe");
                    if py.exists() {
                        let py_str = py.to_string_lossy().to_string();
                        if let Ok(r) = run_cmd(&py_str, &[&path_str], dir) {
                            return Ok(r);
                        }
                    }
                }
            }
        }
    }

    // macOS: check homebrew, system, and Conda paths
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        let mac_paths = [
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
            "/usr/bin/python3",
        ];
        let conda_paths = [
            format!("{}/anaconda3/bin/python", home),
            format!("{}/miniconda3/bin/python", home),
            format!("{}/miniforge3/bin/python", home),
            format!("{}/mambaforge/bin/python", home),
        ];

        for p in mac_paths {
            if std::path::Path::new(p).exists() {
                if let Ok(r) = run_cmd(p, &[&path_str], dir) {
                    return Ok(r);
                }
            }
        }
        for p in &conda_paths {
            if std::path::Path::new(p).exists() {
                if let Ok(r) = run_cmd(p, &[&path_str], dir) {
                    return Ok(r);
                }
            }
        }
    }

    Err("Python not found. Please install Python and ensure it is in your PATH.".to_string())
}

fn run_node(dir: &Path, code: &str, filename: &str) -> Result<(String, String, Option<i32>), String> {
    let path = write_file(dir, filename, code)?;
    let path_str = path.to_string_lossy().to_string();
    run_cmd("node", &[&path_str], dir)
}

fn run_c(dir: &Path, code: &str, filename: &str) -> Result<(String, String, Option<i32>), String> {
    let src = write_file(dir, filename, code)?;
    let src_str = src.to_string_lossy().to_string();
    let out_name = if cfg!(windows) { "a.exe" } else { "a.out" };
    let out_path = dir.join(out_name);
    let out_str = out_path.to_string_lossy().to_string();

    let (_, stderr, exit) = run_cmd("gcc", &[&src_str, "-o", &out_str, "-lm"], dir)?;
    if exit != Some(0) {
        return Ok(("".into(), format!("[Compilation Error]\n{}", stderr), exit));
    }
    run_cmd(&out_str, &[], dir)
}

fn run_cpp(dir: &Path, code: &str, filename: &str) -> Result<(String, String, Option<i32>), String> {
    let src = write_file(dir, filename, code)?;
    let src_str = src.to_string_lossy().to_string();
    let out_name = if cfg!(windows) { "a.exe" } else { "a.out" };
    let out_path = dir.join(out_name);
    let out_str = out_path.to_string_lossy().to_string();

    let (_, stderr, exit) = run_cmd("g++", &[&src_str, "-o", &out_str, "-std=c++17"], dir)?;
    if exit != Some(0) {
        return Ok(("".into(), format!("[Compilation Error]\n{}", stderr), exit));
    }
    run_cmd(&out_str, &[], dir)
}

fn run_java(dir: &Path, code: &str, filename: &str) -> Result<(String, String, Option<i32>), String> {
    // Write the file as-is (may include subdirectory path)
    let src = write_file(dir, filename, code)?;
    let src_str = src.to_string_lossy().to_string();
    let dir_str = dir.to_string_lossy().to_string();

    // Extract class name from filename (strip path and .java extension)
    let basename = filename.rsplit('/').next().unwrap_or(filename);
    let class_name = basename.trim_end_matches(".java");

    // Compile
    let (_, stderr, exit) = run_cmd("javac", &[&src_str], dir)?;
    if exit != Some(0) {
        return Ok(("".into(), format!("[Compilation Error]\n{}", stderr), exit));
    }
    run_cmd("java", &["-cp", &dir_str, class_name], dir)
}
