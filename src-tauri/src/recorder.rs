use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::path::PathBuf;

pub struct ScreenRecorder {
    process: Option<Child>,
    output_path: Option<PathBuf>,
}

impl ScreenRecorder {
    pub fn new() -> Self {
        Self { process: None, output_path: None }
    }

    pub fn start(&mut self, output_dir: &str) -> Result<String, String> {
        if self.process.is_some() {
            return Err("Recording already in progress".to_string());
        }

        let dir = PathBuf::from(output_dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create dir: {}", e))?;

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let ext = if cfg!(target_os = "macos") { "mov" } else { "mp4" };
        let filename = format!("exam_recording_{}.{}", timestamp, ext);
        let output_path = dir.join(&filename);
        let output_str = output_path.to_string_lossy().to_string();

        // macOS uses built-in screencapture (no FFmpeg needed)
        // Windows/Linux use FFmpeg
        let child = if cfg!(target_os = "macos") {
            build_ffmpeg_command("", &output_str)?
        } else {
            let ffmpeg = find_ffmpeg()?;
            build_ffmpeg_command(&ffmpeg, &output_str)?
        };

        self.process = Some(child);
        self.output_path = Some(output_path.clone());
        Ok(output_path.to_string_lossy().to_string())
    }

    pub fn stop(&mut self) -> Result<String, String> {
        if let Some(mut child) = self.process.take() {
            // macOS screencapture: SIGTERM to stop gracefully
            // Windows FFmpeg: send 'q' to stdin
            if cfg!(target_os = "macos") {
                let _ = child.kill();
            } else {
                if let Some(ref mut stdin) = child.stdin {
                    use std::io::Write;
                    let _ = stdin.write_all(b"q");
                    let _ = stdin.flush();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));

            match child.wait() {
                Ok(_) => {}
                Err(_) => { let _ = child.kill(); }
            }

            if let Some(path) = self.output_path.take() {
                return Ok(path.to_string_lossy().to_string());
            }
        }
        Err("No recording in progress".to_string())
    }

    pub fn is_recording(&self) -> bool {
        self.process.is_some()
    }

    pub fn recording_path(&self) -> Option<String> {
        self.output_path.as_ref().map(|p| p.to_string_lossy().to_string())
    }
}

impl Drop for ScreenRecorder {
    fn drop(&mut self) { let _ = self.stop(); }
}

/// Find FFmpeg binary, checking multiple common locations.
fn find_ffmpeg() -> Result<String, String> {
    // 1. Check PATH
    if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok() {
        return Ok("ffmpeg".to_string());
    }

    // 2. Search common Windows locations
    #[cfg(target_os = "windows")]
    {
        let candidates = discover_ffmpeg_windows();
        for path in candidates {
            if std::path::Path::new(&path).exists() {
                return Ok(path);
            }
        }
    }

    // 3. macOS / Linux - likely in PATH already
    #[cfg(not(target_os = "windows"))]
    {
        for p in ["/usr/local/bin/ffmpeg", "/opt/homebrew/bin/ffmpeg"] {
            if std::path::Path::new(p).exists() {
                return Ok(p.to_string());
            }
        }
    }

    Err("FFmpeg not found. Please install FFmpeg and ensure it is in your PATH.".to_string())
}

#[cfg(target_os = "windows")]
fn discover_ffmpeg_windows() -> Vec<String> {
    let mut paths = Vec::new();

    // Check user PATH entries (winget installs here)
    if let Ok(user_path) = std::env::var("PATH") {
        for dir in user_path.split(';') {
            let candidate = std::path::PathBuf::from(dir).join("ffmpeg.exe");
            if candidate.exists() {
                paths.push(candidate.to_string_lossy().to_string());
            }
        }
    }

    // Scan WinGet packages dir
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let winget_dir = std::path::PathBuf::from(&local)
            .join("Microsoft").join("WinGet").join("Packages");
        if let Ok(entries) = std::fs::read_dir(&winget_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name.contains("ffmpeg") {
                    // Recurse into bin/ subdirectory
                    let bin = entry.path().join("ffmpeg-8.1-full_build").join("bin").join("ffmpeg.exe");
                    if bin.exists() {
                        paths.push(bin.to_string_lossy().to_string());
                        continue;
                    }
                    // Generic search
                    if let Ok(sub) = glob_ffmpeg_in_dir(&entry.path()) {
                        paths.push(sub);
                    }
                }
            }
        }
    }

    // Common manual install locations
    for base in ["C:\\ffmpeg\\bin\\ffmpeg.exe", "C:\\Program Files\\ffmpeg\\bin\\ffmpeg.exe"] {
        paths.push(base.to_string());
    }

    paths
}

#[cfg(target_os = "windows")]
fn glob_ffmpeg_in_dir(dir: &std::path::Path) -> Result<String, String> {
    for entry in walkdir_simple(dir, 3) {
        if entry.ends_with("ffmpeg.exe") {
            return Ok(entry);
        }
    }
    Err("not found".to_string())
}

#[cfg(target_os = "windows")]
fn walkdir_simple(dir: &std::path::Path, depth: u32) -> Vec<String> {
    let mut results = Vec::new();
    if depth == 0 { return results; }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                results.push(path.to_string_lossy().to_string());
            } else if path.is_dir() {
                results.extend(walkdir_simple(&path, depth - 1));
            }
        }
    }
    results
}

#[cfg(target_os = "windows")]
fn build_ffmpeg_command(ffmpeg: &str, output_path: &str) -> Result<Child, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // Strategy: try GPU-accelerated first, fall back to CPU
    // 1. ddagrab (DXGI GPU capture) + h264_nvenc (NVIDIA GPU encode) — near zero CPU
    // 2. ddagrab + h264_qsv (Intel QuickSync) — near zero CPU
    // 3. ddagrab + h264_amf (AMD AMF) — near zero CPU
    // 4. ddagrab + libx264 ultrafast — GPU capture, CPU encode
    // 5. gdigrab + libx264 ultrafast — CPU everything (last resort)

    let strategies: Vec<Vec<String>> = vec![
        // ddagrab + NVIDIA NVENC
        vec![
            "-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_nvenc",
            "-preset", "p1", "-qp", "32", "-pix_fmt", "yuv420p", output_path,
        ].into_iter().map(String::from).collect(),
        // ddagrab + Intel QuickSync
        vec![
            "-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_qsv",
            "-preset", "veryfast", "-global_quality", "32", "-pix_fmt", "yuv420p", output_path,
        ].into_iter().map(String::from).collect(),
        // ddagrab + AMD AMF
        vec![
            "-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_amf",
            "-quality", "speed", "-qp_i", "32", "-qp_p", "32", "-pix_fmt", "yuv420p", output_path,
        ].into_iter().map(String::from).collect(),
        // ddagrab + CPU (libx264) — 2fps, half resolution for low CPU usage
        vec![
            "-y", "-filter_complex", "ddagrab=framerate=2,scale=iw/2:ih/2",
            "-c:v", "libx264", "-preset", "ultrafast", "-crf", "36",
            "-pix_fmt", "yuv420p", output_path,
        ].into_iter().map(String::from).collect(),
        // gdigrab + CPU — ultra-low spec fallback: 2fps, half res, max compression
        vec![
            "-y", "-f", "gdigrab", "-framerate", "2", "-i", "desktop",
            "-vf", "scale=iw/2:ih/2",
            "-c:v", "libx264", "-preset", "ultrafast", "-crf", "38",
            "-pix_fmt", "yuv420p", "-movflags", "+faststart", output_path,
        ].into_iter().map(String::from).collect(),
    ];

    for (i, args) in strategies.iter().enumerate() {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match Command::new(ffmpeg)
            .args(&arg_refs)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            Ok(mut child) => {
                // Give it a moment to see if it crashes immediately
                std::thread::sleep(std::time::Duration::from_millis(500));
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        // Process exited immediately — this strategy failed, try next
                        continue;
                    }
                    Ok(None) => {
                        // Still running — success!
                        eprintln!("Recording strategy #{} succeeded", i + 1);
                        return Ok(child);
                    }
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        }
    }

    Err("All recording strategies failed. Is FFmpeg installed?".to_string())
}

#[cfg(target_os = "macos")]
fn build_ffmpeg_command(_ffmpeg: &str, output_path: &str) -> Result<Child, String> {
    // Use macOS built-in screencapture — no FFmpeg needed.
    // -v = video mode, -C = capture cursor
    // screencapture runs until killed (we send SIGTERM on stop)
    Command::new("screencapture")
        .args(["-v", "-C", output_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start screen recording: {}", e))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn build_ffmpeg_command(ffmpeg: &str, output_path: &str) -> Result<Child, String> {
    Command::new(ffmpeg)
        .args([
            "-y", "-f", "x11grab", "-framerate", "5",
            "-i", ":0.0",
            "-c:v", "libx264", "-preset", "ultrafast", "-crf", "32",
            "-pix_fmt", "yuv420p", "-movflags", "+faststart",
            output_path,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start FFmpeg: {}", e))
}

pub type RecorderState = Mutex<ScreenRecorder>;
