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

fn count_lines(path: &Path) -> usize {
    std::fs::read(path)
        .map(|b| b.iter().filter(|&&c| c == b'\n').count() + 1)
        .unwrap_or(0)
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
            // Skip only the IDE's own artifact files; monitor everything else.
            if is_ide_artifact_file(&rel) {
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
            let Some(new_hash) = hash_file(full) else { continue; };
            let new_size = full.metadata().map(|m| m.len()).unwrap_or(0);
            let new_lines = count_lines(full);
            let new_mtime = file_mtime(full);

            if initial_existed {
                if let Some(prev) = state.get(rel.as_str()) {
                    if prev.hash != new_hash {
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

        let mut save_counter: u32 = 0;

        loop {
            thread::sleep(Duration::from_secs(2));

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
                let new_hash = match hash_file(full) {
                    Some(h) => h,
                    None => continue,
                };
                let new_size = full.metadata().map(|m| m.len()).unwrap_or(0);

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
                        now <= grace_until
                            && match expected {
                                Some(h) => h == new_hash,
                                None => true,
                            }
                    })
                    .unwrap_or(false);

                let new_lines = count_lines(full);
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
        }
    });
}
