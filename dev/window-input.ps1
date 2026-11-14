# Process-targeted messages; does not move the user's mouse or change app focus.
param(
    [Parameter(Mandatory = $true)][int]$TargetProcessId,
    [string]$Text = "",
    [int]$X = -1,
    [int]$Y = -1,
    [switch]$Right,
    [switch]$Close,
    [int]$VirtualKey = 0
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class AppInput {
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
[AppInput]::SetProcessDPIAware() | Out-Null
$appProcess = Get-Process -Id $TargetProcessId -ErrorAction Stop
if ($appProcess.ProcessName -ne 'not-bad-query' -or $appProcess.MainWindowHandle -eq 0) { throw 'No matching preview window' }
$handle = $appProcess.MainWindowHandle
if ($X -ge 0 -and $Y -ge 0) {
    $point = [IntPtr](($Y -shl 16) -bor ($X -band 0xffff))
    [AppInput]::SendMessage($handle, 0x200, [IntPtr]::Zero, $point) | Out-Null
    $down = if ($Right) { 0x204 } else { 0x201 }
    $up = if ($Right) { 0x205 } else { 0x202 }
    [AppInput]::SendMessage($handle, $down, [IntPtr]1, $point) | Out-Null
    [AppInput]::SendMessage($handle, $up, [IntPtr]::Zero, $point) | Out-Null
}
foreach ($character in $Text.ToCharArray()) {
    [AppInput]::SendMessage($handle, 0x102, [IntPtr][int]$character, [IntPtr]1) | Out-Null
}
if ($VirtualKey) {
    [AppInput]::SendMessage($handle, 0x100, [IntPtr]$VirtualKey, [IntPtr]1) | Out-Null
    [AppInput]::SendMessage($handle, 0x101, [IntPtr]$VirtualKey, [IntPtr]0xC0000001L) | Out-Null
}
if ($Close) { [AppInput]::SendMessage($handle, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null }
# Let GPUI paint newly opened/closed overlays before a subsequent scripted action.
Start-Sleep -Milliseconds 150
