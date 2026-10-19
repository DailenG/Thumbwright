$ErrorActionPreference = 'Stop'

$keys = @(Get-UninstallRegistryKey -SoftwareName 'Thumbwright')
if ($keys.Count -eq 0) {
    Write-Warning 'Thumbwright is not listed in Programs and Features; nothing to uninstall.'
    return
}
if ($keys.Count -gt 1) {
    Write-Warning "$($keys.Count) matches for 'Thumbwright'; not uninstalling. Remove it from Settings > Apps instead:"
    $keys | ForEach-Object { Write-Warning "- $($_.DisplayName)" }
    return
}

Uninstall-ChocolateyPackage `
    -PackageName $env:ChocolateyPackageName `
    -FileType 'msi' `
    -SilentArgs "$($keys[0].PSChildName) /qn /norestart /l*v `"$($env:TEMP)\$($env:ChocolateyPackageName).$($env:ChocolateyPackageVersion).MsiUninstall.log`"" `
    -ValidExitCodes @(0, 3010, 1605, 1614, 1641)
