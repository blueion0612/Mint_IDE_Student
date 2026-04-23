use super::log::{ActivityEvent, LogHandle};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Monitors which window has foreground focus.
/// Logs focus_lost and focus_returned events with duration.
pub fn start_focus_monitor(log: LogHandle, app_handle: AppHandle) {
    thread::spawn(move || {
        let mut was_focused = true;
        let mut lost_focus_at: Option<i64> = None;

        loop {
            thread::sleep(Duration::from_millis(250));

            let (is_our_app, foreground_app) = check_foreground_window();

            if was_focused && !is_our_app {
                lost_focus_at = Some(chrono::Local::now().timestamp_millis());
                let detail = if foreground_app.contains(" — \"") || foreground_app.contains(" (\"") {
                    format!("Switched to: {}", foreground_app)
                } else {
                    format!("Switched to: {}", foreground_app)
                };
                let event = ActivityEvent::new("focus_lost", &detail, None, None);
                log.add_event(event.clone());
                let _ = app_handle.emit("activity-event", &event);
                was_focused = false;
            } else if !was_focused && is_our_app {
                let duration_ms = lost_focus_at
                    .map(|t| (chrono::Local::now().timestamp_millis() - t) as f64)
                    .unwrap_or(0.0);
                let detail = format!("Returned after {:.1}s", duration_ms / 1000.0);
                let event = ActivityEvent::new("focus_returned", &detail, None, Some(duration_ms));
                log.add_event(event.clone());
                let _ = app_handle.emit("activity-event", &event);
                was_focused = true;
                lost_focus_at = None;
            }
        }
    });
}

#[cfg(target_os = "windows")]
fn check_foreground_window() -> (bool, String) {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowTextLengthW(hwnd: isize) -> i32;
        fn GetWindowTextW(hwnd: isize, text: *mut u16, max: i32) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
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

    unsafe {
        let fg_hwnd = GetForegroundWindow();
        if fg_hwnd == 0 {
            return (false, "unknown".to_string());
        }

        let mut fg_pid: u32 = 0;
        GetWindowThreadProcessId(fg_hwnd, &mut fg_pid);
        let our_pid = GetCurrentProcessId();

        // Always extract the exe name first so we can also detect Tauri's
        // WebView2 helper process (different PID than our main, but still us).
        let raw_exe = {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, fg_pid);
            if handle == 0 {
                format!("pid:{}", fg_pid)
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
                    format!("pid:{}", fg_pid)
                }
            }
        };

        let is_ours = fg_pid == our_pid
            || raw_exe.eq_ignore_ascii_case("msedgewebview2.exe");

        let exe_name = if is_ours {
            "MINT Exam IDE".to_string()
        } else {
            raw_exe
        };

        // Window title (e.g. "ChatGPT - Google Chrome")
        let title_len = GetWindowTextLengthW(fg_hwnd);
        let title = if title_len > 0 {
            let mut tbuf = vec![0u16; (title_len + 1) as usize];
            let n = GetWindowTextW(fg_hwnd, tbuf.as_mut_ptr(), title_len + 1);
            if n > 0 {
                OsString::from_wide(&tbuf[..n as usize]).to_string_lossy().into_owned()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let label = if is_ours || title.is_empty() {
            exe_name
        } else {
            format!("{} — \"{}\"", exe_name, title)
        };
        (is_ours, label)
    }
}

#[cfg(target_os = "macos")]
fn check_foreground_window() -> (bool, String) {
    use std::process::Command;

    let output = Command::new("osascript")
        .args(["-e", "tell application \"System Events\" to get name of first application process whose frontmost is true"])
        .output();

    match output {
        Ok(out) => {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let is_ours = name.contains("MINT") || name.contains("mint-exam-ide");
            (is_ours, name)
        }
        Err(_) => (false, "unknown".to_string()),
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn check_foreground_window() -> (bool, String) {
    (true, "unknown".to_string())
}
