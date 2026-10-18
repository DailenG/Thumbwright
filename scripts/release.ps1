#Requires -Version 5.1
<#
.SYNOPSIS
    Release build: cargo release build, collect to dist\, Authenticode-sign, build and sign the
    sparse MSIX package and the MSI.

.DESCRIPTION
    1. cargo xtask licenses --check and cargo build --release --workspace --locked
    2. Collects thumbwright_shell.dll, tw-worker.exe, twctl.exe, LICENSE and
       THIRD_PARTY_LICENSES.md into dist\ (cleaned first); a missing binary fails the release
    3. Signs those binaries (REQ-SEC-4, issue #1), then verifies every signature
    4. Reads the subject of the certificate that signed the DLL: that is the sparse package's
       Publisher (it must equal the signing certificate subject exactly, issue #19)
    5. Rebuilds with TW_MSIX_PUBLISHER set so thumbwright-settings.exe embeds the matching
       <msix> identity element (crates\tw-settings\build.rs), collects it, signs and verifies it
    6. Builds dist\Thumbwright.msix (packaging\msix\build.ps1 -Publisher <that subject>), signs
       it with the same method, verifies it, and checks its signer subject equals the Publisher
    7. Builds dist\Thumbwright-<version>-x64.msi from the signed payload, Thumbwright.msix
       included (packaging\wix; the installer registers the package for all users)
    8. Signs the MSI, then verifies it

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

.PARAMETER DevPublisher
    Publisher written into the sparse package and the settings exe when -SkipSign is used (a
    signed release takes it from the signing certificate instead). The package is unsigned in
    that case, so Windows will not register it.

.PARAMETER DistDir
    Output directory, emptied first. Default: dist\ in the repository.

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
    [string]$DevPublisher = 'CN=Thumbwright Unsigned Development',
    [string]$DistDir,
    [switch]$AcceptWixEula
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dist = if ($DistDir) { [System.IO.Path]::GetFullPath($DistDir) } else { Join-Path $root 'dist' }

# Identity of the sparse package for the Windows 11 menu (packaging\msix). The name goes to the
# executable's manifest, to the package and to the MSI; the application id is read from the
# package manifest template so that the executable and the package cannot disagree.
$msixPackageName = 'Thumbwright.ShellExtension'
$msixApplicationId = $null
$template = Get-Content -LiteralPath (Join-Path $root 'packaging\msix\AppxManifest.xml') -Raw
$idMatch = [regex]::Match($template, '<Application\s+Id="([^"]+)"')
if (-not $idMatch.Success) { throw 'No <Application Id="..."> in packaging\msix\AppxManifest.xml' }
$msixApplicationId = $idMatch.Groups[1].Value

# Subject of the certificate that signed a PE file or MSI, exactly as Windows reports it
# (.NET Framework form: "CN=..., O=..., L=..., S=..., C=..."). CreateFromSignedFile is used
# rather than Get-AuthenticodeSignature, which does not load when PSModulePath is inherited from
# PowerShell 7.
function Get-SignerSubject {
    param([string]$Path)
    $certificate = [System.Security.Cryptography.X509Certificates.X509Certificate]::CreateFromSignedFile($Path)
    return $certificate.Subject
}

# Subject of the certificate that signed an .msix: its AppxSignature.p7x part is a 4-byte "PKCX"
# tag followed by a PKCS #7 SignedData blob.
function Get-MsixSignerSubject {
    param([string]$Path)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    Add-Type -AssemblyName System.Security
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $zip.GetEntry('AppxSignature.p7x')
        if ($null -eq $entry) { throw "$Path has no AppxSignature.p7x; it is not signed." }
        $stream = $entry.Open()
        try {
            $memory = New-Object System.IO.MemoryStream
            $stream.CopyTo($memory)
            $part = $memory.ToArray()
        } finally {
            $stream.Dispose()
        }
    } finally {
        $zip.Dispose()
    }
    if ($part.Length -le 4 -or [System.Text.Encoding]::ASCII.GetString($part, 0, 4) -ne 'PKCX') {
        throw "$Path has an AppxSignature.p7x without the PKCX tag."
    }
    [byte[]]$blob = New-Object byte[] ($part.Length - 4)
    [System.Array]::Copy($part, 4, $blob, 0, $blob.Length)
    $cms = New-Object System.Security.Cryptography.Pkcs.SignedCms
    $cms.Decode($blob)
    return $cms.SignerInfos[0].Certificate.Subject
}

# The executable must carry the <msix> element that ties it to the package; without a match
# Windows runs it unpackaged and no error is shown. Reads the manifest straight from the file.
function Assert-EmbeddedIdentity {
    param([string]$Exe, [string]$Publisher, [string]$PackageName, [string]$ApplicationId)
    $text = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($Exe))
    $found = [regex]::Match($text, '<msix\b[^>]*>')
    if (-not $found.Success) { throw "$Exe has no <msix> element in its embedded manifest." }
    $xml = $found.Value
    if (-not $xml.EndsWith('/>')) { $xml += '</msix>' }
    $element = ([xml]$xml).DocumentElement
    $expected = @{ publisher = $Publisher; packageName = $PackageName; applicationId = $ApplicationId }
    foreach ($name in $expected.Keys) {
        $actual = $element.GetAttribute($name)
        if ($actual -ne $expected[$name]) {
            throw "$(Split-Path -Leaf $Exe): embedded msix $name is '$actual', expected '$($expected[$name])'."
        }
    }
    Write-Host "  $(Split-Path -Leaf $Exe) embeds the package identity (publisher, packageName, applicationId)"
}

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

    # The first pass has no package identity (the publisher is not known yet); see below.
    Remove-Item Env:TW_MSIX_PUBLISHER, Env:TW_MSIX_PACKAGE_NAME, Env:TW_MSIX_APPLICATION_ID -ErrorAction SilentlyContinue
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

# The settings exe is built, signed and collected after the others: it embeds the package
# identity, whose publisher is only known once something has been signed (next section).
$binaries = @('thumbwright_shell.dll', 'tw-worker.exe', 'twctl.exe')
foreach ($name in $binaries) {
    $src = Join-Path $releaseDir $name
    if (-not (Test-Path -LiteralPath $src)) { throw "Release build did not produce $src" }
    Copy-Item -LiteralPath $src -Destination $dist
}
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $dist
Copy-Item -LiteralPath (Join-Path $root 'THIRD_PARTY_LICENSES.md') -Destination $dist
$payload = @($binaries | ForEach-Object { Join-Path $dist $_ })

if ($SkipSign) {
    $publisher = $DevPublisher
    Write-Warning "SkipSign: the sparse package publisher is the placeholder $publisher; the package is unsigned and cannot be registered."
} else {
    Write-Host '== Sign binaries'
    Invoke-Sign -Files $payload -Method $method -Scope 'Bin' -Tool $tool
    Test-Signed -Files $payload -Tool $tool -AddMissingTimestamp
    # The package Publisher must equal the signing certificate subject exactly, so it is read
    # from the certificate that just signed the DLL, never typed in.
    $publisher = Get-SignerSubject (Join-Path $dist 'thumbwright_shell.dll')
}
Write-Host "Package publisher: $publisher"

# Rebuild so thumbwright-settings.exe embeds the <msix> identity element for this publisher
# (crates/tw-settings/build.rs). Only that crate's build script re-runs; everything else is up
# to date.
Write-Host '== Settings exe with package identity'
$settingsExe = 'thumbwright-settings.exe'
$env:TW_MSIX_PUBLISHER = $publisher
$env:TW_MSIX_PACKAGE_NAME = $msixPackageName
$env:TW_MSIX_APPLICATION_ID = $msixApplicationId
Push-Location $root
try {
    Invoke-Native 'cargo' @('build', '--release', '--workspace', '--locked')
} finally {
    Pop-Location
    Remove-Item Env:TW_MSIX_PUBLISHER, Env:TW_MSIX_PACKAGE_NAME, Env:TW_MSIX_APPLICATION_ID -ErrorAction SilentlyContinue
}
$settingsSrc = Join-Path $releaseDir $settingsExe
if (-not (Test-Path -LiteralPath $settingsSrc)) { throw "Release build did not produce $settingsSrc" }
Copy-Item -LiteralPath $settingsSrc -Destination $dist
$settingsPath = Join-Path $dist $settingsExe
Assert-EmbeddedIdentity $settingsPath $publisher $msixPackageName $msixApplicationId
if (-not $SkipSign) {
    Invoke-Sign -Files @($settingsPath) -Method $method -Scope 'Bin' -Tool $tool
    Test-Signed -Files @($settingsPath) -Tool $tool -AddMissingTimestamp
}

Write-Host '== Build sparse package'
$msix = Join-Path $dist 'Thumbwright.msix'
& (Join-Path $root 'packaging\msix\build.ps1') -Publisher $publisher -PackageName $msixPackageName -OutputPath $msix | Out-Null
if (-not (Test-Path -LiteralPath $msix)) { throw "packaging\msix\build.ps1 did not produce $msix" }
if (-not $SkipSign) {
    Write-Host '== Sign sparse package'
    Invoke-Sign -Files @($msix) -Method $method -Scope 'Bin' -Tool $tool
    Test-Signed -Files @($msix) -Tool $tool -AddMissingTimestamp
    $msixSigner = Get-MsixSignerSubject $msix
    if ($msixSigner -ne $publisher) {
        throw "The package is signed by '$msixSigner' but its Publisher is '$publisher'; Windows would refuse to register it."
    }
    Write-Host '  package signer subject equals the manifest Publisher'
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
# .NET hashing: Get-FileHash is missing from Windows PowerShell 5.1 when PSModulePath is
# inherited from PowerShell 7 (it then loads 7's Microsoft.PowerShell.Utility).
$sha = [System.Security.Cryptography.SHA256]::Create()
Get-ChildItem -LiteralPath $dist -File | ForEach-Object {
    $stream = [System.IO.File]::OpenRead($_.FullName)
    try { $hash = [System.BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
    finally { $stream.Dispose() }
    '{0,-34} {1,12:N0} bytes  sha256 {2}' -f $_.Name, $_.Length, $hash
}
$sha.Dispose()
if ($SkipSign) {
    Write-Warning "UNSIGNED build: $msi"
} else {
    Write-Host "Signed release: $msi"
}
