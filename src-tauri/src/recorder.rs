use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::path::{Path, PathBuf};

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
        let dir = self.output_dir.clone().ok_or("No output dir")?;
        self.segment_index += 1;

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        // The strategy decides the container extension (ffmpeg → .mp4,
        // macOS screencapture fallback → .mov), so only the base name is
        // fixed here.
        let base = format!("rec_{}_{:03}", timestamp, self.segment_index);

        match build_recording_command(&dir, &base) {
            Ok((child, strategy, path)) => {
                self.last_strategy = Some(strategy);
                self.process = Some(child);
                Ok(path.to_string_lossy().to_string())
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
        // Reap the child if the capture process crashed silently (driver hang,
        // OOM, etc.). Without this, `process.is_some()` keeps returning true
        // for a dead child — the IDE shows "● REC" but no frames are being
        // captured, and grading later finds 0-byte / truncated mp4 with no
        // warning.
        if let Some(child) = self.process.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.last_error = Some(format!(
                        "capture process exited unexpectedly during recording (code {:?})",
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

}

impl Drop for ScreenRecorder {
    fn drop(&mut self) { let _ = self.stop(); }
}

/// Graceful stop for a recorder child — gives FFmpeg / screencapture a chance
/// to finalize the mp4/mov moov atom. Without this the file is unplayable.
///
/// - Windows: send CTRL_BREAK_EVENT to the process group + write "q" to stdin
///   (FFmpeg honors either). Requires the child to have been spawned with
///   CREATE_NEW_PROCESS_GROUP.
/// - macOS: SIGINT (`kill -2`) — both ffmpeg and screencapture finalize on it.
///   SIGKILL/SIGTERM truncates the file.
/// - Linux: SIGINT.
/// - Fallback: bounded wait (15s), then SIGKILL. The wait polls every 100ms so
///   the common case (ffmpeg finalizes in well under a second) is not slowed;
///   the long ceiling exists because finalizing a multi-hour capture (moov
///   write / screencapture's stop-time file production) can take several
///   seconds, and killing it early destroys the whole recording.
fn graceful_stop_recorder(mut child: Child) {
    let pid = child.id();

    #[cfg(target_os = "windows")]
    {
        // CTRL_BREAK to the process group — FFmpeg's idiomatic stop signal.
        // The IDE is a GUI process with no console, so this is usually a no-op
        // (returns false); the stdin "q" below is the real graceful-stop path.
        let ctrl_ok = unsafe { generate_console_ctrl_event_break(pid) };
        if !ctrl_ok {
            eprintln!("[recorder] GenerateConsoleCtrlEvent(CTRL_BREAK) returned false (expected for a GUI process); relying on stdin 'q'");
        }
        // Belt: also write "q" via stdin pipe (FFmpeg polls stdin even when
        // it's not a tty), then close the pipe.
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(b"q\n");
            let _ = stdin.flush();
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = pid;
        // SIGINT (= screencapture/ffmpeg graceful stop). Use `kill -2 <pid>`
        // — `-pid` would target the process group which we did NOT set for
        // the recorder (it's a single-process tree).
        let _ = Command::new("kill")
            .args(["-2", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    }

    // Bounded wait for graceful exit, then SIGKILL.
    for _ in 0..150 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    // Graceful stop timed out — SIGKILL truncates the file (no moov atom), so
    // the segment may be unplayable. Log it so the fallback is not silent.
    eprintln!("[recorder] graceful stop timed out after 15s; SIGKILL — recording segment may be truncated/unplayable");
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
    // `status()` (not `spawn()`) — spawn-and-forget leaks a zombie child per
    // probe on Unix. -version exits immediately, so the wait is negligible.
    let mut probe = Command::new("ffmpeg");
    probe
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // CREATE_NO_WINDOW: without it this probe FLASHES a console window — and
    // it runs on every start_recording attempt, i.e. every 15s while the
    // recording retry loop is active. Highly visible during an exam.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        probe.creation_flags(0x08000000);
    }
    if probe.status().map(|s| s.success()).unwrap_or(false) {
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
        // A .app launched from Finder gets a minimal PATH that excludes
        // Homebrew, so probe the brew locations (Apple Silicon + Intel)
        // directly.
        for p in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"] {
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

    // winget Links dirs FIRST — stable symlink location that exists the moment
    // winget finishes, independent of this process's PATH. Critical right after
    // install-windows.ps1: it installs Gyan.FFmpeg with --scope MACHINE, but an
    // IDE launched from the Start Menu inherits Explorer's PRE-INSTALL PATH, so
    // the PATH probe fails — and the old discovery only scanned the USER-scope
    // WinGet dir, never the machine one. Result: first-launch "녹화 실패"
    // until reboot. (User scope: %LOCALAPPDATA%\Microsoft\WinGet\Links;
    // machine scope: %ProgramFiles%\WinGet\Links.)
    if let Ok(pf) = std::env::var("ProgramFiles") {
        let link = std::path::PathBuf::from(&pf).join("WinGet").join("Links").join("ffmpeg.exe");
        if link.exists() {
            paths.push(link.to_string_lossy().to_string());
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let link = std::path::PathBuf::from(&local)
            .join("Microsoft").join("WinGet").join("Links").join("ffmpeg.exe");
        if link.exists() {
            paths.push(link.to_string_lossy().to_string());
        }
    }

    if let Ok(user_path) = std::env::var("PATH") {
        for dir in user_path.split(';') {
            let candidate = std::path::PathBuf::from(dir).join("ffmpeg.exe");
            if candidate.exists() {
                paths.push(candidate.to_string_lossy().to_string());
            }
        }
    }

    // winget Packages trees — BOTH scopes (Links above normally suffices, but
    // some winget versions have shipped broken/missing symlinks).
    let mut package_roots: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(pf) = std::env::var("ProgramFiles") {
        package_roots.push(std::path::PathBuf::from(pf).join("WinGet").join("Packages"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        package_roots.push(
            std::path::PathBuf::from(local).join("Microsoft").join("WinGet").join("Packages"),
        );
    }
    for winget_dir in package_roots {
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

    // Common manual-install location.
    if std::path::Path::new("C:\\ffmpeg\\bin\\ffmpeg.exe").exists() {
        paths.push("C:\\ffmpeg\\bin\\ffmpeg.exe".to_string());
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

/// Spawn a capture command and verify it survives its startup window (device
/// open, permission check, encoder init all fail within ~1s). On early exit
/// the partial output file is removed (a failed strategy with `-y` may have
/// left a 0-byte / moov-less file that must never reach the submit collector)
/// and the stderr tail is returned for the error report. On success a drain
/// thread keeps the stderr pipe from back-pressuring the encoder.
fn verify_spawn(cmd: &mut Command, output_path: &str, label: &str) -> Result<(Child, String), String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{}: spawn failed: {}", label, e))?;

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
            let _ = std::fs::remove_file(output_path);
            Err(format!("{} (exit {:?}): {}", label, status.code(), tail.replace('\n', " | ")))
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
            Ok((child, label.to_string()))
        }
        Err(e) => Err(format!("{}: wait failed: {}", label, e)),
    }
}

/// Even-dimension half-scale. A plain `iw/2:ih/2` produces an ODD width on
/// 1366×768-class displays (683), which libx264+yuv420p rejects — the whole
/// strategy chain then fails on such laptops. trunc(x/4)*2 halves AND floors
/// to even.
const SCALE_HALF_EVEN: &str = "scale=trunc(iw/4)*2:trunc(ih/4)*2";

#[cfg(target_os = "windows")]
fn build_recording_command(dir: &Path, base: &str) -> Result<(Child, String, PathBuf), String> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW hides the FFmpeg console. CREATE_NEW_PROCESS_GROUP
    // lets us send CTRL_BREAK_EVENT to it for graceful shutdown — without
    // it, GenerateConsoleCtrlEvent would target the IDE itself.
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

    let ffmpeg = find_ffmpeg()?;
    let output_path = dir.join(format!("{}.mp4", base));
    let output_str = output_path.to_string_lossy().to_string();

    let scale_half = SCALE_HALF_EVEN.to_string();
    // `-flush_packets 1` on EVERY strategy: without it ffmpeg's 32KB AVIO
    // buffer holds encoded data in memory, and at this bitrate (~1-2KB/s on a
    // static IDE screen) the mp4 grew in ~25-second bursts — the 3s/12s start
    // probe and the stall watchdog both read a 0-byte file from a perfectly
    // healthy capture and false-alarmed on real machines. Flushing per packet
    // makes on-disk size track reality (a few tiny writes/sec — negligible).
    //
    // 5 fps (was 2): the user found 2fps too choppy to review. On a mostly
    // static screen x264 encodes unchanged frames as skip frames (bytes), so
    // the size cost is far below linear; ultrafast half-res keeps low-spec CPU
    // impact minimal.
    let strategies: Vec<(&str, Vec<String>)> = vec![
        // gdigrab default cursor capture is off — without -draw_mouse 1 the
        // student's pointer doesn't appear in the recording, which makes it
        // hard to correlate suspicious clicks with timestamps during grading.
        ("CPU/GDI (most compatible)",
         ["-y", "-f", "gdigrab", "-framerate", "5", "-draw_mouse", "1", "-i", "desktop",
          "-vf", scale_half.as_str(), "-c:v", "libx264", "-preset", "ultrafast", "-crf", "38",
          "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-flush_packets", "1", output_str.as_str()]
            .iter().map(|s| s.to_string()).collect()),
        ("CPU/DDA",
         ["-y", "-filter_complex", "ddagrab=framerate=5,scale=trunc(iw/4)*2:trunc(ih/4)*2",
          "-c:v", "libx264", "-preset", "ultrafast", "-crf", "36",
          "-pix_fmt", "yuv420p", "-flush_packets", "1", output_str.as_str()]
            .iter().map(|s| s.to_string()).collect()),
        ("NVIDIA NVENC",
         ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_nvenc",
          "-preset", "p1", "-qp", "32", "-pix_fmt", "yuv420p", "-flush_packets", "1", output_str.as_str()]
            .iter().map(|s| s.to_string()).collect()),
        ("Intel QuickSync",
         ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_qsv",
          "-preset", "veryfast", "-global_quality", "32", "-pix_fmt", "yuv420p", "-flush_packets", "1", output_str.as_str()]
            .iter().map(|s| s.to_string()).collect()),
        ("AMD AMF",
         ["-y", "-filter_complex", "ddagrab=framerate=5", "-c:v", "h264_amf",
          "-quality", "speed", "-qp_i", "32", "-qp_p", "32", "-pix_fmt", "yuv420p", "-flush_packets", "1", output_str.as_str()]
            .iter().map(|s| s.to_string()).collect()),
    ];

    let mut errors: Vec<String> = Vec::new();

    for (label, args) in &strategies {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(&arg_refs)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        match verify_spawn(&mut cmd, &output_str, label) {
            Ok((child, strategy)) => return Ok((child, strategy, output_path)),
            Err(e) => errors.push(e),
        }
    }

    Err(format!("All FFmpeg strategies failed:\n  - {}", errors.join("\n  - ")))
}

/// macOS Screen Recording permission (TCC). Preflight reports the CURRENT
/// in-process verdict; Request additionally registers the app in
/// System Settings > Privacy & Security > Screen Recording and shows the
/// system prompt the first time it is ever called. Both are used ONLY to fire
/// the first-launch prompt — the authoritative gate is `screen_capture_authorized_fresh`
/// (see its doc for why the in-process verdict is unreliable).
#[cfg(target_os = "macos")]
fn ensure_screen_capture_permission() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    unsafe {
        if CGPreflightScreenCaptureAccess() {
            return true;
        }
        CGRequestScreenCaptureAccess()
    }
}

/// If launched with MINT_TCC_PREFLIGHT set, print the CURRENT Screen Recording
/// verdict ("1"/"0") and exit — used by `screen_capture_authorized_fresh`.
/// Call this at the very top of the app entry point, BEFORE building any GUI.
#[cfg(target_os = "macos")]
pub fn run_tcc_preflight_and_exit_if_requested() {
    if std::env::var_os("MINT_TCC_PREFLIGHT").is_some() {
        use std::io::Write;
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGPreflightScreenCaptureAccess() -> bool;
        }
        let ok = unsafe { CGPreflightScreenCaptureAccess() };
        // Explicit write + flush: std::process::exit does NOT flush buffered
        // stdout, and a lost byte here would make screen_capture_authorized_fresh
        // read empty and never start recording on every Mac.
        let mut out = std::io::stdout();
        let _ = out.write_all(if ok { b"1" } else { b"0" });
        let _ = out.flush();
        std::process::exit(0);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn run_tcc_preflight_and_exit_if_requested() {}

/// Authoritative Screen Recording check via a FRESHLY SPAWNED child of our own
/// binary. This is essential and NOT redundant with CGPreflight:
///   1. The in-process CGPreflight verdict is cached for the process lifetime,
///      so after the student grants permission mid-session it stays stale
///      (false) until an app restart.
///   2. ffmpeg's avfoundation screen capture and screencapture do NOT error
///      when unauthorized — they silently record only the desktop wallpaper —
///      so capture liveness/file-growth (all our health checks) cannot detect a
///      missing grant. We MUST refuse to start capture until the grant is real.
/// A fresh child re-reads the current TCC verdict (same binary = same code
/// identity), giving the up-to-date answer that gates ffmpeg spawning.
#[cfg(target_os = "macos")]
fn screen_capture_authorized_fresh() -> bool {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    match Command::new(exe)
        .env("MINT_TCC_PREFLIGHT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim() == "1",
        Err(_) => false,
    }
}

/// Parse `ffmpeg -f avfoundation -list_devices true` stderr for avfoundation
/// screen-capture devices. Returns (index of "Capture screen 0", total screen
/// count). The index shifts with the number of cameras attached, so it cannot
/// be hardcoded. Lines look like:
///   [AVFoundation indev @ 0x...] [2] Capture screen 0
/// Takes the LAST "Capture screen 0" match, not the first: avfoundation lists
/// real cameras BEFORE synthesized screen devices, so a hostile virtual camera
/// named "Capture screen 0" (installed to hijack the recording) sorts before
/// the genuine screen entry — taking the last match defeats that.
#[cfg(target_os = "macos")]
fn avfoundation_screens(ffmpeg: &str) -> (Option<u32>, u32) {
    let out = match Command::new(ffmpeg)
        .args(["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return (None, 0),
    };
    // ffmpeg exits non-zero after listing (no real input) — that's expected;
    // the device table is on stderr either way.
    let text = String::from_utf8_lossy(&out.stderr);
    let mut screen0: Option<u32> = None;
    let mut screen_count = 0u32;
    for line in text.lines() {
        for (num, rest) in line.split('[').filter_map(|seg| seg.split_once(']')) {
            let rest = rest.trim();
            if rest.starts_with("Capture screen") {
                screen_count += 1;
                if rest == "Capture screen 0" {
                    if let Ok(n) = num.trim().parse::<u32>() {
                        screen0 = Some(n); // keep LAST match
                    }
                }
            }
        }
    }
    (screen0, screen_count)
}

#[cfg(target_os = "macos")]
fn build_recording_command(dir: &Path, base: &str) -> Result<(Child, String, PathBuf), String> {
    // Fire the first-launch TCC prompt (registers the app in System Settings +
    // shows the dialog the first time). Its in-process return is unreliable, so
    // it is NOT used to decide whether to proceed.
    let _ = ensure_screen_capture_permission();

    // AUTHORITATIVE gate: refuse to start capture until a FRESH-process TCC
    // verdict says Screen Recording is granted. This is critical — ffmpeg and
    // screencapture record only the wallpaper (no error, live process, growing
    // file) when unauthorized, so none of the downstream health checks can
    // catch a missing grant. Returning Err here drives the frontend's 15s retry
    // loop; once the student grants the permission, a subsequent attempt spawns
    // a capture that records for real.
    if !screen_capture_authorized_fresh() {
        return Err(
            "화면 기록 권한이 아직 허용되지 않았습니다. 시스템 설정 > 개인정보 보호 및 보안 > 화면 기록에서 'MINT Exam IDE'를 켜세요. 허용하면 자동으로 녹화가 시작됩니다.".to_string()
        );
    }

    let mut errors: Vec<String> = Vec::new();

    // Primary: ffmpeg avfoundation. install-mac.sh installs ffmpeg via brew,
    // and unlike `screencapture -v` it (a) writes the mp4 PROGRESSIVELY, so
    // the health probe / stall watchdog in lib.rs see real growth, (b) has no
    // interactive UI on any macOS version, and (c) finalizes quickly on
    // SIGINT. `screencapture -v` stays as the fallback for Macs without
    // ffmpeg.
    match find_ffmpeg() {
        Ok(ffmpeg) => {
            let output_path = dir.join(format!("{}.mp4", base));
            let output_str = output_path.to_string_lossy().to_string();
            let (screen0, screen_count) = avfoundation_screens(&ffmpeg);
            let input = screen0
                .map(|n| n.to_string())
                .unwrap_or_else(|| "Capture screen 0".to_string());
            // avfoundation captures ONE display; note extra displays in the
            // strategy string (logged into the recording_start event) so a
            // grader knows a second monitor existed but wasn't recorded.
            let multi = if screen_count > 1 {
                format!(" [주의: 디스플레이 {}개 중 주 화면만 녹화됨]", screen_count)
            } else {
                String::new()
            };

            // Some ffmpeg/macOS combinations reject particular capture rates
            // ("selected framerate is not supported") — walk a small ladder,
            // preferring 5fps (2fps reviewed too choppy). -flush_packets 1:
            // see the Windows strategy comment — without it the AVIO buffer
            // makes health probes read a 0-byte file from a healthy capture.
            for fps in ["5", "15", "2"] {
                let label = format!("macOS avfoundation {}fps{}", fps, multi);
                let mut cmd = Command::new(&ffmpeg);
                cmd.args([
                    "-y", "-f", "avfoundation",
                    "-framerate", fps,
                    "-capture_cursor", "1",
                    "-i", input.as_str(),
                    "-vf", SCALE_HALF_EVEN,
                    "-c:v", "libx264", "-preset", "ultrafast", "-crf", "38",
                    "-pix_fmt", "yuv420p",
                    "-flush_packets", "1",
                    output_str.as_str(),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
                match verify_spawn(&mut cmd, &output_str, &label) {
                    Ok((child, strategy)) => return Ok((child, strategy, output_path)),
                    Err(e) => errors.push(e),
                }
            }
        }
        Err(e) => errors.push(e),
    }

    // Fallback: Apple's screencapture. -v video, -C cursor, -x mute the
    // start/stop sounds. NOTE: it may produce the .mov only at stop time, so
    // size-based health checks don't apply to this strategy (lib.rs keys off
    // the strategy label containing "screencapture").
    {
        let output_path = dir.join(format!("{}.mov", base));
        let output_str = output_path.to_string_lossy().to_string();
        let mut cmd = Command::new("screencapture");
        cmd.args(["-v", "-C", "-x", output_str.as_str()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match verify_spawn(&mut cmd, &output_str, "macOS screencapture") {
            Ok((child, strategy)) => return Ok((child, strategy, output_path)),
            Err(e) => errors.push(e),
        }
    }

    // Permission was already confirmed granted above, so a failure here is a
    // real capture/encoder problem, not a TCC denial.
    Err(format!("모든 녹화 방식이 실패했습니다: {}", errors.join(" | ")))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn build_recording_command(dir: &Path, base: &str) -> Result<(Child, String, PathBuf), String> {
    let ffmpeg = find_ffmpeg()?;
    let output_path = dir.join(format!("{}.mp4", base));
    let output_str = output_path.to_string_lossy().to_string();
    let mut cmd = Command::new(&ffmpeg);
    cmd.args(["-y", "-f", "x11grab", "-framerate", "5", "-i", ":0.0",
              "-vf", SCALE_HALF_EVEN,
              "-c:v", "libx264", "-preset", "ultrafast", "-crf", "36",
              "-pix_fmt", "yuv420p", "-flush_packets", "1", output_str.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let (child, strategy) = verify_spawn(&mut cmd, &output_str, "Linux x11grab")?;
    Ok((child, strategy, output_path))
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

    // Fill up to 1024 bytes (or EOF) — a single `read` may legally return short,
    // which would leave a tail un-obfuscated. MUST stay symmetric with the
    // grader's deobfuscate_video (same fill logic) so both touch the identical
    // byte range and the XOR round-trips.
    let mut buf = [0u8; 1024];
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(e.to_string()),
        }
    }
    for i in 0..n {
        buf[i] ^= key[i % key.len()];
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())
}
