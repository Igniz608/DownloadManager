param(
    [string]$Version = "",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $RepoRoot

if ([string]::IsNullOrWhiteSpace($Version)) {
    $cargoToml = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
    if ($cargoToml -match 'version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        $Version = "0.1.0"
    }
}

Write-Host "=== VortexDL Release Builder (v$Version) ===" -ForegroundColor Cyan

if (-not $SkipBuild) {
    Write-Host "[1/5] Running workspace tests..." -ForegroundColor Yellow
    cargo test --workspace
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }

    Write-Host "[2/5] Building release binaries..." -ForegroundColor Yellow
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }
}

$ReleaseDir = Join-Path $RepoRoot "target\release"
$DistDir = Join-Path $RepoRoot "dist"
$BundleBinDir = Join-Path $RepoRoot "build\bundle-bin"
New-Item -ItemType Directory -Force -Path $DistDir | Out-Null
New-Item -ItemType Directory -Force -Path $BundleBinDir | Out-Null

$RequiredFiles = @("vortex-gui.exe", "vortex-native-host.exe", "WebView2Loader.dll")
foreach ($f in $RequiredFiles) {
    $p = Join-Path $ReleaseDir $f
    if (-not (Test-Path $p)) {
        throw "Missing release artifact: $p"
    }
}

Write-Host "[3/5] Preparing bundled yt-dlp & FFmpeg binaries..." -ForegroundColor Yellow
function Resolve-Or-StageTool {
    param([string]$ExeName)
    $dest = Join-Path $BundleBinDir $ExeName
    if (Test-Path $dest) { return $dest }

    $localBin = Join-Path $env:LOCALAPPDATA "VortexDL\bin\$ExeName"
    if (Test-Path $localBin) {
        Copy-Item $localBin -Destination $dest -Force
        return $dest
    }
    $onPath = Get-Command $ExeName -ErrorAction SilentlyContinue
    if ($onPath -and (Test-Path $onPath.Source)) {
        Copy-Item $onPath.Source -Destination $dest -Force
        return $dest
    }
    return $null
}

$ytDlpExe = Resolve-Or-StageTool "yt-dlp.exe"
if (-not $ytDlpExe) {
    Write-Host "  Downloading yt-dlp.exe from GitHub..." -ForegroundColor DarkGray
    $ytDest = Join-Path $BundleBinDir "yt-dlp.exe"
    Invoke-WebRequest -Uri "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe" -OutFile $ytDest -UseBasicParsing
    $ytDlpExe = $ytDest
}

$ffmpegExe = Resolve-Or-StageTool "ffmpeg.exe"
$ffprobeExe = Resolve-Or-StageTool "ffprobe.exe"
if ((-not $ffmpegExe) -or (-not $ffprobeExe)) {
    Write-Host "  Downloading FFmpeg essentials (ffmpeg + ffprobe)..." -ForegroundColor DarkGray
    $ffZip = Join-Path $BundleBinDir "ffmpeg-essentials.zip"
    $ffExtract = Join-Path $BundleBinDir "_ffmpeg_extract"
    if (Test-Path $ffExtract) { Remove-Item -Recurse -Force $ffExtract }
    $urls = @(
        "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
        "https://github.com/GyanD/codexffmpeg/releases/latest/download/ffmpeg-release-essentials.zip",
        "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip"
    )
    $downloaded = $false
    foreach ($u in $urls) {
        try {
            Invoke-WebRequest -Uri $u -OutFile $ffZip -UseBasicParsing -TimeoutSec 180
            $downloaded = $true
            break
        } catch {
            Write-Host "  Mirror failed ($u): $($_.Exception.Message)" -ForegroundColor DarkYellow
        }
    }
    if (-not $downloaded) { throw "Failed to download FFmpeg essentials from all mirrors." }
    Expand-Archive -Path $ffZip -DestinationPath $ffExtract -Force
    $foundFfmpeg = Get-ChildItem -Path $ffExtract -Filter "ffmpeg.exe" -Recurse | Select-Object -First 1
    $foundFfprobe = Get-ChildItem -Path $ffExtract -Filter "ffprobe.exe" -Recurse | Select-Object -First 1
    if (-not $foundFfmpeg -or -not $foundFfprobe) {
        throw "ffmpeg.exe or ffprobe.exe not found inside archive."
    }
    Copy-Item $foundFfmpeg.FullName -Destination (Join-Path $BundleBinDir "ffmpeg.exe") -Force
    Copy-Item $foundFfprobe.FullName -Destination (Join-Path $BundleBinDir "ffprobe.exe") -Force
    Remove-Item -Force $ffZip -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $ffExtract -ErrorAction SilentlyContinue
}

foreach ($toolExe in @("yt-dlp.exe", "ffmpeg.exe", "ffprobe.exe")) {
    $tp = Join-Path $BundleBinDir $toolExe
    if (-not (Test-Path $tp)) { throw "Missing bundled tool binary: $tp" }
    $mb = [math]::Round((Get-Item $tp).Length / 1MB, 2)
    Write-Host ("  - Bundled {0} ({1} MB)" -f $toolExe, $mb) -ForegroundColor DarkGreen
}

Write-Host "[4/5] Packaging Portable ZIP..." -ForegroundColor Yellow
$PortableName = "VortexDL-Portable-v$Version-win64"
$StagingDir = Join-Path $DistDir $PortableName
$ZipPath = Join-Path $DistDir "$PortableName.zip"

if (Test-Path $StagingDir) { Remove-Item -Recurse -Force $StagingDir }
if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }

New-Item -ItemType Directory -Force -Path $StagingDir | Out-Null
foreach ($f in $RequiredFiles) {
    Copy-Item (Join-Path $ReleaseDir $f) -Destination (Join-Path $StagingDir $f)
}
$PortableBinDir = Join-Path $StagingDir "bin"
New-Item -ItemType Directory -Force -Path $PortableBinDir | Out-Null
foreach ($toolExe in @("yt-dlp.exe", "ffmpeg.exe", "ffprobe.exe")) {
    Copy-Item (Join-Path $BundleBinDir $toolExe) -Destination (Join-Path $PortableBinDir $toolExe)
}
Copy-Item (Join-Path $RepoRoot "extension") -Destination (Join-Path $StagingDir "extension") -Recurse
$FirefoxXpi = Join-Path $RepoRoot "installer\vortexdl-firefox.xpi"
if (Test-Path $FirefoxXpi) {
    Copy-Item $FirefoxXpi -Destination (Join-Path $StagingDir "vortexdl-firefox.xpi")
}
New-Item -ItemType File -Force -Path (Join-Path $StagingDir "portable.flag") | Out-Null

$ReadmeText = @"
VortexDL Portable v$Version
===========================

- yt-dlp.exe, ffmpeg.exe e ffprobe.exe já vêm incluídos na pasta .\bin por padrão.
- Todos os dados (configurações, fila e perfil WebView2) são salvos na pasta .\data ao lado do executável enquanto o arquivo portable.flag existir.
- Para integrar com seu navegador:
  1. Abra o vortex-gui.exe e vá na aba "Extensão".
  2. Marque os navegadores instalados e clique em "Integrar Selecionados".
  3. Edge: instala automaticamente pela Microsoft Edge Add-ons Store (reinicie o Edge se estiver aberto).
  4. Firefox: abre o pacote assinado vortexdl-firefox.xpi direto no Firefox (clique em "Adicionar", sem about:debugging).
  5. Chrome / Brave / Vivaldi / Opera: o caminho da pasta .\extension já vai copiado; ative o Modo do desenvolvedor e clique em "Carregar sem compactação" (Ctrl+V).
"@
Set-Content -Path (Join-Path $StagingDir "LEIA-ME-PORTABLE.txt") -Value $ReadmeText -Encoding UTF8

$SevenZip = Get-Command 7z -ErrorAction SilentlyContinue
if ($SevenZip) {
    Push-Location $DistDir
    try {
        & $SevenZip.Source a -tzip -mx=9 $ZipPath $PortableName | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "7z failed" }
    } finally {
        Pop-Location
    }
} else {
    Compress-Archive -Path $StagingDir -DestinationPath $ZipPath -CompressionLevel Optimal
}

Remove-Item -Recurse -Force $StagingDir

Write-Host "[5/5] Building Windows Setup EXE (Inno Setup)..." -ForegroundColor Yellow
$IsccCandidates = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
)
$IsccCmd = Get-Command iscc -ErrorAction SilentlyContinue
if ($IsccCmd) { $IsccCandidates = @($IsccCmd.Source) + $IsccCandidates }
$Iscc = $IsccCandidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1

if (-not $Iscc) {
    throw "Inno Setup 6 (ISCC.exe) not found. Install via: winget install -e --id JRSoftware.InnoSetup"
}

$IssPath = Join-Path $RepoRoot "installer\vortexdl.iss"
& $Iscc "/DMyAppVersion=$Version" "/DRepoRoot=$RepoRoot" "/DReleaseDir=$ReleaseDir" "/DOutputDir=$DistDir" "/DBundleBinDir=$BundleBinDir" $IssPath
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }

$SetupExe = Join-Path $DistDir "VortexDL-Setup-v$Version-win64.exe"
$ExtPkgScript = Join-Path $PSScriptRoot "build-extension-packages.ps1"
if (Test-Path $ExtPkgScript) {
    & $ExtPkgScript -OutputDir $DistDir
}

Write-Host ""
Write-Host "Release artifacts ready:" -ForegroundColor Green
Get-Item $ZipPath, $SetupExe | ForEach-Object {
    $mb = [math]::Round($_.Length / 1MB, 2)
    Write-Host ("  - {0} ({1} MB)" -f $_.FullName, $mb)
}
