<#
.SYNOPSIS
  Installs media_downloader's native-messaging host for the current user (no admin needed).

.DESCRIPTION
  1. Builds release binaries (skip with -NoBuild).
  2. Copies mdl-daemon.exe, mdl-shim.exe, mdl-cli.exe to %LOCALAPPDATA%\mdl\bin.
  3. Writes the host manifest com.mdl.host.json, allowing ONLY our extension ID.
  4. Registers it for Chrome and Edge under HKCU.
  5. Optional -Autostart: start mdl-daemon at logon (otherwise the shim starts it on demand).

.EXAMPLE
  .\install.ps1
  .\install.ps1 -Autostart
  .\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$NoBuild,
    [switch]$Autostart,
    [switch]$Uninstall,
    [string]$ExtensionId
)
$ErrorActionPreference = 'Stop'

$HostName  = 'com.mdl.host'
$Root      = $PSScriptRoot
$InstallTo = Join-Path $env:LOCALAPPDATA 'mdl\bin'
$Manifest  = Join-Path $InstallTo "$HostName.json"
$RegKeys   = @(
    "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\$HostName"
)
$RunKey    = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$Binaries  = 'mdl-daemon.exe', 'mdl-shim.exe', 'mdl-cli.exe'

function Stop-Daemon {
    # Binaries can't be overwritten while running. The daemon persists its jobs, so stopping it is safe.
    Get-Process mdl-daemon, mdl-shim -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300
}

if ($Uninstall) {
    Stop-Daemon
    foreach ($k in $RegKeys) { if (Test-Path $k) { Remove-Item $k -Force } }
    Remove-ItemProperty -Path $RunKey -Name 'mdl-daemon' -ErrorAction SilentlyContinue
    if (Test-Path $InstallTo) { Remove-Item $InstallTo -Recurse -Force }
    Write-Host "Uninstalled. Job list and logs kept in $(Split-Path $InstallTo)."
    return
}

if (-not $ExtensionId) {
    $idFile = Join-Path $Root 'extension\EXTENSION_ID'
    if (-not (Test-Path $idFile)) { throw "No -ExtensionId given and $idFile is missing." }
    $ExtensionId = (Get-Content $idFile -Raw).Trim()
}
if ($ExtensionId -notmatch '^[a-p]{32}$') { throw "'$ExtensionId' is not a valid extension ID (32 letters a-p)." }

if (-not $NoBuild) {
    Write-Host 'Building release binaries...'
    Push-Location $Root
    try {
        # cargo prints progress on stderr; under 'Stop', Windows PowerShell 5.1 would treat that as fatal.
        $ErrorActionPreference = 'Continue'
        cargo build --release -p mdl-daemon -p mdl-shim -p mdl-cli
        $ErrorActionPreference = 'Stop'
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally { Pop-Location }
}

Stop-Daemon
New-Item -ItemType Directory -Force $InstallTo | Out-Null
foreach ($b in $Binaries) {
    $src = Join-Path $Root "target\release\$b"
    if (-not (Test-Path $src)) { throw "$src not found - build first or drop -NoBuild." }
    Copy-Item $src $InstallTo -Force
}

$shimPath = Join-Path $InstallTo 'mdl-shim.exe'
$manifestJson = [ordered]@{
    name            = $HostName
    description     = 'media_downloader native messaging host'
    path            = $shimPath
    type            = 'stdio'
    allowed_origins = @("chrome-extension://$ExtensionId/")
} | ConvertTo-Json
# Chrome rejects a UTF-8 BOM in the manifest, so write bytes explicitly.
[System.IO.File]::WriteAllText($Manifest, $manifestJson, (New-Object System.Text.UTF8Encoding $false))

foreach ($k in $RegKeys) {
    New-Item -Path $k -Force | Out-Null
    Set-Item -Path $k -Value $Manifest
}

if ($Autostart) {
    Set-ItemProperty -Path $RunKey -Name 'mdl-daemon' -Value "`"$(Join-Path $InstallTo 'mdl-daemon.exe')`""
    Write-Host 'Autostart enabled (starts at next logon).'
}

Write-Host ''
Write-Host "Installed to   $InstallTo"
Write-Host "Host manifest  $Manifest"
Write-Host "Allowed origin chrome-extension://$ExtensionId/"
Write-Host "Registered for Chrome + Edge. Logs: $(Split-Path $InstallTo)\shim.log, daemon.log"
