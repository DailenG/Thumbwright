#Requires -Version 5.1
<#
.SYNOPSIS
    Builds the Thumbwright per-machine MSI from a directory of (already signed) binaries.

.DESCRIPTION
    Wraps `wix build` for packaging\wix\Package.wxs. SourceDir must contain thumbwright_shell.dll,
    tw-worker.exe, twctl.exe, Thumbwright.msix (the sparse package for the Windows 11 menu,
    built by packaging\msix\build.ps1; the MSI registers it for all users), LICENSE and
    THIRD_PARTY_LICENSES.md; thumbwright-settings.exe is included when present (or excluded
    with -NoSettings). The MSI never contains Thumbwright.ini.

    WiX v7 requires acceptance of the WiX OSMF EULA (free below US$10,000/year of revenue from
    projects using WiX; see packaging\wix\README.md). Accept it once with `wix eula accept wix7`,
    or pass -AcceptWixEula to accept for this invocation only.

.PARAMETER SourceDir
    Directory with the payload files.

.PARAMETER OutFile
    Path of the MSI to write. The directory is created if needed.

.PARAMETER Version
    MSI ProductVersion (major.minor.build). Windows Installer compares only the first three
    fields, so a 4-part version is rejected here instead of being silently truncated.

.PARAMETER NoSettings
    Leave thumbwright-settings.exe out even if it exists in SourceDir.

.PARAMETER AcceptWixEula
    Pass `-acceptEula wix7` to wix for this build.

.EXAMPLE
    .\build-msi.ps1 -SourceDir ..\..\dist -OutFile ..\..\dist\Thumbwright-0.1.0-x64.msi -Version 0.1.0
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$SourceDir,

    [Parameter(Mandatory)]
    [string]$OutFile,

    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version,

    [switch]$NoSettings,

    [switch]$AcceptWixEula
)

$ErrorActionPreference = 'Stop'

$wxs = Join-Path $PSScriptRoot 'Package.wxs'
$SourceDir = (Resolve-Path -LiteralPath $SourceDir).Path

$required = @('thumbwright_shell.dll', 'tw-worker.exe', 'twctl.exe', 'Thumbwright.msix', 'LICENSE', 'THIRD_PARTY_LICENSES.md')
foreach ($name in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $SourceDir $name))) {
        throw "Payload file missing from ${SourceDir}: $name"
    }
}
# Portable mode is keyed on this file next to the binaries (REQ-REG-6); an installed copy must not have it.
if (Test-Path -LiteralPath (Join-Path $SourceDir 'Thumbwright.ini')) {
    throw "Thumbwright.ini found in $SourceDir. Remove it: it would put the installed copy in portable mode."
}

# The uninstall actions find the sparse package by its identity name, so take it from the package
# that ships rather than repeating it.
function Get-MsixIdentityName {
    param([string]$Path)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $zip.GetEntry('AppxManifest.xml')
        if ($null -eq $entry) { throw "$Path has no AppxManifest.xml" }
        $reader = New-Object System.IO.StreamReader ($entry.Open())
        try { [xml]$manifest = $reader.ReadToEnd() } finally { $reader.Dispose() }
    } finally {
        $zip.Dispose()
    }
    return $manifest.Package.Identity.Name
}
$packageName = Get-MsixIdentityName (Join-Path $SourceDir 'Thumbwright.msix')
if ($packageName -notmatch '^[A-Za-z0-9][A-Za-z0-9.\-]+$') {
    throw "Thumbwright.msix has an unusable identity name: '$packageName'"
}

$wix = Get-Command wix -ErrorAction SilentlyContinue
if ($wix) {
    $wixExe = $wix.Source
} else {
    $wixExe = Join-Path $env:USERPROFILE '.dotnet\tools\wix.exe'
    if (-not (Test-Path -LiteralPath $wixExe)) {
        throw 'wix CLI not found. Install it with: dotnet tool install --global wix --version 7.0.0'
    }
}

# -pdbtype none: no .wixpdb next to the MSI (dist\ must hold only shippable files).
$wixArgs = @('build', '-arch', 'x64', '-pdbtype', 'none', '-d', "Version=$Version", '-d', "PackageName=$packageName", '-d', "SourceDir=$SourceDir")
$settingsPresent = Test-Path -LiteralPath (Join-Path $SourceDir 'thumbwright-settings.exe')
if ($settingsPresent -and -not $NoSettings) {
    $wixArgs += @('-d', 'IncludeSettings=1')
    Write-Verbose 'Including thumbwright-settings.exe'
} else {
    Write-Verbose 'Building without thumbwright-settings.exe'
}
if ($AcceptWixEula) {
    $wixArgs += @('-acceptEula', 'wix7')
}

$outDir = Split-Path -Parent ([System.IO.Path]::GetFullPath($OutFile))
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$wixArgs += @($wxs, '-o', $OutFile)

Write-Verbose "$wixExe $($wixArgs -join ' ')"
& $wixExe @wixArgs
if ($LASTEXITCODE -ne 0) {
    throw "wix build failed with exit code $LASTEXITCODE"
}
Write-Host "Built $OutFile"
