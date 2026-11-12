# mediaopti

A Windows batch optimizer that makes a media library smaller. It scans folders and replaces a file
only when the new version is smaller **and** passes verification. Dates and attributes are kept,
and replaced originals go to the Recycle Bin (or a backup folder) by default.

## What it does

| Type | Formats | Method |
|---|---|---|
| Videos | mp4, mkv, mov, avi, wmv, webm, flv, m4v, mts, m2ts, ts, 3gp, mpg, mpeg; GIFs over 1 MB | NVENC AV1/HEVC/H.264 or x264. If the preset isn't smaller, stronger settings are retried while VMAF stays ≥ 93. |
| Images | png, jpg, gif, bmp, tga | beeim/pingo, oxipng, ECT, jpegtran, gifsicle. Lossless, or visually lossless (SSIMULACRA2 ≥ 93). |
| Audio | flac, mp3, ogg, wav | Cover art optimized; FLAC and Ogg repacked; the decoded audio stays identical. |
| Documents | pdf, docx/xlsx/pptx, odt/ods/odp/odg, epub, cbz, cbr | Pictures inside optimized. Text and drawings are never changed, and PDF pages are rendered and compared. |

Optional lossless conversions: WAV → FLAC, BMP/TGA → PNG, CBR → CBZ.

## Safety

- Every result is verified: identical pixels or audio, or a quality floor (SSIMULACRA2 / VMAF ≥ 93).
  A tool's exit code is never trusted on its own.
- Skipped: hard-linked files, files that change during processing, encrypted or signed documents,
  and pictures with data after their end (motion photos, gain maps, game cards).
- A new name is never overwritten. If it already holds the same content, the smaller copy is
  kept; otherwise the new file is numbered (`clip (2).mp4`).
- Processed files are remembered by path and by content, so moved or copied files are not
  processed again.

## Requirements

- Windows 10 or 11, 64-bit.
- An NVIDIA GPU with NVENC for GPU video encoding (AV1 needs an RTX 40 series or newer).

## Usage

1. Extract the release zip and run `mediaopti.exe`.
2. **Queue:** choose a folder, or drag files and folders onto the window. Files dropped during a
   run join the running queue.
3. **Settings:** switch each type on or off and choose its quality. Settings are saved
   automatically.
4. Click **Start**. **Pause** and **Stop** work at any time.

Files run in stages, each sized to its bottleneck: videos (on the GPU), then audio (2 at a time,
disk-bound), documents, and images (on every free core).

Command line: `mediaopti.exe [--start] [folder]` opens a folder; `--start` begins at once with the
last settings (useful with Task Scheduler).

For very large libraries, choose *Move to a backup folder*: the Recycle Bin handles only about 25
files per second and drops its oldest items when full.

## External tools

Image, audio and document optimizers are external programs. Install them with:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/fetch-tools.ps1
```

The open-source tools come from [Scoop](https://scoop.sh); cjpegli, OptiVorbis, pingo and beeim are
downloaded. pingo and beeim are free for private use only and are never bundled. Missing tools are
skipped; the **About** page lists what was found.

## Data

Stored in `%APPDATA%\mediaopti`: settings, the processed history (`processedFiles.txt`,
`processed-content.tsv`), and logs. The batchviproc and imageopti histories are imported on first
start.

## Development

Requires [Rust](https://rustup.rs) (MSVC) and the Visual Studio C++ build tools.

```powershell
powershell -ExecutionPolicy Bypass -File scripts/fetch-ffmpeg.ps1   # FFmpeg into bin/
cargo run
cargo test                                                          # needs bin/ffmpeg.exe and an NVIDIA GPU
cargo clippy --all-targets --locked -- -D warnings
powershell -ExecutionPolicy Bypass -File scripts/package.ps1        # dist/mediaopti-<version>-win64.zip
```

The release zip includes FFmpeg and the redistributable tools with their licences
(`THIRD-PARTY.txt`). On another PC, run `fetch-tools.ps1 -NoScoop -Dest <folder>\tools` to add
pingo and beeim.

## Credits and license

Based on [batchviproc](https://github.com/kachurovskiy/batchviproc) by Maxim Kachurovskiy. MIT
License, see [LICENSE](LICENSE). FFmpeg and the bundled tools keep their own licenses.
