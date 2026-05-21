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

/// Persistent on-disk snapshot of the baseline. We dot-prefix the filename
/// so the integrity monitor itself skips it (scan_dir_recursive filters
/// `.` / `_` prefixes).
const BASELINE_FILENAME: &str = ".mint_baseline.json";

#[derive(Serialize, Deserialize)]
struct BaselineSnapshot {
    saved_at: u64,
    files: HashMap<String, FileState>,
}

fn baseline_path(root: &Path) -> PathBuf {
    root.join(BASELINE_FILENAME)
}

fn load_baseline(root: &Path) -> Option<HashMap<String, FileState>> {
    let text = std::fs::read_to_string(baseline_path(root)).ok()?;
    let snap: BaselineSnapshot = serde_json::from_str(&text).ok()?;
    Some(snap.files)
}

fn save_baseline(root: &Path, state: &HashMap<String, FileState>) {
    let snap = BaselineSnapshot {
        saved_at: epoch_secs(),
        files: state.clone(),
    };
    let Ok(text) = serde_json::to_string(&snap) else { return; };
    // Atomic write: temp file + rename. A torn write would leave the next
    // restart with no baseline (worse than the previous baseline).
    let final_path = baseline_path(root);
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

/// Shared set of "known writes" — the IDE registers a path here
/// right before writing, so the integrity checker skips that change.
pub type KnownWrites = Arc<Mutex<HashMap<String, u64>>>;

pub fn new_known_writes() -> KnownWrites {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Call this from ws_write_file / ws_rename etc. so the next scan
/// doesn't flag our own write as tampering.
pub fn mark_known_write(known: &KnownWrites, relative_path: &str) {
    if let Ok(mut map) = known.lock() {
        // Grace period: 8s. The integrity loop polls every 2s, but we leave
        // headroom for filesystem buffer flush + IDE write completion +
        // network drive latency. Earlier 3s was too tight and triggered
        // false TAMPER on slow drives.
        map.insert(
            normalize_key(relative_path),
            epoch_secs() + 8,
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

fn hash_file(path: &Path) -> Option<String> {
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
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel_raw = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let rel = normalize_key(&rel_raw);

        // Skip our own log files
        if rel.starts_with('_') || rel.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            scan_dir_recursive(&path, root, out);
        } else {
            out.push((rel, path));
        }
    }
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
        let mut state: HashMap<String, FileState> = load_baseline(&root).unwrap_or_default();
        let mut state_dirty = false;

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
                map.retain(|_, &mut grace_until| grace_until >= now);
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

                // Check if this is a known (IDE-initiated) write. We do NOT
                // remove the entry on first hit — within one grace window the
                // student may save the same file twice (manual save + auto
                // save). The retain() above handles cleanup on expiry.
                let is_known = known_writes.lock().ok()
                    .and_then(|map| map.get(rel.as_str()).copied())
                    .map(|grace_until| now <= grace_until)
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
                    .map(|grace_until| now <= grace_until)
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
