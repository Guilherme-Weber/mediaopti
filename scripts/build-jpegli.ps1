# Builds cjpegli from github.com/google/jpegli (it publishes no releases; libjxl stopped
# including cjpegli after 0.11.2) into bin\cjpegli.exe, statically linked (only Windows DLLs).
# Needs CMake, Ninja and MinGW-w64 gcc (e.g. winget install BrechtSanders.WinLibs.POSIX.MSVCRT).
# Checked 2026-11-17 at 031a0077: same sizes and SSIMULACRA2 (+-0.1, compiler rounding) as
# libjxl 0.11.2's cjpegli on 150 encodes, about 10% faster.
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/build-jpegli.ps1 [-Commit <sha>] [-Work <folder>]
# Then run the tests and a real-file check before releasing (CLAUDE.md).

param([string]$Commit = '', [string]$Work = 'D:\mediaopti-build\jpegli')
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

foreach ($t in 'git', 'cmake', 'ninja', 'gcc', 'g++') {
    if (-not (Get-Command $t -ErrorAction SilentlyContinue)) { throw "$t not found on PATH" }
}

$src = Join-Path $Work 'src'
$build = Join-Path $Work 'build'
if (-not (Test-Path $src)) {
    git clone https://github.com/google/jpegli.git $src
    if ($LASTEXITCODE -ne 0) { throw 'git clone failed' }
}
Push-Location $src
try {
    git fetch --quiet origin
    if ($Commit) { git checkout --quiet $Commit } else { git checkout --quiet origin/main }
    if ($LASTEXITCODE -ne 0) { throw 'git checkout failed' }
    git submodule update --init --recursive --quiet
    $sha = (git rev-parse --short=8 HEAD).Trim()
    $date = (git log -1 --format=%cd --date=short).Trim()
} finally { Pop-Location }

Remove-Item $build -Recurse -Force -ErrorAction SilentlyContinue
cmake -G Ninja -S $src -B $build -DCMAKE_BUILD_TYPE=Release -DCMAKE_C_COMPILER=gcc -DCMAKE_CXX_COMPILER=g++ `
    -DBUILD_SHARED_LIBS=OFF -DJPEGLI_STATIC=ON -DBUILD_TESTING=OFF -DJPEGLI_ENABLE_BENCHMARK=OFF `
    -DJPEGLI_ENABLE_JNI=OFF -DJPEGLI_ENABLE_SJPEG=OFF -DJPEGLI_ENABLE_OPENEXR=OFF -DJPEGLI_ENABLE_DOXYGEN=OFF `
    -DJPEGLI_ENABLE_MANPAGES=OFF -DJPEGLI_ENABLE_FUZZERS=OFF -DJPEGLI_BUNDLE_LIBPNG=ON -DJPEGLI_ENABLE_TCMALLOC=OFF
if ($LASTEXITCODE -ne 0) { throw 'cmake configure failed' }
# The bundled libpng's configuration step doesn't get zlib's include folders.
$env:CPATH = "$src\third_party\zlib;$build\third_party\zlib"
cmake --build $build --target cjpegli
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

$exe = Join-Path $build 'tools\cjpegli.exe'
$deps = & objdump -p $exe 2>$null | Select-String 'DLL Name: (.+)' | ForEach-Object { $_.Matches[0].Groups[1].Value.Trim() }
$foreign = $deps | Where-Object { $_ -notmatch '^(KERNEL32|msvcrt|ADVAPI32|USER32|ucrtbase|api-ms-.*)\.dll$' }
if ($foreign) { throw "cjpegli.exe needs DLLs that are not part of Windows: $($foreign -join ', ')" }

$bin = Join-Path $root 'bin'
Copy-Item $exe (Join-Path $bin 'cjpegli.exe') -Force
Copy-Item (Join-Path $src 'LICENSE') (Join-Path $bin 'jpegli-LICENSE.txt') -Force
# The version shown on the About page and written to THIRD-PARTY.txt (package.ps1).
$versionsFile = Join-Path $bin 'versions.json'
$versions = @{}
if (Test-Path $versionsFile) {
    (Get-Content $versionsFile -Raw | ConvertFrom-Json).PSObject.Properties | ForEach-Object { $versions[$_.Name] = $_.Value }
}
$versions['cjpegli'] = "$sha ($date)"
[IO.File]::WriteAllText($versionsFile, ($versions | ConvertTo-Json), (New-Object Text.UTF8Encoding $false))
Write-Host "bin\cjpegli.exe: jpegli $sha ($date)" -ForegroundColor Green
