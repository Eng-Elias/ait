# §5.11 "verified install" gate — Windows leg (Spec R16 us-37,
# ticket R16-T9); the PowerShell twin of scripts/verify-install.sh.
#
#   scripts/verify-install.ps1 <tag>
#
# Downloads the release's ait-installer.ps1, runs it under
# powershell.exe — the Windows PowerShell 5.1 host a stock machine
# ships, where a user's `irm … | iex` lands — and asserts
# `ait.exe --version` prints exactly `ait <v>` where <v> is the tag
# minus a leading `v`. The ait-gui twin follows on the same contract.
# Exits non-zero naming what it got vs what it expected on any
# mismatch. Backs .github/workflows/verify-install.yml's
# `verify-windows` leg and is the maintainer's local check on a
# Windows box.
#
# Env (mirrors verify-install.sh; the generated installer honors the
# same names — verified against target/distrib/ait-installer.ps1):
#   AIT_RELEASE_BASE_URL  where ait-installer.ps1 is fetched from;
#                         default
#                         https://github.com/Eng-Elias/ait/releases/download/<tag>
#   AIT_DOWNLOAD_URL      installer env var — overrides the artifact
#                         base URL baked into ait-installer.ps1. Set
#                         both to a locally served target/distrib/ to
#                         exercise a locally built release end to end.
#   AIT_INSTALL_DIR       installer env var — install PREFIX override;
#                         the binary lands in <dir>\bin (cargo-home
#                         layout) and the version check mirrors that.
#   AIT_GITHUB_TOKEN, AIT_NO_MODIFY_PATH, INSTALLER_*   inherited by
#                         the installer child process unchanged.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Tag
)

$ErrorActionPreference = 'Stop'

$version = $Tag -replace '^v', ''
$env:AIT_EXPECTED_VERSION = "ait $version"
$env:AIT_EXPECTED_GUI_VERSION = "ait-gui $version"
if (-not $env:AIT_RELEASE_BASE_URL) {
    $env:AIT_RELEASE_BASE_URL =
        "https://github.com/Eng-Elias/ait/releases/download/$Tag"
}

# Mirror the installer's own dir resolution (install-path CARGO_HOME):
# a forced AIT_INSTALL_DIR is a prefix — the binary lands in <dir>\bin.
function Resolve-InstallDir {
    if ($env:AIT_INSTALL_DIR) {
        Join-Path $env:AIT_INSTALL_DIR 'bin'
    } elseif ($env:CARGO_HOME) {
        Join-Path $env:CARGO_HOME 'bin'
    } else {
        Join-Path $HOME '.cargo\bin'
    }
}

# Fetch + run one installer under the stock 5.1 host, then assert the
# installed binary's --version.
function Assert-Installed([string]$Name, [string]$Expected, [string]$Dir) {
    $installer = Join-Path $work.FullName "$Name-installer.ps1"
    Invoke-WebRequest -UseBasicParsing `
        -Uri "$env:AIT_RELEASE_BASE_URL/$Name-installer.ps1" `
        -OutFile $installer
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $installer
    if ($LASTEXITCODE -ne 0) {
        throw "verify-install: $Name-installer.ps1 exited $LASTEXITCODE"
    }
    $exe = Join-Path $Dir "$Name.exe"
    $got = & $exe --version
    if ($LASTEXITCODE -ne 0 -or -not $got) {
        throw "verify-install: '$exe --version' failed to run"
    }
    if ($got -ne $Expected) {
        throw "verify-install: expected '$Expected', got '$got'"
    }
    Write-Output "verify-install: $got — install verified ($exe)"
}

$work = New-Item -ItemType Directory -Force -Path (
    Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName()))
try {
    $installDir = Resolve-InstallDir
    Assert-Installed 'ait' $env:AIT_EXPECTED_VERSION $installDir
    Assert-Installed 'ait-gui' $env:AIT_EXPECTED_GUI_VERSION $installDir
} finally {
    Remove-Item -Recurse -Force $work.FullName
}
