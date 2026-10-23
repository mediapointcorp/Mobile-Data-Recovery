<#
.SYNOPSIS
  Builds PhotoRecovery (Rust worker + Electron app), runs tests, and
  optionally produces the NSIS installer.
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
  powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -SkipTests -Installer
#>
[CmdletBinding()]
param(
    [switch]$SkipTests,
    [switch]$Installer
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Run([string]$Exe, [string[]]$Arguments) {
    Write-Host "> $Exe $($Arguments -join ' ')" -ForegroundColor Cyan
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Exe failed with exit code $LASTEXITCODE" }
}

Run cargo @('build', '--release', '--workspace', '--locked')
if (-not $SkipTests) {
    Run cargo @('test', '--workspace', '--locked')
}

Push-Location app
try {
    Run npm @('ci', '--no-audit', '--no-fund')
    Run npm @('run', 'typecheck')
    if (-not $SkipTests) { Run npm @('test') }
    Run npm @('run', 'build')
    if ($Installer) {
        Run npx @('electron-builder', '--win', 'nsis', '--x64')
        Write-Host "Installer written to $root\app\release" -ForegroundColor Green
    }
} finally {
    Pop-Location
}

Write-Host ''
Write-Host "Worker: $root\target\release\photorecovery-worker.exe" -ForegroundColor Green
Write-Host 'Run the app: cd app; npm start'
