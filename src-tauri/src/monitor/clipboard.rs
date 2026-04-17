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
                eprintln!("Failed to initialize clipboard monitor: {}", e);
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
                let char_count = current.len() as u32;
                let truncated = if current.len() > 200 {
                    format!("{}...", &current[..200])
                } else {
                    current.clone()
                };

                let source = detect_clipboard_source();

                let event_type = if source == "self" {
                    "clipboard_internal"
                } else {
                    "clipboard_external"
                };

                let detail = format!(
                    "[Source: {}] {}",
                    source,
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
fn detect_clipboard_source() -> String {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    // Use raw Win32 FFI for maximum compatibility
    #[link(name = "user32")]
    extern "system" {
        fn GetClipboardOwner() -> isize;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
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

    unsafe {
        let hwnd = GetClipboardOwner();
        if hwnd == 0 {
            return "unknown".to_string();
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return "unknown".to_string();
        }

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return format!("pid:{}", pid);
        }

        let mut buf = [0u16; 260];
        let mut size = 260u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);

        if ok != 0 && size > 0 {
            let path = OsString::from_wide(&buf[..size as usize])
                .to_string_lossy()
                .to_string();
            path.rsplit('\\')
                .next()
                .unwrap_or("unknown")
                .to_string()
        } else {
            format!("pid:{}", pid)
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn detect_clipboard_source() -> String {
    "external".to_string()
}
