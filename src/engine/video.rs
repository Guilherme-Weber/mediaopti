//! One video job: claim, encode with fallbacks, check, commit, and finish through one funnel.

use super::*;

/// Every exit path of a claimed video job funnels through [`Engine::finish_video`].
enum VideoResult {
    /// Replaced original (in place or at a newly claimed target name).
    Replaced { target: PathBuf, renamed: bool, saved_bytes: u64, final_size: u64, pct: i64 },
    /// Same-content existing copy replaced with our smaller version; original disposed.
    DuplicateReplaced { existing: PathBuf, saved_bytes: u64, final_size: u64 },
    /// Same-content existing copy was already smaller; original disposed.
    DuplicateKeptExisting { existing: PathBuf, saved_bytes: u64, final_size: u64 },
    /// Original kept unchanged and remembered in history.
    Kept { log: String, suppress_gif_pass: bool },
    /// Skipped for now (not remembered; retried next run).
    Skipped { log: String, gone: bool },
    /// Failed this run only (not remembered; retried next run).
    Failed { log: String },
    /// Unrecoverable failure (remembered in history, will not retry).
    Broken { log: String },
    /// Fatal ffmpeg startup error (stops remaining video workers).
    Fatal { log: String },
    /// Ended early by Stop or Pause (requeues on Pause, keeping optional resume memo).
    EndedEarly { memo: Option<vmaf::VideoResume> },
    /// Handed off to the GIF image optimizer (already accounted for by `finish_image`).
    HandedToGif(Outcome),
}

impl Engine {
    /// A GIF whose video was not smaller: it stays a GIF and goes through the GIF optimizer,
    /// whose own checks decide. Only then: a GIF the video path failed on (encode error,
    /// damaged, not animated, no free name) is left as it is (`remember_without_gif_pass`).
    /// `None`: not a GIF, or images are off.
    async fn gif_fallback(self: &Arc<Self>, worker_id: u32, input: &Path, display: &str) -> Option<Outcome> {
        if !args::is_gif(input) || self.lock().options.images == ImageMode::Off {
            return None;
        }
        let outcome = self.gif_as_image(worker_id, input, display).await;
        // Done as a video, even when the GIF pass was stopped: later runs only retry the GIF
        // optimizer if it didn't finish (see `WorkQueue::place`), never the encode.
        self.add_processed(input);
        if matches!(outcome, Outcome::Stopped) && !self.lock().stop_requested {
            // A Pause ended the GIF pass: the file waits in the CPU queue (`finish_image`),
            // and this video worker carries on after Resume.
            return Some(Outcome::Released);
        }
        Some(outcome)
    }

    /// A GIF the video path failed on is done for the GIF optimizer too (all image passes),
    /// so `WorkQueue::place` doesn't send it there later: only GIFs whose video wasn't smaller go.
    fn remember_without_gif_pass(&self, input: &Path) {
        if args::is_gif(input) {
            self.lock().history.add_all_image_passes(input);
        }
    }

    pub(super) async fn process_one(self: &Arc<Self>, worker_id: u32) -> Outcome {
        let cancel = self.lock().cancel.clone();
        if !self.wait_while_paused(&cancel).await {
            return Outcome::Stopped;
        }
        let claimed = {
            let mut inner = self.lock();
            if !inner.working || inner.stop_requested {
                return Outcome::Stopped;
            }
            let next = {
                let Inner { queue, history, .. } = &mut *inner;
                queue.claim_video(history).map(|input| (queue.display_name(&input), input))
            };
            if next.is_none() {
                // Leaving now, in the same lock as the check: files added later start a
                // new worker instead of waiting for this one.
                inner.video_ids.remove(&worker_id);
            }
            next
        };
        let Some((display, input)) = claimed else {
            return Outcome::Empty;
        };
        // The job runs as a task of its own: a panic in it (a bug, a malformed file nobody
        // foresaw) fails this file only, instead of ending the worker with its bookkeeping
        // half done.
        // It keeps the release token it started with: a Pause swaps `Inner::release` for a
        // new one, and the job must still see that it was released (`job_released`).
        let job = {
            let (engine, input, display) = (Arc::clone(self), input.clone(), display.clone());
            let token = self.lock().release.clone();
            tokio::spawn(super::ffmpeg::JOB_TOKEN.scope(token, async move { engine.process_claimed(worker_id, input, display).await }))
        };
        let result = match job.await {
            Ok(res) => res,
            Err(err) if err.is_panic() => {
                let why = super::images::panic_text(err.into_panic().as_ref());
                VideoResult::Failed {
                    log: format!("[Worker {worker_id}] Failed on {display}: internal error: {why}; left as it was (tried again next run)"),
                }
            }
            Err(_) => VideoResult::EndedEarly { memo: None },
        };
        self.finish_video(worker_id, &input, &display, result).await
    }

    /// Single exit funnel for every claimed video job (mirrors `finish_image`).
    async fn finish_video(self: &Arc<Self>, worker_id: u32, input: &Path, display: &str, result: VideoResult) -> Outcome {
        match result {
            VideoResult::HandedToGif(outcome) => outcome,
            VideoResult::Replaced { target, renamed, saved_bytes, final_size, pct } => {
                let saved_mb = saved_bytes as f64 / (1024.0 * 1024.0);
                // Content was already hashed and recorded durably inside `replace_original`.
                self.add_processed(input);
                {
                    let mut inner = self.lock();
                    inner.queue.release_video(input);
                    inner.workers.remove(&worker_id);
                    if renamed {
                        inner.history.add(&target);
                        inner.queue.mark_moved(input, &target, Some(final_size));
                    }
                    inner.run.completed += 1;
                    inner.run.compressed += 1;
                    inner.space_saved_mb += saved_mb;
                }
                self.log(format!("[Worker {worker_id}] Saved {} MB ({pct}%) from {display}", round1(saved_mb)));
                self.send_state(false);
                Outcome::Processed
            }
            VideoResult::DuplicateReplaced { existing, saved_bytes, final_size } => {
                let name = existing.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.add_processed(&existing);
                {
                    let mut inner = self.lock();
                    inner.queue.release_video(input);
                    inner.workers.remove(&worker_id);
                    inner.queue.mark_moved(input, &existing, Some(final_size));
                    inner.run.completed += 1;
                    inner.run.compressed += 1;
                    inner.space_saved_mb += saved_bytes as f64 / (1024.0 * 1024.0);
                }
                self.log(format!(
                    "[Worker {worker_id}] {display}: {name} was the same video; it now holds the smaller new version and the original was removed"
                ));
                self.send_state(false);
                Outcome::Processed
            }
            VideoResult::DuplicateKeptExisting { existing, saved_bytes, final_size } => {
                let name = existing.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.remember(&existing, &existing).await;
                {
                    let mut inner = self.lock();
                    inner.queue.release_video(input);
                    inner.workers.remove(&worker_id);
                    inner.queue.mark_moved(input, &existing, Some(final_size));
                    inner.run.completed += 1;
                    inner.run.compressed += 1;
                    inner.space_saved_mb += saved_bytes as f64 / (1024.0 * 1024.0);
                }
                self.log(format!("[Worker {worker_id}] {display}: {name} is the same video and smaller; the original was removed"));
                self.send_state(false);
                Outcome::Processed
            }
            VideoResult::Kept { log, suppress_gif_pass } => {
                self.release_tracking(worker_id, input);
                self.remember(input, input).await;
                if suppress_gif_pass {
                    self.remember_without_gif_pass(input);
                }
                {
                    let mut inner = self.lock();
                    inner.run.completed += 1;
                    inner.run.kept += 1;
                }
                if !log.is_empty() {
                    self.log(log);
                }
                self.send_state(false);
                Outcome::Skipped
            }
            VideoResult::Skipped { log, gone } => {
                {
                    let mut inner = self.lock();
                    inner.queue.release_video(input);
                    inner.workers.remove(&worker_id);
                    if gone {
                        inner.queue.mark_gone(input);
                    }
                    inner.run.completed += 1;
                    inner.run.skipped += 1;
                }
                if !log.is_empty() {
                    self.log(log);
                }
                self.send_state(false);
                Outcome::Skipped
            }
            VideoResult::Failed { log } => {
                {
                    let mut inner = self.lock();
                    inner.queue.release_video(input);
                    inner.workers.remove(&worker_id);
                    inner.queue.mark_failed(input);
                    inner.run.completed += 1;
                    inner.run.failed += 1;
                }
                self.log(log);
                self.send_state(true);
                Outcome::Failed
            }
            VideoResult::Broken { log } => {
                {
                    let mut inner = self.lock();
                    inner.queue.release_video(input);
                    inner.workers.remove(&worker_id);
                    inner.queue.mark_failed(input);
                    inner.run.completed += 1;
                    inner.run.failed += 1;
                }
                self.remember(input, input).await;
                self.remember_without_gif_pass(input);
                self.log(log);
                self.send_state(true);
                Outcome::Failed
            }
            VideoResult::Fatal { log } => {
                {
                    let mut inner = self.lock();
                    inner.queue.release_video(input);
                    inner.workers.remove(&worker_id);
                    inner.queue.mark_failed(input);
                    inner.run.completed += 1;
                    inner.run.failed += 1;
                }
                self.log(log);
                self.send_state(true);
                Outcome::Fatal
            }
            VideoResult::EndedEarly { memo } => {
                let (released, kept) = {
                    let mut inner = self.lock();
                    inner.workers.remove(&worker_id);
                    inner.queue.release_video(input);
                    let released = inner.working && !inner.stop_requested;
                    let mut kept = false;
                    if released {
                        inner.queue.requeue_video(input);
                        if let Some(m) = memo.filter(|m| m.primary.is_some()) {
                            inner.resume.insert(input.to_path_buf(), m);
                            kept = true;
                        }
                    } else if let Some(m) = memo {
                        m.discard();
                    }
                    (released, kept)
                };
                self.log(if released && kept {
                    format!("[Worker {worker_id}] {display} paused; on Resume it continues from its last finished step")
                } else if released {
                    format!("[Worker {worker_id}] {display} paused; on Resume its encode starts again")
                } else {
                    format!("[Worker {worker_id}] Stopped while processing {display}")
                });
                self.send_state(true);
                if released { Outcome::Released } else { Outcome::Stopped }
            }
        }
    }

    fn unchecked_result(worker_id: u32, display: &str, what: Option<&str>) -> VideoResult {
        let what = what.unwrap_or("its tracks");
        VideoResult::Failed {
            log: format!(
                "[Worker {worker_id}] Failed on {display}: {what} could not be checked (ffmpeg didn't finish in time or couldn't start); left as it was (tried again next run)"
            ),
        }
    }

    /// One claimed video, from its checks to its replacement.
    async fn process_claimed(self: &Arc<Self>, worker_id: u32, input: PathBuf, display: String) -> VideoResult {
        system::panic_here_in_tests(&input);

        // Size and date now, to notice if the file changes while it is being encoded
        // (still downloading, edited, synced). Also leave hard-linked files alone:
        // replacing one would split it from its other names.
        let path = input.clone();
        let (source_state, links) = self
            .blocking(move || (file_state(&path), system::hard_link_count(&path).unwrap_or(1)))
            .await;
        // Gone since the scan (moved, deleted, or set aside as the copy of a video with the
        // same content): nothing to do, and nothing to remember.
        if source_state.is_none() {
            return VideoResult::Skipped {
                log: format!("[Worker {worker_id}] Skipped {display}: it is no longer there"),
                gone: true,
            };
        }
        // Like the CPU files' work folders: the new version (and the quality retry's second
        // one) is written next to the original, so its drive needs room for about twice it.
        // Held for the whole job, so videos encoding at once on one drive share its space.
        let size = source_state.map_or(0, |s| s.0);
        let disk = self.budget.reserve_disk(input.parent().unwrap_or(&input), size.saturating_mul(2), crate::budget::VIDEO_DISK_RESERVE);
        let _disk = match disk {
            Ok(d) => d,
            Err(free) => {
                let gb = |b: u64| b as f64 / 1_073_741_824.0;
                return VideoResult::Skipped {
                    log: format!(
                        "[Worker {worker_id}] Skipped {display}: its drive has {:.1} GB free for it, about {:.1} GB is needed (it will be tried next time)",
                        gb(free),
                        gb(size.saturating_mul(2) + crate::budget::VIDEO_DISK_RESERVE)
                    ),
                    gone: false,
                };
            }
        };
        if links > 1 {
            return VideoResult::Skipped {
                log: format!("[Worker {worker_id}] Skipped {display}: it is a hard link ({links} names for one file)"),
                gone: false,
            };
        }

        // Skip static 1-frame GIFs/WebPs and tiny spacers immediately.
        if args::is_animated_picture(&input) {
            let path = input.clone();
            let webp = args::is_webp(&input);
            let valid = self
                .blocking(move || {
                    std::fs::File::open(&path).is_ok_and(|f| {
                        let r = std::io::BufReader::new(f);
                        if webp {
                            system::is_animated_and_valid_webp(r)
                        } else {
                            system::is_animated_and_valid_gif(r)
                        }
                    })
                })
                .await;
            if !valid {
                let kind_name = if webp { "WebP" } else { "GIF" };
                return VideoResult::Skipped {
                    log: format!(
                        "[Worker {worker_id}] Skipped {display}: it is no longer an animated {kind_name} (changed since the scan); it will be checked again next time"
                    ),
                    gone: false,
                };
            }
        }

        // What it holds besides the picture decides the container (MKV when MP4 can't
        // hold it) and is carried over by every attempt, then checked in the result.
        let streams = if args::is_animated_picture(&input) { Ok(args::Streams::default()) } else { self.probe_streams(&input).await };
        let Ok(streams) = streams else {
            if self.job_released() {
                return VideoResult::EndedEarly { memo: None };
            }
            return Self::unchecked_result(worker_id, &display, None);
        };
        let drop_data = self.lock().options.drop_data;
        if let Some(blocked) = streams.blocker(drop_data) {
            let hint = if blocked.permanent { "" } else { " (Advanced: Remove data tracks lets it be optimized without them)" };
            let verb = if blocked.permanent { "Kept" } else { "Skipped" };
            let log = format!("[Worker {worker_id}] {verb} {display}: it holds {}; left as it is{hint}", blocked.why);
            return if blocked.permanent {
                VideoResult::Kept { log, suppress_gif_pass: false }
            } else {
                VideoResult::Skipped { log, gone: false }
            };
        }
        let dropped = streams.dropped_data();
        if !dropped.is_empty() {
            self.log(format!("[Worker {worker_id}] {display}: its data tracks ({}) are left out (Advanced option)", dropped.join(", ")));
        }
        let ext = args::output_ext(&input, &streams);
        let tmp = super::replace::unclaimed_temp(|n| args::temp_name_with(&input, n, &ext), worker_id);
        self.reset_worker(worker_id, &display, "encoding");
        self.log_remove(&tmp);
        self.send_state(true);
        self.log(format!("[Worker {worker_id}] Processing: {display}"));

        // Audio is copied (MKV takes what MP4 can't); re-encoding it is only the retry when
        // the muxer refuses it. A picture smaller than NVENC takes goes straight to x264.
        let encoder = self.lock().options.encoder.clone();
        let encoder_is_nvenc = encoder.ends_with("_nvenc");
        let too_small_for_nvenc = encoder_is_nvenc && streams.size.is_some_and(|size| !args::nvenc_fits(&encoder, size));
        let initial_mode = if too_small_for_nvenc { Mode::CpuFallback } else { Mode::Primary };
        // Paused before: what it had finished (dropped if the settings changed meanwhile).
        let mut memo = {
            let mut inner = self.lock();
            let key = vmaf::resume_key(&inner.options);
            match inner.resume.remove(&input) {
                Some(m) if m.settings == key => m,
                stale => {
                    if let Some(m) = stale {
                        m.discard();
                    }
                    vmaf::VideoResume { settings: key, ..Default::default() }
                }
            }
        };
        let primary_done = memo.primary;
        let gpu = match primary_done {
            Some((_, _, _, gpu)) => gpu,
            None if too_small_for_nvenc => None,
            None => self.gpu_decode_for(&streams, &input),
        };
        let plan_for = |mode: Mode, gpu_decode: Option<(u32, u32)>| args::EncodePlan {
            mode,
            cq_boost: 0,
            gpu_decode,
            streams: streams.clone(),
        };
        let mut attempt = match primary_done {
            Some((_, duration, _, _)) => {
                self.log(format!("[Worker {worker_id}] {display}: the preset encode was done before the pause; continuing with the quality retry"));
                let mut diag = Diagnostics::default();
                diag.duration_sec = duration;
                Attempt { status: AttemptStatus::Exited(0), diag }
            }
            None => self.encode(worker_id, &input, &tmp, &display, plan_for(initial_mode, gpu)).await,
        };
        // The settings the final output was made with; the quality retry reuses them.
        let mut used_mode = primary_done.map_or(initial_mode, |p| p.2);

        // Audio-only file (e.g. a voice note in .mp4): skip and remember.
        if attempt.diag.no_video_stream {
            self.log_remove(&tmp);
            return VideoResult::Kept {
                log: format!("[Worker {worker_id}] Kept audio-only file (no video stream): {display}"),
                suppress_gif_pass: false,
            };
        }

        // Corrupt or incomplete file: remember it so it is never retried. Out of memory also
        // prints "Invalid data found"; that file is fine.
        if attempt.diag.corrupt_input && !attempt.diag.out_of_memory {
            self.log_remove(&tmp);
            let why = attempt.diag.reason().unwrap_or_else(|| "no details".into());
            return VideoResult::Broken {
                log: format!(
                    "[Worker {worker_id}] Failed on {display}: corrupt or unreadable, ffmpeg can't decode it ({why}); remembered in history (will not retry)"
                ),
            };
        }

        let can_retry = |engine: &Engine, a: &Attempt| {
            let inner = engine.lock();
            matches!(a.status, AttemptStatus::Exited(code) if code != 0)
                && !a.diag.corrupt_input
                && !a.diag.no_video_stream
                && !a.diag.out_of_memory
                && !inner.stop_requested
                && inner.working
        };
        let mut tried_cpu = too_small_for_nvenc;

        // Tiny GIF/sticker or stream rejected by the NVENC hardware -> CPU x264.
        if can_retry(self, &attempt) && attempt.diag.nvenc_unsupported {
            tried_cpu = true;
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] NVENC hardware limit on {display}; falling back to CPU software encoder..."
            ));
            self.reset_worker(worker_id, &display, "encoding on the CPU");
            attempt = self.encode(worker_id, &input, &tmp, &display, plan_for(Mode::CpuFallback, None)).await;
            used_mode = Mode::CpuFallback;
        }

        // Audio stream copy rejected by the muxer -> transcode audio to AAC. Never lossless
        // audio: the check only counts tracks, so FLAC/TrueHD/PCM would silently become AAC
        // (and the failure is rarely the audio's; the CPU fallback below copies it).
        if can_retry(self, &attempt) && !tried_cpu && initial_mode == Mode::Primary && !args::is_animated_picture(&input) && streams.lossless_audio {
            self.log(format!(
                "[Worker {worker_id}] The first encode failed on {display}; its audio is lossless, so it is not re-encoded to AAC"
            ));
        }
        if can_retry(self, &attempt)
            && !tried_cpu
            && initial_mode == Mode::Primary
            && !args::is_animated_picture(&input)
            && !streams.lossless_audio
        {
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Stream copy failed on {display}; retrying with AAC audio transcode..."
            ));
            self.reset_worker(worker_id, &display, "encoding, audio as AAC");
            attempt = self.encode(worker_id, &input, &tmp, &display, plan_for(Mode::TranscodeAudio, gpu)).await;
            used_mode = Mode::TranscodeAudio;
        }

        // Final safety net: CPU fallback for NVENC encoders.
        if can_retry(self, &attempt) && !tried_cpu && encoder_is_nvenc {
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Hardware pipeline failed on {display}; attempting final CPU software fallback..."
            ));
            self.reset_worker(worker_id, &display, "encoding on the CPU");
            attempt = self.encode(worker_id, &input, &tmp, &display, plan_for(Mode::CpuFallback, None)).await;
            used_mode = Mode::CpuFallback;
        }

        let stop_requested = self.lock().stop_requested;
        if matches!(attempt.status, AttemptStatus::Stopped) || stop_requested {
            self.log_remove(&tmp);
            return VideoResult::EndedEarly { memo: None };
        }

        let code = match attempt.status {
            AttemptStatus::SpawnError(err) => {
                self.log_remove(&tmp);
                return VideoResult::Fatal {
                    log: format!(
                        "[Worker {worker_id}] Failed on {display}: could not start ffmpeg ({}): {err}",
                        self.ffmpeg_path.display()
                    ),
                };
            }
            AttemptStatus::Exited(code) => code,
            AttemptStatus::Stopped => unreachable!(),
        };

        if code != 0 && attempt.diag.out_of_memory {
            self.log_remove(&tmp);
            return VideoResult::Failed {
                log: format!(
                    "[Worker {worker_id}] Failed on {display}: not enough memory to encode it (GPU or system); it will be tried again next run"
                ),
            };
        }
        if code != 0 {
            self.log_remove(&tmp);
            for line in &attempt.diag.error_lines {
                self.log(line.clone());
            }
            let why = attempt.diag.reason().unwrap_or_else(|| "no error printed".into());
            return VideoResult::Broken {
                log: format!("[Worker {worker_id}] Failed on {display}: ffmpeg exited with code {code} ({why}); remembered in history"),
            };
        }

        let (in_path, tmp_path) = (input.clone(), tmp.clone());
        let resumed_size = primary_done.map(|p| p.0);
        let metas = self
            .blocking(move || {
                let out = match resumed_size {
                    Some(s) => Ok(s as u64),
                    None => std::fs::metadata(&tmp_path).map(|m| m.len()),
                };
                (std::fs::metadata(&in_path), out)
            })
            .await;
        let (in_meta, out_len) = match metas {
            (Ok(i), Ok(o)) => (i, o),
            (Err(err), _) | (_, Err(err)) => {
                self.log_remove(&tmp);
                return VideoResult::Failed {
                    log: format!(
                        "[Worker {worker_id}] Failed on {display}: could not read the size of the original or the new version: {err}"
                    ),
                };
            }
        };

        let in_size = in_meta.len() as f64;
        let mut out_size = out_len as f64;

        if out_size >= in_size {
            // The preset could not shrink it (typically a source that is already
            // compressed): retry stronger settings, keeping the measured quality.
            let floor = self.lock().options.vmaf_floor;
            let retry = match floor {
                Some(floor) if in_size > 0.0 => {
                    memo.primary = Some((out_size, attempt.diag.duration_sec, used_mode, gpu));
                    let retry_plan = plan_for(used_mode, gpu);
                    let r = self
                        .quality_retry(worker_id, &input, &tmp, &display, &retry_plan, in_size, out_size, attempt.diag.duration_sec, floor, &mut memo)
                        .await;
                    self.lock().workers.remove(&worker_id);
                    r
                }
                _ => Retry::Rejected("quality check off".into()),
            };
            match retry {
                Retry::Stopped => {
                    self.log_remove(&tmp);
                    return VideoResult::EndedEarly { memo: Some(memo) };
                }
                Retry::Accepted { boost, vmaf, worst } => {
                    let t = tmp.clone();
                    out_size = self.blocking(move || std::fs::metadata(t).map_or(in_size, |m| m.len() as f64)).await;
                    self.log(format!(
                        "[Worker {worker_id}] Quality retry for {display}: +{boost} CQ, {:.0}% of the original, VMAF {vmaf:.1} (worst 5%: {worst:.1})",
                        out_size / in_size * 100.0
                    ));
                }
                Retry::Rejected(reason) => {
                    self.log_remove(&tmp);
                    self.log(format!(
                        "[Worker {worker_id}] Video was not smaller for {display}; kept the original file ({reason})"
                    ));
                    // A GIF that doesn't make a smaller video stays a GIF, but is still
                    // optimized as an image (e.g. slideshow-like "process" GIFs).
                    if let Some(outcome) = self.gif_fallback(worker_id, &input, &display).await {
                        return VideoResult::HandedToGif(outcome);
                    }
                    return VideoResult::Kept {
                        log: String::new(),
                        suppress_gif_pass: false,
                    };
                }
            }
        }
        // A GIF's video must keep its animation: same frames, same length. Any other video
        // must keep everything else it held (audio tracks, subtitles, attachments, covers,
        // chapters) and its length.
        let lost: Result<Option<String>, super::ffmpeg::Unfinished> = async {
            if args::is_animated_picture(&input) {
                let (anim_timing, video_timing) = (self.video_timing(&input).await?, self.video_timing(&tmp).await?);
                let label = if args::is_webp(&input) { "WebP" } else { "GIF" };
                return Ok(super::ffmpeg::timing_mismatch(anim_timing, video_timing).map(|why| format!("doesn't match the {label} ({why})")));
            }
            let after = self.probe_streams(&tmp).await?;
            let mut why = super::ffmpeg::streams_lost(&streams, &after);
            // PCM made FLAC must decode to exactly the original samples.
            for (i, codec) in streams.audio_codecs.iter().enumerate() {
                if why.is_none() && args::is_integer_pcm(codec) && !self.same_audio_track(&input, &tmp, i).await? {
                    why = Some(format!("audio track {} (its FLAC doesn't decode to the original PCM)", i + 1));
                }
            }
            Ok(why.map(|why| format!("lost {why}")))
        }
        .await;
        // Stop or Pause cut the checks short: that says nothing about the new version.
        if self.job_released() {
            self.log_remove(&tmp);
            return VideoResult::EndedEarly { memo: None };
        }
        let Ok(lost) = lost else {
            self.log_remove(&tmp);
            return Self::unchecked_result(worker_id, &display, Some("its new version"));
        };
        if let Some(why) = lost {
            self.log_remove(&tmp);
            return VideoResult::Broken {
                log: format!("[Worker {worker_id}] Failed on {display}: the new version {why}; kept the original (remembered in history)"),
            };
        }
        self.place_new_version(worker_id, input, display, tmp, attempt.diag.duration_sec, (in_size as u64, out_size as u64), source_state)
            .await
    }

    /// The last step of a video job whose new version passed its checks: delegates name claiming,
    /// same-content resolution (Option A for hard links), freshness verification, replacement,
    /// and attribute restoration to `replace.rs`.
    #[allow(clippy::too_many_arguments)]
    async fn place_new_version(
        self: &Arc<Self>,
        worker_id: u32,
        input: PathBuf,
        display: String,
        tmp: PathBuf,
        duration_sec: f64,
        (in_size, out_size): (u64, u64),
        source_state: Option<(u64, Option<std::time::SystemTime>)>,
    ) -> VideoResult {
        let in_ext = args::ext_lower(&input);
        let tmp_ext = args::ext_lower(&tmp);
        let desired_target = if in_ext == tmp_ext {
            input.clone()
        } else {
            let stem = input.file_stem().map_or_default(|s| s.to_string_lossy().into_owned());
            let ext = tmp.extension().map_or_default(|e| format!(".{}", e.to_string_lossy()));
            input.with_file_name(format!("{stem}{ext}"))
        };

        let disposal = self.lock().disposal_for(&input);
        let (in_c, dt_c) = (input.clone(), desired_target.clone());
        let first_step = self.blocking(move || super::replace::try_claim_first(&in_c, &dt_c)).await;
        let outcome = match first_step {
            super::replace::ClaimStep::Ready { target, claimed } => {
                let (engine, in_c, tmp_c) = (Arc::clone(self), input.clone(), tmp.clone());
                self.blocking(move || {
                    super::replace::commit_claimed(
                        &in_c,
                        &target,
                        claimed,
                        source_state,
                        Some(super::replace::Candidate::SiblingTemp { path: &tmp_c, size: out_size }),
                        &disposal,
                        &|p| engine.note_content_now(p, history::Pass::Video).map(|_| ()),
                        &|msg| engine.log(msg),
                    )
                })
                .await
            }
            super::replace::ClaimStep::Collision(existing) => match self.same_video(&input, &existing, duration_sec).await {
                Err(Stopped) => {
                    self.log_remove(&tmp);
                    return VideoResult::EndedEarly { memo: None };
                }
                Ok(true) => {
                    let (engine, in_c, tmp_c) = (Arc::clone(self), input.clone(), tmp.clone());
                    self.blocking(move || {
                        super::replace::commit_same_content(
                            &in_c,
                            &existing,
                            source_state,
                            in_size,
                            Some(super::replace::Candidate::SiblingTemp { path: &tmp_c, size: out_size }),
                            &disposal,
                            &|p| engine.lock().queue.is_in_progress(p),
                            &|p| engine.note_content_now(p, history::Pass::Video).map(|_| ()),
                            &|msg| engine.log(msg),
                        )
                    })
                    .await
                }
                Ok(false) => {
                    let existing_c = existing.clone();
                    if let Some(target) = self.blocking(move || super::replace::claim_numbered(&existing_c)).await {
                        let (engine, in_c, tmp_c) = (Arc::clone(self), input.clone(), tmp.clone());
                        self.blocking(move || {
                            super::replace::commit_claimed(
                                &in_c,
                                &target,
                                true,
                                source_state,
                                Some(super::replace::Candidate::SiblingTemp { path: &tmp_c, size: out_size }),
                                &disposal,
                                &|p| engine.note_content_now(p, history::Pass::Video).map(|_| ()),
                                &|msg| engine.log(msg),
                            )
                        })
                        .await
                    } else {
                        self.log_remove(&tmp);
                        super::replace::CommitOutcome::NoFreeName
                    }
                }
            },
        };

        match outcome {
            super::replace::CommitOutcome::Replaced { target, renamed, size } => VideoResult::Replaced {
                target,
                renamed,
                saved_bytes: in_size.saturating_sub(size),
                final_size: size,
                pct: (in_size.saturating_sub(size) as f64 / in_size.max(1) as f64 * 100.0).round() as i64,
            },
            super::replace::CommitOutcome::DuplicateReplaced { existing, saved_bytes, final_size } => {
                VideoResult::DuplicateReplaced { existing, saved_bytes, final_size }
            }
            super::replace::CommitOutcome::DuplicateKeptExisting { existing, saved_bytes, final_size } => {
                VideoResult::DuplicateKeptExisting { existing, saved_bytes, final_size }
            }
            super::replace::CommitOutcome::DuplicateKeptBoth { existing, .. } => {
                let name = existing.file_name().unwrap_or_default().to_string_lossy().into_owned();
                VideoResult::Kept {
                    log: format!("[Worker {worker_id}] {display}: {name} is the same video (a hard link, bigger than ours); both kept"),
                    suppress_gif_pass: false,
                }
            }
            super::replace::CommitOutcome::BusyDuplicate(existing) => VideoResult::Skipped {
                log: format!(
                    "[Worker {worker_id}] Skipped {display}: {} has the same content and is being encoded at the same time; compared next time",
                    existing.file_name().unwrap_or_default().to_string_lossy()
                ),
                gone: false,
            },
            super::replace::CommitOutcome::OriginalChanged => VideoResult::Skipped {
                log: format!(
                    "[Worker {worker_id}] {display} changed while it was being encoded; left unchanged (it will be processed next time)"
                ),
                gone: false,
            },
            super::replace::CommitOutcome::NoFreeName => VideoResult::Kept {
                log: format!("[Worker {worker_id}] No free name for the new version of {display}; kept the original"),
                suppress_gif_pass: true,
            },
            super::replace::CommitOutcome::Renamed(_) => unreachable!("videos always provide a candidate"),
            super::replace::CommitOutcome::Failed(err) => VideoResult::Failed {
                log: format!("[Worker {worker_id}] Failed on {display}: {err}"),
            },
        }
    }

    /// Whether `existing` is the same video as `original`: about the same length, the same
    /// tracks, chapters and tags either way (whichever copy stays must not lose anything the
    /// other has), and VMAF at or above the quality floor (worst 5%: floor - 5). `Err` if
    /// Stop was pressed.
    pub(super) async fn same_video(&self, original: &Path, existing: &Path, duration_sec: f64) -> Result<bool, Stopped> {
        let theirs = self.probe_streams(existing).await;
        if self.job_released() {
            return Err(Stopped);
        }
        let Ok(theirs) = theirs else {
            return Ok(false);
        };
        let close = |d: f64| duration_sec > 0.0 && (d - duration_sec).abs() <= (duration_sec * 0.02).max(0.5);
        if !theirs.duration.is_some_and(close) {
            return Ok(false);
        }
        let ours = self.probe_streams(original).await;
        if self.job_released() {
            return Err(Stopped);
        }
        let Ok(ours) = ours else {
            return Ok(false);
        };
        if super::ffmpeg::streams_lost(&ours, &theirs).is_some() || super::ffmpeg::streams_lost(&theirs, &ours).is_some() {
            return Ok(false);
        }
        let floor = self.lock().options.vmaf_floor.unwrap_or(93.0);
        Ok(match self.measure_vmaf(original, existing, duration_sec, None).await? {
            Ok((vmaf, worst)) => vmaf >= floor && worst >= floor - 5.0,
            Err(_) => false,
        })
    }

    /// Waits until another encoder fits in memory (RAM and, beside other encoders, GPU
    /// memory). CPU jobs and VMAF measurements count as running work. False on Stop.
    pub(super) async fn wait_for_memory(self: &Arc<Self>, worker_id: u32, display: &str, cancel: &CancellationToken) -> bool {
        let mut announced = false;
        let step = self.worker_step(worker_id);
        loop {
            let (running, gpu_users, nvenc) = {
                let inner = self.lock();
                let encoders = inner.running_processes;
                (encoders + inner.queue.active_cpu_total(), encoders, inner.options.encoder.ends_with("_nvenc"))
            };
            let vram = if nvenc && gpu_users > 0 { batch::free_vram_mb().await } else { None };
            let (ok, memory) = self.budget.may_start_encoder(running, vram);
            if ok {
                if announced {
                    self.reset_worker(worker_id, display, &step);
                    self.log(format!("[Worker {worker_id}] Memory available again; starting {display}"));
                }
                return true;
            }
            if !announced {
                announced = true;
                self.reset_worker(worker_id, display, "waiting for memory");
                let ram_gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
                let detail = match (memory, vram) {
                    (Some(m), Some(v)) => format!(" ({:.1} GB commit, {:.1} GB RAM, {:.1} GB GPU free)", ram_gb(m.available_commit), ram_gb(m.available_physical), v as f64 / 1024.0),
                    (Some(m), None) => format!(" ({:.1} GB commit, {:.1} GB RAM free)", ram_gb(m.available_commit), ram_gb(m.available_physical)),
                    (None, Some(v)) => format!(" ({:.1} GB GPU free)", v as f64 / 1024.0),
                    (None, None) => String::new(),
                };
                self.log(format!("[Worker {worker_id}] Waiting for free memory before starting {display}{detail}"));
                self.send_state(false);
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = cancel.cancelled() => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::ffmpeg::{Attempt, AttemptStatus, BoxFuture, Unfinished, VideoRunner};
    use crate::args::{Conversions, EncodePlan, ImageMode, Mode, Options, Originals, Streams};

    #[derive(Default)]
    struct LogUi {
        logs: Mutex<Vec<String>>,
    }
    impl Ui for LogUi {
        fn log(&self, message: String) {
            self.logs.lock().unwrap().push(message);
        }
        fn state(&self, _: &StateDto) {}
    }

    type EncodeFn = Box<dyn Fn(&Path, &Path, &EncodePlan) -> Attempt + Send + Sync>;
    type VmafFn = Box<dyn Fn(&Path, &Path) -> Result<(f64, f64), String> + Send + Sync>;

    struct ScriptedRunner {
        streams: Streams,
        calls: Arc<Mutex<Vec<(Mode, u32)>>>,
        encode_fn: EncodeFn,
        vmaf_fn: VmafFn,
    }

    impl VideoRunner for ScriptedRunner {
        fn probe_streams<'a>(&'a self, _engine: &'a Engine, _path: &'a Path) -> BoxFuture<'a, Result<Streams, Unfinished>> {
            let s = self.streams.clone();
            Box::pin(async move { Ok(s) })
        }

        fn video_timing<'a>(&'a self, _engine: &'a Engine, _path: &'a Path) -> BoxFuture<'a, Result<Option<(u64, f64)>, Unfinished>> {
            Box::pin(async { Ok(Some((30, 1.0))) })
        }

        fn same_audio_track<'a>(&'a self, _engine: &'a Engine, _a: &'a Path, _b: &'a Path, _index: usize) -> BoxFuture<'a, Result<bool, Unfinished>> {
            Box::pin(async { Ok(true) })
        }

        fn encode<'a>(
            &'a self,
            _engine: &'a Arc<Engine>,
            _worker_id: u32,
            input: &'a Path,
            tmp: &'a Path,
            _display: &'a str,
            plan: &'a EncodePlan,
        ) -> BoxFuture<'a, Attempt> {
            self.calls.lock().unwrap().push((plan.mode, plan.cq_boost));
            let res = (self.encode_fn)(input, tmp, plan);
            Box::pin(async move { res })
        }

        fn measure_vmaf<'a>(
            &'a self,
            _engine: &'a Engine,
            original: &'a Path,
            encoded: &'a Path,
            _duration_sec: f64,
            _threads: Option<usize>,
        ) -> BoxFuture<'a, super::ffmpeg::VmafOutcome> {
            let res = (self.vmaf_fn)(original, encoded);
            Box::pin(async move { Ok(res) })
        }
    }

    fn run_scripted(name: &str, options: Options, streams: Streams, encode_fn: EncodeFn, vmaf_fn: VmafFn) -> (StateDto, Vec<(Mode, u32)>, Vec<u8>, String) {
        let dir = std::env::temp_dir().join(format!("mo-scripted-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let media = dir.join("media");
        std::fs::create_dir_all(&media).unwrap();
        let clip = media.join("clip.mp4");
        std::fs::write(&clip, vec![b'X'; 1000]).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let ui = Arc::new(LogUi::default());
        struct Forward(Arc<LogUi>);
        impl Ui for Forward {
            fn log(&self, m: String) {
                self.0.log(m);
            }
            fn state(&self, s: &StateDto) {
                self.0.state(s);
            }
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(ScriptedRunner {
            streams,
            calls: Arc::clone(&calls),
            encode_fn,
            vmaf_fn,
        });
        let engine = Engine::new_with_runner(Box::new(Forward(Arc::clone(&ui))), &dir.join("data"), PathBuf::from("ffmpeg.exe"), rt.handle().clone(), runner);
        rt.block_on(async {
            engine.apply_options(options.clone());
            engine.load_dir(media.clone()).await;
            engine.clone().run_batch(options).await;
        });
        let final_bytes = std::fs::read(&clip)
            .or_else(|_| std::fs::read(media.join("clip.mkv")))
            .unwrap();
        let snap = engine.snapshot();
        let logs = ui.logs.lock().unwrap().join("\n");
        let recorded = calls.lock().unwrap().clone();
        let _ = std::fs::remove_dir_all(&dir);
        (snap, recorded, final_bytes, logs)
    }

    fn video_opts() -> Options {
        Options {
            originals: Originals::Delete,
            images: ImageMode::Off,
            audio: ImageMode::Off,
            docs: false,
            convert: Conversions::NONE,
            vmaf_floor: Some(93.0),
            concurrency: 1,
            ..Options::default()
        }
    }

    fn diag_10s() -> Diagnostics {
        let mut d = Diagnostics::default();
        d.duration_sec = 10.0;
        d
    }

    #[test]
    fn nvenc_unsupported_falls_back_to_cpu_without_gpu() {
        let (snap, calls, bytes, logs) = run_scripted(
            "nvenc-fallback",
            video_opts(),
            Streams { duration: Some(10.0), ..Streams::default() },
            Box::new(|_, tmp, plan| {
                let mut diag = diag_10s();
                if plan.mode == Mode::Primary {
                    diag.nvenc_unsupported = true;
                    Attempt { status: AttemptStatus::Exited(1), diag }
                } else {
                    std::fs::write(tmp, vec![b'C'; 400]).unwrap();
                    Attempt { status: AttemptStatus::Exited(0), diag }
                }
            }),
            Box::new(|_, _| Ok((98.0, 96.0))),
        );
        assert_eq!(calls, vec![(Mode::Primary, 0), (Mode::CpuFallback, 0)], "{logs}");
        assert_eq!((snap.compressed_this_run, snap.failed_this_run), (1, 0), "{logs}");
        assert_eq!(bytes.len(), 400);
    }

    #[test]
    fn muxer_refusal_transcodes_lossy_audio_but_never_lossless_audio() {
        // Lossy audio -> retries with Mode::TranscodeAudio and succeeds.
        let (snap, calls, _, logs) = run_scripted(
            "aac-retry",
            video_opts(),
            Streams { audio: 1, audio_codecs: vec!["wmav2".into()], lossless_audio: false, duration: Some(10.0), ..Streams::default() },
            Box::new(|_, tmp, plan| {
                let diag = diag_10s();
                if plan.mode == Mode::Primary {
                    Attempt { status: AttemptStatus::Exited(1), diag }
                } else {
                    std::fs::write(tmp, vec![b'A'; 500]).unwrap();
                    Attempt { status: AttemptStatus::Exited(0), diag }
                }
            }),
            Box::new(|_, _| Ok((98.0, 96.0))),
        );
        assert_eq!(calls, vec![(Mode::Primary, 0), (Mode::TranscodeAudio, 0)], "{logs}");
        assert_eq!(snap.compressed_this_run, 1, "{logs}");

        // Lossless audio -> skips Mode::TranscodeAudio, goes straight to CpuFallback!
        let (snap, calls, _, logs) = run_scripted(
            "lossless-protect",
            video_opts(),
            Streams { audio: 1, audio_codecs: vec!["flac".into()], lossless_audio: true, duration: Some(10.0), ..Streams::default() },
            Box::new(|_, tmp, plan| {
                let diag = diag_10s();
                if plan.mode == Mode::Primary {
                    Attempt { status: AttemptStatus::Exited(1), diag }
                } else {
                    std::fs::write(tmp, vec![b'L'; 500]).unwrap();
                    Attempt { status: AttemptStatus::Exited(0), diag }
                }
            }),
            Box::new(|_, _| Ok((98.0, 96.0))),
        );
        assert_eq!(calls, vec![(Mode::Primary, 0), (Mode::CpuFallback, 0)], "{logs}");
        assert!(logs.contains("its audio is lossless, so it is not re-encoded to AAC"), "{logs}");
        assert_eq!(snap.compressed_this_run, 1, "{logs}");
    }

    #[test]
    fn oom_does_not_fall_back_to_cpu_or_mark_file_permanently_broken() {
        let (snap, calls, bytes, logs) = run_scripted(
            "oom-no-broken",
            video_opts(),
            Streams { duration: Some(10.0), ..Streams::default() },
            Box::new(|_, _, _| {
                let mut diag = diag_10s();
                diag.out_of_memory = true;
                Attempt { status: AttemptStatus::Exited(1), diag }
            }),
            Box::new(|_, _| Ok((98.0, 96.0))),
        );
        assert_eq!(calls, vec![(Mode::Primary, 0)], "OOM must not fall back to AAC/CPU: {logs}");
        assert_eq!((snap.failed_this_run, snap.history_count), (1, 0), "OOM is never remembered in history: {logs}");
        assert_eq!(bytes.len(), 1000, "original untouched");
    }

    #[test]
    fn vmaf_quality_retry_climbs_and_keeps_strongest_passing_step() {
        let (snap, calls, bytes, logs) = run_scripted(
            "vmaf-climb",
            video_opts(),
            Streams { duration: Some(10.0), ..Streams::default() },
            Box::new(|_, tmp, plan| {
                let diag = diag_10s();
                // Primary (+0) is bigger (1100 B -> ratio 1.1, predicts +4 first).
                // +4 is 700 B (marker b'4'), +8 is 500 B (marker b'8').
                match plan.cq_boost {
                    0 => std::fs::write(tmp, vec![b'0'; 1100]).unwrap(),
                    4 => std::fs::write(tmp, vec![b'4'; 700]).unwrap(),
                    8 => std::fs::write(tmp, vec![b'8'; 500]).unwrap(),
                    _ => std::fs::write(tmp, vec![b'9'; 400]).unwrap(),
                }
                Attempt { status: AttemptStatus::Exited(0), diag }
            }),
            Box::new(|_, encoded| {
                let tag = std::fs::read(encoded).unwrap()[0];
                match tag {
                    b'4' => Ok((96.5, 94.0)), // passes >= 93 + margin 1.0 -> climbs to +8
                    b'8' => Ok((91.0, 88.0)), // fails floor -> keeps +4 winner!
                    _ => Ok((85.0, 80.0)),
                }
            }),
        );
        assert_eq!(calls, vec![(Mode::Primary, 0), (Mode::Primary, 4), (Mode::Primary, 8)], "{logs}");
        assert_eq!((snap.compressed_this_run, snap.failed_this_run), (1, 0), "{logs}");
        assert_eq!(bytes, vec![b'4'; 700], "must keep the +4 winner, not +8");
        assert!(logs.contains("Quality retry for clip.mp4: +4 CQ"), "{logs}");
    }
}
