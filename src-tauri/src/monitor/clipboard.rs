use super::log::{ActivityEvent, LogHandle};
use arboard::Clipboard;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Monitors clipboard changes by polling at regular intervals.
/// Cross-platform: uses `arboard` crate which works on Windows, macOS, and Linux.
pub fn start_clipboard_monitor(log: LogHandle, app_handle: AppHandle) {
    thread::spawn(move || {
        let mut clipboard = match Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                // This thread is the ONLY clipboard monitoring there is — a
                // silent return meant copy/paste-source evidence was missing
                // for the entire exam with nothing but an invisible stderr
                // line (GUI build has no console). Surface it like the macOS
                // Automation-denied case so the student can report it.
                let event = ActivityEvent::new(
                    "monitor_health_fail",
                    &format!(
                        "클립보드 모니터링을 시작하지 못했습니다 ({}). 복사/붙여넣기 출처가 기록되지 않습니다 — 감독관에게 알리세요.",
                        e
                    ),
                    None,
                    None,
                );
                log.add_event(event.clone());
                let _ = app_handle.emit("activity-event", &event);
                return;
            }
        };

        let mut last_content = clipboard.get_text().unwrap_or_default();

        loop {
            thread::sleep(Duration::from_millis(1000));

            let current = match clipboard.get_text() {
                Ok(text) => text,
                Err(_) => continue,
            };

            if current != last_content && !current.is_empty() {
                // Truncate on CHARACTER boundaries, not bytes. A byte slice at
                // [..200] panics when byte 200 is mid-UTF-8 (e.g. 67 Korean
                // chars = 201 bytes) — that panic kills this monitor thread for
                // the whole session (no catch_unwind), permanently disabling
                // clipboard monitoring. A student could trip it deliberately.
                let char_count = current.chars().count() as u32;
                let truncated = if char_count > 200 {
                    let head: String = current.chars().take(200).collect();
                    format!("{}...", head)
                } else {
                    current.clone()
                };

                let (source, window_title) = detect_clipboard_source();

                let event_type = if source == "self" {
                    "clipboard_internal"
                } else {
                    "clipboard_external"
                };

                let title_part = if window_title.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", window_title)
                };
                let detail = format!(
                    "[Source: {}{}] {}",
                    source,
                    title_part,
                    truncated.replace('\n', "\\n").replace('\r', "")
                );

                let event = ActivityEvent::new(event_type, &detail, Some(char_count), None);
                log.add_event(event.clone());
                let _ = app_handle.emit("activity-event", &event);

                last_content = current;
            }
        }
    });
}

#[cfg(target_os = "windows")]
fn detect_clipboard_source() -> (String, String) {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "user32")]
    extern "system" {
        fn GetClipboardOwner() -> isize;
        fn GetForegroundWindow() -> isize;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowTextLengthW(hwnd: isize) -> i32;
        fn GetWindowTextW(hwnd: isize, text: *mut u16, max: i32) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn QueryFullProcessImageNameW(
            process: isize,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    fn read_title(hwnd: isize) -> String {
        unsafe {
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 { return String::new(); }
            let mut buf = vec![0u16; (len + 1) as usize];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), len + 1);
            if n > 0 {
                OsString::from_wide(&buf[..n as usize]).to_string_lossy().into_owned()
            } else {
                String::new()
            }
        }
    }

    let (exe_name, title, owner_pid): (String, String, u32) = unsafe {
        let hwnd = GetClipboardOwner();
        if hwnd == 0 { return ("unknown".to_string(), String::new()); }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 { return ("unknown".to_string(), String::new()); }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        let exe = if handle == 0 {
            format!("pid:{}", pid)
        } else {
            let mut buf = [0u16; 260];
            let mut size = 260u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
            CloseHandle(handle);
            if ok != 0 && size > 0 {
                OsString::from_wide(&buf[..size as usize])
                    .to_string_lossy()
                    .rsplit('\\')
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            } else {
                format!("pid:{}", pid)
            }
        };
        // Title from clipboard owner; fall back to foreground window if empty.
        let mut t = read_title(hwnd);
        if t.is_empty() {
            t = read_title(GetForegroundWindow());
        }
        (exe, t, pid)
    };

    // Self-test by PID LINEAGE, not by exe basename. A copy from our editor is
    // owned by our WebView2 child (which descends from us); the main process
    // itself is covered because pid_is_descendant_of starts its walk at the
    // owner pid (equality counts). The old basename comparison could be
    // defeated by renaming any app's exe to "mint-exam-ide.exe" — its copies
    // then logged as clipboard_internal instead of clipboard_external.
    if super::focus::pid_is_descendant_of(owner_pid, std::process::id()) {
        return ("self".to_string(), title);
    }

    (exe_name, title)
}

#[cfg(target_os = "macos")]
fn detect_clipboard_source() -> (String, String) {
    // macOS doesn't expose pasteboard owner directly; the best proxy is the
    // currently-frontmost app, which is the app the user just copied from.
    let Some((app, fg_pid)) = super::focus::frontmost_app_macos() else {
        return ("unknown".to_string(), String::new());
    };
    // PID equality decides when available; exact-name fallback only when
    // System Events returned no unix id (contains("MINT") was spoofable by
    // renaming an app bundle).
    let is_self = match fg_pid {
        Some(p) => p == std::process::id(),
        None => app == "MINT Exam IDE" || app == "mint-exam-ide",
    };
    let source = if is_self { "self".to_string() } else { app };
    (source, String::new())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn detect_clipboard_source() -> (String, String) {
    ("external".to_string(), String::new())
}
