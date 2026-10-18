#Requires -Version 5.1
<#
.SYNOPSIS
    Release build: cargo release build, collect to dist\, Authenticode-sign, build and sign the MSI.

.DESCRIPTION
    1. cargo xtask licenses --check and cargo build --release --workspace --locked
    2. Collects thumbwright_shell.dll, tw-worker.exe, twctl.exe, thumbwright-settings.exe, LICENSE
       and THIRD_PARTY_LICENSES.md into dist\ (cleaned first); a missing binary fails the release
    3. Signs the binaries (REQ-SEC-4, issue #1), then verifies every signature
    4. Builds dist\Thumbwright-<version>-x64.msi from the signed payload (packaging\wix)
    5. Signs the MSI, then verifies it

    Signing, in order of preference:
      a. The Azure Trusted Signing helper (-SignHelper, default
         C:\Syncs\Resilio\Code\MCP\code-signer\sign-helper.ps1) when the file exists, the
         AzureTrustedSignTool command is on PATH and AZURE_CLIENT_ID, AZURE_TENANT_ID,
         AZURE_CLIENT_SECRET, AZURE_TRUSTED_SIGNING_ACCOUNT and AZURE_TRUSTED_SIGNING_PROFILE
         are set (AZURE_TRUSTED_SIGNING_ENDPOINT is optional).
      b. signtool sign /fd SHA256 /tr <RFC3161 url> /td SHA256 /sha1 <-CertThumbprint> when a
         thumbprint is given.
    If neither is usable the script stops before signing anything; -SkipSign produces unsigned
    development output instead. Verification is `signtool verify /pa /v` and additionally requires
    a timestamp (RFC 3161) so signatures outlive the certificate. A file the signing helper left
    without a timestamp gets one added with `signtool timestamp` before verification.

.PARAMETER SkipSign
    Do not sign or verify. Output is for local testing only and must not be distributed.

.PARAMETER CertThumbprint
    SHA-1 thumbprint of a code-signing certificate in the CurrentUser or LocalMachine store, for
    the signtool path.

.PARAMETER TimestampUrl
    RFC 3161 timestamp authority for signtool.

.PARAMETER SignHelper
    Path of the Azure Trusted Signing helper script.

.PARAMETER SignTool
    Path of signtool.exe. Default: newest x64 signtool under the Windows 10/11 SDK, else PATH.

.PARAMETER AcceptWixEula
    Pass `-acceptEula wix7` to the MSI build (see packaging\wix\README.md for the terms).

.EXAMPLE
    .\scripts\release.ps1 -SkipSign

.EXAMPLE
    .\scripts\release.ps1                      # Azure Trusted Signing via the helper
    .\scripts\release.ps1 -CertThumbprint 0123456789ABCDEF0123456789ABCDEF01234567
#>
[CmdletBinding()]
param(
    [switch]$SkipSign,
    [string]$CertThumbprint,
    [string]$TimestampUrl = 'http://timestamp.digicert.com',
    [string]$SignHelper = 'C:\Syncs\Resilio\Code\MCP\code-signer\sign-helper.ps1',
    [string]$SignTool,
    [switch]$AcceptWixEula
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist'

function Get-WorkspaceVersion {
    $text = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw
    $m = [regex]::Match($text, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"(\d+\.\d+\.\d+)"')
    if (-not $m.Success) {
        throw 'Could not read [workspace.package] version (major.minor.patch) from Cargo.toml'
    }
    return $m.Groups[1].Value
}

function Find-SignTool {
    if ($SignTool) {
        if (-not (Test-Path -LiteralPath $SignTool)) { throw "signtool not found at $SignTool" }
        return $SignTool
    }
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path -LiteralPath $kits) {
        $found = Get-ChildItem -LiteralPath $kits -Directory |
            Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } |
            Select-Object -First 1
        if ($found) { return $found }
    }
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw 'signtool.exe not found (install the Windows SDK, or pass -SignTool).'
}

function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments)
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Exe $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

# Decide how to sign before building anything, so a misconfigured machine fails in seconds
# instead of after a 30 minute release build.
function Get-SigningMethod {
    $azureVars = @('AZURE_CLIENT_ID', 'AZURE_TENANT_ID', 'AZURE_CLIENT_SECRET',
        'AZURE_TRUSTED_SIGNING_ACCOUNT', 'AZURE_TRUSTED_SIGNING_PROFILE')
    $missing = @($azureVars | Where-Object { [string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($_)) })
    $haveTool = [bool](Get-Command AzureTrustedSignTool -ErrorAction SilentlyContinue)
    if ((Test-Path -LiteralPath $SignHelper) -and $haveTool -and $missing.Count -eq 0) {
        return 'azure'
    }
    if ($CertThumbprint) {
        return 'signtool'
    }
    $why = @()
    if (-not (Test-Path -LiteralPath $SignHelper)) { $why += "helper not found: $SignHelper" }
    if (-not $haveTool) { $why += 'AzureTrustedSignTool is not on PATH' }
    if ($missing.Count -gt 0) { $why += "unset: $($missing -join ', ')" }
    throw ("No signing method available. Azure Trusted Signing: $($why -join '; '). " +
        'signtool: pass -CertThumbprint. For an unsigned development build pass -SkipSign.')
}

function Invoke-Sign {
    param([string[]]$Files, [string]$Method, [string]$Scope, [string]$Tool)
    if ($Method -eq 'azure') {
        & $SignHelper -FilePaths $Files -Scope $Scope
        return
    }
    foreach ($file in $Files) {
        Invoke-Native $Tool @('sign', '/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256',
            '/sha1', $CertThumbprint, $file)
    }
}

# signtool verify /pa is the Authenticode policy; "timestamped" in /v output is the RFC 3161
# requirement. Reports every failing file, not just the first.
function Test-Signed {
    param([string[]]$Files, [string]$Tool, [switch]$AddMissingTimestamp)
    # Windows PowerShell 5.1 turns redirected native stderr into terminating errors under
    # 'Stop'; signtool reports a failed verification on stderr, and that must reach the check below.
    $ErrorActionPreference = 'Continue'
    $bad = @()
    foreach ($file in $Files) {
        $out = & $Tool verify /pa /v $file 2>&1 | Out-String
        $ok = ($LASTEXITCODE -eq 0)
        if ($ok -and ($out -notmatch 'timestamped')) {
            if ($AddMissingTimestamp) {
                Write-Warning "$file is signed without a timestamp; adding one from $TimestampUrl"
                & $Tool timestamp /tr $TimestampUrl /td SHA256 $file | Out-Null
                $out = & $Tool verify /pa /v $file 2>&1 | Out-String
                $ok = ($LASTEXITCODE -eq 0) -and ($out -match 'timestamped')
            } else {
                $ok = $false
            }
        }
        if ($ok) {
            Write-Host "  signed + timestamped: $(Split-Path -Leaf $file)"
        } else {
            Write-Warning "Signature check failed for $file`n$out"
            $bad += $file
        }
    }
    if ($bad.Count -gt 0) {
        throw "$($bad.Count) file(s) unsigned or untimestamped: $($bad -join ', ')"
    }
}

$version = Get-WorkspaceVersion
Write-Host "Thumbwright $version release build"

$method = $null
$tool = $null
if ($SkipSign) {
    Write-Warning 'SkipSign: binaries and MSI will be UNSIGNED. Development use only.'
} else {
    $tool = Find-SignTool
    $method = Get-SigningMethod
    Write-Host "Signing method: $method (signtool: $tool)"
}

Push-Location $root
try {
    Write-Host '== Dependency policy'
    Invoke-Native 'cargo' @('xtask', 'licenses', '--check')

    Write-Host '== cargo build --release --workspace'
    Invoke-Native 'cargo' @('build', '--release', '--workspace', '--locked')
} finally {
    Pop-Location
}

$targetDir = $env:CARGO_TARGET_DIR
if ([string]::IsNullOrWhiteSpace($targetDir)) { $targetDir = Join-Path $root 'target' }
$releaseDir = Join-Path $targetDir 'release'

Write-Host "== Collect into $dist"
if (Test-Path -LiteralPath $dist) {
    Remove-Item -LiteralPath $dist -Recurse -Force
}
New-Item -ItemType Directory -Path $dist | Out-Null

$binaries = @('thumbwright_shell.dll', 'tw-worker.exe', 'twctl.exe', 'thumbwright-settings.exe')
foreach ($name in $binaries) {
    $src = Join-Path $releaseDir $name
    if (-not (Test-Path -LiteralPath $src)) { throw "Release build did not produce $src" }
    Copy-Item -LiteralPath $src -Destination $dist
}
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $dist
Copy-Item -LiteralPath (Join-Path $root 'THIRD_PARTY_LICENSES.md') -Destination $dist
$payload = @($binaries | ForEach-Object { Join-Path $dist $_ })

if (-not $SkipSign) {
    Write-Host '== Sign binaries'
    Invoke-Sign -Files $payload -Method $method -Scope 'Bin' -Tool $tool
    Test-Signed -Files $payload -Tool $tool -AddMissingTimestamp
}

Write-Host '== Build MSI'
$msi = Join-Path $dist "Thumbwright-$version-x64.msi"
$buildArgs = @{ SourceDir = $dist; OutFile = $msi; Version = $version }
if ($AcceptWixEula) { $buildArgs.AcceptWixEula = $true }
& (Join-Path $root 'packaging\wix\build-msi.ps1') @buildArgs

if (-not $SkipSign) {
    Write-Host '== Sign MSI'
    Invoke-Sign -Files @($msi) -Method $method -Scope 'MSI' -Tool $tool
    Test-Signed -Files @($msi) -Tool $tool -AddMissingTimestamp
}

Write-Host '== Result'
Get-ChildItem -LiteralPath $dist -File | ForEach-Object {
    $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
    '{0,-34} {1,12:N0} bytes  sha256 {2}' -f $_.Name, $_.Length, $hash
}
if ($SkipSign) {
    Write-Warning "UNSIGNED build: $msi"
} else {
    Write-Host "Signed release: $msi"
}
