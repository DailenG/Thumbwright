# Thumbwright MSI (WiX v7)

`Package.wxs` is a per-machine, x64 Windows Installer package (`Program Files\Thumbwright`). Build it with `scripts/release.ps1` (full release: build, sign, package, sign) or directly with `packaging/wix/build-msi.ps1` (package a directory of binaries). The installer UI needs the `WixToolset.UI.wixext` extension in the same version as the `wix` CLI; `build-msi.ps1` adds it to the per-user wix cache (`%USERPROFILE%\.wix`, nothing system-wide) when it is missing, or do it once by hand with `wix extension add -g WixToolset.UI.wixext/7.0.0`.

## Contents

`thumbwright_shell.dll`, `tw-worker.exe`, `twctl.exe`, `Thumbwright.msix` (the sparse package for the Windows 11 menu, see below), `LICENSE`, `THIRD_PARTY_LICENSES.md`, and `thumbwright-settings.exe` when it is in the source directory (the `IncludeSettings` WiX define; `build-msi.ps1` sets it automatically, `-NoSettings` suppresses it), together with its Start Menu shortcut and the exit-dialog launch described under "Installer UI and Start Menu". The package never contains `Thumbwright.ini`: its presence next to the binaries is the portable-mode marker (REQ-REG-6), and an installed copy must not be portable (`build-msi.ps1` refuses a source directory that has one).

The MSI has no Registry table rows by default. Every shell registry key is written by the DLL's own journaled engine, so the MSI owns files and the engine owns HKLM state (including `HKLM\Software\Thumbwright\Registration`, the engine's journal). The only Registry rows are the optional machine-wide defaults written when `MENU` or `KIT` is passed (see "Silent presets").

## Installer UI and Start Menu

- **UI.** `WixUI_Minimal` from `WixToolset.UI.wixext`: one welcome and licence page, progress, and an exit dialog. The licence page shows `license-summary.rtf` (a short GPL-3.0 summary, not the full text; the full text is installed as `LICENSE`). The page's checkbox only lets setup continue, and the text says so, because the GPL needs no acceptance to use the software. The dialogs are in the UI sequence only, so `/qn`, `/qb` and Chocolatey installs show nothing.
- **Exit dialog checkbox "Open Thumbwright Workbench"** (ticked by default, new installs only, and only when `thumbwright-settings.exe` is in the package). Finish runs the immediate custom action `LaunchWorkbench`, which starts `"[WindowsFolder]explorer.exe" "[InstallFolder]thumbwright-settings.exe"`. The launch goes through `explorer.exe` because of elevation: when `msiexec` is started from an elevated prompt, the installer UI process is elevated, and a Workbench started from it would run elevated and write its settings for the elevated account's `HKCU`, not the signed-in user's. `explorer.exe <file>` hands the request to the already running, unelevated Explorer shell, which starts the Workbench in the user's own session and token. When the UI runs unelevated (the usual double-click on the MSI) there is nothing to de-elevate and the result is the same. [INFERENCE: the de-elevating hand-off is the shell's behaviour and was not exercised on a live install; the dry run checks only that the action and its command line are present.] If Explorer is not running, `explorer.exe` starts the file itself with the installer's token.
- **Start Menu shortcut "Thumbwright Workbench"** in the all-users Programs folder (`ProgramMenuFolder`), target `thumbwright-settings.exe`, icon `thumbwright.ico`. It is a child of the settings file, so it shares that file's component and Windows Installer removes it on uninstall; the non-advertised shortcut starts the exe directly with no repair check. `wix msi validate` raises ICE43 and ICE57 for it (a file-keyed shortcut component in a per-machine package); both are about per-user shortcuts and are suppressed in `scripts/test-install.ps1`.

## Silent presets: `MENU` and `KIT`

Two public, secure properties preset the first run for users who have not saved any settings yet (MSP deployments, Chocolatey). They change nothing for a user who already has settings in `HKCU\Software\Thumbwright`, and install nothing when absent.

| Property | Values | Stored (HKLM, 64-bit) | Effect |
|---|---|---|---|
| `MENU=0` | `0` or `1` (default `1`, nothing written) | `Software\Thumbwright\Defaults\MenuEnabled` = DWORD 0 | The right-click menu is off until the user turns it on. |
| `KIT=<name>` | `photos`, `paperwork`, `design`, `cad`, `thumbnails`, `everything` (case-insensitive; default none) | `Software\Thumbwright\Defaults\StarterKit` = the kit name | The Workbench first run preselects this starter kit. |

```
msiexec /i Thumbwright-0.1.2-x64.msi /qn MENU=0 KIT=photos
choco install thumbwright --params "'/Menu:0 /Kit:photos'"
```

- `Settings::machine_defaults()` (`tw-core/src/config.rs`) reads that key; `Settings::load` uses it when the user's HKCU key is missing or holds no value. `Settings::machine_starter_kit()` returns the kit name for the Workbench.
- Each value is its own Registry row in a component that is installed only when its property selects it, so a plain install writes nothing. The kit value written is a constant per component (six components), never the property text. `KIT` and `MENU` are not formatted into any command line (`test-install.ps1` still checks that no action formats a public property). A value outside the list fails the install with a message (`RejectBadMenu`, `RejectBadKit`).
- Windows Installer removes a component whose condition is no longer true, so repair, modify and upgrade must pass the properties again, otherwise the defaults are removed. Uninstall removes the `Defaults` key (`RemoveRegistryKey`).

## Registration: `regsvr32` custom actions, not `twctl`

Decision (issue #2): the installer runs the DLL's `DllRegisterServer` / `DllUnregisterServer` through `regsvr32.exe` in deferred, non-impersonated (LocalSystem) custom actions.

Why this and not `twctl`:

- `DllRegisterServer` is the path the spec describes (FUNCTIONAL_SPEC.md 4.2, REQ-REG-8) and is the only one with `Scope::Machine` (HKLM + `Shell Extensions\Approved`). `twctl register` is per-user and deliberately refuses to run from an installed copy (REQ-REG-6).
- The registration plan (extension list, classes) lives in the DLL, so the installer cannot drift from it, and the DLL registers exactly what it implements.
- `regsvr32` is on every Windows install and returns non-zero when `DllRegisterServer` fails, so the install fails and rolls back instead of leaving a half-registered product.
- The MSI `SelfReg` table is not used: Microsoft discourages it, and it loads the DLL into the installer process rather than a separate one.

Actions and ordering (verified against the built package by `scripts/test-install.ps1 -DryRun`):

| Action | Runs | Condition | Failure |
|---|---|---|---|
| `RegisterShell` | after `InstallFiles` | `NOT (REMOVE="ALL")` (install, repair, modify) | install fails and rolls back |
| `RegisterShellRollback` | before `RegisterShell` in the rollback script | same | ignored (best effort) |
| `UnregisterShell` | before `RemoveFiles` | `REMOVE="ALL"` (uninstall; the removal of the old product in a major upgrade) | ignored |

- Uninstall unregisters while the DLL is still on disk (`regsvr32 /u` loads it).
- Repair (`msiexec /fa`) re-registers; `DllRegisterServer` first undoes its own previous registration (it reads the journal), so repeats never stack.
- Major upgrade: `MajorUpgrade` schedules `RemoveExistingProducts` right after `InstallValidate`, i.e. before the new files are installed. The old product's `UnregisterShell` runs first, then the new product installs and registers. Same-or-older versions are refused.
- `UnregisterShell` is `Return="ignore"` on purpose: a failure there (for example a corrupt journal, which the engine refuses to guess about) must not make the product impossible to uninstall. The exit code is in the verbose MSI log, and `test-install.ps1` fails on any residue.
- `regsvr32` is the 64-bit one (`[System64Folder]`) so HKLM writes land in the 64-bit registry view the DLL is registered in.
- Deferred actions cannot read session properties at run time; for EXE actions Windows Installer formats `ExeCommand` when it writes the script, so `[InstallFolder]` is already resolved.
- **The install directory is a private property on purpose** (audit P-1, P-2). The directory Id is `InstallFolder`, mixed case: Windows Installer treats an all-upper-case name as a *public* property that `msiexec /i ... INSTALLFOLDER=...` can override, and the value is pasted unescaped into single-quoted strings inside `powershell -Command` (a `'`, `;` and `#` in the folder name ran arbitrary PowerShell as LocalSystem) and decides where the SYSTEM-run `regsvr32` loads the DLL from. A private property cannot be set on the command line, so the install location is always `Program Files\Thumbwright`. Never rename the Id to an upper-case name. As defence in depth, the `RejectUnsafeInstallFolder` error action (after `CostFinalize`) refuses an install directory containing a single quote, which would break the quoting even for a legitimate relocation of Program Files. `test-install.ps1 -DryRun` fails if any directory Id other than `TARGETDIR` is public or any action formats an upper-case `[PROPERTY]`.

Known operational caveat: if Explorer or a `dllhost.exe` thumbnail host has the DLL mapped, Windows Installer cannot delete or replace it until the process exits (the install then reports reboot-required, exit code 3010). `test-install.ps1` stops `dllhost.exe` instances holding the DLL before uninstalling.

## Windows 11 modern menu: the sparse package, provisioned for all users

The Explorer verb needs package identity, which `packaging/msix` supplies as a sparse package (no payload; the install directory is its external location). Decision (issue #19): the MSI registers it for all users with the route Microsoft documents for a machine-wide installer ("Grant package identity by packaging with external location", section "Register the identity package in your installer", Per-Machine):

```
Add-AppxPackage -Stage <install dir>\Thumbwright.msix -ExternalLocation <install dir>
Add-AppxProvisionedPackage -Online -PackagePath <install dir>\Thumbwright.msix -SkipLicense
```

Uninstall is the documented mirror: `Remove-AppxProvisionedPackage` for the provisioned entry, then `Remove-AppxPackage -AllUsers`.

What was considered:

- **Per-user `Add-AppxPackage -Path ... -ExternalLocation ...`, or `PackageManager.AddPackageByUriAsync` with `AddPackageOptions.ExternalLocationUri`**: documented for per-user installs only. It registers for the one user the process runs as. A per-machine MSI custom action runs as LocalSystem, which Windows cannot register a package for (a documented limitation of deployment), so this route would register nothing useful here; running it per user (first run of an app, a per-user custom action) would leave users who never run that without the verb.
- **`Add-AppxProvisionedPackage` alone**: the documentation gives it no external location, so the documented route stages the package with the external location first and provisions the staged package. Newer Windows builds add `-ExternalLocationPath` to `Add-AppxProvisionedPackage` (present on the 10.0.26340 development machine); the package supports Windows 10 19041 and later, so the stage-then-provision route, documented for all of them, is used. The API equivalents are `PackageManager.StagePackageByUriAsync` with `StagePackageOptions.ExternalLocationUri`, `ProvisionPackageForAllUsersAsync`, `DeprovisionPackageForAllUsersAsync` and `RemovePackageAsync(..., RemovalOptions.RemoveForAllUsers)`; PowerShell was chosen because the MSI then needs no helper binary.

Consequences of provisioning:

- **The verb appears at each user's next sign-in.** Provisioned families are registered for a user at logon, before the desktop appears. The installer's custom action is LocalSystem, so nobody, the installing user included, is registered at install time. To get the verb in the current session without signing out: `Add-AppxPackage -RegisterByFamilyName -MainPackage <PackageFamilyName>` (the family name is `Thumbwright.ShellExtension_<publisher id>`; `Get-AppxProvisionedPackage -Online` shows the package name it derives from). `scripts/test-install.ps1` does exactly this to prove the staged package registers.
- **Newer versions need no re-provisioning**: provisioning tracks the package family, and a major upgrade removes the old product (deprovision and remove for all users) before the new one stages and provisions.
- **The actions are idempotent.** The removal actions also run on install and repair, before `InstallFiles`, and clear any earlier registration of the same package, because `Add-AppxPackage` refuses a version that is already registered; repair (`msiexec /fa`) and a same-version reinstall therefore work. On a clean machine they find nothing.
- **Windows may file the verb under "App extensions" or hide it until enabled.** On a 10.0.26340 machine, a newly registered package's verb appeared at the top level only after the user enabled it in the menu's **Customize menu** page (stored as `HKCU\...\Explorer\ContextMenu\Preferences\extensions\<PackageFamilyName>`, `Enabled=1`), and Windows 11 files packaged verbs from other vendors under **App extensions**; users can also reach the full classic menu with Shift + right-click (or Show more options where it still exists). Before it is enabled, Explorer did activate the class and call `GetState`/`GetTitle`/`GetFlags` on the whole tree (all returning success), but showed nothing. This is the user's choice in Windows, so the installer does not write that key.

Live check (2026-10-18, unsigned development registration, Developer Mode, 10.0.26340): after `Add-AppxPackage -Register` with the external location and an Explorer restart, Explorer started `DllHost /Processid:{3911672C-...}` with `thumbwright_shell.dll` and queried all 29 child commands; after enabling the extension in Customize menu, "Thumbwright" showed at the top level of the Windows 11 menu next to other packaged extensions. Out of process, `Convert into > BMP` through the packaged class returned `S_OK` and `twctl` wrote `sample.bmp` (300x200).

How the custom actions are built (`Package.wxs`):

| Action | Runs | Condition | Does |
|---|---|---|---|
| `DeprovisionPackage` | before `RemovePackage` | always (uninstall, the removal of the old product in a major upgrade, install and repair) | `Get-AppxProvisionedPackage -Online \| Where-Object DisplayName -eq '<name>' \| Remove-AppxProvisionedPackage -Online` |
| `RemovePackage` | before `UnregisterShell` | always | `Get-AppxPackage -AllUsers -Name '<name>' \| Remove-AppxPackage -AllUsers` |
| `StagePackage` | after `RegisterShell` | `NOT (REMOVE="ALL")` | `Add-AppxPackage -Stage '<install dir>\Thumbwright.msix' -ExternalLocation '<install dir>'` |
| `ProvisionPackage` | after `StagePackage` | same | `Add-AppxProvisionedPackage -Online -PackagePath '<install dir>\Thumbwright.msix' -SkipLicense` |
| `StagePackageRollback`, `ProvisionPackageRollback` | just before the action they undo | same | the `RemovePackage` and `DeprovisionPackage` commands |

All are deferred, non-impersonated (LocalSystem) EXE actions with `Return="ignore"`, except the two rollback actions (rollback scripts run in reverse order, so provisioning is undone before staging).

- Windows PowerShell 5.1, the 64-bit one (`[System64Folder]WindowsPowerShell\v1.0\powershell.exe`), `-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "<one command line>"`. No script file ships, so nothing has to be signed or pass the execution policy (execution policy governs script files, not `-Command`). The commands have no braces and no inner double quotes, which Windows Installer's formatted text and the command line would otherwise mangle.
- Each command is its own action because the `CustomAction.Target` column holds 255 characters (a longer value draws ICE03 warnings, which an earlier single-script version produced). `test-install.ps1 -DryRun` checks the length, the quoting and the ordering.
- The package is removed first on uninstall (deprovision, remove for all users, then `regsvr32 /u`, then the files). Removing a package shuts down its running processes, which should include the package's COM surrogate that would otherwise hold `thumbwright_shell.dll` open when the files are deleted [INFERENCE: not observed, the MSI could not be run unelevated]; if a handle survives, Windows Installer's usual reboot-required handling applies.
- Failure is deliberately not fatal. The thumbnail provider is the product and the verb an addition; the exit codes are in the verbose MSI log, and `test-install.ps1` fails when a signed release did not provision. An unsigned development package (`release.ps1 -SkipSign`) cannot be registered at all, and its MSI must still install.
- The package name the removal actions use is read from `Thumbwright.msix` by `build-msi.ps1` and passed as the `PackageName` WiX define, so it is written down in one place (`release.ps1`).

Identity and signing (REQ-REG-9, fresh identity): the package `Identity/@Publisher` must equal the subject of the certificate that signs it, or registration fails. `release.ps1` therefore signs the binaries first, reads the subject from the signed DLL's certificate, rebuilds `thumbwright-settings.exe` with that publisher embedded as an `<msix publisher packageName applicationId>` element in its manifest (`crates/tw-settings/build.rs`, environment `TW_MSIX_PUBLISHER`; absent in ordinary builds, which run unpackaged), builds `Thumbwright.msix` with `packaging/msix/build.ps1 -Publisher <subject>`, signs it with the same method, and checks that the signer subject in `AppxSignature.p7x` equals the manifest Publisher. The package's single `Application` names `thumbwright-settings.exe`; the verb is declared with `windows.fileExplorerContextMenus` and the class behind it, `CLSID_EXPLORER_COMMAND` in `thumbwright_shell.dll`, with `com:SurrogateServer` (the CLSID is read from `crates/tw-core/src/ids.rs`). A certificate chained to a publicly trusted root (Azure Trusted Signing) needs no certificate installed on the target; a self-signed one would have to be in `LocalMachine\TrustedPeople`.

## Testing

- `scripts/test-install.ps1 -DryRun` (no elevation): builds a throw-away MSI (or inspects `-Msi`), checks its File/Component/CustomAction/InstallExecuteSequence tables through the Windows Installer API (including the package actions' flags, command text and ordering), the installer UI (dialogs, UI sequence, GPL-3.0 summary, exit-dialog launch), the Start Menu shortcut (Shortcut table, target, icon, owning component) and the `MENU`/`KIT` presets (secure properties, conditional Registry rows, rejection actions), extracts it with `msiexec /a`, inspects the extracted `Thumbwright.msix` (identity, Application executable, `CLSID_EXPLORER_COMMAND` in the verb and the COM class, no payload besides assets), and runs ICE validation (minus ICE43 and ICE57, see above).
- `scripts/test-install.ps1` (elevated, on a test machine): install, upgrade, repair and uninstall with before/after `reg export` diffs of the HKLM and HKCU class, `Approved` and product keys (C6). For a signed package it also checks that the sparse package is provisioned after install (and that the staged copy registers for the current user), and that it is neither provisioned nor registered for any user after uninstall.
- `cargo test -p tw-core machine_scope` runs the `Scope::Machine` plan (Approved entries on) against an HKCU sandbox for install, upgrade, repair and uninstall with a zero-diff check.

## WiX licensing (v7)

WiX v7 is MS-RL licensed source; the prebuilt binaries (`wix.exe`, the NuGet packages) are offered under the Open Source Maintenance Fee (OSMF) EULA v1.1 and require explicit acceptance, enforced from v7: `wix eula accept wix7` once per user/machine, or `-acceptEula wix7` per invocation (`build-msi.ps1 -AcceptWixEula`). The fee applies only to users with revenue-generating use and annual gross revenue of US$10,000 or more; below that, or if you pay the maintainers separately, you are exempt. Organizations above the threshold sponsor the `wixtoolset` GitHub organization. The fee is not a license fee: the source stays MS-RL, and self-compiled binaries are not subject to the EULA. See <https://docs.firegiant.com/wix/osmf/> and <https://github.com/wixtoolset/wix/blob/main/OSMFEULA.txt>.

WiX is build tooling here: it is not linked into or shipped with Thumbwright, and the generated MSI contains only this package's own tables and files (no WiX custom-action DLLs; the package uses only built-in Windows Installer actions plus `regsvr32.exe` and `powershell.exe`).
