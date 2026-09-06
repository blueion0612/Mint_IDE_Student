use serde::Serialize;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Runtime};

// The run pipeline is generic over the Tauri runtime rather than hard-wired to
// the default one. Production still passes the real handle and infers `R`;
// tests pass `MockRuntime`, which is what makes it possible to drive a whole
// run — compile, spawn, stdin, events — and assert on what the frontend would
// have received. Without this the pipeline could only be tested a piece at a
// time, and the mistakes that matter here live in how the pieces are ordered.

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
/// Largest single `run-output` event. Matches what one 64 KB read used to
/// produce, so a flooding program cannot hand the webview one huge payload.
const MAX_EMIT_CHUNK_BYTES: usize = 64 * 1024;

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

/// Where a run's output goes.
///
/// In the app this is the Tauri `AppHandle`, which turns each call into the
/// `run-output` / `run-done` events the frontend listens for. Making it a trait
/// costs one virtual call per emit — nothing against a process spawn — and buys
/// the ability to drive a COMPLETE run in a test: compile a real file, spawn a
/// real child, feed its stdin, and assert on exactly what the frontend would
/// have been told. That is the only level at which an ordering mistake shows
/// up, and the alternative (Tauri's mock runtime) links a windowing runtime
/// into the test binary, which does not load on every machine.
pub trait RunEvents: Send + Sync + 'static {
    fn line(&self, stream: &str, text: &str);
    fn done(&self, exit_code: Option<i32>, duration_ms: u64, stdout: &str, stderr: &str);
}

/// Shared handle to a run's event sink.
pub type Events = Arc<dyn RunEvents>;

#[derive(Clone, Serialize)]
struct RunDone {
    exit_code: Option<i32>,
    duration_ms: u64,
    stdout: String,
    stderr: String,
}

impl<R: Runtime> RunEvents for AppHandle<R> {
    fn line(&self, stream: &str, text: &str) {
        let _ = self.emit(
            "run-output",
            RunOutputLine {
                stream: stream.to_string(),
                text: text.to_string(),
            },
        );
    }

    fn done(&self, exit_code: Option<i32>, duration_ms: u64, stdout: &str, stderr: &str) {
        let _ = self.emit(
            "run-done",
            RunDone {
                exit_code,
                duration_ms,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            },
        );
    }
}

/// Wrap a Tauri handle as a run event sink.
pub fn events_from_handle<R: Runtime>(handle: AppHandle<R>) -> Events {
    Arc::new(handle)
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

/// Standard input for the run in flight.
///
/// Before this existed the child inherited the IDE's stdin. A GUI process has
/// no console, so that handle was invalid and every read hit EOF immediately:
/// `std::cin >> n` left `n` untouched and `input()` raised EOFError. Exam
/// problems overwhelmingly read their input from stdin, so C++ (and Python)
/// answers produced confidently wrong output with no visible error.
///
/// Two things make this more than a pipe.
///
/// A run does not have a pipe for its whole life. A C++ Run spends its first
/// half-second to second COMPILING, and a student who knows their program wants
/// input starts typing immediately — exactly as they would into a terminal.
/// Those keystrokes are held here and delivered the moment the program exists.
///
/// And writing to a pipe BLOCKS once the OS buffer (64 KB on Windows) is full.
/// A program that stops reading — one that crashed, or only wanted the first
/// line — plus a student pasting a large test input is enough to block the
/// writer forever. If that write happened on the command thread it would freeze
/// the UI, and if it held this lock it would also hang the Stop that is the
/// student's way out. So the pipe is owned by a dedicated writer thread and fed
/// through a channel; nothing on a UI path ever blocks on it.
enum StdinMsg {
    Data(Vec<u8>),
    Close,
}

#[derive(Default)]
pub struct StdinState {
    /// The generation of the run in flight, from its start until it finishes.
    /// `None` means nothing is running and input has nowhere to go.
    active_gen: Option<u64>,
    /// Channel to the writer thread, once the program has been spawned.
    tx: Option<(u64, std::sync::mpsc::Sender<StdinMsg>)>,
    /// Type-ahead recorded before the program existed.
    pending: Vec<u8>,
    /// EOF pressed before the program existed; applied once the pipe appears.
    eof_requested: bool,
    /// Bytes handed to the writer thread and not yet written. Bounds the queue
    /// for a program that has stopped reading.
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

pub type RunningStdin = Arc<Mutex<StdinState>>;

pub fn new_running_stdin() -> RunningStdin {
    Arc::new(Mutex::new(StdinState::default()))
}

/// Ceiling on unconsumed input, whether waiting for the program to start or
/// waiting for it to read. Far past any exam input; a student holding a key
/// down or pasting a runaway file cannot grow this without limit.
const MAX_UNCONSUMED_STDIN: usize = 1 << 20; // 1 MB

/// Open a run's input. Called when the run is claimed, BEFORE any compile, so
/// type-ahead has somewhere to go.
fn begin_stdin_run(handle: &RunningStdin, gen: u64) {
    if let Ok(mut st) = handle.lock() {
        st.active_gen = Some(gen);
        st.tx = None;
        st.pending.clear();
        st.eof_requested = false;
        st.queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    }
}

/// Hand the program's pipe to a writer thread and flush whatever the student
/// typed while it was still compiling.
fn publish_stdin_pipe(handle: &RunningStdin, gen: u64, pipe: Option<std::process::ChildStdin>) {
    let pipe = match pipe {
        Some(p) => p,
        None => return,
    };
    let mut st = match handle.lock() {
        Ok(st) => st,
        Err(_) => return,
    };
    // A newer run already owns the state: this pipe belongs to a run that was
    // superseded, so let it drop (closing it) rather than crossing the wires.
    if st.active_gen != Some(gen) {
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel::<StdinMsg>();
    let queued = st.queued.clone();
    thread::spawn(move || {
        use std::io::Write;
        let mut pipe = pipe;
        while let Ok(msg) = rx.recv() {
            match msg {
                StdinMsg::Data(bytes) => {
                    let n = bytes.len();
                    let res = pipe.write_all(&bytes).and_then(|_| pipe.flush());
                    queued.fetch_sub(n.min(queued.load(std::sync::atomic::Ordering::Relaxed)),
                                     std::sync::atomic::Ordering::Relaxed);
                    if res.is_err() {
                        break; // the program closed its end
                    }
                }
                StdinMsg::Close => break,
            }
        }
        // Dropping `pipe` closes it, which is the EOF a reader loop waits for.
    });

    if !st.pending.is_empty() {
        let pending = std::mem::take(&mut st.pending);
        st.queued.fetch_add(pending.len(), std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(StdinMsg::Data(pending));
    }
    if st.eof_requested {
        let _ = tx.send(StdinMsg::Close);
        return; // `tx` drops here, so nothing more can be queued
    }
    st.tx = Some((gen, tx));
}

/// Feed a chunk to the running program's stdin.
///
/// Returns `Ok(true)` when the bytes were queued (for the program, or as
/// type-ahead), `Ok(false)` when nothing is running, EOF has been sent, or the
/// program has closed its end. A finished or non-reading program is not a
/// failure the student should have to interpret.
///
/// Never blocks: the actual write happens on the writer thread.
pub fn write_stdin(handle: &RunningStdin, text: &str) -> Result<bool, String> {
    use std::sync::atomic::Ordering;
    let mut st = handle.lock().map_err(|_| "stdin state poisoned".to_string())?;

    if st.eof_requested && st.tx.is_none() {
        // EOF was already sent for this run; further input would never be read.
        return Ok(false);
    }

    if let Some((_, tx)) = st.tx.as_ref() {
        if st.queued.load(Ordering::Relaxed) + text.len() > MAX_UNCONSUMED_STDIN {
            // The program is not reading. Refusing is the honest answer, and
            // the frontend turns it into "this program is not taking input".
            return Ok(false);
        }
        st.queued.fetch_add(text.len(), Ordering::Relaxed);
        return match tx.send(StdinMsg::Data(text.as_bytes().to_vec())) {
            Ok(()) => Ok(true),
            // The writer thread is gone, which means the program closed its end.
            Err(_) => {
                st.queued.fetch_sub(text.len(), Ordering::Relaxed);
                st.tx = None;
                Ok(false)
            }
        };
    }

    // No pipe yet. If a run is in flight it is still compiling, so hold the
    // keystrokes — that is what a terminal does.
    if st.active_gen.is_some() {
        if st.pending.len() + text.len() <= MAX_UNCONSUMED_STDIN {
            st.pending.extend_from_slice(text.as_bytes());
            return Ok(true);
        }
        return Ok(false);
    }

    Ok(false)
}

/// Close stdin, signalling EOF.
///
/// Essential for C++: `while (std::cin >> x)` and `while (getline(...))`
/// terminate ONLY on EOF, so this is what lets a student finish an input-loop
/// program at all. Pressing it before the program has been spawned is recorded
/// and applied as soon as it is.
pub fn close_stdin(handle: &RunningStdin) -> bool {
    match handle.lock() {
        Ok(mut st) => {
            if let Some((_, tx)) = st.tx.take() {
                // Queued input is written first; the writer then closes the
                // pipe. Dropping `tx` alone would also do it, but sending Close
                // makes the intent explicit and survives a full queue.
                let _ = tx.send(StdinMsg::Close);
                return true;
            }
            if st.active_gen.is_some() && !st.eof_requested {
                st.eof_requested = true;
                return true;
            }
            false
        }
        Err(_) => false,
    }
}

/// Finish a run's input: close the pipe and forget any type-ahead.
///
/// Generation-checked for the same reason the child reaper is: a run that has
/// already been replaced must not close the CURRENT run's stdin.
fn clear_stdin_generation(handle: &RunningStdin, gen: u64) {
    if let Ok(mut st) = handle.lock() {
        if st.active_gen == Some(gen) {
            st.active_gen = None;
            // Dropping the sender ends the writer thread, which closes the pipe.
            st.tx = None;
            st.pending.clear();
            st.eof_requested = false;
        }
    }
}

/// The generation currently occupying the process slot, if any.
///
/// Introspection for the tests: the Stop-during-compile test has to wait until
/// the compiler is genuinely published before pressing Stop, or it would be
/// testing a race rather than the behaviour.
#[allow(dead_code)]
pub fn current_slot_generation(process_handle: &RunningProcess) -> Option<u64> {
    process_handle.lock().ok().and_then(|g| g.as_ref().map(|(id, _)| *id))
}

/// Whether the running program's stdin pipe exists yet. `false` while a
/// compiled language is still compiling.
///
/// Introspection for the tests: the type-ahead tests have to be able to say
/// "the program provably does not exist yet" before writing, or they would pass
/// for the wrong reason.
#[allow(dead_code)]
pub fn stdin_pipe_ready(handle: &RunningStdin) -> bool {
    handle.lock().map(|st| st.tx.is_some()).unwrap_or(false)
}

/// Wait for the program to finish, THEN release its input.
///
/// The ordering is the whole point, so it lives in one function rather than as
/// two adjacent statements someone could reorder. Closing stdin before the wait
/// hands the program an immediate EOF: `std::cin >> n` reads nothing, `input()`
/// raises, and an exam answer prints a confident wrong result with no error
/// anywhere. `wait()` is what blocks for the life of the program, and that is
/// exactly the window in which the output panel's input box must be able to
/// feed it. Reintroducing the swap fails six of the end-to-end tests at the
/// bottom of this file.
fn wait_program_then_release_stdin(
    process_handle: &RunningProcess,
    stdin_handle: &RunningStdin,
    gen: u64,
) -> (bool, Option<i32>) {
    // Waits IN the slot, so the child remains findable — and therefore
    // killable — by Stop, by the runaway-output auto-stop and by the
    // kill-on-exit hook for as long as it runs.
    let outcome = wait_child_in_slot(process_handle, gen, None, Duration::from_millis(25));
    release_slot(process_handle, gen);
    let result = match outcome {
        SlotWait::Exited(status) => (true, status.code()),
        // Stopped, superseded, or (with no limit) unreachable.
        _ => (false, None),
    };
    clear_stdin_generation(stdin_handle, gen);
    result
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
    app_handle: Events,
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

    // Claim the run generation and open the run's input SYNCHRONOUSLY, before
    // the worker thread starts.
    //
    // Both used to happen inside the thread, which left a window between
    // `run_code` returning to the frontend - where the input row appears - and
    // the run existing at all. Anything typed in that window was rejected, and
    // the frontend reads a rejection as "this program is not reading input",
    // latching the box shut for the rest of the run. The window is short, but a
    // student who knows their program wants input types into it immediately,
    // and a loaded machine widens it arbitrarily.
    //
    // Claiming the generation here is also more honest: it belongs to the
    // invocation, not to the thread that services it. The compiler child is
    // published under this same id, which is what lets Stop cancel a compile.
    let my_id = RUN_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    begin_stdin_run(&stdin_handle, my_id);

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
            None => {
                // Compile error, or a Stop during the compile. Either way no
                // program will run, so release the input state; leaving it
                // "active" would silently swallow the next thing typed.
                clear_stdin_generation(&stdin_handle, my_id);
                return;
            }
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
                clear_stdin_generation(&stdin_handle, my_id);
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
                clear_stdin_generation(&stdin_handle, my_id);
                return;
            }
            *guard = Some((my_id, child));
        }
        // Hand over the pipe under the SAME generation, flushing whatever was
        // typed while the compile was still running.
        publish_stdin_pipe(&stdin_handle, my_id, child_stdin);

        let ah1 = app_handle.clone();

        let stdout_collected = Arc::new(Mutex::new(String::new()));
        let stderr_collected = Arc::new(Mutex::new(String::new()));
        // stdout's copy is taken by the flusher, which is the only thing that
        // emits it; the reader no longer touches it.
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
        // stdout is read by one thread and EMITTED by another.
        //
        // The reader alone used to do both, flushing its batch buffer only on
        // the way round the loop — that is, only when the NEXT read returned.
        // A program that prints a prompt and then blocks on input therefore
        // showed the student nothing: the prompt sat in the buffer until more
        // output arrived or the program exited. That is precisely the shape of
        // an interactive exam program ("이름을 입력하세요: " then getline), and
        // it was invisible until stdin existed to make such programs possible.
        // (The old code had a 50 ms escape hatch that usually hid this, because
        // process start-up took longer than the interval — on a fast machine it
        // did not, and the prompt vanished.)
        //
        // A dedicated flusher on a ~50 ms tick keeps the batching that stops an
        // event flood while bounding how long any byte can wait. Only the
        // flusher emits, so ordering is preserved.
        let emit_state: Arc<(Mutex<(String, bool)>, Condvar)> =
            Arc::new((Mutex::new((String::new(), false)), Condvar::new()));

        let flush_state = emit_state.clone();
        let ah_flush = app_handle.clone();
        let sc_flush = stdout_collected.clone();
        let t_flush = thread::spawn(move || {
            let (lock, cv) = &*flush_state;
            loop {
                let (text, done) = {
                    let guard = lock.lock().unwrap();
                    // Wake on the tick, or early when the reader signals a full
                    // batch or the end of the stream.
                    let (mut guard, _) = cv
                        .wait_timeout(guard, Duration::from_millis(EMIT_BATCH_INTERVAL_MS))
                        .unwrap();
                    (std::mem::take(&mut guard.0), guard.1)
                };
                if !text.is_empty() {
                    push_capped(&sc_flush, STDOUT_BUF_CAP_BYTES, &text);
                    // One emit per event stays bounded. The reader can hand
                    // over megabytes between ticks when a program floods, and
                    // the webview should not be given a multi-megabyte payload
                    // in one go — the old synchronous flush was naturally
                    // capped at one 64 KB read, so keep that shape.
                    let mut rest: &str = &text;
                    while !rest.is_empty() {
                        let mut cut = MAX_EMIT_CHUNK_BYTES.min(rest.len());
                        while cut > 0 && !rest.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        if cut == 0 {
                            cut = rest.len(); // a single char longer than the cap
                        }
                        emit_line(&ah_flush, "stdout", &rest[..cut]);
                        rest = &rest[cut..];
                    }
                }
                if done {
                    break;
                }
            }
        });

        let t1 = thread::spawn(move || -> bool {
            let mut auto_stopped = false;
            let mut limit_message: Option<String> = None;
            if let Some(mut out) = stdout {
                use std::io::Read;
                let mut chunk = [0u8; 65536];
                let mut pending: Vec<u8> = Vec::new();
                let (lock, cv) = &*emit_state;
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
                    let decoded = drain_utf8_lossy(&mut pending);
                    if !decoded.is_empty() {
                        let mut guard = lock.lock().unwrap();
                        guard.0.push_str(&decoded);
                        if guard.0.len() >= EMIT_BATCH_MAX_BYTES {
                            cv.notify_one();
                        }
                    }

                    let over_lines = lines_so_far > MAX_OUTPUT_LINES_BEFORE_AUTO_STOP;
                    let over_bytes = bytes_so_far > MAX_OUTPUT_BYTES_BEFORE_AUTO_STOP;
                    if over_lines || over_bytes {
                        limit_message = Some(if over_lines {
                            format!("{} lines", MAX_OUTPUT_LINES_BEFORE_AUTO_STOP)
                        } else {
                            format!("{} MB", MAX_OUTPUT_BYTES_BEFORE_AUTO_STOP / (1024 * 1024))
                        });
                        // Kill first: the student is waiting, and the message
                        // below is only worth reading once the flood stops.
                        stop_process_generation(&proc_handle1, my_id);
                        auto_stopped = true;
                        break;
                    }
                }
                // Whatever is left, including an incomplete final multi-byte
                // character, then close the stream.
                let mut tail = drain_utf8_lossy(&mut pending);
                if !pending.is_empty() {
                    tail.push_str(&String::from_utf8_lossy(&pending));
                }
                {
                    let mut guard = lock.lock().unwrap();
                    guard.0.push_str(&tail);
                    guard.1 = true;
                    cv.notify_one();
                }
            } else {
                let (lock, cv) = &*emit_state;
                let mut guard = lock.lock().unwrap();
                guard.1 = true;
                cv.notify_one();
            }
            // The auto-stop notice must come AFTER the output it interrupts, so
            // wait for the flusher to drain before saying anything.
            let _ = t_flush.join();
            if let Some(what) = limit_message {
                emit_line(&ah1, "system",
                    &format!("\n[OUTPUT LIMIT EXCEEDED — {}. Auto-stopping process.]\n", what));
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
        // Only OUR child counts: a concurrent Stop (child taken by stop_code)
        // or a newer run means the slot holds a DIFFERENT generation, and we
        // must neither steal it nor fire a misattributed run-done.
        //
        // The child is left IN the slot while it runs. Taking it out first —
        // which is what this did — emptied the slot for the whole life of every
        // program, so Stop, the runaway auto-stop and the kill-on-exit hook all
        // looked in an empty slot and killed nothing.
        //
        // stdin stays open across the wait and is released after it, which is
        // the ordering the whole input feature depends on.
        let (owned, exit_code) =
            wait_program_then_release_stdin(&process_handle, &stdin_handle, my_id);

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
fn run_pip_streaming(py_cmd: &str, args: &[&str], app: &Events) -> i32 {
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
pub fn pip_install_smart<R: Runtime>(
    packages: &[String],
    python_path: Option<&str>,
    app_handle: AppHandle<R>,
) {
    // Wrapped once; everything below reports through the same sink the run
    // pipeline uses, so install progress and program output share one path.
    let app_handle: Events = events_from_handle(app_handle);
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

pub fn pip_uninstall<R: Runtime>(
    packages: &[String],
    python_path: Option<&str>,
    app_handle: AppHandle<R>,
) {
    let app_handle: Events = events_from_handle(app_handle);
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

fn emit_line(app: &Events, stream: &str, text: &str) {
    app.line(stream, text);
}

fn emit_done_with_output(app: &Events, exit_code: Option<i32>, duration_ms: u64, stdout: &str, stderr: &str) {
    app.done(exit_code, duration_ms, stdout, stderr);
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

/// Outcome of waiting for a child that lives in the shared process slot.
enum SlotWait {
    Exited(std::process::ExitStatus),
    /// Ran past the limit and was killed.
    TimedOut,
    /// The student pressed Stop (or a newer run took the slot).
    Cancelled,
}

/// Wait for a child that lives in the SHARED process slot.
///
/// The child STAYS in the slot for as long as it runs, and that is the whole
/// point: `stop_code` stops a program by taking it out of the slot and killing
/// it, so a child held anywhere else cannot be stopped at all.
///
/// This function previously existed only for the compiler while the program was
/// waited on with a plain `child.wait()` — which required taking the child OUT
/// of the slot first. The slot was therefore empty for the entire life of every
/// program, so Stop found nothing, killed nothing, and merely reset the UI while
/// the program kept running. The auto-stop on runaway output and the
/// kill-on-exit hook looked in the same empty slot and were equally inert.
///
/// `std::process::Child` has no timed wait, so this polls. The lock is held only
/// for the `try_wait` itself, leaving Stop free to take the child between polls.
fn wait_child_in_slot(
    process_handle: &RunningProcess,
    my_id: u64,
    limit: Option<Duration>,
    poll: Duration,
) -> SlotWait {
    let start = Instant::now();
    loop {
        {
            let mut guard = match process_handle.lock() {
                Ok(g) => g,
                Err(_) => return SlotWait::Cancelled,
            };
            match guard.as_mut() {
                Some((id, child)) if *id == my_id => match child.try_wait() {
                    Ok(Some(status)) => return SlotWait::Exited(status),
                    Ok(None) => {
                        if let Some(limit) = limit {
                            if start.elapsed() >= limit {
                                let _ = child.kill();
                                let _ = child.wait();
                                return SlotWait::TimedOut;
                            }
                        }
                    }
                    Err(_) => return SlotWait::Cancelled,
                },
                // Someone took our child: Stop, or a newer run.
                _ => return SlotWait::Cancelled,
            }
        }
        thread::sleep(poll);
    }
}

/// Remove this run's child from the slot, if it is still ours.
fn release_slot(process_handle: &RunningProcess, my_id: u64) {
    if let Ok(mut guard) = process_handle.lock() {
        if matches!(guard.as_ref(), Some((id, _)) if *id == my_id) {
            *guard = None;
        }
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

    let wait = wait_child_in_slot(
        process_handle,
        my_id,
        Some(COMPILE_TIMEOUT),
        Duration::from_millis(20),
    );

    // Take the compiler back out of the slot so the program can take its place.
    // If Stop already took it, this is a no-op.
    release_slot(process_handle, my_id);

    let compile_out = join_bounded(t_out, Duration::from_secs(5)).unwrap_or_default();
    let compile_err = join_bounded(t_err, Duration::from_secs(5)).unwrap_or_default();
    let combined = format!("{}{}", compile_err, compile_out);

    match wait {
        SlotWait::Exited(status) if status.success() => CompileAttempt::Success { warnings: combined },
        SlotWait::Exited(status) => CompileAttempt::Failed {
            diagnostics: combined,
            code: status.code(),
        },
        SlotWait::TimedOut => CompileAttempt::TimedOut,
        SlotWait::Cancelled => CompileAttempt::Cancelled,
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
    app: &Events,
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

    // Record the toolchain for the submission manifest, now rather than at
    // submit time — see `toolchain::note_compiler_used`.
    crate::toolchain::note_compiler_used(&compiler);

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

    // Compatibility headers for whatever this platform's standard library is
    // missing — today that is <bits/stdc++.h>, which libstdc++ has and libc++
    // does not. Written into the build directory, added with -idirafter so a
    // toolchain that has the real one is unaffected.
    let compat = crate::toolchain::ensure_compat_headers(&build_dir);

    let spec = crate::toolchain::plan_compile_units(
        dir,
        filename,
        cpp,
        &compiler,
        &standard,
        &exe,
        usize::MAX,
        compat.as_deref(),
    );

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
                    dir, filename, cpp, &compiler, &standard, &exe, 1, compat.as_deref(),
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
    app: &Events,
    process_handle: &RunningProcess,
    my_id: u64,
) -> Option<(String, Vec<String>, Option<std::path::PathBuf>)> {
    let b = build_native(dir, filename, false, app, process_handle, my_id)?;
    Some((b.exe.to_string_lossy().to_string(), vec![], b.bin_dir))
}

fn build_and_run_cpp(
    dir: &Path,
    filename: &str,
    app: &Events,
    process_handle: &RunningProcess,
    my_id: u64,
) -> Option<(String, Vec<String>, Option<std::path::PathBuf>)> {
    let b = build_native(dir, filename, true, app, process_handle, my_id)?;
    Some((b.exe.to_string_lossy().to_string(), vec![], b.bin_dir))
}

fn build_and_run_java(dir: &Path, filename: &str, app: &Events) -> Option<(String, Vec<String>)> {
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
        begin_stdin_run(&handle, 1);
        publish_stdin_pipe(&handle, 1, child.stdin.take());

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
        begin_stdin_run(&handle, 7);
        publish_stdin_pipe(&handle, 7, child.stdin.take());

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
        begin_stdin_run(&handle, 9);
        publish_stdin_pipe(&handle, 9, child.stdin.take());

        close_stdin(&handle);
        child.wait().unwrap();
        // The run is over as far as the UI is concerned.
        clear_stdin_generation(&handle, 9);

        // Typing into a program that has finished is an ordinary thing to do by
        // accident; it must read as "not delivered", never as a failure the
        // student has to interpret.
        assert_eq!(write_stdin(&handle, "late\n").unwrap(), false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_while_the_program_is_still_compiling_is_not_lost() {
        let dir = scratch("typeahead");
        let exe = match build_echo_program(&dir) {
            Some(e) => e,
            None => {
                eprintln!("SKIP typing_while_the_program_is_still_compiling_is_not_lost: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };

        let handle: RunningStdin = new_running_stdin();
        // The run has been claimed but the compiler is still working, so there
        // is no pipe. A student who knows their program wants input starts
        // typing here — as they would into a terminal.
        begin_stdin_run(&handle, 3);
        assert!(!stdin_pipe_ready(&handle));
        assert!(
            write_stdin(&handle, "early\n").unwrap(),
            "type-ahead must be accepted, not rejected"
        );
        assert!(write_stdin(&handle, "alsoearly\n").unwrap());

        // Now the program exists.
        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let mut out = child.stdout.take().unwrap();
        publish_stdin_pipe(&handle, 3, child.stdin.take());

        assert!(write_stdin(&handle, "late\n").unwrap());
        close_stdin(&handle);
        child.wait().unwrap();

        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        let got: Vec<&str> = text.lines().filter(|l| l.starts_with("got:")).collect();
        assert_eq!(
            got,
            vec!["got:early", "got:alsoearly", "got:late"],
            "type-ahead must arrive first and in order; full output: {}",
            text
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eof_pressed_during_the_compile_is_applied_when_the_program_starts() {
        let dir = scratch("early-eof");
        let exe = match build_echo_program(&dir) {
            Some(e) => e,
            None => {
                eprintln!("SKIP eof_pressed_during_the_compile_is_applied_when_the_program_starts: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };

        let handle: RunningStdin = new_running_stdin();
        begin_stdin_run(&handle, 5);
        assert!(write_stdin(&handle, "one\n").unwrap());
        assert!(close_stdin(&handle), "EOF before the program starts is still an EOF");
        // Anything typed after EOF has nowhere to go.
        assert_eq!(write_stdin(&handle, "ignored\n").unwrap(), false);

        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let mut out = child.stdout.take().unwrap();
        publish_stdin_pipe(&handle, 5, child.stdin.take());

        // The program must see the one buffered line and then EOF, and exit on
        // its own without anything further.
        let status = child.wait().expect("program must terminate on the deferred EOF");
        assert!(status.success());

        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        assert!(text.contains("got:one"), "output: {}", text);
        assert!(text.contains("eof:1"), "exactly one line should have arrived: {}", text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn type_ahead_does_not_leak_into_the_next_run() {
        let handle: RunningStdin = new_running_stdin();
        begin_stdin_run(&handle, 10);
        assert!(write_stdin(&handle, "from run 10\n").unwrap());
        clear_stdin_generation(&handle, 10);

        // Nothing is running now.
        assert_eq!(write_stdin(&handle, "orphan\n").unwrap(), false);

        // A new run starts with empty input, not the previous run's leftovers.
        begin_stdin_run(&handle, 11);
        assert!(
            handle.lock().unwrap().pending.is_empty(),
            "a new run must not inherit the previous run's type-ahead"
        );
    }

    #[test]
    fn a_superseded_runs_pipe_is_not_installed() {
        let dir = scratch("superseded");
        let exe = match build_echo_program(&dir) {
            Some(e) => e,
            None => {
                eprintln!("SKIP a_superseded_runs_pipe_is_not_installed: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        let handle: RunningStdin = new_running_stdin();
        begin_stdin_run(&handle, 20);
        // A newer run takes over while run 20 was still compiling.
        begin_stdin_run(&handle, 21);

        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        publish_stdin_pipe(&handle, 20, child.stdin.take());
        assert!(
            !stdin_pipe_ready(&handle),
            "a superseded run must not install its pipe over the current run's"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_that_never_reads_cannot_block_the_caller() {
        let dir = scratch("noread");
        crate::toolchain::clear_compiler_cache();
        let compiler = match crate::toolchain::find_compiler(None, true) {
            Some(c) => c,
            None => {
                eprintln!("SKIP a_program_that_never_reads_cannot_block_the_caller: no C++ compiler");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        // A program that ignores stdin entirely: the OS pipe buffer (64 KB on
        // Windows) fills and every further write would block forever. That must
        // not reach the caller — `write_stdin` is invoked from a Tauri command,
        // and a blocking write there froze the UI and, holding the lock, hung
        // the Stop that was the student's way out.
        std::fs::write(
            dir.join("noread.cpp"),
            "#include <chrono>\n#include <thread>\n\
             int main(){ std::this_thread::sleep_for(std::chrono::seconds(30)); }\n",
        )
        .unwrap();
        let exe = dir.join(if cfg!(windows) { "noread.exe" } else { "noread" });
        let spec = crate::toolchain::plan_compile(
            &dir, "noread.cpp", true, &compiler,
            crate::toolchain::DEFAULT_CPP_STANDARD, &exe,
        );
        let (ok, diag) = spec.run();
        assert!(ok, "fixture failed to compile: {}", diag);

        let mut child = Command::new(&exe)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let handle: RunningStdin = new_running_stdin();
        begin_stdin_run(&handle, 60);
        publish_stdin_pipe(&handle, 60, child.stdin.take());

        // Well past any pipe buffer.
        let chunk = "x".repeat(32 * 1024);
        let start = Instant::now();
        for _ in 0..16 {
            // The answer may be false once the queue cap is reached; what
            // matters is that it RETURNS.
            let _ = write_stdin(&handle, &chunk);
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "write_stdin blocked on a program that is not reading"
            );
        }

        // And Stop must still work while all that is outstanding.
        let stop_start = Instant::now();
        close_stdin(&handle);
        clear_stdin_generation(&handle, 60);
        assert!(
            stop_start.elapsed() < Duration::from_secs(2),
            "closing stdin blocked behind a stuck write"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_run_means_nothing_is_delivered() {
        let handle: RunningStdin = new_running_stdin();
        assert_eq!(write_stdin(&handle, "x\n").unwrap(), false);
        assert!(!close_stdin(&handle));
    }

    #[test]
    fn type_ahead_is_bounded() {
        // A student holding a key down, or a runaway paste, must not grow this
        // buffer without limit while a compile is in flight.
        let handle: RunningStdin = new_running_stdin();
        begin_stdin_run(&handle, 30);
        let chunk = "x".repeat(64 * 1024);
        let mut accepted = 0usize;
        for _ in 0..64 {
            if write_stdin(&handle, &chunk).unwrap() {
                accepted += chunk.len();
            } else {
                break;
            }
        }
        assert!(accepted <= 1 << 20, "type-ahead grew past its cap: {}", accepted);
        assert!(accepted > 0, "some type-ahead should have been accepted");
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
        begin_stdin_run(&handle, 42);
        publish_stdin_pipe(&handle, 42, child.stdin.take());

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

/// End-to-end tests of the actual run pipeline.
///
/// Everything else in this file tests a piece. These drive
/// `execute_code_streaming` itself: a real source file is compiled by a real
/// compiler, a real child process runs, real bytes go down its stdin, and the
/// assertions are made on exactly what the frontend would have been told.
///
/// That level matters because the mistakes that actually happen here are
/// ordering mistakes. Releasing stdin before `child.wait()` rather than after
/// it type-checks, passes every unit test in this file, and silently hands the
/// program EOF the instant it starts — an exam answer then reads nothing and
/// prints a confident wrong result. Only a whole run catches that.
///
/// The sink is a plain collector rather than Tauri's mock runtime, which links
/// a windowing runtime into the test binary and does not load on every machine.
#[cfg(test)]
mod e2e_tests {
    use super::*;

    /// These tests share process-global state — `RUN_GEN` and the cancellation
    /// set — because a real session only ever has one run in flight. Cargo runs
    /// tests in parallel, so without this one test's Stop would cancel
    /// another's run and the failure would look like a product bug. Production
    /// needs no such lock: the Run button becomes Stop, which is what serialises
    /// runs there.
    static E2E_LOCK: Mutex<()> = Mutex::new(());

    /// Take the lock, ignoring poisoning — a panicking test should not cascade
    /// into every later one.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Kills whatever is still in the process slot when a test ends.
    ///
    /// A test that leaves a child running is not merely untidy here: the child
    /// inherited the test harness's stdout pipe, so it holds that pipe open
    /// after cargo exits and anything reading the harness's output waits
    /// forever. Dropping this guard reaps the child on every exit path,
    /// including a panic.
    struct RunGuard(RunningProcess, RunningStdin);
    impl Drop for RunGuard {
        fn drop(&mut self) {
            close_stdin(&self.1);
            if let Some(child) = self.0.lock().ok().and_then(|mut g| g.take().map(|(_, c)| c)) {
                stop_taken_child(child);
            }
        }
    }

    /// Collects what the frontend would have received.
    #[derive(Default)]
    struct Collector {
        stdout: Mutex<String>,
        stderr: Mutex<String>,
        system: Mutex<String>,
        done: Mutex<Option<(Option<i32>, u64)>>,
    }

    impl RunEvents for Collector {
        fn line(&self, stream: &str, text: &str) {
            let sink = match stream {
                "stdout" => &self.stdout,
                "stderr" => &self.stderr,
                _ => &self.system,
            };
            sink.lock().unwrap().push_str(text);
        }
        fn done(&self, exit_code: Option<i32>, duration_ms: u64, _stdout: &str, _stderr: &str) {
            *self.done.lock().unwrap() = Some((exit_code, duration_ms));
        }
    }

    struct Ws(std::path::PathBuf);
    impl Ws {
        fn new(tag: &str) -> Ws {
            let d = std::env::temp_dir().join(format!(
                "mint-e2e-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|x| x.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&d).unwrap();
            Ws(d)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
    }
    impl Drop for Ws {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct RunResult {
        stdout: String,
        stderr: String,
        system: String,
        exit_code: Option<i32>,
        finished: bool,
    }

    /// Drive one complete run. `feed` is called once the program is live, with
    /// the stdin handle, so a test sends input exactly the way the UI does.
    fn run_program<F>(ws: &Ws, language: &str, filename: &str, code: &str, feed: F) -> RunResult
    where
        F: FnOnce(&RunningStdin),
    {
        let collector = Arc::new(Collector::default());
        let events: Events = collector.clone();
        let process = new_running_process();
        let stdin = new_running_stdin();
        let _reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            language,
            code,
            filename,
            Some(&ws.0.to_string_lossy()),
            None,
            events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );

        // Wait for the child to exist before feeding it, so the test is not
        // racing the compile step.
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let live = stdin_pipe_ready(&stdin);
            let finished = collector.done.lock().unwrap().is_some();
            if live || finished || Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        feed(&stdin);

        // Then wait for completion.
        let deadline = Instant::now() + Duration::from_secs(180);
        while collector.done.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }

        // Copied into locals first: a MutexGuard temporary that lives to the end
        // of the block would outlive the Arc it borrows from.
        let done = *collector.done.lock().unwrap();
        let stdout = collector.stdout.lock().unwrap().clone();
        let stderr = collector.stderr.lock().unwrap().clone();
        let system = collector.system.lock().unwrap().clone();
        RunResult {
            stdout,
            stderr,
            system,
            exit_code: done.and_then(|(c, _)| c),
            finished: done.is_some(),
        }
    }

    fn have_cxx() -> bool {
        crate::toolchain::clear_compiler_cache();
        crate::toolchain::find_compiler(None, true).is_some()
    }

    #[test]
    fn cpp_run_reads_stdin_and_reports_its_output() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP cpp_run_reads_stdin_and_reports_its_output: no C++ compiler");
            return;
        }
        let ws = Ws::new("cpp-stdin");
        let code = "#include <iostream>\n\
                    int main(){ int a,b; std::cin>>a>>b; std::cout<<(a*b)<<std::endl; return 0; }\n";
        let r = run_program(&ws, "cpp", "main.cpp", code, |stdin| {
            // Exactly what the UI does when the student presses Enter.
            assert!(
                write_stdin(stdin, "6 7\n").unwrap(),
                "stdin must be open while the program is running"
            );
        });

        assert!(r.finished, "run-done never arrived (system log: {})", r.system);
        assert_eq!(r.stdout.trim(), "42", "stderr: {} system: {}", r.stderr, r.system);
        assert_eq!(r.exit_code, Some(0));
        assert!(r.system.contains("Compiling"), "the compile step should be announced");
    }

    #[test]
    fn input_typed_during_the_compile_still_reaches_the_program() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP input_typed_during_the_compile_still_reaches_the_program: no C++ compiler");
            return;
        }
        // A C++ Run spends its first half-second to second compiling. A student
        // who knows their program wants input starts typing straight away, and
        // in the first version of this feature those keystrokes were rejected —
        // which ALSO latched the input box shut for the rest of the run. This
        // drives the whole pipeline and writes before the program can possibly
        // exist.
        let ws = Ws::new("cpp-typeahead");
        let code = "#include <iostream>\n\
                    int main(){ int a,b; std::cin>>a>>b; std::cout<<(a+b)<<std::endl; }\n";

        let collector = Arc::new(Collector::default());
        let events: Events = collector.clone();
        let process = new_running_process();
        let stdin = new_running_stdin();
        let _reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            "cpp",
            code,
            "main.cpp",
            Some(&ws.0.to_string_lossy()),
            None,
            events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );

        // Write IMMEDIATELY. The compiler cannot have finished yet.
        assert!(
            !stdin_pipe_ready(&stdin),
            "the program cannot already exist; this test would prove nothing"
        );
        assert!(
            write_stdin(&stdin, "17 25\n").unwrap(),
            "input typed during the compile must be accepted"
        );

        let deadline = Instant::now() + Duration::from_secs(180);
        while collector.done.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }

        let out = collector.stdout.lock().unwrap().clone();
        let sys_log = collector.system.lock().unwrap().clone();
        assert_eq!(out.trim(), "42", "system log: {}", sys_log);
    }

    /// What `lib::stop_code` does, without the Tauri State plumbing: record the
    /// stop against the current generation, close input, take the child out of
    /// the slot and kill it.
    fn press_stop(process: &RunningProcess, stdin: &RunningStdin) {
        close_stdin(stdin);
        note_stop_request();
        if let Some(child) = process.lock().ok().and_then(|mut g| g.take().map(|(_, c)| c)) {
            stop_taken_child(child);
        }
    }

    #[test]
    fn stop_during_the_compile_prevents_the_program_from_running() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP stop_during_the_compile_prevents_the_program_from_running: no C++ compiler");
            return;
        }
        // Stop used to be a no-op while a compile was in flight: it found an
        // empty process slot, did nothing, and the program started anyway once
        // the compiler finished. The student pressed Stop and watched their
        // program run. <bits/stdc++.h> gives a compile long enough (~1s) to land
        // a Stop inside reliably.
        let ws = Ws::new("stop-compile");
        let code = "#include <bits/stdc++.h>\n\
                    int main(){ std::cout << \"SHOULD NOT RUN\" << std::endl; \
                    std::ofstream f(\"ran.txt\"); f << \"ran\"; return 0; }\n";

        let collector = Arc::new(Collector::default());
        let events: Events = collector.clone();
        let process = new_running_process();
        let stdin = new_running_stdin();
        let _reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            "cpp", code, "main.cpp",
            Some(&ws.0.to_string_lossy()),
            None,
            events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );

        // Wait until the compiler is actually in the slot, then stop it.
        let deadline = Instant::now() + Duration::from_secs(30);
        while current_slot_generation(&process).is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            current_slot_generation(&process).is_some(),
            "the compiler should have been published to the process slot"
        );
        press_stop(&process, &stdin);

        // Give the run every chance to (wrongly) proceed.
        thread::sleep(Duration::from_secs(3));

        let out = collector.stdout.lock().unwrap().clone();
        assert!(
            !out.contains("SHOULD NOT RUN"),
            "the program ran after Stop: {:?}",
            out
        );
        assert!(
            !ws.0.join("ran.txt").exists(),
            "the program ran after Stop and left its output behind"
        );
        assert!(
            collector.done.lock().unwrap().is_none(),
            "a cancelled run must not fire run-done; the UI already reset itself"
        );
    }

    #[test]
    fn runaway_output_is_auto_stopped_and_the_program_really_dies() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP runaway_output_is_auto_stopped_and_the_program_really_dies: no C++ compiler");
            return;
        }
        // `while(1) printf(...)` is a normal exam mistake. The auto-stop looks
        // for the child in the same shared slot Stop does, so it was inert for
        // exactly the same reason: the run thread had already taken the child
        // out. The line cap would be announced and the program would keep
        // flooding.
        let ws = Ws::new("runaway");
        let code = "#include <cstdio>\n\
                    int main(){ for(long long i=0;;++i) printf(\"%lld\\n\", i); }\n";

        let collector = Arc::new(Collector::default());
        let events: Events = collector.clone();
        let process = new_running_process();
        let stdin = new_running_stdin();
        let _reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            "cpp", code, "main.cpp",
            Some(&ws.0.to_string_lossy()),
            None,
            events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );

        // The cap is 200k lines / 64 MB, which this program reaches in seconds.
        let deadline = Instant::now() + Duration::from_secs(180);
        while collector.done.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            collector.done.lock().unwrap().is_some(),
            "the run never finished; a runaway program must be auto-stopped"
        );
        assert!(
            collector.system.lock().unwrap().contains("OUTPUT LIMIT EXCEEDED"),
            "the student should be told why it stopped: {}",
            collector.system.lock().unwrap()
        );

        // And the announcement has to be true.
        let settled = collector.stdout.lock().unwrap().len();
        thread::sleep(Duration::from_millis(800));
        assert_eq!(
            collector.stdout.lock().unwrap().len(),
            settled,
            "the program was still printing after the auto-stop announced it had been stopped"
        );
    }

    #[test]
    fn stop_actually_stops_the_program_not_just_the_ui() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP stop_actually_stops_the_program_not_just_the_ui: no C++ compiler");
            return;
        }
        // Asserting that the process slot is empty after Stop proves nothing:
        // it is empty either way. This watches the program's OUTPUT instead. A
        // program that keeps printing after Stop is a program that was never
        // stopped — and in an exam that is a runaway the student cannot escape.
        let ws = Ws::new("stop-proof");
        let code = "#include <iostream>\n#include <chrono>\n#include <thread>\n\
                    int main(){ for(long long i=0;;++i){ std::cout << i << std::endl;\n\
                    std::this_thread::sleep_for(std::chrono::milliseconds(50)); } }\n";

        let collector = Arc::new(Collector::default());
        let events: Events = collector.clone();
        let process = new_running_process();
        let stdin = new_running_stdin();
        let _reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            "cpp", code, "main.cpp",
            Some(&ws.0.to_string_lossy()),
            None,
            events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );

        // Wait until it is unmistakably running.
        let deadline = Instant::now() + Duration::from_secs(120);
        while collector.stdout.lock().unwrap().len() < 4 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            collector.stdout.lock().unwrap().len() >= 4,
            "the program never started printing; system log: {}",
            collector.system.lock().unwrap()
        );

        press_stop(&process, &stdin);

        // Give any in-flight output time to land, then take a reading.
        thread::sleep(Duration::from_millis(600));
        let after_stop = collector.stdout.lock().unwrap().len();

        // A stopped program prints nothing more. Two seconds is forty more
        // lines at the program's rate — no ambiguity either way.
        thread::sleep(Duration::from_secs(2));
        let later = collector.stdout.lock().unwrap().len();

        assert_eq!(
            later, after_stop,
            "the program kept printing after Stop — it was never killed \
             (grew from {} to {} bytes)",
            after_stop, later
        );
    }

    #[test]
    fn stop_while_running_kills_the_program() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP stop_while_running_kills_the_program: no C++ compiler");
            return;
        }
        // The runaway an exam actually produces: a loop that never ends.
        let ws = Ws::new("stop-running");
        let code = "#include <iostream>\n#include <chrono>\n#include <thread>\n\
                    int main(){ std::cout << \"started\" << std::endl;\n\
                    for(;;) std::this_thread::sleep_for(std::chrono::milliseconds(50)); }\n";

        let collector = Arc::new(Collector::default());
        let events: Events = collector.clone();
        let process = new_running_process();
        let stdin = new_running_stdin();
        let _reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            "cpp", code, "main.cpp",
            Some(&ws.0.to_string_lossy()),
            None,
            events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );

        // Wait for the program itself to be live and talking. Whatever happens,
        // the child must not outlive this test: an orphan holding the inherited
        // stdout pipe keeps the whole test harness from exiting.
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut started = false;
        while Instant::now() < deadline {
            if collector.stdout.lock().unwrap().contains("started") {
                started = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        if !started {
            let out = collector.stdout.lock().unwrap().clone();
            let err = collector.stderr.lock().unwrap().clone();
            let sys_log = collector.system.lock().unwrap().clone();
            press_stop(&process, &stdin);
            panic!(
                "the program never reported starting.\nstdout: {:?}\nstderr: {:?}\nsystem: {:?}",
                out, err, sys_log
            );
        }

        let stop_at = Instant::now();
        press_stop(&process, &stdin);

        // run-done is not emitted for a stopped run (the UI already reset
        // itself), so completion is observed through the input being released.
        let deadline = Instant::now() + Duration::from_secs(15);
        while stdin_pipe_ready(&stdin) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !stdin_pipe_ready(&stdin),
            "input was still open {:?} after Stop — the program was not reaped",
            stop_at.elapsed()
        );
        assert!(
            process.lock().unwrap().is_none(),
            "the process slot should be empty after Stop"
        );
    }

    #[test]
    fn cpp_run_ends_on_eof() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP cpp_run_ends_on_eof: no C++ compiler");
            return;
        }
        let ws = Ws::new("cpp-eof");
        let code = "#include <iostream>\n\
                    int main(){ long long x,s=0; while(std::cin>>x) s+=x; std::cout<<s<<std::endl; }\n";
        let r = run_program(&ws, "cpp", "main.cpp", code, |stdin| {
            assert!(write_stdin(stdin, "1 2 3 4\n").unwrap());
            // Without this the program waits forever — which is the whole reason
            // the EOF button exists.
            assert!(close_stdin(stdin));
        });

        assert!(r.finished, "an input loop must terminate on EOF");
        assert_eq!(r.stdout.trim(), "10");
    }

    #[test]
    fn cpp_multi_file_run_links_and_runs() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP cpp_multi_file_run_links_and_runs: no C++ compiler");
            return;
        }
        let ws = Ws::new("cpp-multi");
        ws.write("helper.h", "#pragma once\nint twice(int);\n");
        ws.write("helper.cpp", "#include \"helper.h\"\nint twice(int x){return x*2;}\n");
        let code = "#include <iostream>\n#include \"helper.h\"\n\
                    int main(){ int n; std::cin>>n; std::cout<<twice(n)<<std::endl; }\n";
        let r = run_program(&ws, "cpp", "main.cpp", code, |stdin| {
            assert!(write_stdin(stdin, "21\n").unwrap());
        });

        assert!(r.finished, "system log: {}", r.system);
        assert_eq!(r.stdout.trim(), "42", "stderr: {}", r.stderr);
        assert!(
            r.system.contains("helper.cpp"),
            "the linked sibling should be named in the compile line: {}",
            r.system
        );
    }

    #[test]
    fn a_compile_error_is_reported_and_nothing_runs() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP a_compile_error_is_reported_and_nothing_runs: no C++ compiler");
            return;
        }
        let ws = Ws::new("cpp-broken");
        let code = "#include <iostream>\nint main(){ this is not valid }\n";
        let r = run_program(&ws, "cpp", "main.cpp", code, |_| {});

        assert!(r.finished, "the UI must be released even when the compile fails");
        assert!(r.stderr.contains("[Compilation Error]"), "stderr was: {}", r.stderr);
        assert!(r.stderr.contains("main.cpp:2"), "diagnostic should name the line: {}", r.stderr);
        assert!(r.stdout.is_empty(), "a program that did not compile must not produce output");
    }

    #[test]
    fn a_header_cannot_be_run_on_its_own() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP a_header_cannot_be_run_on_its_own: no C++ compiler");
            return;
        }
        let ws = Ws::new("cpp-header");
        let r = run_program(&ws, "cpp", "util.h", "#pragma once\nint f();\n", |_| {});
        assert!(r.finished);
        assert!(
            r.stderr.contains("헤더 파일"),
            "the student needs to be told what to run instead: {}",
            r.stderr
        );
    }

    #[test]
    fn a_runtime_crash_is_reported_as_a_nonzero_exit() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP a_runtime_crash_is_reported_as_a_nonzero_exit: no C++ compiler");
            return;
        }
        let ws = Ws::new("cpp-crash");
        let code = "#include <vector>\n#include <iostream>\n\
                    int main(){ std::vector<int> v; std::cout << v.at(3) << std::endl; }\n";
        let r = run_program(&ws, "cpp", "main.cpp", code, |_| {});
        assert!(r.finished);
        assert_ne!(r.exit_code, Some(0), "an uncaught exception must not look like success");
    }

    #[test]
    fn program_output_is_streamed_not_only_delivered_at_exit() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP program_output_is_streamed_not_only_delivered_at_exit: no C++ compiler");
            return;
        }
        // A program that prompts and then reads is the normal interactive
        // shape. The prompt must reach the student BEFORE they are expected to
        // answer it, so this asserts on the output WHILE the program is still
        // blocked — asserting after the run would pass even if nothing was
        // emitted until exit, which is exactly the bug this guards.
        let ws = Ws::new("cpp-stream");
        let code = "#include <iostream>\n#include <string>\n\
                    int main(){ std::cout << \"이름을 입력하세요: \" << std::flush;\n\
                    std::string s; std::getline(std::cin, s);\n\
                    std::cout << \"안녕하세요, \" << s << std::endl; }\n";

        let collector = Arc::new(Collector::default());
        let events: Events = collector.clone();
        let process = new_running_process();
        let stdin = new_running_stdin();
        let _reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            "cpp", code, "main.cpp",
            Some(&ws.0.to_string_lossy()),
            None,
            events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );

        // Wait for the PROMPT, with the program still parked in getline.
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if collector.stdout.lock().unwrap().contains("이름을 입력하세요:") {
                break;
            }
            if Instant::now() > deadline {
                let out = collector.stdout.lock().unwrap().clone();
                let err = collector.stderr.lock().unwrap().clone();
                let sys_log = collector.system.lock().unwrap().clone();
                panic!(
                    "an unterminated prompt never streamed.\nstdout: {:?}\nstderr: {:?}\nsystem: {:?}",
                    out, err, sys_log
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            collector.done.lock().unwrap().is_none(),
            "the program should still be waiting for input at this point"
        );

        write_stdin(&stdin, "민트\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(60);
        while collector.done.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let out = collector.stdout.lock().unwrap().clone();
        assert!(out.contains("안녕하세요, 민트"), "stdout: {:?}", out);
    }

    #[test]
    fn the_binary_is_not_written_into_the_workspace() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP the_binary_is_not_written_into_the_workspace: no C++ compiler");
            return;
        }
        // Build output inside the workspace would ride along in the submission
        // zip and be seen by the integrity monitor as a new file on every Run.
        let ws = Ws::new("cpp-clean");
        let code = "#include <iostream>\nint main(){ std::cout << \"ok\" << std::endl; }\n";
        let r = run_program(&ws, "cpp", "main.cpp", code, |_| {});
        assert!(r.finished);
        assert_eq!(r.stdout.trim(), "ok");

        let mut left: Vec<String> = std::fs::read_dir(&ws.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec!["main.cpp".to_string()],
            "the workspace must contain only the student's source"
        );
    }

    #[test]
    fn c_language_runs_through_the_same_pipeline() {
        let _guard = exclusive();
        crate::toolchain::clear_compiler_cache();
        if crate::toolchain::find_compiler(None, false).is_none() {
            eprintln!("SKIP c_language_runs_through_the_same_pipeline: no C compiler");
            return;
        }
        let ws = Ws::new("c-run");
        let code = "#include <stdio.h>\n\
                    int main(void){ int a,b; if(scanf(\"%d %d\", &a, &b)!=2) return 1;\n\
                    printf(\"%d\\n\", a+b); return 0; }\n";
        let r = run_program(&ws, "c", "main.c", code, |stdin| {
            assert!(write_stdin(stdin, "20 22\n").unwrap());
        });
        assert!(r.finished, "system: {}", r.system);
        assert_eq!(r.stdout.trim(), "42", "stderr: {}", r.stderr);
    }

    fn have(cmd: &str) -> bool {
        let mut c = Command::new(cmd);
        c.arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c.status().map(|s| s.success()).unwrap_or(false)
    }

    #[test]
    fn java_still_runs() {
        let _guard = exclusive();
        if !have("javac") {
            eprintln!("SKIP java_still_runs: no JDK");
            return;
        }
        // Java shares the dispatch and the whole run pipeline with C++. It is
        // not what this work was about, which is exactly why it deserves a
        // regression test: the dispatch changed shape underneath it.
        let ws = Ws::new("java");
        let code = "import java.util.Scanner;\n\
                    public class Main { public static void main(String[] a){\n\
                      Scanner s = new Scanner(System.in);\n\
                      int x = s.nextInt(), y = s.nextInt();\n\
                      System.out.println(x + y); } }\n";
        let r = run_program(&ws, "java", "Main.java", code, |stdin| {
            let _ = write_stdin(stdin, "19 23\n");
        });
        assert!(r.finished, "system: {} stderr: {}", r.system, r.stderr);
        assert_eq!(r.stdout.trim(), "42", "stderr: {}", r.stderr);
    }

    #[test]
    fn javascript_still_runs() {
        let _guard = exclusive();
        if !have("node") {
            eprintln!("SKIP javascript_still_runs: no node");
            return;
        }
        let ws = Ws::new("js");
        let code = "process.stdin.on('data', d => {\n\
                      const [a, b] = d.toString().trim().split(/\\s+/).map(Number);\n\
                      console.log(a + b);\n\
                      process.exit(0);\n\
                    });\n";
        let r = run_program(&ws, "javascript", "main.js", code, |stdin| {
            let _ = write_stdin(stdin, "20 22\n");
        });
        assert!(r.finished, "system: {} stderr: {}", r.system, r.stderr);
        assert_eq!(r.stdout.trim(), "42", "stderr: {}", r.stderr);
    }

    #[test]
    fn a_grandchild_holding_the_pipe_does_not_hang_the_run() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP a_grandchild_holding_the_pipe_does_not_hang_the_run: no C++ compiler");
            return;
        }
        // A student's program that spawns something which outlives it — the C++
        // equivalent of Python's multiprocessing — keeps the inherited stdout
        // pipe open after the program itself exits. Completion is gated on the
        // CHILD exiting rather than on the pipe reaching EOF precisely so this
        // does not hang the UI at "running" forever.
        let ws = Ws::new("grandchild");
        let sleeper = if cfg!(windows) {
            // `timeout` needs a console; ping against localhost is the usual
            // console-free way to wait on Windows.
            "system(\"ping -n 4 127.0.0.1 > nul\");"
        } else {
            "system(\"sleep 3\");"
        };
        let code = format!(
            "#include <cstdlib>\n#include <iostream>\n\
             int main(){{ std::cout << \"parent done\" << std::endl; {} return 0; }}\n",
            // The child is spawned SYNCHRONOUSLY here, which still proves the
            // point on the reader side: the run must not be gated on EOF alone.
            sleeper
        );
        let started = Instant::now();
        let r = run_program(&ws, "cpp", "main.cpp", &code, |_| {});
        assert!(r.finished, "the run never completed; system: {}", r.system);
        assert!(r.stdout.contains("parent done"), "stdout: {:?}", r.stdout);
        // Generous, but far below the "hangs forever" failure it guards.
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the run took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_run_started_right_after_a_stop_is_not_killed_by_it() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP a_run_started_right_after_a_stop_is_not_killed_by_it: no C++ compiler");
            return;
        }
        // Stop, then Run again immediately, is what a student does the moment
        // they spot a mistake. The stop must not reach into the NEW run: a kill
        // that lands on the wrong generation would look like the second run
        // silently doing nothing.
        let ws = Ws::new("stop-then-run");
        let loop_code = "#include <iostream>\n#include <chrono>\n#include <thread>\n\
                         int main(){ for(;;){ std::cout << \"loop\" << std::endl;\n\
                         std::this_thread::sleep_for(std::chrono::milliseconds(50)); } }\n";

        let first = Arc::new(Collector::default());
        let process = new_running_process();
        let stdin = new_running_stdin();
        let reaper = RunGuard(process.clone(), stdin.clone());

        execute_code_streaming(
            "cpp", loop_code, "main.cpp",
            Some(&ws.0.to_string_lossy()),
            None,
            first.clone() as Events,
            process.clone(),
            stdin.clone(),
            crate::monitor::new_known_writes(),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        while !first.stdout.lock().unwrap().contains("loop") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            first.stdout.lock().unwrap().contains("loop"),
            "the first run never started; system: {}",
            first.system.lock().unwrap()
        );
        press_stop(&process, &stdin);
        drop(reaper);

        // Second run, with no pause at all.
        let ws2 = Ws::new("stop-then-run-2");
        let second = Arc::new(Collector::default());
        let process2 = new_running_process();
        let stdin2 = new_running_stdin();
        let _reaper2 = RunGuard(process2.clone(), stdin2.clone());
        let quick = "#include <iostream>\nint main(){ std::cout << \"second ok\" << std::endl; }\n";

        execute_code_streaming(
            "cpp", quick, "main.cpp",
            Some(&ws2.0.to_string_lossy()),
            None,
            second.clone() as Events,
            process2.clone(),
            stdin2.clone(),
            crate::monitor::new_known_writes(),
        );

        let deadline = Instant::now() + Duration::from_secs(120);
        while second.done.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let out = second.stdout.lock().unwrap().clone();
        assert!(
            second.done.lock().unwrap().is_some(),
            "the second run never completed — the previous Stop leaked into it. stdout: {:?} system: {}",
            out,
            second.system.lock().unwrap()
        );
        assert!(out.contains("second ok"), "stdout: {:?}", out);
        assert!(
            !out.contains("loop"),
            "the first run's output leaked into the second: {:?}",
            out
        );
    }

    #[test]
    fn two_runs_in_a_row_do_not_mix_their_output() {
        let _guard = exclusive();
        if !have_cxx() {
            eprintln!("SKIP two_runs_in_a_row_do_not_mix_their_output: no C++ compiler");
            return;
        }
        let ws = Ws::new("twice");
        let first = run_program(
            &ws, "cpp", "main.cpp",
            "#include <iostream>\nint main(){ std::cout << \"first\" << std::endl; }\n",
            |_| {},
        );
        assert!(first.finished, "system: {}", first.system);
        assert_eq!(first.stdout.trim(), "first");

        let second = run_program(
            &ws, "cpp", "main.cpp",
            "#include <iostream>\nint main(){ std::cout << \"second\" << std::endl; }\n",
            |_| {},
        );
        assert!(second.finished, "system: {}", second.system);
        assert_eq!(
            second.stdout.trim(),
            "second",
            "the edited program must be the one that runs — a stale binary would print 'first'"
        );
    }

    #[test]
    fn python_input_now_works_too() {
        let _guard = exclusive();
        // The same pipe fixed Python: `input()` used to hit EOF instantly,
        // because a GUI process has no console to inherit stdin from.
        let ws = Ws::new("py-stdin");
        let code = "name = input()\nprint('hello', name)\n";
        let r = run_program(&ws, "python", "main.py", code, |stdin| {
            let _ = write_stdin(stdin, "mint\n");
        });
        if !r.finished || r.stderr.contains("Failed to run") {
            eprintln!("SKIP python_input_now_works_too: no usable python on this machine");
            return;
        }
        assert!(
            r.stdout.contains("hello mint"),
            "stdout: {:?} stderr: {:?}",
            r.stdout,
            r.stderr
        );
    }
}
