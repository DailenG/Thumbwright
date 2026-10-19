# Chocolatey package

Source of the `thumbwright` package on the Chocolatey Community Repository. The package
embeds the signed MSI from the GitHub release; `legal/VERIFICATION.txt` gives its SHA256.

Build (the MSI is not committed):

```
gh release download v0.1.0 -p "*.msi" -D tools
choco pack
```

Install runs `msiexec /i ... /qn` (per-machine, needs elevation); uninstall finds the
product in Programs and Features and runs its `msiexec /x`. For a new version, update
`<version>`, the MSI name in `tools/chocolateyinstall.ps1`, and the release URL and SHA256 in
`legal/VERIFICATION.txt`.
