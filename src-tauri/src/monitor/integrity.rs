use super::log::{ActivityEvent, LogHandle};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Tracks file hashes to detect external modifications.
/// Any change not made through our IDE is flagged as TAMPER.
struct FileState {
    hash: String,
    size: u64,
    modified: u64, // mtime as epoch secs
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
        map.insert(
            relative_path.replace('\\', "/"),
            epoch_secs() + 3, // grace period: ignore changes within 3s
        );
    }
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
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

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
        let mut state: HashMap<String, FileState> = HashMap::new();

        // Initial scan — baseline
        thread::sleep(Duration::from_secs(1));
        for (rel, full) in scan_all_files(&root) {
            if let Some(h) = hash_file(&full) {
                let meta = full.metadata().ok();
                state.insert(rel, FileState {
                    hash: h,
                    size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    modified: file_mtime(&full),
                });
            }
        }

        loop {
            thread::sleep(Duration::from_secs(5));

            let now = epoch_secs();
            let files = scan_all_files(&root);

            // Check for modified or new files
            for (rel, full) in &files {
                let new_mtime = file_mtime(full);
                let new_hash = match hash_file(full) {
                    Some(h) => h,
                    None => continue,
                };
                let new_size = full.metadata().map(|m| m.len()).unwrap_or(0);

                // Check if this is a known (IDE-initiated) write
                let is_known = {
                    if let Ok(mut map) = known_writes.lock() {
                        if let Some(&grace_until) = map.get(rel.as_str()) {
                            if now <= grace_until {
                                // Our write — update state silently
                                map.remove(rel.as_str());
                                true
                            } else {
                                map.remove(rel.as_str());
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if let Some(prev) = state.get(rel.as_str()) {
                    if prev.hash != new_hash && !is_known {
                        // TAMPER DETECTED
                        let detail = format!(
                            "EXTERNAL MODIFICATION: {} (size {}→{}, hash changed)",
                            rel, prev.size, new_size
                        );
                        let event = ActivityEvent::new("tamper_detected", &detail, Some(new_size as u32), None);
                        log.add_event(event.clone());
                        let _ = app_handle.emit("activity-event", &event);
                    }
                } else if !is_known {
                    // New file appeared externally
                    let detail = format!(
                        "EXTERNAL FILE ADDED: {} ({} bytes)",
                        rel, new_size
                    );
                    let event = ActivityEvent::new("tamper_new_file", &detail, Some(new_size as u32), None);
                    log.add_event(event.clone());
                    let _ = app_handle.emit("activity-event", &event);
                }

                // Update state
                state.insert(rel.clone(), FileState {
                    hash: new_hash,
                    size: new_size,
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
                let is_known = {
                    if let Ok(mut map) = known_writes.lock() {
                        map.remove(rel.as_str()).is_some()
                    } else { false }
                };

                if !is_known {
                    let detail = format!("EXTERNAL FILE DELETED: {}", rel);
                    let event = ActivityEvent::new("tamper_deleted", &detail, None, None);
                    log.add_event(event.clone());
                    let _ = app_handle.emit("activity-event", &event);
                }
                state.remove(rel);
            }
        }
    });
}
