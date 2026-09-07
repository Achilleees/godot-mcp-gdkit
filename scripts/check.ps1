#Requires -Version 7.0
# Run from any directory. A release check must exercise the installed Godot engine.
[CmdletBinding()]
param(
    [string]$GodotBin,
    [switch]$Install
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$oldGodotBin = $env:GODOT_BIN
$oldRequireGodot = $env:GDKIT_REQUIRE_GODOT

function Invoke-Cargo {
    param([string[]]$CargoArgs)
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($CargoArgs -join ' ') failed (exit $LASTEXITCODE)"
    }
}

if ($GodotBin) {
    $GodotBin = (Resolve-Path -LiteralPath $GodotBin).Path
    if (-not (Test-Path -LiteralPath $GodotBin -PathType Leaf)) {
        throw 'GodotBin must name a Godot executable.'
    }
}

Push-Location (Join-Path $repoRoot 'server')
try {
    if ($GodotBin) {
        $env:GODOT_BIN = $GodotBin
    }
    $env:GDKIT_REQUIRE_GODOT = '1'
    Invoke-Cargo -CargoArgs @('fmt', '--all', '--', '--check')
    Invoke-Cargo -CargoArgs @('clippy', '--locked', '--all-targets', '--', '-D', 'warnings')
    Invoke-Cargo -CargoArgs @('test', '--locked')
    $buildOutput = & cargo build --locked --release --message-format=json
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --locked --release failed.' }

    if ($Install) {
        if (-not $IsWindows) { throw 'Installing the plugin binary currently requires Windows.' }
        # Use the artifact Cargo actually built, including custom target directories/triples.
        $artifacts = @($buildOutput | ConvertFrom-Json | Where-Object {
            $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'godot-mcp' -and $_.executable
        })
        if ($artifacts.Count -ne 1) { throw 'Expected one godot-mcp executable from Cargo.' }
        $binary = $artifacts[0].executable
        if ([System.IO.Path]::GetExtension($binary) -ne '.exe') {
            throw 'The plugin requires a Windows executable; check the configured Cargo target.'
        }
        Copy-Item -LiteralPath $binary -Destination (Join-Path $repoRoot 'bin/godot-mcp.exe')
        Write-Host 'Validated release binary installed into bin/godot-mcp.exe.'
    }
} finally {
    $env:GODOT_BIN = $oldGodotBin
    $env:GDKIT_REQUIRE_GODOT = $oldRequireGodot
    Pop-Location
}
