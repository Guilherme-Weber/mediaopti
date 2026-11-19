# Changelog

Notable changes to mediaopti. Earlier versions are described in their
[GitHub releases](https://github.com/Guilherme-Weber/mediaopti/releases).

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
