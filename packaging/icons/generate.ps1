<#
.SYNOPSIS
    Regenerates the product icons from thumbwright.svg.

.DESCRIPTION
    Renders thumbwright.svg with twctl (resvg) into:
      thumbwright.ico       16, 20, 24, 32, 40, 48, 64 and 256 px (PNG-compressed entries),
                            used for Programs and Features (packaging/wix/Package.wxs)
      thumbwright.res       the same images as a compiled Win32 resource (RT_ICON entries and
                            RT_GROUP_ICON 1), which the crates' build.rs pass straight to the
                            linker so the executables and the DLL carry the icon without a
                            resource compiler; the Windows 11 menu verb points at it (GetIcon)
      msix/*.png            the sparse package's logos at every scale and target size the
                            shell asks for (packaging/msix/build.ps1 indexes them with makepri)
      ../../site/img/icon.png   256 px, the Chocolatey and web icon
    The outputs are committed; run this after changing the SVG.

.PARAMETER Twctl
    twctl.exe to render with. Default: target\debug\twctl.exe, then target\release\twctl.exe.

.EXAMPLE
    cargo build -p twctl; .\packaging\icons\generate.ps1
#>
[CmdletBinding()]
param([string]$Twctl)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$here = $PSScriptRoot
$root = Resolve-Path (Join-Path $here '..\..')
$svg = Join-Path $here 'thumbwright.svg'
if (-not $Twctl) {
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
    $Twctl = @('debug', 'release') | ForEach-Object { Join-Path $targetDir "$_\twctl.exe" } |
        Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $Twctl) { throw 'twctl.exe not found; build it (cargo build -p twctl) or pass -Twctl.' }
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('tw-icons-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
# Small sizes come from hand-drawn pixel-grid sources (thumbwright-16/24/32.svg) when present:
# 16, 24 and 32 render exactly; 20 uses the 24 grid and 40 the master.
function Source([int]$Size) {
    $grid = @{ 16 = 16; 20 = 24; 24 = 24; 32 = 32 }
    if ($grid.ContainsKey($Size)) {
        $p = Join-Path $here ("thumbwright-{0}.svg" -f $grid[$Size])
        if (Test-Path -LiteralPath $p) { return $p }
    }
    return $svg
}
function Render([int]$Size, [string]$Out) {
    & $Twctl convert (Source $Size) $Out --resize "${Size}x${Size}" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "twctl convert failed for $Size px" }
}

try {
    # .ico: ICONDIR + one ICONDIRENTRY per size, PNG payloads (supported since Vista).
    $sizes = 16, 20, 24, 32, 40, 48, 64, 256
    $images = foreach ($s in $sizes) {
        $p = Join-Path $tmp "$s.png"
        Render $s $p
        , [System.IO.File]::ReadAllBytes($p)
    }
    $ms = New-Object System.IO.MemoryStream
    $w = New-Object System.IO.BinaryWriter $ms
    $w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
        $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
        $w.Write([uint16]1); $w.Write([uint16]32)
        $w.Write([uint32]$images[$i].Length); $w.Write([uint32]$offset)
        $offset += $images[$i].Length
    }
    foreach ($img in $images) { $w.Write($img) }
    $w.Flush()
    [System.IO.File]::WriteAllBytes((Join-Path $here 'thumbwright.ico'), $ms.ToArray())

    # .res: the empty leading entry, one RT_ICON (3) per image with ids 1..n, then
    # RT_GROUP_ICON (14) id 1, which is the icon Explorer shows for the file.
    $res = New-Object System.IO.MemoryStream
    $rw = New-Object System.IO.BinaryWriter $res
    function Write-Resource([uint16]$Type, [uint16]$Id, [uint16]$Flags, [byte[]]$Data) {
        $rw.Write([uint32]$Data.Length); $rw.Write([uint32]32)
        $rw.Write([uint16]0xFFFF); $rw.Write($Type); $rw.Write([uint16]0xFFFF); $rw.Write($Id)
        $rw.Write([uint32]0); $rw.Write($Flags); $rw.Write([uint16]0x0409)
        $rw.Write([uint32]0); $rw.Write([uint32]0)
        $rw.Write($Data)
        while ($res.Length % 4) { $rw.Write([byte]0) }
    }
    $rw.Write([uint32]0); $rw.Write([uint32]32)
    $rw.Write([uint16]0xFFFF); $rw.Write([uint16]0); $rw.Write([uint16]0xFFFF); $rw.Write([uint16]0)
    $rw.Write([byte[]]::new(16))
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        Write-Resource 3 ($i + 1) 0x1010 $images[$i]
    }
    $group = New-Object System.IO.MemoryStream
    $gw = New-Object System.IO.BinaryWriter $group
    $gw.Write([uint16]0); $gw.Write([uint16]1); $gw.Write([uint16]$sizes.Count)
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
        $gw.Write([byte]$dim); $gw.Write([byte]$dim); $gw.Write([byte]0); $gw.Write([byte]0)
        $gw.Write([uint16]1); $gw.Write([uint16]32)
        $gw.Write([uint32]$images[$i].Length); $gw.Write([uint16]($i + 1))
    }
    $gw.Flush()
    Write-Resource 14 1 0x1030 $group.ToArray()
    $rw.Flush()
    [System.IO.File]::WriteAllBytes((Join-Path $here 'thumbwright.res'), $res.ToArray())

    # MSIX logos: base sizes at scales 100-400, plus the app-list target sizes (plain and
    # unplated) that taskbar, Settings and the extension lists pick from.
    $msix = Join-Path $here 'msix'
    if (Test-Path -LiteralPath $msix) { Remove-Item -LiteralPath $msix -Recurse -Force }
    New-Item -ItemType Directory -Path $msix | Out-Null
    $bases = @{ 'StoreLogo' = 50; 'Square44x44Logo' = 44; 'Square150x150Logo' = 150 }
    foreach ($name in $bases.Keys) {
        foreach ($scale in 100, 125, 150, 200, 400) {
            Render ([int][math]::Round($bases[$name] * $scale / 100)) (Join-Path $msix "$name.scale-$scale.png")
        }
    }
    foreach ($t in 16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256) {
        Render $t (Join-Path $msix "Square44x44Logo.targetsize-$t.png")
        Copy-Item (Join-Path $msix "Square44x44Logo.targetsize-$t.png") (Join-Path $msix "Square44x44Logo.targetsize-${t}_altform-unplated.png")
        Copy-Item (Join-Path $msix "Square44x44Logo.targetsize-$t.png") (Join-Path $msix "Square44x44Logo.targetsize-${t}_altform-lightunplated.png")
    }

    Render 256 (Join-Path $root 'site\img\icon.png')
}
finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
Get-ChildItem -LiteralPath $here -Recurse -File | Measure-Object -Property Length -Sum |
    ForEach-Object { "{0} files, {1:N0} bytes under packaging\icons" -f $_.Count, $_.Sum }
