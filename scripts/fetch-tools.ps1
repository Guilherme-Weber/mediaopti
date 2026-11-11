# Installs the image optimizers mediaopti uses.
#   - Open-source tools come from Scoop (per user, no admin rights): oxipng, ECT, mozjpeg
#     (jpegtran/cjpeg/djpeg), gifsicle and libjxl (for its ssimulacra2 quality metric).
#     mediaopti finds them on PATH.
#   - cjpegli (jpegli JPEG encoder) comes from the libjxl 0.11.2 release on GitHub, the last
#     one that ships it for Windows.
#   - pingo and beeim (freeware by Cedric Louvrier, css-ig.net) allow private use only and
#     may not be redistributed, so they are downloaded from the author's site, never bundled.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/fetch-tools.ps1                 # into bin\ (development)
#   powershell -ExecutionPolicy Bypass -File scripts/fetch-tools.ps1 -Dest <folder>  # e.g. the "tools" folder next to mediaopti.exe
#   add -NoPingo to skip pingo and beeim
param(
    [string]$Dest = (Join-Path (Split-Path $PSScriptRoot -Parent) 'bin'),
    [switch]$NoPingo
)
$ErrorActionPreference = 'Stop'

if (-not (Get-Command scoop -ErrorAction SilentlyContinue)) {
    Write-Host "Scoop not found. Install it from https://scoop.sh, then run this script again." -ForegroundColor Yellow
    exit 1
}
scoop install oxipng ect mozjpeg gifsicle libjxl

New-Item -ItemType Directory -Force $Dest | Out-Null
function Get-FromZip([string]$Url, [string[]]$Names) {
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("mediaopti-tools-" + [guid]::NewGuid())
    New-Item -ItemType Directory $tmp | Out-Null
    try {
        Invoke-WebRequest $Url -OutFile "$tmp\a.zip"
        Expand-Archive "$tmp\a.zip" -DestinationPath "$tmp\x"
        foreach ($n in $Names) {
            $f = Get-ChildItem "$tmp\x" -Recurse -Filter $n | Select-Object -First 1
            if (-not $f) { throw "$n not found in $Url" }
            Copy-Item $f.FullName $Dest -Force
        }
    }
    finally { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }
}

Write-Host "Downloading cjpegli (libjxl 0.11.2, GitHub) ..."
Get-FromZip 'https://github.com/libjxl/libjxl/releases/download/v0.11.2/jxl-x64-windows-static.zip' @('cjpegli.exe')

if (-not $NoPingo) {
    foreach ($name in 'pingo', 'beeim') {
        Write-Host "Downloading $name (css-ig.net, private use only) ..."
        Get-FromZip "https://css-ig.net/bin/$name.zip" @("$name.exe")
    }
}

Write-Host "Image tools installed. mediaopti lists what it found when a batch with images starts."
