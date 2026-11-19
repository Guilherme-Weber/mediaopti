# Makes a release in one go, stopping at the first problem:
#   1. the working tree is clean, on main and in step with origin;
#   2. no control characters in tracked text files (broken backslashes);
#   3. tools: scripts/check-tools.ps1 (stops if a packaged tool has a newer version, unless
#      -SkipToolCheck);
#   4. version set in Cargo.toml; clippy (lint policy), the tests and the dependency audit
#      (cargo audit: known vulnerabilities; install with `cargo install cargo-audit --locked`);
#   5. scripts/package.ps1;
#   6. commit "Release <version>" (no AI attribution), push, GitHub release with the zip and
#      the notes, and the published SHA-256 compared with the local zip.
# Without -Publish it stops after step 5 and puts Cargo.toml/Cargo.lock back (a dry run).
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/release.ps1 -Version 3.0.13 -Notes <notes.md> [-Publish]
# The notes are markdown ("## What's new" ... "## Install"); see earlier releases.

param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Notes,
    [switch]$Publish,
    [switch]$SkipToolCheck
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
function Step([string]$Text) { Write-Host "== $Text" -ForegroundColor Cyan }
function Run([scriptblock]$Block, [string]$What) {
    & $Block
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "version must look like 3.0.13" }
if (-not (Test-Path $Notes)) { throw "notes file not found: $Notes" }
$notesText = Get-Content $Notes -Raw
if ($notesText -notmatch "mediaopti-$([regex]::Escape($Version))-win64\.zip") { throw "the notes don't name mediaopti-$Version-win64.zip" }

Step 'Working tree'
$branch = (git rev-parse --abbrev-ref HEAD).Trim()
if ($branch -ne 'main') { throw "on $branch, not main" }
if (git status --porcelain) { throw 'uncommitted changes: commit them first' }
Run { git fetch --quiet origin } 'git fetch'
if ((git rev-list --count HEAD..origin/main).Trim() -ne '0') { throw 'origin/main has commits this copy lacks: pull first' }
if (git tag --list "v$Version") { throw "v$Version already exists" }
$current = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if ([version]$Version -le [version]$current) { throw "$Version is not newer than $current" }

Step 'Control characters'
$bad = git ls-files '*.rs' '*.md' '*.slint' '*.ps1' '*.toml' '*.json' | ForEach-Object {
    if ([IO.File]::ReadAllText((Join-Path $root $_)) -match '[\x00-\x08\x0b\x0c\x0e-\x1f]') { $_ }
}
if ($bad) { throw "control characters in: $($bad -join ', ')" }

if (-not $SkipToolCheck) {
    Step 'Tools'
    $report = & (Join-Path $PSScriptRoot 'check-tools.ps1') 6>&1 | Out-String
    Write-Host $report.Trim()
    if ($report -notmatch 'Every packaged tool is at its latest version') {
        throw 'a packaged tool has a newer version: update and test it, or pass -SkipToolCheck'
    }
}

Step "Version $current -> $Version"
$toml = Get-Content Cargo.toml -Raw
$toml = ([regex]'(?m)^version = ".+"').Replace($toml, "version = `"$Version`"", 1)
[IO.File]::WriteAllText((Join-Path $root 'Cargo.toml'), $toml, (New-Object Text.UTF8Encoding $false))
try {
    Step 'Clippy'
    Run { cargo clippy --all-targets -- -D warnings } 'clippy'
    Step 'Tests'
    # Every test must really run here: one that would skip (a missing tool) fails instead.
    $env:MEDIAOPTI_REQUIRE_TOOLS = '1'
    try { Run { cargo test } 'tests' } finally { Remove-Item Env:MEDIAOPTI_REQUIRE_TOOLS }
    Step 'Dependency audit'
    if (-not (Get-Command cargo-audit -ErrorAction SilentlyContinue)) { throw 'cargo-audit not found: cargo install cargo-audit --locked' }
    Run { cargo audit } 'cargo audit'
    Step 'Package'
    Run { & (Join-Path $PSScriptRoot 'package.ps1') } 'package'
    $zip = Join-Path $root "dist\mediaopti-$Version-win64.zip"
    if (-not (Test-Path $zip)) { throw "$zip was not made" }
    # (package.ps1 refuses an executable that names this machine's user folder or checkout.)
    # The .pdb stays here, never published (it names this machine's paths): with it, the raw
    # addresses of a user's crash-*.txt report give file and line.
    $symbols = Join-Path $root 'dist\symbols'
    New-Item -ItemType Directory -Force $symbols | Out-Null
    Copy-Item (Join-Path $root 'target\package\release\mediaopti.pdb') (Join-Path $symbols "mediaopti-$Version.pdb")
    $sha = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    Write-Host "$zip  sha256:$sha"

    if (-not $Publish) {
        Write-Host 'Dry run: nothing committed or published (Cargo.toml and Cargo.lock put back).' -ForegroundColor Yellow
        return
    }

    Step 'Commit and push'
    Run { git add Cargo.toml Cargo.lock } 'git add'
    Run { git commit -q -m "Release $Version" } 'git commit'
    Run { git push -q origin main } 'git push'

    Step 'GitHub release'
    Run { gh release create "v$Version" $zip --title "mediaopti $Version" --notes-file $Notes } 'gh release create'
    $digest = (gh release view "v$Version" --json assets -q '.assets[0].digest').Trim()
    if ($digest -ne "sha256:$sha") { throw "published digest $digest differs from the local zip (sha256:$sha)" }
    Write-Host "Released $Version (sha256:$sha)" -ForegroundColor Green
}
finally {
    if (-not $Publish -or (git status --porcelain Cargo.toml)) {
        git checkout -- Cargo.toml Cargo.lock 2>$null
    }
}
