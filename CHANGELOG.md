# Changelog

Notable changes to mediaopti. Earlier versions are described in their
[GitHub releases](https://github.com/Guilherme-Weber/mediaopti/releases).

## 3.4.0

Major throughput and memory-safety overhaul for images, documents and large libraries, plus
diagnostic and reliability fixes.

### Added

- **Durable failure log:** every failed file is also appended to `%APPDATA%/mediaopti/logs/failures.log`
  (never rotated away); `scripts/log-summary.ps1 -Failures` summarises it by cause.
- **Bytes in size readouts:** saved sizes in the log and file/saved sizes in the UI now scale
  down to bytes (`B`) below 1 KB instead of rounding tiny savings up to `1 KB`.

### Changed

- **Faster, memory-budgeted image parallelism:** replaced the single global `BIG_LOSSY` mutex
  with a FIFO memory-budgeted gate (`BigLossyGate`) and a double-ended image queue that pairs
  at most three heavy (`>= 4 MiB`) images with light images across the remaining CPU slots.
  Scope-limited `oxipng` memory promises to `png_chain`, allowed `methods_at_once` to step down
  `n..=2`, shared the best-size limit across sibling lossy methods, and cached the decoded
  reference PNG per `ssimulacra2` search.
- **Lossy-first PNG fast path & streaming verification:** static PNGs in lossy mode run the
  lossy search before the lossless chain (skipping redundant lossless passes when lossy wins).
  Static non-interlaced PNGs are verified row-by-row in Rust (`png` crate, ~64 KiB RAM, no
  `ffmpeg` spawn, including masked alpha without a global lock); probe-guided quality search
  cuts unnecessary `ssimulacra2` evaluations and cleans temp candidates immediately.
- **Multi-threaded document & PDF optimization:** `archive::optimize_pages`, `pdf::optimize_images`
  and `pdf::pages_look_same` borrow idle CPU slots to process pages and embedded images in
  parallel while respecting memory limits.
- **Lower I/O and memory overhead across the pipeline:** streaming PNG/JPEG/GIF trailing-data
  and JPEG segment inspection (no full-file RAM reads), in-place fast path for short ASCII
  pictures, lazy GIF animation check when Videos is off, streaming history load, single
  `ffmpeg -i` probe per video, no double BLAKE2b hash after replace, and no double FLAC
  decode/encode.
- **Clearer UI progress & controls:** the `CPU jobs` tile explains when slots are paused for
  memory or bounded by documents, the rate shows rolling 5-minute `files/min`, `Stop` cancels
  in-progress folder scans cleanly, and the `Forget history` dialog clarifies that files already
  compressed lossily would be compressed again.
- **Bundled FFmpeg updated to 9.1** (`scripts/fetch-ffmpeg.ps1`) and `url` updated to `2.5.9`
  (fixes `RUSTSEC-2026-0592`).

### Fixed

- **Child process handling & shutdown:** `tools::run` no longer captures unused stdout/stderr
  pipes and waits properly on Windows (`WaitForSingleObject`); `shutdown` waits for active
  image workers to finish cleanly; watchdog extends to 180 s at `>= 99%` progress.
- **Safety & edge cases:** preserved pending replace journals on unplugged drives, checked
  sRGB `iCCP` chunks on PNGs, skipped non-UTF-8 and unsupported-compression (LZMA/Deflate64)
  CBZ entries cleanly instead of failing them, fixed lazy O(1) file index lookups, and fixed
  `scripts/log-summary.ps1` `-Failures` switch collision.

## 3.3.2

### Changed

- A file made smaller by a lossy pass says the quality it was measured at, instead of
  "visually lossless": "Saved 1.2 MB (41%) from photo.png (lossy, SSIMULACRA2 84.6)". For a
  comic or a song's cover art it is the lowest score among its pictures; for a PDF, its
  lowest-scoring page.

## 3.3.1

### Changed

- Cover art in FLAC, MP3 and other lossless audio follows the Images quality level (it
  stayed at 93), also while Images is switched off; with Images at lossless only, covers
  stay at 93. Lowering the level checks the audio files once more for smaller covers; the
  sound itself never changes. PDF pictures still stay at 93.

## 3.3.0

### Added

- Pictures have two smaller quality levels besides visually lossless (SSIMULACRA2 ≥ 93):
  "High quality" (≥ 85) and "Smallest" (≥ 80, about what Pinga's lossy mode gives, with
  slight banding seen up close). On five big PNGs, 93 found no lossy version at all (even
  pingo's quality 100 scored 86-89), while Pinga's output scored 81-83 at ~25% of the size.
  Comics follow the setting; cover art and PDF pictures stay at 93. A picture done at a
  higher level is done again at a lower one (moved or renamed too); one done at a lower
  level is not redone. At 85 and 80, JPEGs saved below quality 90 are searched too. On the
  same five PNGs: 33% of the size at 85 (scores 85.4-86.2), 25% at 80 (80.3-81.0; Pinga's
  output was 28%).

### Changed

- Pictures keep the CPU busy: their tools (oxipng, ECT) run on every thread, at
  below-normal priority, with the same results as on one, and their lossy methods search
  side by side, as far as the free memory allows (what each picture takes beyond one thread
  is set aside, so pictures starting together don't all count on the same memory). Five
  big PNGs: 733 s before, 551 s now, the same bytes.
- Audio, documents and pictures use every core but one (it was two); their tools still run
  at below-normal priority, so the desktop and games come first.
- "Forget history" asks in the app's own dialog, which says what it really does (every file
  is processed again; pictures already made smaller would be compressed again), and
  starting the app a second time brings its window to the front instead of a message box.

## 3.2.1

Fixes from a long real run (14 failures, 13 of them false) and from a review of the whole
program. Verification is unchanged; checked on 90 real files and the failed GIF and WMV of
that run (0 failures, moved and renamed files recognised by content).

### Fixed

- Animated GIFs exported with XMP data after their last frame (Photoshop, After Effects) are
  made into videos again: the check took that data for a frame the video had lost and kept
  the GIF. Frames are now counted by decoding the GIF.
- WMV files whose new version is MKV no longer fail with "lost its tags": Matroska writes a
  tag name's spaces as underscores ("VBR Peak" as VBR_PEAK), and that counted as a lost tag.
- Pausing while two videos of the same name in one folder (`clip.avi`, `clip.mkv`) were in
  the quality retry could put one video in the other's place on Resume: their parked files
  shared a name. Each job now parks under its own names.
- A video with a second video track (a dashcam's rear camera, a screen recording's webcam)
  is left as it is: only the first track was re-encoded and the others were lost.
- Lossless audio (FLAC, ALAC, TrueHD, DTS-HD MA, PCM) is never re-encoded to AAC when the
  first encode fails; the CPU fallback copies it.
- CMYK/YCCK JPEGs, alone or inside PDFs, get the lossless pass only: re-encoded, they came
  back as RGB with the CMYK colour profile still attached.
- A video another program had open while it was looked at is tried again next run instead
  of being encoded as if it had no other tracks; the same when an HDR video's first frame
  can't be read.
- Pause or Stop during a quality measurement waits for the measuring ffmpeg to end, so its
  encode can be parked or removed ("os error 32" left temporary files behind).
- Camera MOVs with PCM audio and a timecode track no longer fail with "lost its timecode
  track": their MKV keeps the timecode as a tag.
- EPUB, CBZ and Office files whose folder entries were written compressed (Java, Python)
  keep their optimized pictures: the check expected the folders compressed in the new file.
- A missing tool (flac, OptiVorbis, qpdf, 7-Zip, gifsicle, oxipng, jpegtran) no longer marks
  its files as done: they are left for the next run. Without pingo/beeim or ssimulacra2,
  pictures get the lossless pass and are noted as lossless, so they are searched later.
- "Remove metadata" removes the padding of FLAC files without cover art too.
- Big GIFs optimized before as images (imageopti's history) are made into videos; a big GIF
  that went through the video attempt and the GIF optimizer, then moved or renamed, is
  recognised for both and not optimized again.
- A stripped pass on content already known from a plain pass is noted durably at once.
- Scans skip the folders Windows keeps for itself (hidden and system: `$RECYCLE.BIN`,
  `System Volume Information`) and every backup folder used before, even once the option is
  back to the Recycle Bin.
- A huge picture inside a comic or PDF searches lossily alone and only with its memory free,
  like a picture on its own.
- CPU jobs waking together no longer all start on the same free memory: each one let in a
  moment ago raises what the next one needs.
- A big GIF's GIF pass stops on Pause like other CPU work (it waits in the queue, its video
  is not redone) and notes its content before the original is set aside.
- A copy of the same video or picture is never replaced when it is a hard link, and a video
  that changed while it was encoded is never dropped as a duplicate.
- Switching to visually lossless (or redoing stripped files) no longer reads the whole
  library by content during the scan: only files whose size had the wanted pass are hashed,
  and types switched off are not.

### Changed

- Every failed file is logged as "Failed on <file>: <reason>" and shows in red in the
  activity log (video failures had other wordings and weren't red). A file ffmpeg can't decode
  says why, in ffmpeg's words (e.g. "[indeo5] Corrupted tile data encountered!"); a temporary
  file still open when it was to be removed says that the next scan removes it.

### Development

- `scripts/log-summary.ps1`: the last run's outcome and its failures grouped by cause;
  `docs/known-failures.md` lists the causes found so far.
- The dependency audit (`cargo audit`) runs in CI and in `release.ps1`.
- The engine's end-to-end tests are split by area in `src/engine_tests/`.

## 3.2.0

A release of the window only: what is processed, how it is verified and the results are the
same as in 3.1.0. Built with Rust 1.100.

### Changed

- The window uses one accent colour everywhere: primary buttons and switches are drawn in the
  app's cyan instead of following the Windows accent colour.
- Queue while running: the figures are tiles like the waiting-files tiles; thousands are
  grouped; after the videos, Speed shows files per minute and the GPU tile becomes CPU jobs;
  Failed turns red when a file fails.
- The folder card puts the counts and the history in a footer under the folder and its
  buttons; Change folders is hidden during a batch.
- Files being worked on show the file name first, then its last two folders, with the step,
  time and bar in fixed columns (the names no longer move as the time ticks); one column in a
  narrow window, and each column reads top to bottom.
- The activity log keeps the time in its own column (wrapped lines stay indented), shows
  savings brighter, "kept" lines dimmer and failures in red.
- Progress: a finished type turns green and a line separates the files from the types.
- Advanced options use the same card headers as Settings; About lists the tools in two
  columns; a disabled checkbox looks dimmer than an enabled one; the empty queue's drop zone
  also opens the folder picker when clicked.

## 3.1.0

A release focused on safety and robustness, after an audit of the whole program.

Measured against 3.0.12 on the same machine: the release test corpus (38 files of every kind)
and an 87-minute 1080p AV1 video give byte-identical results in the same time (872 s vs 877 s;
4,082 s vs 4,104 s), with the same memory peak when there is room for it. A queue of a million
files is shown in 2 ms instead of 158 ms and takes 7 bytes per file instead of 129. The
executable grows from 15.1 MB to 19.8 MB (unwinding, which lets an error stay with one file).

### Added

- A replacement journal: before an original is set aside, a line is written to disk. A
  replacement interrupted by a crash or power loss is completed or undone at the next scan.
- Files under paths longer than 260 characters are processed.
- The new version keeps the original's alternate data streams (for example the
  "downloaded from the internet" mark).
- Crash reports can be traced to a file and line by the developer (symbols are kept with each
  release build, not shipped).
- **About > Tools** says when a tool runs through a Scoop shim with arguments.

### Changed

- A program error while processing one file fails only that file; the batch goes on.
- A tool that cannot start or times out no longer marks the file as broken: the file is tried
  again in the next run. After 20 such failures in a row for one kind of file (PDFs when qpdf
  is missing, for example), the remaining files of that kind wait for the next run; the
  others go on.
- The lossy search of a very large picture runs only when its memory is free; otherwise the
  picture gets the lossless pass only.
- Videos reserve twice their size on their drive, shared with the other jobs.
- A video may now be up to 1 second shorter than its original; 1% is allowed only for formats
  whose length is an estimate (MPEG-TS, MPEG-PS, AVI, FLV, ASF).
- The end-of-batch report tells kept, skipped, empty and cut-off files apart.
- Large queues use much less memory, and Stop, Pause, Clear queue and Forget history no longer
  freeze the window while the engine is busy.
- Closing the window during a batch keeps it open ("Stopping…") until every job has ended.
- One unreadable field in the settings file no longer resets every setting.
- cjpegli rebuilt from google/jpegli `1d0b3565` (a hardening fix in its encoder): byte-identical
  output to the previous build on 172 encodes.

### Fixed

- A new version is only ever renamed into place, and the original is kept when the new version
  is missing.
- A backup never overwrites another backup.
- Two files with the same name are treated as the same content only when nothing would be lost
  by keeping either one.
- A numbered name (`clip (2).mp4`) keeps every character of the original name.
- A failure to start ffmpeg for lack of memory is waited out instead of failing the file.
- ffmpeg probes end on Stop, Pause and after a time limit. A probe that doesn't finish never
  decides anything: the video is left as it was and tried again in the next run.
- A video deleted or moved after the scan is skipped instead of being remembered as broken.
- PDF: re-encoded JPEGs are checked to be sequential, and a damaged PDF is rewritten only when
  its pages render the same.
- A JPEG with a header segment shorter than its declared length no longer stops the program.
- An unreadable leftover temporary file whose original is gone is removed again.
- A log line is written when a replaced file's dates and attributes cannot be restored.
- Release builds no longer look for tools in the development folder.
- A long usage text no longer stalls the tool version check.
- After a batch, files that were converted, renamed or removed as duplicates no longer appear
  in the queue as waiting.

### Removed

- Code for systems other than Windows (it was never built or tested).
