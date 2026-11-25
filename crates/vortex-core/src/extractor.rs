use crate::model::{AppSettings, ExtractorKind, ExtractorProfile};
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[cfg(windows)]
use winreg::{RegKey, enums::*};

pub fn detect_default_extractors(existing: &[ExtractorProfile]) -> Vec<ExtractorProfile> {
    let mut profiles = Vec::new();

    let nanazip_path = find_nanazip();
    profiles.push(ExtractorProfile {
        id: "nanazip".into(),
        name: "NanaZip".into(),
        kind: ExtractorKind::NanaZip,
        executable: nanazip_path
            .clone()
            .unwrap_or_else(|| "NanaZipC.exe".into()),
        args_template: "x -y -o\"{dest}\" \"{archive}\"".into(),
        password_arg_template: Some("-p{password}".into()),
        silent: true,
        detected: nanazip_path.is_some(),
        detected_path: nanazip_path,
        supported_extensions: all_archive_exts(),
        enabled: true,
    });

    let sevenzip_path = find_sevenzip();
    profiles.push(ExtractorProfile {
        id: "7zip".into(),
        name: "7-Zip".into(),
        kind: ExtractorKind::SevenZip,
        executable: sevenzip_path.clone().unwrap_or_else(|| "7z.exe".into()),
        args_template: "x -y -o\"{dest}\" \"{archive}\"".into(),
        password_arg_template: Some("-p{password}".into()),
        silent: true,
        detected: sevenzip_path.is_some(),
        detected_path: sevenzip_path,
        supported_extensions: all_archive_exts(),
        enabled: true,
    });

    let winrar_path = find_winrar();
    profiles.push(ExtractorProfile {
        id: "winrar".into(),
        name: "WinRAR".into(),
        kind: ExtractorKind::WinRar,
        executable: winrar_path.clone().unwrap_or_else(|| "WinRAR.exe".into()),
        args_template: "x -y -ibck \"{archive}\" \"{dest}\\\"".into(),
        password_arg_template: Some("-p{password}".into()),
        silent: true,
        detected: winrar_path.is_some(),
        detected_path: winrar_path,
        supported_extensions: all_archive_exts(),
        enabled: true,
    });

    let peazip_path = find_peazip();
    profiles.push(ExtractorProfile {
        id: "peazip".into(),
        name: "PeaZip".into(),
        kind: ExtractorKind::PeaZip,
        executable: peazip_path.clone().unwrap_or_else(|| "peazip.exe".into()),
        args_template: "-ext2here \"{archive}\"".into(),
        password_arg_template: None,
        silent: true,
        detected: peazip_path.is_some(),
        detected_path: peazip_path,
        supported_extensions: all_archive_exts(),
        enabled: true,
    });

    let win_tar = find_in_path("tar.exe");
    profiles.push(ExtractorProfile {
        id: "windows_native".into(),
        name: "Windows Nativo (tar.exe / PowerShell)".into(),
        kind: ExtractorKind::WindowsNative,
        executable: win_tar.clone().unwrap_or_else(|| "tar.exe".into()),
        args_template: "-xf \"{archive}\" -C \"{dest}\"".into(),
        password_arg_template: None,
        silent: true,
        detected: true,
        detected_path: win_tar.or_else(|| Some("PowerShell Expand-Archive".into())),
        supported_extensions: vec![
            "zip", "rar", "7z", "tar", "gz", "tgz", "xz", "bz2", "zst", "cab",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        enabled: true,
    });

    // Preserve any user-created custom plugins or custom overrides
    for old in existing {
        if old.kind == ExtractorKind::CustomPlugin {
            let mut updated = old.clone();
            updated.detected = Path::new(&old.executable).exists()
                || find_in_path(&old.executable).is_some();
            profiles.push(updated);
        } else if let Some(prof) = profiles.iter_mut().find(|p| p.id == old.id) {
            if !old.executable.is_empty() && old.executable != prof.executable {
                // Keep user custom override if path exists
                if Path::new(&old.executable).exists() || find_in_path(&old.executable).is_some() {
                    prof.executable = old.executable.clone();
                    prof.detected = true;
                }
            }
            prof.enabled = old.enabled;
        }
    }

    profiles
}

fn all_archive_exts() -> Vec<String> {
    vec![
        "zip", "rar", "7z", "tar", "gz", "tgz", "xz", "bz2", "zst", "iso", "cab", "001",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub fn is_extractable_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let exts = [
        ".zip", ".rar", ".7z", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".tar.bz2", ".tar.zst",
        ".gz", ".xz", ".bz2", ".iso", ".cab", ".001",
    ];
    exts.iter().any(|e| name.ends_with(e))
}

pub fn select_best_extractor<'a>(
    settings: &'a AppSettings,
    override_id: Option<&str>,
) -> Option<&'a ExtractorProfile> {
    let chosen_id = override_id.unwrap_or(settings.default_extractor_id.as_str());
    if chosen_id == "none" {
        return None;
    }
    if chosen_id != "auto" {
        if let Some(p) = settings
            .extractors
            .iter()
            .find(|e| e.id == chosen_id && e.enabled && e.detected)
        {
            return Some(p);
        }
    }
    // Auto priority: NanaZip -> 7-Zip -> WinRAR -> PeaZip -> Windows Native -> Custom
    let priority = ["nanazip", "7zip", "winrar", "peazip", "windows_native"];
    for id in priority {
        if let Some(p) = settings
            .extractors
            .iter()
            .find(|e| e.id == id && e.enabled && e.detected)
        {
            return Some(p);
        }
    }
    settings
        .extractors
        .iter()
        .find(|e| e.enabled && e.detected)
}

pub async fn extract_archive(
    archive_path: &Path,
    dest_dir: &Path,
    profile: &ExtractorProfile,
    explicit_password: Option<&str>,
    password_list: &[String],
    into_subfolder: bool,
) -> Result<PathBuf> {
    if !archive_path.exists() {
        return Err(anyhow!(
            "Arquivo para extração não encontrado: {}",
            archive_path.display()
        ));
    }

    let final_dest = if into_subfolder {
        let folder_name = archive_stem_clean(archive_path);
        dest_dir.join(folder_name)
    } else {
        dest_dir.to_path_buf()
    };
    tokio::fs::create_dir_all(&final_dest).await?;

    let mut passwords_to_try: Vec<Option<String>> = Vec::new();
    if let Some(pw) = explicit_password.filter(|s| !s.is_empty()) {
        passwords_to_try.push(Some(pw.to_string()));
    }
    passwords_to_try.push(None);
    for pw in password_list {
        if !pw.is_empty() {
            passwords_to_try.push(Some(pw.clone()));
        }
    }

    let mut last_err = None;
    for pw in passwords_to_try {
        match run_extractor_once(archive_path, &final_dest, profile, pw.as_deref()).await {
            Ok(()) => return Ok(final_dest),
            Err(err) => {
                last_err = Some(err);
                if pw.is_none() && password_list.is_empty() && explicit_password.is_none() {
                    break;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("Falha desconhecida ao extrair arquivo")))
}

async fn run_extractor_once(
    archive_path: &Path,
    dest_dir: &Path,
    profile: &ExtractorProfile,
    password: Option<&str>,
) -> Result<()> {
    let archive_str = archive_path.to_string_lossy().to_string();
    let dest_str = dest_dir.to_string_lossy().to_string();

    if profile.kind == ExtractorKind::WindowsNative {
        // Try Windows 11 libarchive `tar.exe -xf "<archive>" -C "<dest>"`
        let output = Command::new(&profile.executable)
            .arg("-xf")
            .arg(&archive_str)
            .arg("-C")
            .arg(&dest_str)
            .output()
            .await;

        if let Ok(out) = output {
            if out.status.success() {
                return Ok(());
            }
            // Fallback for .zip using PowerShell Expand-Archive
            if archive_str.to_lowercase().ends_with(".zip") {
                let ps_cmd = format!(
                    "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                    archive_str.replace('\'', "''"),
                    dest_str.replace('\'', "''")
                );
                let ps_out = Command::new("powershell")
                    .args(["-NoProfile", "-Command", &ps_cmd])
                    .output()
                    .await?;
                if ps_out.status.success() {
                    return Ok(());
                }
                return Err(anyhow!(
                    "Windows Nativo falhou: {}",
                    String::from_utf8_lossy(&ps_out.stderr)
                ));
            }
            return Err(anyhow!(
                "tar.exe falhou: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }

    // PeaZip special CLI mode extracts beside archive or into folder
    if profile.kind == ExtractorKind::PeaZip && profile.args_template.contains("-ext2here") {
        // PeaZip's bundled 7z is fastest for silent target extraction if present
        let peazip_dir = Path::new(&profile.executable).parent();
        if let Some(dir) = peazip_dir {
            let bundled_7z = dir.join("res").join("bin").join("7z").join("7z.exe");
            if bundled_7z.exists() {
                let mut cmd = Command::new(&bundled_7z);
                cmd.arg("x").arg("-y");
                if let Some(pw) = password {
                    cmd.arg(format!("-p{pw}"));
                }
                cmd.arg(format!("-o{dest_str}")).arg(&archive_str);
                let out = cmd.output().await?;
                if out.status.success() {
                    return Ok(());
                }
                return Err(anyhow!(
                    "PeaZip/7z falhou: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
    }

    let mut rendered = profile
        .args_template
        .replace("{archive}", &archive_str)
        .replace("{dest}", &dest_str);

    if let Some(pw) = password {
        if let Some(ref pw_tpl) = profile.password_arg_template {
            let pw_arg = pw_tpl.replace("{password}", pw);
            rendered = format!("{pw_arg} {rendered}");
        }
    }

    let args = split_command_args(&rendered);
    let out = Command::new(&profile.executable)
        .args(&args)
        .output()
        .await
        .with_context(|| format!("Não foi possível executar {}", profile.executable))?;

    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        Err(anyhow!(
            "{} retornou erro ({}): {} {}",
            profile.name,
            out.status,
            stderr.trim(),
            stdout.trim()
        ))
    }
}

fn split_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in input.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' if !in_quotes => {
                if !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        args.push(cur);
    }
    args
}

fn archive_stem_clean(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted");
    let lower = name.to_lowercase();
    for suffix in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".part01.rar", ".part1.rar", ".7z.001", ".zip.001"] {
        if lower.ends_with(suffix) {
            return name[..name.len() - suffix.len()].to_string();
        }
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted")
        .to_string()
}

pub fn find_in_path(binary: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

fn find_nanazip() -> Option<String> {
    if let Some(p) = find_in_path("NanaZipC.exe") {
        return Some(p);
    }
    if let Some(local) = dirs::data_local_dir() {
        let alias = local.join("Microsoft").join("WindowsApps").join("NanaZipC.exe");
        if alias.exists() {
            return Some(alias.to_string_lossy().to_string());
        }
        let alias_gui = local.join("Microsoft").join("WindowsApps").join("NanaZip.exe");
        if alias_gui.exists() {
            // Check WindowsApps install location for NanaZipC.exe
            if let Some(pkg) = find_nanazip_appx_cli() {
                return Some(pkg);
            }
            return Some(alias_gui.to_string_lossy().to_string());
        }
    }
    find_nanazip_appx_cli()
}

fn find_nanazip_appx_cli() -> Option<String> {
    let win_apps = PathBuf::from(r"C:\Program Files\WindowsApps");
    let d_win_apps = PathBuf::from(r"D:\Program Files\WindowsApps");
    for base in [win_apps, d_win_apps] {
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("MouriNaruto.NanaZip") {
                    let cli = entry.path().join("NanaZipC.exe");
                    if cli.exists() {
                        return Some(cli.to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    None
}

fn find_sevenzip() -> Option<String> {
    let candidates = [
        r"C:\Program Files\7-Zip\7z.exe",
        r"C:\Program Files (x86)\7-Zip\7z.exe",
        r"D:\Program Files\7-Zip\7z.exe",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    #[cfg(windows)]
    {
        if let Ok(hklm) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SOFTWARE\7-Zip") {
            if let Ok(dir) = hklm.get_value::<String, _>("Path") {
                let exe = Path::new(&dir).join("7z.exe");
                if exe.exists() {
                    return Some(exe.to_string_lossy().to_string());
                }
            }
        }
    }
    find_in_path("7z.exe")
}

fn find_winrar() -> Option<String> {
    let candidates = [
        r"C:\Program Files\WinRAR\WinRAR.exe",
        r"C:\Program Files\WinRAR\UnRAR.exe",
        r"C:\Program Files (x86)\WinRAR\WinRAR.exe",
        r"D:\Program Files\WinRAR\WinRAR.exe",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    #[cfg(windows)]
    {
        for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            if let Ok(key) = RegKey::predef(root).open_subkey(r"SOFTWARE\WinRAR") {
                if let Ok(exe) = key.get_value::<String, _>("exe64") {
                    if Path::new(&exe).exists() {
                        return Some(exe);
                    }
                }
            }
        }
    }
    find_in_path("WinRAR.exe").or_else(|| find_in_path("UnRAR.exe"))
}

fn find_peazip() -> Option<String> {
    let candidates = [
        r"C:\Program Files\PeaZip\peazip.exe",
        r"C:\Program Files (x86)\PeaZip\peazip.exe",
        r"D:\Program Files\PeaZip\peazip.exe",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    find_in_path("peazip.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_stem_strips_multipart_suffixes() {
        assert_eq!(archive_stem_clean(Path::new(r"C:\dl\Game.part01.rar")), "Game");
        assert_eq!(archive_stem_clean(Path::new(r"C:\dl\Dataset.7z.001")), "Dataset");
        assert_eq!(archive_stem_clean(Path::new(r"C:\dl\bundle.tar.gz")), "bundle");
    }

    #[test]
    fn detects_builtin_extractors_on_machine() {
        let profiles = detect_default_extractors(&[]);
        assert!(profiles.iter().any(|p| p.id == "nanazip"));
        assert!(profiles.iter().any(|p| p.id == "winrar"));
        assert!(profiles.iter().any(|p| p.id == "peazip"));
        assert!(profiles.iter().any(|p| p.id == "windows_native"));
    }

    #[test]
    fn splits_quoted_command_args() {
        let args = split_command_args(r#"x -y -o"{dest}" "{archive}""#);
        assert_eq!(args, vec!["x", "-y", "-o{dest}", "{archive}"]);
    }
}
