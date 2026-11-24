# Finds (and optionally cleans) PNG files bloated by Adobe Photoshop's
# `photoshop:DocumentAncestors` XMP metadata bug (uncompressed `iTXt` chunks
# ranging from hundreds of KB to >10 MB of useless UUIDs; >8 MB also triggers
# `libpng warning: iTXt: out of memory`).
#
# By default this script is READ-ONLY: it only scans and prints a report.
# Pass -Fix to strip ONLY the bloated `DocumentAncestors` `iTXt` chunk byte-for-byte
# (IDAT SHA-256 verified, timestamps and read-only attributes preserved).
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/find-xmp-bloat.ps1 -Path "F:\USER\Pictures"
#   powershell -ExecutionPolicy Bypass -File scripts/find-xmp-bloat.ps1 -Path "F:\USER\Pictures" -MinChunkKB 256 -OutCsv report.csv
#   powershell -ExecutionPolicy Bypass -File scripts/find-xmp-bloat.ps1 -Path "F:\USER\Pictures\SomeFolder" -Fix
#   powershell -ExecutionPolicy Bypass -File scripts/find-xmp-bloat.ps1 -FromCsv report.csv -Fix

param(
    [string]$Path = '',
    [int]$MinChunkKB = 100,
    [string]$OutCsv = '',
    [string]$FromCsv = '',
    [switch]$Fix,
    [switch]$VerboseFiles
)
$ErrorActionPreference = 'Stop'

if (-not $Path -and -not $FromCsv) { throw 'Pass -Path <dir> or -FromCsv <report.csv>' }
$minBytes = [int64]$MinChunkKB * 1024
$hits = New-Object 'Collections.Generic.List[PSObject]'

if ($FromCsv) {
    if (-not (Test-Path -LiteralPath $FromCsv)) { throw "CSV not found: $FromCsv" }
    foreach ($row in (Import-Csv -LiteralPath $FromCsv)) {
        if ($row.Path -and (Test-Path -LiteralPath $row.Path)) { $hits.Add($row) }
    }
    Write-Host "== Loaded $($hits.Count) file(s) from $FromCsv" -ForegroundColor Cyan
} else {
    if (-not (Test-Path -LiteralPath $Path)) { throw "Path not found: $Path" }

    # Fast .NET enumerator + low-I/O chunk-header scanner (skips files smaller than MinChunkKB
    # without opening them, and seeks past IDAT without reading pixel data).
    $rootDir = New-Object IO.DirectoryInfo($Path)
    $stack = New-Object 'Collections.Generic.Stack[IO.DirectoryInfo]'
    $stack.Push($rootDir)

    $scannedPngs = [int64]0
    $openedPngs = [int64]0
    $readErrors = [int64]0
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $sigBuf = New-Object byte[] 8
    $hdrBuf = New-Object byte[] 8

    while ($stack.Count -gt 0) {
        $dir = $stack.Pop()
        try {
            foreach ($sub in $dir.EnumerateDirectories()) {
                # Skip hidden+system folders ($RECYCLE.BIN, System Volume Information)
                $attr = $sub.Attributes
                if ((($attr -band [IO.FileAttributes]::Hidden) -ne 0) -and (($attr -band [IO.FileAttributes]::System) -ne 0)) { continue }
                if (($attr -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
                $stack.Push($sub)
            }
        } catch { continue }

        $enumFiles = $null
        try { $enumFiles = $dir.EnumerateFiles('*.png') } catch { continue }
        foreach ($f in $enumFiles) {
            $scannedPngs++
            if ($f.Length -lt ($minBytes + 64)) { continue }
            $openedPngs++
            $fs = $null
            try {
                $fs = [IO.File]::Open($f.FullName, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
                if ($fs.Read($sigBuf, 0, 8) -ne 8) { continue }
                if ($sigBuf[0] -ne 0x89 -or $sigBuf[1] -ne 0x50 -or $sigBuf[2] -ne 0x4E -or $sigBuf[3] -ne 0x47) { continue }

                $bloatBytes = [int64]0
                $maxChunk = [int64]0
                $chunks = 0
                while ($fs.Read($hdrBuf, 0, 8) -eq 8 -and $chunks -lt 50000) {
                    $chunks++
                    $len = ([uint32]$hdrBuf[0] -shl 24) -bor ([uint32]$hdrBuf[1] -shl 16) -bor ([uint32]$hdrBuf[2] -shl 8) -bor [uint32]$hdrBuf[3]
                    $type = [Text.Encoding]::ASCII.GetString($hdrBuf, 4, 4)
                    $nextPos = $fs.Position + [int64]$len + 4
                    if ($nextPos -gt $fs.Length) { break }

                    if ($type -eq 'iTXt' -and [int64]$len -ge $minBytes) {
                        $chunkData = New-Object byte[] ([int]$len)
                        if ($fs.Read($chunkData, 0, [int]$len) -eq [int]$len) {
                            $txt = [Text.Encoding]::UTF8.GetString($chunkData)
                            if ($txt.StartsWith('XML:com.adobe.xmp') -and $txt.Contains('DocumentAncestors')) {
                                $bloatBytes += ([int64]$len + 12)
                                if ([int64]$len -gt $maxChunk) { $maxChunk = [int64]$len }
                            }
                        }
                        $fs.Position = $nextPos
                    } else {
                        $fs.Position = $nextPos
                    }
                    if ($type -eq 'IEND') { break }
                }

                if ($bloatBytes -gt 0) {
                    $hits.Add([PSCustomObject]@{
                        Path       = $f.FullName
                        Folder     = $f.DirectoryName
                        FileBytes  = $f.Length
                        BloatBytes = $bloatBytes
                        MaxChunk   = $maxChunk
                        Over8MB    = ($maxChunk -ge 8000000)
                    })
                    if ($VerboseFiles) {
                        Write-Host ("  BLOATED ({0:N2} MB / {1:N2} MB): {2}" -f ($bloatBytes / 1MB), ($f.Length / 1MB), $f.FullName)
                    }
                }
            } catch {
                $readErrors++
            } finally {
                if ($fs) { $fs.Dispose() }
            }
        }
    }

    $sw.Stop()
    $totalFileMB = (($hits | Measure-Object -Property FileBytes -Sum).Sum) / 1MB
    $totalBloatMB = (($hits | Measure-Object -Property BloatBytes -Sum).Sum) / 1MB
    $over8 = @($hits | Where-Object { $_.Over8MB }).Count

    Write-Host "== XMP DocumentAncestors scan finished in $([math]::Round($sw.Elapsed.TotalSeconds, 1)) s" -ForegroundColor Cyan
    Write-Host ("  PNGs seen:       {0:N0} (opened >= {1} KB: {2:N0}, read errors: {3:N0})" -f $scannedPngs, $MinChunkKB, $openedPngs, $readErrors)
    Write-Host ("  Bloated PNGs:    {0:N0} (over libpng 8 MB cap: {1:N0})" -f $hits.Count, $over8)
    Write-Host ("  Total size:      {0:N2} MB -> {1:N2} MB (reclaimable: {2:N2} MB)" -f $totalFileMB, ($totalFileMB - $totalBloatMB), $totalBloatMB)

    if ($hits.Count -gt 0) {
        ''
        Write-Host '  Top folders by reclaimable bloat:' -ForegroundColor Cyan
        $hits | Group-Object Folder | ForEach-Object {
            $sum = ($_.Group | Measure-Object -Property BloatBytes -Sum).Sum
            $o8 = @($_.Group | Where-Object { $_.Over8MB }).Count
            [PSCustomObject]@{ Folder = $_.Name; Files = $_.Count; Over8MB = $o8; BloatMB = [math]::Round($sum / 1MB, 2) }
        } | Sort-Object BloatMB -Descending | Select-Object -First 25 | Format-Table -AutoSize
    }

    if ($OutCsv) {
        $hits | Export-Csv -LiteralPath $OutCsv -NoTypeInformation -Encoding UTF8
        Write-Host "  Report saved to: $OutCsv" -ForegroundColor Green
    }
}

if (-not $Fix -or $hits.Count -eq 0) { exit 0 }

Write-Host "== Fixing $($hits.Count) PNG(s) (byte-for-byte chunk copy, IDAT SHA-256 verified)..." -ForegroundColor Yellow
$fixed = 0; $skipped = 0; $savedBytes = [int64]0
$sha = [Security.Cryptography.SHA256]::Create()

foreach ($hit in $hits) {
    $p = $hit.Path
    try {
        $fi = Get-Item -LiteralPath $p
        $origBytes = [IO.File]::ReadAllBytes($p)
        $pos = 8
        $out = New-Object IO.MemoryStream($origBytes.Length)
        $out.Write($origBytes, 0, 8)
        $origIdat = New-Object IO.MemoryStream
        $newIdat = New-Object IO.MemoryStream
        $strippedAny = $false
        $valid = $true
        $origTail = [byte[]]@()
        $newTail = [byte[]]@()

        while ($pos + 12 -le $origBytes.Length) {
            $len = ([uint32]$origBytes[$pos] -shl 24) -bor ([uint32]$origBytes[$pos+1] -shl 16) -bor ([uint32]$origBytes[$pos+2] -shl 8) -bor [uint32]$origBytes[$pos+3]
            $type = [Text.Encoding]::ASCII.GetString($origBytes, $pos + 4, 4)
            $chunkTotal = [int64]12 + [int64]$len
            if ($pos + $chunkTotal -gt $origBytes.Length) { $valid = $false; break }
            $drop = $false
            if ($type -eq 'iTXt' -and [int64]$len -ge $minBytes) {
                $txt = [Text.Encoding]::UTF8.GetString($origBytes, $pos + 8, [int]$len)
                if ($txt.StartsWith('XML:com.adobe.xmp') -and $txt.Contains('DocumentAncestors')) {
                    $drop = $true
                    $strippedAny = $true
                }
            }
            if ($type -eq 'IDAT') {
                $origIdat.Write($origBytes, $pos, [int]$chunkTotal)
                if (-not $drop) { $newIdat.Write($origBytes, $pos, [int]$chunkTotal) }
            }
            if (-not $drop) { $out.Write($origBytes, $pos, [int]$chunkTotal) }
            $pos += [int]$chunkTotal
            if ($type -eq 'IEND') {
                if ($pos -lt $origBytes.Length) {
                    $tailLen = $origBytes.Length - $pos
                    $origTail = New-Object byte[] $tailLen
                    [Array]::Copy($origBytes, $pos, $origTail, 0, $tailLen)
                    $out.Write($origTail, 0, $tailLen)
                    $newTail = $origTail
                }
                break
            }
        }

        if ($valid -and $strippedAny -and $origIdat.Length -gt 0) {
            $h1 = [BitConverter]::ToString($sha.ComputeHash($origIdat.ToArray()))
            $h2 = [BitConverter]::ToString($sha.ComputeHash($newIdat.ToArray()))
            $t1 = [BitConverter]::ToString($sha.ComputeHash($origTail))
            $t2 = [BitConverter]::ToString($sha.ComputeHash($newTail))
            if ($h1 -ne $h2 -or $t1 -ne $t2) {
                Write-Warning "IDAT/tail SHA-256 mismatch, skipped: $p"
                $skipped++
            } else {
                $ct = $fi.CreationTimeUtc; $wt = $fi.LastWriteTimeUtc; $at = $fi.Attributes
                $tmp = $p + '.xmpstrip.tmp'
                try {
                    [IO.File]::WriteAllBytes($tmp, $out.ToArray())
                    $ro = ($at -band [IO.FileAttributes]::ReadOnly) -ne 0
                    if ($ro) { $fi.Attributes = $at -band (-bnot [IO.FileAttributes]::ReadOnly) }
                    [IO.File]::Replace($tmp, $p, [NullString]::Value)
                    $fiNew = Get-Item -LiteralPath $p
                    $fiNew.CreationTimeUtc = $ct
                    $fiNew.LastWriteTimeUtc = $wt
                    if ($ro) { $fiNew.Attributes = $at }
                    $savedBytes += ($origBytes.Length - $out.Length)
                    $fixed++
                } catch {
                    if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
                    Write-Warning "Could not replace (busy/locked?), skipped: $p :: $($_.Exception.Message)"
                    $skipped++
                }
            }
        } else {
            $skipped++
        }
        $out.Dispose(); $origIdat.Dispose(); $newIdat.Dispose()
    } catch {
        Write-Warning "Error reading $p :: $($_.Exception.Message)"
        $skipped++
    }
}
$sha.Dispose()
Write-Host ("== Fixed: {0:N0} | Skipped: {1:N0} | Reclaimed: {2:N2} MB" -f $fixed, $skipped, ($savedBytes / 1MB)) -ForegroundColor Green
