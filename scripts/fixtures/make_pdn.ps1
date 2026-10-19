# Saves a PNG as a .pdn with a portable Paint.NET (5.1.12 was used) by driving its Save As
# dialog. Paint.NET has no command line for saving. Input is only sent while a Paint.NET
# window owns the foreground; any other foreground aborts the script. An already running
# Paint.NET is closed first. -Source and -Out use forward slashes when called from bash.
#
#   make_pdn.ps1 -Source C:/wt/tools/gen/big.png -Out C:/wt/tools/gen/big.pdn
#
# The dialog opens in the source's folder with the type list closed; the type entry
# "Paint.NET (*.pdn)" is the first item, one row below the combo box.
param(
    [string]$Source,
    [string]$Out,
    [string]$PaintNet = 'C:\wt\tools\pdn\PaintDotNet.exe'
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class G {
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    public static uint Pid(IntPtr h) { uint p; GetWindowThreadProcessId(h, out p); return p; }
    public static void Raise(IntPtr h) {
        IntPtr cur = GetForegroundWindow();
        uint p; uint curT = GetWindowThreadProcessId(cur, out p);
        uint me = GetCurrentThreadId();
        AttachThreadInput(me, curT, true);
        ShowWindow(h, 9);
        SetForegroundWindow(h);
        AttachThreadInput(me, curT, false);
    }
    public static void Click(int x, int y) {
        SetCursorPos(x, y);
        System.Threading.Thread.Sleep(200);
        mouse_event(0x02, 0, 0, 0, UIntPtr.Zero);
        mouse_event(0x04, 0, 0, 0, UIntPtr.Zero);
    }
}
'@
$A = [System.Windows.Automation.AutomationElement]
$TS = [System.Windows.Automation.TreeScope]
if (Test-Path $Out) { Remove-Item $Out }
Get-Process PaintDotNet -ErrorAction SilentlyContinue | ForEach-Object { $_.CloseMainWindow() | Out-Null }
Start-Sleep -Seconds 2
Get-Process PaintDotNet -ErrorAction SilentlyContinue | Stop-Process -Force
$p = Start-Process -FilePath $PaintNet -ArgumentList ('"' + $Source + '"') -PassThru
$leaf = [IO.Path]::GetFileName($Source)
$deadline = (Get-Date).AddSeconds(60)
while ((Get-Date) -lt $deadline) {
    $p.Refresh()
    if ($p.MainWindowHandle -ne 0 -and $p.MainWindowTitle -like "*$leaf*") { break }
    Start-Sleep -Milliseconds 500
}
Start-Sleep -Seconds 2
function Guard([string]$what) {
    $fpid = [G]::Pid([G]::GetForegroundWindow())
    if ($fpid -ne $p.Id) { throw ("{0}: foreground pid {1} is not Paint.NET {2}" -f $what, $fpid, $p.Id) }
}
$p.Refresh()
[G]::Raise($p.MainWindowHandle)
Start-Sleep -Milliseconds 800
Guard 'before ctrl+shift+s'
[System.Windows.Forms.SendKeys]::SendWait('^+s')
$byName = New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, 'Save As')
$dlg = $null
$end = (Get-Date).AddSeconds(15)
while (-not $dlg -and (Get-Date) -lt $end) {
    foreach ($w in $A::RootElement.FindAll($TS::Descendants, $byName)) {
        if ($w.Current.ControlType.ProgrammaticName -eq 'ControlType.Window' -and $w.Current.ProcessId -eq $p.Id) { $dlg = $w }
    }
    Start-Sleep -Milliseconds 300
}
if (-not $dlg) { throw 'Save As dialog did not appear' }
Start-Sleep -Seconds 1
Guard 'dialog open'
$host1 = $dlg.FindFirst($TS::Descendants, (New-Object System.Windows.Automation.PropertyCondition($A::AutomationIdProperty, 'FileTypeControlHost')))
$r = $host1.Current.BoundingRectangle
$cx = [int]($r.Left + $r.Width / 2); $cy = [int]($r.Top + $r.Height / 2)
Write-Output ("type combo at {0},{1}" -f $cx, $cy)
Guard 'before combo click'
[G]::Click($cx, $cy)
Start-Sleep -Seconds 1
Guard 'before item click'
[G]::Click($cx, [int]($cy + 19))
Start-Sleep -Seconds 1
# Choosing the type renamed the file to <stem>.pdn in the source's folder.
Guard 'before Enter'
[System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
Start-Sleep -Seconds 3
Write-Output ('saved: ' + (Test-Path $Out))
