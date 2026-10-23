<#
.SYNOPSIS
  Installs what is needed to build PhotoRecovery on Windows 10/11 (winget).
.DESCRIPTION
  Checks for, and after confirmation installs:
    - Visual Studio 2022 Build Tools (C++ workload)
    - Rust (rustup, stable MSVC toolchain)
    - Node.js 22 LTS
  Nothing is installed without asking. Safe to re-run.
#>
[CmdletBinding()]
param(
    [switch]$Yes
)
$ErrorActionPreference = 'Stop'

function Confirm-Step([string]$What) {
    if ($Yes) { return $true }
    $a = Read-Host "Install $What now? [y/N]"
    return $a -match '^(y|yes)$'
}

function Have([string]$Cmd) {
    return [bool](Get-Command $Cmd -ErrorAction SilentlyContinue)
}

if (-not (Have 'winget')) {
    Write-Warning 'winget not found. Install "App Installer" from the Microsoft Store, or install the prerequisites manually (docs/WINDOWS_BUILD.md).'
    exit 1
}

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasMsvc = (Test-Path $vswhere) -and (& $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
if ($hasMsvc) {
    Write-Host 'MSVC build tools: found'
} elseif (Confirm-Step 'Visual Studio 2022 Build Tools (C++)') {
    winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-package-agreements --accept-source-agreements `
        --override '--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
}

if (Have 'rustup') {
    Write-Host 'rustup: found'
    rustup default stable-x86_64-pc-windows-msvc | Out-Null
} elseif (Confirm-Step 'Rust (rustup)') {
    winget install --id Rustlang.Rustup -e --accept-package-agreements --accept-source-agreements
    Write-Host 'Open a new terminal after installation so cargo is on PATH.'
}

if (Have 'node') {
    $v = (node --version)
    Write-Host "node: $v"
    if (-not ($v -match '^v(2[2-9]|[3-9]\d)\.')) { Write-Warning 'Node.js 22 LTS or newer is recommended.' }
} elseif (Confirm-Step 'Node.js 22 LTS') {
    winget install --id OpenJS.NodeJS.LTS -e --accept-package-agreements --accept-source-agreements
}

Write-Host ''
Write-Host 'Done. Next: powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer'
