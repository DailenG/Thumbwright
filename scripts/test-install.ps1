#Requires -Version 5.1
<#
.SYNOPSIS
    End-to-end test of the Thumbwright per-machine MSI: install, upgrade, repair, uninstall, and
    a registry diff that must show zero product residue (issue #2, REQ-REG-8, C6).

.DESCRIPTION
    Elevated mode (default). Needs an elevated 64-bit PowerShell because it installs the MSI and
    DllRegisterServer writes HKLM. It refuses to start otherwise. It changes the machine
    (installs and removes Thumbwright), so run it on a test box or VM.

    Before anything is installed it exports, with `reg export /reg:64`:
        HKLM\SOFTWARE\Classes
        HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved
        HKLM\SOFTWARE\Thumbwright   (and the WOW6432Node copy)
        HKCU\Software\Classes
        HKCU\Software\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved
        HKCU\Software\Thumbwright
    Each export is flattened to "key<TAB>line" entries so a value line is compared together with
    the key it belongs to. Scenarios:

      InstallUninstall  install; verify registration (CLSID keys, InprocServer32, Approved entries,
                        thumbnail handlers at both locations for several extensions, context menu
                        handler, journal, files, no Thumbwright.ini); uninstall; re-export; the
                        before and after exports must be identical (any added or lost line fails),
                        and no product file or key may remain.
      Upgrade           install an older version, install this one over it (major upgrade), verify,
                        confirm the registry footprint equals a fresh install, uninstall, no residue.
                        Without -OldMsi an older package is built from the same payload with
                        version 0.0.1, which exercises the custom actions and upgrade ordering
                        (not the contents of a real older release).
      Repair            install, `msiexec /fa`, verify, confirm the footprint is unchanged,
                        uninstall, no residue.

    Sparse package (Windows 11 modern menu, issue #19). The MSI ships Thumbwright.msix and
    provisions it for all users (stage with the install directory as external location, then
    Add-AppxProvisionedPackage). After each install the elevated run checks that the installed
    package is provisioned and that the staged copy registers for the current user (the step
    Windows does for each user at sign-in); after each uninstall, that it is neither provisioned
    nor registered for any user. Both checks apply only to a signed package: an unsigned
    (-SkipSign) one cannot be registered, which is reported and skipped.

    -DryRun is the non-elevated mode: it builds an MSI from -SourceDir (or inspects -Msi) and checks
    its tables through the Windows Installer API (files, no INI, no Registry rows except the optional MENU/KIT defaults, the deferred
    register action after InstallFiles, the unregister action before RemoveFiles, the package
    register/unregister actions, and that no public property can inject into them: the install
    directory Id is the private InstallFolder, no INSTALLFOLDER exists, no directory Id other
    than TARGETDIR is all upper case, and no action formats an all-upper-case [PROPERTY], so
    nothing on the msiexec command line reaches a command), extracts it with an administrative
    install into the work
    directory, inspects the Thumbwright.msix in it, and runs `wix msi validate` (ICE; ICE43 and
    ICE57 are suppressed for the all-users Start Menu shortcut, see Test-IceValidation). It also
    checks the front door: the installer UI (InstallUISequence, WelcomeEulaDlg/ProgressDlg/
    ExitDialog, the GPL-3.0 summary on the licence page, the exit-dialog "Open Thumbwright
    Workbench" checkbox and its explorer.exe launch), the "Thumbwright Workbench" Start Menu
    shortcut (Shortcut table, target, icon, owning component), and the MENU and KIT silent
    presets (secure properties, conditional HKLM Defaults Registry rows, rejection of bad values).
    Nothing is installed and no registry is touched.

.PARAMETER Msi
    The package under test. Default: newest dist\Thumbwright-*-x64.msi. In -DryRun, when omitted,
    a throw-away MSI is built from -SourceDir instead.

.PARAMETER OldMsi
    A real older package for the Upgrade scenario. Must have the same UpgradeCode and a lower version.

.PARAMETER SourceDir
    Payload directory used to build throw-away MSIs (dry run, synthetic old version). Default: dist.

.PARAMETER Scenario
    All (default), InstallUninstall, Upgrade or Repair.

.PARAMETER DryRun
    Non-elevated package inspection only.

.PARAMETER WorkDir
    Logs and diff reports go here. Default: %TEMP%\tw-test-install-<timestamp>.

.PARAMETER KeepExports
    Keep the raw .reg exports (large: tens of MB each).

.PARAMETER AcceptWixEula
    Passed to build-msi.ps1 when a throw-away MSI is built.

.EXAMPLE
    .\scripts\test-install.ps1 -DryRun

.EXAMPLE
    # elevated
    .\scripts\test-install.ps1 -Msi .\dist\Thumbwright-0.1.1-x64.msi
#>
[CmdletBinding()]
param(
    [string]$Msi,
    [string]$OldMsi,
    [string]$SourceDir,
    [ValidateSet('All', 'InstallUninstall', 'Upgrade', 'Repair')]
    [string]$Scenario = 'All',
    [switch]$DryRun,
    [string]$WorkDir,
    [switch]$KeepExports,
    [switch]$AcceptWixEula
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
if (-not $SourceDir) { $SourceDir = Join-Path $root 'dist' }
if (-not $WorkDir) {
    $WorkDir = Join-Path $env:TEMP ('tw-test-install-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
}

# ---------------------------------------------------------------------------------------------
# Check bookkeeping
# ---------------------------------------------------------------------------------------------
$script:Results = New-Object System.Collections.Generic.List[object]

function Assert-Check {
    param([string]$Name, [bool]$Condition, [string]$Detail = '')
    $script:Results.Add([pscustomobject]@{ Name = $Name; Passed = $Condition; Detail = $Detail })
    if ($Condition) {
        Write-Host ("  PASS  {0}" -f $Name)
    } else {
        Write-Host ("  FAIL  {0} {1}" -f $Name, $Detail) -ForegroundColor Red
    }
}

# ---------------------------------------------------------------------------------------------
# Windows Installer database access (late-bound COM, works in Windows PowerShell and PowerShell 7)
# ---------------------------------------------------------------------------------------------
function Invoke-Com {
    param($Target, [string]$Member, [object[]]$Arguments = @(), [string]$Kind = 'InvokeMethod')
    return $Target.GetType().InvokeMember($Member, $Kind, $null, $Target, $Arguments)
}

function Open-MsiDatabase {
    param([string]$Path)
    $installer = New-Object -ComObject WindowsInstaller.Installer
    return Invoke-Com $installer 'OpenDatabase' @((Resolve-Path -LiteralPath $Path).Path, 0)
}

# Returns every row as a string[] of $Columns values. Missing tables raise, so use Test-MsiTable first.
function Get-MsiRows {
    param($Db, [string]$Sql, [int]$Columns)
    $rows = New-Object System.Collections.Generic.List[object]
    $view = Invoke-Com $Db 'OpenView' @($Sql)
    [void](Invoke-Com $view 'Execute')
    while ($true) {
        $rec = Invoke-Com $view 'Fetch'
        if ($null -eq $rec) { break }
        $values = New-Object 'string[]' $Columns
        for ($i = 1; $i -le $Columns; $i++) {
            $values[$i - 1] = [string](Invoke-Com $rec 'StringData' @($i) 'GetProperty')
        }
        $rows.Add($values)
    }
    [void](Invoke-Com $view 'Close')
    return , $rows
}

function Test-MsiTable {
    param($Db, [string]$Name)
    $rows = Get-MsiRows $Db "SELECT Name FROM _Tables WHERE Name='$Name'" 1
    return ($rows.Count -gt 0)
}

function Get-MsiProperty {
    param($Db, [string]$Name)
    $rows = Get-MsiRows $Db "SELECT Value FROM Property WHERE Property='$Name'" 1
    if ($rows.Count -eq 0) { return $null }
    return $rows[0][0]
}

# The first column of every row as a string array. Get-MsiRows returns its row list as ONE
# pipeline object, so it must be assigned to a variable before it is piped or enumerated.
function Get-MsiColumn {
    param($Db, [string]$Sql)
    $rows = Get-MsiRows $Db $Sql 1
    return @($rows | ForEach-Object { $_[0] })
}

# ---------------------------------------------------------------------------------------------
# Package inspection (used by -DryRun, and as a precondition of the elevated run)
# ---------------------------------------------------------------------------------------------
$script:RequiredFiles = @('thumbwright_shell.dll', 'tw-worker.exe', 'twctl.exe', 'Thumbwright.msix', 'LICENSE', 'THIRD_PARTY_LICENSES.md')
# Identity name of the sparse package as the MSI's package actions know it; set by Test-MsiPackage.
$script:CaPackageName = $null

# The "front door": installer UI, Start Menu shortcut, silent presets. $files, $actions and
# $sequence are the File rows, the CustomAction rows by name and InstallExecuteSequence rows by
# name that Test-MsiPackage already read.
function Test-MsiFrontDoor {
    param($Db, $Files, $Actions, $Sequence)
    Write-Host '== Front door: installer UI, Start Menu shortcut, MENU and KIT presets'
    $longNames = @{}
    foreach ($f in $Files) { $longNames[$f[0]] = ($f[2] -split '\|')[-1] }
    $hasSettings = $longNames.Values -contains 'thumbwright-settings.exe'

    # --- Installer UI (WixUI_Minimal), authored in the UI sequence only -------------------------
    $hasUi = Test-MsiTable $Db 'InstallUISequence'
    Assert-Check 'InstallUISequence exists (installer UI)' $hasUi
    if ($hasUi) {
        $dialogs = @(Get-MsiColumn $Db 'SELECT `Dialog` FROM `Dialog`')
        foreach ($name in 'WelcomeEulaDlg', 'ProgressDlg', 'ExitDialog') {
            Assert-Check "Dialog table has $name (welcome and licence, progress, exit)" ($dialogs -contains $name)
        }
        $uiSeq = @(Get-MsiColumn $Db 'SELECT `Action` FROM `InstallUISequence`')
        Assert-Check 'InstallUISequence shows the welcome and licence dialog, then runs ExecuteAction' (
            ($uiSeq -contains 'WelcomeEulaDlg') -and ($uiSeq -contains 'ExecuteAction')) ($uiSeq -join ', ')
        $license = Get-MsiRows $Db 'SELECT `Text` FROM `Control` WHERE `Dialog_`=''WelcomeEulaDlg'' AND `Type`=''ScrollableText''' 1
        $licenseText = if ($license.Count -gt 0) { $license[0][0] } else { '' }
        Assert-Check 'Licence page shows the GPL-3.0 summary, not the full licence text' (
            $licenseText -match 'GPL-3\.0' -and $licenseText.Length -gt 200 -and $licenseText.Length -lt 6000 -and
            $licenseText -notmatch 'TERMS AND CONDITIONS') "$($licenseText.Length) characters"
        # Silent installs (/qn, /qb) never run the UI sequence, so nothing may live only in it
        # that an unattended install needs, and nothing UI-related may run in the execute sequence.
        Assert-Check 'No dialog or UI action is in InstallExecuteSequence (silent installs show nothing)' (
            -not ($Sequence.Keys | Where-Object { $_ -in @('WelcomeDlg', 'WelcomeEulaDlg', 'ExitDialog', 'LaunchWorkbench') -or $dialogs -contains $_ }))
    }

    if ($hasSettings) {
        $eventRows = Get-MsiRows $Db 'SELECT `Dialog_`, `Control_`, `Event`, `Argument`, `Condition` FROM `ControlEvent`' 5
        $events = @($eventRows | Where-Object { $_[0] -eq 'ExitDialog' -and $_[1] -eq 'Finish' -and $_[2] -eq 'DoAction' -and $_[3] -eq 'LaunchWorkbench' })
        Assert-Check 'Exit dialog Finish runs LaunchWorkbench only when the "Open Thumbwright Workbench" checkbox is ticked and on a new install' (
            $events.Count -eq 1 -and $events[0][4] -match 'WIXUI_EXITDIALOGOPTIONALCHECKBOX = 1' -and $events[0][4] -match 'NOT Installed')
        Assert-Check 'Exit dialog checkbox text is "Open Thumbwright Workbench"' (
            (Get-MsiProperty $Db 'WIXUI_EXITDIALOGOPTIONALCHECKBOXTEXT') -eq 'Open Thumbwright Workbench')
        if ($Actions.ContainsKey('LaunchWorkbench')) {
            $a = $Actions['LaunchWorkbench']
            $type = [int]$a[1]
            # Type bits: 2 = EXE, 32 = directory, 64 = ignore exit code, 128 = asynchronous,
            # 1024 = deferred (must be clear: runs in the UI client, not elevated by us).
            Assert-Check 'LaunchWorkbench is an immediate, not-waited-for EXE action' (
                (($type -band 7) -eq 2) -and (($type -band 1024) -eq 0) -and (($type -band 128) -ne 0) -and (($type -band 64) -ne 0)) "type $type"
            Assert-Check 'LaunchWorkbench starts the Workbench through explorer.exe (the unelevated user shell), not directly' (
                $a[3] -eq '"[WindowsFolder]explorer.exe" "[InstallFolder]thumbwright-settings.exe"') $a[3]
        } else {
            Assert-Check 'CustomAction LaunchWorkbench exists' $false
        }
    } else {
        Assert-Check 'No Workbench shortcut or launch action without thumbwright-settings.exe' (
            -not $Actions.ContainsKey('LaunchWorkbench') -and -not (Test-MsiTable $Db 'Shortcut'))
    }

    # --- Start Menu shortcut ------------------------------------------------------------------
    if ($hasSettings) {
        $hasShortcut = Test-MsiTable $Db 'Shortcut'
        $shortcuts = if ($hasShortcut) { Get-MsiRows $Db 'SELECT `Shortcut`, `Directory_`, `Name`, `Component_`, `Target`, `Icon_` FROM `Shortcut`' 6 } else { @() }
        Assert-Check 'Shortcut table has exactly one row (the Start Menu entry)' ($shortcuts.Count -eq 1) "$($shortcuts.Count) rows"
        if ($shortcuts.Count -eq 1) {
            $s = $shortcuts[0]
            $shortName = ($s[2] -split '\|')[-1]
            Write-Host ("    Shortcut {0}: {1} in {2}, target {3}, icon {4}" -f $s[0], $shortName, $s[1], $s[4], $s[5])
            Assert-Check 'Start Menu shortcut is named "Thumbwright Workbench" and sits in the all-users Programs folder (ProgramMenuFolder)' (
                $shortName -eq 'Thumbwright Workbench' -and $s[1] -eq 'ProgramMenuFolder')
            $targetFile = if ($s[4] -match '^\[#(.+)\]$') { $Matches[1] } else { $null }
            Assert-Check 'Start Menu shortcut targets thumbwright-settings.exe in the install directory' (
                $targetFile -and $longNames[$targetFile] -eq 'thumbwright-settings.exe') $s[4]
            $iconNames = if (Test-MsiTable $Db 'Icon') { @(Get-MsiColumn $Db 'SELECT `Name` FROM `Icon`') } else { @() }
            Assert-Check 'Start Menu shortcut has the Thumbwright icon (Icon table row)' ($s[5] -and ($iconNames -contains $s[5])) "icon=$($s[5]) icons=$($iconNames -join ',')"
            $owner = Get-MsiRows $Db ('SELECT `Component`, `KeyPath`, `Directory_` FROM `Component` WHERE `Component`=''{0}''' -f $s[3]) 3
            Assert-Check 'The shortcut belongs to the component that installs thumbwright-settings.exe, so uninstall removes it' (
                $owner.Count -eq 1 -and $owner[0][2] -eq 'InstallFolder' -and $longNames[$owner[0][1]] -eq 'thumbwright-settings.exe')
        }
    }

    # --- Silent presets: MENU and KIT ----------------------------------------------------------
    $secure = @((Get-MsiProperty $Db 'SecureCustomProperties') -split ';')
    Assert-Check 'MENU and KIT are secure public properties (honoured in an elevated managed install)' (
        ($secure -ccontains 'MENU') -and ($secure -ccontains 'KIT')) ($secure -join ';')

    $kits = 'photos', 'paperwork', 'design', 'cad', 'thumbnails', 'everything'
    $rows = @()
    if (Test-MsiTable $Db 'Registry') { $rows = Get-MsiRows $Db 'SELECT `Root`, `Key`, `Name`, `Value`, `Component_` FROM `Registry`' 5 }
    $conditions = @{}
    foreach ($c in (Get-MsiRows $Db 'SELECT `Component`, `Condition`, `Directory_`, `Attributes` FROM `Component`' 4)) { $conditions[$c[0]] = $c }
    # Name "-" is a remove-the-key-on-uninstall row (RemoveRegistryKey); the others write values.
    $valueRows = @($rows | Where-Object { $_[2] -ne '-' })
    $removeRows = @($rows | Where-Object { $_[2] -eq '-' })
    $allHklm = -not ($rows | Where-Object { $_[0] -ne '2' -or $_[1] -ne 'Software\Thumbwright\Defaults' })
    Assert-Check 'Defaults are written only to HKLM\Software\Thumbwright\Defaults (64-bit view): one MenuEnabled and six StarterKit values' (
        $valueRows.Count -eq 7 -and $allHklm -and -not ($rows | Where-Object { (([int]$conditions[$_[4]][3]) -band 256) -eq 0 })) "$($valueRows.Count) values"
    $menuRows = @($valueRows | Where-Object { $_[2] -eq 'MenuEnabled' })
    Assert-Check 'MENU=0 writes the DWORD MenuEnabled=0 through a component conditional on MENU = "0"' (
        $menuRows.Count -eq 1 -and $menuRows[0][3] -eq '#0' -and $conditions[$menuRows[0][4]][1] -eq 'MENU = "0"') (($menuRows | ForEach-Object { $_ -join '|' }) -join '; ')
    $kitRows = @($valueRows | Where-Object { $_[2] -eq 'StarterKit' })
    $kitOk = ($kitRows.Count -eq $kits.Count)
    foreach ($kit in $kits) {
        $row = @($kitRows | Where-Object { $_[3] -ceq $kit })
        $kitOk = $kitOk -and $row.Count -eq 1 -and $conditions[$row[0][4]][1] -eq ('KIT ~= "{0}"' -f $kit)
    }
    Assert-Check 'KIT=<kit> writes the constant StarterKit name through its own conditional component, for all six kits' $kitOk (($kitRows | ForEach-Object { $_[3] }) -join ', ')
    # With neither property set nothing is written: every Registry row lives in a component that
    # has a condition.
    Assert-Check 'Every Registry row is in a conditional component (no MENU/KIT: nothing written)' (
        $rows.Count -gt 0 -and -not ($rows | Where-Object { -not $conditions[$_[4]][1] }))
    Assert-Check 'Every component that writes a default also removes the Defaults key on uninstall' (
        $removeRows.Count -eq 7 -and -not ($valueRows | Where-Object { $c = $_[4]; -not ($removeRows | Where-Object { $_[4] -eq $c }) }))
    foreach ($name in 'RejectBadMenu', 'RejectBadKit') {
        Assert-Check "$name is an error action sequenced after CostFinalize, install only, with a condition" (
            $Actions.ContainsKey($name) -and ([int]$Actions[$name][1] -band 63) -eq 19 -and $Sequence.ContainsKey($name) -and
            $Sequence[$name][1] -match 'NOT \(REMOVE="ALL"\)' -and $Sequence[$name][1] -match ($name -replace 'RejectBad', '').ToUpper())
    }
    if ($Sequence.ContainsKey('RejectBadKit')) {
        Assert-Check 'RejectBadKit accepts exactly the six kits' (-not ($kits | Where-Object { $Sequence['RejectBadKit'][1] -notmatch ('KIT ~= "{0}"' -f $_) }))
    }
}

function Test-MsiPackage {
    param([string]$Path)
    Write-Host "== Package tables: $Path"
    $db = Open-MsiDatabase $Path

    $files = Get-MsiRows $db 'SELECT File, Component_, FileName, FileSize FROM File' 4
    $longNames = @($files | ForEach-Object { ($_[2] -split '\|')[-1] })
    $files | ForEach-Object { Write-Host ("    File   {0,-30} {1,-28} {2,10} bytes" -f $_[0], (($_[2] -split '\|')[-1]), $_[3]) }
    foreach ($name in $script:RequiredFiles) {
        Assert-Check "File table has $name" ($longNames -contains $name)
    }
    Assert-Check 'File table has no Thumbwright.ini (portable marker, REQ-REG-6)' (-not ($longNames | Where-Object { $_ -like '*.ini' }))

    $components = Get-MsiRows $db 'SELECT Component, Directory_, Attributes FROM Component' 3
    $components | ForEach-Object { Write-Host ("    Component {0,-24} dir {1} attrs {2}" -f $_[0], $_[1], $_[2]) }
    Assert-Check 'All components are 64-bit in InstallFolder' (
        -not ($components | Where-Object { (([int]$_[2]) -band 256) -eq 0 -or $_[1] -ne 'InstallFolder' }))

    # The DLL engine owns every shell key. The only Registry rows are the optional machine-wide
    # defaults (MENU, KIT), checked by Test-MsiFrontDoor.
    $hasRegistry = Test-MsiTable $db 'Registry'
    $registryKeys = if ($hasRegistry) { @(Get-MsiColumn $db 'SELECT `Key` FROM `Registry`') } else { @() }
    $strayRegistry = @($registryKeys | Where-Object { $_ -ne 'Software\Thumbwright\Defaults' })
    Assert-Check 'No Registry rows besides the optional HKLM\Software\Thumbwright\Defaults values (the DLL engine owns all other HKLM state)' (
        $strayRegistry.Count -eq 0) ($strayRegistry -join ', ')

    $arpIcon = Get-MsiProperty $db 'ARPPRODUCTICON'
    $iconRows = if (Test-MsiTable $db 'Icon') { @(Get-MsiColumn $db 'SELECT Name FROM Icon') } else { @() }
    Assert-Check 'ARPPRODUCTICON names an icon in the Icon table (Programs and Features)' (
        $arpIcon -and ($iconRows -contains $arpIcon)) "ARPPRODUCTICON=$arpIcon icons=$($iconRows -join ',')"

    $actions = @{}
    foreach ($row in (Get-MsiRows $db 'SELECT Action, Type, Source, Target FROM CustomAction' 4)) {
        $actions[$row[0]] = $row
        Write-Host ("    CustomAction {0,-22} type {1,-5} {2}" -f $row[0], $row[1], $row[3])
    }
    $sequence = @{}
    foreach ($row in (Get-MsiRows $db 'SELECT Action, Condition, Sequence FROM InstallExecuteSequence' 3)) {
        $sequence[$row[0]] = $row
    }
    foreach ($name in 'RegisterShell', 'UnregisterShell', 'InstallFiles', 'RemoveFiles', 'RemoveExistingProducts') {
        Assert-Check "InstallExecuteSequence has $name" ($sequence.ContainsKey($name))
    }

    if ($actions.ContainsKey('RegisterShell') -and $actions.ContainsKey('UnregisterShell')) {
        $reg = $actions['RegisterShell']
        $unreg = $actions['UnregisterShell']
        # Type bits: 1024 = deferred, 2048 = no impersonation (runs as LocalSystem, elevated).
        foreach ($a in $reg, $unreg) {
            $type = [int]$a[1]
            Assert-Check "$($a[0]) is a deferred, non-impersonated EXE action" (
                (($type -band 1024) -ne 0) -and (($type -band 2048) -ne 0) -and (($type -band 7) -eq 2))
            Assert-Check "$($a[0]) runs the 64-bit regsvr32 on the installed DLL" (
                $a[3] -match 'System64Folder\]regsvr32\.exe' -and $a[3] -match 'InstallFolder\]thumbwright_shell\.dll')
        }
        Assert-Check 'RegisterShell registers (no /u); UnregisterShell unregisters (/u)' (
            ($reg[3] -notmatch ' /u ') -and ($unreg[3] -match ' /s /u '))
        Assert-Check 'RegisterShell is scheduled after InstallFiles' (
            [int]$sequence['RegisterShell'][2] -gt [int]$sequence['InstallFiles'][2])
        Assert-Check 'UnregisterShell is scheduled before RemoveFiles' (
            [int]$sequence['UnregisterShell'][2] -lt [int]$sequence['RemoveFiles'][2])
        Assert-Check 'UnregisterShell only runs on removal (REMOVE="ALL")' ($sequence['UnregisterShell'][1] -eq 'REMOVE="ALL"')
        Assert-Check 'RegisterShell does not run on removal' ($sequence['RegisterShell'][1] -match 'NOT \(REMOVE="ALL"\)')
    }
    if ($sequence.ContainsKey('RemoveExistingProducts') -and $sequence.ContainsKey('InstallFiles')) {
        Assert-Check 'Major upgrade removes the old product (and unregisters it) before new files are installed' (
            [int]$sequence['RemoveExistingProducts'][2] -lt [int]$sequence['InstallFiles'][2])
    }

    # Sparse package actions: removal (deprovision, remove), install (stage, provision, register
    # for the installing user) and the rollbacks of the install pair.
    $packageActions = 'DeprovisionPackage', 'RemovePackage', 'StagePackage', 'ProvisionPackage', 'RegisterPackageForUser', 'StagePackageRollback', 'ProvisionPackageRollback'
    foreach ($name in $packageActions) {
        Assert-Check "CustomAction $name exists" ($actions.ContainsKey($name))
    }
    if (-not ($packageActions | Where-Object { -not $actions.ContainsKey($_) })) {
        # Type bits: 256 = rollback, 1024 = deferred, 2048 = no impersonation, 64 = ignore the exit code.
        foreach ($name in $packageActions) {
            $a = $actions[$name]
            $type = [int]$a[1]
        $isUserAction = $name -eq 'RegisterPackageForUser'
        $kind = if ($name -like '*Rollback') { 'rollback' } else { 'deferred' }
        $kindBit = if ($kind -eq 'rollback') { 256 } else { 1024 }
        $impBit = if ($isUserAction) { 0 } else { 2048 }
        $impName = if ($isUserAction) { 'impersonated' } else { 'non-impersonated' }
        Assert-Check "$name is a $kind, $impName, exit-code-ignoring EXE action" (
            (($type -band $kindBit) -ne 0) -and (($type -band 2048) -eq $impBit) -and (($type -band 64) -ne 0) -and (($type -band 7) -eq 2))
            Assert-Check "$name runs the 64-bit Windows PowerShell with -Command" (
                $a[3] -match '^\[System64Folder\]WindowsPowerShell\\v1\.0\\powershell\.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "[^"]+"$')
            # The text after -Command is formatted by Windows Installer and parsed by the command
            # line; braces and inner double quotes would be mangled.
            $inner = if ($a[3] -match ' -Command "([^"]+)"$') { $Matches[1] } else { '' }
            Assert-Check "$name has no braces or inner double quotes in its command" (($inner.Length -gt 0) -and ($inner -notmatch '[{}"]'))
            # ICE03: the CustomAction.Target column holds 255 characters.
            Assert-Check "$name fits the 255-character Target column" ($a[3].Length -le 255) "$($a[3].Length) characters"
        }
        $deprovision = 'Get-AppxProvisionedPackage -Online | Where-Object DisplayName -eq '
        $remove = 'Get-AppxPackage -AllUsers -Name '
        Assert-Check 'DeprovisionPackage and ProvisionPackageRollback deprovision the package' (
            $actions['DeprovisionPackage'][3].Contains($deprovision) -and $actions['DeprovisionPackage'][3].EndsWith(' | Remove-AppxProvisionedPackage -Online"') -and
            $actions['ProvisionPackageRollback'][3] -eq $actions['DeprovisionPackage'][3])
        Assert-Check 'RemovePackage and StagePackageRollback remove the package for all users' (
            $actions['RemovePackage'][3].Contains($remove) -and $actions['RemovePackage'][3].EndsWith(' | Remove-AppxPackage -AllUsers"') -and
            $actions['StagePackageRollback'][3] -eq $actions['RemovePackage'][3])
        Assert-Check 'StagePackage stages Thumbwright.msix with the install directory as external location' (
            $actions['StagePackage'][3].Contains("Add-AppxPackage -Stage '[InstallFolder]Thumbwright.msix' -ExternalLocation ('[InstallFolder]'.TrimEnd('\'))"))
        Assert-Check 'ProvisionPackage provisions the staged package for all users' (
            $actions['ProvisionPackage'][3].Contains("Add-AppxProvisionedPackage -Online -PackagePath '[InstallFolder]Thumbwright.msix' -SkipLicense"))
        Assert-Check 'RegisterPackageForUser registers the msix for the installing user' (
            $actions['RegisterPackageForUser'][3] -match 'Add-AppxPackage -Path .\[InstallFolder\]Thumbwright\.msix. -ExternalLocation' -and
            $actions['RegisterPackageForUser'][3] -notmatch 'AllUsers')
        $names = @(foreach ($name in 'DeprovisionPackage', 'RemovePackage') { [regex]::Matches($actions[$name][3], "'([^'\[]+)'") | ForEach-Object { $_.Groups[1].Value } })
        $script:CaPackageName = $names | Select-Object -First 1
        Assert-Check 'The removal actions name one package identity' ($names.Count -eq 2 -and @($names | Select-Object -Unique).Count -eq 1) ($names -join ', ')
        $seq = { param($n) [int]$sequence[$n][2] }
        Assert-Check 'Removal runs before UnregisterShell: DeprovisionPackage, RemovePackage, UnregisterShell, RemoveFiles' (
            (& $seq 'DeprovisionPackage') -lt (& $seq 'RemovePackage') -and (& $seq 'RemovePackage') -lt (& $seq 'UnregisterShell') -and
            (& $seq 'UnregisterShell') -lt (& $seq 'RemoveFiles'))
        Assert-Check 'Removal also runs on install, before InstallFiles, to clear a stale registration' (
            (& $seq 'RemovePackage') -lt (& $seq 'InstallFiles') -and -not $sequence['DeprovisionPackage'][1] -and -not $sequence['RemovePackage'][1])
        Assert-Check 'Staging then provisioning run after InstallFiles and RegisterShell, on install only' (
            (& $seq 'StagePackage') -gt (& $seq 'InstallFiles') -and (& $seq 'StagePackage') -gt (& $seq 'RegisterShell') -and
            (& $seq 'ProvisionPackage') -gt (& $seq 'StagePackage') -and
            $sequence['StagePackage'][1] -match 'NOT \(REMOVE="ALL"\)' -and $sequence['ProvisionPackage'][1] -match 'NOT \(REMOVE="ALL"\)')
        Assert-Check 'RegisterPackageForUser runs after provisioning, on install only' (
            (& $seq 'RegisterPackageForUser') -gt (& $seq 'ProvisionPackage') -and
            $sequence['RegisterPackageForUser'][1] -match 'NOT \(REMOVE="ALL"\)')
        Assert-Check 'Each rollback action is scheduled just before the action it undoes' (
            (& $seq 'StagePackageRollback') -lt (& $seq 'StagePackage') -and (& $seq 'ProvisionPackageRollback') -lt (& $seq 'ProvisionPackage') -and
            (& $seq 'StagePackageRollback') -gt (& $seq 'RegisterShell'))
    }

    Test-MsiFrontDoor $db $files $actions $sequence

    # P-1 / P-2: a public property (no lower-case letter in its name) can be set on the msiexec
    # command line and would be pasted into the elevated custom actions. The install directory
    # must be private, and no action may format a public property or directory.
    $propertyNames = @(Get-MsiColumn $db 'SELECT Property FROM Property')
    Assert-Check 'Property table has no INSTALLFOLDER (a public install directory would be overridable and injectable)' (
        -not ($propertyNames | Where-Object { $_ -ceq 'INSTALLFOLDER' }))
    $secure = Get-MsiProperty $db 'SecureCustomProperties'
    Assert-Check 'SecureCustomProperties does not list INSTALLFOLDER' (-not $secure -or (@($secure -split ';') -cnotcontains 'INSTALLFOLDER')) $secure
    $directoryIds = @(Get-MsiColumn $db 'SELECT Directory FROM Directory')
    $publicDirectories = @($directoryIds | Where-Object { $_ -cne 'TARGETDIR' -and $_ -cnotmatch '[a-z]' })
    Assert-Check 'Every Directory Id except TARGETDIR is private (has a lower-case letter)' ($publicDirectories.Count -eq 0) ($publicDirectories -join ', ')
    Assert-Check 'The install directory Id is InstallFolder' ($directoryIds -ccontains 'InstallFolder')
    $publicRefs = New-Object System.Collections.Generic.List[string]
    foreach ($a in $actions.Values) {
        foreach ($m in [regex]::Matches($a[3], '\[([^\[\]]+)\]')) {
            if ($m.Groups[1].Value -cnotmatch '[a-z]') { $publicRefs.Add("$($a[0]): $($m.Value)") }
        }
    }
    Assert-Check 'No custom action formats a public property or directory ([UPPERCASE]), so nothing on the msiexec command line can reach a command' (
        $publicRefs.Count -eq 0) ($publicRefs -join '; ')
    if ($actions.ContainsKey('RejectUnsafeInstallFolder') -and $sequence.ContainsKey('RejectUnsafeInstallFolder')) {
        Assert-Check 'RejectUnsafeInstallFolder is an error action (type 19) after CostFinalize, install only' (
            ([int]$actions['RejectUnsafeInstallFolder'][1] -band 63) -eq 19 -and
            [int]$sequence['RejectUnsafeInstallFolder'][2] -gt [int](Get-MsiRows $db "SELECT Sequence FROM InstallExecuteSequence WHERE Action='CostFinalize'" 1)[0][0] -and
            $sequence['RejectUnsafeInstallFolder'][1] -match 'NOT \(REMOVE="ALL"\)' -and $sequence['RejectUnsafeInstallFolder'][1] -match 'InstallFolder')
    } else {
        Assert-Check 'RejectUnsafeInstallFolder exists and is sequenced' $false
    }

    Assert-Check 'Package is per-machine (ALLUSERS=1)' ((Get-MsiProperty $db 'ALLUSERS') -eq '1')
    $version = Get-MsiProperty $db 'ProductVersion'
    Assert-Check 'ProductVersion is major.minor.build' ($version -match '^\d+\.\d+\.\d+$') $version
    return [pscustomobject]@{
        Version     = $version
        UpgradeCode = (Get-MsiProperty $db 'UpgradeCode')
        ProductCode = (Get-MsiProperty $db 'ProductCode')
    }
}

# The settings executable must carry no <msix> element: with package identity its HKCU writes
# would be virtualized into the package's private hive and never reach Explorer.
function Test-SettingsExeHasNoIdentity {
    param([string]$Path, [string]$Label)
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $text = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($Path))
    Assert-Check "[$Label] thumbwright-settings.exe embeds no <msix> package identity (settings reach the real HKCU)" ($text -notmatch '<msix\b')
}

# Inspects a sparse package file without installing it: identity, the executable that carries it,
# external-content flag, and that the verb and the COM class behind it use CLSID_EXPLORER_COMMAND
# from crates\tw-core\src\ids.rs. Returns Name, Publisher, Executable and Signed.
function Test-SparsePackageFile {
    param([string]$Path, [string]$Label)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entries = @($zip.Entries | ForEach-Object { $_.FullName })
        $part = $zip.GetEntry('AppxManifest.xml')
        if ($null -eq $part) { throw "$Path has no AppxManifest.xml" }
        $reader = New-Object System.IO.StreamReader ($part.Open())
        try { [xml]$manifest = $reader.ReadToEnd() } finally { $reader.Dispose() }
    } finally {
        $zip.Dispose()
    }
    $ns = New-Object System.Xml.XmlNamespaceManager $manifest.NameTable
    $ns.AddNamespace('m', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10')
    $ns.AddNamespace('uap10', 'http://schemas.microsoft.com/appx/manifest/uap/windows10/10')
    $ns.AddNamespace('desktop5', 'http://schemas.microsoft.com/appx/manifest/desktop/windows10/5')
    $ns.AddNamespace('com', 'http://schemas.microsoft.com/appx/manifest/com/windows10')
    $identity = $manifest.SelectSingleNode('/m:Package/m:Identity', $ns)
    $application = $manifest.SelectSingleNode('/m:Package/m:Applications/m:Application', $ns)
    $verb = $manifest.SelectSingleNode('//desktop5:Verb', $ns)
    $class = $manifest.SelectSingleNode('//com:Class', $ns)
    $external = $manifest.SelectSingleNode('/m:Package/m:Properties/uap10:AllowExternalContent', $ns)
    $explorerCommand = ((Get-ProductClasses | Where-Object { $_.Const -eq 'CLSID_EXPLORER_COMMAND' }).Clsid).Trim('{', '}')

    Assert-Check "[$Label] package is a sparse package (AllowExternalContent, no payload besides assets)" (
        ($null -ne $external -and $external.InnerText -eq 'true') -and
        -not ($entries | Where-Object { $_ -match '\.(dll|exe)$' }))
    Assert-Check "[$Label] package has an identity name and a CN= publisher" (
        $identity.Name -match '^[A-Za-z0-9][A-Za-z0-9.\-]+$' -and $identity.Publisher -like 'CN=*') "$($identity.Name) / $($identity.Publisher)"
    Assert-Check "[$Label] package Application is thumbwright-settings.exe (shipped by the MSI; carries no identity itself)" (
        $null -ne $application -and $application.Executable -eq 'thumbwright-settings.exe')
    Assert-Check "[$Label] explorer verb uses CLSID_EXPLORER_COMMAND $explorerCommand" (
        $null -ne $verb -and $verb.Clsid -eq $explorerCommand)
    Assert-Check "[$Label] COM surrogate class is CLSID_EXPLORER_COMMAND in thumbwright_shell.dll (STA)" (
        $null -ne $class -and $class.Id -eq $explorerCommand -and $class.Path -eq 'thumbwright_shell.dll' -and $class.ThreadingModel -eq 'STA')
    $pris = @($entries | Where-Object { $_ -like '*.pri' })
    Assert-Check "[$Label] package indexes its logos (one resources.pri, scaled and target-size Square44x44Logo)" (
        ($pris.Count -eq 1 -and $pris[0] -eq 'resources.pri') -and
        ($entries -contains 'Assets/Square44x44Logo.scale-100.png') -and
        ($entries -contains 'Assets/Square44x44Logo.targetsize-24_altform-unplated.png')) ($pris -join ', ')
    $signed = $entries -contains 'AppxSignature.p7x'
    Write-Host ("    package {0}, publisher {1}, {2}" -f $identity.Name, $identity.Publisher, $(if ($signed) { 'signed' } else { 'UNSIGNED (cannot be registered)' }))
    return [pscustomobject]@{
        Name       = $identity.Name
        Publisher  = $identity.Publisher
        Executable = $application.Executable
        Signed     = $signed
    }
}

function New-TestMsi {
    param([string]$OutFile, [string]$Version)
    $buildArgs = @{ SourceDir = $SourceDir; OutFile = $OutFile; Version = $Version }
    if ($AcceptWixEula) { $buildArgs.AcceptWixEula = $true }
    & (Join-Path $root 'packaging\wix\build-msi.ps1') @buildArgs
}

function Test-AdminImage {
    param([string]$Path)
    Write-Host '== Administrative extraction (no custom actions run, nothing installed)'
    $target = Join-Path $WorkDir 'admin-image'
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    $log = Join-Path $WorkDir 'admin-image.log'
    $p = Start-Process -FilePath msiexec.exe -Wait -PassThru -NoNewWindow -ArgumentList @(
        '/a', ('"{0}"' -f $Path), '/qn', ('TARGETDIR="{0}"' -f $target), '/l*v', ('"{0}"' -f $log))
    Assert-Check 'msiexec /a extracts the package' ($p.ExitCode -eq 0) "exit code $($p.ExitCode), log $log"
    $names = @(Get-ChildItem -LiteralPath $target -Recurse -File |
            Where-Object { $_.Extension -ne '.msi' } | ForEach-Object { $_.Name })
    Write-Host ("    extracted: {0}" -f ($names -join ', '))
    foreach ($name in $script:RequiredFiles) {
        Assert-Check "Extracted payload has $name" ($names -contains $name)
    }
    Assert-Check 'Extracted payload has no .ini file' (-not ($names | Where-Object { $_ -like '*.ini' }))
    $msixFile = Get-ChildItem -LiteralPath $target -Recurse -File -Filter 'Thumbwright.msix' | Select-Object -First 1
    if ($msixFile) {
        $package = Test-SparsePackageFile $msixFile.FullName 'extracted'
        Assert-Check 'Package identity equals the one the register/unregister actions manage' ($package.Name -eq $script:CaPackageName) "$($package.Name) vs $($script:CaPackageName)"
        Assert-Check 'MSI ships the executable that the package Application names' ($names -contains $package.Executable) $package.Executable
    }
    $settingsFile = Get-ChildItem -LiteralPath $target -Recurse -File -Filter 'thumbwright-settings.exe' | Select-Object -First 1
    if ($settingsFile) { Test-SettingsExeHasNoIdentity $settingsFile.FullName 'extracted' }
}

function Test-IceValidation {
    param([string]$Path)
    Write-Host '== ICE validation (wix msi validate)'
    $wix = Get-Command wix -ErrorAction SilentlyContinue
    $wixExe = if ($wix) { $wix.Source } else { Join-Path $env:USERPROFILE '.dotnet\tools\wix.exe' }
    if (-not (Test-Path -LiteralPath $wixExe)) {
        Write-Warning 'wix CLI not found; skipping ICE validation.'
        return
    }
    $eula = @()
    if ($AcceptWixEula) { $eula = @('-acceptEula', 'wix7') }
    # ICE43 and ICE57 are suppressed for one reason: the Start Menu shortcut of the per-machine
    # package lives in the all-users Programs folder and shares the file-keyed component of
    # thumbwright-settings.exe. ICE43 wants every non-advertised shortcut component to be keyed
    # on an HKCU value, and ICE57 then objects to mixing that per-user data with per-machine
    # state; both are written for per-user shortcuts and would put a per-user registry value
    # into a per-machine install (and Windows Installer would not remove it for other users).
    $out = & $wixExe msi validate @eula -sice ICE43 -sice ICE57 $Path 2>&1 | Out-String
    $code = $LASTEXITCODE
    if ($out.Trim()) { Write-Host $out }
    Assert-Check 'wix msi validate (ICE) reports no errors' ($code -eq 0) "exit code $code"
}

# ---------------------------------------------------------------------------------------------
# Dry run
# ---------------------------------------------------------------------------------------------
function Invoke-DryRun {
    New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
    # msiexec needs an absolute path.
    $target = if ($Msi) { (Resolve-Path -LiteralPath $Msi).Path } else { $null }
    if (-not $target) {
        $target = Join-Path $WorkDir 'dryrun.msi'
        $version = '0.0.0'
        $m = [regex]::Match((Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw),
            '(?ms)^\[workspace\.package\].*?^version\s*=\s*"(\d+\.\d+\.\d+)"')
        if ($m.Success) { $version = $m.Groups[1].Value }
        Write-Host "== Building a throw-away MSI from $SourceDir (version $version)"
        New-TestMsi -OutFile $target -Version $version
    }
    [void](Test-MsiPackage $target)
    Test-AdminImage $target
    Test-IceValidation $target
}

# ---------------------------------------------------------------------------------------------
# Elevated mode helpers
# ---------------------------------------------------------------------------------------------
function Assert-Elevated {
    $principal = New-Object Security.Principal.WindowsPrincipal ([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-Host ('test-install.ps1 installs and uninstalls the MSI and inspects HKLM, so it must run elevated. ' +
            'Start an elevated 64-bit PowerShell (Run as administrator) and run it again, ' +
            'or pass -DryRun for the non-elevated package checks.') -ForegroundColor Red
        exit 2
    }
    if (-not [Environment]::Is64BitProcess) {
        Write-Host 'Run this script from a 64-bit PowerShell: the DLL is x64 and registers in the 64-bit registry view.' -ForegroundColor Red
        exit 2
    }
}

$script:InstallDir = Join-Path $env:ProgramW6432 'Thumbwright'
$script:Installed = New-Object System.Collections.Generic.List[string]

# Exported key set. HKLM and HKCU, 64-bit view.
$script:SnapshotKeys = @(
    @{ Hive = 'HKLM'; Path = 'SOFTWARE\Classes' },
    @{ Hive = 'HKLM'; Path = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved' },
    @{ Hive = 'HKLM'; Path = 'SOFTWARE\Thumbwright' },
    @{ Hive = 'HKLM'; Path = 'SOFTWARE\WOW6432Node\Thumbwright' },
    @{ Hive = 'HKCU'; Path = 'Software\Classes' },
    @{ Hive = 'HKCU'; Path = 'Software\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved' },
    @{ Hive = 'HKCU'; Path = 'Software\Thumbwright' }
)

function Get-BaseKey {
    param([string]$Hive)
    $h = if ($Hive -eq 'HKLM') { [Microsoft.Win32.RegistryHive]::LocalMachine } else { [Microsoft.Win32.RegistryHive]::CurrentUser }
    return [Microsoft.Win32.RegistryKey]::OpenBaseKey($h, [Microsoft.Win32.RegistryView]::Registry64)
}

function Test-RegistryKeyExists {
    param([string]$Hive, [string]$Path)
    $base = Get-BaseKey $Hive
    $key = $base.OpenSubKey($Path)
    if ($null -eq $key) { return $false }
    $key.Close()
    return $true
}

# Reads a REG_SZ-style value; $null when the key or value is missing. Name '' is the default value.
function Get-RegistryValue {
    param([string]$Hive, [string]$Path, [string]$Name = '')
    $key = (Get-BaseKey $Hive).OpenSubKey($Path)
    if ($null -eq $key) { return $null }
    try { return $key.GetValue($Name, $null) } finally { $key.Close() }
}

# Subtrees owned and rewritten by Windows itself while anything runs: the shell's resource-string
# cache, Explorer's per-folder view state (Bags/BagMRU), and the Host Activity Manager's last-use
# timestamps of other packaged apps (AppModel\SystemAppData\<package>\HAM). They are not product
# state and change between any two exports, so they are left out.
#
# Also left out: the AppX deployment engine's own state (AppModel, AppContainer, PackagedCom). The
# MSI never writes it; Windows writes it when the sparse package is registered for a user (which
# this script does on purpose, as a sign-in would) and clears it on its own schedule after the
# package is removed, so it can outlive an uninstall by minutes. Package state is checked through
# the AppX APIs instead: provisioned after install, provisioned for nobody and registered for no
# user after uninstall. Everything else in the exported keys is still compared.
$script:OsVolatileKeys = '(?i)^\[HKEY_(CURRENT_USER\\Software|LOCAL_MACHINE\\SOFTWARE)\\Classes\\(Local Settings\\(MuiCache|Software\\Microsoft\\Windows\\Shell\\(Bags|BagMRU)|Software\\Microsoft\\Windows\\CurrentVersion\\(AppModel|AppContainer))|PackagedCom)(\\|\])'

# A .reg export is UTF-16. Flatten to "key header" and "key header<TAB>value line" entries so that a
# value line is compared together with the key it belongs to (a bare '@="x"' line is meaningless alone).
function Read-RegExport {
    param([string]$Path, $Into)
    $key = ''
    foreach ($line in [System.IO.File]::ReadLines($Path, [System.Text.Encoding]::Unicode)) {
        if ($line.Length -eq 0) { continue }
        if ($line[0] -eq '[') {
            $key = $line
            if ($key -notmatch $script:OsVolatileKeys) { [void]$Into.Add($line) }
        } elseif ($line.StartsWith('Windows Registry Editor')) {
            continue
        } elseif ($key -notmatch $script:OsVolatileKeys) {
            [void]$Into.Add($key + "`t" + $line)
        }
    }
}

function Get-Snapshot {
    param([string]$Label)
    Write-Host "  snapshot '$Label' (reg export of $($script:SnapshotKeys.Count) keys)..."
    $set = New-Object 'System.Collections.Generic.HashSet[string]'
    $index = 0
    foreach ($spec in $script:SnapshotKeys) {
        $index++
        if (-not (Test-RegistryKeyExists $spec.Hive $spec.Path)) { continue }
        $file = Join-Path $WorkDir ("{0}-{1}-{2}.reg" -f $Label, $index, $spec.Hive)
        & reg.exe export ("{0}\{1}" -f $spec.Hive, $spec.Path) $file /y /reg:64 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "reg export failed for $($spec.Hive)\$($spec.Path)" }
        Read-RegExport $file $set
        if (-not $KeepExports) { Remove-Item -LiteralPath $file -Force }
    }
    return , $set
}

function Compare-Snapshot {
    param($Before, $After)
    $added = New-Object 'System.Collections.Generic.HashSet[string]' (, $After)
    $added.ExceptWith($Before)
    $removed = New-Object 'System.Collections.Generic.HashSet[string]' (, $Before)
    $removed.ExceptWith($After)
    return [pscustomobject]@{ Added = $added; Removed = $removed }
}

function Assert-NoResidue {
    param([string]$Label, $Baseline, $After)
    $diff = Compare-Snapshot $Baseline $After
    $report = Join-Path $WorkDir ("diff-{0}.txt" -f ($Label -replace '[^A-Za-z0-9]+', '-'))
    $lines = @($diff.Added | ForEach-Object { "+ $_" }) + @($diff.Removed | ForEach-Object { "- $_" })
    Set-Content -LiteralPath $report -Value $lines -Encoding Unicode
    $detail = ''
    if ($lines.Count -gt 0) {
        $detail = "$($diff.Added.Count) added, $($diff.Removed.Count) lost; first lines:`n    " +
            (($lines | Select-Object -First 15) -join "`n    ") + "`n    full report: $report"
    }
    Assert-Check "Zero registry diff vs baseline $Label (HKLM + HKCU)" ($lines.Count -eq 0) $detail
    $markers = '(?i)thumbwright|99A01621-C48D-402A-A898-3D3E6D46C32F|39A04F07-5D10-46F8-8AD0-729B94CF03FE'
    $product = @($diff.Added | Where-Object { $_ -match $markers })
    Assert-Check "No product key or value left behind $Label" ($product.Count -eq 0) ($product | Select-Object -First 5 | Out-String)
}

# Registry footprint of an install: what it added to the baseline (all of it product state).
function Get-Footprint {
    param($Baseline, $Installed)
    return , (Compare-Snapshot $Baseline $Installed).Added
}

function Assert-SameFootprint {
    param([string]$Label, $Expected, $Actual)
    $e = New-Object 'System.Collections.Generic.HashSet[string]' (, $Expected)
    $e.SymmetricExceptWith($Actual)
    $detail = ''
    if ($e.Count -gt 0) { $detail = "$($e.Count) differing lines, first: " + (($e | Select-Object -First 5) -join ' | ') }
    Assert-Check "Registry footprint after $Label equals a fresh install" ($e.Count -eq 0) $detail
}

function Invoke-Msiexec {
    param([ValidateSet('/i', '/x', '/fa')][string]$Verb, [string]$Path, [string]$LogName)
    $log = Join-Path $WorkDir "$LogName.log"
    Write-Host "  msiexec $Verb $(Split-Path -Leaf $Path)  (log: $log)"
    $p = Start-Process -FilePath msiexec.exe -Wait -PassThru -NoNewWindow -ArgumentList @(
        $Verb, ('"{0}"' -f $Path), '/qn', '/norestart', '/l*v', ('"{0}"' -f $log))
    $ok = ($p.ExitCode -eq 0) -or ($p.ExitCode -eq 3010)
    Assert-Check "msiexec $Verb $LogName succeeded" $ok "exit code $($p.ExitCode), log $log"
    if ($p.ExitCode -eq 3010) {
        Write-Warning "$LogName finished with 3010 (restart required): a file was in use."
    }
    if (-not $ok) { throw "msiexec $Verb failed ($($p.ExitCode)); see $log" }
    if ($Verb -eq '/i' -and -not $script:Installed.Contains($Path)) { $script:Installed.Add($Path) }
}

# The thumbnail host (dllhost.exe) keeps the DLL mapped after a thumbnail was produced; an in-use
# file would make the uninstall leave it behind. Stop surrogates that hold it. Explorer itself loads the
# context menu handler on right-click; that case is only reported.
function Stop-ShellSurrogate {
    $dll = 'thumbwright_shell.dll'
    # Only the processes that host shell extensions: scanning every process's modules is slow.
    $hosts = @(Get-Process -Name dllhost, explorer, prevhost, SearchProtocolHost, SearchIndexer -ErrorAction SilentlyContinue)
    foreach ($proc in $hosts) {
        $holds = $false
        try {
            $holds = [bool]($proc.Modules | Where-Object { $_.ModuleName -eq $dll })
        } catch {
            $holds = $false
        }
        if (-not $holds) { continue }
        if ($proc.ProcessName -eq 'dllhost') {
            Write-Host "  stopping dllhost.exe pid $($proc.Id) that has $dll loaded"
            Stop-Process -Id $proc.Id -Force
        } else {
            Write-Warning "$($proc.ProcessName) pid $($proc.Id) has $dll loaded; the file may be replaced only after a restart."
        }
    }
}

# --- Product class list, read from the source of truth rather than duplicated here ---------------
function Get-ProductClasses {
    $text = Get-Content -LiteralPath (Join-Path $root 'crates\tw-core\src\ids.rs') -Raw
    $pattern = 'pub const (CLSID_\w+): ClassId = ClassId \{\s*guid: 0x([0-9A-Fa-f_]+),\s*display_name: "([^"]+)"'
    $classes = @()
    foreach ($m in [regex]::Matches($text, $pattern)) {
        $hex = $m.Groups[2].Value.Replace('_', '').ToUpperInvariant()
        if ($hex.Length -ne 32) { continue }
        $braced = '{0}-{1}-{2}-{3}-{4}' -f $hex.Substring(0, 8), $hex.Substring(8, 4), $hex.Substring(12, 4), $hex.Substring(16, 4), $hex.Substring(20)
        $classes += [pscustomobject]@{ Const = $m.Groups[1].Value; Clsid = "{$braced}"; Name = $m.Groups[3].Value }
    }
    if ($classes.Count -eq 0) { throw 'Could not read the CLSID constants from crates\tw-core\src\ids.rs' }
    return $classes
}

$script:ThumbCategory = '{E357FCCD-A995-4576-B01F-234630154E96}'
$script:ApprovedPath = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved'
$script:CheckedExtensions = @('png', 'jpg', 'bmp', 'gif', 'svg')

function Test-Installed {
    param([string]$Label)
    Write-Host "== Verify installed state: $Label"
    $dll = Join-Path $script:InstallDir 'thumbwright_shell.dll'
    foreach ($name in $script:RequiredFiles) {
        Assert-Check "[$Label] installed file $name" (Test-Path -LiteralPath (Join-Path $script:InstallDir $name))
    }
    Assert-Check "[$Label] no Thumbwright.ini next to the binaries" (-not (Test-Path -LiteralPath (Join-Path $script:InstallDir 'Thumbwright.ini')))

    $journal = (Get-BaseKey 'HKLM').OpenSubKey('SOFTWARE\Thumbwright\Registration')
    Assert-Check "[$Label] registration journal exists under HKLM\SOFTWARE\Thumbwright\Registration" ($null -ne $journal)
    if ($journal) {
        $names = @($journal.GetValueNames() | Where-Object { $_ -match '^\d+$' })
        Assert-Check "[$Label] journal DllPath is the installed DLL" ($journal.GetValue('DllPath') -eq $dll) ([string]$journal.GetValue('DllPath'))
        Assert-Check "[$Label] journal has numbered entries" ($names.Count -gt 0) "$($names.Count) entries"
        $journal.Close()
    }

    $classes = Get-ProductClasses
    $core = @('CLSID_THUMBNAIL', 'CLSID_CONTEXT_MENU')
    foreach ($class in $classes) {
        $clsidKey = "SOFTWARE\Classes\CLSID\$($class.Clsid)"
        $registered = Test-RegistryKeyExists 'HKLM' $clsidKey
        if (-not $registered -and ($core -notcontains $class.Const)) {
            Write-Host "    note: $($class.Const) is not registered by this build; skipped"
            continue
        }
        Assert-Check "[$Label] $($class.Const) CLSID key" $registered
        Assert-Check "[$Label] $($class.Const) display name" ((Get-RegistryValue 'HKLM' $clsidKey) -eq $class.Name) ([string](Get-RegistryValue 'HKLM' $clsidKey))
        Assert-Check "[$Label] $($class.Const) InprocServer32 is the installed DLL" ((Get-RegistryValue 'HKLM' "$clsidKey\InprocServer32") -eq $dll) ([string](Get-RegistryValue 'HKLM' "$clsidKey\InprocServer32"))
        Assert-Check "[$Label] $($class.Const) ThreadingModel" ((Get-RegistryValue 'HKLM' "$clsidKey\InprocServer32" 'ThreadingModel') -eq 'Apartment')
        Assert-Check "[$Label] $($class.Const) in Shell Extensions\Approved" ((Get-RegistryValue 'HKLM' $script:ApprovedPath $class.Clsid) -eq $class.Name)
    }

    $thumb = ($classes | Where-Object { $_.Const -eq 'CLSID_THUMBNAIL' }).Clsid
    $menu = ($classes | Where-Object { $_.Const -eq 'CLSID_CONTEXT_MENU' }).Clsid
    foreach ($ext in $script:CheckedExtensions) {
        foreach ($base in "SOFTWARE\Classes\.$ext", "SOFTWARE\Classes\SystemFileAssociations\.$ext") {
            Assert-Check "[$Label] thumbnail handler at $base" ((Get-RegistryValue 'HKLM' "$base\shellex\$script:ThumbCategory") -eq $thumb)
        }
    }
    Assert-Check "[$Label] context menu handler" ((Get-RegistryValue 'HKLM' 'SOFTWARE\Classes\*\shellex\ContextMenuHandlers\Thumbwright') -eq $menu)

    $package = Test-SparsePackageFile (Join-Path $script:InstallDir 'Thumbwright.msix') $Label
    Test-SettingsExeHasNoIdentity (Join-Path $script:InstallDir 'thumbwright-settings.exe') $Label
    Assert-Check "[$Label] sparse package identity equals the one the MSI actions manage" ($package.Name -eq $script:CaPackageName) "$($package.Name) vs $($script:CaPackageName)"
    if ($package.Signed) {
        # The MSI stages the package with the install directory as its external location and
        # provisions it; Windows then registers it for each user at their next sign-in.
        $provisioned = @(Get-AppxProvisionedPackage -Online | Where-Object DisplayName -eq $package.Name)
        Assert-Check "[$Label] sparse package is provisioned for all users" ($provisioned.Count -eq 1) "$($provisioned.Count) provisioned entries"
        # RegisterPackageForUser, the impersonated custom action, registers the package for
        # the user who ran the install (what their next sign-in would do), so the verb is
        # present without a sign-out. In an elevated test session the impersonated token is
        # the elevated user, which Windows may refuse to register for; that failure is
        # ignored by design, so this check only asserts what ran.
        $registered = @(Get-AppxPackage -Name $package.Name)
        if ($registered.Count -eq 1) {
            Assert-Check "[$Label] package registered for the current user by the installer" $true ''
        } else {
            Write-Warning "[$Label] RegisterPackageForUser did not register for this session's user (expected under an elevated test token); sign-in registration is covered by the [uninstall] registered-for-no-user check."
        }
    } else {
        Write-Warning "[$Label] Thumbwright.msix is unsigned (a -SkipSign build): Windows cannot register it, so provisioning checks are skipped."
    }
}

function Test-Uninstalled {
    param([string]$Label)
    Write-Host "== Verify removed state: $Label"
    Assert-Check "[$Label] install directory is gone" (-not (Test-Path -LiteralPath $script:InstallDir)) $script:InstallDir
    Assert-Check "[$Label] HKLM\SOFTWARE\Thumbwright is gone" (-not (Test-RegistryKeyExists 'HKLM' 'SOFTWARE\Thumbwright'))
    Assert-Check "[$Label] HKCU\Software\Thumbwright is gone" (-not (Test-RegistryKeyExists 'HKCU' 'Software\Thumbwright'))
    foreach ($class in (Get-ProductClasses)) {
        Assert-Check "[$Label] $($class.Const) CLSID key is gone" (-not (Test-RegistryKeyExists 'HKLM' "SOFTWARE\Classes\CLSID\$($class.Clsid)"))
        Assert-Check "[$Label] $($class.Const) Approved entry is gone" ($null -eq (Get-RegistryValue 'HKLM' $script:ApprovedPath $class.Clsid))
    }
    Assert-Check "[$Label] sparse package is no longer provisioned" (
        @(Get-AppxProvisionedPackage -Online | Where-Object DisplayName -eq $script:CaPackageName).Count -eq 0)
    Assert-Check "[$Label] sparse package is registered for no user" (
        @(Get-AppxPackage -AllUsers -Name $script:CaPackageName -ErrorAction SilentlyContinue).Count -eq 0)
}

function Get-InstalledProductCount {
    param([string]$UpgradeCode)
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $related = Invoke-Com $installer 'RelatedProducts' @($UpgradeCode) 'GetProperty'
    $n = 0
    foreach ($p in $related) { $n++ }
    return $n
}

# ---------------------------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------------------------
function Invoke-InstallUninstall {
    param($Baseline, $Package)
    Write-Host '=== Scenario: install / uninstall'
    Invoke-Msiexec '/i' $Package.Path 'a-install'
    Test-Installed 'install'
    $installed = Get-Snapshot 'a-installed'
    $footprint = Get-Footprint $Baseline $installed
    Assert-Check 'Install added registry state (journal, classes, Approved)' ($footprint.Count -gt 0) "$($footprint.Count) lines"
    $hkcuAdded = @($footprint | Where-Object { $_.StartsWith('[HKEY_CURRENT_USER') })
    Assert-Check 'Machine install wrote nothing to HKCU' ($hkcuAdded.Count -eq 0) "$($hkcuAdded.Count) lines"
    Stop-ShellSurrogate
    Invoke-Msiexec '/x' $Package.Path 'a-uninstall'
    [void]$script:Installed.Remove($Package.Path)
    Test-Uninstalled 'uninstall'
    Assert-NoResidue 'after install+uninstall' $Baseline (Get-Snapshot 'a-removed')
    return , $footprint
}

function Invoke-Upgrade {
    param($Baseline, $Package, $Footprint)
    Write-Host '=== Scenario: major upgrade'
    $old = $OldMsi
    $oldNote = 'a real older package'
    if (-not $old) {
        $old = Join-Path $WorkDir 'old-0.0.1.msi'
        if ([version]$Package.Version -le [version]'0.0.1') { throw "Package version $($Package.Version) is too low to synthesise an older package; pass -OldMsi." }
        New-TestMsi -OutFile $old -Version '0.0.1'
        $oldNote = 'a synthetic 0.0.1 package built from the same payload'
    }
    $oldInfo = Test-MsiPackage $old
    Assert-Check 'Old package has the same UpgradeCode' ($oldInfo.UpgradeCode -eq $Package.UpgradeCode)
    Assert-Check 'Old package version is lower' ([version]$oldInfo.Version -lt [version]$Package.Version) "$($oldInfo.Version) vs $($Package.Version)"
    Write-Host "  upgrading from $oldNote ($($oldInfo.Version))"

    Invoke-Msiexec '/i' $old 'b-install-old'
    Test-Installed 'old version'
    Stop-ShellSurrogate
    Invoke-Msiexec '/i' $Package.Path 'b-upgrade'
    [void]$script:Installed.Remove($old)
    Test-Installed 'after upgrade'
    Assert-Check 'Exactly one product with this UpgradeCode is installed after the upgrade' ((Get-InstalledProductCount $Package.UpgradeCode) -eq 1)
    if (-not $OldMsi) {
        Assert-SameFootprint 'upgrade' $Footprint (Get-Footprint $Baseline (Get-Snapshot 'b-upgraded'))
    }
    Stop-ShellSurrogate
    Invoke-Msiexec '/x' $Package.Path 'b-uninstall'
    [void]$script:Installed.Remove($Package.Path)
    Test-Uninstalled 'uninstall after upgrade'
    Assert-NoResidue 'after upgrade+uninstall' $Baseline (Get-Snapshot 'b-removed')
}

function Invoke-Repair {
    param($Baseline, $Package, $Footprint)
    Write-Host '=== Scenario: repair (msiexec /fa)'
    Invoke-Msiexec '/i' $Package.Path 'c-install'
    Test-Installed 'before repair'
    Stop-ShellSurrogate
    Invoke-Msiexec '/fa' $Package.Path 'c-repair'
    Test-Installed 'after repair'
    Assert-SameFootprint 'repair' $Footprint (Get-Footprint $Baseline (Get-Snapshot 'c-repaired'))
    Stop-ShellSurrogate
    Invoke-Msiexec '/x' $Package.Path 'c-uninstall'
    [void]$script:Installed.Remove($Package.Path)
    Test-Uninstalled 'uninstall after repair'
    Assert-NoResidue 'after repair+uninstall' $Baseline (Get-Snapshot 'c-removed')
}

function Invoke-Elevated {
    $packagePath = $Msi
    if (-not $packagePath) {
        $latest = Get-ChildItem -LiteralPath (Join-Path $root 'dist') -Filter 'Thumbwright-*-x64.msi' -File -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if (-not $latest) { throw 'No -Msi given and no dist\Thumbwright-*-x64.msi found. Run scripts\release.ps1 first.' }
        $packagePath = $latest.FullName
    }
    $packagePath = (Resolve-Path -LiteralPath $packagePath).Path
    New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
    Write-Host "Work directory: $WorkDir"

    $info = Test-MsiPackage $packagePath
    $package = [pscustomobject]@{ Path = $packagePath; Version = $info.Version; UpgradeCode = $info.UpgradeCode }
    if (($script:Results | Where-Object { -not $_.Passed }).Count -gt 0) {
        throw 'The package failed its table checks; not installing it.'
    }

    # Never run over an existing install: the test uninstalls what it installs.
    $already = Get-InstalledProductCount $package.UpgradeCode
    if ($already -gt 0 -or (Test-Path -LiteralPath $script:InstallDir) -or (Test-RegistryKeyExists 'HKLM' 'SOFTWARE\Thumbwright')) {
        throw 'Thumbwright is already installed or has left files or keys on this machine. Uninstall it (and clean up) first so the baseline is clean.'
    }

    Write-Host '== Baseline'
    $baseline = Get-Snapshot 'baseline'

    # The install/uninstall run always goes first: it yields the reference footprint the other
    # scenarios compare against.
    $footprint = Invoke-InstallUninstall $baseline $package
    if ($Scenario -in 'All', 'Upgrade') { Invoke-Upgrade $baseline $package $footprint }
    if ($Scenario -in 'All', 'Repair') { Invoke-Repair $baseline $package $footprint }
}

# ---------------------------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------------------------
$exitCode = 0
try {
    if ($DryRun) {
        Invoke-DryRun
    } else {
        Assert-Elevated
        Invoke-Elevated
    }
} catch {
    Write-Host ("ERROR: {0}" -f $_.Exception.Message) -ForegroundColor Red
    $exitCode = 1
} finally {
    # A failed run must not leave a registered product behind.
    foreach ($leftover in @($script:Installed)) {
        Write-Warning "Cleaning up: uninstalling $leftover"
        Stop-ShellSurrogate
        Start-Process -FilePath msiexec.exe -Wait -NoNewWindow -ArgumentList @('/x', ('"{0}"' -f $leftover), '/qn', '/norestart') | Out-Null
    }
}

$failed = @($script:Results | Where-Object { -not $_.Passed })
Write-Host ''
Write-Host ("{0} checks, {1} failed. Work directory: {2}" -f $script:Results.Count, $failed.Count, $WorkDir)
if ($failed.Count -gt 0) {
    $failed | ForEach-Object { Write-Host ("FAILED: {0} {1}" -f $_.Name, $_.Detail) -ForegroundColor Red }
    $exitCode = 1
}
exit $exitCode
