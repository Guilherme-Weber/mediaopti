# Installs the external tools mediaopti uses for images, audio and documents.
#   - Open-source tools come from Scoop (per user, no admin rights): oxipng, ECT, mozjpeg
#     (jpegtran/cjpeg/djpeg), gifsicle, libjxl (its ssimulacra2 quality metric), flac,
#     7-Zip (CBR), qpdf and MuPDF (mutool, PDF page rendering). mediaopti finds them on PATH.
#   - cjpegli (jpegli JPEG encoder) comes from the libjxl 0.11.2 release on GitHub, the last
#     one that ships it for Windows; OptiVorbis (Ogg Vorbis) from its 0.3.2 release.
#   - pingo and beeim (freeware by Cedric Louvrier, css-ig.net) allow private use only and
#     may not be redistributed, so they are downloaded from the author's site, never bundled.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/fetch-tools.ps1                 # into bin\ (development)
#   powershell -ExecutionPolicy Bypass -File scripts/fetch-tools.ps1 -Dest <folder>  # e.g. the "tools" folder next to mediaopti.exe
#   add -NoPingo to skip pingo and beeim, -NoScoop to only download (tools already installed)
param(
    [string]$Dest = (Join-Path (Split-Path $PSScriptRoot -Parent) 'bin'),
    [switch]$NoPingo,
    [switch]$NoScoop
)
$ErrorActionPreference = 'Stop'

if (-not $NoScoop) {
    if (-not (Get-Command scoop -ErrorAction SilentlyContinue)) {
        Write-Host "Scoop not found. Install it from https://scoop.sh, then run this script again." -ForegroundColor Yellow
        exit 1
    }
    # mupdf is in the "extras" bucket.
    if (-not ((scoop bucket list) | Where-Object { $_.Name -eq 'extras' })) { scoop bucket add extras }
    scoop install oxipng ect mozjpeg gifsicle libjxl flac 7zip qpdf mupdf
}

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

# cjpegli: the release uses one built from google/jpegli (scripts/build-jpegli.ps1; jpegli
# publishes no releases). Without a build, libjxl 0.11.2's (the last libjxl release with it,
# same results) is downloaded; an existing one is kept.
if (-not (Test-Path (Join-Path $Dest 'cjpegli.exe'))) {
    Write-Host "Downloading cjpegli (libjxl 0.11.2, GitHub) ..."
    Get-FromZip 'https://github.com/libjxl/libjxl/releases/download/v0.11.2/jxl-x64-windows-static.zip' @('cjpegli.exe')
}

Write-Host "Downloading OptiVorbis 0.3.2 (GitHub) ..."
Get-FromZip 'https://github.com/OptiVorbis/OptiVorbis/releases/download/v0.3.2/OptiVorbis.CLI.x86_64-pc-windows-gnu.zip' @('optivorbis.exe')

if (-not $NoPingo) {
    foreach ($name in 'pingo', 'beeim') {
        Write-Host "Downloading $name (css-ig.net, private use only) ..."
        Get-FromZip "https://css-ig.net/bin/$name.zip" @("$name.exe")
    }
}

Write-Host "Tools installed. mediaopti lists what it found when a batch with images starts."
