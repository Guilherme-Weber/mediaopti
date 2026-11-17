# Builds the release app and packages it as a portable zip with ffmpeg and the redistributable
# tools (each in tools\<name>\ with its DLLs and licence):
#   dist/mediaopti-<version>-win64.zip
#
# The tools are taken from Scoop (run scripts/fetch-tools.ps1 first) and bin\ (cjpegli,
# OptiVorbis). pingo and beeim may not be redistributed and are never packaged: on another PC,
# run fetch-tools.ps1 -NoScoop -Dest <folder>\tools to download them.
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/package.ps1 [-NoTools]

param([switch]$NoTools)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$ffmpeg = Join-Path $root 'bin\ffmpeg.exe'
if (-not (Test-Path $ffmpeg)) {
    & (Join-Path $PSScriptRoot 'fetch-ffmpeg.ps1')
}

# Paths of this machine (the user folder, holding Cargo's sources of the dependencies, and
# this checkout) are written into the executable for panic messages: replaced by neutral
# ones. A separate target folder keeps the everyday build's cache.
$profileDir = [Environment]::GetFolderPath('UserProfile')
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $profileDir '.cargo' }
$savedFlags, $savedTarget = $env:RUSTFLAGS, $env:CARGO_TARGET_DIR
$env:RUSTFLAGS = "--remap-path-prefix=$profileDir=~ --remap-path-prefix=$cargoHome=cargo --remap-path-prefix=$root=mediaopti"
$env:CARGO_TARGET_DIR = Join-Path $root 'target\package'
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
} finally {
    $env:RUSTFLAGS, $env:CARGO_TARGET_DIR = $savedFlags, $savedTarget
}
$exePath = Join-Path $root 'target\package\release\mediaopti.exe'
$exeText = [System.Text.Encoding]::Latin1.GetString([System.IO.File]::ReadAllBytes($exePath))
foreach ($private in @($profileDir, $root)) {
    $found = [regex]::Matches($exeText, [regex]::Escape($private), 'IgnoreCase').Count
    if ($found -gt 0) { throw "the executable still names $private ($found times)" }
}

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$name = "mediaopti-$version-win64"
$stage = Join-Path $root "dist\$name"
$zip = Join-Path $root "dist\$name.zip"

Remove-Item $stage, $zip -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exePath $stage
Copy-Item $ffmpeg $stage
Copy-Item (Join-Path $root 'bin\ffmpeg-LICENSE.txt') $stage -ErrorAction SilentlyContinue
Copy-Item (Join-Path $root 'LICENSE') $stage

if (-not $NoTools) {
    $scoop = Join-Path $env:USERPROFILE 'scoop\apps'
    $bin = Join-Path $root 'bin'
    # cjpegli's version as scripts/build-jpegli.ps1 recorded it; without a build it is libjxl
    # 0.11.2's (fetch-tools.ps1).
    $jpegliVersion = 'libjxl 0.11.2'
    $binVersions = Join-Path $bin 'versions.json'
    if (Test-Path $binVersions) {
        $recorded = (Get-Content $binVersions -Raw | ConvertFrom-Json).cjpegli
        if ($recorded) { $jpegliVersion = $recorded }
    }
    # folder, licence, source, then (source file, packaged name) pairs; paths relative to the
    # Scoop app's "current" folder, or "bin:" for the project's bin folder.
    $tools = @(
        @{ dir = 'oxipng'; app = 'oxipng'; licence = 'MIT'; src = 'https://github.com/shssoichiro/oxipng'; files = @('oxipng.exe', 'LICENSE.txt') },
        @{ dir = 'ect'; app = 'ect'; licence = 'Apache-2.0'; src = 'https://github.com/fhanau/Efficient-Compression-Tool'; files = @('ect.exe') },
        @{ dir = 'mozjpeg'; app = 'mozjpeg'; licence = 'IJG / BSD-3-Clause / zlib'; src = 'https://github.com/mozilla/mozjpeg'
           files = @('cjpeg-static.exe=cjpeg.exe', 'djpeg-static.exe=djpeg.exe', 'jpegtran-static.exe=jpegtran.exe') },
        @{ dir = 'gifsicle'; app = 'gifsicle'; licence = 'GPL-2.0'; src = 'https://github.com/kohler/gifsicle'; files = @('gifsicle.exe') },
        @{ dir = 'libjxl'; app = 'libjxl'; licence = 'BSD-3-Clause'; src = 'https://github.com/libjxl/libjxl'
           files = @('bin\ssimulacra2.exe=ssimulacra2.exe', 'licenses\LICENSE.libjxl=LICENSE.txt') },
        # Built from google/jpegli by scripts/build-jpegli.ps1 (version in bin\versions.json).
        @{ dir = 'jpegli'; app = $null; version = $jpegliVersion; licence = 'BSD-3-Clause'; src = 'https://github.com/google/jpegli'
           files = @('bin:cjpegli.exe=cjpegli.exe', 'bin:jpegli-LICENSE.txt=LICENSE.txt') },
        @{ dir = 'flac'; app = 'flac'; licence = 'GPL-2.0 (flac tool), BSD-3-Clause (libFLAC)'; src = 'https://github.com/xiph/flac'; files = @('flac.exe', 'libFLAC.dll') },
        @{ dir = '7zip'; app = '7zip'; licence = 'LGPL-2.1 with the unRAR restriction'; src = 'https://www.7-zip.org'; files = @('7z.exe', '7z.dll', 'License.txt') },
        @{ dir = 'qpdf'; app = 'qpdf'; licence = 'Apache-2.0'; src = 'https://github.com/qpdf/qpdf'; files = @('bin\*.dll', 'bin\qpdf.exe=qpdf.exe') },
        @{ dir = 'mupdf'; app = 'mupdf'; licence = 'AGPL-3.0'; src = 'https://mupdf.com'; files = @('mutool.exe', 'COPYING.txt') },
        @{ dir = 'optivorbis'; app = $null; version = '0.3.2'; licence = 'AGPL-3.0'; src = 'https://github.com/OptiVorbis/OptiVorbis'; files = @('bin:optivorbis.exe=optivorbis.exe') }
    )
    $notice = @('Third-party tools in tools\ (unmodified binaries; source code at the addresses below).',
                'pingo and beeim (css-ig.net) are not included: they may not be redistributed.', '')
    # Each packaged program's version, for the About page (some print none themselves).
    $versions = [ordered]@{}
    foreach ($t in $tools) {
        $to = Join-Path $stage "tools\$($t.dir)"
        New-Item -ItemType Directory -Force $to | Out-Null
        $tv = "$($t.version)"
        if ($t.app) {
            $appDir = Join-Path $scoop "$($t.app)\current"
            if (-not (Test-Path $appDir)) { throw "$($t.app) is not installed (run scripts/fetch-tools.ps1)" }
            $manifest = Join-Path $appDir 'scoop-manifest.json'
            if (Test-Path $manifest) { $tv = (Get-Content $manifest -Raw | ConvertFrom-Json).version }
        }
        foreach ($f in $t.files) {
            $from, $as = $f -split '=', 2
            $base = if ($from.StartsWith('bin:')) { $from = $from.Substring(4); $bin } else { $appDir }
            $items = Get-ChildItem (Join-Path $base $from) -ErrorAction SilentlyContinue
            if (-not $items) { throw "missing $from for $($t.dir)" }
            foreach ($i in $items) {
                $target = if ($as) { Join-Path $to $as } else { Join-Path $to $i.Name }
                Copy-Item $i.FullName $target
                if ($target.EndsWith('.exe')) {
                    $exe = [IO.Path]::GetFileNameWithoutExtension($target)
                    $versions[$exe] = $tv
                }
            }
        }
        $notice += "$($t.dir) $tv - $($t.licence) - $($t.src)"
    }
    $notice += 'FFmpeg (ffmpeg.exe) - see ffmpeg-LICENSE.txt - https://ffmpeg.org'
    $notice | Set-Content (Join-Path $stage 'THIRD-PARTY.txt') -Encoding utf8
    # Without a byte-order mark (Windows PowerShell's utf8 adds one).
    [IO.File]::WriteAllText((Join-Path $stage 'tools\versions.json'), ($versions | ConvertTo-Json), (New-Object Text.UTF8Encoding $false))
}

Compress-Archive -Path "$stage\*" -DestinationPath $zip
Remove-Item $stage -Recurse -Force
$sizeMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "Created $zip ($sizeMb MB)"
