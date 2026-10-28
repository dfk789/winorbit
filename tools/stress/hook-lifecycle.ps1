<#
.SYNOPSIS
    Windows checks for the WinOrbit keyboard hook: injected input is ignored,
    and the hook lifecycle survives exit and a stalled UI thread.

.DESCRIPTION
    Since TASK-S06C WinOrbit ignores every key event Windows flags as injected
    (LLKHF_INJECTED / LLKHF_LOWER_IL_INJECTED). Everything this script sends
    with keybd_event is injected, so it can no longer drive WinOrbit's hotkey
    state at all. The script therefore:

      * checks that injected input is ignored: injected keys must pass
        through WinOrbit (Windows marks them down), the overlay must stay
        hidden, and WinOrbit's debug log must show no switch messages;
      * uses YOUR physical Alt+Tab, when prompted, as the positive control
        that the hook is alive and handles real keys, before and after the
        injected phase and around the UI stall;
      * keeps the lifecycle checks that stay honest with injected input:
        exit while the hook callback is being invoked, and responsiveness of
        the hook's filtered path while the UI thread is suspended.

    Retired after S06C, because their premise needed WinOrbit to act on
    injected input: the injected-Alt+Tab interception probe, the periodic
    liveness probes, and ColdStart overlay latency (it opened the overlay with
    injected Alt+Tab). Overlay latency now needs physical input.

    How "passed through" is detected: after the keys, an unassigned sentinel
    key is injected and the script waits until Windows reports it down. Input
    is processed in order, so by then the earlier keys have been through all
    keyboard hooks, and Windows only marks a key down if no hook consumed it.
    A detector check (an unassigned key, which must be seen down) proves this
    works on the machine before any result is trusted.

    Modes:
      Full           1. Injected-input check with physical controls.
                     2. Tray "Exit" while injected keys keep invoking the
                        hook, repeated; every run must exit 0 in time.
                     3. UI thread stalled (every thread except
                        "keyboard-hook" suspended): filtered-path latency,
                        a physical Alt+Tab and typing during the stall, and
                        recovery afterwards.
                     4. No Alt, Tab, Shift, Esc, mask (vkE8) or probe key
                        left down.
      Injected       Step 1 only.
      SelfTestPass   Sends no input. Exits 0.
      SelfTestFail   Sends no input. Records one failure; exits non-zero.
      SelfTestAbort  Sends no input. Throws mid-run; exits non-zero.

    Exit code: 0 only if the run completed and recorded no failure; 1
    otherwise (including aborted runs).

    Requirements for Full and Injected:
      - No other WinOrbit instance running.
      - winorbit.ini next to the exe with [switch-apps] enable = yes and
        [log] level = debug with a log path (the switch messages are read
        from the log), and at least two normal app windows open.

    The injected Alt+Tab opens the NATIVE Windows switcher for a moment; that
    is expected, because WinOrbit ignores it. The script closes it with Esc.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File hook-lifecycle.ps1 -Exe .\winorbit.exe
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File hook-lifecycle.ps1 -Mode SelfTestFail; $LASTEXITCODE
#>
param(
    [string] $Exe,
    [ValidateSet('Full', 'Injected', 'SelfTestPass', 'SelfTestFail', 'SelfTestAbort')]
    [string] $Mode = 'Full',
    [int] $ExitIterations = 25,
    [int] $StallSeconds = 10
)

$ErrorActionPreference = 'Stop'
$failures = New-Object System.Collections.Generic.List[string]
$log = Join-Path (Get-Location) ("winorbit-stress-{0}-{1:yyyyMMdd-HHmmss}.log" -f $Mode, (Get-Date))
$sendsInput = $Mode -in @('Full', 'Injected')
$transcriptStarted = $false
$exitCode = 1
$helperLoaded = $false

$SC_ALT = 0x38; $SC_TAB = 0x0F; $SC_BACKTICK = 0x29; $SC_ESC = 0x01; $SC_SHIFT = 0x2A
$VK_ALT = 0x12; $VK_TAB = 0x09; $VK_SHIFT = 0x10; $VK_ESC = 0x1B; $VK_MASK = 0xE8
# Unassigned virtual keys: no app reacts to them and WinOrbit ignores them.
$VK_SENTINEL = 0x97; $VK_DETECTOR = 0x98; $VK_TRAFFIC = 0x99
$WM_COMMAND = 0x0111; $IDM_EXIT = 1
# Two blocked hook calls (sentinel down and up) would cost at least two system
# hook timeouts; a healthy filtered path takes a few milliseconds.
$DRAIN_LIMIT_MS = 1000

function Release-All {
    [W]::Key($SC_TAB, $true); [W]::Key($SC_BACKTICK, $true); [W]::Key($SC_ESC, $true)
    [W]::Key($SC_SHIFT, $true)
    [W]::Vk($VK_DETECTOR, $true); [W]::Vk($VK_SENTINEL, $true); [W]::Vk($VK_TRAFFIC, $true)
    [W]::Key($SC_ALT, $true)
}

# Waits until every key injected so far has been through all keyboard hooks,
# using the sentinel key as an in-order marker. Returns the milliseconds it
# took. The deadline is a hang guard only.
function Wait-InputDrained {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    [W]::Vk($VK_SENTINEL, $false)
    while (-not [W]::IsDown($VK_SENTINEL)) {
        if ($watch.ElapsedMilliseconds -gt 10000) { throw "sentinel key never arrived; input is stuck" }
        Start-Sleep -Milliseconds 2
    }
    [W]::Vk($VK_SENTINEL, $true)
    while ([W]::IsDown($VK_SENTINEL)) {
        if ($watch.ElapsedMilliseconds -gt 10000) { throw "sentinel key never released; input is stuck" }
        Start-Sleep -Milliseconds 2
    }
    return $watch.ElapsedMilliseconds
}

# An unassigned key must be seen down: proves "passed through" is observable.
function Test-Detector {
    try {
        [W]::Vk($VK_DETECTOR, $false)
        [void](Wait-InputDrained)
        return [W]::IsDown($VK_DETECTOR)
    } finally {
        [W]::Vk($VK_DETECTOR, $true)
    }
}

function Wait-OverlayHidden($app, [int] $seconds = 5) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ([W]::IsWindowVisible($app.Hwnd) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 10
    }
}

# Injects the held keys, then $key, and reports whether $key passed through
# WinOrbit and whether the overlay ever became visible. Cancels a native
# switcher with Esc before releasing the held keys. The key is checked under
# the virtual key it was injected as, which depends on the keyboard layout
# (scan 0x29 is VK_OEM_3 on US layouts but VK_OEM_8 on UK ones).
function Test-InjectedChord($app, [string] $label, [uint32[]] $holdScans, [uint32] $keyScan) {
    $keyVk = [W]::VkOf($keyScan)
    $passed = $false
    $overlay = $false
    try {
        foreach ($scan in $holdScans) { [W]::Key($scan, $false) }
        [W]::Key($keyScan, $false)
        [void](Wait-InputDrained)
        $passed = [W]::IsDown($keyVk)
        [W]::Key($keyScan, $true)
        $deadline = (Get-Date).AddMilliseconds(400)
        while ((Get-Date) -lt $deadline) {
            if ([W]::IsWindowVisible($app.Hwnd)) { $overlay = $true }
            Start-Sleep -Milliseconds 10
        }
        if ($holdScans.Count -gt 0 -and $keyScan -ne $SC_ESC) { [W]::Key($SC_ESC, $false); [W]::Key($SC_ESC, $true) }
    } finally {
        [W]::Key($keyScan, $true)
        [array]::Reverse($holdScans)
        foreach ($scan in $holdScans) { [W]::Key($scan, $true) }
    }
    [void](Wait-InputDrained)
    if ([W]::IsWindowVisible($app.Hwnd)) { $overlay = $true }
    Write-Host ("  injected {0,-22} (vk 0x{3:X2}) passed through: {1,-5} WinOrbit overlay shown: {2}" -f $label, $passed, $overlay, $keyVk)
    if (-not $passed) { $failures.Add("injected: $label was consumed") }
    if ($overlay) { $failures.Add("injected: $label showed the WinOrbit overlay") }
}

# Prompts for a physical Alt+Tab and waits for the overlay. Returns whether it
# was seen. This is the positive control that the hook handles real keys.
function Wait-PhysicalAltTab($app, [string] $when) {
    Write-Host ""
    Write-Host "  >>> NOW, on your keyboard: hold Alt, tap Tab once, release Alt  ($when)" -ForegroundColor Yellow
    $seen = $false
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        if ([W]::IsWindowVisible($app.Hwnd)) { $seen = $true; break }
        Start-Sleep -Milliseconds 10
    }
    if ($seen) { Wait-OverlayHidden $app 60 }
    Write-Host "  physical Alt+Tab $when -> WinOrbit overlay shown: $seen"
    if (-not $seen) { $failures.Add("physical: no WinOrbit overlay for physical Alt+Tab $when") }
    Start-Sleep -Milliseconds 1500
    return $seen
}

function Get-IniValue([string] $ini, [string] $section, [string] $key) {
    $current = ''
    foreach ($line in Get-Content $ini) {
        if ($line -match '^\s*\[(.+)\]\s*$') { $current = $Matches[1]; continue }
        if ($current -eq $section -and $line -match "^\s*$key\s*=\s*(.*?)\s*$") { return $Matches[1] }
    }
    return ''
}

# WinOrbit's log file and its current length, so only this run is read back.
function Get-LogInfo {
    $ini = Join-Path (Split-Path $Exe) 'winorbit.ini'
    $level = Get-IniValue $ini 'log' 'level'
    $path = Get-IniValue $ini 'log' 'path'
    if ($level -ne 'debug' -or -not $path) {
        throw "winorbit.ini must set [log] level = debug and a path; the switch messages are read from the log"
    }
    if (-not [IO.Path]::IsPathRooted($path)) { $path = Join-Path (Split-Path $Exe) $path }
    $offset = 0
    if (Test-Path $path) { $offset = (Get-Item $path).Length }
    return @{ Path = $path; Offset = $offset }
}

# Switch messages WinOrbit handled during this run, with their log time in
# seconds since WinOrbit started logging.
function Read-SwitchMessages($logInfo) {
    $stream = [IO.File]::Open($logInfo.Path, 'Open', 'Read', 'ReadWrite')
    try {
        [void]$stream.Seek($logInfo.Offset, 'Begin')
        $text = (New-Object IO.StreamReader($stream)).ReadToEnd()
    } finally {
        $stream.Dispose()
    }
    $messages = @()
    foreach ($line in $text -split "`n") {
        if ($line -match '^\[(\d+):(\d+):(\d+)\.(\d+)\].*message (WM_USER_\w+)') {
            $t = [int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [int]$Matches[3] + [int]$Matches[4] / 1000.0
            $messages += [pscustomobject]@{ T = $t; Message = $Matches[5] }
        }
    }
    return $messages
}

function Count-Messages($messages, [double] $from, [double] $to) {
    return @($messages | Where-Object { $_.T -ge $from -and $_.T -le $to }).Count
}

function Start-WinOrbit {
    if (Get-Process -Name winorbit -ErrorAction SilentlyContinue) {
        throw "Another WinOrbit is running; exit it first."
    }
    $p = Start-Process -FilePath $Exe -WorkingDirectory (Split-Path $Exe) -PassThru
    $clock = [Diagnostics.Stopwatch]::StartNew()
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
    # Clock starts at launch, so it is at or ahead of WinOrbit's log clock.
    return @{ Process = $p; Hwnd = $hwnd; Clock = $clock }
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

# Injects an unassigned key on a background runspace until stopped, so the
# hook callback keeps being invoked. It does not switch anything.
function Start-Traffic {
    $script:trafficFlag = [hashtable]::Synchronized(@{ Stop = $false })
    $ps = [PowerShell]::Create()
    [void]$ps.AddScript({
        param($flag, $vk)
        while (-not $flag.Stop) {
            [W]::Vk($vk, $false); [W]::Vk($vk, $true)
            Start-Sleep -Milliseconds 2
        }
    }).AddArgument($script:trafficFlag).AddArgument([byte]$VK_TRAFFIC)
    $handle = $ps.BeginInvoke()
    return @{ PS = $ps; Handle = $handle }
}

function Stop-Traffic($traffic) {
    $script:trafficFlag.Stop = $true
    [void]$traffic.PS.EndInvoke($traffic.Handle)
    $traffic.PS.Dispose()
    Release-All
}

function Invoke-InjectedCheck {
    Write-Host "`n[1] Injected input is ignored (physical Alt+Tab as control)"
    $logInfo = Get-LogInfo
    $app = Start-WinOrbit
    try {
        $detector = Test-Detector
        Write-Host "  detector (unassigned key seen down): $detector"
        if (-not $detector) { throw "detector check failed; pass-through cannot be observed on this machine" }

        [void](Wait-PhysicalAltTab $app "before the injected keys")
        Write-Host "`n  Injecting keys now. The native Windows switcher may flash; that is expected."
        $injectedFrom = $app.Clock.Elapsed.TotalSeconds
        Test-InjectedChord $app "Alt+Tab" @($SC_ALT) $SC_TAB
        Test-InjectedChord $app "Shift+Alt+Tab" @($SC_ALT, $SC_SHIFT) $SC_TAB
        Test-InjectedChord $app "Alt+`` (backtick)" @($SC_ALT) $SC_BACKTICK
        Test-InjectedChord $app "Alt (down/up)" @() $SC_ALT
        Test-InjectedChord $app "Shift (down/up)" @() $SC_SHIFT
        Test-InjectedChord $app "Esc" @() $SC_ESC
        # Let any (wrongly) posted message reach the log before the window ends.
        Start-Sleep -Milliseconds 1000
        $injectedTo = $app.Clock.Elapsed.TotalSeconds

        [void](Wait-PhysicalAltTab $app "after the injected keys")
    } finally {
        $result = Stop-WinOrbit $app
    }
    if (-not $result.Exited -or $result.Code -ne 0) { $failures.Add("exit after [1] failed (code $($result.Code))") }

    # Read after exit so nothing is still buffered.
    $messages = Read-SwitchMessages $logInfo
    # WinOrbit's log clock starts a little after the script's launch clock, so
    # log times run slightly behind; widen the start of the window by 1 s. The
    # physical control before it always ends at least 1.5 s earlier.
    $injectedFrom = [Math]::Max(0, $injectedFrom - 1.0)
    $injected = Count-Messages $messages $injectedFrom $injectedTo
    $total = @($messages).Count
    Write-Host ("  WinOrbit switch messages: {0} in total, {1} during the injected keys ({2:N1}-{3:N1} s)" -f $total, $injected, $injectedFrom, $injectedTo)
    if ($injected -ne 0) {
        $failures.Add("injected: WinOrbit handled $injected switch message(s) during injected input")
        $messages | Where-Object { $_.T -ge $injectedFrom -and $_.T -le $injectedTo } | ForEach-Object { Write-Host ("    {0:N3} s {1}" -f $_.T, $_.Message) }
    }
    if ($total -eq 0) { $failures.Add("log: no switch messages at all; the physical controls or log reading did not work") }
}

function Invoke-Full {
    Invoke-InjectedCheck

    # 2. Exit while the hook callback is being invoked. Injected events return
    # before the hook state is locked, so this covers the hook thread and the
    # exit path, not state-lock contention (unit-tested instead).
    Write-Host "`n[2] Exit while injected keys keep invoking the hook x$ExitIterations"
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
    Write-Host "`n[3] UI thread stalled for $StallSeconds s"
    $app = Start-WinOrbit
    [void](Wait-PhysicalAltTab $app "before the stall")
    $baseline = Wait-InputDrained
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
        Write-Host "  suspended $($suspended.Count) thread(s)"
        $drains = @()
        for ($i = 0; $i -lt 5; $i++) { $drains += Wait-InputDrained }
        $worst = ($drains | Measure-Object -Maximum).Maximum
        Write-Host ("  filtered-path latency (injected sentinel through the hook): baseline {0} ms, during stall max {1} ms" -f $baseline, $worst)
        if ($worst -gt $DRAIN_LIMIT_MS) { $failures.Add("stall: hook took $worst ms to pass injected input") }
        Write-Host ""
        Write-Host "  >>> NOW, within $StallSeconds s: hold Alt, tap Tab twice, release Alt," -ForegroundColor Yellow
        Write-Host "  >>> then type a few words anywhere. Typing must not lag. (Nothing will appear yet.)" -ForegroundColor Yellow
        Start-Sleep -Seconds $StallSeconds
    } finally {
        foreach ($h in $suspended) { [void][W]::ResumeThread($h); [void][W]::CloseHandle($h) }
        Release-All
    }
    $out = [IntPtr]::Zero
    # SMTO_ABORTIFHUNG; WM_NULL round trip proves the UI thread is back.
    $responsive = [W]::SendMessageTimeout($app.Hwnd, 0, [IntPtr]::Zero, [IntPtr]::Zero, 2, 5000, [ref]$out) -ne [IntPtr]::Zero
    Write-Host "  UI responsive after resume: $responsive"
    if (-not $responsive) { $failures.Add("ui: not responsive after resume") }
    Wait-OverlayHidden $app 10
    [void](Wait-PhysicalAltTab $app "after the stall")
    $result = Stop-WinOrbit $app
    Write-Host "  final exit: exited=$($result.Exited) code=$($result.Code)"
    if (-not $result.Exited -or $result.Code -ne 0) { $failures.Add("final exit failed") }
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

    // Scan code plus its virtual key, like a real key. Windows still flags it
    // as injected (LLKHF_INJECTED).
    public static void Key(uint scan, bool up) {
        keybd_event(VkOf(scan), (byte)scan, up ? 2u : 0u, UIntPtr.Zero);
    }

    // The virtual key Key() sends for a scan code, in the active layout.
    public static byte VkOf(uint scan) {
        return (byte)MapVirtualKey(scan, 1);
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
        'Injected' { Invoke-InjectedCheck }
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
            foreach ($key in @(@('Alt', $VK_ALT), @('Tab', $VK_TAB), @('Shift', $VK_SHIFT), @('Esc', $VK_ESC), @('backtick', [W]::VkOf($SC_BACKTICK)), @('mask vkE8', $VK_MASK), @('sentinel', $VK_SENTINEL), @('detector', $VK_DETECTOR), @('traffic', $VK_TRAFFIC))) {
                if ([W]::IsDown($key[1])) { $stuck += $key[0] }
            }
            Write-Host ("`n[4] keys left down: {0}" -f $(if ($stuck.Count) { $stuck -join ', ' } else { 'none' }))
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
