# Known failures

Causes of failed files found in real runs, so the next one is recognised at once. Start a
diagnosis with `scripts/log-summary.ps1` (the last run's failures grouped by cause), then
look the cause up here. Add an entry for every new cause: the log line, what it really was,
how it was checked and what was done.

Every counted failure is logged as `[<tag>] Failed on <file>: <reason>` (since 3.2.1; before,
video failures had other wordings, which `log-summary.ps1` still reads). Files a failure
"remembered in history" are not tried again: after fixing a false failure, their entries must
be removed from the history for them to be redone.

## Videos

### GIF "doesn't match the GIF (GIF N frames; video N-1 frames)": false, fixed in 3.2.1

- Seen: 11 GIFs on 2026-11-19 (Photoshop/After Effects exports), always exactly one frame
  and one frame's delay short.
- Cause: an application extension (XMP, ~17 KB) after the last picture. ffmpeg's GIF demuxer
  hands it over as one more packet, and `video_timing` counted packets. gifsicle and decoding
  both give N-1 frames: the video was right.
- Fix: a GIF's frames are counted by decoding (`-fps_mode passthrough -enc_time_base demux
  -c:v wrapped_avframe -f framecrc`). Test: `a_gif_with_data_after_its_last_frame_becomes_a_video`.

### "lost its tags (vbr peak, buffer average)" on WMV: false, fixed in 3.2.1

- Seen: 2 WMV (WMA audio, so the output is MKV) on 2026-11-19.
- Cause: the Matroska muxer writes a tag key's spaces as underscores (`VBR Peak` ->
  `VBR_PEAK`); the comparison wanted the same key. The values were kept.
- Fix: `ffmpeg::same_tag_key` treats space and underscore as the same. Test in
  `ffmpeg.rs` (`streams_lost` tags).

### "corrupt or unreadable, ffmpeg can't decode it (...)": real

- Seen: Indeo 5 AVI (`IV50`, 1998): "[indeo5] Corrupted tile data encountered!" on many
  frames. ffmpeg's Indeo 5 decoder doesn't handle every file; re-encoding would lose frames.
  Left as it is (remembered). Before 3.2.1 the line gave no reason.

## Not failures

- `Could not remove temporary file ... (still open by another process: os error 32)`: an
  ffmpeg that was still quitting (Stop, or the end of a quality retry). The next scan's
  cleanup removes it (`settle_orphan_temp`).
- `Kept ...: it holds a QuickTime VR panorama`: left alone on purpose (re-encoding destroys
  the panorama).
- `Video was not smaller ... VMAF NN is below 93`: the quality floor did its job.

## Documents (2026-11-15)

- `could not start ...: The paging file is too small (os error 1455)` / `Insufficient system
  resources (1450)` / `Insufficient quota (1453)`: commit memory ran out (big lossy PDF
  searches in parallel). Since 3.1.0 these are `ToolUnavailable`, never remembered, and big
  searches wait for memory.
