use serde::Serialize;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const EMIT_BATCH_INTERVAL_MS: u64 = 50;
const EMIT_BATCH_MAX_BYTES: usize = 8192;
const MAX_OUTPUT_LINES_BEFORE_AUTO_STOP: u64 = 200_000;
/// Byte-based auto-stop companion to the line cap. A program that prints one
/// enormous line (`print(list(range(10**8)))` — a real student mistake) or
/// spams without newlines never trips the LINE cap; before the chunked reader
/// this OOM'd the IDE on low-RAM laptops.
const MAX_OUTPUT_BYTES_BEFORE_AUTO_STOP: u64 = 64 * 1024 * 1024;
/// Retained-in-RAM cap for the collected stdout (run-done payload).
const STDOUT_BUF_CAP_BYTES: usize = 8 * 1024 * 1024;

// Cached Python path — found once, reused forever
static CACHED_PYTHON: Mutex<Option<String>> = Mutex::new(None);

// Monotonic run generation. Each streaming run claims the next id and stores
// it alongside its child; the post-wait reaper only takes/emits for ITS id, so
// a lingering thread from a stopped run can neither steal a newer run's child
// nor emit a misattributed run-done for it.
static RUN_GEN: AtomicU64 = AtomicU64::new(0);

/// Event sent to frontend for each line of output
#[derive(Debug, Clone, Serialize)]
pub struct RunOutputLine {
    pub stream: String, // "stdout", "stderr", "system"
    pub text: String,
}

/// Shared handle to the running process so it can be stopped. The `u64` is the
/// run generation that owns this child (see RUN_GEN).
pub type RunningProcess = Arc<Mutex<Option<(u64, Child)>>>;

pub fn new_running_process() -> RunningProcess {
    Arc::new(Mutex::new(None))
}

/// Run generations that were stopped while no child was in the slot.
///
/// There is a real gap between "the compiler exited" and "the program is
/// published": `build_native` takes the compiler child out, then the program is
/// spawned. A Stop landing in that window finds nothing to kill and would
/// otherwise be lost, letting the program start after the student stopped it.
/// Recording the id closes the gap. The set is tiny and drained as soon as it
/// is read.
static CANCELLED_RUNS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Record that the CURRENT run was asked to stop.
///
/// Called by `stop_code` in addition to killing whatever is in the slot. The
/// frontend serialises runs (the Run button becomes Stop), so the generation
/// most recently handed out by `RUN_GEN` is the one being stopped — including
/// when the slot is momentarily empty between the compiler exiting and the
/// program being published.
pub fn note_stop_request() {
    let gen = RUN_GEN.load(Ordering::SeqCst);
    if let Ok(mut c) = CANCELLED_RUNS.lock() {
        if !c.contains(&gen) {
            c.push(gen);
        }
        // Bounded: an entry is normally consumed by the run it belongs to, but
        // a run that exited before reading its own flag would otherwise leak.
        while c.len() > 32 {
            c.remove(0);
        }
    }
}

/// Consume the cancellation flag for `gen`, if one was set.
fn take_cancelled(gen: u64) -> bool {
    match CANCELLED_RUNS.lock() {
        Ok(mut c) => {
            let had = c.contains(&gen);
            c.retain(|g| *g != gen);
            had
        }
        Err(_) => false,
    }
}

/// The write end of the running child's stdin, keyed by the same run
/// generation as the child itself.
///
/// Before this existed the child inherited the IDE's stdin. A GUI process has
/// no console, so that handle was invalid and every read hit EOF immediately:
/// `std::cin >> n` left `n` untouched and `input()` raised EOFError. Exam
/// problems overwhelmingly read their input from stdin, so C++ (and Python)
/// answers produced confidently wrong output with no visible error.
pub type RunningStdin = Arc<Mutex<Option<(u64, std::process::ChildStdin)>>>;

pub fn new_running_stdin() -> RunningStdin {
    Arc::new(Mutex::new(None))
}

/// Feed a chunk to the running program's stdin.
///
/// Returns `Ok(true)` when the bytes were handed to the pipe, `Ok(false)` when
/// nothing is running or the program has already closed its end (a finished or
/// non-reading program is not an error the student should see as a failure).
pub fn write_stdin(handle: &RunningStdin, text: &str) -> Result<bool, String> {
    use std::io::Write;
    let mut guard = handle.lock().map_err(|_| "stdin state poisoned".to_string())?;
    let stdin = match guard.as_mut() {
        Some((_, s)) => s,
        None => return Ok(false),
    };
    match stdin.write_all(text.as_bytes()).and_then(|_| stdin.flush()) {
        Ok(()) => Ok(true),
        // The child exited (or closed stdin) between the UI click and the
        // write. Drop our end so later writes short-circuit instead of
        // repeating the same OS error.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            *guard = None;
            Ok(false)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Close stdin, signalling EOF to the program.
///
/// Essential for C++: `while (std::cin >> x)` and `while (getline(...))`
/// terminate ONLY on EOF, so without this a student could never end an
/// input loop and every such program would look like it hung.
pub fn close_stdin(handle: &RunningStdin) -> bool {
    match handle.lock() {
        Ok(mut guard) => guard.take().is_some(),
        Err(_) => false,
    }
}

/// Wait for the program to finish, THEN release our end of its stdin.
///
/// The ordering is the whole point, so it lives in one function rather than as
/// two adjacent statements someone could reorder. Closing stdin before the wait
/// hands the program an immediate EOF: `std::cin >> n` reads nothing, `input()`
/// raises, and an exam answer prints a confident wrong result with no error
/// anywhere. `wait()` is what blocks for the life of the program, and that is
/// exactly the window in which the output panel's input box must be able to
/// feed it.
fn wait_then_release_stdin(
    child: Option<Child>,
    stdin_handle: &RunningStdin,
    gen: u64,
) -> Option<i32> {
    let code = child.and_then(|mut c| c.wait().ok().and_then(|s| s.code()));
    clear_stdin_generation(stdin_handle, gen);
    code
}

/// Drop the stdin handle if it still belongs to `gen`.
///
/// Generation-checked for the same reason the child reaper is: a run that has
/// already been replaced must not close the CURRENT run's stdin.
fn clear_stdin_generation(handle: &RunningStdin, gen: u64) {
    if let Ok(mut guard) = handle.lock() {
        let owned = matches!(guard.as_ref(), Some((id, _)) if *id == gen);
        if owned {
            *guard = None;
        }
    }
}

pub fn snapshot_workspace_files(root: &std::path::Path) -> std::collections::HashSet<String> {
    snapshot_workspace_paths(root).into_keys().collect()
}

/// rel (normalized, '/'-joined, NFC) -> actual on-disk path. The actual path is
/// kept so callers can re-open the file even on macOS, where the on-disk name
/// is NFD but the rel key is NFC. Mirrors the integrity scanner's traversal:
/// recurses into ALL real directories, skips symlinks/junctions (no infinite
/// recursion), and skips only the IDE's own artifact files.
pub fn snapshot_workspace_paths(
    root: &std::path::Path,
) -> std::collections::HashMap<String, std::path::PathBuf> {
    let mut out = std::collections::HashMap::new();
    fn is_ide_artifact(rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        name.starts_with("_log_")
            || name == ".mint_baseline.json"
            || name == ".mint_baseline.json.tmp"
            || name.starts_with("._notebook_")
    }
    fn entry_is_link(entry: &std::fs::DirEntry) -> bool {
        if let Ok(ft) = entry.file_type() {
            if ft.is_symlink() {
                return true;
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            if let Ok(md) = entry.metadata() {
                if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return true;
                }
            }
        }
        false
    }
    fn walk(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut std::collections::HashMap<String, std::path::PathBuf>,
        depth: u32,
    ) {
        use unicode_normalization::UnicodeNormalization;
        if depth > 64 {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry_is_link(&entry) {
                    continue;
                }
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/")
                    .nfc()
                    .collect::<String>();
                if path.is_dir() {
                    walk(&path, root, out, depth + 1);
                } else if !is_ide_artifact(&rel) {
                    out.insert(rel, path);
                }
            }
        }
    }
    walk(root, root, &mut out, 0);
    out
}

/// Execute code with real-time streaming output via events.
/// Returns immediately — output comes through "run-output" events.
pub fn execute_code_streaming(
    language: &str,
    code: &str,
    filename: &str,
    workspace_dir: Option<&str>,
    python_path: Option<&str>,
    app_handle: AppHandle,
    process_handle: RunningProcess,
    stdin_handle: RunningStdin,
    known_writes: crate::monitor::KnownWrites,
) {
    let work_dir = workspace_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mint-exam-ide"));
    let _ = std::fs::create_dir_all(&work_dir);

    // Snapshot workspace files BEFORE running, so we can register any new
    // files the student's code creates (e.g. plt.savefig, df.to_csv) as
    // known writes — preventing tamper_new_file false positives.
    let pre_snapshot = snapshot_workspace_files(&work_dir);

    let lang = language.to_string();
    let code = code.to_string();
    let fname = filename.to_string();
    let py_path = python_path.map(|s| s.to_string());
    let dir = work_dir.clone();

    thread::spawn(move || {
        let start = std::time::Instant::now();

        // Write file
        let file_path = dir.join(&fname);
        if let Some(parent) = file_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&file_path, &code).is_err() {
            emit_line(&app_handle, "system", "Failed to write file\n");
            emit_done_with_output(&app_handle, None, 0, "", "");
            return;
        }

        // Build command
        // Claim the run generation up front, BEFORE any compile.
        //
        // It used to be claimed after the build, which left compiled languages
        // with an uncancellable window: Stop during a 100 ms-to-2 minute
        // compile found an empty process slot, did nothing, and the program
        // started anyway once the compiler finished — the student pressed Stop
        // and watched their program run. The compiler child is now published
        // under this id like any other, so Stop kills it.
        let my_id = RUN_GEN.fetch_add(1, Ordering::SeqCst) + 1;

        // Every builder yields (program, args, extra_PATH_dir). Only the
        // compiled languages need the third: a MinGW binary looks for its
        // runtime DLLs beside the compiler that produced it.
        let result = match lang.as_str() {
            "python" => build_python_cmd(&dir, &fname, py_path.as_deref()).map(|(c, a)| (c, a, None)),
            "javascript" | "typescript" => build_node_cmd(&dir, &fname).map(|(c, a)| (c, a, None)),
            "c" => build_and_run_c(&dir, &fname, &app_handle, &process_handle, my_id),
            "cpp" => build_and_run_cpp(&dir, &fname, &app_handle, &process_handle, my_id),
            "java" => build_and_run_java(&dir, &fname, &app_handle).map(|(c, a)| (c, a, None)),
            _ => {
                emit_line(&app_handle, "stderr", &format!("Unsupported language: {}\n", lang));
                emit_done_with_output(&app_handle, None, 0, "", "");
                return;
            }
        };

        let (cmd, args, extra_path_dir) = match result {
            Some(v) => v,
            None => return, // compile error already emitted
        };

        // Spawn with piped stdout/stderr
        let mut command = Command::new(&cmd);
        command.args(&args)
            .current_dir(&dir)
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1")
            .env("TF_CPP_MIN_LOG_LEVEL", "3")      // suppress TensorFlow warnings
            .env("TF_ENABLE_ONEDNN_OPTS", "0")     // suppress oneDNN messages
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // stdin is PIPED, not inherited. A GUI process has no console, so the
        // inherited handle was invalid: `std::cin >> n` and Python's `input()`
        // both saw instant EOF and the program carried on with uninitialised
        // values — silently wrong answers on any exam problem that reads input.
        // With a pipe the child blocks exactly as it would in a terminal, and
        // the output panel's input box feeds it.
        if let Some(ref bd) = extra_path_dir {
            crate::toolchain::prepend_path(&mut command, bd);
        }

        // matplotlib backend selection:
        // - Windows: force TkAgg. Our dedicated Python ships with Include_tcltk=1
        //   so it is always available; without forcing it matplotlib sometimes
        //   silently falls back to non-interactive Agg and plt.show() does nothing.
        // - macOS: do NOT force TkAgg. macOS framework Python's native MacOSX
        //   backend handles retina/DPI/multi-monitor far more reliably than
        //   X11-based Tk. Let matplotlib auto-select.
        // - Linux: same as macOS, let matplotlib choose.
        #[cfg(target_os = "windows")]
        { command.env("MPLBACKEND", "TkAgg"); }

        // Hide console window on Windows (does NOT affect GUI windows like matplotlib)
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        // Put the child in its own process group so we can SIGKILL the entire
        // tree (including grandchildren from multiprocessing/subprocess.run).
        // Without setsid, `kill -KILL -<pid>` targets the IDE's group on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let child = command.spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                emit_line(&app_handle, "stderr", &format!("Failed to run '{}': {}. Is it installed?\n", cmd, e));
                emit_done_with_output(&app_handle, None, 0, "", "");
                return;
            }
        };

        // Take stdout/stderr first, then publish the child exactly once. (The
        // old pre-store `*guard = None` left a spawn-to-store gap where a Stop
        // arriving in that window found None, killed nothing, yet the run
        // proceeded — UI showed stopped while Python kept running.)
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child_stdin = child.stdin.take();

        // Publish the child under the generation claimed before the build.
        //
        // If a Stop arrived while we were compiling, the slot no longer holds
        // our id (or holds a newer run's) — in that case the program must NOT
        // start. Kill what we just spawned and leave quietly; the frontend
        // already reset itself when the student pressed Stop.
        {
            let mut guard = process_handle.lock().unwrap();
            let cancelled = match guard.as_ref() {
                Some((id, _)) => *id != my_id,
                // An empty slot is the NORMAL state here: build_native takes
                // the compiler child back out on success. Only an explicit
                // stop request means this run was cancelled.
                None => take_cancelled(my_id),
            };
            if cancelled {
                drop(guard);
                stop_taken_child(child);
                return;
            }
            *guard = Some((my_id, child));
        }
        // Publish stdin under the SAME generation. Generation-keying matters
        // for the same reason it does for the child itself: a stale input line
        // typed against a finished run must not be delivered into a newer run's
        // stdin.
        {
            let mut guard = stdin_handle.lock().unwrap();
            *guard = child_stdin.map(|s| (my_id, s));
        }

        let ah1 = app_handle.clone();

        let stdout_collected = Arc::new(Mutex::new(String::new()));
        let stderr_collected = Arc::new(Mutex::new(String::new()));
        let sc1 = stdout_collected.clone();
        let sc2 = stderr_collected.clone();

        // stdout: CHUNKED reader (64KB), batched into ~50ms emits. Chunking —
        // not `lines()` — is what bounds memory: BufRead::lines() buffers an
        // entire line before returning it, so a single newline-less giant line
        // was held in RAM in full (and shipped to the webview in one event).
        // Chunking also streams partial lines, so `print(x, end="")` prompts
        // and `\r` progress bars now appear live instead of only at exit.
        // Auto-stop on EITHER the line cap or the total-bytes cap.
        let line_counter = Arc::new(AtomicU64::new(0));
        let byte_counter = Arc::new(AtomicU64::new(0));
        let lc1 = line_counter.clone();
        let bc1 = byte_counter.clone();
        let proc_handle1 = process_handle.clone();
        let t1 = thread::spawn(move || -> bool {
            let mut auto_stopped = false;
            if let Some(mut out) = stdout {
                use std::io::Read;
                let mut chunk = [0u8; 65536];
                let mut pending: Vec<u8> = Vec::new();
                let mut buffer = String::new();
                let mut last_flush = Instant::now();
                loop {
                    let n = match out.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    let nl = chunk[..n].iter().filter(|&&b| b == b'\n').count() as u64;
                    let lines_so_far = lc1.fetch_add(nl, Ordering::Relaxed) + nl;
                    let bytes_so_far = bc1.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                    pending.extend_from_slice(&chunk[..n]);
                    buffer.push_str(&drain_utf8_lossy(&mut pending));

                    let over_lines = lines_so_far > MAX_OUTPUT_LINES_BEFORE_AUTO_STOP;
                    let over_bytes = bytes_so_far > MAX_OUTPUT_BYTES_BEFORE_AUTO_STOP;
                    if over_lines || over_bytes {
                        if !buffer.is_empty() {
                            push_capped(&sc1, STDOUT_BUF_CAP_BYTES, &buffer);
                            emit_line(&ah1, "stdout", &buffer);
                            buffer.clear();
                        }
                        let what = if over_lines {
                            format!("{} lines", MAX_OUTPUT_LINES_BEFORE_AUTO_STOP)
                        } else {
                            format!("{} MB", MAX_OUTPUT_BYTES_BEFORE_AUTO_STOP / (1024 * 1024))
                        };
                        emit_line(&ah1, "system",
                            &format!("\n[OUTPUT LIMIT EXCEEDED — {}. Auto-stopping process.]\n", what));
                        stop_process_generation(&proc_handle1, my_id);
                        auto_stopped = true;
                        break;
                    }
                    if buffer.len() >= EMIT_BATCH_MAX_BYTES
                        || last_flush.elapsed() >= Duration::from_millis(EMIT_BATCH_INTERVAL_MS)
                    {
                        push_capped(&sc1, STDOUT_BUF_CAP_BYTES, &buffer);
                        emit_line(&ah1, "stdout", &buffer);
                        buffer.clear();
                        last_flush = Instant::now();
                    }
                }
                if !auto_stopped {
                    // Flush any decodable remainder + a lossy tail (incomplete
                    // final multi-byte char at EOF).
                    buffer.push_str(&drain_utf8_lossy(&mut pending));
                    if !pending.is_empty() {
                        buffer.push_str(&String::from_utf8_lossy(&pending));
                    }
                    if !buffer.is_empty() {
                        push_capped(&sc1, STDOUT_BUF_CAP_BYTES, &buffer);
                        emit_line(&ah1, "stdout", &buffer);
                    }
                }
            }
            auto_stopped
        });

        // stderr: collect silently (chunked, capped), display AFTER stdout
        // finishes. Counts toward the SAME line/byte auto-stop caps as stdout —
        // a program that spams only stderr (`logging`/`warnings` default there)
        // must not evade them.
        const STDERR_BUF_CAP_BYTES: usize = 4 * 1024 * 1024; // 4 MB retained
        let lc2 = line_counter.clone();
        let bc2 = byte_counter.clone();
        let proc_handle2 = process_handle.clone();
        let ah2 = app_handle.clone();
        let t2 = thread::spawn(move || {
            if let Some(mut err) = stderr {
                use std::io::Read;
                let mut chunk = [0u8; 65536];
                let mut pending: Vec<u8> = Vec::new();
                loop {
                    let n = match err.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    let nl = chunk[..n].iter().filter(|&&b| b == b'\n').count() as u64;
                    let lines_so_far = lc2.fetch_add(nl, Ordering::Relaxed) + nl;
                    let bytes_so_far = bc2.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                    pending.extend_from_slice(&chunk[..n]);
                    push_capped(&sc2, STDERR_BUF_CAP_BYTES, &drain_utf8_lossy(&mut pending));
                    let over_lines = lines_so_far > MAX_OUTPUT_LINES_BEFORE_AUTO_STOP;
                    if over_lines || bytes_so_far > MAX_OUTPUT_BYTES_BEFORE_AUTO_STOP {
                        let what = if over_lines {
                            format!("{} lines", MAX_OUTPUT_LINES_BEFORE_AUTO_STOP)
                        } else {
                            format!("{} MB", MAX_OUTPUT_BYTES_BEFORE_AUTO_STOP / (1024 * 1024))
                        };
                        emit_line(&ah2, "system",
                            &format!("\n[OUTPUT LIMIT EXCEEDED — {}. Auto-stopping process.]\n", what));
                        stop_process_generation(&proc_handle2, my_id);
                        break;
                    }
                }
                if !pending.is_empty() {
                    push_capped(&sc2, STDERR_BUF_CAP_BYTES, &String::from_utf8_lossy(&pending));
                }
            }
        });

        // Reap OUR child FIRST — gate completion on the child EXITING, not on
        // the reader threads reaching pipe EOF. If the student's program spawns
        // a grandchild that inherits the stdout/stderr pipe (multiprocessing,
        // subprocess.Popen, sklearn/joblib n_jobs=-1), the pipe write-end stays
        // open after the direct child exits, so `reader.lines()` never sees EOF
        // and t1.join() would block FOREVER — run-done never fires and the UI is
        // stuck "running" for a program that already finished. Waiting on the
        // child process is independent of the pipe, so it returns correctly.
        //
        // Only reap if the child is still the active one: a concurrent Stop
        // (child taken by stop_code) or a newer run means the slot holds a
        // DIFFERENT generation — we must not steal it or fire a misattributed
        // run-done. Taking under the lock keeps a concurrent Stop unblocked.
        let my_child = {
            let mut guard = process_handle.lock().unwrap();
            match guard.as_ref() {
                Some((id, _)) if *id == my_id => guard.take().map(|(_, c)| c),
                _ => None,
            }
        };
        let owned = my_child.is_some();
        // Blocks for the life of the program, keeping stdin open across it, then
        // releases the handle. A student who presses Stop is released instead by
        // `stop_code`, which closes stdin explicitly before killing the child.
        let exit_code = wait_then_release_stdin(my_child, &stdin_handle, my_id);

        // The direct child has exited. Drain the reader threads, but BOUNDED:
        // in the normal case the pipe hit EOF the instant the child exited, so
        // they finish in microseconds; in the grandchild-inherited-pipe case
        // they never will, so detach after a short grace (they keep draining
        // harmlessly to their buffers) rather than hang the run forever.
        let auto_stopped = join_bounded(t1, Duration::from_secs(3)).unwrap_or(false);
        join_bounded(t2, Duration::from_secs(3));

        // Now emit stderr all at once (after stdout)
        {
            let err_str = stderr_collected.lock().unwrap().clone();
            if !err_str.is_empty() {
                emit_line(&app_handle, "stderr", &err_str);
            }
        }

        // Register the files this run touched as known writes, each PINNED to
        // its post-run content hash: a legitimate program output (plt.savefig,
        // csv append) is not flagged, but an EXTERNAL overwrite of any path
        // within the grace window (different content) still raises tamper.
        // Untouched files are pinned to their unchanged hash (harmless — only a
        // no-op equal-content "change" could be excused). Files the program
        // DELETED get a time-only deletion grace so program-driven removals are
        // not flagged. (Previously the pre/post UNION was marked time-only,
        // which let `print(1)` launder the ENTIRE workspace for 8s.)
        // Register the files this run touched as known writes, each PINNED to
        // its post-run content hash (so a program output isn't flagged, while
        // an external overwrite of any path with DIFFERENT content within the
        // grace window still raises tamper). Files the program DELETED get a
        // time-only deletion grace so program-driven removals aren't flagged.
        // (Monitoring is content-pinned and not suppressed during runs, so an
        // external edit is still flagged by the live poll; generated-output
        // file types are excluded from monitoring entirely — see integrity.rs.)
        let post_paths = snapshot_workspace_paths(&dir);
        for (rel, path) in &post_paths {
            match crate::monitor::hash_file_retry(path) {
                Some(h) => crate::monitor::mark_known_write_hash(&known_writes, rel, &h),
                None => crate::monitor::mark_known_write(&known_writes, rel),
            }
        }
        let post_rels: std::collections::HashSet<String> = post_paths.into_keys().collect();
        for rel in pre_snapshot.difference(&post_rels) {
            crate::monitor::mark_known_write(&known_writes, rel);
        }

        // Emit run-done only if we owned the child (normal completion) or we
        // auto-stopped this run ourselves. Staying silent when the user's Stop
        // already reset the UI (and a new run may be active) prevents the
        // straggler/misattributed run-done class of bugs.
        //
        // The collected strings ride along ONLY as the frontend's fallback for
        // runs so fast their streamed chunks were missed — always tiny output.
        // Ship a bounded prefix, not the full retained buffers: serializing up
        // to 12MB of JSON per run-done was a pointless post-run hiccup on
        // low-spec machines (the streaming path already displayed everything).
        if owned || auto_stopped {
            const DONE_STDOUT_CAP: usize = 256 * 1024;
            const DONE_STDERR_CAP: usize = 128 * 1024;
            let elapsed = start.elapsed().as_millis() as u64;
            let stdout_str = cap_prefix(&stdout_collected.lock().unwrap(), DONE_STDOUT_CAP);
            let stderr_str = cap_prefix(&stderr_collected.lock().unwrap(), DONE_STDERR_CAP);
            emit_done_with_output(&app_handle, exit_code, elapsed, &stdout_str, &stderr_str);
        }
    });
}

/// Char-boundary-safe bounded prefix of `s` (with a truncation marker).
fn cap_prefix(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut cut = cap;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]\n", &s[..cut])
}

/// Auto-stop helper for the output-reader threads: take and kill the child in
/// the shared slot ONLY if it still belongs to run generation `my_id`. The
/// readers can outlive their run (detached by join_bounded when a grandchild
/// holds the pipe open), so an unconditional take here could SIGKILL a NEWER
/// run's freshly-published child while draining the old run's backlog.
fn stop_process_generation(process_handle: &RunningProcess, my_id: u64) -> bool {
    let child_opt = process_handle.lock().ok().and_then(|mut g| match g.as_ref() {
        Some((id, _)) if *id == my_id => g.take().map(|(_, c)| c),
        _ => None,
    });
    match child_opt {
        Some(child) => { stop_taken_child(child); true }
        None => false,
    }
}

/// Kill a previously-taken child and its descendant tree.
/// On Windows: taskkill /F /T. On Unix: SIGKILL to the child's process group
/// (works because we set process_group(0) at spawn time).
pub fn stop_taken_child(mut child: Child) {
    let pid = child.id();

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .output();
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Negative PID = process group. Requires the child to be a group
        // leader, which we ensured via `process_group(0)` at spawn.
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", pid)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// Run a pip command with streaming output. Returns final exit code.
fn run_pip_streaming(py_cmd: &str, args: &[&str], app: &AppHandle) -> i32 {
    let mut command = Command::new(py_cmd);
    command.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let child = command.spawn();
    match child {
        Ok(mut c) => {
            let stdout = c.stdout.take();
            let stderr = c.stderr.take();
            let app2 = app.clone();
            let t1 = thread::spawn(move || {
                if let Some(out) = stdout {
                    for line in BufReader::new(out).lines().flatten() {
                        emit_line(&app2, "stdout", &format!("{}\n", line));
                    }
                }
            });
            let app3 = app.clone();
            let t2 = thread::spawn(move || {
                if let Some(err) = stderr {
                    for line in BufReader::new(err).lines().flatten() {
                        emit_line(&app3, "stderr", &format!("{}\n", line));
                    }
                }
            });
            t1.join().ok();
            t2.join().ok();
            c.wait().ok().and_then(|s| s.code()).unwrap_or(-1)
        }
        Err(e) => {
            emit_line(app, "stderr", &format!("pip failed: {}\n", e));
            -1
        }
    }
}

/// Smart install: routes torch / tensorflow through their proper indexes.
pub fn pip_install_smart(
    packages: &[String],
    python_path: Option<&str>,
    app_handle: AppHandle,
) {
    let py = find_python(python_path);
    let pkgs = packages.to_vec();

    thread::spawn(move || {
        let py_cmd = match py {
            Some(p) => p,
            None => {
                emit_line(&app_handle, "stderr", "Python not found\n");
                emit_line(&app_handle, "system", "[INSTALL_DONE:fail]\n");
                return;
            }
        };

        let mut torch_pkgs: Vec<String> = Vec::new();
        let mut tf_pkgs: Vec<String> = Vec::new();
        let mut other_pkgs: Vec<String> = Vec::new();
        for pkg in &pkgs {
            let lower = pkg.to_lowercase();
            if lower.starts_with("torch") {
                torch_pkgs.push(pkg.clone());
            } else if lower.starts_with("tensorflow") {
                tf_pkgs.push(pkg.clone());
            } else {
                other_pkgs.push(pkg.clone());
            }
        }

        let mut overall_ok = true;

        if !other_pkgs.is_empty() {
            emit_line(&app_handle, "system",
                &format!("Installing: {}\n", other_pkgs.join(", ")));
            let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into(), "--upgrade".into()];
            args.extend(other_pkgs);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
            if code != 0 { overall_ok = false; }
            emit_line(&app_handle, "system", &format!("[core exit {}]\n", code));
        }

        if !torch_pkgs.is_empty() {
            emit_line(&app_handle, "system",
                &format!("Installing PyTorch (CPU): {}\n", torch_pkgs.join(", ")));
            let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into()];
            args.extend(torch_pkgs);
            args.push("--index-url".into());
            args.push("https://download.pytorch.org/whl/cpu".into());
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
            if code != 0 { overall_ok = false; }
            emit_line(&app_handle, "system", &format!("[torch exit {}]\n", code));
        }

        if !tf_pkgs.is_empty() {
            emit_line(&app_handle, "system",
                &format!("Installing TensorFlow: {}\n", tf_pkgs.join(", ")));
            let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into()];
            args.extend(tf_pkgs);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
            if code != 0 { overall_ok = false; }
            emit_line(&app_handle, "system", &format!("[tensorflow exit {}]\n", code));
        }

        let marker = if overall_ok { "[INSTALL_DONE:ok]\n" } else { "[INSTALL_DONE:partial]\n" };
        emit_line(&app_handle, "system", marker);
    });
}

pub fn pip_uninstall(
    packages: &[String],
    python_path: Option<&str>,
    app_handle: AppHandle,
) {
    let py = find_python(python_path);
    let pkgs = packages.to_vec();

    thread::spawn(move || {
        let py_cmd = match py {
            Some(p) => p,
            None => {
                emit_line(&app_handle, "stderr", "Python not found\n");
                emit_line(&app_handle, "system", "[UNINSTALL_DONE:fail]\n");
                return;
            }
        };
        emit_line(&app_handle, "system",
            &format!("Uninstalling: {}\n", pkgs.join(", ")));
        let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "uninstall".into(), "-y".into()];
        args.extend(pkgs);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let code = run_pip_streaming(&py_cmd, &arg_refs, &app_handle);
        let marker = if code == 0 { "[UNINSTALL_DONE:ok]\n" } else { "[UNINSTALL_DONE:fail]\n" };
        emit_line(&app_handle, "system", marker);
    });
}

pub fn pip_list(python_path: Option<&str>) -> Vec<String> {
    let py = match find_python(python_path) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut command = Command::new(&py);
    command.args(["-m", "pip", "list", "--format=freeze"]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = match command.output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|l| l.split("==").next().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

// ===== Helpers =====

/// Number of trailing bytes in `buf` that form an INCOMPLETE (not invalid)
/// UTF-8 sequence — i.e. a multi-byte char split by the chunk boundary. 0..=3.
fn incomplete_utf8_suffix_len(buf: &[u8]) -> usize {
    let len = buf.len();
    let start = len.saturating_sub(3);
    for i in (start..len).rev() {
        let b = buf[i];
        if b < 0x80 {
            return 0; // ASCII — nothing dangling
        }
        if b >= 0xC0 {
            // Leading byte of a 2-4 byte sequence.
            let need = if b >= 0xF0 { 4 } else if b >= 0xE0 { 3 } else { 2 };
            return if i + need > len { len - i } else { 0 };
        }
        // 0x80..=0xBF: continuation byte — keep scanning back for the lead.
    }
    0 // ≥4 trailing continuation bytes can't be a valid split — let lossy eat them
}

/// Decode `pending` as UTF-8 (invalid bytes → U+FFFD), leaving at most an
/// incomplete trailing multi-byte sequence in place for the next chunk.
/// SINGLE-PASS: an earlier version re-validated the remainder after every
/// invalid byte, which went O(n²) — a cell doing
/// `sys.stdout.buffer.write(os.urandom(10**7))` burned CPU for minutes.
/// from_utf8_lossy already substitutes U+FFFD for every invalid sequence in
/// one pass; we only need to hold back a split char at the very end.
pub(crate) fn drain_utf8_lossy(pending: &mut Vec<u8>) -> String {
    let keep = incomplete_utf8_suffix_len(pending);
    let cut = pending.len() - keep;
    let out = String::from_utf8_lossy(&pending[..cut]).into_owned();
    pending.drain(..cut);
    out
}

/// Append `s` to a shared collected-output String, bounded at `cap` bytes
/// (char-boundary-safe cut + one truncation marker). The RETAINED buffer is
/// what run-done ships to the frontend; streaming display is unaffected.
pub(crate) fn push_capped(sink: &Arc<Mutex<String>>, cap: usize, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Ok(mut buf) = sink.lock() {
        if buf.len() >= cap {
            return;
        }
        let room = cap - buf.len();
        if s.len() <= room {
            buf.push_str(s);
        } else {
            let mut cut = room;
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            buf.push_str(&s[..cut]);
            buf.push_str("\n[output truncated — retained buffer limit reached]\n");
        }
    }
}

/// Join a thread but give up after `timeout`, leaving it detached (it keeps
/// running). Used to drain the output reader threads without hanging the run
/// when a grandchild holds the inherited pipe open past the direct child's
/// exit. `JoinHandle::is_finished` (stable) lets us poll without blocking.
pub(crate) fn join_bounded<T>(handle: thread::JoinHandle<T>, timeout: Duration) -> Option<T> {
    let start = Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= timeout {
            return None; // detach; the thread finishes when the pipe finally closes
        }
        thread::sleep(Duration::from_millis(20));
    }
    handle.join().ok()
}

fn emit_line(app: &AppHandle, stream: &str, text: &str) {
    let _ = app.emit("run-output", RunOutputLine {
        stream: stream.to_string(),
        text: text.to_string(),
    });
}

fn emit_done_with_output(app: &AppHandle, exit_code: Option<i32>, duration_ms: u64, stdout: &str, stderr: &str) {
    #[derive(Clone, Serialize)]
    struct RunDone {
        exit_code: Option<i32>,
        duration_ms: u64,
        stdout: String,
        stderr: String,
    }
    let _ = app.emit("run-done", RunDone {
        exit_code, duration_ms,
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    });
}

pub fn find_python_cached(python_path: Option<&str>) -> Option<String> {
    find_python(python_path)
}

fn find_python(python_path: Option<&str>) -> Option<String> {
    // User-selected path takes priority
    if let Some(py) = python_path {
        return Some(py.to_string());
    }

    // Return cached path if available (instant)
    if let Ok(cache) = CACHED_PYTHON.lock() {
        if let Some(ref cached) = *cache {
            return Some(cached.clone());
        }
    }

    // First-time discovery
    let found = discover_python();
    if let Some(ref py) = found {
        if let Ok(mut cache) = CACHED_PYTHON.lock() {
            *cache = Some(py.clone());
        }
    }
    found
}

fn discover_python() -> Option<String> {
    // Fast: check PATH first (no subprocess spawn for exists-check)
    #[cfg(target_os = "windows")]
    {
        // Check well-known Windows paths by file existence (no process spawn = instant)
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();

        let direct_paths = [
            // MINT-dedicated portable Python first — keeps the no-python_path
            // fallback consistent with lib::find_system_python.
            "C:\\ProgramData\\MINT_Python\\Python312\\python.exe".to_string(),
            format!("{}\\AppData\\Local\\Programs\\Python\\Python312\\python.exe", home),
            format!("{}\\AppData\\Local\\Programs\\Python\\Python311\\python.exe", home),
            format!("{}\\AppData\\Local\\Programs\\Python\\Python310\\python.exe", home),
            format!("{}\\anaconda3\\python.exe", home),
            format!("{}\\miniconda3\\python.exe", home),
            "C:\\Python312\\python.exe".to_string(),
            "C:\\Python311\\python.exe".to_string(),
        ];

        for p in &direct_paths {
            if std::path::Path::new(p).exists() {
                return Some(p.clone());
            }
        }

        // Fallback: try PATH commands (slower, spawns process)
        use std::os::windows::process::CommandExt;
        for cmd in ["python", "py"] {
            if let Ok(out) = Command::new(cmd)
                .arg("--version")
                .stdout(Stdio::null()).stderr(Stdio::null())
                .creation_flags(0x08000000)
                .status()
            {
                if out.success() { return Some(cmd.to_string()); }
            }
        }

        // Scan LOCALAPPDATA
        let base = std::path::PathBuf::from(&local).join("Programs").join("Python");
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let py = entry.path().join("python.exe");
                if py.exists() { return Some(py.to_string_lossy().to_string()); }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // macOS/Linux: check common paths by file existence.
        // Probe the versioned python3.12 FIRST (both the keg-only symlink and
        // the direct keg path) — that is exactly what install-mac.sh guarantees
        // via `brew install python@3.12`. Bare `python3` may resolve to brew's
        // current default (3.13/3.14) or CLT's 3.9, drifting from the pinned
        // 3.12 the packages were installed for.
        let home = std::env::var("HOME").unwrap_or_default();
        let paths = [
            "/opt/homebrew/bin/python3.12",
            "/opt/homebrew/opt/python@3.12/bin/python3.12",
            "/usr/local/bin/python3.12",
            "/usr/local/opt/python@3.12/bin/python3.12",
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
            "/usr/bin/python3",
            &format!("{}/anaconda3/bin/python", home),
            &format!("{}/miniconda3/bin/python", home),
            &format!("{}/miniforge3/bin/python", home),
        ];
        for p in paths {
            if std::path::Path::new(p).exists() { return Some(p.to_string()); }
        }
    }

    None
}

fn build_python_cmd(dir: &Path, filename: &str, python_path: Option<&str>) -> Option<(String, Vec<String>)> {
    let py = find_python(python_path)?;
    let file = dir.join(filename);
    Some((py, vec![file.to_string_lossy().to_string()]))
}

fn build_node_cmd(dir: &Path, filename: &str) -> Option<(String, Vec<String>)> {
    let file = dir.join(filename);
    Some(("node".to_string(), vec![file.to_string_lossy().to_string()]))
}

/// A compiler invocation that is about to run, plus everything the RUN step
/// needs afterwards.
struct NativeBuild {
    exe: std::path::PathBuf,
    /// Directory holding the compiler, put on the child's PATH so a
    /// dynamically-linked MinGW binary can find libstdc++/libgcc/libwinpthread.
    bin_dir: Option<std::path::PathBuf>,
}

/// Outcome of waiting for a compile.
enum CompileWait {
    Done(std::process::ExitStatus),
    /// Ran past `COMPILE_TIMEOUT` and was killed.
    TimedOut,
    /// The student pressed Stop (or a newer run took the slot).
    Cancelled,
}

/// Wait for a compile that lives in the SHARED process slot.
///
/// The compiler child is published under the run generation exactly like the
/// program is, which is what makes Stop work during a compile: `stop_code`
/// takes it out of the slot and kills it, and the next poll here sees the slot
/// no longer holds this generation.
///
/// `std::process::Child` has no timed wait, so this polls; 20 ms is short
/// enough that a normal sub-second compile is not measurably delayed.
fn wait_compile_in_slot(
    process_handle: &RunningProcess,
    my_id: u64,
    limit: Duration,
) -> CompileWait {
    let start = Instant::now();
    loop {
        {
            let mut guard = match process_handle.lock() {
                Ok(g) => g,
                Err(_) => return CompileWait::Cancelled,
            };
            match guard.as_mut() {
                Some((id, child)) if *id == my_id => match child.try_wait() {
                    Ok(Some(status)) => return CompileWait::Done(status),
                    Ok(None) => {
                        if start.elapsed() >= limit {
                            let _ = child.kill();
                            let _ = child.wait();
                            return CompileWait::TimedOut;
                        }
                    }
                    Err(_) => return CompileWait::Cancelled,
                },
                // Someone took our child: Stop, or a newer run.
                _ => return CompileWait::Cancelled,
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);
/// Compiler diagnostics beyond this are truncated. One bad `std::` type error
/// can emit megabytes of template backtrace; the panel only needs the head.
const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;

/// The result of one compiler invocation.
enum CompileAttempt {
    /// Compiled and linked. Carries any warnings the compiler printed.
    Success { warnings: String },
    /// The compiler rejected the program.
    Failed { diagnostics: String, code: Option<i32> },
    /// Ran past `COMPILE_TIMEOUT` and was killed.
    TimedOut,
    /// The student pressed Stop.
    Cancelled,
    /// The compiler binary itself could not be started.
    SpawnError(String),
}

/// Run one planned compile, publishing the compiler under `my_id` so a Stop
/// pressed mid-compile reaches it.
fn run_compile_attempt(
    spec: &crate::toolchain::CompileSpec,
    process_handle: &RunningProcess,
    my_id: u64,
) -> CompileAttempt {
    let mut command = Command::new(&spec.compiler);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::toolchain::compile_env(&mut command);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return CompileAttempt::SpawnError(e.to_string()),
    };

    // Drain the pipes on threads. A compiler that fills the 64 KB stderr pipe
    // buffer while we sit polling would deadlock: it blocks on write, we block
    // waiting for it to exit. Template-heavy C++ produces that much output
    // routinely.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let t_out = thread::spawn(move || read_capped(&mut out_pipe, MAX_DIAGNOSTIC_BYTES));
    let t_err = thread::spawn(move || read_capped(&mut err_pipe, MAX_DIAGNOSTIC_BYTES));

    // Publish the compiler under this run's generation so Stop can kill it.
    {
        let mut guard = match process_handle.lock() {
            Ok(g) => g,
            Err(_) => {
                let _ = child.kill();
                return CompileAttempt::Cancelled;
            }
        };
        *guard = Some((my_id, child));
    }

    let wait = wait_compile_in_slot(process_handle, my_id, COMPILE_TIMEOUT);

    // Take the compiler back out of the slot so the program can take its place.
    // If Stop already took it, this is a no-op.
    if let Ok(mut guard) = process_handle.lock() {
        let ours = matches!(guard.as_ref(), Some((id, _)) if *id == my_id);
        if ours {
            *guard = None;
        }
    }

    let compile_out = join_bounded(t_out, Duration::from_secs(5)).unwrap_or_default();
    let compile_err = join_bounded(t_err, Duration::from_secs(5)).unwrap_or_default();
    let combined = format!("{}{}", compile_err, compile_out);

    match wait {
        CompileWait::Done(status) if status.success() => CompileAttempt::Success { warnings: combined },
        CompileWait::Done(status) => CompileAttempt::Failed {
            diagnostics: combined,
            code: status.code(),
        },
        CompileWait::TimedOut => CompileAttempt::TimedOut,
        CompileWait::Cancelled => CompileAttempt::Cancelled,
    }
}

/// Compile the active file together with its sibling translation units and
/// return the executable to run.
///
/// Everything here exists because of a specific failure of the previous
/// one-liner (`g++ file.cpp -o a.exe`):
///
/// * `a.exe` was written INTO the workspace, so it entered the submission zip
///   and had to be special-cased in the integrity monitor. Output now lives in
///   a per-workspace build directory outside the workspace entirely.
/// * A second source file was never compiled, so any multi-file assignment
///   failed to link with what looks like the student's own error.
/// * The compiler was invoked as bare `g++`, so on Windows — where nothing puts
///   a compiler on PATH — every C++ Run failed. Discovery now finds the pinned
///   portable toolchain by path.
/// * The compile ran with the workspace as an argument path. A Korean Windows
///   username makes that path non-ASCII and MinGW's driver is not reliably
///   unicode-clean; the compile now runs WITH THE SOURCE DIRECTORY AS CWD and
///   passes plain relative filenames, so the unicode part of the path is
///   resolved by the OS rather than parsed by the compiler.
fn build_native(
    dir: &Path,
    filename: &str,
    cpp: bool,
    app: &AppHandle,
    process_handle: &RunningProcess,
    my_id: u64,
) -> Option<NativeBuild> {
    let cfg = crate::setup::load_config();
    let (explicit, standard) = if cpp {
        (
            cfg.cpp_compiler_path.clone(),
            crate::toolchain::normalize_standard(cfg.cpp_standard.as_deref().unwrap_or("")),
        )
    } else {
        (
            cfg.c_compiler_path.clone(),
            crate::toolchain::normalize_c_standard(cfg.c_standard.as_deref().unwrap_or("")),
        )
    };

    let compiler = match crate::toolchain::find_compiler(explicit.as_deref(), cpp) {
        Some(c) => c,
        None => {
            let lang = if cpp { "C++" } else { "C" };
            emit_line(app, "stderr", &format!(
                "[{} 컴파일러를 찾을 수 없습니다]\n\
                 설치 스크립트(install-windows.ps1 / install-mac.sh / install-linux.sh)를 \
                 다시 실행하면 컴파일러가 설치됩니다.\n\
                 이미 설치되어 있다면 하단 상태바의 'C++' 항목에서 경로를 직접 지정하세요.\n",
                lang
            ));
            emit_done_with_output(app, None, 0, "", "");
            return None;
        }
    };

    // A header is not a program. Compiling one succeeds (GCC quietly builds a
    // precompiled header) and produces no executable, so without this guard the
    // student got "compiled but no executable was produced" — technically true
    // and completely unhelpful.
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(ext.as_str(), "h" | "hpp" | "hh" | "hxx" | "inc") {
        emit_line(app, "stderr", &format!(
            "[{} 은 헤더 파일이라 단독 실행할 수 없습니다]\n\
             main 함수가 있는 .cpp 파일을 열고 Run 하세요. 같은 폴더의 파일은 자동으로 함께 컴파일됩니다.\n",
            filename
        ));
        emit_done_with_output(app, None, 0, "", "");
        return None;
    }

    let src_abs = dir.join(filename);
    let build_dir = crate::toolchain::build_dir_for_workspace(dir);
    if let Err(e) = std::fs::create_dir_all(&build_dir) {
        emit_line(app, "stderr", &format!("빌드 폴더를 만들지 못했습니다 ({}): {}\n", build_dir.display(), e));
        emit_done_with_output(app, None, 0, "", "");
        return None;
    }

    // Remove the previous binary so a failed link can never leave the OLD
    // program runnable — a student who fixes a syntax error, re-runs, and sees
    // the previous build's output would be debugging a ghost. If the file is
    // locked (a previous run still exiting, or an antivirus scanning it), fall
    // back to a fresh name rather than failing the whole Run.
    let mut exe = build_dir.join(crate::toolchain::exe_name_for(&src_abs));
    if exe.exists() && std::fs::remove_file(&exe).is_err() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let alt = format!(
            "{}_{}{}",
            exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "program".into()),
            stamp,
            if cfg!(windows) { ".exe" } else { "" }
        );
        exe = build_dir.join(alt);
    }

    let spec = crate::toolchain::plan_compile(dir, filename, cpp, &compiler, &standard, &exe);

    if spec.units.len() > 1 {
        let extra: Vec<&str> = spec.units.iter().skip(1).map(|s| s.as_str()).collect();
        emit_line(app, "system", &format!(
            "$ Compiling {} (+{} linked: {})\n",
            spec.units.first().map(|s| s.as_str()).unwrap_or(filename),
            extra.len(),
            extra.join(", ")
        ));
    } else {
        emit_line(app, "system", &format!("$ Compiling {}\n", spec.units.first().map(|s| s.as_str()).unwrap_or(filename)));
    }

    let compile_start = Instant::now();
    let mut attempt = run_compile_attempt(&spec, process_handle, my_id);

    // Auto-linking every sibling is right for the assignment layout it exists
    // for (main.cpp + utils.cpp + utils.h), but a scratch file that happens to
    // define the same helper turns into "multiple definition of ...". That is a
    // link error the student did not cause and cannot act on mid-exam, so fall
    // back to the file they actually asked to run and say so.
    if spec.units.len() > 1 {
        if let CompileAttempt::Failed { ref diagnostics, .. } = attempt {
            if diagnostics.contains("multiple definition of") {
                emit_line(app, "system",
                    "\n같은 폴더의 다른 파일과 함수가 중복되어, 현재 파일만 단독 컴파일합니다.\n");
                let solo = crate::toolchain::plan_compile_units(
                    dir, filename, cpp, &compiler, &standard, &exe, 1,
                );
                attempt = run_compile_attempt(&solo, process_handle, my_id);
            }
        }
    }

    match attempt {
        CompileAttempt::SpawnError(e) => {
            emit_line(app, "stderr", &format!("컴파일러를 실행하지 못했습니다 ({}): {}\n", compiler, e));
            emit_done_with_output(app, None, 0, "", "");
            return None;
        }
        CompileAttempt::TimedOut => {
            emit_line(app, "stderr", &format!(
                "[컴파일 시간 초과 — {}초]\n무한 템플릿 재귀나 지나치게 큰 소스가 아닌지 확인하세요.\n",
                COMPILE_TIMEOUT.as_secs()
            ));
            emit_done_with_output(app, None, compile_start.elapsed().as_millis() as u64, "", "");
            return None;
        }
        // Stop already took our compiler child and the frontend already reset
        // itself. Emitting run-done here would fire a second completion for a
        // run that never started — the misattributed-run-done class of bug the
        // generation checks exist to prevent.
        CompileAttempt::Cancelled => {
            take_cancelled(my_id);
            return None;
        }
        CompileAttempt::Failed { diagnostics, code } => {
            emit_line(app, "stderr", "[Compilation Error]\n");
            if !diagnostics.is_empty() {
                emit_line(app, "stderr", &diagnostics);
            }
            // Two link failures are common enough to deserve a plain-language hint.
            if diagnostics.contains("undefined reference to `main'")
                || diagnostics.contains("undefined reference to 'main'")
                || diagnostics.contains("WinMain")
                || diagnostics.contains("_main")
            {
                emit_line(app, "system", "\n힌트: main 함수가 있는 파일을 선택한 뒤 Run 하세요.\n");
            } else if diagnostics.contains("Permission denied")
                || diagnostics.contains("cannot open output file")
            {
                emit_line(app, "system", "\n힌트: 이전 실행이 아직 종료되지 않았습니다. Stop 후 다시 Run 하세요.\n");
            }
            emit_done_with_output(app, code, compile_start.elapsed().as_millis() as u64, "", "");
            return None;
        }
        CompileAttempt::Success { warnings } => {
            // Warnings on a SUCCESSFUL compile still matter to a student, so
            // show them — but clearly labelled so they are not mistaken for the
            // program's own output.
            if !warnings.trim().is_empty() {
                emit_line(app, "stderr", &format!("[Compiler Warnings]\n{}\n", warnings.trim_end()));
            }
        }
    }

    // A Stop that landed between the compiler exiting and this line must still
    // stop the program from starting.
    if take_cancelled(my_id) {
        return None;
    }

    if !exe.exists() {
        emit_line(app, "stderr", "컴파일은 성공했다고 보고됐지만 실행 파일이 생성되지 않았습니다.\n");
        emit_done_with_output(app, None, compile_start.elapsed().as_millis() as u64, "", "");
        return None;
    }

    emit_line(app, "system", &format!("$ Compiled in {} ms\n", compile_start.elapsed().as_millis()));

    Some(NativeBuild {
        exe,
        bin_dir: crate::toolchain::compiler_bin_dir(&compiler),
    })
}

/// Read a pipe to EOF with a hard byte cap, decoding lossily.
fn read_capped<R: std::io::Read>(pipe: &mut Option<R>, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    if let Some(p) = pipe.as_mut() {
        let mut chunk = [0u8; 16384];
        loop {
            match p.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < cap {
                        let room = cap - buf.len();
                        buf.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    // Keep draining past the cap so the writer never blocks.
                }
                Err(_) => break,
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn build_and_run_c(
    dir: &Path,
    filename: &str,
    app: &AppHandle,
    process_handle: &RunningProcess,
    my_id: u64,
) -> Option<(String, Vec<String>, Option<std::path::PathBuf>)> {
    let b = build_native(dir, filename, false, app, process_handle, my_id)?;
    Some((b.exe.to_string_lossy().to_string(), vec![], b.bin_dir))
}

fn build_and_run_cpp(
    dir: &Path,
    filename: &str,
    app: &AppHandle,
    process_handle: &RunningProcess,
    my_id: u64,
) -> Option<(String, Vec<String>, Option<std::path::PathBuf>)> {
    let b = build_native(dir, filename, true, app, process_handle, my_id)?;
    Some((b.exe.to_string_lossy().to_string(), vec![], b.bin_dir))
}

fn build_and_run_java(dir: &Path, filename: &str, app: &AppHandle) -> Option<(String, Vec<String>)> {
    let src = dir.join(filename);
    let src_str = src.to_string_lossy().to_string();
    let dir_str = dir.to_string_lossy().to_string();

    let mut javac = Command::new("javac");
    javac.arg(&src_str);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        javac.creation_flags(0x08000000);
    }
    let output = javac.output();
    match output {
        Ok(o) if !o.status.success() => {
            emit_line(app, "stderr", "[Compilation Error]\n");
            emit_line(app, "stderr", &String::from_utf8_lossy(&o.stderr));
            emit_done_with_output(app, o.status.code(), 0, "", "");
            return None;
        }
        Err(e) => {
            emit_line(app, "stderr", &format!("Failed to run 'javac': {}\n", e));
            emit_done_with_output(app, None, 0, "", "");
            return None;
        }
        _ => {}
    }

    let basename = filename.rsplit('/').next().unwrap_or(filename);
    let class_name = basename.trim_end_matches(".java");
    Some(("java".to_string(), vec!["-cp".to_string(), dir_str, class_name.to_string()]))
}

/// Tests for the standard-input plumbing.
///
/// These exist because the first version of this feature closed stdin before
/// `child.wait()` rather than after it. That is a one-line ordering mistake
/// that compiles, type-checks, passes every static check — and hands the
/// program an immediate EOF, so `std::cin >> n` reads nothing and an exam
/// answer prints a confident wrong result. Only running a real child process
/// catches it.
#[cfg(test)]
mod stdin_tests {
    use super::*;
    use std::io::Read;

    /// Build a tiny program that echoes what it reads, and hand back its path.
    /// Returns None when the machine has no C++ compiler (CI on a bare runner).
    fn build_echo_program(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        crate::toolchain::clear_compiler_cache();
        let compiler = crate::toolchain::find_compiler(None, true)?;
        std::fs::create_dir_all(dir).ok()?;
        let src = dir.join("echo.cpp");
        std::fs::write(
            &src,
            "#include <iostream>\n#include <string>\n\
             int main(){ std::string line; long long n=0;\n\
             while (std::getline(std::cin, line)) { ++n; std::cout << \"got:\" << line << std::endl; }\n\
             std::cout << \"eof:\" << n << std::endl; return 0; }\n",
        )
        .ok()?;
        let exe = dir.join(if cfg!(windows) { "echo.exe" } else { "echo" });
        let spec = crate::toolchain::plan_compile(
            dir,
            "echo.cpp",
            true,
            &compiler,
            crate::toolchain::DEFAULT_CPP_STANDARD,
            &exe,
        );
        let (ok, diag) = spec.run();
        assert!(ok, "test fixture failed to compile: {}", diag);
        Some(exe)
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mint-stdin-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stdin_survives_until_the_program_exits() {
        let dir = scratch("survives");
        let exe = match build_echo_program(&dir) {
            Some(e) => e,
            None => {
                eprintln!("SKIP stdin_survives_until_the_program_exits: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };

        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn echo program");

        let mut out = child.stdout.take().unwrap();
        let handle: RunningStdin = new_running_stdin();
        *handle.lock().unwrap() = Some((1, child.stdin.take().unwrap()));

        // Deliver two lines with a pause between them. If stdin were closed at
        // start-up the program would already be at EOF and the second write
        // would report "not delivered".
        assert!(write_stdin(&handle, "alpha\n").unwrap(), "first line must be delivered");
        thread::sleep(Duration::from_millis(120));
        assert!(
            write_stdin(&handle, "beta\n").unwrap(),
            "stdin must still be open while the program runs"
        );

        // EOF is what ends a `while (getline(...))` loop.
        assert!(close_stdin(&handle), "close_stdin should report it closed something");

        let status = child.wait().expect("wait");
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();

        assert!(text.contains("got:alpha"), "output was: {}", text);
        assert!(text.contains("got:beta"), "output was: {}", text);
        assert!(text.contains("eof:2"), "program must see EXACTLY two lines: {}", text);
        assert!(status.success());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_multi_line_paste_arrives_in_order() {
        let dir = scratch("paste");
        let exe = match build_echo_program(&dir) {
            Some(e) => e,
            None => {
                eprintln!("SKIP a_multi_line_paste_arrives_in_order: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };

        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let mut out = child.stdout.take().unwrap();
        let handle: RunningStdin = new_running_stdin();
        *handle.lock().unwrap() = Some((7, child.stdin.take().unwrap()));

        // The UI sends a pasted block as ONE call precisely so ordering cannot
        // be interleaved by the student typing while it is being consumed.
        assert!(write_stdin(&handle, "1\n2\n3\n").unwrap());
        close_stdin(&handle);
        child.wait().unwrap();

        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        let got: Vec<&str> = text.lines().filter(|l| l.starts_with("got:")).collect();
        assert_eq!(got, vec!["got:1", "got:2", "got:3"], "full output: {}", text);
    }

    #[test]
    fn writing_after_the_program_exits_reports_undelivered_not_an_error() {
        let dir = scratch("gone");
        let exe = match build_echo_program(&dir) {
            Some(e) => e,
            None => {
                eprintln!("SKIP writing_after_the_program_exits_reports_undelivered_not_an_error: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };

        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let handle: RunningStdin = new_running_stdin();
        *handle.lock().unwrap() = Some((9, child.stdin.take().unwrap()));

        close_stdin(&handle);
        child.wait().unwrap();

        // Typing into a program that has finished is an ordinary thing to do by
        // accident; it must read as "not delivered", never as a failure the
        // student has to interpret.
        assert_eq!(write_stdin(&handle, "late\n").unwrap(), false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_run_means_nothing_is_delivered() {
        let handle: RunningStdin = new_running_stdin();
        assert_eq!(write_stdin(&handle, "x\n").unwrap(), false);
        assert!(!close_stdin(&handle));
    }

    #[test]
    fn a_stale_generation_cannot_close_the_current_runs_stdin() {
        let handle: RunningStdin = new_running_stdin();
        let dir = scratch("gen");
        let exe = match build_echo_program(&dir) {
            Some(e) => e,
            None => {
                eprintln!("SKIP a_stale_generation_cannot_close_the_current_runs_stdin: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        *handle.lock().unwrap() = Some((42, child.stdin.take().unwrap()));

        // A straggler thread from run 41 must not close run 42's stdin.
        clear_stdin_generation(&handle, 41);
        assert!(
            write_stdin(&handle, "still open\n").unwrap(),
            "a stale generation must not have closed the current stdin"
        );

        clear_stdin_generation(&handle, 42);
        assert_eq!(write_stdin(&handle, "now closed\n").unwrap(), false);

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_request_is_recorded_and_consumed_once() {
        let gen = RUN_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        note_stop_request();
        assert!(take_cancelled(gen), "the stop must be visible to the run it targets");
        assert!(!take_cancelled(gen), "a cancellation must only fire once");
    }
}
