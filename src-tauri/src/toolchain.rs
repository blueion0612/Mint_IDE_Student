//! C / C++ toolchain discovery, verification and build-command construction.
//!
//! This mirrors what `runner::discover_python` + `lib::detect_pythons` +
//! `lib::verify_exam_environment` do for Python, so the C++ side of the IDE has
//! the same three guarantees Python already had:
//!
//!   1. the compiler is FOUND without the student touching PATH,
//!   2. the student can SEE which compiler is bound and pick another,
//!   3. the environment is VERIFIED (compile + link + run a probe) before an
//!      exam starts, rather than failing on the first Run.
//!
//! Windows deserves the long comment. `install-windows.ps1` unpacks a pinned
//! portable MinGW-w64 (WinLibs GCC, UCRT runtime) to `C:\ProgramData\MINT_MinGW`
//! exactly like the pinned portable Python, so every student compiles with a
//! byte-identical toolchain. Discovery must NOT rely on PATH for it: the
//! installer extracts after the IDE's environment block was inherited, and the
//! same stale-PATH trap already bit ffmpeg once (v5.0.1 — winget machine-scope
//! installs were invisible until discovery scanned the install roots directly).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// Cached compiler path — discovery walks the filesystem, so do it once.
static CACHED_CXX: Mutex<Option<String>> = Mutex::new(None);
static CACHED_CC: Mutex<Option<String>> = Mutex::new(None);

/// Root of the portable MinGW-w64 that `install-windows.ps1` unpacks.
/// ASCII path on purpose: a Korean username in the path broke MSI-era Python
/// and would equally break a toolchain whose own build scripts are not
/// unicode-clean.
#[cfg(target_os = "windows")]
pub const MINT_MINGW_ROOT: &str = "C:\\ProgramData\\MINT_MinGW";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilerInfo {
    /// Absolute path (or bare command name when only PATH resolution is available).
    pub path: String,
    /// First line of `--version`, e.g. "g++ (MinGW-W64 …) 15.3.0".
    pub version: String,
    /// "gcc" | "clang" | "unknown"
    pub kind: String,
    /// True for the toolchain MINT itself installed — shown first and preferred.
    pub is_mint: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CppVerifyResult {
    pub ok: bool,
    pub compiler: String,
    pub version: String,
    /// Which stage failed: "" | "discover" | "compile" | "link" | "run" | "output"
    pub failed_stage: String,
    pub message: String,
    /// Standard the probe was compiled with, e.g. "c++17".
    pub standard: String,
}

/// Hide the console window that gcc/g++/clang would otherwise flash on Windows.
/// Every probe in this module spawns a process; without this the student sees a
/// black window blink each time the IDE checks its toolchain.
fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

/// `<exe> --version` first line, or None if the binary does not run.
///
/// Requires a SUCCESS exit status: a Microsoft Store `python.exe`-style stub
/// (or a broken shim) prints to stderr and exits non-zero, and treating that as
/// a working compiler put a garbage entry in the selector once already.
pub fn probe_version(exe: &str) -> Option<String> {
    let out = quiet(Command::new(exe).arg("--version"))
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim().to_string();
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

fn kind_of(version: &str) -> String {
    let v = version.to_ascii_lowercase();
    if v.contains("clang") {
        "clang".to_string()
    } else if v.contains("g++") || v.contains("gcc") || v.contains("free software foundation") {
        "gcc".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Candidate absolute paths for a C++ (or C) driver, most-preferred first.
/// `cpp = true` returns C++ drivers (g++/clang++), `false` returns C drivers.
fn candidate_paths(cpp: bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        let exe = if cpp { "g++.exe" } else { "gcc.exe" };
        let clang_exe = if cpp { "clang++.exe" } else { "clang.exe" };

        // 1. The toolchain MINT installed. WinLibs archives unpack to a
        //    `mingw64/` (64-bit) or `mingw32/` directory; accept the root too in
        //    case a future layout drops the extra level.
        for sub in ["mingw64\\bin", "mingw32\\bin", "bin"] {
            out.push(PathBuf::from(MINT_MINGW_ROOT).join(sub).join(exe));
        }

        // 2. winget-installed WinLibs, BOTH scopes. A machine-scope winget
        //    install lands under %ProgramFiles%\WinGet\Packages and is invisible
        //    to a user-scope-only scan — the exact bug that made first-launch
        //    recording fail in v5.0.0 (fixed in v5.0.1 for ffmpeg).
        let mut winget_roots: Vec<PathBuf> = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            winget_roots.push(PathBuf::from(&local).join("Microsoft\\WinGet\\Packages"));
        }
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(pf) = std::env::var(var) {
                winget_roots.push(PathBuf::from(&pf).join("WinGet\\Packages"));
            }
        }
        for root in winget_roots {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if !p.is_dir() {
                        continue;
                    }
                    // WinLibs / MSYS2 / mingw-builds all end in a bin dir one or
                    // two levels down; probe the shapes we know.
                    for sub in ["mingw64\\bin", "mingw32\\bin", "ucrt64\\bin", "bin"] {
                        out.push(p.join(sub).join(exe));
                    }
                }
            }
        }

        // 3. Conventional standalone installs.
        for root in [
            "C:\\mingw64",
            "C:\\mingw32",
            "C:\\MinGW",
            "C:\\msys64\\ucrt64",
            "C:\\msys64\\mingw64",
            "C:\\msys64\\clang64",
            "C:\\TDM-GCC-64",
            "C:\\Strawberry\\c",
        ] {
            out.push(PathBuf::from(root).join("bin").join(exe));
        }
        // Bundled-with-an-IDE toolchains students often already have.
        for root in [
            "C:\\Program Files\\CodeBlocks\\MinGW",
            "C:\\Program Files (x86)\\CodeBlocks\\MinGW",
            "C:\\Program Files (x86)\\Dev-Cpp\\MinGW64",
            "C:\\Dev-Cpp\\MinGW64",
        ] {
            out.push(PathBuf::from(root).join("bin").join(exe));
        }
        // LLVM, last: on Windows clang defaults to an MSVC-style driver and
        // needs MSVC's linker/CRT, which a student machine may not have.
        for root in ["C:\\Program Files\\LLVM", "C:\\Program Files (x86)\\LLVM"] {
            out.push(PathBuf::from(root).join("bin").join(clang_exe));
        }
    }

    #[cfg(target_os = "macos")]
    {
        let clang_exe = if cpp { "clang++" } else { "clang" };
        // Xcode CLT is what install-mac.sh guarantees, and its clang++ is the
        // only compiler on macOS that links against the SDK without extra flags.
        out.push(PathBuf::from("/usr/bin").join(clang_exe));
        // Homebrew GCC, if the student installed it. Versioned first: bare `g++`
        // on macOS is an Apple clang SHIM, so an unversioned hit says nothing
        // about which compiler is really behind it.
        let gcc_exe = if cpp { "g++" } else { "gcc" };
        for prefix in ["/opt/homebrew/bin", "/usr/local/bin"] {
            for v in ["-15", "-14", "-13", "-12"] {
                out.push(PathBuf::from(prefix).join(format!("{}{}", gcc_exe, v)));
            }
        }
        for prefix in ["/opt/homebrew/opt/llvm/bin", "/usr/local/opt/llvm/bin"] {
            out.push(PathBuf::from(prefix).join(clang_exe));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let gcc_exe = if cpp { "g++" } else { "gcc" };
        let clang_exe = if cpp { "clang++" } else { "clang" };
        for prefix in ["/usr/bin", "/usr/local/bin", "/bin"] {
            out.push(PathBuf::from(prefix).join(gcc_exe));
            out.push(PathBuf::from(prefix).join(clang_exe));
        }
    }

    out
}

/// Bare command names to try through PATH once the known locations miss.
fn path_candidates(cpp: bool) -> &'static [&'static str] {
    if cpp {
        &["g++", "clang++", "c++"]
    } else {
        &["gcc", "clang", "cc"]
    }
}

/// Find a usable compiler driver. `explicit` (from settings) always wins.
pub fn find_compiler(explicit: Option<&str>, cpp: bool) -> Option<String> {
    if let Some(p) = explicit {
        let p = p.trim();
        // A settings path that no longer exists must not break every Run.
        // Students uninstall things, and a stale entry would otherwise turn
        // into "could not start the compiler" on every compile with no hint
        // that the fix is in settings. An absolute path is checked on disk (one
        // stat, cheap enough for the Run path); a bare command name is left to
        // PATH resolution, which is the only thing that can answer it.
        let usable = !p.is_empty()
            && (!looks_absolute(p) || Path::new(p).is_file());
        if usable {
            return Some(p.to_string());
        }
    }

    let cache = if cpp { &CACHED_CXX } else { &CACHED_CC };
    if let Ok(guard) = cache.lock() {
        if let Some(ref hit) = *guard {
            return Some(hit.clone());
        }
    }

    let found = discover_compiler(cpp);
    if let Some(ref c) = found {
        if let Ok(mut guard) = cache.lock() {
            *guard = Some(c.clone());
        }
    }
    found
}

/// Drop the discovery cache. Called after an install/settings change so a newly
/// installed toolchain is picked up without restarting the IDE.
pub fn clear_compiler_cache() {
    if let Ok(mut g) = CACHED_CXX.lock() {
        *g = None;
    }
    if let Ok(mut g) = CACHED_CC.lock() {
        *g = None;
    }
}

fn discover_compiler(cpp: bool) -> Option<String> {
    for cand in candidate_paths(cpp) {
        if cand.is_file() {
            // Existence is not enough — a half-extracted archive leaves a file
            // that cannot execute. Only accept it if it actually reports a
            // version.
            let s = cand.to_string_lossy().to_string();
            if probe_version(&s).is_some() {
                return Some(s);
            }
        }
    }
    for name in path_candidates(cpp) {
        if probe_version(name).is_some() {
            return Some(name.to_string());
        }
    }
    None
}

/// Every compiler we can find, for the settings selector. Deduplicated by
/// resolved path so the same toolchain reached through two roots is listed once.
pub fn detect_compilers(cpp: bool) -> Vec<CompilerInfo> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<CompilerInfo> = Vec::new();

    let consider = |path_str: String, out: &mut Vec<CompilerInfo>, seen: &mut std::collections::HashSet<String>| {
        // Canonicalize so C:\mingw64\bin\g++.exe reached twice is one entry.
        let key = std::fs::canonicalize(&path_str)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path_str.clone());
        if !seen.insert(key) {
            return;
        }
        if let Some(version) = probe_version(&path_str) {
            let is_mint = is_mint_toolchain(&path_str);
            out.push(CompilerInfo {
                kind: kind_of(&version),
                path: path_str,
                version,
                is_mint,
            });
        }
    };

    for cand in candidate_paths(cpp) {
        if cand.is_file() {
            consider(cand.to_string_lossy().to_string(), &mut out, &mut seen);
        }
    }
    for name in path_candidates(cpp) {
        // Resolve the PATH hit to a real path so it dedupes against the
        // absolute candidates above instead of showing up as a twin entry.
        if let Some(resolved) = which(name) {
            consider(resolved, &mut out, &mut seen);
        }
    }

    // MINT's own toolchain first, then GCC before Clang, then by path.
    out.sort_by(|a, b| {
        b.is_mint
            .cmp(&a.is_mint)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn is_mint_toolchain(path: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        return path
            .to_ascii_lowercase()
            .starts_with(&MINT_MINGW_ROOT.to_ascii_lowercase());
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        false
    }
}

/// Whether a configured compiler path names a location on disk rather than a
/// command to resolve through PATH.
fn looks_absolute(p: &str) -> bool {
    p.contains('/') || p.contains('\\')
}

/// Resolve a bare command through PATH without spawning a shell.
fn which(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        #[cfg(target_os = "windows")]
        let cands = [
            dir.join(format!("{}.exe", name)),
            dir.join(format!("{}.cmd", name)),
            dir.join(format!("{}.bat", name)),
        ];
        #[cfg(not(target_os = "windows"))]
        let cands = [dir.join(name)];

        for c in cands.iter() {
            if c.is_file() {
                return Some(c.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Directory holding the compiler, so the runtime DLLs that live beside it can
/// be put on the child's PATH. MinGW builds link `libstdc++-6.dll`,
/// `libgcc_s_seh-1.dll` and `libwinpthread-1.dll` dynamically unless told
/// otherwise; we DO pass `-static`, but a student-selected compiler may ignore
/// it, and prepending the bin dir costs nothing and removes a whole class of
/// "the exe silently fails to start" reports.
pub fn compiler_bin_dir(compiler: &str) -> Option<PathBuf> {
    Path::new(compiler).parent().map(|p| p.to_path_buf())
}

/// Compile + link + run a tiny program that exercises the parts an exam
/// actually uses: iostream, string, vector, algorithm, and reading from stdin.
/// Returns which stage failed so the wizard can say something more useful than
/// "C++ not working".
pub fn verify_cpp(explicit: Option<&str>, standard: &str) -> CppVerifyResult {
    let std_flag = normalize_standard(standard);
    let compiler = match find_compiler(explicit, true) {
        Some(c) => c,
        None => {
            return CppVerifyResult {
                ok: false,
                compiler: String::new(),
                version: String::new(),
                failed_stage: "discover".to_string(),
                message: "C++ 컴파일러를 찾지 못했습니다. 설치 스크립트를 다시 실행하거나 설정에서 컴파일러 경로를 지정하세요.".to_string(),
                standard: std_flag,
            };
        }
    };
    let version = probe_version(&compiler).unwrap_or_default();

    // Unique per call, not just per process: the wizard's "compile test" button
    // and the status-bar one can both be in flight, and the cleanup guard below
    // removes the whole directory on the way out — a shared path would let one
    // run delete the other's executable mid-probe.
    let dir = std::env::temp_dir().join(format!(
        "mint-cpp-verify-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return CppVerifyResult {
            ok: false,
            compiler,
            version,
            failed_stage: "compile".to_string(),
            message: format!("임시 폴더를 만들지 못했습니다: {}", e),
            standard: std_flag,
        };
    }
    // Best-effort cleanup guard: the probe leaves nothing behind even on an
    // early return, so a student who re-runs verification 20 times does not
    // accumulate junk in %TEMP%.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());

    let src = dir.join("mint_probe.cpp");
    let exe = dir.join(if cfg!(windows) { "mint_probe.exe" } else { "mint_probe" });
    const PROBE: &str = r#"#include <iostream>
#include <string>
#include <vector>
#include <algorithm>
int main() {
    std::vector<int> v{3, 1, 2};
    std::sort(v.begin(), v.end());
    std::string line;
    std::getline(std::cin, line);
    std::cout << "MINT_CPP_OK " << v[0] << v[1] << v[2] << " " << line << std::endl;
    return 0;
}
"#;
    if let Err(e) = std::fs::write(&src, PROBE) {
        return CppVerifyResult {
            ok: false,
            compiler,
            version,
            failed_stage: "compile".to_string(),
            message: format!("probe 소스를 쓰지 못했습니다: {}", e),
            standard: std_flag,
        };
    }

    let mut args = base_compile_args(&std_flag);
    args.push(src.to_string_lossy().to_string());
    args.push("-o".to_string());
    args.push(exe.to_string_lossy().to_string());

    let mut compile_cmd = Command::new(&compiler);
    compile_cmd.args(&args).stdin(Stdio::null());
    compile_env(&mut compile_cmd);
    let compile = quiet(&mut compile_cmd).output();
    match compile {
        Ok(o) if !o.status.success() => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            // "undefined reference" / "cannot find -l" means the driver ran but
            // linking failed — a distinct, more actionable failure.
            let stage = if err.contains("undefined reference") || err.contains("cannot find -l") || err.contains("ld returned") {
                "link"
            } else {
                "compile"
            };
            return CppVerifyResult {
                ok: false,
                compiler,
                version,
                failed_stage: stage.to_string(),
                message: cap(&err, 4000),
                standard: std_flag,
            };
        }
        Err(e) => {
            return CppVerifyResult {
                ok: false,
                compiler: compiler.clone(),
                version,
                failed_stage: "compile".to_string(),
                message: format!("컴파일러를 실행하지 못했습니다 ({}): {}", compiler, e),
                standard: std_flag,
            };
        }
        _ => {}
    }

    // Run it, feeding a line on stdin so the probe also proves that a compiled
    // binary can READ input — the single most common thing an exam program does
    // and the thing that was silently broken before stdin was wired up.
    let mut run = Command::new(&exe);
    run.current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(bin) = compiler_bin_dir(&compiler) {
        prepend_path(&mut run, &bin);
    }
    let child = quiet(&mut run).spawn();
    let output = match child {
        Ok(mut c) => {
            use std::io::Write;
            if let Some(mut sin) = c.stdin.take() {
                let _ = sin.write_all(b"stdin-works\n");
                let _ = sin.flush();
            }
            c.wait_with_output()
        }
        Err(e) => {
            return CppVerifyResult {
                ok: false,
                compiler,
                version,
                failed_stage: "run".to_string(),
                message: format!("컴파일은 됐지만 실행에 실패했습니다: {}", e),
                standard: std_flag,
            };
        }
    };

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.contains("MINT_CPP_OK 123 stdin-works") {
                CppVerifyResult {
                    ok: true,
                    compiler,
                    version,
                    failed_stage: String::new(),
                    message: "컴파일 · 링크 · 실행 · 표준입력 모두 정상입니다.".to_string(),
                    standard: std_flag,
                }
            } else {
                CppVerifyResult {
                    ok: false,
                    compiler,
                    version,
                    failed_stage: "output".to_string(),
                    message: format!(
                        "probe 출력이 예상과 다릅니다.\nstdout: {}\nstderr: {}",
                        cap(stdout.trim(), 500),
                        cap(String::from_utf8_lossy(&o.stderr).trim(), 500)
                    ),
                    standard: std_flag,
                }
            }
        }
        Err(e) => CppVerifyResult {
            ok: false,
            compiler,
            version,
            failed_stage: "run".to_string(),
            message: format!("probe 실행 결과를 읽지 못했습니다: {}", e),
            standard: std_flag,
        },
    }
}

/// Prepend `dir` to the PATH of a command being built.
pub fn prepend_path(cmd: &mut Command, dir: &Path) {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

/// Accepts "c++17", "17", "gnu++20", "" … and returns a canonical `c++NN`.
/// An unrecognised value falls back to the exam default rather than being
/// passed through to the compiler, so a corrupted config cannot make every Run
/// fail with "unrecognized command line option".
pub fn normalize_standard(input: &str) -> String {
    let s = input.trim().to_ascii_lowercase();
    const ALLOWED: [&str; 8] = [
        "c++11", "c++14", "c++17", "c++20", "c++23", "gnu++17", "gnu++20", "gnu++23",
    ];
    if ALLOWED.contains(&s.as_str()) {
        return s;
    }
    let bare = format!("c++{}", s);
    if ALLOWED.contains(&bare.as_str()) {
        return bare;
    }
    DEFAULT_CPP_STANDARD.to_string()
}

pub const DEFAULT_CPP_STANDARD: &str = "c++17";
pub const DEFAULT_C_STANDARD: &str = "c17";

pub fn normalize_c_standard(input: &str) -> String {
    let s = input.trim().to_ascii_lowercase();
    const ALLOWED: [&str; 6] = ["c99", "c11", "c17", "gnu99", "gnu11", "gnu17"];
    if ALLOWED.contains(&s.as_str()) {
        return s;
    }
    DEFAULT_C_STANDARD.to_string()
}

/// Flags shared by the verification probe and real Runs.
///
/// `-static` matters on Windows: a MinGW binary otherwise needs
/// `libstdc++-6.dll` / `libgcc_s_seh-1.dll` / `libwinpthread-1.dll` beside it or
/// on PATH, and a student who copies their `.exe` elsewhere — or runs it after
/// the IDE has exited — gets a wordless "the application was unable to start"
/// box. Static linking costs a few hundred KB and removes the whole class.
///
/// `-g` is deliberately absent: debug info triples link time on the slow disks
/// these exams run on and nothing in the IDE reads it.
pub fn base_compile_args(std_flag: &str) -> Vec<String> {
    // Deliberately MINIMAL. Every flag here must be accepted by both GCC (the
    // pinned MinGW toolchain on Windows, and Linux distros) and Apple Clang
    // (macOS), across the versions students will actually have. A flag one of
    // them rejects does not degrade gracefully — it fails EVERY compile on that
    // platform, which is exactly the kind of breakage that cannot be diagnosed
    // from inside an exam.
    //
    // Two cosmetic flags were dropped for that reason: `-fdiagnostics-color` is
    // unnecessary because both compilers already emit no colour when stdout is a
    // pipe (measured, not assumed), and diagnostic locale is pinned through the
    // environment instead (see `compile_env`).
    let mut args: Vec<String> = vec![
        format!("-std={}", std_flag),
        "-O2".to_string(),
        // Warnings students should see, without -Werror: an exam answer that
        // warns must still RUN.
        "-Wall".to_string(),
    ];
    if cfg!(windows) {
        args.push("-static".to_string());
    }
    args
}

/// Environment for a compiler invocation.
///
/// GCC and Clang translate their diagnostics when the locale says to. Korean
/// diagnostics are perfectly readable, but the output panel classifies lines by
/// matching `error:` / `warning:` / `note:` to pick out line numbers for the
/// editor's error highlighting, and a translated message breaks that silently.
/// Pinning the compiler's locale — not the student's — keeps both behaviours
/// predictable on every machine.
pub fn compile_env(cmd: &mut Command) {
    cmd.env("LC_ALL", "C");
    cmd.env("LANG", "C");
}

fn cap(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… (생략)", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_normalization_accepts_known_and_falls_back() {
        assert_eq!(normalize_standard("c++17"), "c++17");
        assert_eq!(normalize_standard("C++20"), "c++20");
        assert_eq!(normalize_standard("20"), "c++20");
        assert_eq!(normalize_standard("gnu++20"), "gnu++20");
        // Garbage must not reach the command line.
        assert_eq!(normalize_standard("; rm -rf /"), DEFAULT_CPP_STANDARD);
        assert_eq!(normalize_standard(""), DEFAULT_CPP_STANDARD);
        assert_eq!(normalize_standard("c++99"), DEFAULT_CPP_STANDARD);
    }

    #[test]
    fn c_standard_normalization() {
        assert_eq!(normalize_c_standard("c11"), "c11");
        assert_eq!(normalize_c_standard("GNU17"), "gnu17");
        assert_eq!(normalize_c_standard("nonsense"), DEFAULT_C_STANDARD);
    }

    #[test]
    fn base_args_carry_standard_and_never_werror() {
        let args = base_compile_args("c++17");
        assert!(args.contains(&"-std=c++17".to_string()));
        assert!(!args.iter().any(|a| a == "-Werror"));
        assert!(args.iter().any(|a| a == "-Wall"));
        if cfg!(windows) {
            assert!(args.contains(&"-static".to_string()));
        } else {
            // `-static` on macOS has no static libSystem to link against and on
            // Linux a static glibc breaks getaddrinfo/NSS. It is a Windows-only
            // answer to a Windows-only problem (MinGW runtime DLLs).
            assert!(!args.contains(&"-static".to_string()));
        }
    }

    #[test]
    fn base_args_stay_portable_across_gcc_and_clang() {
        // Anything here must be understood by BOTH GCC and Apple Clang. A flag
        // only one of them accepts fails every compile on the other platform.
        const PORTABLE_PREFIXES: [&str; 4] = ["-std=", "-O", "-W", "-static"];
        // -idirafter and -I are added by plan_compile, not by this function, but
        // both are understood by GCC and Clang alike.
        for a in base_compile_args("c++17") {
            assert!(
                PORTABLE_PREFIXES.iter().any(|p| a.starts_with(p)),
                "non-portable compiler flag in the default set: {}",
                a
            );
        }
    }

    #[test]
    fn an_absolute_path_is_told_apart_from_a_bare_command() {
        assert!(looks_absolute("C:\\mingw64\\bin\\g++.exe"));
        assert!(looks_absolute("/usr/bin/clang++"));
        assert!(!looks_absolute("g++"));
        assert!(!looks_absolute("clang++"));
    }

    #[test]
    fn a_stale_configured_compiler_falls_back_to_discovery() {
        clear_compiler_cache();
        // A student picks a compiler, then uninstalls it. The stored path must
        // not turn every Run into "could not start the compiler" — the IDE
        // falls back to whatever it can find, which is the same thing it would
        // have done before the setting existed.
        let ghost = if cfg!(windows) {
            "C:\\definitely\\not\\here\\g++.exe"
        } else {
            "/definitely/not/here/g++"
        };
        let resolved = find_compiler(Some(ghost), true);
        assert_ne!(
            resolved.as_deref(),
            Some(ghost),
            "a path that does not exist must not be handed to the compiler driver"
        );
        // On a machine with a compiler we should have found it; on a bare CI
        // runner None is the correct answer. Either way, never the ghost.
        clear_compiler_cache();
    }

    #[test]
    fn a_bare_command_name_is_passed_through() {
        clear_compiler_cache();
        // Not every configuration is a path. `g++` is a legitimate value and
        // only PATH can resolve it, so it must survive unchanged.
        assert_eq!(find_compiler(Some("g++"), true).as_deref(), Some("g++"));
        clear_compiler_cache();
    }

    #[test]
    fn kind_detection() {
        assert_eq!(kind_of("g++ (MinGW-W64 x86_64-ucrt-posix-seh) 15.3.0"), "gcc");
        assert_eq!(kind_of("Apple clang version 15.0.0"), "clang");
        assert_eq!(kind_of("something else"), "unknown");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Build planning
//
// A C++ answer is rarely one file. `main.cpp` + `utils.cpp` + `utils.h` is the
// shape every assignment brief uses, and the previous single-file
// `g++ main.cpp -o a.exe` produced "undefined reference to `helper()`" — which
// reads as the student's bug, not the IDE's. Runs now compile the active file
// TOGETHER WITH its sibling translation units.
// ─────────────────────────────────────────────────────────────────────────────

/// Source extensions treated as C++ translation units.
pub const CPP_EXTS: [&str; 5] = ["cpp", "cc", "cxx", "c++", "cp"];
/// Source extensions treated as C translation units.
pub const C_EXTS: [&str; 1] = ["c"];

/// Never link more than this many files. A student who imports a library
/// source tree into the workspace should get a clear cap rather than a
/// multi-minute link.
const MAX_TRANSLATION_UNITS: usize = 64;
/// Files larger than this are not scanned for `main` (and not auto-linked).
const MAX_TU_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// Remove comments and string/char literals so a token search cannot be fooled
/// by a commented-out main or one mentioned inside a string. Handles line
/// comments, block comments, quoted literals and C++11 raw strings.
pub fn strip_comments_and_literals(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < b.len() {
        // line comment
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // block comment
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                if b[i] == b'\n' {
                    out.push('\n'); // keep line structure
                }
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        // raw string: R"delim( ... )delim"
        if b[i] == b'R' && i + 1 < b.len() && b[i + 1] == b'"' {
            let mut j = i + 2;
            let mut delim = Vec::new();
            while j < b.len() && b[j] != b'(' && delim.len() < 16 {
                delim.push(b[j]);
                j += 1;
            }
            if j < b.len() && b[j] == b'(' {
                let closing: Vec<u8> = {
                    let mut c = vec![b')'];
                    c.extend_from_slice(&delim);
                    c.push(b'"');
                    c
                };
                j += 1;
                while j < b.len() {
                    if b[j..].starts_with(&closing) {
                        j += closing.len();
                        break;
                    }
                    if b[j] == b'\n' {
                        out.push('\n');
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        // quoted literals
        if b[i] == b'"' || b[i] == b'\'' {
            let quote = b[i];
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == quote {
                    i += 1;
                    break;
                }
                if b[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            continue;
        }
        // Copy the byte through, decoding multi-byte UTF-8 as a unit so the
        // output stays valid text.
        let ch_len = utf8_len(b[i]);
        if ch_len == 1 {
            out.push(b[i] as char);
            i += 1;
        } else {
            let end = (i + ch_len).min(b.len());
            match std::str::from_utf8(&b[i..end]) {
                Ok(s) => out.push_str(s),
                Err(_) => out.push(' '),
            }
            i = end;
        }
    }
    out
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// True when the source defines (or declares) `main`. Used to decide which
/// sibling files may be linked in: two mains in one link is
/// "multiple definition of main", a worse failure than not linking at all.
pub fn defines_main(src: &str) -> bool {
    let cleaned = strip_comments_and_literals(src);
    let b = cleaned.as_bytes();
    let mut i = 0usize;
    while let Some(pos) = find_from(b, i, b"main") {
        let before_ok = pos == 0 || !is_ident_byte(b[pos - 1]);
        let mut j = pos + 4;
        while j < b.len() && (b[j] as char).is_whitespace() {
            j += 1;
        }
        let after_ok = j < b.len() && b[j] == b'(';
        if before_ok && after_ok {
            return true;
        }
        i = pos + 4;
    }
    false
}

fn is_ident_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

fn find_from(hay: &[u8], start: usize, needle: &[u8]) -> Option<usize> {
    if start >= hay.len() || needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay[start..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + start)
}

/// The set of files a Run should compile: the active file first, then every
/// sibling translation unit that does NOT define `main`.
///
/// Non-recursive on purpose. Recursing would sweep in a backup copy of the same
/// file (duplicate symbols) or an unrelated sub-assignment; one directory is the
/// unit a student reasons about.
pub fn collect_translation_units(active: &Path, cpp: bool) -> Vec<PathBuf> {
    let mut units: Vec<PathBuf> = vec![active.to_path_buf()];
    let dir = match active.parent() {
        Some(d) => d,
        None => return units,
    };
    let exts: &[&str] = if cpp { &CPP_EXTS } else { &C_EXTS };

    let mut siblings: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path == active {
                continue;
            }
            let ext = match path.extension().and_then(|e| e.to_str()) {
                Some(e) => e.to_ascii_lowercase(),
                None => continue,
            };
            if !exts.contains(&ext.as_str()) {
                continue;
            }
            // Skip anything implausible as an exam source before reading it.
            match entry.metadata() {
                Ok(md) if md.len() <= MAX_TU_SCAN_BYTES => {}
                _ => continue,
            }
            // Lossy, not `read_to_string`. A .cpp a student imported from
            // another editor can be CP949 rather than UTF-8, and a strict read
            // fails on it — which would silently drop the file from the link and
            // produce "undefined reference" pointing at nothing the student did.
            // Reading lossily still answers the only question asked here (does
            // this file define `main`?), because `main` is ASCII either way.
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    // A file with its own main is a separate program.
                    if !defines_main(&text) {
                        siblings.push(path);
                    }
                }
                Err(_) => {}
            }
        }
    }
    // Deterministic link order — otherwise directory iteration order decides,
    // and a link that works on one machine can fail on another.
    siblings.sort();
    units.extend(siblings);
    units.truncate(MAX_TRANSLATION_UNITS);
    units
}

/// Where compiled binaries go: OUTSIDE the workspace.
///
/// Inside the workspace they would (a) be seen by the integrity monitor as new
/// files on every Run, (b) ride along in the submission zip, and (c) collide
/// with the student's own files. `a.exe` was exempted from monitoring by name
/// for exactly this reason; moving the whole build tree out removes the special
/// case instead of adding more of them.
///
/// The path is forced ASCII: a Korean Windows username makes LOCALAPPDATA
/// non-ASCII, and MinGW's linker is not reliably unicode-clean on output paths
/// (the same reason the exam venv falls back to C:\ProgramData).
pub fn build_dir_for_workspace(workspace: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    let tag = hex::encode(hasher.finalize());
    let tag = tag[..16].to_string();

    let primary = crate::setup::app_data_root().join("build").join(&tag);
    if is_ascii_path(&primary) {
        return primary;
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from("C:\\ProgramData\\MINT_Exam_IDE\\build").join(&tag)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::temp_dir().join("MINT_Exam_IDE_build").join(&tag)
    }
}

/// Delete build directories that no longer belong to a live workspace.
///
/// Every exam session creates a NEW workspace, and every workspace gets its own
/// build directory keyed by path hash. Statically linked binaries are a couple
/// of megabytes each, so a machine used for a semester of exams would otherwise
/// accumulate them forever in a hidden directory no student would ever find.
///
/// Age-based rather than reference-based on purpose: the workspace a build
/// directory belongs to may have been deleted, renamed, or be on a drive that
/// is not mounted, and none of those should stop the cleanup. Anything still in
/// use is younger than the cutoff by definition — it was written by a compile.
pub fn prune_old_build_dirs(max_age: std::time::Duration) {
    let mut roots: Vec<PathBuf> = vec![crate::setup::app_data_root().join("build")];
    #[cfg(target_os = "windows")]
    {
        // The ASCII fallback root used when LOCALAPPDATA is non-ASCII.
        roots.push(PathBuf::from("C:\\ProgramData\\MINT_Exam_IDE\\build"));
    }
    #[cfg(not(target_os = "windows"))]
    {
        roots.push(std::env::temp_dir().join("MINT_Exam_IDE_build"));
    }

    let now = std::time::SystemTime::now();
    for root in roots {
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Newest mtime anywhere inside decides: the directory's own mtime
            // does not change when a file inside it is overwritten.
            let age = newest_mtime(&path)
                .and_then(|t| now.duration_since(t).ok())
                .unwrap_or_default();
            if age > max_age {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

fn newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = std::fs::metadata(dir).ok()?.modified().ok()?;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                if t > newest {
                    newest = t;
                }
            }
        }
    }
    Some(newest)
}

/// Build directories older than this are removed at startup.
///
/// A week comfortably covers an exam plus any re-runs, while keeping a machine
/// used all semester from filling up.
pub const BUILD_DIR_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

fn is_ascii_path(p: &Path) -> bool {
    p.to_string_lossy().is_ascii()
}

/// Executable name for a source file, sanitized to ASCII.
///
/// A student may well name a file in Korean. The source itself is compiled by
/// RELATIVE name from its own directory (so the OS resolves the unicode), but
/// the OUTPUT path is ours to choose, and choosing ASCII keeps the linker on the
/// path it handles best. Two sources that sanitize to the same name are kept
/// apart by a short hash of the original stem.
pub fn exe_name_for(source: &Path) -> String {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "program".to_string());

    let mut ascii: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if ascii.trim_matches('_').is_empty() {
        ascii = "program".to_string();
    }
    if !stem.is_ascii() {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(stem.as_bytes());
        let tag = hex::encode(h.finalize());
        ascii = format!("{}_{}", ascii, &tag[..8]);
    }
    if ascii.len() > 60 {
        ascii.truncate(60);
    }
    if cfg!(windows) {
        format!("{}.exe", ascii)
    } else {
        ascii
    }
}

/// Everything needed to invoke the compiler, computed without side effects so
/// it can be asserted on in tests.
#[derive(Debug, Clone)]
pub struct CompileSpec {
    pub compiler: String,
    pub args: Vec<String>,
    /// The compile runs with this as CWD and passes RELATIVE source names.
    ///
    /// This is the whole reason a Korean Windows username does not break C++:
    /// `%LOCALAPPDATA%` becomes non-ASCII, MinGW's driver is not reliably
    /// unicode-clean when it PARSES such a path from argv, but it never has to
    /// — the OS resolves the directory and the compiler only ever sees
    /// `main.cpp`.
    pub cwd: PathBuf,
    /// Where the binary lands. Carried on the spec so a caller (and the tests)
    /// never has to re-derive it from the argument list.
    #[allow(dead_code)]
    pub exe: PathBuf,
    /// Source files in link order, as passed on the command line.
    pub units: Vec<String>,
}

/// Build the compiler invocation for `rel_file` inside `workspace`.
///
/// The convenience form: every translation unit, no compatibility headers.
/// Production goes through `plan_compile_units`, which needs both knobs.
#[allow(dead_code)]
pub fn plan_compile(
    workspace: &Path,
    rel_file: &str,
    cpp: bool,
    compiler: &str,
    standard: &str,
    exe: &Path,
) -> CompileSpec {
    plan_compile_units(workspace, rel_file, cpp, compiler, standard, exe, usize::MAX, None)
}

/// As `plan_compile`, but linking at most `max_units` translation units.
///
/// `max_units = 1` is the retry used when auto-linking siblings produced
/// "multiple definition of": it falls back to exactly the file the student
/// asked to run, which is what the old single-file behaviour always did.
pub fn plan_compile_units(
    workspace: &Path,
    rel_file: &str,
    cpp: bool,
    compiler: &str,
    standard: &str,
    exe: &Path,
    max_units: usize,
    compat_include: Option<&Path>,
) -> CompileSpec {
    let src_abs = workspace.join(rel_file);
    let project_dir = src_abs.parent().unwrap_or(workspace).to_path_buf();
    let mut units_abs = collect_translation_units(&src_abs, cpp);
    units_abs.truncate(max_units.max(1));

    let mut args = base_compile_args(standard);
    // The source directory and the workspace root are both include roots, so
    // `#include "utils.h"` resolves whether the header sits beside the source
    // or at the top of the workspace.
    //
    // BOTH are relative, for the same reason the source names are: the compile
    // runs with the source directory as CWD, and a Korean Windows username
    // makes the absolute workspace path non-ASCII. Handing that to MinGW's
    // driver as an argument is the one thing this design avoids everywhere
    // else, and it would be pointless to reintroduce it here — so the workspace
    // root is expressed as `..`, `../..` and so on.
    args.push("-I".to_string());
    args.push(".".to_string());
    if let Some(up) = relative_up_to_workspace(rel_file) {
        args.push("-I".to_string());
        args.push(up);
    }

    let mut units: Vec<String> = Vec::new();
    for u in &units_abs {
        let rel = u
            .strip_prefix(&project_dir)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| u.to_string_lossy().to_string());
        units.push(rel);
    }
    // `-idirafter`, not `-I`: the directory goes AFTER the system headers, so a
    // toolchain that already has the real <bits/stdc++.h> (every GCC) keeps
    // using it and only a toolchain without one (Apple Clang) falls through to
    // the shim. Both compilers accept the flag. The path is absolute, which is
    // safe here for the same reason the output path is: the build directory is
    // forced to ASCII.
    if let Some(compat) = compat_include {
        args.push("-idirafter".to_string());
        args.push(compat.to_string_lossy().to_string());
    }

    args.extend(units.iter().cloned());
    args.push("-o".to_string());
    args.push(exe.to_string_lossy().to_string());

    CompileSpec {
        compiler: compiler.to_string(),
        args,
        cwd: project_dir,
        exe: exe.to_path_buf(),
        units,
    }
}

/// Contents of the `bits/stdc++.h` compatibility header.
///
/// Every include is guarded by `__has_include`, so the same file works against
/// libstdc++, libc++ and any standard level from C++11 up. Headers that only
/// exist in newer standards simply do not appear.
const BITS_STDCXX_SHIM: &str = r#"// MINT Exam IDE — compatibility header.
//
// <bits/stdc++.h> is a libstdc++ extension. It does not exist on macOS, whose
// clang uses libc++, so a student who writes the include that every
// competitive-programming habit teaches would have working code on Windows and
// a compile error on a Mac. This file exists only for the platforms that lack
// the real one: it is added with -idirafter, so wherever the genuine
// <bits/stdc++.h> exists, that one is still used.
#pragma once

#if defined(__has_include)
#  define MINT_HAS(x) __has_include(x)
#else
#  define MINT_HAS(x) 1
#endif

// C library
#if MINT_HAS(<cassert>)
#  include <cassert>
#endif
#if MINT_HAS(<cctype>)
#  include <cctype>
#endif
#if MINT_HAS(<cerrno>)
#  include <cerrno>
#endif
#if MINT_HAS(<cfloat>)
#  include <cfloat>
#endif
#if MINT_HAS(<climits>)
#  include <climits>
#endif
#if MINT_HAS(<cmath>)
#  include <cmath>
#endif
#if MINT_HAS(<cstdarg>)
#  include <cstdarg>
#endif
#if MINT_HAS(<cstddef>)
#  include <cstddef>
#endif
#if MINT_HAS(<cstdint>)
#  include <cstdint>
#endif
#if MINT_HAS(<cstdio>)
#  include <cstdio>
#endif
#if MINT_HAS(<cstdlib>)
#  include <cstdlib>
#endif
#if MINT_HAS(<cstring>)
#  include <cstring>
#endif
#if MINT_HAS(<ctime>)
#  include <ctime>
#endif
#if MINT_HAS(<cwchar>)
#  include <cwchar>
#endif

// Containers
#if MINT_HAS(<array>)
#  include <array>
#endif
#if MINT_HAS(<bitset>)
#  include <bitset>
#endif
#if MINT_HAS(<deque>)
#  include <deque>
#endif
#if MINT_HAS(<forward_list>)
#  include <forward_list>
#endif
#if MINT_HAS(<list>)
#  include <list>
#endif
#if MINT_HAS(<map>)
#  include <map>
#endif
#if MINT_HAS(<queue>)
#  include <queue>
#endif
#if MINT_HAS(<set>)
#  include <set>
#endif
#if MINT_HAS(<stack>)
#  include <stack>
#endif
#if MINT_HAS(<unordered_map>)
#  include <unordered_map>
#endif
#if MINT_HAS(<unordered_set>)
#  include <unordered_set>
#endif
#if MINT_HAS(<vector>)
#  include <vector>
#endif

// Algorithms, numerics, utilities
#if MINT_HAS(<algorithm>)
#  include <algorithm>
#endif
#if MINT_HAS(<bit>)
#  include <bit>
#endif
#if MINT_HAS(<chrono>)
#  include <chrono>
#endif
#if MINT_HAS(<complex>)
#  include <complex>
#endif
#if MINT_HAS(<functional>)
#  include <functional>
#endif
#if MINT_HAS(<initializer_list>)
#  include <initializer_list>
#endif
#if MINT_HAS(<iterator>)
#  include <iterator>
#endif
#if MINT_HAS(<limits>)
#  include <limits>
#endif
#if MINT_HAS(<memory>)
#  include <memory>
#endif
#if MINT_HAS(<numeric>)
#  include <numeric>
#endif
#if MINT_HAS(<optional>)
#  include <optional>
#endif
#if MINT_HAS(<random>)
#  include <random>
#endif
#if MINT_HAS(<ratio>)
#  include <ratio>
#endif
#if MINT_HAS(<string>)
#  include <string>
#endif
#if MINT_HAS(<string_view>)
#  include <string_view>
#endif
#if MINT_HAS(<tuple>)
#  include <tuple>
#endif
#if MINT_HAS(<type_traits>)
#  include <type_traits>
#endif
#if MINT_HAS(<utility>)
#  include <utility>
#endif
#if MINT_HAS(<variant>)
#  include <variant>
#endif

// I/O
#if MINT_HAS(<fstream>)
#  include <fstream>
#endif
#if MINT_HAS(<iomanip>)
#  include <iomanip>
#endif
#if MINT_HAS(<ios>)
#  include <ios>
#endif
#if MINT_HAS(<iostream>)
#  include <iostream>
#endif
#if MINT_HAS(<istream>)
#  include <istream>
#endif
#if MINT_HAS(<ostream>)
#  include <ostream>
#endif
#if MINT_HAS(<sstream>)
#  include <sstream>
#endif
#if MINT_HAS(<streambuf>)
#  include <streambuf>
#endif

// Concurrency and exceptions
#if MINT_HAS(<atomic>)
#  include <atomic>
#endif
#if MINT_HAS(<condition_variable>)
#  include <condition_variable>
#endif
#if MINT_HAS(<exception>)
#  include <exception>
#endif
#if MINT_HAS(<mutex>)
#  include <mutex>
#endif
#if MINT_HAS(<new>)
#  include <new>
#endif
#if MINT_HAS(<stdexcept>)
#  include <stdexcept>
#endif
#if MINT_HAS(<thread>)
#  include <thread>
#endif

#undef MINT_HAS
"#;

/// Write the compatibility headers into `build_dir` and return the directory to
/// hand the compiler, or None if it could not be written (in which case the
/// compile simply proceeds without them).
///
/// Rewritten only when the contents differ, so a Run does not touch the disk
/// for nothing.
pub fn ensure_compat_headers(build_dir: &Path) -> Option<PathBuf> {
    let root = build_dir.join("mint_compat");
    let bits = root.join("bits");
    let header = bits.join("stdc++.h");

    let current = std::fs::read_to_string(&header).unwrap_or_default();
    if current != BITS_STDCXX_SHIM {
        std::fs::create_dir_all(&bits).ok()?;
        std::fs::write(&header, BITS_STDCXX_SHIM).ok()?;
    }
    Some(root)
}

/// `..`-style path from the directory holding `rel_file` back to the workspace
/// root, or None when the file already sits at the root.
///
/// `rel_file` is a workspace-relative, forward-slash path (`cpp_project/main.cpp`).
fn relative_up_to_workspace(rel_file: &str) -> Option<String> {
    let normalized = rel_file.replace('\\', "/");
    let depth = normalized
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .count()
        .saturating_sub(1); // the file name itself
    if depth == 0 {
        return None;
    }
    let mut parts: Vec<&str> = Vec::with_capacity(depth);
    for _ in 0..depth {
        parts.push("..");
    }
    Some(parts.join("/"))
}

impl CompileSpec {
    /// Run the planned compile. Returns (success, combined diagnostics).
    ///
    /// Production goes through `runner::build_native`, which needs streaming,
    /// a timeout and event emission; this is the same invocation without the
    /// orchestration, so tests can assert on real compiler behaviour.
    #[allow(dead_code)]
    pub fn run(&self) -> (bool, String) {
        let mut cmd = Command::new(&self.compiler);
        cmd.args(&self.args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null());
        compile_env(&mut cmd);
        match quiet(&mut cmd).output() {
            Ok(o) => {
                let mut text = String::from_utf8_lossy(&o.stderr).to_string();
                text.push_str(&String::from_utf8_lossy(&o.stdout));
                (o.status.success(), text)
            }
            Err(e) => (false, format!("failed to spawn {}: {}", self.compiler, e)),
        }
    }
}

#[cfg(test)]
mod build_tests {
    use super::*;

    #[test]
    fn strips_comments_and_literals_before_matching() {
        let src = "// int main() in a line comment\n\
                   /* int main() in a block comment */\n\
                   const char* s = \"int main() in a string\";\n\
                   char c = '(';\n";
        assert!(!defines_main(src));
    }

    #[test]
    fn detects_real_main_forms() {
        assert!(defines_main("int main() { return 0; }"));
        assert!(defines_main("int main(int argc, char** argv){}"));
        assert!(defines_main("int\nmain\n(\n)\n{}"));
        assert!(defines_main("auto main() -> int { return 0; }"));
        assert!(defines_main("int main(void);"));
    }

    #[test]
    fn does_not_match_identifiers_containing_main() {
        assert!(!defines_main("int domain(int x){return x;}"));
        assert!(!defines_main("int main_helper(){return 0;}"));
        assert!(!defines_main("int mainly;"));
        // WinMain is a different entry point and must not be mistaken for one.
        assert!(!defines_main("int WinMain(void*, void*, char*, int){return 0;}"));
    }

    #[test]
    fn raw_strings_do_not_leak_a_main() {
        let src = "const char* q = R\"delim(int main() {})delim\"; int helper(){return 1;}";
        assert!(!defines_main(src));
    }

    #[test]
    fn unterminated_literal_does_not_panic() {
        // A half-typed line is the NORMAL state of a file being edited.
        assert!(!defines_main("const char* s = \"unterminated"));
        assert!(!defines_main("/* unterminated block comment"));
        assert!(!defines_main("char c = 'x"));
    }

    #[test]
    fn exe_name_is_ascii_and_stable() {
        let a = exe_name_for(Path::new("/ws/main.cpp"));
        assert!(a.starts_with("main"));
        assert!(a.is_ascii());

        let k1 = exe_name_for(Path::new("/ws/문제1.cpp"));
        let k2 = exe_name_for(Path::new("/ws/문제1.cpp"));
        let k3 = exe_name_for(Path::new("/ws/문제2.cpp"));
        assert!(k1.is_ascii(), "exe name must be ASCII, got {}", k1);
        assert_eq!(k1, k2, "same source must map to the same exe name");
        assert_ne!(k1, k3, "different sources must not collide");
    }

    #[test]
    fn workspace_include_path_is_relative_not_absolute() {
        assert_eq!(relative_up_to_workspace("main.cpp"), None);
        assert_eq!(relative_up_to_workspace("cpp_project/main.cpp"), Some("..".to_string()));
        assert_eq!(
            relative_up_to_workspace("a/b/main.cpp"),
            Some("../..".to_string())
        );
        assert_eq!(
            relative_up_to_workspace("a\\b\\main.cpp"),
            Some("../..".to_string()),
            "Windows separators must be handled"
        );
    }

    #[test]
    fn no_absolute_path_reaches_the_compiler_except_the_output() {
        // Absolute paths in argv are the one thing that breaks on a Korean
        // Windows username. The OUTPUT path is allowed — it is ours, and
        // build_dir_for_workspace forces it to ASCII.
        let exe = Path::new("C:/ascii/build/out.exe");
        let spec = plan_compile(
            Path::new("C:/ws"),
            "sub/main.cpp",
            true,
            "g++",
            "c++17",
            exe,
        );
        let out = exe.to_string_lossy().to_string();
        for a in &spec.args {
            // The output path and the compat-header directory are both ours and
            // both live under an ASCII build directory.
            if *a == out || a.contains("mint_compat") {
                continue;
            }
            assert!(
                !a.contains(":\\") && !a.contains(":/"),
                "unexpected absolute path passed to the compiler: {}",
                a
            );
        }
        assert!(spec.args.iter().any(|a| a == ".."), "workspace root include missing");
    }

    #[test]
    fn build_dir_is_ascii_and_per_workspace() {
        let a = build_dir_for_workspace(Path::new("C:\\ws\\one"));
        let b = build_dir_for_workspace(Path::new("C:\\ws\\two"));
        assert!(a.to_string_lossy().is_ascii());
        assert_ne!(a, b);
    }
}


/// Tests that invoke a REAL compiler.
///
/// Every one of them SKIPS (rather than fails) when no toolchain is present, so
/// CI on a bare runner stays green while a developer machine — and the exam
/// machines this ships to — get genuine coverage of compile, link, run and
/// stdin. The unit tests above cover the logic that must hold everywhere.
#[cfg(test)]
mod real_compiler_tests {
    use super::*;
    use std::io::Write;

    fn cxx() -> Option<String> {
        clear_compiler_cache();
        find_compiler(None, true)
    }

    /// A scratch workspace that cleans itself up.
    struct Ws(PathBuf);
    impl Ws {
        fn new(tag: &str) -> Ws {
            let dir = std::env::temp_dir().join(format!(
                "mint-cpp-test-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("temp workspace");
            Ws(dir)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Ws {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Compile `rel` in `ws` and run the result, feeding `stdin_text`.
    /// Returns (stdout, stderr, exit_code).
    fn compile_and_run(ws: &Ws, rel: &str, compiler: &str, stdin_text: &str) -> (String, String, Option<i32>) {
        let exe = ws.path().join(if cfg!(windows) { "prog.exe" } else { "prog" });
        let spec = plan_compile(ws.path(), rel, true, compiler, DEFAULT_CPP_STANDARD, &exe);
        let (ok, diag) = spec.run();
        assert!(ok, "compile failed for {}:\n{}", rel, diag);
        assert!(exe.exists(), "no executable produced for {}", rel);

        let mut cmd = Command::new(&exe);
        cmd.current_dir(ws.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(bin) = compiler_bin_dir(compiler) {
            prepend_path(&mut cmd, &bin);
        }
        let mut child = quiet(&mut cmd).spawn().expect("spawn compiled program");
        {
            let mut sin = child.stdin.take().expect("stdin pipe");
            sin.write_all(stdin_text.as_bytes()).unwrap();
            sin.flush().unwrap();
            // Dropping closes the pipe = EOF, which is what the IDE's
            // "EOF" button does.
        }
        let out = child.wait_with_output().expect("run compiled program");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.code(),
        )
    }

    #[test]
    fn verify_cpp_end_to_end() {
        if cxx().is_none() {
            eprintln!("SKIP verify_cpp_end_to_end: no C++ compiler on this machine");
            return;
        }
        let r = verify_cpp(None, "c++17");
        assert!(r.ok, "verify failed at stage '{}': {}", r.failed_stage, r.message);
        assert!(!r.compiler.is_empty());
        assert!(!r.version.is_empty(), "compiler reported no version");
    }

    #[test]
    fn single_file_reads_stdin() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP single_file_reads_stdin: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("single");
        // The shape of essentially every exam problem: read n, then n values.
        ws.write(
            "main.cpp",
            "#include <iostream>\n\
             int main(){int n; std::cin>>n; long long s=0,x;\n\
             for(int i=0;i<n;i++){std::cin>>x;s+=x;}\n\
             std::cout<<s<<std::endl; return 0;}\n",
        );
        let (out, err, code) = compile_and_run(&ws, "main.cpp", &c, "3\n10 20 30\n");
        assert_eq!(out.trim(), "60", "stdout was {:?} (stderr {:?})", out, err);
        assert_eq!(code, Some(0));
    }

    #[test]
    fn read_until_eof_terminates() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP read_until_eof_terminates: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("eof");
        // `while (cin >> x)` ends ONLY at EOF. Before stdin was wired up this
        // could never terminate from the UI.
        ws.write(
            "main.cpp",
            "#include <iostream>\n\
             int main(){long long x,s=0; while(std::cin>>x) s+=x;\n\
             std::cout<<s<<std::endl; return 0;}\n",
        );
        let (out, _, code) = compile_and_run(&ws, "main.cpp", &c, "1 2 3 4 5\n");
        assert_eq!(out.trim(), "15");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn multi_file_project_links() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP multi_file_project_links: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("multi");
        ws.write("utils.h", "#pragma once\nint addAll(int a, int b);\n");
        ws.write("utils.cpp", "#include \"utils.h\"\nint addAll(int a,int b){return a+b;}\n");
        ws.write(
            "main.cpp",
            "#include <iostream>\n#include \"utils.h\"\n\
             int main(){int a,b; std::cin>>a>>b;\n\
             std::cout<<addAll(a,b)<<std::endl; return 0;}\n",
        );
        let (out, err, code) = compile_and_run(&ws, "main.cpp", &c, "7 5\n");
        assert_eq!(out.trim(), "12", "stderr: {}", err);
        assert_eq!(code, Some(0));
    }

    #[test]
    fn sibling_with_its_own_main_is_not_linked() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP sibling_with_its_own_main_is_not_linked: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("twomains");
        // Two independent programs in one folder is the NORMAL state of an
        // assignment folder. Linking both is "multiple definition of main".
        ws.write("problem1.cpp", "#include <iostream>\nint main(){std::cout<<\"one\"<<std::endl;}\n");
        ws.write("problem2.cpp", "#include <iostream>\nint main(){std::cout<<\"two\"<<std::endl;}\n");
        let (out, _, _) = compile_and_run(&ws, "problem2.cpp", &c, "");
        assert_eq!(out.trim(), "two", "the ACTIVE file must be the one that runs");
    }

    #[test]
    fn non_ascii_directory_compiles() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP non_ascii_directory_compiles: no C++ compiler");
                return;
            }
        };
        // Simulates a Korean Windows username: the workspace path is non-ASCII.
        // The compile must still work, because the plan passes RELATIVE source
        // names and lets the OS resolve the directory.
        let ws = Ws::new("한글경로");
        ws.write("main.cpp", "#include <iostream>\nint main(){std::cout<<\"ok\"<<std::endl;}\n");
        let (out, err, code) = compile_and_run(&ws, "main.cpp", &c, "");
        assert_eq!(out.trim(), "ok", "stderr: {}", err);
        assert_eq!(code, Some(0));
    }

    #[test]
    fn korean_source_filename_compiles() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP korean_source_filename_compiles: no C++ compiler");
                return;
            }
        };
        // Students name files after the problem. `문제1.cpp` must build, and the
        // binary it produces must land on an ASCII path (exe_name_for) so the
        // linker never has to write a unicode output path.
        let ws = Ws::new("hangul-file");
        ws.write("문제1.cpp", "#include <iostream>\nint main(){int a,b;std::cin>>a>>b;std::cout<<a*b<<std::endl;}\n");
        let (out, err, code) = compile_and_run(&ws, "문제1.cpp", &c, "6 7\n");
        assert_eq!(out.trim(), "42", "stderr: {}", err);
        assert_eq!(code, Some(0));
    }

    #[test]
    fn spaces_in_source_filename_compile() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP spaces_in_source_filename_compile: no C++ compiler");
                return;
            }
        };
        // "assignment 1.cpp" — a space in an argv entry is only safe because
        // Rust's Command passes arguments as a vector, not a joined string.
        let ws = Ws::new("spaced");
        ws.write("assignment 1.cpp", "#include <iostream>\nint main(){std::cout<<\"spaced ok\"<<std::endl;}\n");
        let (out, err, code) = compile_and_run(&ws, "assignment 1.cpp", &c, "");
        assert_eq!(out.trim(), "spaced ok", "stderr: {}", err);
        assert_eq!(code, Some(0));
    }

    #[test]
    fn korean_output_is_utf8_clean() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP korean_output_is_utf8_clean: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("hangul-out");
        ws.write(
            "main.cpp",
            "#include <iostream>\nint main(){std::cout<<\"결과: 정답\"<<std::endl;}\n",
        );
        let (out, err, _) = compile_and_run(&ws, "main.cpp", &c, "");
        assert!(out.contains("결과: 정답"), "got {:?} (stderr {:?})", out, err);
    }

    #[test]
    fn duplicate_symbol_falls_back_to_the_active_file_alone() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP duplicate_symbol_falls_back_to_the_active_file_alone: no C++ compiler");
                return;
            }
        };
        // A scratch file with no main that happens to define the same helper.
        // Auto-linking siblings turns this into "multiple definition of", which
        // the student did not cause; the runner retries with the active file
        // alone, and this proves BOTH halves of that behaviour.
        let ws = Ws::new("dupsym");
        ws.write(
            "main.cpp",
            "#include <iostream>\nint helper(){return 1;}\nint main(){std::cout<<helper()<<std::endl;}\n",
        );
        ws.write("scratch.cpp", "int helper(){return 2;}\n");

        let exe = ws.path().join(if cfg!(windows) { "dup.exe" } else { "dup" });

        // 1. Linking both must fail the way the fallback keys on.
        let both = plan_compile(ws.path(), "main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe);
        assert_eq!(both.units.len(), 2, "sibling should have been picked up");
        let (ok, diag) = both.run();
        assert!(!ok, "two definitions of helper must not link");
        assert!(
            diag.contains("multiple definition of"),
            "fallback keys on this exact phrase; got: {}",
            diag
        );

        // 2. The single-unit retry must succeed and produce the ACTIVE file's answer.
        let solo = plan_compile_units(ws.path(), "main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe, 1, None);
        assert_eq!(solo.units.len(), 1);
        let (ok2, diag2) = solo.run();
        assert!(ok2, "single-file retry must compile: {}", diag2);

        let out = Command::new(&exe)
            .current_dir(ws.path())
            .output()
            .expect("run fallback binary");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
    }

    #[test]
    fn header_only_change_is_picked_up_on_the_next_run() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP header_only_change_is_picked_up_on_the_next_run: no C++ compiler");
                return;
            }
        };
        // Every Run recompiles from source — there is no object cache to go
        // stale. A student who edits only the header must not have to touch the
        // .cpp to see the change.
        let ws = Ws::new("hdr");
        ws.write("v.h", "#pragma once\n#define ANSWER 1\n");
        ws.write("main.cpp", "#include <iostream>\n#include \"v.h\"\nint main(){std::cout<<ANSWER<<std::endl;}\n");
        let (out1, _, _) = compile_and_run(&ws, "main.cpp", &c, "");
        assert_eq!(out1.trim(), "1");

        ws.write("v.h", "#pragma once\n#define ANSWER 2\n");
        let (out2, _, _) = compile_and_run(&ws, "main.cpp", &c, "");
        assert_eq!(out2.trim(), "2", "a header-only edit must take effect");
    }

    #[test]
    fn cpp20_and_cpp23_standards_are_accepted() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP cpp20_and_cpp23_standards_are_accepted: no C++ compiler");
                return;
            }
        };
        // The wizard offers these; a standard the toolchain rejects would fail
        // every Run for a student who picked it.
        let ws = Ws::new("stds");
        ws.write("main.cpp", "#include <iostream>\nint main(){std::cout<<\"std ok\"<<std::endl;}\n");
        for std_flag in ["c++11", "c++14", "c++17", "c++20", "c++23"] {
            let exe = ws.path().join(if cfg!(windows) { "s.exe" } else { "s" });
            let _ = std::fs::remove_file(&exe);
            let spec = plan_compile(ws.path(), "main.cpp", true, &c, std_flag, &exe);
            let (ok, diag) = spec.run();
            assert!(ok, "standard {} was rejected: {}", std_flag, diag);
        }
    }

    #[test]
    fn c_language_path_also_builds_and_reads_stdin() {
        clear_compiler_cache();
        let cc = match find_compiler(None, false) {
            Some(c) => c,
            None => {
                eprintln!("SKIP c_language_path_also_builds_and_reads_stdin: no C compiler");
                return;
            }
        };
        // C shares the whole pipeline with C++, so it must keep working.
        let ws = Ws::new("clang-c");
        ws.write(
            "main.c",
            "#include <stdio.h>\nint main(void){int a,b; if(scanf(\"%d %d\",&a,&b)!=2) return 1; printf(\"%d\\n\", a+b); return 0;}\n",
        );
        let exe = ws.path().join(if cfg!(windows) { "cprog.exe" } else { "cprog" });
        let spec = plan_compile(ws.path(), "main.c", false, &cc, DEFAULT_C_STANDARD, &exe);
        let (ok, diag) = spec.run();
        assert!(ok, "C compile failed: {}", diag);

        let mut cmd = Command::new(&exe);
        cmd.current_dir(ws.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(bin) = compiler_bin_dir(&cc) {
            prepend_path(&mut cmd, &bin);
        }
        let mut child = quiet(&mut cmd).spawn().expect("spawn C program");
        {
            let mut sin = child.stdin.take().unwrap();
            sin.write_all(b"4 38\n").unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    }

    /// Compile like production does (CWD = source directory) but RUN like
    /// production does (CWD = workspace root, binary living outside the
    /// workspace). The two differ on purpose and file I/O depends on it.
    fn compile_then_run_from_workspace(
        ws: &Ws,
        rel: &str,
        compiler: &str,
        stdin_text: &str,
    ) -> (String, Option<i32>) {
        // Binary outside the workspace, exactly as build_dir_for_workspace does.
        let outside = std::env::temp_dir().join(format!(
            "mint-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&outside).unwrap();
        let exe = outside.join(if cfg!(windows) { "p.exe" } else { "p" });

        let spec = plan_compile(ws.path(), rel, true, compiler, DEFAULT_CPP_STANDARD, &exe);
        let (ok, diag) = spec.run();
        assert!(ok, "compile failed: {}", diag);

        let mut cmd = Command::new(&exe);
        cmd.current_dir(ws.path()) // the workspace root, like execute_code_streaming
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(bin) = compiler_bin_dir(compiler) {
            prepend_path(&mut cmd, &bin);
        }
        let mut child = quiet(&mut cmd).spawn().expect("spawn");
        {
            use std::io::Write;
            let mut sin = child.stdin.take().unwrap();
            let _ = sin.write_all(stdin_text.as_bytes());
        }
        let out = child.wait_with_output().expect("wait");
        let _ = std::fs::remove_dir_all(&outside);
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            out.status.code(),
        )
    }

    #[test]
    fn program_file_output_lands_in_the_workspace_not_the_build_dir() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP program_file_output_lands_in_the_workspace_not_the_build_dir: no C++ compiler");
                return;
            }
        };
        // File-I/O problems are common in exams, and the answer file has to end
        // up in the workspace — that is what gets zipped into the submission.
        // The binary lives outside the workspace, so this only holds because the
        // run sets the workspace as CWD.
        let ws = Ws::new("fileout");
        ws.write(
            "main.cpp",
            "#include <fstream>\n#include <iostream>\n\
             int main(){ std::ofstream f(\"result.txt\"); f << \"written\" << std::endl; \
             std::cout << \"done\" << std::endl; }\n",
        );
        let (out, code) = compile_then_run_from_workspace(&ws, "main.cpp", &c, "");
        assert_eq!(out.trim(), "done");
        assert_eq!(code, Some(0));

        let produced = ws.path().join("result.txt");
        assert!(
            produced.exists(),
            "program output must land in the workspace so it is submitted"
        );
        assert_eq!(std::fs::read_to_string(produced).unwrap().trim(), "written");
    }

    #[test]
    fn program_reads_a_data_file_from_the_workspace() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP program_reads_a_data_file_from_the_workspace: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("filein");
        ws.write("data.txt", "17\n25\n");
        ws.write(
            "main.cpp",
            "#include <fstream>\n#include <iostream>\n\
             int main(){ std::ifstream f(\"data.txt\"); int a=0,b=0; f>>a>>b; \
             std::cout<<(a+b)<<std::endl; }\n",
        );
        let (out, _) = compile_then_run_from_workspace(&ws, "main.cpp", &c, "");
        assert_eq!(out.trim(), "42", "a relative path must resolve in the workspace");
    }

    #[test]
    fn binary_runs_without_the_compiler_on_path() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP binary_runs_without_the_compiler_on_path: no C++ compiler");
                return;
            }
        };
        // On Windows a MinGW binary needs libstdc++-6.dll / libgcc_s_seh-1.dll /
        // libwinpthread-1.dll unless it was linked statically. `-static` is in
        // the default flags precisely so the produced .exe is self-contained;
        // this proves it by running WITHOUT adding the compiler's bin directory
        // to PATH.
        let ws = Ws::new("nopath");
        ws.write(
            "main.cpp",
            "#include <iostream>\n#include <string>\n#include <thread>\n\
             int main(){ std::string s=\"standalone\"; std::thread t([&]{ std::cout<<s<<std::endl; }); t.join(); }\n",
        );
        let exe = ws.path().join(if cfg!(windows) { "s.exe" } else { "s" });
        let spec = plan_compile(ws.path(), "main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe);
        let (ok, diag) = spec.run();
        assert!(ok, "compile failed: {}", diag);

        let mut cmd = Command::new(&exe);
        cmd.current_dir(ws.path()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Deliberately NO prepend_path here.
        let out = quiet(&mut cmd).output().expect("run without compiler on PATH");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "standalone",
            "binary needed something that was not linked in (stderr: {})",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn nested_source_finds_a_header_at_the_workspace_root() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP nested_source_finds_a_header_at_the_workspace_root: no C++ compiler");
                return;
            }
        };
        // The sample workspace ships a `cpp_project/` folder, and a student can
        // make one at any time. A header at the workspace root has to resolve
        // from there — and it must do so through a RELATIVE include path, since
        // this workspace has a non-ASCII name exactly like a Korean username
        // produces.
        let ws = Ws::new("한글-중첩");
        ws.write("shared.h", "#pragma once\nconstexpr int SHARED = 7;\n");
        ws.write(
            "cpp_project/main.cpp",
            "#include <iostream>\n#include \"shared.h\"\nint main(){std::cout<<SHARED*6<<std::endl;}\n",
        );

        let exe = ws.path().join(if cfg!(windows) { "n.exe" } else { "n" });
        let spec = plan_compile(ws.path(), "cpp_project/main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe);
        assert!(spec.args.iter().any(|a| a == ".."), "workspace-root include missing");
        let (ok, diag) = spec.run();
        assert!(ok, "nested compile failed: {}", diag);

        let out = Command::new(&exe)
            .current_dir(ws.path())
            .output()
            .expect("run nested binary");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    }

    #[test]
    fn nested_source_links_its_own_siblings_only() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP nested_source_links_its_own_siblings_only: no C++ compiler");
                return;
            }
        };
        // A helper in the SAME folder links; an unrelated file at the workspace
        // root must not be dragged in.
        let ws = Ws::new("nested-sib");
        ws.write("root_helper.cpp", "int rootOnly(){return 999;}\n");
        ws.write("cpp_project/util.h", "#pragma once\nint twice(int);\n");
        ws.write("cpp_project/util.cpp", "#include \"util.h\"\nint twice(int x){return x*2;}\n");
        ws.write(
            "cpp_project/main.cpp",
            "#include <iostream>\n#include \"util.h\"\nint main(){std::cout<<twice(21)<<std::endl;}\n",
        );

        let exe = ws.path().join(if cfg!(windows) { "ns.exe" } else { "ns" });
        let spec = plan_compile(ws.path(), "cpp_project/main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe);
        assert_eq!(spec.units.len(), 2, "expected main.cpp + util.cpp, got {:?}", spec.units);
        assert!(
            !spec.units.iter().any(|u| u.contains("root_helper")),
            "a file outside the source directory must not be linked: {:?}",
            spec.units
        );
        let (ok, diag) = spec.run();
        assert!(ok, "nested link failed: {}", diag);

        let out = Command::new(&exe).current_dir(ws.path()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    }

    #[test]
    fn a_cp949_sibling_is_still_linked() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP a_cp949_sibling_is_still_linked: no C++ compiler");
                return;
            }
        };
        // A .cpp imported from another Korean editor is often CP949, not UTF-8.
        // Reading it strictly would drop it from the link and produce an
        // "undefined reference" that points at nothing the student did. The
        // sibling scan reads lossily so the file is still offered to the
        // compiler; whether GCC accepts the encoding is then GCC's message to
        // give, not a silent omission.
        let ws = Ws::new("cp949");
        ws.write("main.cpp", "#include <iostream>\nint helper();\nint main(){std::cout<<helper()<<std::endl;}\n");
        // 0xB0 0xA1 is a valid CP949 syllable and invalid UTF-8.
        let mut bytes: Vec<u8> = b"// ".to_vec();
        bytes.extend_from_slice(&[0xB0, 0xA1]);
        bytes.extend_from_slice(b"\nint helper(){return 42;}\n");
        std::fs::write(ws.path().join("helper.cpp"), &bytes).unwrap();

        let exe = ws.path().join(if cfg!(windows) { "cp.exe" } else { "cp" });
        let spec = plan_compile(ws.path(), "main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe);
        assert_eq!(
            spec.units.len(),
            2,
            "a non-UTF-8 sibling must still be offered to the compiler: {:?}",
            spec.units
        );
    }

    #[test]
    fn the_bits_stdcxx_shim_is_valid_cpp() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP the_bits_stdcxx_shim_is_valid_cpp: no C++ compiler");
                return;
            }
        };
        // This machine's GCC has the real <bits/stdc++.h>, so a normal compile
        // would never touch the shim and could not tell us whether it is even
        // valid. Force it with -I (before the system headers) so the shim IS
        // what gets included — the situation a macOS student is in.
        let ws = Ws::new("shim");
        let compat = ensure_compat_headers(ws.path()).expect("write compat headers");
        assert!(compat.join("bits").join("stdc++.h").is_file());

        ws.write(
            "main.cpp",
            "#include <bits/stdc++.h>\nusing namespace std;\n\
             int main(){ vector<int> v{3,1,2}; sort(v.begin(), v.end());\n\
             map<string,int> m; m[\"a\"]=1; string s=\"x\";\n\
             cout << v[0] << v[1] << v[2] << m[\"a\"] << s << endl; }\n",
        );
        let exe = ws.path().join(if cfg!(windows) { "shim.exe" } else { "shim" });
        let mut args = base_compile_args(DEFAULT_CPP_STANDARD);
        args.push("-I".to_string());
        args.push(compat.to_string_lossy().to_string());
        args.push("main.cpp".to_string());
        args.push("-o".to_string());
        args.push(exe.to_string_lossy().to_string());

        let mut cmd = Command::new(&c);
        cmd.args(&args).current_dir(ws.path()).stdin(Stdio::null());
        compile_env(&mut cmd);
        let out = quiet(&mut cmd).output().expect("compile with the shim");
        assert!(
            out.status.success(),
            "the shim header does not compile:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let run = Command::new(&exe).current_dir(ws.path()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "1231x");
    }

    #[test]
    fn bits_stdcxx_compiles_through_the_normal_run_path() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP bits_stdcxx_compiles_through_the_normal_run_path: no C++ compiler");
                return;
            }
        };
        // The include every competitive-programming habit teaches must work on
        // whatever this machine has, through exactly the arguments a Run uses.
        let ws = Ws::new("bits");
        let compat = ensure_compat_headers(ws.path()).expect("compat headers");
        ws.write(
            "main.cpp",
            "#include <bits/stdc++.h>\nusing namespace std;\n\
             int main(){ int n; cin >> n; vector<int> v(n); for(auto&x:v) cin>>x;\n\
             sort(v.rbegin(), v.rend()); for(int x:v) cout<<x<<' '; cout<<endl; }\n",
        );
        let exe = ws.path().join(if cfg!(windows) { "b.exe" } else { "b" });
        let spec = plan_compile_units(
            ws.path(), "main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe, usize::MAX, Some(&compat),
        );
        assert!(spec.args.iter().any(|a| a == "-idirafter"), "compat include missing");
        let (ok, diag) = spec.run();
        assert!(ok, "bits/stdc++.h failed to compile: {}", diag);

        let mut cmd = Command::new(&exe);
        cmd.current_dir(ws.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = quiet(&mut cmd).spawn().unwrap();
        {
            let mut sin = child.stdin.take().unwrap();
            sin.write_all(b"3\n5 1 9\n").unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "9 5 1");
    }

    /// Pull a `const NAME = ` + backtick-string out of the frontend source.
    ///
    /// The samples live in `src/main.ts` because they sit beside the Python
    /// ones; this reaches across to them rather than duplicating the text,
    /// which would drift.
    fn sample_from_main_ts(name: &str) -> Option<String> {
        let main_ts = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("src").join("main.ts");
        let src = std::fs::read_to_string(main_ts).ok()?;
        let needle = format!("const {} = `", name);
        let start = src.find(&needle)? + needle.len();
        let rest = &src[start..];
        let end = rest.find("`;")?;
        Some(rest[..end].to_string())
    }

    #[test]
    fn the_cpp_sample_files_compile_and_run() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP the_cpp_sample_files_compile_and_run: no C++ compiler");
                return;
            }
        };
        // These are the first C++ a student ever sees. A sample that does not
        // compile is worse than no sample: it looks like the IDE is broken.
        let main_cpp = match sample_from_main_ts("DEFAULT_MAIN_CPP") {
            Some(t) => t,
            None => {
                eprintln!("SKIP the_cpp_sample_files_compile_and_run: could not read src/main.ts");
                return;
            }
        };
        let input_cpp = sample_from_main_ts("DEFAULT_INPUT_CPP").expect("DEFAULT_INPUT_CPP");
        let proj_main = sample_from_main_ts("DEFAULT_PROJECT_MAIN_CPP").expect("DEFAULT_PROJECT_MAIN_CPP");
        let proj_h = sample_from_main_ts("DEFAULT_PROJECT_STATS_H").expect("DEFAULT_PROJECT_STATS_H");
        let proj_cpp = sample_from_main_ts("DEFAULT_PROJECT_STATS_CPP").expect("DEFAULT_PROJECT_STATS_CPP");

        let ws = Ws::new("samples");
        ws.write("main.cpp", &main_cpp);
        ws.write("test_input.cpp", &input_cpp);
        ws.write("cpp_project/main.cpp", &proj_main);
        ws.write("cpp_project/stats.h", &proj_h);
        ws.write("cpp_project/stats.cpp", &proj_cpp);

        // main.cpp: runs with no input.
        let (out, err, code) = compile_and_run(&ws, "main.cpp", &c, "");
        assert_eq!(code, Some(0), "sample main.cpp exited {:?}: {}", code, err);
        assert!(out.contains("Hello, MINT C++!"), "sample main.cpp said: {:?}", out);

        // test_input.cpp: the one that teaches the input box.
        let (out2, err2, code2) = compile_and_run(&ws, "test_input.cpp", &c, "3\n10 20 30\n");
        assert_eq!(code2, Some(0), "sample test_input.cpp exited {:?}: {}", code2, err2);
        assert!(out2.contains("60"), "sample test_input.cpp said: {:?}", out2);

        // cpp_project: the multi-file sample must link with no configuration.
        let exe = ws.path().join(if cfg!(windows) { "proj.exe" } else { "proj" });
        let spec = plan_compile(ws.path(), "cpp_project/main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe);
        assert_eq!(spec.units.len(), 2, "stats.cpp should be linked automatically: {:?}", spec.units);
        let (ok, diag) = spec.run();
        assert!(ok, "sample cpp_project failed to build: {}", diag);

        let mut cmd = Command::new(&exe);
        cmd.current_dir(ws.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = quiet(&mut cmd).spawn().unwrap();
        {
            let mut sin = child.stdin.take().unwrap();
            sin.write_all("1\n2\n3\n".as_bytes()).unwrap();
        }
        let done = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&done.stdout);
        assert!(text.contains("평균"), "sample cpp_project said: {:?}", text);
    }

    #[test]
    fn compile_error_is_reported_not_run() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP compile_error_is_reported_not_run: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("broken");
        ws.write("main.cpp", "#include <iostream>\nint main(){ this is not c++ }\n");
        let exe = ws.path().join(if cfg!(windows) { "prog.exe" } else { "prog" });
        let spec = plan_compile(ws.path(), "main.cpp", true, &c, DEFAULT_CPP_STANDARD, &exe);
        let (ok, diag) = spec.run();
        assert!(!ok, "a broken program must not compile");
        assert!(!exe.exists(), "no binary may be produced from a failed compile");
        // The diagnostic must carry a line number, which is what the editor's
        // error highlighting keys on.
        assert!(diag.contains(":2:"), "diagnostic lacked a line reference: {}", diag);
    }

    #[test]
    fn runtime_crash_surfaces_a_nonzero_exit() {
        let c = match cxx() {
            Some(c) => c,
            None => {
                eprintln!("SKIP runtime_crash_surfaces_a_nonzero_exit: no C++ compiler");
                return;
            }
        };
        let ws = Ws::new("crash");
        ws.write(
            "main.cpp",
            "#include <vector>\n#include <iostream>\n\
             int main(){std::vector<int> v; std::cout<<v.at(5)<<std::endl;}\n",
        );
        let (_, _, code) = compile_and_run(&ws, "main.cpp", &c, "");
        assert_ne!(code, Some(0), "an uncaught exception must not look like success");
    }
}
