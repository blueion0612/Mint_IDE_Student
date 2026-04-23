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

pub fn mint_exam_root() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MINT_Exam")
}

pub fn workspaces_dir() -> PathBuf {
    mint_exam_root().join("Workspaces")
}

pub fn recordings_dir() -> PathBuf {
    mint_exam_root().join("Recordings")
}

/// Hardcoded, mutually-verified package versions for Python 3.12.8.
/// Bump in sync whenever the dedicated Python version changes.
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
