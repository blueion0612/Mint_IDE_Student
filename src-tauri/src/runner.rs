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
    let mut command = Command::new(cmd);
    command.args(args).current_dir(cwd);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = command.output()
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
    if let Some(py) = python_path {
        run_cmd(py, &[&path_str], dir)
    } else {
        run_cmd("python3", &[&path_str], dir)
            .or_else(|_| run_cmd("python", &[&path_str], dir))
    }
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
