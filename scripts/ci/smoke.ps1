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
$originalLocation = (Get-Location).Path
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
    # `inka build` confines the source to the current working directory, so run
    # from inside the app dir (mirrors the Linux smoke.sh).
    Set-Location -LiteralPath $apps
    $out = & $inka run simple.js
    if ($out -notmatch 'smoke-run') { Fail "run output missing: $out" }

    Info 'inka build + run artifact'
    Set-Content -Path (Join-Path $apps 'artifact.js') -Value 'console.log("smoke-artifact");'
    & $inka build artifact.js
    if ($LASTEXITCODE -ne 0) { Fail "build exited $LASTEXITCODE" }
    $artifact = Join-Path $apps 'artifact.exe'
    if (-not (Test-Path -LiteralPath $artifact)) { Fail "build did not produce $artifact" }
    $out = & $artifact
    if ($out -notmatch 'smoke-artifact') { Fail "artifact output missing: $out" }

    Info 'inka desktop packaging (laufey from the staged mirror)'
    # Seed the laufey cache from the mirrored Windows archive so packaging does
    # not reach laufey's host; `versions.json` carries the version/archive name.
    $doc = Get-Content -LiteralPath (Join-Path $Stage 'versions.json') -Raw | ConvertFrom-Json
    $winLaufey = $doc.targets.'x86_64-pc-windows-msvc'.laufey
    $backendDir = Join-Path $scratch (Join-Path $winLaufey.version `
        (Join-Path 'webview' 'x86_64-pc-windows-msvc'))
    New-Item -ItemType Directory -Force -Path $backendDir | Out-Null
    Expand-Archive -LiteralPath (Join-Path $Stage $winLaufey.backends.webview.archive) `
        -DestinationPath $backendDir -Force
    Set-Content -LiteralPath (Join-Path $backendDir '.downloaded') -Value "v$($winLaufey.version)"
    $env:INKA_LAUFEY_CACHE = $scratch
    $deskDist = Join-Path $apps 'smoke-desktop'
    & $inka desktop simple.js --backend webview --name SmokeApp --installer `
        --deep-link smoketest -o $deskDist
    if ($LASTEXITCODE -ne 0) { Fail "desktop exited $LASTEXITCODE" }
    foreach ($f in @('SmokeApp.exe', 'SmokeApp.dll', 'runtime-version', 'register-deep-links.bat')) {
        if (-not (Test-Path -LiteralPath (Join-Path $deskDist $f))) {
            Fail "desktop packaging missing $f"
        }
    }
    if (-not (Test-Path -LiteralPath "$deskDist.zip")) { Fail 'desktop packaging missing the .zip' }
    # Validate the .msi with a real msiexec administrative install (exercises
    # the string-pool table ordering that msiexec enforces as error 2219).
    if (-not (Test-Path -LiteralPath "$deskDist.msi")) { Fail 'desktop packaging missing the .msi' }
    $admin = Join-Path $scratch 'msi-admin'
    $p = Start-Process -Wait -PassThru -NoNewWindow msiexec.exe -ArgumentList `
        '/a', "$deskDist.msi", '/qn', "TARGETDIR=$admin"
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { Fail "msiexec /a failed: $($p.ExitCode)" }
    if (-not (Get-ChildItem -Path $admin -Recurse -Filter 'SmokeApp.exe' -ErrorAction SilentlyContinue)) {
        Fail 'msi administrative install produced no SmokeApp.exe'
    }

    Info 'inka desktop self-extracting packaging'
    $smallDist = Join-Path $apps 'smoke-small'
    & $inka desktop simple.js --backend webview --name SmokeSmall --compress -o $smallDist
    if ($LASTEXITCODE -ne 0) { Fail "desktop --compress exited $LASTEXITCODE" }
    if (-not (Test-Path -LiteralPath (Join-Path $smallDist 'payload.tar.gz'))) {
        Fail 'self-extracting packaging missing payload.tar.gz'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $smallDist 'SmokeSmall.bat'))) {
        Fail 'self-extracting packaging missing SmokeSmall.bat'
    }
    $listing = (tar -tzf (Join-Path $smallDist 'payload.tar.gz')) -join "`n"
    if ($listing -notmatch 'SmokeSmall/SmokeSmall.exe') {
        Fail 'self-extracting payload does not contain the app'
    }

    Info 'inka update is current (runtime)'
    $out = (& $inka update --from $Stage --no-toolchain) -join "`n"
    if ($out -notmatch 'is current' -and $out -notmatch 'up to date') {
        Fail "update did not report current:`n$out"
    }

    Write-Host 'smoke: OK' -ForegroundColor Green
} finally {
    # Leave the scratch dir before deleting it (Windows can't remove the cwd).
    Set-Location -LiteralPath $originalLocation
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
