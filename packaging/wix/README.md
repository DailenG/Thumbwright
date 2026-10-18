# Thumbwright MSI (WiX v7)

`Package.wxs` is a per-machine, x64 Windows Installer package (`Program Files\Thumbwright`). Build it with `scripts/release.ps1` (full release: build, sign, package, sign) or directly with `packaging/wix/build-msi.ps1` (package a directory of binaries).

## Contents

`thumbwright_shell.dll`, `tw-worker.exe`, `twctl.exe`, `LICENSE`, `THIRD_PARTY_LICENSES.md`, and `thumbwright-settings.exe` when it is in the source directory (the `IncludeSettings` WiX define; `build-msi.ps1` sets it automatically, `-NoSettings` suppresses it). The package never contains `Thumbwright.ini`: its presence next to the binaries is the portable-mode marker (REQ-REG-6), and an installed copy must not be portable (`build-msi.ps1` refuses a source directory that has one).

The MSI has no Registry table rows. Every shell registry key is written by the DLL's own journaled engine, so the MSI owns files and the engine owns HKLM state (including `HKLM\Software\Thumbwright\Registration`, the engine's journal).

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
- Deferred actions cannot read session properties at run time; for EXE actions Windows Installer formats `ExeCommand` when it writes the script, so `[INSTALLFOLDER]` is already resolved.

Known operational caveat: if Explorer or a `dllhost.exe` thumbnail host has the DLL mapped, Windows Installer cannot delete or replace it until the process exits (the install then reports reboot-required, exit code 3010). `test-install.ps1` stops `dllhost.exe` instances holding the DLL before uninstalling.

## Testing

- `scripts/test-install.ps1 -DryRun` (no elevation): builds a throw-away MSI (or inspects `-Msi`), checks its File/Component/CustomAction/InstallExecuteSequence tables through the Windows Installer API, extracts it with `msiexec /a`, and runs ICE validation.
- `scripts/test-install.ps1` (elevated, on a test machine): install, upgrade, repair and uninstall with before/after `reg export` diffs of the HKLM and HKCU class, `Approved` and product keys (C6).
- `cargo test -p tw-core machine_scope` runs the `Scope::Machine` plan (Approved entries on) against an HKCU sandbox for install, upgrade, repair and uninstall with a zero-diff check.

## WiX licensing (v7)

WiX v7 is MS-RL licensed source; the prebuilt binaries (`wix.exe`, the NuGet packages) are offered under the Open Source Maintenance Fee (OSMF) EULA v1.1 and require explicit acceptance, enforced from v7: `wix eula accept wix7` once per user/machine, or `-acceptEula wix7` per invocation (`build-msi.ps1 -AcceptWixEula`). The fee applies only to users with revenue-generating use and annual gross revenue of US$10,000 or more; below that, or if you pay the maintainers separately, you are exempt. Organizations above the threshold sponsor the `wixtoolset` GitHub organization. The fee is not a license fee: the source stays MS-RL, and self-compiled binaries are not subject to the EULA. See <https://docs.firegiant.com/wix/osmf/> and <https://github.com/wixtoolset/wix/blob/main/OSMFEULA.txt>.

WiX is build tooling here: it is not linked into or shipped with Thumbwright, and the generated MSI contains only this package's own tables and files (no WiX custom-action DLLs; the package uses only built-in Windows Installer actions plus `regsvr32.exe`).
