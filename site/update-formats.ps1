<#
.SYNOPSIS
    Embeds the format table from `twctl formats --json` into site/index.html.

.DESCRIPTION
    The website's "Does it support my file type?" box has no server: its data is a JavaScript
    array between the markers /*FORMATS:BEGIN*/ and /*FORMATS:END*/ in index.html. Run this
    script after the format table changes (crates/tw-core/src/formats/table.rs) and commit the
    result.

    Each row is [ext, description, category, source, thumbnail 0|1, convertible 0|1, os codec],
    with the extension written without its dot.

    One correction is applied on top of the table: the worker routes avif and avifs through the
    OS imaging codecs only (WIC_ONLY in crates/tw-core/src/decode/route.rs), so they need the
    Microsoft AV1 extension even though the table's os_codec column says None. They are marked
    WicAv1 here so the page tells the truth. Remove the override once the table is fixed.

.PARAMETER Twctl
    twctl.exe to ask. Default: the installed copy in Program Files, then target\release.

.EXAMPLE
    .\site\update-formats.ps1
#>
[CmdletBinding()]
param([string]$Twctl)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Resolve-Path (Join-Path $PSScriptRoot '..')
if (-not $Twctl) {
    $candidates = @(
        (Join-Path $env:ProgramFiles 'Thumbwright\twctl.exe'),
        (Join-Path $root 'target\release\twctl.exe'),
        (Join-Path $root 'target\debug\twctl.exe')
    )
    $Twctl = $candidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $Twctl) { throw 'twctl.exe not found; pass -Twctl.' }
}

$rows = (& $Twctl formats --json) -join "`n" | ConvertFrom-Json
if (-not $rows -or $rows.Count -lt 300) { throw "Unexpected format list ($($rows.Count) rows)." }

function Quote([string]$s) { ConvertTo-Json -InputObject $s -Compress }

$wicAv1 = @('avif', 'avifs')
$items = foreach ($r in ($rows | Sort-Object ext)) {
    $ext = $r.ext.TrimStart('.').ToLowerInvariant()
    $codec = $r.os_codec
    if ($wicAv1 -contains $ext) { $codec = 'WicAv1' }
    $t = if ($r.thumbnail) { 1 } else { 0 }
    $c = if ($r.convertible) { 1 } else { 0 }
    '[{0},{1},{2},{3},{4},{5},{6}]' -f (Quote $ext), (Quote $r.description), (Quote $r.category), (Quote $r.source), $t, $c, (Quote $codec)
}
$data = '[' + ($items -join ",`n") + ']'

$index = Join-Path $PSScriptRoot 'index.html'
$html = [System.IO.File]::ReadAllText($index, [System.Text.Encoding]::UTF8)
$pattern = '(?s)/\*FORMATS:BEGIN\*/.*?/\*FORMATS:END\*/'
if ($html -notmatch $pattern) { throw 'Markers /*FORMATS:BEGIN*/ and /*FORMATS:END*/ not found in index.html.' }
$new = [regex]::Replace($html, $pattern, [System.Text.RegularExpressions.MatchEvaluator] { param($m) "/*FORMATS:BEGIN*/$data/*FORMATS:END*/" })
[System.IO.File]::WriteAllText($index, $new, (New-Object System.Text.UTF8Encoding($false)))
Write-Host ("Embedded {0} formats ({1} with thumbnails) into {2}" -f $rows.Count, @($rows | Where-Object thumbnail).Count, $index)
