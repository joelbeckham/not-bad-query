# Send keystrokes to the good-query window (SendKeys syntax: ^ = Ctrl, % = Alt, + = Shift, {ENTER}).
#   powershell -File dev/keys.ps1 -Keys "select 1^{ENTER}"
param(
    [Parameter(Mandatory = $true)][string]$Keys,
    [string]$ProcessName = "good-query",
    [int]$DelayMs = 300
)
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Fg {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
}
"@
$p = Get-Process -Name $ProcessName -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
[Fg]::ShowWindow($p.MainWindowHandle, 9) | Out-Null
[Fg]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
(New-Object -ComObject WScript.Shell).AppActivate($p.Id) | Out-Null
Start-Sleep -Milliseconds $DelayMs
[System.Windows.Forms.SendKeys]::SendWait($Keys)
