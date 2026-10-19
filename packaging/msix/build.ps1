<#
.SYNOPSIS
    Builds the unsigned sparse MSIX package for the Windows 11 modern context menu.

.DESCRIPTION
    Fills the tokens in AppxManifest.xml, copies the logos from packaging\icons\msix (every
    scale and target size, made by packaging\icons\generate.ps1), indexes them into
    resources.pri with makepri so the shell can pick a size, and runs makeappx pack. The
    package has no payload besides the manifest, logos and index: the binaries stay in the
    install directory, which is given as the external location when the package is
    registered (see the comment in AppxManifest.xml).

    The package is NOT signed here. Signing is the release step: sign the .msix with the
    certificate whose subject is exactly the -Publisher value, or registration fails.

    The Publisher is a release input and has no default: use the subject of the signing
    certificate that will sign the package.

.PARAMETER Publisher
    Package publisher, identical to the signing certificate subject, for example
    "CN=Example Publisher". Required.

.PARAMETER PublisherDisplayName
    Publisher name shown in Settings. Defaults to the CN value of -Publisher.

.PARAMETER PackageName
    Package identity name. Defaults to this product's own identity.

.PARAMETER Version
    Four-part package version. Defaults to the workspace version with a .0 revision.

.PARAMETER OutputPath
    Where to write the .msix. Defaults to out\Thumbwright.msix next to this script.

.PARAMETER MakeAppx
    Path to makeappx.exe. Defaults to the newest Windows 10/11 SDK found; makepri.exe is
    taken from the same directory.

.EXAMPLE
    .\build.ps1 -Publisher 'CN=Example Publisher'
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^CN=.+')]
    [string]$Publisher,

    [string]$PublisherDisplayName,

    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9.\-]{2,49}$')]
    [string]$PackageName = 'Thumbwright.ShellExtension',

    [ValidatePattern('^\d+\.\d+\.\d+\.\d+$')]
    [string]$Version,

    [string]$OutputPath = (Join-Path $PSScriptRoot 'out\Thumbwright.msix'),

    [string]$MakeAppx
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')

function Get-WorkspaceVersion {
    $cargo = Join-Path $repoRoot 'Cargo.toml'
    $text = Get-Content -LiteralPath $cargo -Raw
    if ($text -notmatch '(?ms)\[workspace\.package\].*?^version\s*=\s*"(\d+\.\d+\.\d+)"') {
        throw "Cannot read the workspace version from $cargo"
    }
    return $Matches[1]
}

# The manifest CLSID comes from the Rust source so the two cannot drift apart.
function Get-ExplorerCommandClsid {
    $ids = Join-Path $repoRoot 'crates\tw-core\src\ids.rs'
    $text = Get-Content -LiteralPath $ids -Raw
    $pattern = 'CLSID_EXPLORER_COMMAND:\s*ClassId\s*=\s*ClassId\s*\{\s*guid:\s*0x([0-9A-Fa-f_]+)'
    if ($text -notmatch $pattern) {
        throw "CLSID_EXPLORER_COMMAND not found in $ids"
    }
    $hex = $Matches[1].Replace('_', '')
    if ($hex.Length -ne 32) {
        throw "CLSID_EXPLORER_COMMAND is not a 128-bit value: $hex"
    }
    return ('{0}-{1}-{2}-{3}-{4}' -f $hex.Substring(0, 8), $hex.Substring(8, 4),
        $hex.Substring(12, 4), $hex.Substring(16, 4), $hex.Substring(20, 12)).ToUpperInvariant()
}

function Find-MakeAppx {
    if ($MakeAppx) {
        return $MakeAppx
    }
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $found = Get-ChildItem -Path $kits -Filter makeappx.exe -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Sort-Object { [version]($_.FullName -replace '^.*\\bin\\(\d+(\.\d+)+)\\.*$', '$1') } -Descending |
        Select-Object -First 1
    if (-not $found) {
        throw 'makeappx.exe not found. Install the Windows SDK or pass -MakeAppx.'
    }
    return $found.FullName
}


if (-not $Version) {
    $Version = (Get-WorkspaceVersion) + '.0'
}
if (-not $PublisherDisplayName) {
    $PublisherDisplayName = ($Publisher -replace '^CN=', '') -replace ',.*$', ''
}

$tokens = @{
    '@PACKAGE_NAME@'           = $PackageName
    '@PUBLISHER@'              = [System.Security.SecurityElement]::Escape($Publisher)
    '@PUBLISHER_DISPLAY_NAME@' = [System.Security.SecurityElement]::Escape($PublisherDisplayName)
    '@VERSION@'                = $Version
    '@EXPLORER_COMMAND_CLSID@' = Get-ExplorerCommandClsid
    '@SHELL_DLL@'              = 'thumbwright_shell.dll'
    '@APP_EXE@'                = 'thumbwright-settings.exe'
}

# The template's header comment describes the tokens and would be filled in like the rest;
# the shipped manifest does not need it.
$manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'AppxManifest.xml') -Raw
$manifest = [regex]::Replace($manifest, '(?s)<!--.*?-->\s*', '')
foreach ($name in $tokens.Keys) {
    $manifest = $manifest.Replace($name, $tokens[$name])
}
$left = [regex]::Matches($manifest, '@[A-Z_]+@') | ForEach-Object { $_.Value }
if ($left) {
    throw "Unfilled manifest tokens: $($left -join ', ')"
}
[xml]$null = $manifest   # fail early on malformed XML

$makeappxExe = Find-MakeAppx
$makepriExe = Join-Path (Split-Path -Parent $makeappxExe) 'makepri.exe'
if (-not (Test-Path -LiteralPath $makepriExe)) {
    throw "makepri.exe not found next to $makeappxExe."
}
$logos = Join-Path $PSScriptRoot '..\icons\msix'
if (-not (Test-Path -LiteralPath (Join-Path $logos 'Square44x44Logo.scale-100.png'))) {
    throw "Logos missing in $logos; run packaging\icons\generate.ps1."
}
$outputFull = [System.IO.Path]::GetFullPath($OutputPath)

if ($PSCmdlet.ShouldProcess($outputFull, 'Build unsigned sparse MSIX package')) {
    $stage = Join-Path ([System.IO.Path]::GetTempPath()) ('tw-msix-' + [guid]::NewGuid().ToString('N'))
    try {
        $assets = Join-Path $stage 'Assets'
        New-Item -ItemType Directory -Path $assets -Force | Out-Null
        Write-Verbose "Staging in $stage"
        [System.IO.File]::WriteAllText((Join-Path $stage 'AppxManifest.xml'), $manifest, (New-Object System.Text.UTF8Encoding $false))
        Copy-Item -Path (Join-Path $logos '*.png') -Destination $assets

        # resources.pri maps Assets\Square44x44Logo.png to its scale and target-size
        # variants; without it the shell finds no file under the plain name. The config's
        # autoResourcePackage splitting is removed: a sparse package has no resource packs, so
        # every variant must be indexed in the one resources.pri.
        $priConfig = Join-Path $stage 'priconfig.xml'
        & $makepriExe createconfig /cf $priConfig /dq en-US /pv 10.0.0 /o | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "makepri createconfig failed with exit code $LASTEXITCODE" }
        [xml]$cfg = Get-Content -LiteralPath $priConfig -Raw
        foreach ($node in @($cfg.SelectNodes('//packaging'))) { [void]$node.ParentNode.RemoveChild($node) }
        $cfg.Save($priConfig)
        & $makepriExe new /pr $stage /cf $priConfig /mn (Join-Path $stage 'AppxManifest.xml') /of (Join-Path $stage 'resources.pri') /o | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "makepri new failed with exit code $LASTEXITCODE" }
        Remove-Item -LiteralPath $priConfig

        New-Item -ItemType Directory -Path (Split-Path -Parent $outputFull) -Force | Out-Null
        if (Test-Path -LiteralPath $outputFull) {
            Remove-Item -LiteralPath $outputFull -Force
        }
        # /nv: a sparse package names an executable that is not inside it, which the
        # semantic validation would reject.
        & $makeappxExe pack /d $stage /p $outputFull /nv
        if ($LASTEXITCODE -ne 0) {
            throw "makeappx pack failed with exit code $LASTEXITCODE"
        }
        Write-Verbose "Built $outputFull (unsigned)"
        Get-Item -LiteralPath $outputFull
    }
    finally {
        if (Test-Path -LiteralPath $stage) {
            Remove-Item -LiteralPath $stage -Recurse -Force
        }
    }
}
