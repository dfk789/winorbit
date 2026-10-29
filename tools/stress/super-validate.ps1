<#
.SYNOPSIS
    Physical-keyboard validation of WinOrbit's bare Win/Super tap handling
    (TASK-S06D).

.DESCRIPTION
    Starts the given winorbit.exe and walks you through prompts on your real
    keyboard. Each trial checks, without asking you, what can be measured:

      * whether Start (StartMenuExperienceHost / SearchHost) came to the
        foreground,
      * how many overview actions WinOrbit logged ("bare Win tap" and
        "bare Win double tap" lines in its log),

    and asks you what cannot be measured (did Start flash, did the native
    shortcut do its thing). A trial fails if either disagrees with the
    expectation.

    The script installs its own WH_KEYBOARD_LL hook only to OBSERVE: it never
    consumes or injects into the key stream it watches, and is used only to
    notice when you have released the Windows key. It is installed after
    WinOrbit's hook, so Windows calls it first; it passes every event on.
    The only keys it injects are Esc (to close Start or a menu it opened for
    a trial) and key-ups for anything still down at the end.

    Trials:
      [1] Clean taps: LWin, RWin (skippable), 2 s hold, 10 taps, double tap,
          two slow taps, Shift held + Win.
      [2] Native chord matrix: Win+E/D/R/X/1/A/Shift+S/Ctrl+O/Tab/Left. Each
          must do its native thing and log no overview action.
      [3] Win+L lock/unlock, optional UAC prompt, Ctrl+Alt+Del > Cancel:
          each must log an input reset, and the next tap must work.
      [4] Elevated foreground (Task Manager): Windows sends a non-elevated
          WinOrbit no keys there, so Start must open natively and nothing is
          logged.
      [5] Exit and relaunch: exit code 0, tap works again; with WinOrbit
          exited a tap opens Start natively.
      [6] Disabled: a copy run with [overview] enable = no; taps open Start.
      [7] No Win, Shift, Ctrl, Alt or vkE8 key left down.

    Exit code: 0 only if every trial passed and the run completed.

    Requirements: WinOrbit not running; winorbit.ini next to the exe with
    [overview] enable = yes, double_tap_ms = 400, and [log] level = debug or
    info with a path. Run from a NON-elevated terminal.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File super-validate.ps1 -Exe .\winorbit.exe
#>
param(
    [Parameter(Mandatory = $true)] [string] $Exe,
    [switch] $SkipUac,
    [switch] $SkipLock
)

$ErrorActionPreference = 'Stop'
$failures = New-Object System.Collections.Generic.List[string]
$stamp = '{0:yyyyMMdd-HHmmss}' -f (Get-Date)
$log = Join-Path (Get-Location) "winorbit-s06d-validate-$stamp.log"
$transcriptStarted = $false
$helperLoaded = $false
$observerStarted = $false
$app = $null
$exitCode = 1
$hiddenStartSeen = New-Object System.Collections.Generic.HashSet[string]
$WM_COMMAND = 0x0111; $IDM_EXIT = 1

Add-Type -TypeDefinition @"
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class V {
    delegate IntPtr HookProc(int code, IntPtr wParam, IntPtr lParam);
    [StructLayout(LayoutKind.Sequential)]
    struct KBDLLHOOKSTRUCT { public uint vkCode; public uint scanCode; public uint flags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    struct MSG { public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public int x; public int y; }

    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr SetWindowsHookEx(int id, HookProc proc, IntPtr hMod, uint threadId);
    [DllImport("user32.dll")] static extern bool UnhookWindowsHookEx(IntPtr hook);
    [DllImport("user32.dll")] static extern IntPtr CallNextHookEx(IntPtr hook, int code, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] static extern int GetMessage(out MSG msg, IntPtr hwnd, uint min, uint max);
    [DllImport("user32.dll")] static extern bool PostThreadMessage(uint threadId, uint msg, IntPtr w, IntPtr l);
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("kernel32.dll")] static extern IntPtr GetModuleHandle(string name);
    [DllImport("user32.dll")] public static extern short GetAsyncKeyState(int vk);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder name, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string name);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr hwnd, int attr, out int value, int size);

    const uint LLKHF_INJECTED = 0x10, LLKHF_LOWER_IL_INJECTED = 0x02, LLKHF_UP = 0x80;
    static readonly HookProc Proc = Callback;
    static IntPtr hook = IntPtr.Zero;
    static uint threadId;
    static Thread thread;
    static bool lwin, rwin;

    // Physical Win holds that ended (last Win key released).
    public static volatile int WinReleases;
    // Physical non-Win key presses seen while a Win key was held.
    public static volatile int ChordKeys;

    static IntPtr Callback(int code, IntPtr wParam, IntPtr lParam) {
        if (code >= 0) {
            KBDLLHOOKSTRUCT k = (KBDLLHOOKSTRUCT)Marshal.PtrToStructure(lParam, typeof(KBDLLHOOKSTRUCT));
            bool injected = (k.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED)) != 0;
            bool up = (k.flags & LLKHF_UP) != 0;
            if (!injected) {
                if (k.vkCode == 0x5B || k.vkCode == 0x5C) {
                    bool wasHeld = lwin || rwin;
                    if (k.vkCode == 0x5B) lwin = !up; else rwin = !up;
                    if (up && wasHeld && !lwin && !rwin) WinReleases++;
                } else if (!up && (lwin || rwin)) {
                    ChordKeys++;
                }
            }
        }
        // Observer only: every event goes on unchanged.
        return CallNextHookEx(hook, code, wParam, lParam);
    }

    public static void Start() {
        ManualResetEvent ready = new ManualResetEvent(false);
        string error = null;
        thread = new Thread(() => {
            hook = SetWindowsHookEx(13, Proc, GetModuleHandle(null), 0);
            if (hook == IntPtr.Zero) error = "SetWindowsHookEx failed: " + Marshal.GetLastWin32Error();
            threadId = GetCurrentThreadId();
            ready.Set();
            if (hook == IntPtr.Zero) return;
            MSG msg;
            while (GetMessage(out msg, IntPtr.Zero, 0, 0) > 0) { }
            UnhookWindowsHookEx(hook);
        });
        thread.IsBackground = true;
        thread.Start();
        ready.WaitOne();
        if (error != null) throw new InvalidOperationException(error);
    }

    public static void Stop() {
        if (thread == null) return;
        PostThreadMessage(threadId, 0x0012, IntPtr.Zero, IntPtr.Zero);
        thread.Join(2000);
        thread = null;
    }

    public static void ResetWin() { lwin = false; rwin = false; }

    public static bool IsVisible(IntPtr hwnd) { return IsWindowVisible(hwnd); }

    public static bool IsDown(int vk) { return (GetAsyncKeyState(vk) & 0x8000) != 0; }

    public static void Vk(byte vk, byte scan, bool up, bool extended) {
        keybd_event(vk, scan, (up ? 2u : 0u) | (extended ? 1u : 0u), UIntPtr.Zero);
    }

    public static string Foreground() {
        IntPtr hwnd = GetForegroundWindow();
        uint pid;
        GetWindowThreadProcessId(hwnd, out pid);
        StringBuilder cls = new StringBuilder(256);
        GetClassName(hwnd, cls, 256);
        string name = "?";
        try { name = Process.GetProcessById((int)pid).ProcessName; } catch (Exception) { }
        int cloaked = 0;
        DwmGetWindowAttribute(hwnd, 14, out cloaked, 4); // DWMWA_CLOAKED
        return name + " [" + cls + "]" + (IsWindowVisible(hwnd) ? "" : " invisible") + (cloaked != 0 ? " cloaked" : "");
    }
}
"@
$helperLoaded = $true

function Get-IniValue([string] $ini, [string] $section, [string] $key) {
    $current = ''
    foreach ($line in Get-Content $ini) {
        if ($line -match '^\s*\[(.+)\]\s*$') { $current = $Matches[1]; continue }
        if ($current -eq $section -and $line -match "^\s*$key\s*=\s*(.*?)\s*$") { return $Matches[1] }
    }
    return ''
}

function Get-LogPath([string] $exePath) {
    $ini = Join-Path (Split-Path $exePath) 'winorbit.ini'
    $path = Get-IniValue $ini 'log' 'path'
    $level = Get-IniValue $ini 'log' 'level'
    if (-not $path -or $level -notin @('info', 'debug', 'trace')) {
        throw "$ini must set [log] level = info or debug and a path; overview actions are read from the log"
    }
    if (-not [IO.Path]::IsPathRooted($path)) { $path = Join-Path (Split-Path $exePath) $path }
    return $path
}

function Get-LogLength([string] $path) {
    if (Test-Path $path) { return (Get-Item $path).Length }
    return 0
}

function Read-LogSince([string] $path, [long] $offset) {
    if (-not (Test-Path $path)) { return '' }
    $stream = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try {
        [void]$stream.Seek($offset, 'Begin')
        return (New-Object IO.StreamReader($stream)).ReadToEnd()
    } finally {
        $stream.Dispose()
    }
}

function Count-Taps([string] $text) {
    $toggles = ([regex]::Matches($text, 'bare Win tap \(time')).Count
    $seconds = ([regex]::Matches($text, 'bare Win double tap')).Count
    return @{ Toggle = $toggles; Second = $seconds }
}

function Start-App([string] $exePath) {
    if (Get-Process -Name winorbit -ErrorAction SilentlyContinue) { throw "Another WinOrbit is running; exit it first." }
    $p = Start-Process -FilePath $exePath -WorkingDirectory (Split-Path $exePath) -PassThru
    $deadline = (Get-Date).AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 50
        $hwnd = [V]::FindWindow("WinOrbit", [NullString]::Value)
    } while ($hwnd -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline)
    if ($hwnd -eq [IntPtr]::Zero) {
        if (-not $p.HasExited) { $p.Kill() }
        throw "WinOrbit window did not appear"
    }
    Start-Sleep -Milliseconds 300
    return @{ Process = $p; Hwnd = $hwnd }
}

function Stop-App($a, [int] $timeoutMs = 5000) {
    [void][V]::PostMessage($a.Hwnd, $WM_COMMAND, [IntPtr]$IDM_EXIT, [IntPtr]::Zero)
    if (-not $a.Process.WaitForExit($timeoutMs)) {
        $a.Process.Kill()
        return @{ Exited = $false; Code = $null }
    }
    return @{ Exited = $true; Code = $a.Process.ExitCode }
}

# Returns the foreground "process [class]" if it is Start, else $null. Start
# runs in SearchHost on Windows 11 23H2 (round-2 probe) and in
# StartMenuExperienceHost elsewhere. ShellExperienceHost is not counted: it
# also hosts Quick Settings (Win+A).
function Get-StartForeground {
    $fg = [V]::Foreground()
    # A cloaked or invisible Start host is not a visible Start menu: Windows
    # activates SearchHost briefly during Win+Left without showing anything.
    if ($fg -match '^(StartMenuExperienceHost|SearchHost) ' -and $fg -notmatch ' (cloaked|invisible)$') { return $fg }
    if ($fg -match '^(StartMenuExperienceHost|SearchHost) ' -and -not $script:hiddenStartSeen.Contains($fg)) {
        [void]$script:hiddenStartSeen.Add($fg)
        Write-Host "    (hidden Start host in foreground, not counted: $fg)"
    }
    return $null
}

function Test-StartForeground {
    return [bool](Get-StartForeground)
}

function Close-StartIfOpen {
    if (Test-StartForeground) {
        [V]::Vk(0x1B, 0x01, $false, $false); [V]::Vk(0x1B, 0x01, $true, $false)
        Start-Sleep -Milliseconds 400
    }
}

function Read-YesNo([string] $question) {
    while ($true) {
        $answer = (Read-Host "  $question (y/n)").Trim().ToLower()
        if ($answer -in @('y', 'n')) { return $answer -eq 'y' }
    }
}

function Check([string] $trial, [string] $what, $expected, $actual) {
    $ok = "$expected" -eq "$actual"
    $mark = if ($ok) { 'ok  ' } else { 'FAIL' }
    Write-Host ("    {0} {1}: expected {2}, got {3}" -f $mark, $what, $expected, $actual)
    if (-not $ok) { $failures.Add("${trial}: $what expected $expected, got $actual") }
}

# Waits for $releases Win releases (or Enter when $releases is 0), watching the
# foreground for Start the whole time, then checks the log and asks.
function Invoke-Trial {
    param(
        [string] $name, [string] $instruction, [int] $releases,
        [bool] $expectStart, [int] $expectToggle, [int] $expectSecond,
        [string] $nativeQuestion = '', [int] $seconds = 60, [switch] $Optional, [switch] $NoMeasure, [switch] $ExpectReset,
        [int] $expectCommits = -1, [switch] $ExpectStartClosedAtEnd, [switch] $ExpectOverlayHidden,
        [string] $logPath = $script:logPath
    )
    Close-StartIfOpen
    [V]::ResetWin()
    $before = [V]::WinReleases
    $offset = Get-LogLength $logPath
    Write-Host ""
    Write-Host "  [$name]" -ForegroundColor Cyan
    Write-Host "  >>> NOW: $instruction" -ForegroundColor Yellow
    $startSeen = $false
    $startWhat = $null
    if ($releases -eq 0) {
        [void](Read-Host "  Press Enter here when done")
        Start-Sleep -Milliseconds 300
        $startWhat = Get-StartForeground
        $startSeen = [bool]$startWhat
    } else {
        $deadline = (Get-Date).AddSeconds($seconds)
        $quietUntil = $null
        while ((Get-Date) -lt $deadline) {
            $fg = Get-StartForeground
            if ($fg) { $startSeen = $true; if (-not $startWhat) { $startWhat = $fg } }
            if (-not $quietUntil -and ([V]::WinReleases - $before) -ge $releases) {
                $quietUntil = (Get-Date).AddMilliseconds(1500)
            }
            if ($quietUntil -and (Get-Date) -gt $quietUntil) { break }
            Start-Sleep -Milliseconds 20
        }
        $done = [V]::WinReleases - $before
        if ($done -lt $releases) {
            if ($Optional) {
                Write-Host "    (skipped: saw $done of $releases Win releases)"
                return
            }
            $failures.Add("${name}: saw only $done of $releases Win releases")
        }
    }
    $startAtEnd = Get-StartForeground
    Close-StartIfOpen
    if ($startWhat) { Write-Host "    Start detected as: $startWhat" }
    $text = Read-LogSince $logPath $offset
    $taps = Count-Taps $text
    if ($ExpectReset) {
        $resets = @([regex]::Matches($text, 'input reset: ([^\r\n]+)') | ForEach-Object { $_.Groups[1].Value.Trim() })
        Write-Host ("    input resets logged: {0}" -f $(if ($resets.Count) { $resets -join ', ' } else { 'none' }))
        Check $name 'input reset logged' $true ($resets.Count -gt 0)
    }
    $userStart = Read-YesNo "Did the Start menu open or flash, even briefly?"
    if ($NoMeasure) {
        Write-Host "    (Start not measured: it closes when you come back here)"
    } else {
        Check $name 'Start in foreground (measured)' $expectStart $startSeen
    }
    Check $name 'Start open or flash (you)' $expectStart $userStart
    Check $name 'overview actions logged' $expectToggle $taps.Toggle
    Check $name 'double-tap actions logged' $expectSecond $taps.Second
    if ($expectCommits -ge 0) {
        # One commit per Alt hold, however many Tab presses it had.
        $commits = ([regex]::Matches($text, 'message WM_USER_SWITCH_APPS_DONE')).Count
        Check $name 'WinOrbit switch commits logged' $expectCommits $commits
    }
    if ($ExpectOverlayHidden) {
        Check $name 'WinOrbit overlay hidden (measured)' $true (-not [V]::IsVisible($script:app.Hwnd))
    }
    if ($ExpectStartClosedAtEnd) {
        Check $name 'Start closed at the end (measured)' $true (-not $startAtEnd)
    }
    if ($nativeQuestion) {
        Check $name $nativeQuestion $true (Read-YesNo $nativeQuestion)
    }
}

try {
    Start-Transcript -Path $log | Out-Null
    $transcriptStarted = $true
    $Exe = (Resolve-Path $Exe).Path
    Write-Host "Log: $log"
    Write-Host "Exe: $Exe"
    Write-Host ("Exe sha256: {0}" -f (Get-FileHash -Algorithm SHA256 $Exe).Hash)
    Write-Host ("Windows: {0}" -f (Get-CimInstance Win32_OperatingSystem | ForEach-Object { "$($_.Caption) $($_.Version)" }))
    $elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Write-Host "Terminal elevated: $elevated"
    if ($elevated) { throw "Run from a NON-elevated terminal (Win+R, powershell, plain Enter)." }
    $ini = Join-Path (Split-Path $Exe) 'winorbit.ini'
    Write-Host "winorbit.ini:"
    Get-Content $ini | Where-Object { $_ -match '^\s*[^#\s]' } | ForEach-Object { Write-Host "  $_" }
    $doubleTapMs = Get-IniValue $ini 'overview' 'double_tap_ms'
    if ((Get-IniValue $ini 'overview' 'enable') -ne 'yes' -or $doubleTapMs -notmatch '^\d+$' -or [int]$doubleTapMs -eq 0) {
        throw "$ini must set [overview] enable = yes and a non-zero double_tap_ms"
    }
    Write-Host "double tap window: $doubleTapMs ms"
    $script:logPath = Get-LogPath $Exe
    Write-Host "WinOrbit log: $script:logPath"

    $offset = Get-LogLength $script:logPath
    $app = Start-App $Exe
    $startup = Read-LogSince $script:logPath $offset
    Check 'startup' 'listener owns bare Win tap' $true ($startup -match "bare Win tap owned \(double tap $doubleTapMs ms\)")
    [V]::Start()
    $observerStarted = $true
    Write-Host "`nWinOrbit started. Keep this window focused unless a step says otherwise."
    Write-Host "When Start opens, the script closes it with Esc."

    Write-Host "`n[1] Clean taps"
    Invoke-Trial 'LWin tap' 'tap the LEFT Windows key once' 1 $false 1 0
    Invoke-Trial 'RWin tap' 'tap the RIGHT Windows key once (no right Win key? wait 15 s to skip)' 1 $false 1 0 -seconds 15 -Optional
    Invoke-Trial 'LWin held 2 s' 'hold the LEFT Windows key about 2 seconds, then release' 1 $false 1 0
    Invoke-Trial '10 taps' 'tap the LEFT Windows key 10 times SLOWLY: count one-and-two between taps' 10 $false 10 0 -seconds 90
    Invoke-Trial 'double tap' 'double-tap the LEFT Windows key quickly (Start SHOULD open; the script then closes it)' 2 $true 1 1
    Invoke-Trial 'tap closes open Start' 'double-tap the LEFT Windows key (Start opens), wait a second, tap it once more (Start must close)' 3 $true 1 1 'Did your last tap close Start?' -ExpectStartClosedAtEnd
    Invoke-Trial 'two slow taps' 'tap the LEFT Windows key, wait about 2 seconds, tap it again' 2 $false 2 0
    Invoke-Trial 'Shift held, Win tap' 'hold Shift, tap the LEFT Windows key, release Shift' 1 $false 0 0

    Write-Host "`n[2] Native chords (each must do its normal Windows thing; undo it afterwards)"
    $chords = @(
        @('Win+E', 'press Win+E (Explorer opens; close it)', 'Did Explorer open?'),
        @('Win+D', 'press Win+D, release both keys, press Win+D again (desktop shows, then windows come back)', 'Did the desktop show and come back?'),
        @('Win+R', 'press Win+R (Run opens), then press Esc', 'Did the Run box open?'),
        @('Win+X', 'press Win+X (the power-user menu opens), then press Esc', 'Did the Win+X menu open?'),
        @('Win+1', 'press Win+1 (first taskbar app opens or activates)', 'Did the first taskbar app open or activate?'),
        @('Win+A', 'press Win+A (Quick Settings opens), then press Esc', 'Did Quick Settings open?'),
        @('Win+Shift+S', 'press Win+Shift+S (snipping bar appears), then press Esc', 'Did the snipping bar appear?'),
        @('Win+Ctrl+O', 'press Win+Ctrl+O (On-Screen Keyboard opens), release all keys, press Win+Ctrl+O again to close it', 'Did the On-Screen Keyboard open?'),
        @('Win+Tab', 'press Win+Tab (Task View opens), then press Esc', 'Did Task View open?'),
        @('Win+Left', 'click another window, press Win+Left (it snaps left), then come back here', 'Did the window snap left?')
    )
    foreach ($chord in $chords) {
        $releases = if ($chord[0] -in @('Win+D', 'Win+Ctrl+O')) { 2 } else { 1 }
        Invoke-Trial $chord[0] $chord[1] $releases $false 0 0 $chord[2] -seconds 90
    }

    if (-not $SkipLock) {
        Write-Host "`n[3] Win+L lock/unlock"
        Invoke-Trial 'Win+L' 'press Win+L, sign back in, come back here' 0 $false 0 0 -ExpectReset
        Invoke-Trial 'tap after unlock' 'tap the LEFT Windows key once' 1 $false 1 0
        Invoke-Trial 'chord after unlock' 'press Win+E (Explorer opens; close it)' 1 $false 0 0 'Did Explorer open?'
        if (-not $SkipUac) {
            Invoke-Trial 'UAC prompt' 'press Win+R, type cmd, press Ctrl+Shift+Enter, click No on the UAC prompt, come back here' 0 $false 0 0 -ExpectReset
            Invoke-Trial 'tap after UAC' 'tap the LEFT Windows key once' 1 $false 1 0
        }
        Invoke-Trial 'Ctrl+Alt+Del' 'press Ctrl+Alt+Del, click Cancel, come back here' 0 $false 0 0 -ExpectReset
        Invoke-Trial 'tap after Ctrl+Alt+Del' 'tap the LEFT Windows key once' 1 $false 1 0
        Invoke-Trial 'Alt+Tab after Ctrl+Alt+Del' 'press Alt+Tab, release (WinOrbit switches), then CLICK this window to come back' 0 $false 0 0 'Did WinOrbit''s switcher appear and switch?' -expectCommits 1
        Invoke-Trial 'plain Tab after Ctrl+Alt+Del' 'CLICK this window with the mouse (no Alt+Tab), then press Tab alone twice' 0 $false 0 0 'Did plain Tab do nothing special (no WinOrbit switcher)?' -expectCommits 0
        Invoke-Trial 'Ctrl+Alt+Del during Alt+Tab' 'hold Alt, tap Tab (switcher shows), KEEP Alt held and press Ctrl+Del, click Cancel, release all keys, CLICK this window' 0 $false 0 0 'Was the WinOrbit switcher gone when you came back?' -ExpectReset -expectCommits 0 -ExpectOverlayHidden
        Invoke-Trial 'Alt+Tab after the interrupted switch' 'press Alt+Tab, release (WinOrbit switches), then CLICK this window to come back' 0 $false 0 0 'Did WinOrbit''s switcher appear and switch normally?' -expectCommits 1
    }

    Write-Host "`n[4] Elevated foreground"
    Write-Host "  Windows sends a non-elevated WinOrbit no keys while an elevated window (Task Manager) is focused."
    [void](Read-Host "  >>> First open Task Manager (Ctrl+Shift+Esc), click back into this window, then press Enter")
    Invoke-Trial 'tap in Task Manager' 'click Task Manager, tap the LEFT Windows key ONCE (Start opens natively), then CLICK this window (Start closes)' 0 $true 0 0 -NoMeasure
    Invoke-Trial 'tap back here' 'tap the LEFT Windows key once' 1 $false 1 0

    Write-Host "`n[5] Exit and relaunch"
    [V]::Stop(); $observerStarted = $false
    $stop = Stop-App $app; $app = $null
    Check 'exit' 'exited in time' $true $stop.Exited
    Check 'exit' 'exit code' 0 $stop.Code
    $shutdown = Read-LogSince $script:logPath 0
    if ($shutdown -match 'keyboard hook longest callback (\d+) us') {
        $all = [regex]::Matches($shutdown, 'keyboard hook longest callback (\d+) us')
        Write-Host ("    longest hook callback this run: {0} us" -f $all[$all.Count - 1].Groups[1].Value)
    } else {
        $failures.Add('exit: no hook callback duration logged')
    }
    [V]::Start(); $observerStarted = $true
    Invoke-Trial 'WinOrbit exited' 'tap the LEFT Windows key once (WinOrbit is not running: Start opens)' 1 $true 0 0
    $app = Start-App $Exe
    Invoke-Trial 'after relaunch' 'tap the LEFT Windows key once' 1 $false 1 0
    $stop = Stop-App $app; $app = $null
    Check 'second exit' 'exit code' 0 $stop.Code

    Write-Host "`n[6] Disabled ([overview] enable = no)"
    $disabledDir = Join-Path (Split-Path $Exe) 's06d-disabled'
    New-Item -ItemType Directory -Force -Path $disabledDir | Out-Null
    Copy-Item $Exe (Join-Path $disabledDir 'winorbit.exe') -Force
    $section = ''
    $lines = foreach ($line in Get-Content $ini) {
        if ($line -match '^\s*\[(.+)\]\s*$') { $section = $Matches[1] }
        if ($section -eq 'overview' -and $line -match '^\s*enable\s*=') { 'enable = no' } else { $line }
    }
    $lines | Set-Content -Path (Join-Path $disabledDir 'winorbit.ini') -Encoding ASCII
    $disabledExe = Join-Path $disabledDir 'winorbit.exe'
    $disabledLog = Get-LogPath $disabledExe
    $offset = Get-LogLength $disabledLog
    $app = Start-App $disabledExe
    Check 'disabled startup' 'listener leaves Win native' $true ((Read-LogSince $disabledLog $offset) -match 'bare Win tap native')
    Invoke-Trial 'disabled tap' 'tap the LEFT Windows key once (Start opens natively)' 1 $true 0 0 -logPath $disabledLog
    Invoke-Trial 'disabled Win+E' 'press Win+E (Explorer opens; close it)' 1 $false 0 0 'Did Explorer open?' -logPath $disabledLog
    $stop = Stop-App $app; $app = $null
    Check 'disabled exit' 'exit code' 0 $stop.Code
} catch {
    $failures.Add("aborted: $_")
} finally {
    if ($observerStarted) { [V]::Stop() }
    if ($app) { try { [void](Stop-App $app) } catch { } }
    if ($helperLoaded) {
        try {
            Start-Sleep -Milliseconds 200
            $stuck = @()
            foreach ($key in @(@('LWin', 0x5B), @('RWin', 0x5C), @('Shift', 0x10), @('Ctrl', 0x11), @('Alt', 0x12), @('vkE8', 0xE8))) {
                if ([V]::IsDown($key[1])) { $stuck += $key[0] }
            }
            Write-Host ("`n[7] keys left down: {0}" -f $(if ($stuck.Count) { $stuck -join ', ' } else { 'none' }))
            foreach ($name in $stuck) { $failures.Add("$name left down") }
            foreach ($key in @(@(0x5B, 0x5B, $true), @(0x5C, 0x5C, $true), @(0xA0, 0x2A, $false), @(0xA2, 0x1D, $false), @(0xA4, 0x38, $false), @(0xE8, 0, $false))) {
                if ([V]::IsDown($key[0])) { [V]::Vk([byte]$key[0], [byte]$key[1], $true, $key[2]) }
            }
        } catch {
            $failures.Add("cleanup failed: $_")
        }
    }
    if ($script:logPath -and (Test-Path $script:logPath)) {
        $problems = (Read-LogSince $script:logPath 0) -split "`n" | Where-Object { $_ -match 'ERROR|WARN' -and $_ -match 'keyboard|Start|overview|mask|session' }
        if ($problems) {
            Write-Host "`nWinOrbit log warnings/errors (whole log file):"
            $problems | ForEach-Object { Write-Host "  $_" }
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
    if ($transcriptStarted) { try { Stop-Transcript | Out-Null } catch { } }
}
exit $exitCode
