# Screenshots of the app's window without touching the desktop: the window is captured with
# PrintWindow (no focus, no clicks), so it works from a script while the user works.
#
# The app runs with its own data folder (-Data, created fresh: the real history and settings
# in %APPDATA% are never touched; replaced originals go to a backup folder next to the files).
#
#   powershell -ExecutionPolicy Bypass -File scripts/ui-shots.ps1 -Out D:\shots -Shots 2,10 `
#       [-AppArgs '--page=settings'] [-AppArgs '"D:\folder" --start'] [-CloseAt 8] [-Width 1000]
#   ... -AppArgs '"D:\folder" --start' -UntilDone -Every 15     (a whole batch, then closes)
#
# -AppArgs: the app's command line as one string, quoted as the app should see it (a list
# given through `powershell -File` arrives as one literal string and the app took it for a
# folder name).
# -Shots: seconds after the window appears. -Every: a shot every so many seconds. -UntilDone:
# watches the app's log and, when the batch ends ("Done." or "Stopped"), takes a last shot and
# closes the window at once (no waiting on fixed times). -CloseAt: sends WM_CLOSE (what the X
# button sends) at that second; later shots show what the window does meanwhile. -Timeout:
# the most it watches. Title changes are printed as they happen (they show the state:
# "45% · mediaopti", "Stopping… · mediaopti"). The script waits for the app to exit by itself
# (never kills it: an NVENC encoder must not be killed outright). -Size 1180x1500: resizes the
# window first (no focus needed); Windows keeps it within the screen's height. -ScrollDown 30:
# with -UntilDone, a second last shot after that many mouse-wheel notches (sent to the window
# as messages: no focus, no real mouse), showing the bottom of the page (the activity log).
param(
    [string]$Exe = '',
    [string]$AppArgs = '',
    [Parameter(Mandatory = $true)][string]$Out,
    [double[]]$Shots = @(2),
    [double]$CloseAt = -1,
    [double]$Every = 0,
    [switch]$UntilDone,
    [double]$Timeout = 900,
    [string]$Size = '',
    [int]$ScrollDown = 0,
    [int]$Width = 0,
    [string]$Data = ''
)
$ErrorActionPreference = 'Stop'
# (Windows PowerShell 5.1 has no $PSScriptRoot yet in the parameters' defaults.)
if (-not $Exe) { $Exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\debug\mediaopti.exe' }
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class ShotWin {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
  // The process's visible top-level window titled with "mediaopti" (a debug build also owns a
  // console window, which MainWindowHandle may return).
  public static IntPtr AppWindow(uint pid) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint owner; GetWindowThreadProcessId(h, out owner);
      if (owner != pid || !IsWindowVisible(h)) return true;
      var sb = new System.Text.StringBuilder(256); GetWindowText(h, sb, 256);
      if (sb.ToString().Contains("mediaopti")) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

# Real pixels: without this, a scaled display gives the window's size scaled down and the
# capture is cut off.
[void][ShotWin]::SetProcessDPIAware()

function Save-Shot([IntPtr]$hwnd, [string]$path) {
    $r = New-Object ShotWin+RECT
    [void][ShotWin]::GetWindowRect($hwnd, [ref]$r)
    $w, $h = ($r.Right - $r.Left), ($r.Bottom - $r.Top)
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    # 2 = PW_RENDERFULLCONTENT: the window's real content, even when covered.
    [void][ShotWin]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    if ($Width -gt 0 -and $w -gt $Width) {
        $small = New-Object System.Drawing.Bitmap $Width, ([int]($h * $Width / $w))
        $gs = [System.Drawing.Graphics]::FromImage($small)
        $gs.InterpolationMode = 'HighQualityBicubic'
        $gs.DrawImage($bmp, 0, 0, $small.Width, $small.Height)
        $gs.Dispose()
        $bmp.Dispose()
        $bmp = $small
    }
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

function Title([IntPtr]$hwnd) {
    $sb = New-Object System.Text.StringBuilder 256
    [void][ShotWin]::GetWindowText($hwnd, $sb, 256)
    $sb.ToString()
}

New-Item -ItemType Directory -Force $Out | Out-Null
if (-not $Data) { $Data = Join-Path $Out 'appdata' }
New-Item -ItemType Directory -Force (Join-Path $Data 'mediaopti') | Out-Null
$settings = Join-Path $Data 'mediaopti\ui-settings.json'
if (-not (Test-Path $settings)) { '{"originals":2}' | Set-Content $settings }

$saved = $env:APPDATA
$env:APPDATA = $Data
try {
    $p = if ($AppArgs) { Start-Process -FilePath $Exe -ArgumentList $AppArgs -PassThru } else { Start-Process -FilePath $Exe -PassThru }
} finally {
    $env:APPDATA = $saved
}
$t0 = Get-Date
$hwnd = [IntPtr]::Zero
while ($hwnd -eq [IntPtr]::Zero -and ((Get-Date) - $t0).TotalSeconds -lt 30) {
    Start-Sleep -Milliseconds 200
    $hwnd = [ShotWin]::AppWindow([uint32]$p.Id)
}
if ($hwnd -eq [IntPtr]::Zero) { throw 'the window did not appear' }
if ($Size -match '^(\d+)x(\d+)$') {
    # SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE: only the size changes.
    [void][ShotWin]::SetWindowPos($hwnd, [IntPtr]::Zero, 0, 0, [int]$matches[1], [int]$matches[2], 0x0002 -bor 0x0004 -bor 0x0010)
}
$t0 = Get-Date
$log = Join-Path $Data 'mediaopti\logs\mediaopti.log'
$logStart = if (Test-Path $log) { (Get-Item $log).Length } else { 0 }
function Elapsed { ((Get-Date) - $t0).TotalSeconds }
function Shot([string]$label) {
    $file = Join-Path $Out ('shot-{0:000.0}s-{1}.png' -f (Elapsed), $label)
    Save-Shot $hwnd $file
    "{0,6:N1}s shot {1}  [{2}]" -f (Elapsed), (Split-Path $file -Leaf), (Title $hwnd)
}
# The batch's end, from the app's own log (this run's part of it): "Done." or "Stopped".
function BatchEnded {
    if (-not (Test-Path $log)) { return $false }
    $fs = [IO.File]::Open($log, 'Open', 'Read', 'ReadWrite')
    try {
        [void]$fs.Seek([Math]::Min($logStart, $fs.Length), 'Begin')
        $text = (New-Object IO.StreamReader($fs)).ReadToEnd()
    } finally { $fs.Dispose() }
    return $text -match '(?m) (Done\.|Stopped\.|Stopped after)'
}

$pending = [System.Collections.Generic.List[double]]::new([double[]]($Shots | Sort-Object))
$nextEvery = if ($Every -gt 0) { $Every } else { [double]::MaxValue }
$lastKey = $null
$closed = $false
while (-not $p.HasExited -and (Elapsed) -lt $Timeout) {
    $title = Title $hwnd
    # Printed when the state changes, and every 10% of a batch (not each percent).
    $key = if ($title -match '^(\d+)%(.*)$') { '{0}|{1}' -f [Math]::Floor([int]$matches[1] / 10), $matches[2] } else { $title }
    if ($key -ne $lastKey) {
        "{0,6:N1}s title: {1}" -f (Elapsed), $title
        $lastKey = $key
    }
    if (-not $closed -and $CloseAt -ge 0 -and (Elapsed) -ge $CloseAt) {
        [void][ShotWin]::PostMessage($hwnd, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
        $closed = $true
        "{0,6:N1}s close requested" -f (Elapsed)
    }
    while ($pending.Count -and (Elapsed) -ge $pending[0]) {
        Shot ('at{0:0}s' -f $pending[0])
        $pending.RemoveAt(0)
    }
    if ((Elapsed) -ge $nextEvery) {
        Shot 'running'
        $nextEvery += $Every
    }
    if ($UntilDone -and -not $closed -and (BatchEnded)) {
        # The window refreshes a moment after the log line.
        Start-Sleep -Milliseconds 1500
        Shot 'done'
        if ($ScrollDown -gt 0) {
            $r = New-Object ShotWin+RECT
            [void][ShotWin]::GetWindowRect($hwnd, [ref]$r)
            # The pointer over the page's middle (client coordinates), then the wheel turned
            # down (WM_MOUSEWHEEL: -120 a notch, screen coordinates).
            $cx, $cy = [int](($r.Right - $r.Left) * 0.6), [int](($r.Bottom - $r.Top) / 2)
            [void][ShotWin]::PostMessage($hwnd, 0x0200, [IntPtr]::Zero, [IntPtr](($cy -shl 16) -bor $cx))
            $sx, $sy = ($r.Left + $cx), ($r.Top + $cy)
            $wheel = [IntPtr]([int64](-120 -band 0xFFFF) -shl 16)
            for ($i = 0; $i -lt $ScrollDown; $i++) {
                [void][ShotWin]::PostMessage($hwnd, 0x020A, $wheel, [IntPtr](($sy -shl 16) -bor $sx))
                Start-Sleep -Milliseconds 30
            }
            Start-Sleep -Milliseconds 800
            Shot 'done-bottom'
        }
        break
    }
    # Without -UntilDone and -CloseAt, the run ends after the last fixed shot.
    if (-not $UntilDone -and $CloseAt -lt 0 -and -not $pending.Count -and $Every -le 0) { break }
    Start-Sleep -Milliseconds 250
    $p.Refresh()
}
if ((Elapsed) -ge $Timeout) { "{0,6:N1}s timeout reached" -f (Elapsed) }
if (-not $closed -and -not $p.HasExited) {
    [void][ShotWin]::PostMessage($hwnd, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
}
if (-not $p.WaitForExit(120000)) { "the app is still running after 2 minutes (pid $($p.Id)); not killed" }
else { "exited {0:N1}s after start" -f (Elapsed) }
if (Test-Path $log) {
    "--- the app's log, last lines:"
    Get-Content $log -Tail 4
}
