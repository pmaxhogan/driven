<#
.SYNOPSIS
    Runs the Driven vs rclone benchmark suite locally on Windows.

.DESCRIPTION
    A thin front end over `cargo run -p driven-bench -- run`. It checks the two
    things that most often go wrong before a long run starts - a missing rclone
    binary and missing credentials - so you find out in seconds rather than
    after the fixtures have been generated.

    Credentials come from the gitignored .env.test at the repo root, which the
    harness loads itself; this script only reports whether it is there.

    See bench/README.md for scales, costs and how to read the results.

.PARAMETER Scale
    Fixture size: smoke, small (default), medium or full.

.PARAMETER Tools
    Comma-separated tools to measure. Defaults to "driven,rclone".

.PARAMETER Shape
    Restrict to one fixture shape: huge or tiny-deep.

.PARAMETER Rclone
    Path to the rclone binary, when it is not on PATH.

.PARAMETER Full
    Lift the 2 GiB upload cap. Required for -Scale full.

.PARAMETER KeepRemote
    Leave the uploaded run folder in Drive instead of trashing it.

.EXAMPLE
    .\bench\run.ps1 -Scale smoke

.EXAMPLE
    .\bench\run.ps1 -Scale full -Full
#>
[CmdletBinding()]
param(
    [ValidateSet("smoke", "small", "medium", "full")]
    [string]$Scale = "small",

    [string]$Tools = "driven,rclone",

    [ValidateSet("huge", "tiny-deep")]
    [string]$Shape,

    [string]$Rclone,

    [switch]$Full,

    [switch]$KeepRemote
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot

# --- preflight ------------------------------------------------------------
# Both checks are advisory: the harness enforces them properly. They exist so a
# long run fails in the first second rather than after fixture generation.

if ($Tools -split "," -contains "rclone") {
    $rclonePath = if ($Rclone) { $Rclone } else { (Get-Command rclone -ErrorAction SilentlyContinue).Source }
    if (-not $rclonePath) {
        Write-Error @"
rclone was not found on PATH.
Install it with 'choco install rclone', or unzip the official build from
https://rclone.org/downloads/ and pass -Rclone <path to rclone.exe>.
"@
    }
    Write-Host "rclone:      $rclonePath"
}

$envFile = Join-Path $repoRoot ".env.test"
if (Test-Path $envFile) {
    Write-Host "credentials: $envFile"
} elseif ($env:DRIVEN_E2E_REFRESH_TOKEN) {
    Write-Host "credentials: from the environment"
} else {
    Write-Error @"
No credentials found: neither $envFile nor DRIVEN_E2E_REFRESH_TOKEN is present.
See bench/README.md, 'Prerequisites'.
"@
}

if ($Scale -eq "full" -and -not $Full) {
    Write-Warning "-Scale full uploads ~10 GB per tool and needs -Full to clear the upload cap."
}

# --- run ------------------------------------------------------------------

$benchArgs = @("run", "--scale", $Scale, "--tools", $Tools)
if ($Shape) { $benchArgs += @("--shape", $Shape) }
if ($Rclone) { $benchArgs += @("--rclone", $Rclone) }
if ($Full) { $benchArgs += "--full" }
if ($KeepRemote) { $benchArgs += "--keep-remote" }

Write-Host "scale:       $Scale"
Write-Host "tools:       $Tools"
Write-Host ""

Push-Location $repoRoot
try {
    # --release matters: a dev-profile build spends its time in Driven's hashing
    # and encryption paths rather than measuring them.
    & cargo run --release -p driven-bench -- @benchArgs
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
