#Requires -Version 5.1
<#
.SYNOPSIS
    Release build: cargo release build, collect to dist\, Authenticode-sign, build and sign the
    sparse MSIX package and the MSI.

.DESCRIPTION
    1. cargo xtask licenses --check and cargo build --release --workspace --locked
    2. Collects thumbwright_shell.dll, tw-worker.exe, twctl.exe, thumbwright-settings.exe, LICENSE
       and THIRD_PARTY_LICENSES.md into dist\ (cleaned first); a missing binary fails the release
    3. Signs those binaries (REQ-SEC-4, issue #1), then verifies every signature
    4. Reads the subject of the certificate that signed the DLL: that is the sparse package's
       Publisher (it must equal the signing certificate subject exactly, issue #19)
    5. Checks that thumbwright-settings.exe carries no package identity (a packaged process
       would have its HKCU writes virtualized away from Explorer)
    6. Builds dist\Thumbwright.msix (packaging\msix\build.ps1 -Publisher <that subject>), signs
       it with the same method, verifies it, and checks its signer subject equals the Publisher
    7. Builds dist\Thumbwright-<version>-x64.msi from the signed payload, Thumbwright.msix
       included (packaging\wix; the installer registers the package for all users)
    8. Signs the MSI, then verifies it

    Signing, in order of preference:
      a. The Azure Trusted Signing helper (-SignHelper or $env:TW_SIGN_HELPER; there is no
         default, so no machine-specific path is baked into the repository) when it names an
         existing file, the AzureTrustedSignTool command is on PATH and AZURE_CLIENT_ID,
         AZURE_TENANT_ID, AZURE_CLIENT_SECRET, AZURE_TRUSTED_SIGNING_ACCOUNT and
         AZURE_TRUSTED_SIGNING_PROFILE are set (AZURE_TRUSTED_SIGNING_ENDPOINT is optional).
         The helper receives the signing secrets in its environment: keep it in a folder only you
         can write to, not in a synced or shared one.
      b. signtool sign /fd SHA256 /tr <RFC3161 url> /td SHA256 /sha1 <-CertThumbprint> when a
         thumbprint is given. This is the reproducible route for anyone with a code-signing
         certificate in their certificate store; it needs nothing outside this repository.
    A missing helper is an error with a message that names both routes, never a silent fallback.
    If neither is usable the script stops before signing anything; -SkipSign produces unsigned
    development output instead. Verification is `signtool verify /pa /v` and additionally requires
    a timestamp (RFC 3161) so signatures outlive the certificate. A file the signing helper left
    without a timestamp gets one added with `signtool timestamp` before verification.

    Builder paths (audit P-5). rustc embeds source paths into panic locations, so a plain
    release build carries the builder's cargo registry path (C:\Users\<name>\.cargo\registry\...).
    Both cargo builds here pass `--config target.x86_64-pc-windows-msvc.rustflags=[...]` with a
    --remap-path-prefix for the builder's user profile, rustup home, cargo home, target directory
    and the repository root. Cargo appends these to the rustflags in .cargo/config.toml (the
    crt-static flag stays). The prefixes are derived from this machine's USERPROFILE, CARGO_HOME,
    RUSTUP_HOME and CARGO_TARGET_DIR at run time, so every builder remaps their own paths with no
    edit. The script then scans the binaries and fails if any of those paths is still in them.
    RUSTFLAGS and CARGO_ENCODED_RUSTFLAGS replace config rustflags outright, so the script refuses
    to run with either set.

.PARAMETER SkipSign
    Do not sign or verify. Output is for local testing only and must not be distributed.

.PARAMETER CertThumbprint
    SHA-1 thumbprint of a code-signing certificate in the CurrentUser or LocalMachine store, for
    the signtool path.

.PARAMETER TimestampUrl
    RFC 3161 timestamp authority for signtool.

.PARAMETER SignHelper
    Path of the Azure Trusted Signing helper script. Default: $env:TW_SIGN_HELPER (no built-in
    default).

.PARAMETER SignTool
    Path of signtool.exe. Default: newest x64 signtool under the Windows 10/11 SDK, else PATH.

.PARAMETER DevPublisher
    Publisher written into the sparse package and the settings exe when -SkipSign is used (a
    signed release takes it from the signing certificate instead). The package is unsigned in
    that case, so Windows will not register it.

.PARAMETER DistDir
    Output directory, emptied first. Default: dist\ in the repository.

.PARAMETER TrustedSigningDlib
    Azure.CodeSigning.Dlib.dll (x64) from the Microsoft.Trusted.Signing.Client NuGet package,
    used with signtool for the sparse .msix. Default: $env:TW_TRUSTED_SIGNING_DLIB.

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
    [string]$SignHelper = $env:TW_SIGN_HELPER,
    [string]$SignTool,
    [string]$DevPublisher = 'CN=Thumbwright Unsigned Development',
    [string]$DistDir,
    [string]$TrustedSigningDlib = $env:TW_TRUSTED_SIGNING_DLIB,
    [switch]$AcceptWixEula
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dist = if ($DistDir) { [System.IO.Path]::GetFullPath($DistDir) } else { Join-Path $root 'dist' }

# Identity name of the sparse package for the Windows 11 menu (packaging\msix); it goes to the
# package and to the MSI. The package's Application names thumbwright-settings.exe but that
# executable carries no identity of its own.
$msixPackageName = 'Thumbwright.ShellExtension'

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

# The settings executable must not carry package identity (an <msix> element in its embedded
# manifest). A process with identity writes HKCU into the package's private hive, which
# Explorer never reads. Reads the manifest straight from the file.
function Assert-NoPackageIdentity {
    param([string]$Exe)
    $text = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($Exe))
    if ($text -match '<msix\b') {
        throw "$(Split-Path -Leaf $Exe) embeds an <msix> package identity element; settings it saves would be virtualized and never reach Explorer."
    }
    Write-Host "  $(Split-Path -Leaf $Exe) has no package identity (settings are written to the real HKCU)"
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

# --- Builder path remap (audit P-5) -------------------------------------------------------------
# rustc embeds absolute source paths (panic locations, include_str!/OUT_DIR), which would put the
# builder's user name into a shipped binary. Returns the extra cargo arguments that remap them.
# `--config` arrays are appended to .cargo/config.toml's rustflags (the crt-static flag stays), but
# RUSTFLAGS / CARGO_ENCODED_RUSTFLAGS replace config rustflags outright, so refuse to run with them.
# Later --remap-path-prefix flags win, so the broad prefix (the user profile) comes first.
function Get-PathRemapPrefixes {
    $userHome = $env:USERPROFILE
    if ([string]::IsNullOrWhiteSpace($userHome)) { throw 'USERPROFILE is not set; cannot derive the builder paths to remap.' }
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $userHome '.cargo' }
    $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $userHome '.rustup' }
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
    $pairs = @(
        @($userHome, 'home'), @($rustupHome, 'rustup'), @($cargoHome, 'cargo'),
        @($target, 'target'), @($root, '.'))
    foreach ($p in $pairs) {
        $full = [System.IO.Path]::GetFullPath($p[0]).TrimEnd('\')
        if ($full.Contains("'")) { throw "Cannot remap '$full': a single quote in the path cannot be passed through cargo --config." }
        [pscustomobject]@{ From = $full; To = $p[1] }
    }
}

function Get-PathRemapArgs {
    if ($env:RUSTFLAGS -or $env:CARGO_ENCODED_RUSTFLAGS) {
        throw 'RUSTFLAGS / CARGO_ENCODED_RUSTFLAGS are set; they would replace the rustflags in .cargo/config.toml (crt-static) and the path remap. Unset them for a release build.'
    }
    $flags = @(Get-PathRemapPrefixes | ForEach-Object { "'--remap-path-prefix=$($_.From)=$($_.To)'" })
    return @('--config', ('target.x86_64-pc-windows-msvc.rustflags=[' + ($flags -join ',') + ']'))
}

# Fails the release when a binary still contains a builder path (ASCII or UTF-16, any case).
function Assert-NoBuilderPaths {
    param([string[]]$Files)
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $needles = @(Get-PathRemapPrefixes | ForEach-Object { $_.From } | Select-Object -Unique)
    $bad = @()
    foreach ($file in $Files) {
        $bytes = [System.IO.File]::ReadAllBytes($file)
        $ascii = $latin1.GetString($bytes)
        $wide = [System.Text.Encoding]::Unicode.GetString($bytes)
        $wideOdd = [System.Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1)
        foreach ($n in $needles) {
            foreach ($hay in $ascii, $wide, $wideOdd) {
                if ($hay.IndexOf($n, [StringComparison]::OrdinalIgnoreCase) -ge 0) { $bad += "$(Split-Path -Leaf $file) contains $n"; break }
            }
        }
    }
    if ($bad.Count -gt 0) {
        throw ("Builder paths leaked into the binaries (--remap-path-prefix did not cover them): " + ($bad -join '; '))
    }
    Write-Host '  no builder paths in the binaries'
}

# Decide how to sign before building anything, so a misconfigured machine fails in seconds
# instead of after a 30 minute release build.
function Get-SigningMethod {
    $azureVars = @('AZURE_CLIENT_ID', 'AZURE_TENANT_ID', 'AZURE_CLIENT_SECRET',
        'AZURE_TRUSTED_SIGNING_ACCOUNT', 'AZURE_TRUSTED_SIGNING_PROFILE')
    $missing = @($azureVars | Where-Object { [string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($_)) })
    $haveTool = [bool](Get-Command AzureTrustedSignTool -ErrorAction SilentlyContinue)
    if ($SignHelper -and (Test-Path -LiteralPath $SignHelper) -and $haveTool -and $missing.Count -eq 0) {
        if (-not $TrustedSigningDlib -or -not (Test-Path -LiteralPath $TrustedSigningDlib)) {
            throw ('Azure Trusted Signing also needs -TrustedSigningDlib (Azure.CodeSigning.Dlib.dll, ' +
                'bin\x64 of the Microsoft.Trusted.Signing.Client NuGet package) to sign the sparse package.')
        }
        return 'azure'
    }
    if ($CertThumbprint) {
        return 'signtool'
    }
    $why = @()
    if (-not $SignHelper) {
        $why += 'no signing helper: set $env:TW_SIGN_HELPER or pass -SignHelper (there is no built-in default)'
    } elseif (-not (Test-Path -LiteralPath $SignHelper)) {
        $why += "helper not found: $SignHelper"
    }
    if (-not $haveTool) { $why += 'AzureTrustedSignTool is not on PATH' }
    if ($missing.Count -gt 0) { $why += "unset: $($missing -join ', ')" }
    throw ("No signing method available. Azure Trusted Signing: $($why -join '; '). " +
        'Reproducible alternative with any code-signing certificate in your store: pass -CertThumbprint <sha1>. ' +
        'For an unsigned development build pass -SkipSign.')
}

# AzureTrustedSignTool signs PE files and MSIs, but repacks app packages with MakeAppx, which
# rejects a sparse package (its Application executable is outside the package). For .msix it
# uses signtool with the Trusted Signing dlib instead (Microsoft's documented route for
# packages), with the same account and profile.
function Invoke-AzurePackageSign {
    param([string]$File, [string]$Tool)
    $endpoint = $env:AZURE_TRUSTED_SIGNING_ENDPOINT
    if ([string]::IsNullOrWhiteSpace($endpoint)) { $endpoint = 'https://eus.codesigning.azure.net' }
    $metadata = Join-Path ([System.IO.Path]::GetTempPath()) ("tw-sign-{0}.json" -f [guid]::NewGuid())
    @{ Endpoint = $endpoint
       CodeSigningAccountName = $env:AZURE_TRUSTED_SIGNING_ACCOUNT
       CertificateProfileName = $env:AZURE_TRUSTED_SIGNING_PROFILE } |
        ConvertTo-Json | Set-Content -LiteralPath $metadata -Encoding ASCII
    try {
        Invoke-Native $Tool @('sign', '/fd', 'SHA256', '/tr', 'http://timestamp.acs.microsoft.com',
            '/td', 'SHA256', '/dlib', $TrustedSigningDlib, '/dmdf', $metadata, $File)
    } finally {
        Remove-Item -LiteralPath $metadata -Force -ErrorAction SilentlyContinue
    }
}

function Invoke-Sign {
    param([string[]]$Files, [string]$Method, [string]$Scope, [string]$Tool)
    if ($Method -eq 'azure') {
        $packages = @($Files | Where-Object { [System.IO.Path]::GetExtension($_) -in '.msix', '.appx' })
        $others = @($Files | Where-Object { $packages -notcontains $_ })
        if ($others.Count -gt 0) { & $SignHelper -FilePaths $others -Scope $Scope }
        foreach ($p in $packages) { Invoke-AzurePackageSign -File $p -Tool $Tool }
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

# Remap the builder's paths out of the binaries (audit P-5); refuses to run with RUSTFLAGS set.
$remapArgs = Get-PathRemapArgs

Push-Location $root
try {
    Write-Host '== Dependency policy'
    Invoke-Native 'cargo' @('xtask', 'licenses', '--check')

    # No executable carries package identity (see Assert-NoPackageIdentity).
    Write-Host '== cargo build --release --workspace'
    Invoke-Native 'cargo' (@('build', '--release', '--workspace', '--locked') + $remapArgs)
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
Write-Host '== Scan binaries for builder paths'
Assert-NoBuilderPaths $payload

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

# thumbwright-settings.exe must NOT carry package identity: a packaged process has its HKCU
# writes redirected into the package's private hive, so settings saved from the window would
# never reach Explorer (docs/ARCHITECTURE.md, "Settings store"). It was collected and signed
# with the other binaries above; make sure no <msix> element slipped in.
Assert-NoPackageIdentity (Join-Path $dist 'thumbwright-settings.exe')

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
