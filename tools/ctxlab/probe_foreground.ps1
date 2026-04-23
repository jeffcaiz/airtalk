#Requires -Version 5.1
# Probe what metadata we can extract about the foreground window on Windows.
# Goal: build intuition about the "cheap Win32 layer" vs "descendants / UIA layer"
# for future context-id work in airtalk.
#
# Usage:
#   # Live mode: polls every ~1.5s, only reprints when foreground HWND changes.
#   #   Alt+Tab between apps to watch what the fields look like.
#   powershell -ExecutionPolicy Bypass -File .\tools\ctxlab\probe_foreground.ps1
#
#   # Single snapshot after a 3s countdown (no Ctrl+C gymnastics):
#   powershell -ExecutionPolicy Bypass -File .\tools\ctxlab\probe_foreground.ps1 -Once
#
#   # Snapshot immediately, no countdown (useful from scripts/agents):
#   powershell -ExecutionPolicy Bypass -File .\tools\ctxlab\probe_foreground.ps1 -Once -DelaySeconds 0

param(
    [int]$DelaySeconds = 3,
    [switch]$Once,
    [int]$IntervalMs = 1500
)

$ErrorActionPreference = 'Stop'

if (-not ([System.Management.Automation.PSTypeName]'Ctxlab.Win32').Type) {
    Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.Runtime.InteropServices;

namespace Ctxlab {
    public static class Win32 {
        [DllImport("user32.dll")]
        public static extern IntPtr GetForegroundWindow();

        [DllImport("user32.dll", CharSet=CharSet.Unicode)]
        public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);

        [DllImport("user32.dll", CharSet=CharSet.Unicode)]
        public static extern int GetClassName(IntPtr hWnd, StringBuilder lpClassName, int nMaxCount);

        [DllImport("user32.dll")]
        public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);

        [DllImport("user32.dll")]
        public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);

        [DllImport("user32.dll")]
        public static extern bool GetGUIThreadInfo(uint idThread, ref GUITHREADINFO lpgui);

        [StructLayout(LayoutKind.Sequential)]
        public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }

        [StructLayout(LayoutKind.Sequential)]
        public struct GUITHREADINFO {
            public int cbSize;
            public uint flags;
            public IntPtr hwndActive;
            public IntPtr hwndFocus;
            public IntPtr hwndCapture;
            public IntPtr hwndMenuOwner;
            public IntPtr hwndMoveSize;
            public IntPtr hwndCaret;
            public RECT rcCaret;
        }
    }
}
"@
}

function Get-WindowInfo([IntPtr]$hwnd) {
    $title = New-Object System.Text.StringBuilder 512
    [Ctxlab.Win32]::GetWindowText($hwnd, $title, $title.Capacity) | Out-Null

    $cls = New-Object System.Text.StringBuilder 256
    [Ctxlab.Win32]::GetClassName($hwnd, $cls, $cls.Capacity) | Out-Null

    $winPid = 0
    $tid = [Ctxlab.Win32]::GetWindowThreadProcessId($hwnd, [ref]$winPid)

    $rect = New-Object Ctxlab.Win32+RECT
    [Ctxlab.Win32]::GetWindowRect($hwnd, [ref]$rect) | Out-Null

    [PSCustomObject]@{
        HWnd  = ('0x{0:X}' -f $hwnd.ToInt64())
        Title = $title.ToString()
        Class = $cls.ToString()
        PID   = $winPid
        TID   = $tid
        Rect  = ('{0},{1} {2}x{3}' -f $rect.Left, $rect.Top, ($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top))
    }
}

function Write-Section($name) {
    Write-Host ''
    Write-Host ("=== {0} ===" -f $name) -ForegroundColor Cyan
}

function Show-ProcessTree($rootPid, $allProcs) {
    function Walk($procPid, $depth, $all) {
        $kids = $all | Where-Object { $_.ParentProcessId -eq $procPid }
        foreach ($k in $kids) {
            $indent = '  ' * $depth
            Write-Host ('{0}+ [{1,5}] {2}' -f $indent, $k.ProcessId, $k.Name) -ForegroundColor Green
            if ($k.CommandLine) {
                $cmd = $k.CommandLine
                if ($cmd.Length -gt 200) { $cmd = $cmd.Substring(0, 200) + '...' }
                Write-Host ('{0}      cmd: {1}' -f $indent, $cmd) -ForegroundColor DarkGray
            }
            Walk $k.ProcessId ($depth + 1) $all
        }
    }
    Walk $rootPid 0 $allProcs
    $hasKids = $allProcs | Where-Object { $_.ParentProcessId -eq $rootPid }
    if (-not $hasKids) {
        Write-Host '  (no children — this app hosts its own UI directly)' -ForegroundColor DarkGray
    }
}

function Dump-Foreground {
    $hwnd = [Ctxlab.Win32]::GetForegroundWindow()
    if ($hwnd -eq [IntPtr]::Zero) {
        Write-Host 'No foreground window (is the screen locked?)' -ForegroundColor Red
        return
    }
    $win = Get-WindowInfo $hwnd

    Write-Section 'Layer 1: Foreground Window (Win32)'
    $win | Format-List

    Write-Section 'Layer 1: Window-Owning Process'
    $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$($win.PID)"
    if ($proc) {
        [PSCustomObject]@{
            Name      = $proc.Name
            ExePath   = $proc.ExecutablePath
            ParentPID = $proc.ParentProcessId
            CmdLine   = $proc.CommandLine
        } | Format-List
    } else {
        Write-Host "Process $($win.PID) not found" -ForegroundColor Red
    }

    Write-Section 'Layer 1.5: Focused Control Inside That Window (GetGUIThreadInfo)'
    $gti = New-Object Ctxlab.Win32+GUITHREADINFO
    $gti.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf([type][Ctxlab.Win32+GUITHREADINFO])
    $ok = [Ctxlab.Win32]::GetGUIThreadInfo($win.TID, [ref]$gti)
    if ($ok -and $gti.hwndFocus -ne [IntPtr]::Zero) {
        $focusInfo = Get-WindowInfo $gti.hwndFocus
        Write-Host 'Focused child HWND:'
        $focusInfo | Format-List
        Write-Host ('Caret HWND: 0x{0:X}   Caret rect: {1},{2} {3}x{4}' -f `
            $gti.hwndCaret.ToInt64(), `
            $gti.rcCaret.Left, $gti.rcCaret.Top, `
            ($gti.rcCaret.Right - $gti.rcCaret.Left), `
            ($gti.rcCaret.Bottom - $gti.rcCaret.Top))
    } else {
        Write-Host "No focused child reported (ok=$ok)." -ForegroundColor DarkYellow
        Write-Host '  Common for Chromium/Electron/UWP — they manage focus inside their own UI tree,' -ForegroundColor DarkGray
        Write-Host '  not through standard Win32 child HWNDs. You need UIA to see further.' -ForegroundColor DarkGray
    }

    Write-Section 'Layer 2: Process Tree Below Window-Owning Process'
    $all = Get-CimInstance Win32_Process
    Show-ProcessTree $win.PID $all
}

if ($Once) {
    for ($i = $DelaySeconds; $i -gt 0; $i--) {
        Write-Host ('Snapshot in {0}s... (focus your target window now)' -f $i) -ForegroundColor Yellow
        Start-Sleep -Seconds 1
    }
    Dump-Foreground
} else {
    Write-Host ''
    Write-Host 'Polling foreground. Alt+Tab between apps to see how fields change.' -ForegroundColor Yellow
    Write-Host 'Only reprints when foreground HWND changes. Ctrl+C to stop.' -ForegroundColor Yellow
    $lastHwnd = [IntPtr]::Zero
    while ($true) {
        $h = [Ctxlab.Win32]::GetForegroundWindow()
        if ($h -ne $lastHwnd) {
            $lastHwnd = $h
            Dump-Foreground
            Write-Host ('-' * 70) -ForegroundColor DarkGray
        }
        Start-Sleep -Milliseconds $IntervalMs
    }
}
