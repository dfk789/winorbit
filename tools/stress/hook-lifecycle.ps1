<#
.SYNOPSIS
    Windows stress checks for the WinOrbit keyboard hook lifecycle.

.DESCRIPTION
    Drives a WinOrbit build with injected Alt+Tab / Alt+` input. The hook only
    ignores input it injected itself, so this exercises the real hook path.

    Hook liveness and overlay latency are measured separately:

      * Hook probe: holds Alt, presses Tab, then presses an unassigned sentinel
        key and waits until Windows reports the sentinel down. Input is
        processed in order, so by then Tab has been through every keyboard
        hook. If WinOrbit's hook consumed Tab, Windows never marks Tab down;
        if Tab is down, the hook did not intercept it. This does not depend on
        the UI thread or on the overlay painting. A detector check (Alt + an
        unassigned key WinOrbit ignores, which must be seen down) proves the
        method works on this machine before any probe is trusted.
      * Overlay latency: time from Alt+Tab until the overlay window becomes
        visible. Reported as a measurement, never as hook health.

    Probes cancel with Esc before releasing Alt, so they do not switch windows.

    Modes:
      Full           1. Detector check, cold and warm overlay latency.
                     2. Tray "Exit" while hotkey traffic is in flight, repeated.
                        Every run must exit within the timeout with code 0.
                     3. UI thread stalled (every thread except "keyboard-hook"
                        suspended) while hotkey traffic continues; hook probes
                        during the stall; recovery after resume.
                     4. Hook probe repeated over a longer period.
                     5. No Alt, Tab, mask (vkE8) or probe key left down.
      ColdStart      Fresh launches only, measuring cold (first) and warm
                     (second) overlay latency. Run it against each build to
                     compare them.
      SelfTestPass   Sends no input. Exits 0.
      SelfTestFail   Sends no input. Records one failure; exits non-zero.
      SelfTestAbort  Sends no input. Throws mid-run; exits non-zero.

    Exit code: 0 only if the run completed and recorded no failure; 1
    otherwise (including aborted runs).

    Requirements for Full and ColdStart:
      - No other WinOrbit instance running.
      - winorbit.ini next to the exe with [switch-apps] enable = yes, and at
        least one normal app window open.

    WARNING: Full and ColdStart send real Alt+Tab input and switch between your
    windows while they run. Full takes about 4 minutes, ColdStart about 30 s.
    Use a separate virtual desktop with a few windows and do not type into
    anything important meanwhile.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File hook-lifecycle.ps1 -Exe .\winorbit.exe
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File hook-lifecycle.ps1 -Mode SelfTestFail; $LASTEXITCODE
#>
param(
    [string] $Exe,
    [ValidateSet('Full', 'ColdStart', 'SelfTestPass', 'SelfTestFail', 'SelfTestAbort')]
    [string] $Mode = 'Full',
    [int] $ExitIterations = 25,
    [int] $StallSeconds = 5,
    [int] $ProbeMinutes = 2,
    [int] $ColdRuns = 5
)

$ErrorActionPreference = 'Stop'
$failures = New-Object System.Collections.Generic.List[string]
$log = Join-Path (Get-Location) ("winorbit-stress-{0}-{1:yyyyMMdd-HHmmss}.log" -f $Mode, (Get-Date))
$sendsInput = $Mode -in @('Full', 'ColdStart')
$transcriptStarted = $false
$exitCode = 1
$helperLoaded = $false

$SC_ALT = 0x38; $SC_TAB = 0x0F; $SC_BACKTICK = 0x29; $SC_ESC = 0x01
$VK_ALT = 0x12; $VK_TAB = 0x09; $VK_MASK = 0xE8
# Unassigned virtual keys: no app reacts to them and WinOrbit ignores them.
$VK_SENTINEL = 0x97; $VK_DETECTOR = 0x98
$WM_COMMAND = 0x0111; $IDM_EXIT = 1

function Release-All {
    [W]::Key($SC_TAB, $true); [W]::Key($SC_BACKTICK, $true); [W]::Key($SC_ESC, $true)
    [W]::Vk($VK_DETECTOR, $true); [W]::Vk($VK_SENTINEL, $true)
    [W]::Key($SC_ALT, $true)
}

# Waits until every key injected so far has been through all keyboard hooks,
# using the sentinel key as an in-order marker. The deadline is a hang guard:
# a dead hook only delays input by the system hook timeout (at most ~1 s).
function Wait-InputDrained {
    [W]::Vk($VK_SENTINEL, $false)
    $deadline = (Get-Date).AddSeconds(10)
    while (-not [W]::IsDown($VK_SENTINEL)) {
        if ((Get-Date) -gt $deadline) { throw "sentinel key never arrived; input is stuck" }
        Start-Sleep -Milliseconds 2
    }
    [W]::Vk($VK_SENTINEL, $true)
    while ([W]::IsDown($VK_SENTINEL)) {
        if ((Get-Date) -gt $deadline) { throw "sentinel key never released; input is stuck" }
        Start-Sleep -Milliseconds 2
    }
}

# Alt + an unassigned key WinOrbit passes through: must be seen down, which
# proves "consumed" and "passed through" can be told apart on this machine.
function Test-Detector {
    try {
        [W]::Key($SC_ALT, $false)
        [W]::Vk($VK_DETECTOR, $false)
        Wait-InputDrained
        return [W]::IsDown($VK_DETECTOR)
    } finally {
        [W]::Vk($VK_DETECTOR, $true)
        [W]::Key($SC_ALT, $true)
    }
}

# 'Intercepted' if WinOrbit's hook consumed Alt+Tab, 'PassedThrough' if not.
# Cancels with Esc before releasing Alt so nothing is switched.
function Test-HookIntercepts {
    try {
        [W]::Key($SC_ALT, $false)
        [W]::Key($SC_TAB, $false)
        Wait-InputDrained
        $tabDown = [W]::IsDown($VK_TAB)
        [W]::Key($SC_TAB, $true)
        [W]::Key($SC_ESC, $false); [W]::Key($SC_ESC, $true)
    } finally {
        [W]::Key($SC_TAB, $true)
        [W]::Key($SC_ALT, $true)
    }
    Wait-InputDrained
    if ($tabDown) { return 'PassedThrough' }
    return 'Intercepted'
}

function Wait-OverlayHidden($app) {
    $deadline = (Get-Date).AddSeconds(5)
    while ([W]::IsWindowVisible($app.Hwnd) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 10
    }
}

# Milliseconds from Alt+Tab until the overlay is visible, or $null if it does
# not appear within 10 s. Cancels with Esc so nothing is switched.
function Measure-OverlayMs($app) {
    $ms = $null
    try {
        [W]::Key($SC_ALT, $false)
        $watch = [Diagnostics.Stopwatch]::StartNew()
        [W]::Key($SC_TAB, $false); [W]::Key($SC_TAB, $true)
        while ($watch.ElapsedMilliseconds -lt 10000) {
            if ([W]::IsWindowVisible($app.Hwnd)) { $ms = $watch.ElapsedMilliseconds; break }
            Start-Sleep -Milliseconds 1
        }
        [W]::Key($SC_ESC, $false); [W]::Key($SC_ESC, $true)
    } finally {
        [W]::Key($SC_ALT, $true)
    }
    Wait-OverlayHidden $app
    return $ms
}

function Format-Ms($ms) {
    if ($null -eq $ms) { return 'not shown in 10 s' }
    return "$ms ms"
}

function Start-WinOrbit {
    if (Get-Process -Name winorbit -ErrorAction SilentlyContinue) {
        throw "Another WinOrbit is running; exit it first."
    }
    $p = Start-Process -FilePath $Exe -WorkingDirectory (Split-Path $Exe) -PassThru
    $deadline = (Get-Date).AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 50
        # [NullString]::Value, not $null: PowerShell turns $null into "" for a
        # string argument, and FindWindow would then only match an empty title.
        $hwnd = [W]::FindWindow("WinOrbit", [NullString]::Value)
    } while ($hwnd -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline)
    if ($hwnd -eq [IntPtr]::Zero) {
        if (-not $p.HasExited) { $p.Kill() }
        throw "WinOrbit window did not appear"
    }
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

# Runs Alt+Tab / Alt+` traffic on a background runspace until stopped.
function Start-Traffic {
    $script:trafficFlag = [hashtable]::Synchronized(@{ Stop = $false })
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

function Get-Median([double[]] $values) {
    $sorted = @($values | Sort-Object)
    $n = $sorted.Count
    if ($n % 2 -eq 1) { return $sorted[($n - 1) / 2] }
    return ($sorted[$n / 2 - 1] + $sorted[$n / 2]) / 2
}

function Write-LatencySummary([string] $label, $samples) {
    $shown = @($samples | Where-Object { $null -ne $_ })
    $missing = $samples.Count - $shown.Count
    if ($shown.Count -eq 0) {
        Write-Host ("  {0}: overlay never shown in {1} run(s)" -f $label, $samples.Count)
        return
    }
    $min = ($shown | Measure-Object -Minimum).Minimum
    $max = ($shown | Measure-Object -Maximum).Maximum
    Write-Host ("  {0}: min {1} ms, median {2} ms, max {3} ms over {4} run(s); not shown: {5}" -f `
        $label, $min, (Get-Median $shown), $max, $shown.Count, $missing)
}

function Assert-Intercepted([string] $where) {
    $result = Test-HookIntercepts
    Write-Host ("  hook probe {0}: {1}" -f $where, $result)
    if ($result -ne 'Intercepted') { $failures.Add("hook: Alt+Tab not intercepted $where") }
}

function Invoke-Full {
    # 1. Detector, then cold/warm overlay latency on a fresh launch.
    Write-Host "`n[1] Detector check and overlay latency"
    $app = Start-WinOrbit
    $detector = Test-Detector
    Write-Host "  detector (Alt + unassigned key seen down): $detector"
    if (-not $detector) { throw "detector check failed; hook probes cannot be trusted on this machine" }
    $cold = Measure-OverlayMs $app
    Start-Sleep -Milliseconds 500
    $warm = Measure-OverlayMs $app
    Write-Host ("  overlay latency: cold {0}, warm {1}" -f (Format-Ms $cold), (Format-Ms $warm))
    if ($null -eq $cold -or $null -eq $warm) { $failures.Add("ui: overlay not shown within 10 s after launch") }
    Assert-Intercepted "after launch"
    $result = Stop-WinOrbit $app
    if (-not $result.Exited -or $result.Code -ne 0) { $failures.Add("exit after [1] failed") }

    # 2. Exit under hotkey traffic.
    Write-Host "`n[2] Exit while hotkey traffic is in flight x$ExitIterations"
    for ($i = 1; $i -le $ExitIterations; $i++) {
        $app = Start-WinOrbit
        $traffic = Start-Traffic
        Start-Sleep -Milliseconds (100 + (Get-Random -Maximum 400))
        $result = Stop-WinOrbit $app
        Stop-Traffic $traffic
        $line = "  run {0,2}: exited={1} code={2}" -f $i, $result.Exited, $result.Code
        Write-Host $line
        if (-not $result.Exited -or $result.Code -ne 0) { $failures.Add("exit run $i -> $line") }
    }

    # 3. Stalled UI thread.
    Write-Host "`n[3] UI thread stalled for $StallSeconds s while hotkeys keep firing"
    $app = Start-WinOrbit
    Write-Host ("  warm-up overlay: {0}" -f (Format-Ms (Measure-OverlayMs $app)))
    Assert-Intercepted "before stall"
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
        $traffic = Start-Traffic
        Start-Sleep -Seconds $StallSeconds
        Stop-Traffic $traffic
        for ($i = 1; $i -le 3; $i++) { Assert-Intercepted "during stall #$i" }
    } finally {
        foreach ($h in $suspended) { [void][W]::ResumeThread($h); [void][W]::CloseHandle($h) }
        Release-All
    }
    $out = [IntPtr]::Zero
    # SMTO_ABORTIFHUNG; WM_NULL round trip proves the UI thread is back.
    $responsive = [W]::SendMessageTimeout($app.Hwnd, 0, [IntPtr]::Zero, [IntPtr]::Zero, 2, 5000, [ref]$out) -ne [IntPtr]::Zero
    Write-Host "  UI responsive after resume: $responsive"
    if (-not $responsive) { $failures.Add("ui: not responsive after resume") }
    Wait-OverlayHidden $app
    Assert-Intercepted "after stall"
    $after = Measure-OverlayMs $app
    Write-Host ("  overlay after resume: {0}" -f (Format-Ms $after))
    if ($null -eq $after) { $failures.Add("ui: overlay not shown within 10 s after resume") }

    # 4. Liveness over time.
    Write-Host "`n[4] Hook probe every 15 s for $ProbeMinutes min"
    $deadline = (Get-Date).AddMinutes($ProbeMinutes)
    $n = 0
    while ((Get-Date) -lt $deadline) {
        $n++
        Assert-Intercepted ("#{0} at {1:HH:mm:ss}" -f $n, (Get-Date))
        Start-Sleep -Seconds 15
    }

    $result = Stop-WinOrbit $app
    Write-Host "  final exit: exited=$($result.Exited) code=$($result.Code)"
    if (-not $result.Exited -or $result.Code -ne 0) { $failures.Add("final exit failed") }
}

function Invoke-ColdStart {
    Write-Host "`nCold-start overlay latency x$ColdRuns (fresh launch each run)"
    $cold = @(); $warm = @()
    for ($i = 1; $i -le $ColdRuns; $i++) {
        $app = Start-WinOrbit
        $c = Measure-OverlayMs $app
        Start-Sleep -Milliseconds 500
        $w = Measure-OverlayMs $app
        $result = Stop-WinOrbit $app
        Write-Host ("  run {0}: cold {1}, warm {2}, exit code {3}" -f $i, (Format-Ms $c), (Format-Ms $w), $result.Code)
        $cold += , $c; $warm += , $w
        Start-Sleep -Milliseconds 500
    }
    Write-LatencySummary "cold" $cold
    Write-LatencySummary "warm" $warm
}

try {
    Start-Transcript -Path $log | Out-Null
    $transcriptStarted = $true
    Write-Host "Log: $log"
    Write-Host "Mode: $Mode"

    if ($sendsInput) {
        if (-not $Exe) { throw "-Exe is required for mode $Mode" }
        $Exe = (Resolve-Path $Exe).Path
        Write-Host "Exe: $Exe"
        Write-Host ("Exe sha256: {0}" -f (Get-FileHash -Algorithm SHA256 $Exe).Hash)
        Write-Host ("Windows: {0}" -f (Get-CimInstance Win32_OperatingSystem | ForEach-Object { "$($_.Caption) $($_.Version)" }))
        $ini = Join-Path (Split-Path $Exe) 'winorbit.ini'
        if (Test-Path $ini) {
            Write-Host "winorbit.ini:"
            Get-Content $ini | Where-Object { $_ -match '^\s*[^#\s]' } | ForEach-Object { Write-Host "  $_" }
        }

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

    // Virtual-key only (scan code 0), for keys WinOrbit never matches.
    public static void Vk(byte vk, bool up) {
        keybd_event(vk, 0, up ? 2u : 0u, UIntPtr.Zero);
    }

    // Async key state is only updated for input no keyboard hook consumed.
    public static bool IsDown(int vk) {
        return (GetAsyncKeyState(vk) & 0x8000) != 0;
    }
}
"@
        $helperLoaded = $true
    }

    switch ($Mode) {
        'Full' { Invoke-Full }
        'ColdStart' { Invoke-ColdStart }
        'SelfTestPass' { Write-Host "self-test: no failures recorded" }
        'SelfTestFail' { $failures.Add("self-test: forced failure") }
        'SelfTestAbort' { throw "self-test: forced abort" }
    }
} catch {
    # A run that aborts part-way must never report PASS.
    $failures.Add("aborted: $_")
    if ($sendsInput) {
        Get-Process -Name winorbit -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    }
} finally {
    if ($helperLoaded) {
        try {
            Release-All
            # Nothing may be left logically down.
            Start-Sleep -Milliseconds 200
            $stuck = @()
            foreach ($key in @(@('Alt', $VK_ALT), @('Tab', $VK_TAB), @('mask vkE8', $VK_MASK), @('sentinel', $VK_SENTINEL), @('detector', $VK_DETECTOR))) {
                if ([W]::IsDown($key[1])) { $stuck += $key[0] }
            }
            Write-Host ("`n[5] keys left down: {0}" -f $(if ($stuck.Count) { $stuck -join ', ' } else { 'none' }))
            foreach ($name in $stuck) { $failures.Add("$name left down") }
        } catch {
            $failures.Add("cleanup failed: $_")
        }
    }

    if ($failures.Count -eq 0) {
        Write-Host "`nRESULT: PASS" -ForegroundColor Green
    } else {
        Write-Host "`nRESULT: FAIL" -ForegroundColor Red
        $failures | ForEach-Object { Write-Host "  - $_" }
    }
    $exitCode = [int]($failures.Count -ne 0)
    Write-Host "exit code: $exitCode"
    if ($transcriptStarted) {
        try { Stop-Transcript | Out-Null } catch { }
    }
}
exit $exitCode
