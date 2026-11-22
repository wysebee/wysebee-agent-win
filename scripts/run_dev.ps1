<#
.SYNOPSIS
    Run Wysebee Agent from source, against a Wysebee Cloud of your choice, for development.

.DESCRIPTION
    scripts\run_dev.ps1                    # Wysebee Cloud: https://app.wysebee.com
    scripts\run_dev.ps1 -Url https://other.example   # any other cloud
    scripts\run_dev.ps1 -Fresh             # forget the dev state first (unpaired, no folders)
    scripts\run_dev.ps1 -Release           # a release build
    scripts\run_dev.ps1 -Headless "Dev agent"   # no window: the headless example

    Each cloud keeps its own state folder, apart from an installed copy:
        %LOCALAPPDATA%\Wysebee Agent Dev\<prod|custom>\
    ($env:WYSEBEE_AGENT_STATE_DIR overrides it). One instance runs per state folder, so this
    can run beside the installed app. A debug build has a console: the log goes there.

    The Credential Manager item follows the state folder: the installed app's is account
    link-secret, each folder here has its own (link-secret <hash of the folder>), so pairing
    here leaves the installed copy paired.
#>
param(
    [string]$Url = '',
    [switch]$Fresh,
    [switch]$Release,
    [string]$Headless = ''
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'common.ps1')
Assert-Windows 'run_dev.ps1'
Assert-Cargo

$EnvName = 'prod'
if ($Url) {
    $EnvName = 'custom'
} else {
    $Url = 'https://app.wysebee.com'
}

$StateDir = if ($env:WYSEBEE_AGENT_STATE_DIR) { $env:WYSEBEE_AGENT_STATE_DIR } else { Join-Path $env:LOCALAPPDATA "Wysebee Agent Dev\$EnvName" }

if ($Fresh -and (Test-Path $StateDir)) {
    Write-Host "Removing dev state: $StateDir"
    Remove-Item -Recurse -Force -LiteralPath $StateDir
}
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null

$env:WYSEBEE_CLOUD_URL = $Url
$env:WYSEBEE_AGENT_STATE_DIR = $StateDir
Write-Host "Cloud:  $env:WYSEBEE_CLOUD_URL (the default; an address saved in the Server tab wins)"
Write-Host "State:  $env:WYSEBEE_AGENT_STATE_DIR"

$CargoProfile = @()
if ($Release) { $CargoProfile = @('--release') }

Push-Location $RepoRoot
try {
    if ($Headless) {
        Invoke-Native cargo run @CargoProfile -p agent-core --example headless '--' $StateDir $Headless
    } else {
        # The window (ui\) is compiled into the app. Cargo notices an edited ui file by itself,
        # but not a new one; marking main.rs as changed (its time only, not its contents)
        # recompiles the agent-app crate alone, which reads every file in ui\ again.
        (Get-Item (Join-Path $RepoRoot 'crates\agent-app\src\main.rs')).LastWriteTime = Get-Date
        Write-Host 'UI:     rebuilt from ui\'
        Invoke-Native cargo run @CargoProfile -p agent-app
    }
} finally {
    Pop-Location
}
