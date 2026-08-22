# chat-cli installer for Windows
# PowerShell: irm https://raw.githubusercontent.com/quangdang46/chat-cli/main/install.ps1 | iex
[CmdletBinding()]
param(
    [string]$Dest = (Join-Path $env:USERPROFILE ".local\bin"),
    [string]$Version = "",
    [switch]$System,
    [switch]$Easy,
    [switch]$Verify,
    [switch]$FromSource,
    [switch]$Uninstall,
    [switch]$Quiet
)

$ErrorActionPreference = "Stop"
$BinaryName = "chat-cli"
$ExeName = "chat-cli.exe"
$Owner = "quangdang46"
$Repo = "chat-cli"
if ($System) { $Dest = "$env:SystemRoot\System32" } # rarely writable; prefer -Dest

function Write-Info { if (-not $Quiet) { Write-Host "[$BinaryName] $args" } }
function Write-Warn2 { Write-Host "[$BinaryName] WARN: $args" -ForegroundColor Yellow }
function Die { Write-Host "ERROR: $args" -ForegroundColor Red; exit 1 }

# --- Uninstall ---
if ($Uninstall) {
    Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $Dest $ExeName)
    Write-Host "✓ $BinaryName uninstalled from $Dest"
    exit 0
}

# --- Platform (os_arch split into exactly 2 parts) ---
$platform = "windows_x86_64"
$suffix = "windows-x86_64"

# --- Version ---
function Resolve-Version {
    if ($Version) { return $Version }
    try {
        $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Owner/$Repo/releases/latest" `
            -Headers @{ Accept = "application/vnd.github.v3+json" } `
            -TimeoutSec 30
        if ($rel.tag_name) { return $rel.tag_name }
    } catch {}
    return ""
}

# --- Build from source ---
function Build-FromSource {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Die "cargo not found — install Rust: https://rustup.rs"
    }
    $src = Join-Path ([System.IO.Path]::GetTempPath()) "$BinaryName-src-$(Get-Random)"
    git clone --depth 1 "https://github.com/$Owner/$Repo.git" $src | Out-Null
    Push-Location $src
    try {
        cargo build --release -p chat-cli
    } finally { Pop-Location }
    return (Join-Path $src "target\release\$ExeName")
}

# --- Main ---
New-Item -ItemType Directory -Force -Path $Dest | Out-Null

$binPath = Join-Path $Dest $ExeName

if ($FromSource) {
    $built = Build-FromSource
    Copy-Item -Force $built $binPath
} else {
    $tag = Resolve-Version
    if (-not $tag) { Die "Could not resolve latest release — pass -Version vX.Y.Z" }
    Write-Info "Latest release: $tag"

    $archive = "$BinaryName-$tag-$suffix.zip"
    $url = "https://github.com/$Owner/$Repo/releases/download/$tag/$archive"
    $tmpZip = Join-Path ([System.IO.Path]::GetTempPath()) $archive
    $tmpHash = "$tmpZip.sha256"

    try {
        Invoke-WebRequest -Uri $url -OutFile $tmpZip -TimeoutSec 300
        $downloaded = $true
    } catch {
        Write-Warn2 "Download failed: $_"
        $downloaded = $false
    }

    if ($downloaded) {
        # checksum sidecar (best-effort)
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile $tmpHash -TimeoutSec 60
            $expected = (Get-Content $tmpHash | ForEach-Object { ($_ -split "\s+")[0] }).Trim().ToLower()
            $actual = (Get-FileHash $tmpZip -Algorithm SHA256).Hash.ToLower()
            if ($expected -ne $actual) { Die "Checksum mismatch for $archive" }
            Write-Info "Checksum verified"
        } catch {
            Write-Warn2 "Checksum sidecar unavailable — skipping verification"
        }

        $extractDir = Join-Path ([System.IO.Path]::GetTempPath()) "$BinaryName-extract-$(Get-Random)"
        Expand-Archive -Force -Path $tmpZip -DestinationPath $extractDir
        $bin = Get-ChildItem -Recurse $extractDir -Filter $ExeName | Select-Object -First 1
        if (-not $bin) { Die "Binary not found after extract" }
        Copy-Item -Force $bin.FullName $binPath
    } else {
        Write-Warn2 "Falling back to source build..."
        $built = Build-FromSource
        Copy-Item -Force $built $binPath
    }
}

# --- PATH ---
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $Dest) {
    if ($Easy) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$Dest", "User")
        Write-Warn2 "PATH updated — restart your terminal"
    } else {
        Write-Warn2 "Add to PATH manually: $Dest"
    }
}

if ($Verify) {
    & $binPath --help | Select-Object -First 3
}

Write-Host ""
Write-Host "✓ $BinaryName installed → $binPath"
Write-Host ""
Write-Host "  Quick start:"
Write-Host "    $BinaryName auth login deepseek --token <TOKEN>"
Write-Host "    $BinaryName -p `"hi`" --provider deepseek"
Write-Host "    $BinaryName --help"
