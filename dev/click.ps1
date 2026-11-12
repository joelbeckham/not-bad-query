# Click inside the good-query window at window-relative pixel coordinates
# (the same coordinates as in dev/screenshot.ps1 output).
#   powershell -File dev/click.ps1 -X 60 -Y 95 [-Double] [-Right]
param(
    [Parameter(Mandatory = $true)][int]$X,
    [Parameter(Mandatory = $true)][int]$Y,
    [switch]$Double,
    [switch]$Right,
    [string]$ProcessName = "good-query"
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class M {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
[M]::SetProcessDPIAware() | Out-Null
$p = Get-Process -Name $ProcessName -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
[M]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
$r = New-Object M+RECT
[M]::GetWindowRect($p.MainWindowHandle, [ref]$r) | Out-Null
[M]::SetCursorPos($r.Left + $X, $r.Top + $Y) | Out-Null
Start-Sleep -Milliseconds 120
$down = if ($Right) { 0x0008 } else { 0x0002 }
$up = if ($Right) { 0x0010 } else { 0x0004 }
$n = if ($Double) { 2 } else { 1 }
for ($i = 0; $i -lt $n; $i++) {
    [M]::mouse_event($down, 0, 0, 0, [IntPtr]::Zero)
    [M]::mouse_event($up, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 60
}
