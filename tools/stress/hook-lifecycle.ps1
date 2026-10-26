<#
.SYNOPSIS
    Windows stress checks for the WinOrbit keyboard hook lifecycle.

.DESCRIPTION
    Drives a WinOrbit build with injected Alt+Tab / Alt+` input. The hook only
    ignores input it injected itself, so this exercises the real hook path.

    Checks:
      1. Exit (tray "Exit" command) while hotkey traffic is in flight, repeated.
         Every run must exit within the timeout with exit code 0.
      2. UI thread stalled (every thread except "keyboard-hook" suspended)
         while hotkey traffic continues. The hook must stay installed and the
         app must recover once resumed.
      3. Hook liveness probe repeated over a longer period.
      4. No Alt or mask (vkE8) key left logically down afterwards.

    Requirements:
      - No other WinOrbit instance running.
      - winorbit.ini next to the exe with [switch-apps] enable = yes, and at
        least one normal app window open (the probe watches the overlay).

    WARNING: this sends real Alt+Tab input and will switch between your
    windows while it runs. Do not type into anything important meanwhile.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File hook-lifecycle.ps1 -Exe .\winorbit.exe
#>
param(
    [Parameter(Mandatory = $true)] [string] $Exe,
    [int] $ExitIterations = 25,
    [int] $StallSeconds = 5,
    [int] $ProbeMinutes = 2
)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path
$log = Join-Path (Get-Location) ("winorbit-stress-{0:yyyyMMdd-HHmmss}.log" -f (Get-Date))
Start-Transcript -Path $log | Out-Null

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class W {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string name);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint code, uint mapType);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern short GetAsyncKeyState(int vk);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageTimeout(IntPtr h, uint msg, IntPtr w, IntPtr l, uint flags, uint timeout, out IntPtr result);
    [DllImport("kernel32.dll")] public static extern IntPtr OpenThread(uint access, bool inherit, uint id);
    [DllImport("kernel32.dll")] public static extern uint SuspendThread(IntPtr h);
    [DllImport("kernel32.dll")] public static extern uint ResumeThread(IntPtr h);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll")] public static extern int GetThreadDescription(IntPtr h, out IntPtr desc);
    [DllImport("kernel32.dll")] public static extern IntPtr LocalFree(IntPtr p);

    // Scan-code based so the hook (which matches scan codes) sees real keys.
    public static void Key(uint scan, bool up) {
        uint vk = MapVirtualKey(scan, 1);
        keybd_event((byte)vk, (byte)scan, up ? 2u : 0u, UIntPtr.Zero);
    }
}
"@

$SC_ALT = 0x38; $SC_TAB = 0x0F; $SC_BACKTICK = 0x29
$WM_COMMAND = 0x0111; $IDM_EXIT = 1
$failures = New-Object System.Collections.Generic.List[string]

function Release-All {
    [W]::Key($SC_TAB, $true); [W]::Key($SC_BACKTICK, $true); [W]::Key($SC_ALT, $true)
}

function Start-WinOrbit {
    if (Get-Process -Name winorbit -ErrorAction SilentlyContinue) {
        throw "Another WinOrbit is running; exit it first."
    }
    $p = Start-Process -FilePath $Exe -WorkingDirectory (Split-Path $Exe) -PassThru
    $deadline = (Get-Date).AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 50
        $hwnd = [W]::FindWindow("WinOrbit", $null)
    } while ($hwnd -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline)
    if ($hwnd -eq [IntPtr]::Zero) { throw "WinOrbit window did not appear" }
    Start-Sleep -Milliseconds 200
    return @{ Process = $p; Hwnd = $hwnd }
}

function Stop-WinOrbit($app, [int] $timeoutMs = 5000) {
    [void][W]::PostMessage($app.Hwnd, $WM_COMMAND, [IntPtr]$IDM_EXIT, [IntPtr]::Zero)
    $exited = $app.Process.WaitForExit($timeoutMs)
    if (-not $exited) {
        $app.Process.Kill()
        return @{ Exited = $false; Code = $null }
    }
    return @{ Exited = $true; Code = $app.Process.ExitCode }
}

# Hold Alt, tap Tab, and report whether the overlay became visible.
function Test-HookAlive($app) {
    $seen = $false
    try {
        [W]::Key($SC_ALT, $false); Start-Sleep -Milliseconds 30
        [W]::Key($SC_TAB, $false); [W]::Key($SC_TAB, $true)
        $deadline = (Get-Date).AddMilliseconds(1500)
        while ((Get-Date) -lt $deadline) {
            if ([W]::IsWindowVisible($app.Hwnd)) { $seen = $true; break }
            Start-Sleep -Milliseconds 20
        }
    } finally {
        [W]::Key($SC_ALT, $true)
    }
    $deadline = (Get-Date).AddMilliseconds(1500)
    while ([W]::IsWindowVisible($app.Hwnd) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 20
    }
    return $seen
}

# Runs Alt+Tab / Alt+` traffic on a background runspace until stopped.
function Start-Traffic {
    $ps = [PowerShell]::Create()
    [void]$ps.AddScript({
        param($flag)
        while (-not $flag.Stop) {
            [W]::Key(0x38, $false)
            for ($i = 0; $i -lt 3; $i++) { [W]::Key(0x0F, $false); [W]::Key(0x0F, $true); Start-Sleep -Milliseconds 5 }
            for ($i = 0; $i -lt 2; $i++) { [W]::Key(0x29, $false); [W]::Key(0x29, $true); Start-Sleep -Milliseconds 5 }
            [W]::Key(0x38, $true)
            Start-Sleep -Milliseconds 10
        }
        [W]::Key(0x38, $true)
    }).AddArgument($script:trafficFlag)
    $handle = $ps.BeginInvoke()
    return @{ PS = $ps; Handle = $handle }
}

function Stop-Traffic($traffic) {
    $script:trafficFlag.Stop = $true
    [void]$traffic.PS.EndInvoke($traffic.Handle)
    $traffic.PS.Dispose()
    Release-All
}

Write-Host "Log: $log"
Write-Host "Exe: $Exe"
Write-Host ("Windows: {0}" -f (Get-CimInstance Win32_OperatingSystem | ForEach-Object { "$($_.Caption) $($_.Version)" }))

try {
    # 1. Exit under hotkey traffic.
    Write-Host "`n[1] Exit while hotkey traffic is in flight x$ExitIterations"
    for ($i = 1; $i -le $ExitIterations; $i++) {
        $app = Start-WinOrbit
        $script:trafficFlag = [hashtable]::Synchronized(@{ Stop = $false })
        $traffic = Start-Traffic
        Start-Sleep -Milliseconds (100 + (Get-Random -Maximum 400))
        $result = Stop-WinOrbit $app
        Stop-Traffic $traffic
        $line = "  run {0,2}: exited={1} code={2}" -f $i, $result.Exited, $result.Code
        Write-Host $line
        if (-not $result.Exited -or $result.Code -ne 0) { $failures.Add("exit run $i -> $line") }
    }

    # 2. Stalled UI thread.
    Write-Host "`n[2] UI thread stalled for $StallSeconds s while hotkeys keep firing"
    $app = Start-WinOrbit
    if (-not (Test-HookAlive $app)) { $failures.Add("stall: hook not alive before stall") }
    $suspended = @()
    $hookThreadFound = $false
    try {
        $app.Process.Refresh()
        foreach ($t in $app.Process.Threads) {
            # THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION
            $h = [W]::OpenThread(0x0002 -bor 0x0800, $false, [uint32]$t.Id)
            if ($h -eq [IntPtr]::Zero) { continue }
            $desc = [IntPtr]::Zero
            $name = ""
            if ([W]::GetThreadDescription($h, [ref]$desc) -ge 0 -and $desc -ne [IntPtr]::Zero) {
                $name = [Runtime.InteropServices.Marshal]::PtrToStringUni($desc)
                [void][W]::LocalFree($desc)
            }
            if ($name -eq "keyboard-hook") { $hookThreadFound = $true; [void][W]::CloseHandle($h); continue }
            [void][W]::SuspendThread($h)
            $suspended += $h
        }
        if (-not $hookThreadFound) { $failures.Add("stall: keyboard-hook thread not found") }
        Write-Host "  suspended $($suspended.Count) thread(s); type in another window now - it must not lag"
        $script:trafficFlag = [hashtable]::Synchronized(@{ Stop = $false })
        $traffic = Start-Traffic
        Start-Sleep -Seconds $StallSeconds
        Stop-Traffic $traffic
    } finally {
        foreach ($h in $suspended) { [void][W]::ResumeThread($h); [void][W]::CloseHandle($h) }
        Release-All
    }
    $out = [IntPtr]::Zero
    # SMTO_ABORTIFHUNG; WM_NULL round trip proves the UI thread is back.
    $responsive = [W]::SendMessageTimeout($app.Hwnd, 0, [IntPtr]::Zero, [IntPtr]::Zero, 2, 5000, [ref]$out) -ne [IntPtr]::Zero
    Write-Host "  UI responsive after resume: $responsive"
    if (-not $responsive) { $failures.Add("stall: UI not responsive after resume") }
    Start-Sleep -Milliseconds 500
    $alive = Test-HookAlive $app
    Write-Host "  hook alive after stall: $alive"
    if (-not $alive) { $failures.Add("stall: hook not alive after stall") }

    # 3. Liveness over time.
    Write-Host "`n[3] Hook liveness probe every 15 s for $ProbeMinutes min"
    $deadline = (Get-Date).AddMinutes($ProbeMinutes)
    $n = 0
    while ((Get-Date) -lt $deadline) {
        $n++
        $alive = Test-HookAlive $app
        Write-Host ("  probe {0,2} at {1:HH:mm:ss}: alive={2}" -f $n, (Get-Date), $alive)
        if (-not $alive) { $failures.Add("probe ${n}: hook not alive") }
        Start-Sleep -Seconds 15
    }

    $result = Stop-WinOrbit $app
    Write-Host "  final exit: exited=$($result.Exited) code=$($result.Code)"
    if (-not $result.Exited -or $result.Code -ne 0) { $failures.Add("final exit failed") }
} finally {
    Release-All
    # 4. Nothing left logically down.
    Start-Sleep -Milliseconds 200
    $altDown = ([W]::GetAsyncKeyState(0x12) -band 0x8000) -ne 0
    $maskDown = ([W]::GetAsyncKeyState(0xE8) -band 0x8000) -ne 0
    Write-Host "`n[4] Alt down: $altDown, mask vkE8 down: $maskDown"
    if ($altDown) { $failures.Add("Alt left down") }
    if ($maskDown) { $failures.Add("mask key vkE8 left down") }

    if ($failures.Count -eq 0) {
        Write-Host "`nRESULT: PASS" -ForegroundColor Green
    } else {
        Write-Host "`nRESULT: FAIL" -ForegroundColor Red
        $failures | ForEach-Object { Write-Host "  - $_" }
    }
    Stop-Transcript | Out-Null
}
