$ErrorActionPreference = 'Stop'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

# Package parameters become MSI properties that preset the first run for users who have saved no
# settings yet (stored machine-wide by the MSI under HKLM\Software\Thumbwright\Defaults):
#   /Menu:0        start with the right-click menu off (/Menu:1 is the default, the menu on)
#   /Kit:<name>    preselect a starter kit: photos, paperwork, design, cad, thumbnails, everything
# Both are validated here, so nothing but a known constant ever reaches the msiexec command line.
$kits = @('photos', 'paperwork', 'design', 'cad', 'thumbnails', 'everything')
$pp = Get-PackageParameters
$msiProperties = @()
foreach ($name in @($pp.Keys)) {
    $value = [string]$pp[$name]
    switch -Regex ($name) {
        '^Menu$' {
            if ($value -notmatch '^[01]$') { throw "Package parameter /Menu must be 0 or 1, not '$value'." }
            if ($value -eq '0') { $msiProperties += 'MENU=0' }
        }
        '^Kit$' {
            if ($kits -notcontains $value.ToLowerInvariant()) {
                throw "Package parameter /Kit must be one of: $($kits -join ', '). Got '$value'."
            }
            $msiProperties += ('KIT=' + $value.ToLowerInvariant())
        }
        default { Write-Warning "Ignoring unknown package parameter /$name (known: /Menu:0, /Kit:<$($kits -join '|')>)." }
    }
}

$silentArgs = "/qn /norestart /l*v `"$($env:TEMP)\$($env:ChocolateyPackageName).$($env:ChocolateyPackageVersion).MsiInstall.log`""
if ($msiProperties.Count -gt 0) {
    $silentArgs += ' ' + ($msiProperties -join ' ')
    Write-Host "Thumbwright first-run presets: $($msiProperties -join ' ')"
}

$packageArgs = @{
    packageName    = $env:ChocolateyPackageName
    fileType       = 'msi'
    file64         = Join-Path $toolsDir 'Thumbwright-0.1.4-x64.msi'
    softwareName   = 'Thumbwright'
    silentArgs     = $silentArgs
    validExitCodes = @(0, 3010, 1641)
}

Install-ChocolateyInstallPackage @packageArgs

# The MSI is copied into the Windows Installer cache; the embedded copy is not needed afterwards.
Remove-Item -LiteralPath $packageArgs.file64 -Force -ErrorAction SilentlyContinue
