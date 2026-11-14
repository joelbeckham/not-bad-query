# Exercise real native startup with broken configuration, without touching user settings.
param([string]$Executable = 'target/debug/not-bad-query.exe')
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path -LiteralPath $Executable).Path
$fixtureRoot = Join-Path (Resolve-Path .).Path ('target/startup-smoke-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding $false
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class StartupSmoke {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint id);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder name, int count);
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    public static void Close(int processId) {
        EnumWindows((h, l) => {
            uint id; GetWindowThreadProcessId(h, out id);
            var name = new StringBuilder(128);
            GetClassName(h, name, name.Capacity);
            // Closing the debug console instead would terminate the app with
            // STATUS_CONTROL_C_EXIT rather than exercising native app shutdown.
            if (id == processId && name.ToString() == "Zed::Window") SendMessage(h, 0x10, IntPtr.Zero, IntPtr.Zero);
            return true;
        }, IntPtr.Zero);
    }
}
'@
$originalConfig = $env:NBQ_CONFIG_DIR
try {
    foreach ($case in @('clean', 'missing-project', 'bad-settings', 'bad-workspace', 'bad-connection')) {
        $config = Join-Path $fixtureRoot $case
        New-Item -ItemType Directory -Path $config | Out-Null
        switch ($case) {
            'missing-project' { [IO.File]::WriteAllText((Join-Path $config 'workspace.toml'), ("open = ['" + (Join-Path $config 'missing-project') + "']"), $utf8) }
            'bad-settings' { [IO.File]::WriteAllText((Join-Path $config 'settings.toml'), 'dark = [', $utf8) }
            'bad-workspace' { [IO.File]::WriteAllText((Join-Path $config 'workspace.toml'), 'open = [', $utf8) }
            'bad-connection' {
                $project = Join-Path $config 'project'
                New-Item -ItemType Directory -Path (Join-Path $project 'connections') | Out-Null
                [IO.File]::WriteAllText((Join-Path $project 'not-bad-query.toml'), "id = 'startup-test'`nname = 'Startup test'", $utf8)
                [IO.File]::WriteAllText((Join-Path $project 'connections/bad.toml'), 'port = [', $utf8)
                [IO.File]::WriteAllText((Join-Path $config 'workspace.toml'), ("open = ['" + $project + "']"), $utf8)
            }
        }
        $env:NBQ_CONFIG_DIR = $config
        $stderr = Join-Path $config 'stderr.log'
        $process = Start-Process -FilePath $binary -WorkingDirectory (Get-Location).Path -WindowStyle Hidden -RedirectStandardError $stderr -RedirectStandardOutput (Join-Path $config 'stdout.log') -PassThru
        # Cache the native handle before exit; Windows PowerShell otherwise can
        # return a null ExitCode even after WaitForExit succeeds.
        $null = $process.Handle
        try {
            if ($process.WaitForExit(2500)) { throw "Startup failed ($case): $(Get-Content -LiteralPath $stderr -Raw)" }
            if ((Get-Content -LiteralPath $stderr -Raw) -match 'panicked') { throw "Startup panicked ($case)" }
            [StartupSmoke]::Close($process.Id)
            if (-not $process.WaitForExit(5000)) { throw "Window did not close ($case)" }
            if ($process.ExitCode -ne 0) { throw "Nonzero exit ($case): $($process.ExitCode)" }
            Write-Output "PASS $case"
        } finally {
            $process.Refresh()
            if (-not $process.HasExited) { Stop-Process -Id $process.Id }
        }
    }
} finally {
    $env:NBQ_CONFIG_DIR = $originalConfig
}
