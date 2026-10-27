//! Embed version information and an application manifest into `mira-win.exe`.
//!
//! Uses the Windows SDK resource compiler (`rc.exe`) directly, so the crate
//! needs no build dependencies. If the SDK isn't found the build still
//! succeeds, just without the resources.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=mira.manifest");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        return;
    }
    let Some(rc) = find_rc(&target) else {
        println!("cargo:warning=rc.exe not found; building without version info");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version
        .split(['.', '-'])
        .filter_map(|p| p.parse().ok())
        .collect();
    parts.resize(4, 0);
    let commas = format!("{},{},{},{}", parts[0], parts[1], parts[2], parts[3]);
    let manifest = manifest_dir.join("mira.manifest");
    let rc_file = out.join("mira.rc");
    std::fs::write(
        &rc_file,
        format!(
            r#"#pragma code_page(65001)
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Mira contributors"
      VALUE "FileDescription", "Mira - a dynamic-island notch for Windows"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "mira-win"
      VALUE "LegalCopyright", "MIT License, Mira contributors"
      VALUE "OriginalFilename", "mira-win.exe"
      VALUE "ProductName", "Mira"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
            manifest = manifest.display().to_string().replace('\\', "\\\\"),
        ),
    )
    .unwrap();
    let res = out.join("mira.res");
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc_file)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
            // Our resource carries the manifest; don't let link.exe add its own.
            println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
        }
        _ => println!("cargo:warning=rc.exe failed; building without version info"),
    }
}

/// Newest `rc.exe` from the Windows 10/11 SDK for the target architecture.
fn find_rc(target: &str) -> Option<PathBuf> {
    let arch = if target.starts_with("aarch64") {
        "arm64"
    } else if target.starts_with("i686") {
        "x86"
    } else {
        "x64"
    };
    let roots = [
        std::env::var_os("ProgramFiles(x86)").map(PathBuf::from),
        std::env::var_os("ProgramFiles").map(PathBuf::from),
    ];
    let mut best: Option<PathBuf> = None;
    for root in roots.into_iter().flatten() {
        let bin = root.join("Windows Kits").join("10").join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            continue;
        };
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_sdk_version(p) && p.join(arch).join("rc.exe").exists())
            .collect();
        versions.sort();
        if let Some(v) = versions.pop() {
            best = Some(v.join(arch).join("rc.exe"));
        }
    }
    best
}

fn is_sdk_version(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("10."))
}
