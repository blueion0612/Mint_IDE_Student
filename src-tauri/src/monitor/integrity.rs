use super::log::{ActivityEvent, LogHandle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Tracks file hashes to detect external modifications.
/// Any change not made through our IDE is flagged as TAMPER.
#[derive(Clone, Serialize, Deserialize)]
struct FileState {
    hash: String,
    size: u64,
    line_count: usize,
    modified: u64, // mtime as epoch secs
}

/// Persistent baseline. Stored OUTSIDE the student-writable workspace (under
/// %LOCALAPPDATA%\MINT_Exam_IDE\baselines\) and HMAC-signed so a student can
/// neither delete it from the workspace nor hand-edit it to match externally
/// modified files to wipe the tamper history on the next launch.
///
/// NOTE: the HMAC key is embedded in the binary, so this is tamper-EVIDENCE,
/// not secrecy against a determined reverse-engineer. It defeats the realistic
/// "delete the obvious .mint_baseline.json / edit it in Notepad" attack that
/// the previous in-workspace plaintext baseline allowed.
const BASELINE_HMAC_KEY: &[u8] = b"MINT_EXAM_IDE_baseline_v2_integrity_key_2026";

#[derive(Serialize, Deserialize)]
struct BaselineSnapshot {
    saved_at: u64,
    files: HashMap<String, FileState>,
}

#[derive(Serialize, Deserialize)]
struct SignedBaseline {
    payload: String, // serialized BaselineSnapshot
    sig: String,     // HMAC-SHA256(payload)
}

enum BaselineLoad {
    Missing,
    Invalid,
    Ok(HashMap<String, FileState>),
}

/// HMAC-SHA256 (RFC 2104) over `msg` with the embedded key, hex-encoded.
fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        let mut h = Sha256::new();
        h.update(key);
        block[..32].copy_from_slice(&h.finalize());
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }
    let mut hi = Sha256::new();
    hi.update(&ipad[..]);
    hi.update(msg);
    let inner = hi.finalize();
    let mut ho = Sha256::new();
    ho.update(&opad[..]);
    ho.update(&inner);
    hex::encode(ho.finalize())
}

/// Baseline file path: in app-data (NOT the workspace), keyed by a hash of the
/// workspace root so different workspaces don't collide.
fn baseline_path(root: &Path) -> PathBuf {
    let key = {
        let mut h = Sha256::new();
        h.update(normalize_key(&root.to_string_lossy()).as_bytes());
        hex::encode(h.finalize())
    };
    crate::setup::app_data_root()
        .join("baselines")
        .join(format!("{}.json", key))
}

fn load_baseline(root: &Path) -> BaselineLoad {
    let text = match std::fs::read_to_string(baseline_path(root)) {
        Ok(t) => t,
        Err(_) => return BaselineLoad::Missing,
    };
    let signed: SignedBaseline = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(_) => return BaselineLoad::Invalid,
    };
    if hmac_sha256_hex(BASELINE_HMAC_KEY, signed.payload.as_bytes()) != signed.sig {
        return BaselineLoad::Invalid;
    }
    match serde_json::from_str::<BaselineSnapshot>(&signed.payload) {
        Ok(snap) => BaselineLoad::Ok(snap.files),
        Err(_) => BaselineLoad::Invalid,
    }
}

fn save_baseline(root: &Path, state: &HashMap<String, FileState>) {
    let snap = BaselineSnapshot {
        saved_at: epoch_secs(),
        files: state.clone(),
    };
    let Ok(payload) = serde_json::to_string(&snap) else { return; };
    let sig = hmac_sha256_hex(BASELINE_HMAC_KEY, payload.as_bytes());
    let Ok(text) = serde_json::to_string(&SignedBaseline { payload, sig }) else { return; };
    // Atomic write: temp file + rename. A torn write would leave the next
    // restart with no baseline (worse than the previous baseline).
    let final_path = baseline_path(root);
    if let Some(parent) = final_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = final_path.with_extension("json.tmp");
    if std::fs::write(&tmp_path, text.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp_path, &final_path);
    }
}

/// One read → (hash, size, line_count). The poll loop previously read every
/// monitored file TWICE per 2s cycle (once for the hash, once for the line
/// count); on workspaces with a large .txt/.py this doubled steady-state disk
/// I/O for no benefit.
fn stat_bytes(data: &[u8]) -> (String, u64, usize) {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hash = hex::encode(hasher.finalize());
    let lines = data.iter().filter(|&&c| c == b'\n').count() + 1;
    (hash, data.len() as u64, lines)
}

/// Read with brief retry — initial-scan variant of hash_file_retry. A transient
/// AV/Defender share-deny right at launch used to leave the file OUT of the
/// baseline entirely, so the next poll raised a false "EXTERNAL FILE ADDED".
fn read_file_retry(path: &Path) -> Option<Vec<u8>> {
    for _ in 0..3 {
        if let Ok(d) = std::fs::read(path) {
            return Some(d);
        }
        thread::sleep(Duration::from_millis(25));
    }
    std::fs::read(path).ok()
}

/// Shared set of "known writes" — the IDE registers a path here right before
/// writing, so the integrity checker skips that change. The value is
/// `(expected_hash, grace_until)`: when `expected_hash` is `Some`, a change is
/// only treated as an own-write if the on-disk content hash matches it, so an
/// external overwrite of the same path within the grace window (different
/// content) is STILL flagged. `None` means time-only (used for renames /
/// deletes / dir creation where pinning a resulting hash is not meaningful).
pub type KnownWrites = Arc<Mutex<HashMap<String, (Option<String>, u64)>>>;

pub fn new_known_writes() -> KnownWrites {
    Arc::new(Mutex::new(HashMap::new()))
}

/// A read-only snapshot of the integrity baseline (normalized rel -> content
/// hash), republished by the monitor after every poll. Rename-recognition
/// (`lib::pin_rename_sources`) consults it so an IDE-initiated rename/move
/// recognizes a destination ONLY when the source's current bytes are already
/// known-good in the baseline — never arbitrary live disk bytes. Pinning live
/// bytes would let an external PRE-rename edit be laundered: the attacker edits
/// a file between polls, renames its (parent) via the IDE, and the pin would
/// bless the tampered content. Pinning against the baseline value the attacker
/// cannot influence closes that hole while keeping honest renames event-free.
pub type SharedBaseline = Arc<Mutex<HashMap<String, String>>>;

pub fn new_shared_baseline() -> SharedBaseline {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Last-known-good content hash for `rel` (any path separator / unicode form —
/// normalized here to match the monitor's keys).
pub fn shared_baseline_hash(sb: &SharedBaseline, rel: &str) -> Option<String> {
    sb.lock().ok().and_then(|m| m.get(&normalize_key(rel)).cloned())
}

fn publish_baseline(sb: &SharedBaseline, state: &HashMap<String, FileState>) {
    if let Ok(mut m) = sb.lock() {
        m.clear();
        for (k, v) in state {
            m.insert(k.clone(), v.hash.clone());
        }
    }
}

/// Grace period in seconds. The integrity loop polls every 2s; we leave
/// headroom for filesystem buffer flush + IDE write completion + network
/// drive latency. (Content-pinning via mark_known_write_hash means this
/// window no longer blindly excuses arbitrary external overwrites.)
const KNOWN_WRITE_GRACE_SECS: u64 = 8;

/// Time-only known-write (no content pin). Use for rename / delete / dir
/// creation where the resulting on-disk hash is not predictable.
pub fn mark_known_write(known: &KnownWrites, relative_path: &str) {
    if let Ok(mut map) = known.lock() {
        map.insert(
            normalize_key(relative_path),
            (None, epoch_secs() + KNOWN_WRITE_GRACE_SECS),
        );
    }
}

/// Content-pinned known-write. The scan treats a change as our own write ONLY
/// if the on-disk content hash equals `expected_hash` within the grace window
/// — so an external overwrite to the same path (different content) is still
/// flagged as tampering even right after a legitimate IDE write/run.
pub fn mark_known_write_hash(known: &KnownWrites, relative_path: &str, expected_hash: &str) {
    if let Ok(mut map) = known.lock() {
        map.insert(
            normalize_key(relative_path),
            (Some(expected_hash.to_string()), epoch_secs() + KNOWN_WRITE_GRACE_SECS),
        );
    }
}

/// Generated-OUTPUT file extensions that are NOT integrity-monitored. A
/// student program legitimately writes these (plt.savefig, df.to_csv/to_excel,
/// json dumps, model checkpoints, media), and there is no reliable way to tell
/// a program's own output from an external edit via hash polling — so rather
/// than suppress tamper during runs (which opened laundering holes), we simply
/// do not monitor these artifact types at all. SOURCE/answer file types
/// (.py/.ipynb/.c/.cpp/.java/.js/.ts/.txt/.md/...) ARE always monitored, so an
/// external edit of an answer is still flagged immediately — including during a
/// run. ASSUMPTION: exam answers are CODE, not hand-authored data files. If an
/// exam's answer is itself a data file (e.g. a .csv), that file would not be
/// integrity-monitored.
fn is_unmonitored_output(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    // BUILD ARTIFACTS produced by our own run pipeline. These MUST be exempt:
    // the compile step creates them BEFORE the post-run known-write pin, so a
    // 2s poll landing mid-run flagged every C/C++ run (`a.exe`) and every Java
    // run (`*.class`) as tamper_new_file. Likewise `__pycache__/*.pyc`, which
    // Python writes the moment a student imports a local module (the `utils`
    // sample package) — with numpy/pandas import time routinely exceeding one
    // poll, that produced spurious TAMPER-NEW on ordinary runs and eroded
    // trust in the signal. No exam answer is authored as a .pyc/.class/a.exe.
    if name.eq_ignore_ascii_case("a.exe") || name.eq_ignore_ascii_case("a.out") {
        return true;
    }
    let ext = match name.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        // images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "tif" | "webp" | "ico" | "svg"
        // tabular / data
        | "csv" | "tsv" | "xlsx" | "xls" | "parquet" | "feather" | "arrow"
        // serialized / model / scientific
        | "npy" | "npz" | "pkl" | "pickle" | "joblib" | "h5" | "hdf5" | "pt" | "pth" | "onnx" | "ckpt" | "pb"
        // docs / media
        | "pdf" | "docx" | "pptx" | "mp4" | "mov" | "avi" | "mkv" | "webm" | "mp3" | "wav"
        // program-written structured output / logs
        | "json" | "log"
        // build + bytecode artifacts (see note above)
        | "pyc" | "pyo" | "class" | "o" | "obj" | "pdb" | "ilk"
    )
}

/// Normalize a path key so a write registered as `테스트.py` (NFC) and a
/// filesystem read returning `테스트.py` (NFD on macOS) compare equal.
/// Falls back to byte-identical key on platforms where the normalization
/// is unavailable.
fn normalize_key(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    path.replace('\\', "/").nfc().collect::<String>()
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn hash_file(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Some(hex::encode(hasher.finalize()))
}

/// Files LARGER than this are fingerprinted by SIZE ONLY (`meta:<len>`), never
/// content-hashed. Rationale: the 2s poll fully re-read every monitored file;
/// a student importing a big monitored dataset (e.g. a 500MB corpus.txt —
/// csv/xlsx/media are already exempt) turned that into a sustained
/// hundreds-of-MB/s disk+CPU load that crippled low-spec laptops, and every
/// post-run pin re-read it again. ACCEPTED RESIDUAL RISK: an external
/// same-size in-place edit of a >32MB monitored file is not flagged (any
/// size-changing edit still is). Exam ANSWERS are code — kilobytes — and get
/// full content hashing; this affects only oversized auxiliary files.
pub const LARGE_FILE_BYTES: u64 = 32 * 1024 * 1024;

fn meta_fingerprint(len: u64) -> String {
    format!("meta:{}", len)
}

fn is_meta_fingerprint(fp: &str) -> bool {
    fp.starts_with("meta:")
}

/// Content fingerprint under the SAME size rule as the scanner, for pin sites
/// that hold the bytes in memory (ws_write_file / run_code / ws_import_file).
/// The pin and the scan MUST use one scheme — a sha pin for a file the scanner
/// fingerprints as `meta:` would never match and raise a false tamper.
pub fn content_fingerprint(bytes: &[u8]) -> String {
    if bytes.len() as u64 > LARGE_FILE_BYTES {
        return meta_fingerprint(bytes.len() as u64);
    }
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// On-disk fingerprint: size-only for large files, full SHA-256 otherwise.
fn file_fingerprint(path: &Path) -> Option<String> {
    let len = path.metadata().ok()?.len();
    if len > LARGE_FILE_BYTES {
        return Some(meta_fingerprint(len));
    }
    hash_file(path)
}

/// file_fingerprint with a brief retry — for KNOWN-WRITE PIN sites (post-run
/// pin, rename/move) where a transient read failure (e.g. a Windows
/// AV/Defender deny-share lock right after a write) would otherwise leave the
/// file unpinned and raise a one-time false tamper under the content-aware
/// grace. (Name kept from the sha-only era; it now returns the same
/// size-aware fingerprint the scanner computes.)
pub fn hash_file_retry(path: &Path) -> Option<String> {
    for _ in 0..3 {
        if let Some(h) = file_fingerprint(path) {
            return Some(h);
        }
        thread::sleep(Duration::from_millis(25));
    }
    file_fingerprint(path)
}

fn file_mtime(path: &Path) -> u64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d: std::time::Duration| d.as_secs())
        .unwrap_or(0)
}

fn scan_all_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();
    scan_dir_recursive(root, root, &mut result);
    result
}

fn scan_dir_recursive(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    scan_dir_recursive_depth(dir, root, out, 0);
}

fn scan_dir_recursive_depth(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>, depth: u32) {
    // Depth backstop: even if a reparse point slips past entry_is_link, a loop
    // cannot crash the monitor thread via unbounded recursion.
    if depth > 64 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        // Never follow symlinks / junctions — a workspace-local junction loop
        // would otherwise infinitely recurse and kill the monitor thread, and
        // a junction to an external tree would stall/bloat the scan.
        if entry_is_link(&entry) {
            continue;
        }
        let path = entry.path();
        let rel_raw = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let rel = normalize_key(&rel_raw);

        if path.is_dir() {
            // Always recurse into REAL directories. (Previously `_`/`.`-prefixed
            // dirs were skipped wholesale, leaving an unmonitored subtree a
            // student could hide cheat material in.)
            scan_dir_recursive_depth(&path, root, out, depth + 1);
        } else {
            // Skip the IDE's own artifact files AND generated-output file types
            // (program outputs are not security-relevant and there's no reliable
            // way to tell them from external edits via hashing). Everything else
            // — source/answer files — is monitored, always, even during a run.
            if is_ide_artifact_file(&rel) || is_unmonitored_output(&rel) {
                continue;
            }
            out.push((rel, path));
        }
    }
}

/// True for the IDE's own files that legitimately appear in the workspace and
/// must not be flagged as tampering (submission logs, notebook run temp, and
/// the legacy in-workspace baseline). Everything else — including arbitrary
/// `_`/`.`-prefixed student files — is monitored.
fn is_ide_artifact_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.starts_with("_log_")
        || name == ".mint_baseline.json"
        || name == ".mint_baseline.json.tmp"
        || name.starts_with("._notebook_")
}

/// True if a directory entry is a symlink (any OS) or a Windows reparse point
/// (junction / mount point). `DirEntry::metadata()` does not traverse the link.
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

/// Start a background thread that polls workspace files every 2 seconds.
pub fn start_integrity_monitor(
    workspace_root: String,
    log: LogHandle,
    app_handle: AppHandle,
    known_writes: KnownWrites,
    shared_baseline: SharedBaseline,
) {
    thread::spawn(move || {
        let root = PathBuf::from(&workspace_root);

        // Restore baseline from disk if available — closes the "restart to
        // wipe the tamper history" hole. If a student kills the IDE mid-exam
        // and modifies files externally, the next launch's monitor compares
        // against the LAST KNOWN baseline rather than re-scanning (which
        // would treat the modified state as new ground truth).
        let (mut state, baseline_tampered): (HashMap<String, FileState>, bool) =
            match load_baseline(&root) {
                BaselineLoad::Ok(files) => (files, false),
                BaselineLoad::Missing => (HashMap::new(), false),
                BaselineLoad::Invalid => (HashMap::new(), true),
            };
        // Drop any baseline entries for now-unmonitored output types (e.g. a
        // baseline written by an older build that still tracked .png/.csv). They
        // are excluded from the scan now, so without this the first poll's
        // deletion check would emit a one-time spurious tamper_deleted for files
        // that still exist on disk.
        state.retain(|rel, _| !is_unmonitored_output(rel));
        let mut state_dirty = false;

        // A present-but-unverifiable baseline means the signed baseline was
        // hand-edited / corrupted offline. Don't silently re-baseline — surface
        // it loudly (the previous code adopted whatever was on disk with no
        // event, defeating the persisted-baseline anti-restart-wipe protection).
        if baseline_tampered {
            let event = ActivityEvent::new(
                "tamper_detected",
                "INTEGRITY BASELINE INVALID: saved baseline failed signature verification (possible offline tampering). Re-establishing baseline from current disk state.",
                None,
                None,
            );
            log.add_event(event.clone());
            let _ = app_handle.emit("activity-event", &event);
        }

        // Initial scan. If we loaded a baseline, re-check every file: missing
        // entries get added (new files since shutdown), divergences raise an
        // immediate tamper event so the restart-window gap is auditable.
        thread::sleep(Duration::from_secs(1));
        let initial_files = scan_all_files(&root);
        let initial_existed = !state.is_empty();
        for (rel, full) in &initial_files {
            let new_mtime = file_mtime(full);
            let (new_hash, new_size, new_lines) = match full.metadata().ok().map(|m| m.len()) {
                Some(len) if len > LARGE_FILE_BYTES => (meta_fingerprint(len), len, 0usize),
                _ => {
                    let Some(data) = read_file_retry(full) else { continue; };
                    stat_bytes(&data)
                }
            };

            if initial_existed {
                if let Some(prev) = state.get(rel.as_str()) {
                    // FINGERPRINT-SCHEME transition (sha ↔ meta:) with the SAME
                    // size is not content evidence — it happens when a build
                    // upgrade (or threshold change) switches the scheme for an
                    // untouched file. Re-baseline silently. A scheme change
                    // with a DIFFERENT size IS evidence (the size delta) and
                    // still raises the event below.
                    let scheme_transition = is_meta_fingerprint(&prev.hash)
                        != is_meta_fingerprint(&new_hash)
                        && prev.size == new_size;
                    if prev.hash != new_hash && !scheme_transition {
                        let detail = format!(
                            "RESTART-WINDOW MODIFICATION: {} (hash differs from saved baseline)",
                            rel
                        );
                        let event = ActivityEvent::new(
                            "tamper_detected",
                            &detail,
                            Some(new_size as u32),
                            None,
                        );
                        log.add_event(event.clone());
                        let _ = app_handle.emit("activity-event", &event);
                    }
                } else {
                    let detail = format!(
                        "RESTART-WINDOW FILE ADDED: {} ({} bytes)",
                        rel, new_size
                    );
                    let event = ActivityEvent::new(
                        "tamper_new_file",
                        &detail,
                        Some(new_size as u32),
                        None,
                    );
                    log.add_event(event.clone());
                    let _ = app_handle.emit("activity-event", &event);
                }
            }

            state.insert(rel.clone(), FileState {
                hash: new_hash,
                size: new_size,
                line_count: new_lines,
                modified: new_mtime,
            });
            state_dirty = true;
        }
        if state_dirty {
            save_baseline(&root, &state);
            state_dirty = false;
        }
        // Publish the initial baseline so a rename in the first seconds after
        // launch can be recognized against known-good content.
        publish_baseline(&shared_baseline, &state);

        let mut save_counter: u32 = 0;

        loop {
            thread::sleep(Duration::from_secs(2));

            // Did anything in `state` actually change this poll? Republishing
            // the shared baseline clones every key+hash, so on a big workspace
            // doing it unconditionally every 2s was pure allocation churn. The
            // published map is already correct when nothing changed.
            let mut baseline_changed = false;
            let now = epoch_secs();
            // Purge expired known_writes entries up front so the map doesn't
            // grow unbounded (one entry per IDE write across the exam).
            if let Ok(mut map) = known_writes.lock() {
                map.retain(|_, v| v.1 >= now);
            }
            let files = scan_all_files(&root);

            // Check for modified or new files
            for (rel, full) in &files {
                let new_mtime = file_mtime(full);
                let (new_hash, new_size, new_lines) = match full.metadata().ok().map(|m| m.len()) {
                    Some(len) if len > LARGE_FILE_BYTES => (meta_fingerprint(len), len, 0usize),
                    _ => {
                        let Ok(data) = std::fs::read(full) else { continue; };
                        stat_bytes(&data)
                    }
                };

                // Is this our own (IDE-initiated) write? We do NOT remove the
                // entry on first hit — within one grace window the student may
                // save the same file twice (manual + auto save). retain() above
                // handles cleanup on expiry. CONTENT-AWARE: a pinned hash must
                // match the on-disk content; otherwise (e.g. an external
                // overwrite of the same path within the window) it is NOT known
                // and is still flagged as tampering.
                let is_known = known_writes.lock().ok()
                    .and_then(|map| map.get(rel.as_str()).cloned())
                    .map(|(expected, grace_until)| {
                        // Content-aware: a pinned hash must match the on-disk
                        // content. A time-only (None) grace does NOT excuse an
                        // in-place change or a recreate — otherwise an IDE
                        // delete/rename/move registering a blind None grace
                        // would let an external recreate of that path within the
                        // window pass as "known". (None still excuses a pure
                        // DELETION in the deletion loop below.) All IDE ops that
                        // produce content now pin the hash (mark_known_write_hash).
                        now <= grace_until
                            && match expected {
                                Some(h) => h == new_hash,
                                None => false,
                            }
                    })
                    .unwrap_or(false);

                if let Some(prev) = state.get(rel.as_str()) {
                    if prev.hash != new_hash && !is_known {
                        let size_delta: i64 = new_size as i64 - prev.size as i64;
                        let line_delta: i64 = new_lines as i64 - prev.line_count as i64;
                        let detail = format!(
                            "EXTERNAL MODIFICATION: {} (size {}→{} {:+}, lines {}→{} {:+})",
                            rel, prev.size, new_size, size_delta,
                            prev.line_count, new_lines, line_delta
                        );
                        let event = ActivityEvent::new("tamper_detected", &detail, Some(new_size as u32), None);
                        log.add_event(event.clone());
                        let _ = app_handle.emit("activity-event", &event);
                    }
                } else if !is_known {
                    let detail = format!(
                        "EXTERNAL FILE ADDED: {} ({} bytes, {} lines)",
                        rel, new_size, new_lines
                    );
                    let event = ActivityEvent::new("tamper_new_file", &detail, Some(new_size as u32), None);
                    log.add_event(event.clone());
                    let _ = app_handle.emit("activity-event", &event);
                }

                if state.get(rel.as_str()).map(|p| p.hash != new_hash).unwrap_or(true) {
                    state_dirty = true;
                    baseline_changed = true;
                }
                state.insert(rel.clone(), FileState {
                    hash: new_hash,
                    size: new_size,
                    line_count: new_lines,
                    modified: new_mtime,
                });
            }

            // Check for deleted files
            let current_rels: std::collections::HashSet<String> =
                files.iter().map(|(r, _)| r.clone()).collect();
            let deleted: Vec<String> = state.keys()
                .filter(|k| !current_rels.contains(k.as_str()))
                .cloned()
                .collect();

            for rel in &deleted {
                let is_known = known_writes.lock().ok()
                    .and_then(|mut map| map.remove(rel.as_str()))
                    .map(|(_, grace_until)| now <= grace_until)
                    .unwrap_or(false);

                if !is_known {
                    let detail = format!("EXTERNAL FILE DELETED: {}", rel);
                    let event = ActivityEvent::new("tamper_deleted", &detail, None, None);
                    log.add_event(event.clone());
                    let _ = app_handle.emit("activity-event", &event);
                }
                state.remove(rel);
                state_dirty = true;
                baseline_changed = true;
            }

            // Persist the baseline every ~30s (15 polling cycles) OR sooner if
            // something actually changed. Atomic temp+rename means a torn
            // write at IDE crash time just leaves the previous baseline.
            save_counter += 1;
            if state_dirty || save_counter >= 15 {
                save_baseline(&root, &state);
                state_dirty = false;
                save_counter = 0;
            }

            // Republish the baseline snapshot whenever it changed, so
            // rename-recognition always compares against the latest
            // known-good content.
            if baseline_changed {
                publish_baseline(&shared_baseline, &state);
            }
        }
    });
}
