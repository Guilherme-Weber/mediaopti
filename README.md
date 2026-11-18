# mediaopti

A Windows batch optimizer that makes a media library smaller. It scans folders and replaces a file
only when the new version is smaller **and** passes verification. Dates and attributes are kept,
and replaced originals go to the Recycle Bin (or a backup folder) by default.

## What it does

| Type | Formats | Method |
|---|---|---|
| Videos | mp4, mkv, mov, avi, wmv, webm, flv, m4v, mts, m2ts, ts, 3gp, mpg, mpeg; GIFs over 1 MB | NVENC AV1/HEVC/H.264 or x264. If the preset isn't smaller, stronger settings are retried while VMAF stays ≥ 93. Audio tracks, subtitles, attachments, cover pictures, chapters, tags (recording date, place, camera) and HDR10 metadata are kept (MKV when MP4 can't hold them); PCM audio becomes FLAC. |
| Images | png, jpg, gif, bmp, tga | beeim/pingo, oxipng, ECT, mozjpeg, jpegli, gifsicle. Lossless, or visually lossless (SSIMULACRA2 ≥ 93). |
| Audio | flac, mp3, ogg, wav, m4a (ALAC), aiff, ape, wv, tta | FLAC and OptiVorbis. Cover art optimized; FLAC and Ogg repacked; the decoded audio stays identical. |
| Documents | pdf, docx/xlsx/pptx, odt/ods/odp/odg, epub, cbz, cbr | qpdf, MuPDF and 7-Zip. Pictures inside optimized. Text and drawings are never changed, and PDF pages are rendered and compared. |

Lossless conversions, on by default and each switchable in **Settings**: WAV, AIFF, ALAC, APE,
WavPack and TTA → FLAC (when smaller, tags and cover art kept), BMP/TGA → PNG, CBR → CBZ.

## Safety

- Every result is verified: identical pixels or audio, or a quality floor (SSIMULACRA2 / VMAF ≥ 93).
  A tool's exit code is never trusted on its own.
- Skipped: hard-linked files, files that change during processing, encrypted or signed documents,
  and pictures with data after their end (motion photos, gain maps, game cards).
- A new name is never overwritten. If it already holds the same content, the smaller copy is
  kept; otherwise the new file is numbered (`clip (2).mp4`).
- Processed files are remembered by path and by content, so moved or copied files are not
  processed again.
- A replacement interrupted by a crash or power loss is completed or undone at the next scan,
  from a journal written before any original is set aside. Paths longer than 260 characters
  are supported.

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

Updates: at start-up mediaopti checks the latest GitHub release. When a newer version exists, the
**About** page offers **Update and restart**, which downloads the release zip, verifies its
SHA-256 against the checksum GitHub publishes, and replaces the program files in place (all or
nothing). Settings and history are kept. It is available only while no batch is queued or running.
The same page lists each tool with its version. The included tools are updated with mediaopti's
releases; pingo and beeim are checked for new versions once a week and installed only when
**Install** is clicked.

**About > Advanced options** holds settings meant for testing and troubleshooting, with a warning:
GPU decoding on or off, removing data tracks no container can carry over (GoPro telemetry,
iPhone metadata; otherwise such videos are left as they are), removing metadata (EXIF,
comments and text from pictures, padding from FLAC and MP3 tags; colour profiles, rotation and
audio tags are always kept; optionally also for files already processed without it), and each extra optimizer step of the maximum effort (only the
measured default is on). Every result is still verified before a file is replaced.

For very large libraries, choose *Move to a backup folder*: the Recycle Bin handles only about 25
files per second and drops its oldest items when full.

## External tools

Image, audio and document optimizers are external programs. The release zip includes them all
except pingo and beeim, which are free for private use only and may not be redistributed. When
they are missing, mediaopti downloads them from their author's site (css-ig.net) at start-up into
`%APPDATA%\mediaopti\tools`. Missing tools are skipped; the **About** page lists what was found.

For development, install the tools with:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/fetch-tools.ps1
```

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
(`THIRD-PARTY.txt`); pingo and beeim are downloaded by the app itself.

## Credits and license

Based on [batchviproc](https://github.com/kachurovskiy/batchviproc) by Maxim Kachurovskiy. MIT
License, see [LICENSE](LICENSE). FFmpeg and the bundled tools keep their own licenses.

Developed with the help of [Claude Code](https://www.anthropic.com/claude-code), Anthropic's AI
coding assistant.
