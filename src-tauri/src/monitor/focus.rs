use super::log::{ActivityEvent, LogHandle};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Monitors which window has foreground focus.
/// Logs focus_lost and focus_returned events with duration.
pub fn start_focus_monitor(
    log: LogHandle,
    app_handle: AppHandle,
    running: crate::runner::RunningProcess,
) {
    thread::spawn(move || {
        let mut was_focused = true;
        let mut lost_focus_at: Option<i64> = None;
        #[cfg(target_os = "macos")]
        let mut automation_denial_reported = false;

        // Windows uses cheap in-process Win32 calls, so poll fast. macOS spawns
        // an osascript process per sample (fork+exec + AppleEvent round-trip),
        // which is far heavier — poll less often to cut process churn / battery.
        #[cfg(target_os = "macos")]
        const POLL_MS: u64 = 1000;
        #[cfg(not(target_os = "macos"))]
        const POLL_MS: u64 = 250;

        loop {
            thread::sleep(Duration::from_millis(POLL_MS));

            // macOS: osascript needs the Automation (Apple Events → System
            // Events) permission. If it has been denied, every query fails —
            // surface that ONCE, loudly, instead of silently monitoring
            // nothing for the whole exam.
            #[cfg(target_os = "macos")]
            if !automation_denial_reported
                && AUTOMATION_DENIED.load(std::sync::atomic::Ordering::SeqCst)
            {
                automation_denial_reported = true;
                let event = ActivityEvent::new(
                    "monitor_health_fail",
                    "포커스/클립보드 모니터링 권한이 없습니다. 시스템 설정 > 개인정보 보호 및 보안 > 자동화에서 'MINT Exam IDE' → 'System Events'를 허용한 뒤 IDE를 재시작하세요.",
                    None,
                    None,
                );
                log.add_event(event.clone());
                let _ = app_handle.emit("activity-event", &event);
            }

            // The student's own program window (matplotlib/tkinter, same python
            // process) lives in the run-child PID — exempt that exact PID so
            // viewing it isn't logged as focus_lost. A spawned browser has a
            // DIFFERENT PID, so it is still flagged.
            // Exempt the current run-child window: the streaming runner's child
            // (RunningProcess) OR the notebook cell's child (NOTEBOOK_CHILD_PID).
            // The student's own matplotlib/tkinter window lives in that python
            // process; a spawned browser has a different PID and stays flagged.
            let run_child_pid = running.lock().ok()
                .and_then(|g| g.as_ref().map(|(_, c)| c.id()))
                .or_else(|| {
                    let nb = crate::NOTEBOOK_CHILD_PID.load(std::sync::atomic::Ordering::SeqCst);
                    if nb != 0 { Some(nb) } else { None }
                });
            // None = indeterminate (query failed) — keep the previous state
            // rather than fabricating a focus_lost with an empty app name.
            let Some((is_our_app, foreground_app)) = check_foreground_window(run_child_pid) else {
                continue;
            };

            if was_focused && !is_our_app {
                lost_focus_at = Some(chrono::Local::now().timestamp_millis());
                let detail = format!("Switched to: {}", foreground_app);
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
fn check_foreground_window(run_child_pid: Option<u32>) -> Option<(bool, String)> {
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
            // No foreground window (session transition) — indeterminate.
            return None;
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

        // A foreground msedgewebview2.exe is "ours" ONLY if it descends from
        // our process (our own Tauri WebView2 child). Matching the bare
        // basename treated ANY WebView2-hosted app (Office, chat apps, …) as
        // self, silently suppressing focus_lost when a student switched to one.
        // MEMOIZED: while the student works IN the IDE, our own WebView2 child
        // is the foreground window on every 250ms poll — an unmemoized
        // descendant check snapshotted the ENTIRE process table 4×/second for
        // the whole exam (visible CPU cost on low-spec machines).
        let is_ours = fg_pid == our_pid
            || Some(fg_pid) == run_child_pid
            || (raw_exe.eq_ignore_ascii_case("msedgewebview2.exe")
                && descendant_of_us_cached(fg_pid, our_pid));

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
        Some((is_ours, label))
    }
}

/// Set once when osascript reports the Automation permission denial (-1743);
/// the monitor loop turns it into a one-time monitor_health_fail alert.
#[cfg(target_os = "macos")]
pub(crate) static AUTOMATION_DENIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Query the frontmost app's {name, unix id} in ONE osascript round-trip.
/// Returns None when the query fails (Automation permission denied, System
/// Events busy) so the caller keeps its previous focus state instead of
/// logging a bogus focus_lost with an empty name.
#[cfg(target_os = "macos")]
pub(crate) fn frontmost_app_macos() -> Option<(String, Option<u32>)> {
    use std::process::Command;

    // `with timeout` bounds a wedged System Events: without it, osascript
    // inherits AppleScript's default 2-minute Apple-Event timeout, which would
    // stall this monitor thread for two minutes on a single bad sample.
    let output = Command::new("osascript")
        .args(["-e", "with timeout of 2 seconds\ntell application \"System Events\" to get {name, unix id} of (first application process whose frontmost is true)\nend timeout"])
        .output()
        .ok()?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        // errAEEventNotPermitted — the app was denied Automation access (or
        // NSAppleEventsUsageDescription is missing). Every future call will
        // fail the same way, so latch it for the one-time alert.
        if err.contains("-1743") || err.contains("1743") {
            AUTOMATION_DENIED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    // Output form: "AppName, 1234". App names may themselves contain ", " so
    // split at the LAST separator.
    match text.rsplit_once(", ") {
        Some((name, id)) => {
            let pid = id.trim().parse::<u32>().ok();
            Some((name.trim().to_string(), pid))
        }
        None => Some((text, None)),
    }
}

#[cfg(target_os = "macos")]
fn check_foreground_window(run_child_pid: Option<u32>) -> Option<(bool, String)> {
    let (name, fg_pid) = frontmost_app_macos()?;
    let our_pid = std::process::id();
    // PID equality is the authoritative self-test; when a PID is available it
    // ALONE decides. The name comparison is only a fallback for the rare case
    // System Events returned no unix id — and it is EXACT, not contains():
    // a substring match let a student rename any app to "MINT <something>" and
    // have every switch to it silently exempted from focus_lost.
    // The run-child exemption mirrors Windows: the student's own
    // matplotlib/tkinter window lives in the run/notebook python process.
    let is_ours = match fg_pid {
        Some(p) => p == our_pid || Some(p) == run_child_pid,
        None => name == "MINT Exam IDE" || name == "mint-exam-ide",
    };
    Some((is_ours, name))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn check_foreground_window(_run_child_pid: Option<u32>) -> Option<(bool, String)> {
    Some((true, "unknown".to_string()))
}

/// Memoized `pid_is_descendant_of` for the focus poll's hot path. Caches the
/// verdict for the LAST queried pid only — the foreground window does not
/// change 4×/second, so this collapses the steady-state case (our own WebView2
/// child in front) to a single process-table snapshot per focus change instead
/// of one every 250ms. A pid reused by a different process would need to ALSO
/// be named msedgewebview2.exe to reach this path, so a stale verdict is not a
/// realistic bypass.
#[cfg(target_os = "windows")]
fn descendant_of_us_cached(pid: u32, ancestor: u32) -> bool {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(u32, u32, bool)>> = Mutex::new(None);
    if let Ok(guard) = CACHE.lock() {
        if let Some((cached_pid, cached_anc, verdict)) = *guard {
            if cached_pid == pid && cached_anc == ancestor {
                return verdict;
            }
        }
    }
    let verdict = pid_is_descendant_of(pid, ancestor);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((pid, ancestor, verdict));
    }
    verdict
}

/// Walk the parent-PID chain from `pid` upward; true if `ancestor` is reached.
/// Used to tell OUR Tauri WebView2 child (legit, descends from us) apart from
/// an unrelated WebView2-hosted app that merely shares the msedgewebview2.exe
/// image name. Cheap one-shot Toolhelp snapshot; bounded walk guards PID reuse.
#[cfg(target_os = "windows")]
pub(crate) fn pid_is_descendant_of(mut pid: u32, ancestor: u32) -> bool {
    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const INVALID_HANDLE_VALUE: isize = -1;

    if pid == 0 || ancestor == 0 {
        return false;
    }
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        // child PID -> parent PID
        let mut parent_of: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let mut entry: ProcessEntry32W = std::mem::zeroed();
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                parent_of.insert(entry.th32_process_id, entry.th32_parent_process_id);
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);

        let mut guard = 0;
        while pid != 0 && guard < 64 {
            if pid == ancestor {
                return true;
            }
            match parent_of.get(&pid) {
                Some(&parent) if parent != pid => pid = parent,
                _ => break,
            }
            guard += 1;
        }
    }
    false
}
