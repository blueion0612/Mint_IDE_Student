use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupConfig {
    pub setup_done: bool,
    pub package_profile: String,
    pub custom_packages: Vec<String>,
    pub recording_enabled: bool,
    pub include_sample_code: bool,
    pub config_version: u32,
    #[serde(default)]
    pub custom_venv_path: Option<String>,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            setup_done: false,
            package_profile: "basic".to_string(),
            custom_packages: Vec::new(),
            recording_enabled: true,
            include_sample_code: true,
            config_version: 2,
            custom_venv_path: None,
        }
    }
}

pub fn app_data_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
        .join("MINT_Exam_IDE")
}

pub fn default_venv_path() -> PathBuf {
    app_data_root().join("exam-venv")
}

pub fn config_path() -> PathBuf {
    app_data_root().join("setup_config.json")
}

pub fn load_config() -> SetupConfig {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<SetupConfig>(&text) {
            return cfg;
        }
    }
    SetupConfig::default()
}

pub fn save_config(cfg: &SetupConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// Workspaces live under %LOCALAPPDATA%\MINT_Exam_IDE\Workspaces — NOT in
/// Documents — so a casual student opening File Explorer doesn't trip over
/// their own source. AppData\Local is hidden by default on Windows shell.
/// Combined with the explicit hidden attribute set in Workspace::init this
/// keeps honest students from accidentally editing files outside the IDE.
pub fn workspaces_dir() -> PathBuf {
    app_data_root().join("Workspaces")
}

/// Recordings live under %LOCALAPPDATA%\MINT_Exam_IDE\Recordings — a LOCAL,
/// non-redirected, hidden location. Documents is frequently OneDrive-redirected,
/// and ffmpeg writing the live mp4 into a OneDrive-synced folder stalls/locks
/// (observed: a 48-byte capture that never grows). LOCALAPPDATA is local and
/// not synced, so capture is reliable, and the student can't casually reach it.
/// At submit the obfuscated copy is written to the Desktop submission folder
/// and the local original is deleted.
pub fn recordings_dir() -> PathBuf {
    app_data_root().join("Recordings")
}

/// Hardcoded, mutually-verified package versions for Python 3.12.x.
/// Wheel ABI is `cp312-*` so any 3.12.X patch works (the installer
/// ships Astral's portable Python 3.12.13). Bump in sync only when the
/// dedicated Python's minor version changes.
pub fn package_list_for_profile(profile: &str, custom: &[String]) -> Vec<String> {
    match profile {
        "basic" => vec![
            "numpy==2.1.3".into(),
            "pandas==2.2.3".into(),
            "matplotlib==3.10.0".into(),
            "Pillow==11.0.0".into(),
            "openpyxl==3.1.5".into(),
            "requests==2.32.3".into(),
        ],
        "ds" => vec![
            "numpy==2.1.3".into(),
            "pandas==2.2.3".into(),
            "matplotlib==3.10.0".into(),
            "seaborn==0.13.2".into(),
            "scikit-learn==1.5.2".into(),
            "scipy==1.14.1".into(),
            "sympy==1.13.3".into(),
            "Pillow==11.0.0".into(),
            "opencv-python-headless==4.10.0.84".into(),
            "openpyxl==3.1.5".into(),
            "requests==2.32.3".into(),
        ],
        "dl" => vec![
            "numpy==2.1.3".into(),
            "pandas==2.2.3".into(),
            "matplotlib==3.10.0".into(),
            "seaborn==0.13.2".into(),
            "scikit-learn==1.5.2".into(),
            "scipy==1.14.1".into(),
            "sympy==1.13.3".into(),
            "Pillow==11.0.0".into(),
            "opencv-python-headless==4.10.0.84".into(),
            "openpyxl==3.1.5".into(),
            "requests==2.32.3".into(),
            "torch==2.5.1".into(),
            "torchvision==0.20.1".into(),
            "torchaudio==2.5.1".into(),
            "tensorflow-cpu==2.18.0".into(),
        ],
        "custom" => custom.to_vec(),
        _ => Vec::new(),
    }
}
