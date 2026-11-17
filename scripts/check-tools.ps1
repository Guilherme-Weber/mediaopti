# Lists the tools that have a newer version than the one the next release would package.
# Run before scripts/package.ps1; after updating a tool, run the tests and a real-file check
# (CLAUDE.md) before releasing, because the app's settings were measured with these versions.
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/check-tools.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$outdated = 0

# Scoop: oxipng, ect, mozjpeg, gifsicle, libjxl (ssimulacra2), flac, 7zip, qpdf, mupdf.
$apps = 'oxipng', 'ect', 'mozjpeg', 'gifsicle', 'libjxl', 'flac', '7zip', 'qpdf', 'mupdf'
scoop update *> $null
$status = scoop status 2>$null | Where-Object { $apps -contains $_.Name }
foreach ($s in $status) {
    Write-Host ("Scoop: {0} {1} -> {2} (scoop update {0})" -f $s.Name, $s.'Installed Version', $s.'Latest Version') -ForegroundColor Yellow
    $outdated++
}

function Get-LatestTag([string]$Repo) {
    try { (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'mediaopti-check' }).tag_name.TrimStart('v') }
    catch { $null }
}

# Pinned downloads in fetch-tools.ps1.
$fetch = Get-Content (Join-Path $PSScriptRoot 'fetch-tools.ps1') -Raw
$optivorbis = [regex]::Match($fetch, 'OptiVorbis/releases/download/v([\d.]+)/').Groups[1].Value
$latest = Get-LatestTag 'OptiVorbis/OptiVorbis'
if ($latest -and $latest -ne $optivorbis) {
    Write-Host "OptiVorbis: $optivorbis -> $latest (fetch-tools.ps1; package.ps1 reads it from there)" -ForegroundColor Yellow
    $outdated++
}
# cjpegli is built from google/jpegli (scripts/build-jpegli.ps1): new commits on its main branch?
$versionsFile = Join-Path (Join-Path $root 'bin') 'versions.json'
$built = if (Test-Path $versionsFile) { (Get-Content $versionsFile -Raw | ConvertFrom-Json).cjpegli } else { $null }
try {
    $head = Invoke-RestMethod 'https://api.github.com/repos/google/jpegli/commits/main' -Headers @{ 'User-Agent' = 'mediaopti-check' }
    $sha = $head.sha.Substring(0, 8)
    if (-not $built -or $built -notmatch $sha) {
        Write-Host "jpegli: built $built, main is $sha ($($head.commit.committer.date.ToString().Substring(0, 10))) - check its commits, then scripts/build-jpegli.ps1" -ForegroundColor Yellow
        $outdated++
    }
    $releases = Invoke-RestMethod 'https://api.github.com/repos/google/jpegli/releases' -Headers @{ 'User-Agent' = 'mediaopti-check' }
    if (@($releases).Count -gt 0) { Write-Host "jpegli now publishes releases ($(@($releases)[0].tag_name)): a build could be pinned to one" -ForegroundColor Yellow }
} catch {}

# FFmpeg (gyan.dev essentials build, fetch-ffmpeg.ps1).
$ffmpeg = Join-Path $root 'bin\ffmpeg.exe'
if (Test-Path $ffmpeg) {
    $have = [regex]::Match((& $ffmpeg -version 2>&1 | Select-Object -First 1), 'version ([\d.]+)').Groups[1].Value
    $gyan = "$(curl.exe -s https://www.gyan.dev/ffmpeg/builds/release-version)".Trim()
    if ($gyan -and $gyan -ne $have) {
        Write-Host "FFmpeg: $have -> $gyan (delete bin\ffmpeg.exe and run fetch-ffmpeg.ps1)" -ForegroundColor Yellow
        $outdated++
    }
}

# pingo and beeim are not packaged: the app itself looks for new versions (About > Tools).
if ($outdated -eq 0) { Write-Host 'Every packaged tool is at its latest version.' -ForegroundColor Green }
