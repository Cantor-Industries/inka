<#
.SYNOPSIS
  inka bootstrap installer (Windows).

.DESCRIPTION
  Downloads the inka toolchain (CLI + launcher + desktop shim), installs it
  under %LOCALAPPDATA%\inka\bin, adds that to the user PATH, then provisions the
  shared runtime via `inka update`. Per-user; no administrator rights needed.

  Mirrors install.sh: the toolchain archive is verified against versions.json
  (or a `.sha256` sidecar) before extraction.

.PARAMETER Version
  Install a specific release tag (e.g. v0.8.1-beta.10-abcdef0).

.PARAMETER Beta
  Install the newest beta (prerelease) release.

.PARAMETER From
  Release base override: a URL or a local directory (mirrors, CI staging).

.PARAMETER Prefix
  Install prefix (default: %LOCALAPPDATA%\inka).

.PARAMETER NoModifyPath
  Do not add the install's bin directory to the user PATH.

.PARAMETER NoRuntime
  Skip provisioning the shared runtime.

.PARAMETER Force
  Reinstall the toolchain even if the recorded version is current.

.PARAMETER Uninstall
  Remove the install directory and its PATH entry, then exit.

.EXAMPLE
  irm https://github.com/Cantor-Industries/inka/releases/latest/download/install.ps1 -OutFile install.ps1
  .\install.ps1

.EXAMPLE
  .\install.ps1 -Beta
#>
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$Beta,
    [string]$From,
    [string]$Prefix,
    [switch]$NoModifyPath,
    [Alias('NoEngine')]
    [switch]$NoRuntime,
    [switch]$Force,
    [switch]$Uninstall,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 defaults to TLS 1.0; GitHub requires 1.2+.
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

$Repo = if ($env:INKA_REPO) { $env:INKA_REPO } else { 'Cantor-Industries/inka' }
$DefaultBase = "https://github.com/$Repo/releases/latest/download"
$Target = 'x86_64-pc-windows-msvc'
if (-not $Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA 'inka' }
$Bin = Join-Path $Prefix 'bin'

function Info { param($m) Write-Host "info: $m" -ForegroundColor Cyan }
function Warn { param($m) Write-Host "warning: $m" -ForegroundColor Yellow }
function Die { param($m) Write-Host "error: $m" -ForegroundColor Red; exit 1 }

function Usage {
    @'
inka installer (Windows)

usage: install.ps1 [options]

options:
  -Version <tag>   install a specific release tag (default: latest)
  -Beta            install the newest beta release
  -From <dir|url>  release base override (mirrors, local staging)
  -Prefix <dir>    install prefix (default: %LOCALAPPDATA%\inka)
  -NoModifyPath    do not add the install's bin dir to the user PATH
  -NoRuntime       skip the shared runtime
  -Force           reinstall the toolchain even if current
  -Uninstall       remove the toolchain (and runtime) and exit
  -Help            show this help
'@
}

function Get-UserPath { [Environment]::GetEnvironmentVariable('Path', 'User') }

function Add-ToUserPath {
    param($Dir)
    $cur = Get-UserPath
    $parts = @()
    if ($cur) { $parts = @($cur -split ';' | Where-Object { $_ -ne '' }) }
    if ($parts -notcontains $Dir) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $Dir) -join ';'), 'User')
        if (-not $NoModifyPath) { Info "added $Dir to the user PATH" }
    }
    if (($env:Path -split ';') -notcontains $Dir) { $env:Path = "$env:Path;$Dir" }
}

function Remove-FromUserPath {
    param($Dir)
    $cur = Get-UserPath
    if (-not $cur) { return }
    $parts = @($cur -split ';' | Where-Object { $_ -ne '' -and $_ -ne $Dir })
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

function Get-Text {
    param($Base, $Name)
    if ($Base -match '^https?://') {
        (Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Name").Content
    } else {
        Get-Content (Join-Path $Base $Name) -Raw
    }
}

function Get-Remote {
    param($Base, $Name, $Dest)
    if ($Base -match '^https?://') {
        Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Name" -OutFile $Dest
    } else {
        $src = Join-Path $Base $Name
        if (-not (Test-Path -LiteralPath $src)) { Die "not found: $src" }
        Copy-Item -LiteralPath $src -Destination $Dest -Force
    }
}

function Resolve-BetaBase {
    $api = "https://api.github.com/repos/$Repo/releases?per_page=100"
    $rels = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'inka-install' }
    foreach ($r in $rels) {
        if ($r.prerelease) {
            $b = "https://github.com/$Repo/releases/download/$($r.tag_name)"
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "$b/versions.json" | Out-Null
                return $b
            } catch {}
        }
    }
    Die "could not resolve a beta release for $Repo"
}

if ($Help) { Usage; exit 0 }

if ($Uninstall) {
    if (Test-Path -LiteralPath $Prefix) {
        Remove-Item -LiteralPath $Prefix -Recurse -Force
        Info "removed $Prefix"
    } else {
        Info "$Prefix is not installed"
    }
    if (-not $NoModifyPath) { Remove-FromUserPath $Bin }
    exit 0
}

if ($From) { $Base = $From }
elseif ($Version) { $Base = "https://github.com/$Repo/releases/download/$Version" }
elseif ($Beta) { $Base = Resolve-BetaBase }
else { $Base = $DefaultBase }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("inka-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    $versions = (Get-Text $Base 'versions.json') | ConvertFrom-Json
    # Per-target schema: targets[<triple>]; fall back to the legacy top-level.
    $view = $versions
    if ($versions.PSObject.Properties['targets']) {
        $t = $versions.targets.PSObject.Properties[$Target]
        if ($t) { $view = $t.Value }
    }
    $tc = $view.toolchain
    $rel = if ($tc -and $tc.version) { $tc.version } elseif ($versions.release) { $versions.release } else { $Version }
    if (-not $rel) { Die "versions.json has no toolchain version" }
    $archive = if ($tc -and $tc.archive) { $tc.archive } else { "inka-toolchain-$rel-$Target.zip" }
    # The archive name comes from (possibly untrusted) metadata: require a plain
    # basename so it cannot escape the staging dir.
    if ($archive -match '[\\/]' -or $archive -eq '.' -or $archive -eq '..') {
        Die "versions.json names an invalid toolchain archive '$archive'"
    }

    $current = ''
    $verFile = Join-Path $Bin 'VERSION'
    if (Test-Path -LiteralPath $verFile) { $current = (Get-Content -LiteralPath $verFile -Raw).Trim() }

    if (-not $Force -and $current -eq $rel -and (Test-Path -LiteralPath (Join-Path $Bin 'inka.exe'))) {
        Info "inka toolchain $rel is current"
    } else {
        Info "installing inka toolchain $rel"
        $archivePath = Join-Path $tmp $archive
        Get-Remote $Base $archive $archivePath

        $expected = if ($tc -and $tc.sha256) { $tc.sha256 } else { $null }
        if (-not $expected) {
            try { $expected = ((Get-Text $Base "$archive.sha256").Trim() -split '\s+')[0] } catch { $expected = $null }
        }
        $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLower()
        if ($expected) {
            if ($actual -ne $expected.ToLower()) {
                Die "checksum mismatch for $archive (expected $expected, got $actual)"
            }
        } else {
            Warn "no checksum for $archive; skipping verification"
        }

        New-Item -ItemType Directory -Force -Path $Bin | Out-Null
        Expand-Archive -LiteralPath $archivePath -DestinationPath $Bin -Force
        Set-Content -LiteralPath $verFile -Value $rel
    }

    if (-not $NoModifyPath) { Add-ToUserPath $Bin }

    if (-not $NoRuntime) {
        Info "installing runtime from $Base"
        & (Join-Path $Bin 'inka.exe') update --from $Base --no-toolchain
        if ($LASTEXITCODE -ne 0) { Die "inka update failed (exit $LASTEXITCODE)" }
    }

    Info "inka $rel installed at $(Join-Path $Bin 'inka.exe')"
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
