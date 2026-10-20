# Chocolatey package

Source of the `thumbwright` package on the Chocolatey Community Repository. The package
embeds the signed MSI from the GitHub release; `legal/VERIFICATION.txt` gives its SHA256.

Build (the MSI is not committed):

```
gh release download v0.1.4 -p "*.msi" -D tools
choco pack
```

Install runs `msiexec /i ... /qn` (per-machine, needs elevation; the installer window only
appears for an interactive double-click of the MSI, never here); uninstall finds the
product in Programs and Features and runs its `msiexec /x`.

## Package parameters

`tools/chocolateyinstall.ps1` turns two optional package parameters into MSI properties that
preset the first run machine-wide (`HKLM\Software\Thumbwright\Defaults`, read by
`Settings::machine_defaults` for users who have saved nothing yet):

| Parameter | MSI property | Effect |
|---|---|---|
| `/Menu:0` | `MENU=0` | right-click menu off at first run (`/Menu:1` is the default) |
| `/Kit:<name>` | `KIT=<name>` | starter kit `photos`, `paperwork`, `design`, `cad`, `thumbnails` or `everything` |

```
choco install thumbwright --params "'/Menu:0 /Kit:photos'"
```

Values are validated in the script before they reach `msiexec`; unknown parameters give a
warning, bad values stop the install. The same properties work with plain `msiexec`, see
`packaging/wix/README.md`.

## New version checklist

Update `<version>`, the MSI name in `tools/chocolateyinstall.ps1`, and the release URL, MSI
name and SHA256 in `legal/VERIFICATION.txt` (the SHA256 is in the release's `SHA256SUMS.txt`).
Check the listing copy (`summary`, description opener, Notes) against the current menu behaviour.
