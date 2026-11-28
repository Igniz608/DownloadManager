use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub const GITHUB_REPO_OWNER: &str = "Igniz608";
pub const GITHUB_REPO_NAME: &str = "DownloadManager";

pub fn current_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseAssetInfo {
    pub name: String,
    pub download_url: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseUpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub tag_name: String,
    pub release_name: String,
    pub release_notes: String,
    pub html_url: String,
    pub published_at: Option<String>,
    pub update_available: bool,
    pub installer_asset: Option<ReleaseAssetInfo>,
    pub portable_asset: Option<ReleaseAssetInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateLaunchOutcome {
    RestartingNow,
    OpenBrowser(String),
}

#[derive(Debug, Deserialize)]
struct GithubReleaseDto {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAssetDto>,
}

#[derive(Debug, Deserialize)]
struct GithubAssetDto {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

pub fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    if trimmed.is_empty() {
        return None;
    }
    let core = trimmed
        .split(['-', '+'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => {
            let c1 = current.trim().trim_start_matches(['v', 'V']);
            let c2 = candidate.trim().trim_start_matches(['v', 'V']);
            !c2.is_empty() && c1 != c2
        }
    }
}

fn select_release_assets(
    assets: &[GithubAssetDto],
) -> (Option<ReleaseAssetInfo>, Option<ReleaseAssetInfo>) {
    let mut installer: Option<ReleaseAssetInfo> = None;
    let mut portable: Option<ReleaseAssetInfo> = None;

    for a in assets {
        let lower = a.name.to_ascii_lowercase();
        let info = ReleaseAssetInfo {
            name: a.name.clone(),
            download_url: a.browser_download_url.clone(),
            size_bytes: a.size,
        };
        if lower.ends_with(".exe")
            && (lower.contains("setup") || lower.contains("vortexdl") || installer.is_none())
        {
            if installer.is_none() || lower.contains("setup") {
                installer = Some(info);
            }
        } else if lower.ends_with(".zip")
            && (lower.contains("portable") || lower.contains("vortexdl"))
        {
            if portable.is_none() || lower.contains("portable") {
                portable = Some(info);
            }
        }
    }
    (installer, portable)
}

pub async fn check_for_updates(client: &Client) -> Result<ReleaseUpdateInfo> {
    let url = format!(
        "https://api.github.com/repos/{GITHUB_REPO_OWNER}/{GITHUB_REPO_NAME}/releases/latest"
    );
    let ua = format!("VortexDL/{}", current_app_version());

    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .context("Não foi possível conectar ao GitHub para verificar atualizações")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow!(
            "Nenhuma release publicada encontrada em {GITHUB_REPO_OWNER}/{GITHUB_REPO_NAME}."
        ));
    }
    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(anyhow!(
            "Limite de requisições da API do GitHub atingido temporariamente. Tente novamente em alguns minutos."
        ));
    }

    let dto: GithubReleaseDto = resp
        .error_for_status()
        .context("Falha ao consultar releases no GitHub")?
        .json()
        .await
        .context("Resposta inválida da API de releases do GitHub")?;

    let current_version = current_app_version().to_string();
    let latest_version = dto
        .tag_name
        .trim()
        .trim_start_matches(['v', 'V'])
        .to_string();
    let update_available = is_newer_version(&current_version, &dto.tag_name);
    let (installer_asset, portable_asset) = select_release_assets(&dto.assets);

    Ok(ReleaseUpdateInfo {
        current_version,
        latest_version,
        tag_name: dto.tag_name,
        release_name: dto.name.unwrap_or_default(),
        release_notes: dto.body.unwrap_or_default(),
        html_url: dto.html_url,
        published_at: dto.published_at,
        update_available,
        installer_asset,
        portable_asset,
    })
}

fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub async fn download_and_launch_update<F>(
    client: &Client,
    info: &ReleaseUpdateInfo,
    on_progress: F,
) -> Result<UpdateLaunchOutcome>
where
    F: Fn(u64, Option<u64>) + Send + Sync,
{
    let portable = crate::state::is_portable_mode();
    let chosen_asset = if portable {
        info.portable_asset
            .as_ref()
            .or(info.installer_asset.as_ref())
    } else {
        info.installer_asset
            .as_ref()
            .or(info.portable_asset.as_ref())
    };

    let Some(asset) = chosen_asset else {
        return Ok(UpdateLaunchOutcome::OpenBrowser(info.html_url.clone()));
    };

    let update_dir = std::env::temp_dir().join("VortexDL-Update");
    let _ = tokio::fs::remove_dir_all(&update_dir).await;
    tokio::fs::create_dir_all(&update_dir).await?;

    let asset_path = update_dir.join(&asset.name);
    let dl_client = Client::builder()
        .user_agent(format!("VortexDL/{}", current_app_version()))
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .unwrap_or_else(|_| client.clone());

    let resp = dl_client
        .get(&asset.download_url)
        .send()
        .await
        .context("Falha ao baixar pacote de atualização")?
        .error_for_status()
        .context("Erro HTTP ao baixar pacote de atualização")?;

    let total = resp.content_length().or(Some(asset.size_bytes).filter(|&s| s > 0));
    let mut file = tokio::fs::File::create(&asset_path).await?;
    let mut stream = resp.bytes_stream();
    let mut downloaded = 0u64;
    on_progress(0, total);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Erro durante o download da atualização")?;
        file.write_all(&chunk).await?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        on_progress(downloaded, total);
    }
    file.flush().await?;
    drop(file);

    let is_zip = asset.name.to_ascii_lowercase().ends_with(".zip");
    launch_windows_updater(&update_dir, &asset_path, is_zip).await
}

#[cfg(windows)]
fn build_windows_update_script(
    pid: u32,
    exe_dir: &Path,
    asset_path: &Path,
    update_dir: &Path,
    src_root: Option<&Path>,
) -> String {
    if let Some(src_root) = src_root {
        format!(
            r#"$ErrorActionPreference = 'SilentlyContinue'
$pidToWait = {pid}
$targetDir = {target_dir}
$srcRoot = {src_root}
$updateDir = {update_dir}
$logFile = Join-Path $env:TEMP 'VortexDL-Update.log'
function Write-Log([string]$msg) {{
    try {{ Add-Content -LiteralPath $logFile -Value ("$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $msg") -Encoding UTF8 }} catch {{}}
}}
Set-Content -LiteralPath $logFile -Value ("$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') Starting portable update (pid=$pidToWait, target=$targetDir)") -Encoding UTF8
for ($i = 0; $i -lt 30; $i++) {{
    if (-not (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue)) {{ break }}
    Start-Sleep -Milliseconds 300
}}
if (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue) {{
    Write-Log "Force stopping pid $pidToWait"
    Stop-Process -Id $pidToWait -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400
}}
Get-Process | Where-Object {{ $_.Path -and $_.Path.StartsWith($targetDir, [System.StringComparison]::OrdinalIgnoreCase) }} | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Get-ChildItem -LiteralPath $srcRoot -Force | ForEach-Object {{
    if ($_.Name -ieq 'data' -or $_.Name -ieq 'portable.flag') {{ return }}
    $dest = Join-Path $targetDir $_.Name
    if ($_.PSIsContainer) {{
        Copy-Item -LiteralPath $_.FullName -Destination $dest -Recurse -Force
    }} else {{
        Copy-Item -LiteralPath $_.FullName -Destination $dest -Force
    }}
}}
$gui = Join-Path $targetDir 'vortex-gui.exe'
if (Test-Path $gui) {{
    Write-Log "Relaunching $gui"
    Start-Process -FilePath $gui -WorkingDirectory $targetDir
}} else {{
    Write-Log "GUI binary missing after copy: $gui"
}}
Start-Sleep -Seconds 2
Remove-Item -LiteralPath $updateDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Log "Portable update finished"
"#,
            pid = pid,
            target_dir = ps_single_quote(&exe_dir.to_string_lossy()),
            src_root = ps_single_quote(&src_root.to_string_lossy()),
            update_dir = ps_single_quote(&update_dir.to_string_lossy()),
        )
    } else {
        format!(
            r#"$ErrorActionPreference = 'SilentlyContinue'
$pidToWait = {pid}
$targetDir = {target_dir}
$setupExe = {setup_exe}
$updateDir = {update_dir}
$logFile = Join-Path $env:TEMP 'VortexDL-Update.log'
$setupLog = Join-Path $updateDir 'setup.log'
function Write-Log([string]$msg) {{
    try {{ Add-Content -LiteralPath $logFile -Value ("$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $msg") -Encoding UTF8 }} catch {{}}
}}
Set-Content -LiteralPath $logFile -Value ("$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') Starting setup update (pid=$pidToWait, target=$targetDir, setup=$setupExe)") -Encoding UTF8
for ($i = 0; $i -lt 30; $i++) {{
    if (-not (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue)) {{ break }}
    Start-Sleep -Milliseconds 300
}}
if (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue) {{
    Write-Log "Force stopping pid $pidToWait"
    Stop-Process -Id $pidToWait -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400
}}
Get-Process | Where-Object {{ $_.Path -and $_.Path.StartsWith($targetDir, [System.StringComparison]::OrdinalIgnoreCase) }} | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
$dirArg = '/DIR=' + '"' + $targetDir + '"'
$logArg = '/LOG=' + '"' + $setupLog + '"'
Write-Log "Launching Inno Setup"
$proc = Start-Process -FilePath $setupExe -ArgumentList @('/SP-', '/SILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CLOSEAPPLICATIONS', $dirArg, $logArg) -Wait -PassThru
$exitCode = if ($proc) {{ $proc.ExitCode }} else {{ -1 }}
Write-Log "Setup finished with exitCode=$exitCode"
$gui = Join-Path $targetDir 'vortex-gui.exe'
if (Test-Path $gui) {{
    Write-Log "Relaunching $gui"
    Start-Process -FilePath $gui -WorkingDirectory $targetDir
}} else {{
    Write-Log "GUI binary missing after setup: $gui"
}}
Start-Sleep -Seconds 2
if ($exitCode -eq 0) {{
    Remove-Item -LiteralPath $updateDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Log "Cleaned up $updateDir"
}} else {{
    Write-Log "Preserving $updateDir for troubleshooting (exitCode=$exitCode)"
}}
"#,
            pid = pid,
            target_dir = ps_single_quote(&exe_dir.to_string_lossy()),
            setup_exe = ps_single_quote(&asset_path.to_string_lossy()),
            update_dir = ps_single_quote(&update_dir.to_string_lossy()),
        )
    }
}

#[cfg(windows)]
const WINDOWS_UPDATER_CREATION_FLAGS: u32 = 0x0800_0000 | 0x0000_0200; // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP

#[cfg(windows)]
async fn launch_windows_updater(
    update_dir: &Path,
    asset_path: &Path,
    is_zip: bool,
) -> Result<UpdateLaunchOutcome> {
    let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("vortex-gui.exe"));
    let exe_dir = current_exe
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let pid = std::process::id();
    let script_path = update_dir.join("apply_update.ps1");

    let src_root = if is_zip {
        let extract_dir = update_dir.join("extracted");
        tokio::fs::create_dir_all(&extract_dir).await?;
        let tar_bin = crate::extractor::find_windows_tar().unwrap_or_else(|| "tar.exe".to_string());
        let mut tar_cmd = tokio::process::Command::new(tar_bin);
        crate::extractor::configure_silent_command(&mut tar_cmd);
        let out = tar_cmd
            .args([
                "-xf",
                &asset_path.to_string_lossy(),
                "-C",
                &extract_dir.to_string_lossy(),
            ])
            .output()
            .await?;
        if !out.status.success() {
            return Err(anyhow!(
                "Falha ao extrair atualização portátil: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }

        Some(
            find_extracted_app_root(&extract_dir)
                .ok_or_else(|| anyhow!("vortex-gui.exe não encontrado dentro do pacote ZIP"))?,
        )
    } else {
        None
    };

    let script =
        build_windows_update_script(pid, &exe_dir, asset_path, update_dir, src_root.as_deref());

    tokio::fs::write(&script_path, script).await?;

    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script_path.to_string_lossy(),
        ])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(WINDOWS_UPDATER_CREATION_FLAGS)
        .spawn()
        .context("Falha ao iniciar processo de atualização")?;

    Ok(UpdateLaunchOutcome::RestartingNow)
}

#[cfg(not(windows))]
async fn launch_windows_updater(
    _update_dir: &Path,
    asset_path: &Path,
    _is_zip: bool,
) -> Result<UpdateLaunchOutcome> {
    Ok(UpdateLaunchOutcome::OpenBrowser(
        asset_path.to_string_lossy().to_string(),
    ))
}

#[cfg(windows)]
fn find_extracted_app_root(extract_dir: &Path) -> Option<PathBuf> {
    if extract_dir.join("vortex-gui.exe").is_file() {
        return Some(extract_dir.to_path_buf());
    }
    if let Ok(entries) = std::fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("vortex-gui.exe").is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_cleanly() {
        assert_eq!(parse_semver("v0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("0.2.1-rc1"), Some((0, 2, 1)));
        assert!(is_newer_version("0.1.0", "v0.1.1"));
        assert!(is_newer_version("0.1.9", "v0.2.0"));
        assert!(!is_newer_version("0.1.0", "v0.1.0"));
        assert!(!is_newer_version("0.2.0", "v0.1.9"));
    }

    #[test]
    fn selects_setup_and_portable_assets() {
        let assets = vec![
            GithubAssetDto {
                name: "VortexDL-Portable-v0.1.1-win64.zip".into(),
                browser_download_url: "https://example.com/portable.zip".into(),
                size: 20_000_000,
            },
            GithubAssetDto {
                name: "VortexDL-Setup-v0.1.1-win64.exe".into(),
                browser_download_url: "https://example.com/setup.exe".into(),
                size: 18_000_000,
            },
            GithubAssetDto {
                name: "vortexdl-extension-chromium.zip".into(),
                browser_download_url: "https://example.com/ext.zip".into(),
                size: 50_000,
            },
        ];
        let (setup, portable) = select_release_assets(&assets);
        assert_eq!(
            setup.as_ref().map(|a| a.name.as_str()),
            Some("VortexDL-Setup-v0.1.1-win64.exe")
        );
        assert_eq!(
            portable.as_ref().map(|a| a.name.as_str()),
            Some("VortexDL-Portable-v0.1.1-win64.zip")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_updater_uses_valid_creation_flags_and_scripts() {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

        assert_eq!(
            WINDOWS_UPDATER_CREATION_FLAGS,
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        );
        assert_eq!(WINDOWS_UPDATER_CREATION_FLAGS & DETACHED_PROCESS, 0);

        let setup_script = build_windows_update_script(
            4242,
            Path::new(r"D:\Program Files\Vortex's App"),
            Path::new(r"D:\Temp\VortexDL-Update\VortexDL-Setup.exe"),
            Path::new(r"D:\Temp\VortexDL-Update"),
            None,
        );
        assert!(setup_script.contains("D:\\Program Files\\Vortex''s App"));
        assert!(setup_script.contains("Start-Process -FilePath $setupExe"));
        assert!(setup_script.contains("VortexDL-Update.log"));

        let portable_script = build_windows_update_script(
            4242,
            Path::new(r"D:\Portable\VortexDL"),
            Path::new(r"D:\Temp\VortexDL-Update\VortexDL-Portable.zip"),
            Path::new(r"D:\Temp\VortexDL-Update"),
            Some(Path::new(r"D:\Temp\VortexDL-Update\extracted")),
        );
        assert!(portable_script.contains("Copy-Item -LiteralPath $_.FullName"));
        assert!(portable_script.contains("portable.flag"));
    }
}
