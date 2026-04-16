use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize)]
pub struct FileNode {
    pub name: String,
    pub path: String,           // relative path from workspace root
    pub is_dir: bool,
    pub children: Vec<FileNode>,
}

pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Creates a new isolated workspace in a temp-like location.
    /// The directory is created if it doesn't exist.
    pub fn init(base_dir: &Path, session_name: &str) -> Result<Self, String> {
        let root = base_dir.join(session_name);
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("Failed to create workspace: {}", e))?;
        Ok(Self { root })
    }

    pub fn root_path(&self) -> String {
        self.root.to_string_lossy().to_string()
    }

    /// Recursively scan the workspace directory and return a file tree.
    pub fn list_tree(&self) -> Result<Vec<FileNode>, String> {
        scan_dir(&self.root, &self.root)
    }

    /// Read a file inside the workspace. Rejects paths outside the workspace.
    pub fn read_file(&self, relative_path: &str) -> Result<String, String> {
        let full = self.resolve_safe(relative_path)?;
        std::fs::read_to_string(&full)
            .map_err(|e| format!("Failed to read {}: {}", relative_path, e))
    }

    /// Write/create a file inside the workspace.
    pub fn write_file(&self, relative_path: &str, content: &str) -> Result<(), String> {
        let full = self.resolve_safe(relative_path)?;
        // Ensure parent directories exist
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }
        std::fs::write(&full, content)
            .map_err(|e| format!("Failed to write {}: {}", relative_path, e))
    }

    /// Create a directory inside the workspace.
    pub fn create_dir(&self, relative_path: &str) -> Result<(), String> {
        let full = self.resolve_safe(relative_path)?;
        std::fs::create_dir_all(&full)
            .map_err(|e| format!("Failed to create directory {}: {}", relative_path, e))
    }

    /// Rename a file or directory.
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), String> {
        let old_full = self.resolve_safe(old_path)?;
        let new_full = self.resolve_safe(new_path)?;
        std::fs::rename(&old_full, &new_full)
            .map_err(|e| format!("Failed to rename: {}", e))
    }

    /// Delete a file or empty directory.
    pub fn delete(&self, relative_path: &str) -> Result<(), String> {
        let full = self.resolve_safe(relative_path)?;
        if full.is_dir() {
            std::fs::remove_dir_all(&full)
                .map_err(|e| format!("Failed to delete directory: {}", e))
        } else {
            std::fs::remove_file(&full)
                .map_err(|e| format!("Failed to delete file: {}", e))
        }
    }

    /// Public version for import command
    pub fn resolve_safe_for_write(&self, relative_path: &str) -> Result<PathBuf, String> {
        self.resolve_safe(relative_path)
    }

    /// Resolve a relative path and ensure it doesn't escape the workspace root.
    /// Security: rejects any path containing ".." segments or absolute paths.
    fn resolve_safe(&self, relative_path: &str) -> Result<PathBuf, String> {
        let normalized = relative_path.replace('\\', "/");

        // Reject absolute paths
        if normalized.starts_with('/') || normalized.contains(':') {
            return Err("Absolute paths are not allowed".to_string());
        }

        // Reject any ".." segment — blocks traversal before filesystem resolution
        for segment in normalized.split('/') {
            if segment == ".." {
                return Err("Path traversal (..) is not allowed".to_string());
            }
        }

        let full = self.root.join(&normalized);
        let canon_root = self.root.canonicalize().map_err(|e| e.to_string())?;

        // For existing paths, canonicalize and verify prefix
        if full.exists() {
            let canon = full.canonicalize().map_err(|e| e.to_string())?;
            if !canon.starts_with(&canon_root) {
                return Err("Path escapes workspace boundary".to_string());
            }
        } else {
            // For new paths, walk up to the nearest existing ancestor and verify
            let mut ancestor = full.parent();
            while let Some(a) = ancestor {
                if a.exists() {
                    let canon_a = a.canonicalize().map_err(|e| e.to_string())?;
                    if !canon_a.starts_with(&canon_root) {
                        return Err("Path escapes workspace boundary".to_string());
                    }
                    break;
                }
                ancestor = a.parent();
            }
        }

        Ok(full)
    }
}

fn scan_dir(dir: &Path, root: &Path) -> Result<Vec<FileNode>, String> {
    let mut entries = Vec::new();

    let read_dir = std::fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory: {}", e))?;

    let mut items: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
    // Sort: directories first, then alphabetical
    items.sort_by(|a, b| {
        let a_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.file_name().cmp(&b.file_name()),
        }
    });

    for entry in items {
        let name = entry.file_name().to_string_lossy().to_string();
        let full_path = entry.path();
        let relative = full_path.strip_prefix(root)
            .unwrap_or(&full_path)
            .to_string_lossy()
            .replace('\\', "/");
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        let children = if is_dir {
            scan_dir(&full_path, root)?
        } else {
            Vec::new()
        };

        entries.push(FileNode {
            name,
            path: relative,
            is_dir,
            children,
        });
    }

    Ok(entries)
}

pub type WorkspaceState = Mutex<Option<Workspace>>;
