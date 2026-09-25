<#
.SYNOPSIS
  Smoke-test a staged inka Windows release before it is published.

.DESCRIPTION
  Expects a staging directory laid out like a release root:
    install.ps1, versions.json
    inka-toolchain-<rel>-x86_64-pc-windows-msvc.zip   (+ .sha256)
    libinka_runtime-<runtime>.dll                     (+ .sha256)

  Installs through install.ps1 into a throwaway prefix/runtime home, then
  exercises core `inka`. Exits non-zero on any failure.

.PARAMETER Stage
  Path to the staging directory.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Stage
)

$ErrorActionPreference = 'Stop'
$Stage = (Resolve-Path -LiteralPath $Stage).Path

function Info { param($m) Write-Host "== $m ==" }
function Fail { param($m) Write-Host "smoke: $m" -ForegroundColor Red; exit 1 }

if (-not (Test-Path -LiteralPath (Join-Path $Stage 'install.ps1'))) { Fail "no install.ps1 in $Stage" }
$runtimeDll = Get-ChildItem -LiteralPath $Stage -Filter 'libinka_runtime-*.dll' | Select-Object -First 1
if (-not $runtimeDll) { Fail "no libinka_runtime-*.dll in $Stage" }
$runtime = $runtimeDll.BaseName -replace '^libinka_runtime-', ''

# A prerelease engine tuple is only selected in the beta channel.
if ($runtime -match '-(beta|rc)\.') { $env:INKA_CHANNEL = 'beta' }

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("inka-smoke-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $scratch | Out-Null
$env:INKA_RUNTIME_HOME = Join-Path $scratch 'runtime'
$prefix = Join-Path $scratch 'prefix'
$inka = Join-Path $prefix 'bin\inka.exe'
try {
    Info 'install via install.ps1 (toolchain + engine)'
    & (Join-Path $Stage 'install.ps1') -From $Stage -Prefix $prefix -NoModifyPath
    if ($LASTEXITCODE -ne 0) { Fail "install.ps1 exited $LASTEXITCODE" }
    if (-not (Test-Path -LiteralPath $inka)) { Fail 'install did not produce inka.exe' }

    Info 'inka --version'
    $ver = & $inka --version
    if ($ver -notmatch 'inka') { Fail "unexpected --version output: $ver" }

    Info 'inka doctor'
    & $inka doctor
    if ($LASTEXITCODE -ne 0) { Fail "doctor exited $LASTEXITCODE" }

    Info 'inka run'
    $apps = Join-Path $scratch 'apps'
    New-Item -ItemType Directory -Force -Path $apps | Out-Null
    Set-Content -Path (Join-Path $apps 'simple.js') -Value 'console.log("smoke-run");'
    $out = & $inka run (Join-Path $apps 'simple.js')
    if ($out -notmatch 'smoke-run') { Fail "run output missing: $out" }

    Info 'inka build + run artifact'
    Set-Content -Path (Join-Path $apps 'artifact.js') -Value 'console.log("smoke-artifact");'
    & $inka build (Join-Path $apps 'artifact.js')
    if ($LASTEXITCODE -ne 0) { Fail "build exited $LASTEXITCODE" }
    $artifact = Join-Path $apps 'artifact.exe'
    if (-not (Test-Path -LiteralPath $artifact)) { Fail "build did not produce $artifact" }
    $out = & $artifact
    if ($out -notmatch 'smoke-artifact') { Fail "artifact output missing: $out" }

    Info 'inka update is current (runtime)'
    $out = (& $inka update --from $Stage --no-toolchain) -join "`n"
    if ($out -notmatch 'is current' -and $out -notmatch 'up to date') {
        Fail "update did not report current:`n$out"
    }

    Write-Host 'smoke: OK' -ForegroundColor Green
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
