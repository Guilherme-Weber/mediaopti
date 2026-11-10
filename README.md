# mediaopti

Batch media optimizer for Windows. It scans a folder (including subfolders) and makes media smaller.
The original is replaced only when the new file is smaller and passes its checks, and the original's
dates and attributes are kept.

mediaopti merges two tools:

- [batchviproc](https://github.com/Guilherme-Weber/batchviproc) (videos and animated GIFs);
- imageopti (images, audio cover art, PDFs, and Office/EPUB/comic files).

The video part works today. The image, audio, and document parts are still being ported (see
[Roadmap](#roadmap)); until then, imageopti covers them.

## Origin

mediaopti starts from batchviproc 2.0. batchviproc is a fork of
[batchviproc](https://github.com/kachurovskiy/batchviproc) by **Maxim Kachurovskiy**, and the core
workflow comes from that program:

- recursive scanning;
- largest files first;
- a processed-files history;
- replacing the original only when the output is smaller;
- preserving file dates.

The quality and speed settings and the FFmpeg parameters also come from it. The original work is
distributed under the MIT License (see [LICENSE](LICENSE)), which this project keeps.

## Coming from batchviproc

- On first start, batchviproc's processed history and settings are imported from
  `%APPDATA%\batchviproc`, so videos it already handled are not processed again.
- Leftover `.batchviproc.` temporary files are still recognised and cleaned up.

## Videos

Everything batchviproc 2.0 does:

- NVENC AV1/HEVC/H.264 or x264, with 1 to 4 parallel jobs and memory-aware start-up;
- AAC and CPU fallbacks, and handling of real-world problem files;
- Pause/Resume, live per-worker progress, and taskbar progress;
- keep-awake, logs and crash reports, a single instance, and `--start` for scheduled runs.

New in mediaopti:

- **Quality-checked retry ("If not smaller" setting, default VMAF 93).** When the preset output is
  not smaller than the original, mediaopti retries stronger settings (+4, +8, +12, +16 CQ):
  - A result is accepted only if it is smaller and its [VMAF](https://github.com/Netflix/vmaf) score
    against the original stays at or above the floor, with the worst 5% of frames at least 5 below
    it.
  - It keeps climbing while quality holds, and uses the strongest step that passes.
  - Quality is measured on three 8-second windows, or the whole video if it is short.
  - These are the videos batchviproc used to leave untouched. On a test of 30 of them, 21 shrank at
    VMAF ≥ 93, to 66% of their size overall. At the preset, they scored VMAF 95–100 but came out
    bigger than the original.
- **Recycle Bin.** Replaced originals go to the Recycle Bin by default, so you can restore them.
  - If Windows can't recycle a file, it asks instead of deleting it, and nothing is changed if
    recycling fails.
  - The other choice is *Delete permanently*.
  - When the Recycle Bin is full, Windows silently drops its *oldest* items. On a very large run,
    only the most recent originals stay restorable.
- **Content-hash history.** Finished files are also remembered by a BLAKE2b hash of their content,
  so a video that was moved, renamed, or copied is not encoded again. Only files whose size matches
  a processed one are hashed, so the check is almost free.
- **Animated GIFs** under a size limit (default 1 MB) stay GIFs, to be optimized as images. Bigger
  ones become video, as before.
- **Safety checks:**
  - Hard-linked files are skipped, because replacing one would split it from its other names.
  - A file that changes while it is being encoded (still downloading, synced, edited) is left
    unchanged.

## Requirements

- Windows 10 or 11 (64-bit).
- For GPU encoding, an NVIDIA GPU with NVENC. AV1 requires an RTX 40 series or newer.

## Usage

1. Extract the release zip and run `mediaopti.exe`.
2. Choose a folder.
3. Pick the settings: encoder, jobs, quality, speed, the *If not smaller* check, GIF handling, and
   what happens to replaced originals.
4. Click **Start**. Use **Pause/Resume** or **Stop** at any time.

```powershell
mediaopti.exe [--start] [folder]
```

This opens `folder` directly. With `--start`, processing begins immediately with the last-used
settings, which is useful with Windows Task Scheduler.

## Data files

All data is stored in `%APPDATA%\mediaopti`:

- `processedFiles.txt`: the processed history by location (SHA-256 of each path). It has the same
  format as batchviproc.
- `processed-content.tsv`: the processed history by content (`hash`, `size`, `mode` per line). It
  has the same format as imageopti's `history-hashes.tsv`.
- `ui-settings.json`: the last settings.
- `logs\mediaopti.log`: the activity log, rotated at 5 MB.
- `logs\crash-*.txt`: crash reports, written only if the application fails.

## Roadmap

1. Done: rename, the batchviproc import, content-hash history, the Recycle Bin, safety checks, the
   GIF size limit, and the VMAF-checked retry.
2. Images: PNG/JPEG/GIF with the imageopti chains (beeim, pingo, oxipng, ECT, jpegtran, jpegli,
   gifsicle). Every step is verified, with optional lossy encoding at SSIMULACRA2 ≥ 93. This runs
   in a separate CPU worker pool, so images and GPU video encodes don't block each other.
3. Audio and documents: cover art in FLAC/MP3, FLAC/Ogg repacking, WAV → FLAC, PDF/Office/EPUB/CBZ,
   and the BMP/TGA/CBR conversions.
4. A one-time import of imageopti's history.

## Development

Requirements:

- [Rust](https://rustup.rs) (stable, MSVC toolchain);
- Visual Studio Build Tools with the C++ workload.

```powershell
powershell -ExecutionPolicy Bypass -File scripts/fetch-ffmpeg.ps1   # downloads FFmpeg into bin/
cargo run
```

Project layout:

- `src/engine.rs`: queue, workers, fallbacks, the quality retry, pause, and memory control.
- `src/history.rs`: the location and content histories, and the batchviproc import.
- `src/args.rs`: FFmpeg arguments.
- `src/system.rs`: Windows integration (Recycle Bin, hard links, job objects, attributes).
- `src/app.rs`: the bridge between the engine and the interface.
- `ui/app.slint`: the interface.

`scripts/fetch-ffmpeg.ps1` downloads the pinned FFmpeg "essentials" build (currently 9.0.2, which
includes libvmaf) from gyan.dev. It verifies the checksum and checks that the required encoders are
present.

Tests:

```powershell
cargo test                                   # end-to-end tests need bin/ffmpeg.exe and an NVIDIA GPU
cargo test recycles -- --ignored             # moves a temp file into your Recycle Bin
cargo clippy --all-targets --locked -- -D warnings
```

## Building

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

This produces `dist/mediaopti-<version>-win64.zip`, containing `mediaopti.exe` and `ffmpeg.exe`.

## License

MIT. Original work copyright (c) 2021 Maxim Kachurovskiy. See [LICENSE](LICENSE).

FFmpeg is distributed under its own license, included in the release as `ffmpeg-LICENSE.txt`.
