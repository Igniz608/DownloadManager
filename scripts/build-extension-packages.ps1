param(
    [string]$OutputDir = ""
)

$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$ExtDir = Join-Path $RepoRoot "extension"
$ManifestPath = Join-Path $ExtDir "manifest.json"

if (-not (Test-Path $ManifestPath)) {
    throw "Missing extension manifest: $ManifestPath"
}

if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $RepoRoot "dist"
}
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$ManifestRaw = [System.IO.File]::ReadAllText($ManifestPath, $Utf8NoBom)
$BaseManifest = $ManifestRaw | ConvertFrom-Json
$ExtVersion = if ($BaseManifest.version) { [string]$BaseManifest.version } else { "1.0.0" }

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function New-CleanExtensionStaging([string]$Name) {
    $staging = Join-Path $OutputDir "_staging_$Name"
    if (Test-Path $staging) {
        Remove-Item -Recurse -Force $staging
    }
    New-Item -ItemType Directory -Force -Path $staging | Out-Null

    Get-ChildItem -Path $ExtDir -Force | Where-Object { $_.Name -ne "manifest.json" } | ForEach-Object {
        Copy-Item -Path $_.FullName -Destination (Join-Path $staging $_.Name) -Recurse -Force
    }
    return $staging
}

function Write-ZipFromStaging([string]$StagingDir, [string]$ZipPath) {
    if (Test-Path $ZipPath) {
        Remove-Item -Force $ZipPath
    }
    $archive = [System.IO.Compression.ZipFile]::Open($ZipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        $baseLen = $StagingDir.TrimEnd('\', '/').Length + 1
        Get-ChildItem -Path $StagingDir -Recurse -File | Sort-Object FullName | ForEach-Object {
            $entryName = $_.FullName.Substring($baseLen).Replace('\', '/')
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $archive,
                $_.FullName,
                $entryName,
                [System.IO.Compression.CompressionLevel]::Optimal
            ) | Out-Null
        }
    } finally {
        $archive.Dispose()
    }
    Remove-Item -Recurse -Force $StagingDir
}

function Format-CleanJson($Obj) {
    $json = ($Obj | ConvertTo-Json -Depth 12) + "`n"
    return $json.Replace('\u003c', '<').Replace('\u003e', '>').Replace('\u0026', '&')
}

# 1) Chromium Store ZIP (Microsoft Edge Add-ons / Chrome Web Store): remove "key"
$ChromiumStaging = New-CleanExtensionStaging "chromium_store"
$ChromiumManifest = $ManifestRaw | ConvertFrom-Json
$ChromiumManifest.PSObject.Properties.Remove("key")
$ChromiumJson = Format-CleanJson $ChromiumManifest
[System.IO.File]::WriteAllText((Join-Path $ChromiumStaging "manifest.json"), $ChromiumJson, $Utf8NoBom)

$ChromiumZip = Join-Path $OutputDir "VortexDL-Extension-Edge-Chrome-v$ExtVersion.zip"
Write-ZipFromStaging $ChromiumStaging $ChromiumZip

# 2) Firefox AMO ZIP (Self-distributed / Unlisted): remove "key", use background.scripts, add browser_specific_settings.gecko
$FirefoxStaging = New-CleanExtensionStaging "firefox_amo"
$FirefoxManifest = $ManifestRaw | ConvertFrom-Json
$FirefoxManifest.PSObject.Properties.Remove("key")
$FirefoxManifest.background = [ordered]@{
    scripts = @("background.js")
}
$GeckoSettings = [ordered]@{
    gecko = [ordered]@{
        id = "vortexdl@igniz608.dev"
        strict_min_version = "140.0"
        data_collection_permissions = [ordered]@{
            required = @("none")
        }
    }
}
if ($FirefoxManifest.PSObject.Properties["browser_specific_settings"]) {
    $FirefoxManifest.browser_specific_settings = $GeckoSettings
} else {
    $FirefoxManifest | Add-Member -NotePropertyName "browser_specific_settings" -NotePropertyValue $GeckoSettings
}
$FirefoxJson = Format-CleanJson $FirefoxManifest
[System.IO.File]::WriteAllText((Join-Path $FirefoxStaging "manifest.json"), $FirefoxJson, $Utf8NoBom)

$FirefoxZip = Join-Path $OutputDir "VortexDL-Extension-Firefox-v$ExtVersion.zip"
Write-ZipFromStaging $FirefoxStaging $FirefoxZip

Write-Host "Store extension packages ready:" -ForegroundColor Green
Get-Item $ChromiumZip, $FirefoxZip | ForEach-Object {
    $kb = [math]::Round($_.Length / 1KB, 1)
    Write-Host ("  - {0} ({1} KB)" -f $_.FullName, $kb)
}
