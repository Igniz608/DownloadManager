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
    Write-Host "[1/4] Running workspace tests..." -ForegroundColor Yellow
    cargo test --workspace
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }

    Write-Host "[2/4] Building release binaries..." -ForegroundColor Yellow
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }
}

$ReleaseDir = Join-Path $RepoRoot "target\release"
$DistDir = Join-Path $RepoRoot "dist"
New-Item -ItemType Directory -Force -Path $DistDir | Out-Null

$RequiredFiles = @("vortex-gui.exe", "vortex-native-host.exe", "WebView2Loader.dll")
foreach ($f in $RequiredFiles) {
    $p = Join-Path $ReleaseDir $f
    if (-not (Test-Path $p)) {
        throw "Missing release artifact: $p"
    }
}

Write-Host "[3/4] Packaging Portable ZIP..." -ForegroundColor Yellow
$PortableName = "VortexDL-Portable-v$Version-win64"
$StagingDir = Join-Path $DistDir $PortableName
$ZipPath = Join-Path $DistDir "$PortableName.zip"

if (Test-Path $StagingDir) { Remove-Item -Recurse -Force $StagingDir }
if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }

New-Item -ItemType Directory -Force -Path $StagingDir | Out-Null
foreach ($f in $RequiredFiles) {
    Copy-Item (Join-Path $ReleaseDir $f) -Destination (Join-Path $StagingDir $f)
}
Copy-Item (Join-Path $RepoRoot "extension") -Destination (Join-Path $StagingDir "extension") -Recurse
New-Item -ItemType File -Force -Path (Join-Path $StagingDir "portable.flag") | Out-Null

$ReadmeText = @"
VortexDL Portable v$Version
===========================

- Todos os dados (configurações, fila, yt-dlp/FFmpeg e perfil WebView2) são salvos na pasta .\data ao lado do executável enquanto o arquivo portable.flag existir.
- Para integrar com seu navegador:
  1. Abra o vortex-gui.exe e vá na aba "Extensão".
  2. Marque os navegadores instalados e clique em "Integrar Selecionados e Abrir Extensões".
  3. O caminho da pasta .\extension já vai copiado para a área de transferência; ative o Modo do desenvolvedor e clique em "Carregar sem compactação" (Ctrl+V).
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

Write-Host "[4/4] Building Windows Setup EXE (Inno Setup)..." -ForegroundColor Yellow
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
& $Iscc "/DMyAppVersion=$Version" "/DRepoRoot=$RepoRoot" "/DReleaseDir=$ReleaseDir" "/DOutputDir=$DistDir" $IssPath
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }

$SetupExe = Join-Path $DistDir "VortexDL-Setup-v$Version-win64.exe"
Write-Host ""
Write-Host "Release artifacts ready:" -ForegroundColor Green
Get-Item $ZipPath, $SetupExe | ForEach-Object {
    $mb = [math]::Round($_.Length / 1MB, 2)
    Write-Host ("  - {0} ({1} MB)" -f $_.FullName, $mb)
}
