use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::path::PathBuf;

pub struct ScreenRecorder {
    process: Option<Child>,
    output_dir: Option<PathBuf>,
    segment_index: u32,
}

impl ScreenRecorder {
    pub fn new() -> Self {
        Self { process: None, output_dir: None, segment_index: 0 }
    }

    pub fn start(&mut self, output_dir: &str) -> Result<String, String> {
        if self.process.is_some() {
            return Err("Recording already in progress".to_string());
        }

        let dir = PathBuf::from(output_dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create dir: {}", e))?;
        self.output_dir = Some(dir.clone());
        self.segment_index = 0;

        self.start_segment()
    }

    /// Start a new recording segment
    fn start_segment(&mut self) -> Result<String, String> {
        let dir = self.output_dir.as_ref().ok_or("No output dir")?;
        self.segment_index += 1;

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let ext = if cfg!(target_os = "macos") { "mov" } else { "mp4" };
        let filename = format!("rec_{}_{:03}.{}", timestamp, self.segment_index, ext);
        let output_path = dir.join(&filename);
        let output_str = output_path.to_string_lossy().to_string();

        let child = if cfg!(target_os = "macos") {
            build_recording_command("", &output_str)?
        } else {
            let ffmpeg = find_ffmpeg()?;
            build_recording_command(&ffmpeg, &output_str)?
        };

        self.process = Some(child);
        Ok(output_path.to_string_lossy().to_string())
    }

    /// Auto-segment: call periodically to split into ~15 min chunks
    pub fn maybe_rotate(&mut self) -> Option<String> {
        // Only rotate if recording is active
        if self.process.is_none() { return None; }

        // Stop current segment
        if let Some(mut child) = self.process.take() {
            if cfg!(target_os = "macos") {
                let _ = child.kill();
            } else {
                if let Some(ref mut stdin) = child.stdin {
                    use std::io::Write;
                    let _ = stdin.write_all(b"q");
                    let _ = stdin.flush();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = child.wait();
        }

        // Start new segment
        self.start_segment().ok()
    }

    pub fn stop(&mut self) -> Result<String, String> {
        if let Some(mut child) = self.process.take() {
            if cfg!(target_os = "macos") {
                let _ = child.kill();
            } else {
                if let Some(ref mut stdin) = child.stdin {
                    use std::io::Write;
                    let _ = stdin.write_all(b"q");
                    let _ = stdin.flush();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = child.wait();
        }

        self.output_dir
            .as_ref()
            .map(|d| d.to_string_lossy().to_string())
            .ok_or("No recording dir".to_string())
    }

    pub fn is_recording(&self) -> bool {
        self.process.is_some()
    }

    pub fn recording_dir(&self) -> Option<String> {
        self.output_dir.as_ref().map(|p| p.to_string_lossy().to_string())
    }
}

impl Drop for ScreenRecorder {
    fn drop(&mut self) { let _ = self.stop(); }
}

fn find_ffmpeg() -> Result<String, String> {
    if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok() {
        return Ok("ffmpeg".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let candidates = discover_ffmpeg_windows();
        for path in candidates {
            if std::path::Path::new(&path).exists() {
                return Ok(path);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        for p in ["/usr/local/bin/ffmpeg", "/opt/homebrew/bin/ffmpeg"] {
            if std::path::Path::new(p).exists() {
                return Ok(p.to_string());
            }
        }
    }

    Err("FFmpeg not found".to_string())
}

#[cfg(target_os = "windows")]
fn discover_ffmpeg_windows() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(user_path) = std::env::var("PATH") {
        for dir in user_path.split(';') {
            let candidate = std::path::PathBuf::from(dir).join("ffmpeg.exe");
            if candidate.exists() {
                paths.push(candidate.to_string_lossy().to_string());
            }
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let winget_dir = std::path::PathBuf::from(&local)
            .join("Microsoft").join("WinGet").join("Packages");
        if let Ok(entries) = std::fs::read_dir(&winget_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name.contains("ffmpeg") {
                    if let Ok(sub) = glob_ffmpeg_in_dir(&entry.path()) {
                        paths.push(sub);
                    }
                }
            }
        }
    }
    paths
}

#[cfg(target_os = "windows")]
fn glob_ffmpeg_in_dir(dir: &std::path::Path) -> Result<String, String> {
    for entry in walkdir_simple(dir, 3) {
        if entry.ends_with("ffmpeg.exe") { return Ok(entry); }
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
            if path.is_file() { results.push(path.to_string_lossy().to_string()); }
            else if path.is_dir() { results.extend(walkdir_simple(&path, depth - 1)); }
        }
    }
    results
}

#[cfg(target_os = "windows")]
fn build_recording_command(ffmpeg: &str, output_path: &str) -> Result<Child, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // Segmented recording with -t 900 (15 min) — FFmpeg auto-stops, we restart
    // Try GPU first, fall back to CPU
    let strategies: Vec<Vec<String>> = vec![
        // NVIDIA NVENC
        ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_nvenc",
         "-preset", "p1", "-qp", "32", "-pix_fmt", "yuv420p", "-t", "900", output_path]
            .iter().map(|s| s.to_string()).collect(),
        // Intel QuickSync
        ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_qsv",
         "-preset", "veryfast", "-global_quality", "32", "-pix_fmt", "yuv420p", "-t", "900", output_path]
            .iter().map(|s| s.to_string()).collect(),
        // AMD AMF
        ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_amf",
         "-quality", "speed", "-qp_i", "32", "-qp_p", "32", "-pix_fmt", "yuv420p", "-t", "900", output_path]
            .iter().map(|s| s.to_string()).collect(),
        // CPU: 2fps, half res
        ["-y", "-filter_complex", "ddagrab=framerate=2,scale=iw/2:ih/2",
         "-c:v", "libx264", "-preset", "ultrafast", "-crf", "36",
         "-pix_fmt", "yuv420p", "-t", "900", output_path]
            .iter().map(|s| s.to_string()).collect(),
        // Legacy GDI fallback
        ["-y", "-f", "gdigrab", "-framerate", "2", "-i", "desktop",
         "-vf", "scale=iw/2:ih/2", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "38",
         "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-t", "900", output_path]
            .iter().map(|s| s.to_string()).collect(),
    ];

    for args in &strategies {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        if let Ok(mut child) = Command::new(ffmpeg)
            .args(&arg_refs)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            std::thread::sleep(std::time::Duration::from_millis(500));
            match child.try_wait() {
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(child),
                Err(_) => continue,
            }
        }
    }

    Err("All recording strategies failed".to_string())
}

#[cfg(target_os = "macos")]
fn build_recording_command(_ffmpeg: &str, output_path: &str) -> Result<Child, String> {
    Command::new("screencapture")
        .args(["-v", "-C", output_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start screen recording: {}", e))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn build_recording_command(ffmpeg: &str, output_path: &str) -> Result<Child, String> {
    Command::new(ffmpeg)
        .args(["-y", "-f", "x11grab", "-framerate", "2", "-i", ":0.0",
               "-vf", "scale=iw/2:ih/2",
               "-c:v", "libx264", "-preset", "ultrafast", "-crf", "36",
               "-pix_fmt", "yuv420p", "-t", "900", output_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start FFmpeg: {}", e))
}

pub type RecorderState = Mutex<ScreenRecorder>;

/// Obfuscate video file headers so students can't play them.
/// Fast: only modifies first 1KB. Grader reverses with same function.
pub fn obfuscate_video(path: &std::path::Path, key: &[u8]) -> Result<(), String> {
    let mut data = std::fs::read(path).map_err(|e| e.to_string())?;
    let len = data.len().min(1024);
    for i in 0..len {
        data[i] ^= key[i % key.len()];
    }
    std::fs::write(path, &data).map_err(|e| e.to_string())
}
