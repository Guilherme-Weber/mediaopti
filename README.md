# mediaopti

Batch media optimizer for Windows. It scans a folder (including subfolders) and makes media smaller.
The original is replaced only when the new file is smaller and passes its checks, and the original's
dates and attributes are kept.

mediaopti merges two tools:

- [batchviproc](https://github.com/Guilherme-Weber/batchviproc) (videos and animated GIFs);
- imageopti (images, audio cover art, PDFs, and Office/EPUB/comic files).

Videos and images work today. Audio and documents are still being ported (see
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

## Images

PNG, JPEG, and GIFs under the size limit are optimized by a separate pool of CPU workers
(cores − 2), running next to the GPU video encodes. This is the imageopti pipeline, ported:

- **PNG:** beeim (pingo if beeim is missing) → oxipng. beeim drops colour profiles and text chunks;
  mediaopti copies them back from the original.
- **JPEG:** jpegtran (mozjpeg) → ECT.
- **GIF:** gifsicle.

Every lossless step must decode to the same pixels: djpeg for JPEG, ffmpeg frame hashes for
PNG/GIF. Only the colour hidden under fully transparent pixels may change. The colour profile,
EXIF rotation, and metadata must survive.

The **Images** setting chooses:

- **Visually lossless** (default): also tries lossy encoders (beeim's quantizer for PNG; jpegli,
  mozjpeg, and pingo for JPEG). It uses the lowest quality that keeps a
  [SSIMULACRA2](https://github.com/cloudinary/ssimulacra2) score of at least 93, the level of
  pingo/Pinga at quality 100. A lossy result must be at least 3% smaller than the lossless one.
  JPEGs saved below quality 90 are not re-encoded, because they can't get smaller at that score.
  One probe skips images that were already optimized.
- **Lossless only:** pixel-identical results only.
- **Don't process images.**

An image is replaced only if the result is at least 0.5% smaller. Hard links, the "changed meanwhile"
check, the Recycle Bin, dates, and attributes work as for videos. On imageopti's test images,
mediaopti produced the same results, for example Kodak PNG −26.4% and a quality-97 JPEG −28.0%.

The optimizers are external programs. Install them with:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/fetch-tools.ps1
```

This installs the open-source ones with [Scoop](https://scoop.sh) (found on PATH). It downloads
cjpegli from libjxl 0.11.2, and pingo and beeim from css-ig.net. pingo and beeim are free for
private use and may not be redistributed, so they are never bundled. mediaopti looks for the tools
in `%APPDATA%\mediaopti\tools.json` (`{"pingo": "C:\\path\\pingo.exe"}`), a `tools` folder next
to `mediaopti.exe`, the project's `bin` folder, and PATH. Missing tools are skipped. The tools found
are listed in the log when a batch with images starts.

The Recycle Bin handles about 25–30 files per second. For a very large image library that becomes
the bottleneck, and the bin's size limit means only the most recent originals stay restorable.
Consider *Delete permanently* for such runs.

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
  has the same format as imageopti's `history-hashes.tsv`, and imageopti's history is imported once.
  Images are remembered per mode: a lossless pass doesn't stop a later lossy one, and an image that
  had a lossy pass is never lossily re-encoded again.
- `ui-settings.json`: the last settings.
- `logs\mediaopti.log`: the activity log, rotated at 5 MB.
- `logs\crash-*.txt`: crash reports, written only if the application fails.

## Roadmap

1. Done: rename, the batchviproc import, content-hash history, the Recycle Bin, safety checks, the
   GIF size limit, and the VMAF-checked retry.
2. Done: images (PNG/JPEG/small GIFs) in a CPU worker pool, the imageopti import, and per-mode
   image history.
3. Audio and documents: cover art in FLAC/MP3, FLAC/Ogg repacking, WAV → FLAC, PDF/Office/EPUB/CBZ,
   and the BMP/TGA/CBR conversions.
4. A faster option than the Recycle Bin for huge image runs (for example a backup folder on the same
   drive).

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
- `src/history.rs`: the location and content histories, and the batchviproc and imageopti imports.
- `src/image/`: the image pipeline (chains, verification, lossy search, PNG/JPEG parsing, tool runner).
- `src/engine_images.rs`: the CPU image workers.
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
