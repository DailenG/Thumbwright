<#
.SYNOPSIS
    Regenerates the installer artwork from dialog.svg and banner.svg.

.DESCRIPTION
    Renders the two SVGs with twctl (resvg) at WiX UI's exact sizes and saves them as the
    24-bit BMPs WiX expects:
      dialog.bmp   493x312, WixUIDialogBmp (welcome and exit pages)
      banner.bmp   493x58,  WixUIBannerBmp (inner pages)
    The outputs are committed; run this after changing either SVG.

.PARAMETER Twctl
    twctl.exe to render with. Default: target\debug\twctl.exe, then target\release\twctl.exe.
#>
[CmdletBinding()]
param([string]$Twctl)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$here = $PSScriptRoot
$root = Resolve-Path (Join-Path $here '..\..')
if (-not $Twctl) {
    $Twctl = @('target\debug\twctl.exe', 'target\release\twctl.exe') |
        ForEach-Object { Join-Path $root $_ } |
        Where-Object { Test-Path -LiteralPath $_ } |
        Select-Object -First 1
    if (-not $Twctl) { throw 'twctl.exe not found; build it or pass -Twctl.' }
}

foreach ($art in @(@{ Name = 'dialog'; W = 493; H = 312 }, @{ Name = 'banner'; W = 493; H = 58 })) {
    $png = Join-Path ([System.IO.Path]::GetTempPath()) ("tw-$($art.Name)-" + [guid]::NewGuid().ToString('N') + '.png')
    & $Twctl convert (Join-Path $here "$($art.Name).svg") $png --resize "$($art.W)x$($art.H)" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "twctl convert failed for $($art.Name).svg" }
    $src = [System.Drawing.Image]::FromFile($png)
    try {
        $bmp = New-Object System.Drawing.Bitmap $art.W, $art.H, ([System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.Clear([System.Drawing.Color]::White)
        $g.DrawImage($src, 0, 0, $art.W, $art.H)
        $g.Dispose()
        $out = Join-Path $here "$($art.Name).bmp"
        $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Bmp)
        $bmp.Dispose()
        '{0}: {1}x{2}, {3:N0} bytes' -f (Split-Path -Leaf $out), $art.W, $art.H, (Get-Item $out).Length
    } finally {
        $src.Dispose()
        Remove-Item -LiteralPath $png -Force -ErrorAction SilentlyContinue
    }
}
