<#
.SYNOPSIS
    Probes how Windows treats a bare Win/Super tap under a low-level keyboard
    hook, to choose WinOrbit's Start-menu suppression strategy (TASK-S06D).

.DESCRIPTION
    Run with WinOrbit EXITED, from a NON-elevated terminal (the elevated-window
    trial needs UIPI in effect). Installs its own WH_KEYBOARD_LL hook and asks
    for physical key presses. For each trial it applies one strategy to a clean
    Win tap (Win down/up with no other key in between), then detects whether
    Start opened (foreground process StartMenuExperienceHost) and asks you
    whether Start flashed.

    Strategies:
      Pass  Native: the hook passes everything.
      A     On the Win key-up: inject vkE8 down/up, then pass the physical up.
            (Works only if input injected inside the callback lands before
            the event in flight.)
      B     On the Win key-up: inject vkE8 down/up plus a replacement Win
            key-up; consume the physical up only if all 3 went in.
      C     On the first Win key-down: inject vkE8 down/up. Pass every Win
            event natively.

    Also records the raw hook event sequence around Win+L lock/unlock and an
    optional UAC prompt, and compares the hook's idea of which keys are down
    with GetAsyncKeyState afterwards (lost key-ups).

    Writes winorbit-super-probe-<time>.log (transcript) and
    winorbit-super-probe-<time>-events.log (every hook event).

    Safety: no strategy ever consumes a Win event unless its replacement went
    in, and on exit the script releases any Win, Shift, Ctrl or vkE8 key that
    Windows still reports down.
#>
param(
    [switch] $SkipUac
)

$ErrorActionPreference = 'Stop'
$stamp = '{0:yyyyMMdd-HHmmss}' -f (Get-Date)
$log = Join-Path (Get-Location) "winorbit-super-probe-$stamp.log"
$eventsLog = Join-Path (Get-Location) "winorbit-super-probe-$stamp-events.log"
$results = New-Object System.Collections.Generic.List[object]
$transcriptStarted = $false
$hookStarted = $false

Add-Type -TypeDefinition @"
using System;
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class Probe {
    delegate IntPtr HookProc(int code, IntPtr wParam, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential)]
    struct KBDLLHOOKSTRUCT { public uint vkCode; public uint scanCode; public uint flags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Explicit)]
    struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
    [StructLayout(LayoutKind.Sequential)]
    struct INPUT { public uint type; public InputUnion u; }
    [StructLayout(LayoutKind.Sequential)]
    struct MSG { public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public int x; public int y; }

    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr SetWindowsHookEx(int id, HookProc proc, IntPtr hMod, uint threadId);
    [DllImport("user32.dll")] static extern bool UnhookWindowsHookEx(IntPtr hook);
    [DllImport("user32.dll")] static extern IntPtr CallNextHookEx(IntPtr hook, int code, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] static extern int GetMessage(out MSG msg, IntPtr hwnd, uint min, uint max);
    [DllImport("user32.dll")] static extern bool PostThreadMessage(uint threadId, uint msg, IntPtr w, IntPtr l);
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("kernel32.dll")] static extern IntPtr GetModuleHandle(string name);
    [DllImport("user32.dll", SetLastError = true)] static extern uint SendInput(uint count, INPUT[] inputs, int size);
    [DllImport("user32.dll")] public static extern short GetAsyncKeyState(int vk);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder name, int max);

    const int WH_KEYBOARD_LL = 13;
    const uint LLKHF_EXTENDED = 0x01, LLKHF_LOWER_IL_INJECTED = 0x02, LLKHF_INJECTED = 0x10, LLKHF_UP = 0x80;
    const uint KEYEVENTF_EXTENDEDKEY = 0x1, KEYEVENTF_KEYUP = 0x2;
    const ushort VK_MASK = 0xE8;
    const uint VK_LWIN = 0x5B, VK_RWIN = 0x5C;
    static readonly IntPtr Marker = new IntPtr(0x50524F42);

    // Static so the delegate can never be garbage-collected while hooked.
    static readonly HookProc Proc = Callback;
    static IntPtr hook = IntPtr.Zero;
    static uint threadId;
    static Thread thread;
    static readonly Stopwatch Clock = Stopwatch.StartNew();

    public static readonly ConcurrentQueue<string> Events = new ConcurrentQueue<string>();
    // 0 Pass, 1 A, 2 B, 3 C
    public static volatile int Mode;

    // Tap tracking (physical events only).
    static bool lwin, rwin;
    static bool chorded;
    static readonly bool[] down = new bool[256];

    // Outcome of the last completed Win hold.
    public static volatile int Releases;
    public static volatile bool LastClean;
    public static volatile string LastAction = "";

    public static bool[] Down { get { return down; } }
    public static bool WinBelievedDown { get { return lwin || rwin; } }

    static void Note(string s) {
        Events.Enqueue(string.Format("{0,9:F1} ms  {1}", Clock.Elapsed.TotalMilliseconds, s));
    }

    static INPUT Key(ushort vk, ushort scan, uint flags) {
        INPUT input = new INPUT();
        input.type = 1;
        input.u.ki.wVk = vk;
        input.u.ki.wScan = scan;
        input.u.ki.dwFlags = flags;
        input.u.ki.dwExtraInfo = Marker;
        return input;
    }

    static uint Send(INPUT[] inputs) {
        uint sent = SendInput((uint)inputs.Length, inputs, Marshal.SizeOf(typeof(INPUT)));
        int err = sent == inputs.Length ? 0 : Marshal.GetLastWin32Error();
        Note(string.Format("    SendInput {0}/{1} (err {2})", sent, inputs.Length, err));
        return sent;
    }

    public static uint SendMask() {
        return Send(new INPUT[] { Key(VK_MASK, 0, 0), Key(VK_MASK, 0, KEYEVENTF_KEYUP) });
    }

    public static uint SendWinTap() {
        return Send(new INPUT[] { Key((ushort)VK_LWIN, 0x5B, KEYEVENTF_EXTENDEDKEY), Key((ushort)VK_LWIN, 0x5B, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP) });
    }

    public static uint SendCtrlEsc() {
        return Send(new INPUT[] { Key(0x11, 0x1D, 0), Key(0x1B, 0x01, 0), Key(0x1B, 0x01, KEYEVENTF_KEYUP), Key(0x11, 0x1D, KEYEVENTF_KEYUP) });
    }

    public static uint SendEsc() {
        return Send(new INPUT[] { Key(0x1B, 0x01, 0), Key(0x1B, 0x01, KEYEVENTF_KEYUP) });
    }

    public static uint SendKeyUp(ushort vk, ushort scan, bool extended) {
        return Send(new INPUT[] { Key(vk, scan, KEYEVENTF_KEYUP | (extended ? KEYEVENTF_EXTENDEDKEY : 0)) });
    }

    static IntPtr Callback(int code, IntPtr wParam, IntPtr lParam) {
        if (code < 0) return CallNextHookEx(hook, code, wParam, lParam);
        KBDLLHOOKSTRUCT k = (KBDLLHOOKSTRUCT)Marshal.PtrToStructure(lParam, typeof(KBDLLHOOKSTRUCT));
        bool injected = (k.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED)) != 0;
        bool up = (k.flags & LLKHF_UP) != 0;
        Note(string.Format("{0} vk={1:X2} scan={2:X2} flags={3:X2} time={4} extra={5:X}{6} async(LWin)={7}",
            up ? "up  " : "down", k.vkCode, k.scanCode, k.flags, k.time, k.dwExtraInfo.ToInt64(),
            injected ? " INJECTED" : "", (GetAsyncKeyState((int)VK_LWIN) & 0x8000) != 0 ? "down" : "up"));
        if (injected) return CallNextHookEx(hook, code, wParam, lParam);

        bool isWin = k.vkCode == VK_LWIN || k.vkCode == VK_RWIN;
        if (!isWin) {
            down[k.vkCode & 0xFF] = !up;
            if (lwin || rwin) chorded = true;
            return CallNextHookEx(hook, code, wParam, lParam);
        }

        bool wasHeld = lwin || rwin;
        if (!up) {
            if (!wasHeld) {
                // New hold: clean unless another key is already down.
                chorded = false;
                for (int i = 0; i < 256; i++) if (down[i]) { chorded = true; break; }
                if (Mode == 3) { Note("  C: mask on first Win down"); SendMask(); }
            } else if ((k.vkCode == VK_LWIN && !lwin) || (k.vkCode == VK_RWIN && !rwin)) {
                chorded = true; // the other Win side joined
            }
            if (k.vkCode == VK_LWIN) lwin = true; else rwin = true;
            return CallNextHookEx(hook, code, wParam, lParam);
        }

        if (k.vkCode == VK_LWIN) lwin = false; else rwin = false;
        if (!wasHeld || lwin || rwin) return CallNextHookEx(hook, code, wParam, lParam);

        // Last Win side released.
        string action = "pass";
        bool consume = false;
        if (!chorded) {
            if (Mode == 1) {
                Note("  A: mask, then pass the physical up");
                action = "A: mask " + SendMask() + "/2, passed up";
            } else if (Mode == 2) {
                Note("  B: mask + replacement up");
                bool ext = (k.flags & LLKHF_EXTENDED) != 0;
                uint sent = Send(new INPUT[] {
                    Key(VK_MASK, 0, 0), Key(VK_MASK, 0, KEYEVENTF_KEYUP),
                    Key((ushort)k.vkCode, (ushort)k.scanCode, KEYEVENTF_KEYUP | (ext ? KEYEVENTF_EXTENDEDKEY : 0)) });
                consume = sent == 3;
                action = "B: sent " + sent + "/3, " + (consume ? "consumed physical up" : "passed physical up");
            } else if (Mode == 3) {
                action = "C: passed up (mask went in at down)";
            }
        }
        LastClean = !chorded;
        LastAction = action;
        Releases++;
        Note("  release: clean=" + !chorded + " action=" + action);
        return consume ? new IntPtr(1) : CallNextHookEx(hook, code, wParam, lParam);
    }

    public static void Start() {
        ManualResetEvent ready = new ManualResetEvent(false);
        string error = null;
        thread = new Thread(() => {
            hook = SetWindowsHookEx(WH_KEYBOARD_LL, Proc, GetModuleHandle(null), 0);
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

    public static void ResetTracking() {
        lwin = false; rwin = false; chorded = false;
        for (int i = 0; i < 256; i++) down[i] = false;
    }

    public static string Foreground() {
        IntPtr hwnd = GetForegroundWindow();
        uint pid;
        GetWindowThreadProcessId(hwnd, out pid);
        StringBuilder cls = new StringBuilder(256);
        GetClassName(hwnd, cls, 256);
        string name = "?";
        try { name = Process.GetProcessById((int)pid).ProcessName; } catch (Exception) { }
        return name + " [" + cls + "]";
    }
}
"@

function Test-StartOpen([int] $ms = 1500) {
    $deadline = (Get-Date).AddMilliseconds($ms)
    while ((Get-Date) -lt $deadline) {
        $fg = [Probe]::Foreground()
        if ($fg -match '^(StartMenuExperienceHost|SearchHost|ShellExperienceHost) ') { return $fg }
        Start-Sleep -Milliseconds 20
    }
    return $null
}

function Close-Start {
    [void][Probe]::SendEsc()
    Start-Sleep -Milliseconds 400
}

function Read-YesNo([string] $question) {
    while ($true) {
        $answer = (Read-Host "  $question (y/n)").Trim().ToLower()
        if ($answer -in @('y', 'n')) { return $answer -eq 'y' }
    }
}

# Waits for the next completed Win hold, then reports what happened.
function Invoke-Trial([string] $name, [int] $mode, [string] $instruction, [int] $seconds = 30) {
    [Probe]::Mode = $mode
    [Probe]::ResetTracking()
    $before = [Probe]::Releases
    Write-Host ""
    Write-Host "  [$name]" -ForegroundColor Cyan
    Write-Host "  >>> NOW: $instruction" -ForegroundColor Yellow
    $deadline = (Get-Date).AddSeconds($seconds)
    while ([Probe]::Releases -eq $before -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 10 }
    if ([Probe]::Releases -eq $before) {
        Write-Host "  (skipped: no Win release seen)"
        $results.Add([pscustomobject]@{ Trial = $name; Clean = ''; Action = 'skipped'; Detected = ''; Seen = '' })
        return
    }
    $clean = [Probe]::LastClean
    $action = [Probe]::LastAction
    $detected = Test-StartOpen
    if ($detected) { Close-Start }
    Start-Sleep -Milliseconds 300
    $seen = Read-YesNo "Did the Start menu open or flash, even briefly?"
    if ($detected -and -not $seen) { Write-Host "  note: Start was detected in the foreground ($detected)" }
    Write-Host ("  clean={0} action={1} start-detected={2} you-saw-start={3}" -f $clean, $action, [bool]$detected, $seen)
    $results.Add([pscustomobject]@{ Trial = $name; Clean = $clean; Action = $action; Detected = [bool]$detected; Seen = $seen })
}

function Show-StaleKeys([string] $label) {
    $believed = @()
    $down = [Probe]::Down
    for ($vk = 1; $vk -lt 256; $vk++) { if ($down[$vk]) { $believed += ('0x{0:X2}' -f $vk) } }
    $async = @()
    foreach ($vk in @(0x5B, 0x5C, 0x10, 0x11, 0x12, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x0D, 0x4C, 0x52)) {
        if (([Probe]::GetAsyncKeyState($vk) -band 0x8000) -ne 0) { $async += ('0x{0:X2}' -f $vk) }
    }
    $line = "  {0}: hook believes Win down={1}; hook believes down: {2}; GetAsyncKeyState down: {3}" -f $label,
        [Probe]::WinBelievedDown, ($(if ($believed) { $believed -join ' ' } else { 'none' })),
        ($(if ($async) { $async -join ' ' } else { 'none' }))
    Write-Host $line
    $results.Add([pscustomobject]@{ Trial = $label; Clean = ''; Action = $line.Trim(); Detected = ''; Seen = '' })
}

function Release-Stuck {
    foreach ($key in @(@(0x5B, 0x5B, $true), @(0x5C, 0x5C, $true), @(0xA0, 0x2A, $false), @(0xA1, 0x36, $false), @(0xA2, 0x1D, $false), @(0xE8, 0, $false))) {
        if (([Probe]::GetAsyncKeyState($key[0]) -band 0x8000) -ne 0) {
            Write-Host ("  releasing stuck key 0x{0:X2}" -f $key[0])
            [void][Probe]::SendKeyUp([uint16]$key[0], [uint16]$key[1], $key[2])
        }
    }
}

try {
    Start-Transcript -Path $log | Out-Null
    $transcriptStarted = $true
    Write-Host "Log: $log"
    Write-Host "Events: $eventsLog"
    Write-Host ("Windows: {0}" -f (Get-CimInstance Win32_OperatingSystem | ForEach-Object { "$($_.Caption) $($_.Version)" }))
    $elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Write-Host "Probe elevated: $elevated"
    if (Get-Process -Name winorbit -ErrorAction SilentlyContinue) { throw "WinOrbit is running; exit it first (tray > Exit)." }
    if ($elevated) {
        Write-Host "WARNING: this terminal is elevated, so the elevated-window trial is skipped. Run from a normal terminal for full results." -ForegroundColor Red
    }

    [Probe]::Start()
    $hookStarted = $true
    Write-Host "`nProbe hook installed. Keep this window focused unless a step says otherwise."
    Write-Host "When Start opens, the script closes it with Esc. Answer y/n by typing here."

    Write-Host "`n[1] Native behaviour (hook passes everything)"
    Invoke-Trial "native: LWin tap" 0 "tap the LEFT Windows key once"
    Invoke-Trial "native: LWin held 2 s" 0 "hold the LEFT Windows key about 2 seconds, then release"
    Invoke-Trial "native: Shift held, Win tap" 0 "hold Shift, tap the Windows key, then release Shift"
    Invoke-Trial "native: RWin tap" 0 "tap the RIGHT Windows key if your keyboard has one (otherwise wait 15 s to skip)" 15
    Invoke-Trial "native: LWin+RWin overlap" 0 "press LEFT Win, press RIGHT Win, release both (wait 15 s to skip)" 15

    Write-Host "`n[2] Suppression strategies on a clean tap"
    Invoke-Trial "A: mask on up, pass up" 1 "tap the LEFT Windows key once"
    Invoke-Trial "B: mask + replacement up" 2 "tap the LEFT Windows key once"
    Invoke-Trial "C: mask on down" 3 "tap the LEFT Windows key once"
    Invoke-Trial "C: mask on down, Win+E" 3 "press Win+E (Explorer should open; close it afterwards)"
    Invoke-Trial "C: mask on down, Win held 2 s" 3 "hold the LEFT Windows key about 2 seconds, then release"
    Show-StaleKeys "after strategies"

    Write-Host "`n[3] Replay: does injected input open Start?"
    [Probe]::Mode = 0
    foreach ($replay in @('Win tap', 'Ctrl+Esc')) {
        Write-Host "  injecting $replay ..."
        if ($replay -eq 'Win tap') { [void][Probe]::SendWinTap() } else { [void][Probe]::SendCtrlEsc() }
        $detected = Test-StartOpen
        if ($detected) { Close-Start }
        Write-Host ("  injected {0}: start-detected={1}" -f $replay, [bool]$detected)
        $results.Add([pscustomobject]@{ Trial = "replay: injected $replay"; Clean = ''; Action = 'injected'; Detected = [bool]$detected; Seen = '' })
        Start-Sleep -Milliseconds 500
    }

    if (-not $elevated) {
        Write-Host "`n[4] Elevated foreground window (UIPI)"
        Write-Host "  Open Task Manager (Ctrl+Shift+Esc), which runs elevated for admin users. Click into it so it is focused."
        Write-Host "  The Start check reads the foreground, so answer the y/n question after switching back here."
        Invoke-Trial "A with elevated foreground" 1 "with Task Manager focused, tap the LEFT Windows key once" 60
        Invoke-Trial "B with elevated foreground" 2 "click Task Manager again, then tap the LEFT Windows key once" 60
        Invoke-Trial "C with elevated foreground" 3 "click Task Manager again, then tap the LEFT Windows key once" 60
        Show-StaleKeys "after elevated trials"
    }

    Write-Host "`n[5] Win+L lock/unlock"
    [Probe]::Mode = 0
    [Probe]::ResetTracking()
    Write-Host "  >>> NOW: press Win+L to lock, sign back in, then come back here and press Enter." -ForegroundColor Yellow
    [void](Read-Host "  Press Enter after unlocking")
    Show-StaleKeys "after Win+L"
    Invoke-Trial "native: LWin tap after unlock" 0 "tap the LEFT Windows key once"

    if (-not $SkipUac) {
        Write-Host "`n[6] UAC secure desktop (optional)"
        [Probe]::ResetTracking()
        Write-Host "  >>> NOW: press Win+R, type cmd, press Ctrl+Shift+Enter. When the UAC prompt appears, click No." -ForegroundColor Yellow
        Write-Host "      Then come back here and press Enter (or just press Enter to skip)."
        [void](Read-Host "  Press Enter when done")
        Show-StaleKeys "after UAC"
        Invoke-Trial "native: LWin tap after UAC" 0 "tap the LEFT Windows key once"
    }
} catch {
    Write-Host "ABORTED: $_" -ForegroundColor Red
} finally {
    if ($hookStarted) { [Probe]::Stop() }
    try { Release-Stuck } catch { Write-Host "releasing stuck keys failed: $_" -ForegroundColor Red }
    $lines = @()
    $line = $null
    if ($hookStarted) { while ([Probe]::Events.TryDequeue([ref]$line)) { $lines += $line } }
    $lines | Set-Content -Path $eventsLog -Encoding UTF8
    Write-Host "`nSummary"
    $results | Format-Table -AutoSize -Wrap | Out-String -Width 220 | Write-Host
    Write-Host "Events written: $($lines.Count) -> $eventsLog"
    if ($transcriptStarted) { Stop-Transcript | Out-Null }
}
