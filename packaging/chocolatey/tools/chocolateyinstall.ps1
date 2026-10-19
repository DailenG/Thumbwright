$ErrorActionPreference = 'Stop'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

$packageArgs = @{
    packageName    = $env:ChocolateyPackageName
    fileType       = 'msi'
    file64         = Join-Path $toolsDir 'Thumbwright-0.1.0-x64.msi'
    softwareName   = 'Thumbwright'
    silentArgs     = "/qn /norestart /l*v `"$($env:TEMP)\$($env:ChocolateyPackageName).$($env:ChocolateyPackageVersion).MsiInstall.log`""
    validExitCodes = @(0, 3010, 1641)
}

Install-ChocolateyInstallPackage @packageArgs

# The MSI is copied into the Windows Installer cache; the embedded copy is not needed afterwards.
Remove-Item -LiteralPath $packageArgs.file64 -Force -ErrorAction SilentlyContinue
