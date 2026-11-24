# Summary of the app's last run from its logs: how it ended, what each file came to, and
# every failure grouped by its cause, so a run's problems are found without reading the log.
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/log-summary.ps1 [-LogDir <dir>] [-Runs 1] [-List 40] [-Failures]
#   -LogDir    the app's log folder (default: %APPDATA%/mediaopti/logs)
#   -Runs      how many of the latest runs to summarise (default 1)
#   -List      at most this many failures listed one by one per run (default 40)
#   -Failures  summarise the durable failures.log (never rotated away) instead of mediaopti*.log
# Causes already understood are in docs/known-failures.md.

param(
    [string]$LogDir = (Join-Path $env:APPDATA 'mediaopti\logs'),
    [int]$Runs = 1,
    [int]$List = 40,
    [switch]$Failures
)
$ErrorActionPreference = 'Stop'

# The cause of a failure line, without the file: grouped by its first words, numbers dropped.
function Get-Cause([string]$Line) {
    $body = $Line -replace '^\S+ \S+ (\[[^\]]+\] )?', ''
    if ($body -match '^Failed on .+?: (.+)$') { $why = $Matches[1] }
    # Formats before 3.2.1:
    elseif ($body -match '^Corrupt/unreadable') { return 'corrupt or unreadable (old message: no reason given)' }
    elseif ($body -match "^The new version of .+? (doesn't match the GIF|lost [^;(]+)") { $why = "the new version $($Matches[1])" }
    elseif ($body -match '^ffmpeg exited with code (-?\d+)') { $why = "ffmpeg exited with code $($Matches[1])" }
    elseif ($body -match '^Could not start ffmpeg') { $why = 'could not start ffmpeg' }
    elseif ($body -match '^Not enough memory') { $why = 'not enough memory' }
    elseif ($body -match '^Could not inspect processed output') { $why = 'could not read the sizes' }
    else { $why = $body }
    $why = $why -replace '\(.*$', '' -replace '\d+(\.\d+)?', 'N' -replace '\s+', ' '
    return $why.Trim().TrimEnd(';', ',', ':')
}

function Test-Failure([string]$Line) {
    $body = $Line -replace '^\S+ \S+ (\[[^\]]+\] )?', ''
    return $body -match "^(Failed on |Corrupt/unreadable|The new version of .+?; kept the original|ffmpeg exited with code|Could not start ffmpeg|Not enough memory|Could not inspect processed output)"
}

if ($Failures) {
    $failPath = Join-Path $LogDir 'failures.log'
    if (-not (Test-Path -LiteralPath $failPath)) { Write-Host "No failures.log in $LogDir"; exit 0 }
    $all = @(Get-Content -LiteralPath $failPath -Encoding UTF8 | Where-Object { $_.Trim() -ne '' })
    Write-Host "== Durable failures.log ($($all.Count) lines)" -ForegroundColor Cyan
    if ($all) {
        ''
        '  Failures by cause:'
        $all | Group-Object { Get-Cause $_ } | Sort-Object Count -Descending | ForEach-Object {
            '    {0,4}  {1}' -f $_.Count, $_.Name
        }
        ''
        "  Latest $List failures:"
        $all | Select-Object -Last $List | ForEach-Object { '    ' + $_ }
    }
    exit 0
}

# Rotated logs: mediaopti.N.log is older than mediaopti.(N-1).log; mediaopti.log is the newest.
$files = Get-ChildItem -LiteralPath $LogDir -Filter 'mediaopti*.log' | Sort-Object {
    if ($_.Name -match '^mediaopti\.(\d+)\.log$') { - [int]$Matches[1] } else { 0 }
}
if (-not $files) { throw "no logs in $LogDir" }
$lines = @(foreach ($f in $files) { Get-Content -LiteralPath $f.FullName -Encoding UTF8 })

# A run starts at its first "Starting ..." line (videos first, then images) after a scan.
$starts = @()
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^\S+ \S+ Starting \d+ ' -and ($i -eq 0 -or $lines[$i - 1] -notmatch '^\S+ \S+ Starting \d+ ')) { $starts += $i }
}
if (-not $starts) { throw 'no run found in the logs' }

$chosen = $starts | Select-Object -Last $Runs
foreach ($s in $chosen) {
    $next = ($starts | Where-Object { $_ -gt $s } | Select-Object -First 1)
    $end = if ($next) { $next - 1 } else { $lines.Count - 1 }
    $run = $lines[$s..$end]
    $first = $run[0].Substring(0, 19)
    $last = $run[-1].Substring(0, [Math]::Min(19, $run[-1].Length))
    $ending = $run | Where-Object { $_ -match '^\S+ \S+ (Stopped\b|Done\.)' } | Select-Object -Last 1

    Write-Host "== Run $first -> $last" -ForegroundColor Cyan
    $run | Where-Object { $_ -match '^\S+ \S+ Starting \d+ ' } | ForEach-Object { '  ' + ($_ -replace '^\S+ \S+ ', '') }
    if ($ending) { '  End: ' + ($ending -replace '^\S+ \S+ ', '') } else { '  End: not in the log (still running, or the app closed without a summary)' }

    $count = { param($re) ($run | Where-Object { $_ -match $re }).Count }
    $failLines = @($run | Where-Object { Test-Failure $_ })
    ''
    '  Saved:              {0}' -f (& $count '^\S+ \S+ \[[^\]]+\] (Saved|Converted) ')
    '  Not smaller (kept): {0}' -f (& $count '^\S+ \S+ \[[^\]]+\] (Video was not smaller|Kept )')
    '  Skipped:            {0}' -f (& $count '^\S+ \S+ \[[^\]]+\] Skipped ')
    '  Failed:             {0}' -f $failLines.Count
    '  Temp not removed:   {0}' -f (& $count 'Could not remove temporary file')

    if ($failLines) {
        ''
        '  Failures by cause:'
        $failLines | Group-Object { Get-Cause $_ } | Sort-Object Count -Descending | ForEach-Object {
            '    {0,4}  {1}' -f $_.Count, $_.Name
        }
        ''
        "  Failures (first $List):"
        $failLines | Select-Object -First $List | ForEach-Object { '    ' + ($_ -replace '^\S+ ', '') }
    }
    ''
}
