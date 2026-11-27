use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn find_webview2_loader(build_dir: &Path, arch_dir: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(build_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("webview2-com-sys") {
            continue;
        }
        let path = entry.path();
        let direct = path.join("out").join(arch_dir).join("WebView2Loader.dll");
        if direct.is_file() {
            return Some(direct);
        }
        if let Ok(sub_entries) = fs::read_dir(&path) {
            for sub in sub_entries.flatten() {
                let candidate = sub.path().join("out").join(arch_dir).join("WebView2Loader.dll");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn find_profile_dir(out_dir: &Path) -> Option<PathBuf> {
    for ancestor in out_dir.ancestors() {
        if ancestor.file_name().and_then(|n| n.to_str()) == Some("build") {
            return ancestor.parent().map(Path::to_path_buf);
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let arch_dir = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") | Ok("arm64ec") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => return,
    };

    let Ok(out_dir) = env::var("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(profile_dir) = find_profile_dir(&out_dir) else {
        return;
    };
    let build_dir = profile_dir.join("build");
    if let Some(src_dll) = find_webview2_loader(&build_dir, arch_dir) {
        let dest_dll = profile_dir.join("WebView2Loader.dll");
        let _ = fs::copy(&src_dll, &dest_dll);
    }
}
