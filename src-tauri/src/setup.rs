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
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            setup_done: false,
            package_profile: "basic".to_string(),
            custom_packages: Vec::new(),
            recording_enabled: true,
            include_sample_code: true,
            config_version: 1,
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
        .join("MINT_Exam_IDE")
        .join("setup_config.json")
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

pub fn package_list_for_profile(profile: &str, custom: &[String]) -> Vec<String> {
    match profile {
        "basic" => vec![
            "numpy".into(), "pandas".into(), "matplotlib".into(),
            "Pillow".into(), "openpyxl".into(), "requests".into(),
        ],
        "ds" => vec![
            "numpy".into(), "pandas".into(), "matplotlib".into(), "seaborn".into(),
            "scikit-learn".into(), "scipy".into(), "sympy".into(),
            "Pillow".into(), "opencv-python-headless".into(),
            "openpyxl".into(), "requests".into(),
        ],
        "dl" => vec![
            "numpy".into(), "pandas".into(), "matplotlib".into(), "seaborn".into(),
            "scikit-learn".into(), "scipy".into(), "sympy".into(),
            "Pillow".into(), "opencv-python-headless".into(),
            "openpyxl".into(), "requests".into(),
            "torch".into(), "torchvision".into(), "torchaudio".into(),
            "tensorflow-cpu".into(),
        ],
        "custom" => custom.to_vec(),
        _ => Vec::new(),
    }
}
