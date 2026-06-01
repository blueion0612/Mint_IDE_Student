use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::path::PathBuf;

pub struct ScreenRecorder {
    process: Option<Child>,
    output_dir: Option<PathBuf>,
    segment_index: u32,
    last_error: Option<String>,
    last_strategy: Option<String>,
}

impl ScreenRecorder {
    pub fn new() -> Self {
        Self {
            process: None,
            output_dir: None,
            segment_index: 0,
            last_error: None,
            last_strategy: None,
        }
    }

    pub fn start(&mut self, output_dir: &str) -> Result<String, String> {
        if self.process.is_some() {
            return Err("Recording already in progress".to_string());
        }

        let dir = PathBuf::from(output_dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create dir: {}", e))?;
        self.output_dir = Some(dir.clone());
        self.segment_index = 0;
        self.last_error = None;
        self.last_strategy = None;

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

        let result = if cfg!(target_os = "macos") {
            build_recording_command("", &output_str)
        } else {
            match find_ffmpeg() {
                Ok(ffmpeg) => build_recording_command(&ffmpeg, &output_str),
                Err(e) => Err(e),
            }
        };

        match result {
            Ok((child, strategy)) => {
                self.last_strategy = Some(strategy);
                self.process = Some(child);
                Ok(output_path.to_string_lossy().to_string())
            }
            Err(e) => {
                self.last_error = Some(e.clone());
                Err(e)
            }
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }

    pub fn last_strategy(&self) -> Option<String> {
        self.last_strategy.clone()
    }

    /// Auto-segment: call periodically to split into ~15 min chunks
    pub fn maybe_rotate(&mut self) -> Option<String> {
        if self.process.is_none() { return None; }

        if let Some(child) = self.process.take() {
            graceful_stop_recorder(child);
        }

        self.start_segment().ok()
    }

    pub fn stop(&mut self) -> Result<String, String> {
        if let Some(child) = self.process.take() {
            graceful_stop_recorder(child);
        }

        self.output_dir
            .as_ref()
            .map(|d| d.to_string_lossy().to_string())
            .ok_or("No recording dir".to_string())
    }

    pub fn is_recording(&mut self) -> bool {
        // Reap the child if FFmpeg crashed silently (driver hang, OOM, etc.).
        // Without this, `process.is_some()` keeps returning true for a dead
        // child — the IDE shows "● REC" but no frames are being captured,
        // and grading later finds 0-byte / truncated mp4 with no warning.
        if let Some(child) = self.process.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.last_error = Some(format!(
                        "FFmpeg exited unexpectedly during recording (code {:?})",
                        status.code()
                    ));
                    self.process = None;
                    return false;
                }
                Ok(None) => return true, // still running
                Err(e) => {
                    // Couldn't query the owned child (should not happen). Record
                    // it but keep reporting "recording" so a live capture is not
                    // spuriously declared stopped.
                    self.last_error = Some(format!("try_wait failed: {}", e));
                    return true;
                }
            }
        }
        false
    }

    pub fn recording_dir(&self) -> Option<String> {
        self.output_dir.as_ref().map(|p| p.to_string_lossy().to_string())
    }
}

impl Drop for ScreenRecorder {
    fn drop(&mut self) { let _ = self.stop(); }
}

/// Graceful stop for a recorder child — gives FFmpeg / screencapture a chance
/// to finalize the mp4 moov atom. Without this the file is unplayable.
///
/// - Windows: send CTRL_BREAK_EVENT to the process group + write "q" to stdin
///   (FFmpeg honors either). Requires the child to have been spawned with
///   CREATE_NEW_PROCESS_GROUP.
/// - macOS: SIGINT to screencapture (`kill -2`). SIGKILL/SIGTERM truncates mp4.
/// - Linux: SIGINT.
/// - Fallback: 2s wait, then SIGKILL.
fn graceful_stop_recorder(mut child: Child) {
    let pid = child.id();

    #[cfg(target_os = "windows")]
    {
        // CTRL_BREAK to the process group — FFmpeg's idiomatic stop signal.
        // The IDE is a GUI process with no console, so this is usually a no-op
        // (returns false); the stdin "q" below is the real graceful-stop path.
        // Capture/log the result so the dead strand is visible.
        let ctrl_ok = unsafe { generate_console_ctrl_event_break(pid) };
        if !ctrl_ok {
            eprintln!("[recorder] GenerateConsoleCtrlEvent(CTRL_BREAK) returned false (expected for a GUI process); relying on stdin 'q'");
        }
        // Belt: also write "q" via stdin pipe (FFmpeg polls stdin even when
        // it's not a tty).
        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            let _ = stdin.write_all(b"q\n");
            let _ = stdin.flush();
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // SIGINT (= screencapture/ffmpeg graceful stop). Use `kill -2 <pid>`
        // — `-pid` would target the process group which we did NOT set for
        // the recorder (it's a single-process tree).
        let _ = Command::new("kill")
            .args(["-2", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    }

    // Wait up to 2s for graceful exit, then SIGKILL.
    for _ in 0..20 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    // Graceful stop timed out — SIGKILL truncates the mp4 (no moov atom), so the
    // segment may be unplayable. Log it so the fallback is not silent.
    eprintln!("[recorder] graceful stop timed out after 2s; SIGKILL — recording segment may be truncated/unplayable");
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "windows")]
unsafe fn generate_console_ctrl_event_break(process_group_id: u32) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    }
    const CTRL_BREAK_EVENT: u32 = 1;
    GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id) != 0
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
fn build_recording_command(ffmpeg: &str, output_path: &str) -> Result<(Child, String), String> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW hides the FFmpeg console. CREATE_NEW_PROCESS_GROUP
    // lets us send CTRL_BREAK_EVENT to it for graceful shutdown — without
    // it, GenerateConsoleCtrlEvent would target the IDE itself.
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

    let strategies: Vec<(&str, Vec<String>)> = vec![
        // gdigrab default cursor capture is off — without -draw_mouse 1 the
        // student's pointer doesn't appear in the recording, which makes it
        // hard to correlate suspicious clicks with timestamps during grading.
        ("CPU/GDI (most compatible)",
         ["-y", "-f", "gdigrab", "-framerate", "2", "-draw_mouse", "1", "-i", "desktop",
          "-vf", "scale=iw/2:ih/2", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "38",
          "-pix_fmt", "yuv420p", "-movflags", "+faststart", output_path]
            .iter().map(|s| s.to_string()).collect()),
        ("CPU/DDA",
         ["-y", "-filter_complex", "ddagrab=framerate=2,scale=iw/2:ih/2",
          "-c:v", "libx264", "-preset", "ultrafast", "-crf", "36",
          "-pix_fmt", "yuv420p", output_path]
            .iter().map(|s| s.to_string()).collect()),
        ("NVIDIA NVENC",
         ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_nvenc",
          "-preset", "p1", "-qp", "32", "-pix_fmt", "yuv420p", output_path]
            .iter().map(|s| s.to_string()).collect()),
        ("Intel QuickSync",
         ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_qsv",
          "-preset", "veryfast", "-global_quality", "32", "-pix_fmt", "yuv420p", output_path]
            .iter().map(|s| s.to_string()).collect()),
        ("AMD AMF",
         ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_amf",
          "-quality", "speed", "-qp_i", "32", "-qp_p", "32", "-pix_fmt", "yuv420p", output_path]
            .iter().map(|s| s.to_string()).collect()),
    ];

    let mut errors: Vec<String> = Vec::new();

    for (label, args) in &strategies {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let spawn = Command::new(ffmpeg)
            .args(&arg_refs)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn();

        let mut child = match spawn {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{}: spawn failed: {}", label, e));
                continue;
            }
        };

        std::thread::sleep(std::time::Duration::from_millis(1500));
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr_text = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut stderr_text);
                }
                // Char-boundary-safe tail: ffmpeg echoes the output path, which
                // on a Korean-locale account contains 3-byte Hangul; a raw byte
                // slice at len-400 could split a UTF-8 sequence and PANIC. That
                // panic runs while the recorder MutexGuard is held → poisons it
                // → recording dead for the whole session.
                let tail = if stderr_text.len() > 400 {
                    let mut idx = stderr_text.len() - 400;
                    while idx < stderr_text.len() && !stderr_text.is_char_boundary(idx) {
                        idx += 1;
                    }
                    stderr_text[idx..].to_string()
                } else {
                    stderr_text
                };
                // With -y this failed strategy may have left a 0-byte / moov-less
                // partial file at output_path; remove it so the submit collector
                // never bundles a truncated recording (next strategy re-creates).
                let _ = std::fs::remove_file(output_path);
                errors.push(format!("{} (exit {:?}): {}", label, status.code(), tail.replace('\n', " | ")));
                continue;
            }
            Ok(None) => {
                if let Some(stderr) = child.stderr.take() {
                    std::thread::spawn(move || {
                        use std::io::Read;
                        let mut buf = [0u8; 4096];
                        let mut reader = stderr;
                        while reader.read(&mut buf).unwrap_or(0) > 0 {}
                    });
                }
                return Ok((child, label.to_string()));
            }
            Err(e) => {
                errors.push(format!("{}: wait failed: {}", label, e));
                continue;
            }
        }
    }

    Err(format!("All FFmpeg strategies failed:\n  - {}", errors.join("\n  - ")))
}

#[cfg(target_os = "macos")]
fn build_recording_command(_ffmpeg: &str, output_path: &str) -> Result<(Child, String), String> {
    let mut child = Command::new("screencapture")
        .args(["-v", "-C", output_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start screen recording: {}", e))?;

    std::thread::sleep(std::time::Duration::from_millis(800));
    if let Ok(Some(status)) = child.try_wait() {
        let mut err_text = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut err_text);
        }
        return Err(format!(
            "screencapture exited immediately (code {:?}): {}",
            status.code(),
            err_text.trim()
        ));
    }

    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            let mut reader = stderr;
            while reader.read(&mut buf).unwrap_or(0) > 0 {}
        });
    }

    Ok((child, "macOS screencapture".to_string()))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn build_recording_command(ffmpeg: &str, output_path: &str) -> Result<(Child, String), String> {
    let child = Command::new(ffmpeg)
        .args(["-y", "-f", "x11grab", "-framerate", "2", "-i", ":0.0",
               "-vf", "scale=iw/2:ih/2",
               "-c:v", "libx264", "-preset", "ultrafast", "-crf", "36",
               "-pix_fmt", "yuv420p", output_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start FFmpeg: {}", e))?;
    Ok((child, "Linux x11grab".to_string()))
}

pub type RecorderState = Mutex<ScreenRecorder>;

/// Obfuscate video file headers so students can't play them.
/// Reads ONLY the first 1KB into RAM, XORs in place, writes back the same
/// 1KB. The naive `fs::read` + `fs::write` loads the entire file into RAM —
/// a 1~2 GB exam recording on an 8 GB student PC would OOM, leaving a
/// plaintext mp4 on Desktop (security regression + data loss).
pub fn obfuscate_video(path: &std::path::Path, key: &[u8]) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    if key.is_empty() {
        return Err("empty obfuscation key".to_string());
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open {} failed: {}", path.display(), e))?;

    let mut buf = [0u8; 1024];
    let n = file.read(&mut buf).map_err(|e| e.to_string())?;
    for i in 0..n {
        buf[i] ^= key[i % key.len()];
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())
}
