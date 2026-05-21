use std::process::Command;

fn main() {
    // Embed git commit SHA (short form) for tamper detection.
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { String::from_utf8(o.stdout).ok() } else { None })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MINT_GIT_SHA={}", git_sha);

    // Embed build timestamp (UTC ISO-8601).
    let build_time = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    println!("cargo:rustc-env=MINT_BUILD_TIME={}", build_time);

    // Rebuild if HEAD changes.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads/main");
    println!("cargo:rerun-if-changed=app.manifest");

    // Embed our custom Windows app manifest (longPathAware + UTF-8 ANSI
    // codepage + PerMonitorV2 DPI). Without this, Korean Windows + long
    // path users hit ERROR_FILENAME_EXCED_RANGE on ws_* operations.
    #[cfg(windows)]
    let attributes = tauri_build::Attributes::new().windows_attributes(
        tauri_build::WindowsAttributes::new_without_app_manifest()
            .app_manifest(include_str!("app.manifest")),
    );
    #[cfg(not(windows))]
    let attributes = tauri_build::Attributes::new();

    if let Err(error) = tauri_build::try_build(attributes) {
        panic!("error found during tauri-build: {error:#}");
    }
}
