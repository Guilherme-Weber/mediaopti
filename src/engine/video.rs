//! One video job: claim, encode with fallbacks, check, and replace the original.

use super::*;

/// Counts a video job as completed when dropped (every way out of `process_one` after its
/// encode), unless disarmed.
struct Completion<'a> {
    engine: &'a Engine,
    armed: bool,
}

impl Drop for Completion<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.engine.lock().run.completed += 1;
        }
    }
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
        if !matches!(outcome, Outcome::Stopped) {
            // Done as a video: later runs only retry the GIF optimizer if it didn't finish
            // (see `Inner::place`), never the encode.
            self.add_processed(input);
        }
        Some(outcome)
    }

    /// A GIF the video path failed on is done for the GIF optimizer too (both image modes),
    /// so `Inner::place` doesn't send it there later: only GIFs whose video wasn't smaller go.
    fn remember_without_gif_pass(&self, input: &Path) {
        if args::is_gif(input) {
            let mut inner = self.lock();
            inner.history.add_image(input, crate::image::mode(true));
            inner.history.add_image(input, crate::image::mode(false));
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
            let next = inner.claim_next().map(|input| (inner.display_name(&input), input));
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

        // Size and date now, to notice if the file changes while it is being encoded
        // (still downloading, edited, synced). Also leave hard-linked files alone:
        // replacing one would split it from its other names.
        let path = input.clone();
        let (source_state, links) = self
            .blocking(move || (file_state(&path), system::hard_link_count(&path).unwrap_or(1)))
            .await;
        // Like the CPU files' work folders: the new version (and the quality retry's second
        // one) is written next to the original, so its drive needs room for about twice it.
        let size = source_state.map_or(0, |s| s.0);
        let free = system::free_space(input.parent().unwrap_or(&input));
        if free.is_some_and(|f| f < size.saturating_mul(2) + VIDEO_DISK_RESERVE) {
            {
                let mut inner = self.lock();
                inner.in_progress.remove(&input);
                inner.run.completed += 1;
                inner.run.skipped_larger += 1;
            }
            let gb = |b: u64| b as f64 / 1_073_741_824.0;
            self.log(format!(
                "[Worker {worker_id}] Skipped {display}: its drive has {:.1} GB free, about {:.1} GB is needed (it will be tried next time)",
                gb(free.unwrap_or(0)),
                gb(size.saturating_mul(2) + VIDEO_DISK_RESERVE)
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }
        if links > 1 {
            self.lock().in_progress.remove(&input);
            {
                let mut inner = self.lock();
                inner.run.completed += 1;
                inner.run.skipped_larger += 1;
            }
            self.log(format!(
                "[Worker {worker_id}] Skipped {display}: it is a hard link ({links} names for one file)"
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }

        // Skip static 1-frame GIFs and tiny spacer GIFs immediately.
        if args::is_gif(&input) {
            let path = input.clone();
            let valid = self
                .blocking(move || {
                    std::fs::File::open(&path)
                        .is_ok_and(|f| system::is_animated_and_valid_gif(std::io::BufReader::new(f)))
                })
                .await;
            if !valid {
                // The scan sends still and tiny GIFs to the images: this one changed since
                // (replaced on disk). Not remembered: the next scan places it as it is now.
                self.lock().in_progress.remove(&input);
                {
                    let mut inner = self.lock();
                    inner.run.completed += 1;
                    inner.run.skipped_larger += 1;
                }
                self.log(format!(
                    "[Worker {worker_id}] Skipped {display}: it is no longer an animated GIF (changed since the scan); it will be checked again next time"
                ));
                self.send_state(false);
                return Outcome::Skipped;
            }
        }

        // What it holds besides the picture decides the container (MKV when MP4 can't
        // hold it) and is carried over by every attempt, then checked in the result.
        let streams = if args::is_gif(&input) { args::Streams::default() } else { self.probe_streams(&input).await };
        let drop_data = self.lock().options.drop_data;
        if let Some(blocked) = streams.blocker(drop_data) {
            self.lock().in_progress.remove(&input);
            // Kept out of history when the Advanced option could let it through later.
            if blocked.permanent {
                self.remember(&input, &input).await;
            }
            {
                let mut inner = self.lock();
                inner.run.completed += 1;
                inner.run.skipped_larger += 1;
            }
            let hint = if blocked.permanent { "" } else { " (Advanced: Remove data tracks lets it be optimized without them)" };
            self.log(format!("[Worker {worker_id}] Skipped {display}: it holds {}; left as it is{hint}", blocked.why));
            self.send_state(false);
            return Outcome::Skipped;
        }
        let dropped = streams.dropped_data();
        if !dropped.is_empty() {
            self.log(format!("[Worker {worker_id}] {display}: its data tracks ({}) are left out (Advanced option)", dropped.join(", ")));
        }
        let tmp = args::temp_name_with(&input, worker_id, &args::output_ext(&input, &streams));
        self.lock().job_streams.insert(input.clone(), streams.clone());
        self.reset_worker(worker_id, &display, "encoding");
        self.log_remove(&tmp);
        self.send_state(true);
        self.log(format!("[Worker {worker_id}] Processing: {display}"));

        // Audio is copied (MKV takes what MP4 can't); re-encoding it is only the retry when
        // the muxer refuses it.
        let initial_mode = Mode::Primary;
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
            None => self.gpu_decodable(&input).await,
        };
        let mut attempt = match primary_done {
            Some((_, duration, _, _)) => {
                self.log(format!("[Worker {worker_id}] {display}: the preset encode was done before the pause; continuing with the quality retry"));
                let mut diag = Diagnostics::default();
                diag.duration_sec = duration;
                Attempt { status: AttemptStatus::Exited(0), diag }
            }
            None => self.encode(worker_id, &input, &tmp, &display, initial_mode, 0, gpu).await,
        };
        // The settings the final output was made with; the quality retry reuses them.
        let mut used_mode = primary_done.map_or(initial_mode, |p| p.2);

        // Audio-only file (e.g. a voice note in .mp4): skip and remember.
        if attempt.diag.no_video_stream {
            self.release_tracking(worker_id, &input);
            self.log_remove(&tmp);
            self.remember(&input, &input).await;
            {
                let mut inner = self.lock();
                inner.run.completed += 1;
                inner.run.skipped_larger += 1;
            }
            self.log(format!(
                "[Worker {worker_id}] Skipped audio-only file (no video stream): {display}"
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }

        // Corrupt or incomplete file: remember it so it is never retried. Out of memory also
        // prints "Invalid data found"; that file is fine.
        if attempt.diag.corrupt_input && !attempt.diag.out_of_memory {
            self.release_tracking(worker_id, &input);
            self.log_remove(&tmp);
            self.remember(&input, &input).await;
            self.remember_without_gif_pass(&input);
            {
                let mut inner = self.lock();
                inner.run.completed += 1;
                inner.run.failed += 1;
            }
            self.log(format!(
                "[Worker {worker_id}] Corrupt/unreadable file remembered in history (will not retry): {display}"
            ));
            self.send_state(true);
            return Outcome::Failed;
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
        let encoder_is_nvenc = self.lock().options.encoder.ends_with("_nvenc");
        let mut tried_cpu = false;

        // Tiny GIF/sticker or stream rejected by the NVENC hardware -> CPU x264.
        if can_retry(self, &attempt) && attempt.diag.nvenc_unsupported {
            tried_cpu = true;
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] NVENC hardware limit on {display}; falling back to CPU software encoder..."
            ));
            self.reset_worker(worker_id, &display, "encoding on the CPU");
            attempt = self
                .encode(worker_id, &input, &tmp, &display, Mode::CpuFallback, 0, None)
                .await;
            used_mode = Mode::CpuFallback;
        }

        // Audio stream copy rejected by the muxer -> transcode audio to AAC.
        if can_retry(self, &attempt)
            && !tried_cpu
            && initial_mode == Mode::Primary
            && !args::is_gif(&input)
        {
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Stream copy failed on {display}; retrying with AAC audio transcode..."
            ));
            self.reset_worker(worker_id, &display, "encoding, audio as AAC");
            attempt = self
                .encode(worker_id, &input, &tmp, &display, Mode::TranscodeAudio, 0, gpu)
                .await;
            used_mode = Mode::TranscodeAudio;
        }

        // Final safety net: CPU fallback for NVENC encoders.
        if can_retry(self, &attempt) && !tried_cpu && encoder_is_nvenc {
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Hardware pipeline failed on {display}; attempting final CPU software fallback..."
            ));
            self.reset_worker(worker_id, &display, "encoding on the CPU");
            attempt = self
                .encode(worker_id, &input, &tmp, &display, Mode::CpuFallback, 0, None)
                .await;
            used_mode = Mode::CpuFallback;
        }

        self.release_tracking(worker_id, &input);

        let stop_requested = self.lock().stop_requested;
        if matches!(attempt.status, AttemptStatus::Stopped) || stop_requested {
            self.log_remove(&tmp);
            return self.ended_early(worker_id, &input, &display);
        }

        let code = match attempt.status {
            AttemptStatus::SpawnError(err) => {
                {
                    let mut inner = self.lock();
                    inner.failed.insert(input.clone());
                    inner.run.completed += 1;
                    inner.run.failed += 1;
                }
                self.log_remove(&tmp);
                self.log(format!(
                    "[Worker {worker_id}] Could not start ffmpeg ({}): {err}",
                    self.ffmpeg_path.display()
                ));
                self.send_state(true);
                return Outcome::Fatal;
            }
            AttemptStatus::Exited(code) => code,
            AttemptStatus::Stopped => unreachable!(),
        };

        // Counted as completed when this job ends (after a quality retry too), however it ends.
        let mut completion = Completion { engine: self, armed: true };

        if code != 0 && attempt.diag.out_of_memory {
            // Not the file's fault: left out of the history, so the next run tries it again.
            {
                let mut inner = self.lock();
                inner.failed.insert(input.clone());
                inner.run.failed += 1;
            }
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Not enough memory to encode {display} (GPU or system); it will be tried again next run"
            ));
            self.send_state(true);
            return Outcome::Failed;
        }
        if code != 0 {
            self.log_remove(&tmp);
            for line in &attempt.diag.error_lines {
                self.log(line.clone());
            }
            self.lock().failed.insert(input.clone());
            self.remember(&input, &input).await; // Remember unrecoverable files so future runs skip them
            self.remember_without_gif_pass(&input);
            self.lock().run.failed += 1;
            self.log(format!(
                "[Worker {worker_id}] ffmpeg exited with code {code} for {display} (remembered in history)"
            ));
            self.send_state(true);
            return Outcome::Failed;
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
                {
                    let mut inner = self.lock();
                    inner.failed.insert(input.clone());
                    inner.run.failed += 1;
                }
                self.log_remove(&tmp);
                self.log(format!(
                    "Could not inspect processed output for {display}: {err}"
                ));
                self.send_state(true);
                return Outcome::Failed;
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
                    let r = self
                        .quality_retry(worker_id, &input, &tmp, &display, used_mode, gpu, in_size, out_size, attempt.diag.duration_sec, floor, &mut memo)
                        .await;
                    self.lock().workers.remove(&worker_id);
                    r
                }
                _ => Retry::Rejected("quality check off".into()),
            };
            match retry {
                Retry::Stopped => {
                    self.log_remove(&tmp);
                    completion.armed = false;
                    return self.ended_early_keeping(worker_id, &input, &display, Some(memo));
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
                        completion.armed = false; // counted as an image instead
                        return outcome;
                    }
                    self.remember(&input, &input).await;
                    self.lock().run.skipped_larger += 1;
                    self.send_state(false);
                    return Outcome::Skipped;
                }
            }
        }
        // A GIF's video must keep its animation: same frames, same length. Its frame timing
        // is passed through (no -fps_mode cfr for GIFs), so the two must match. Any other
        // video must keep everything else it held (audio tracks, subtitles, attachments,
        // covers, chapters) and its length.
        let lost = if args::is_gif(&input) {
            let (gif_timing, video_timing) = (self.video_timing(&input).await, self.video_timing(&tmp).await);
            super::ffmpeg::timing_mismatch(gif_timing, video_timing).map(|why| format!("doesn't match the GIF ({why})"))
        } else {
            let after = self.probe_streams(&tmp).await;
            let mut why = super::ffmpeg::streams_lost(&streams, &after);
            // PCM made FLAC must decode to exactly the original samples.
            for (i, codec) in streams.audio_codecs.iter().enumerate() {
                if why.is_none() && args::is_integer_pcm(codec) && !self.same_audio_track(&input, &tmp, i).await {
                    why = Some(format!("audio track {} (its FLAC doesn't decode to the original PCM)", i + 1));
                }
            }
            why.map(|why| format!("lost {why}"))
        };
        self.lock().job_streams.remove(&input);
        {
            if let Some(why) = lost {
                self.log_remove(&tmp);
                self.lock().failed.insert(input.clone());
                self.remember(&input, &input).await;
                self.remember_without_gif_pass(&input);
                self.lock().run.failed += 1;
                self.log(format!("[Worker {worker_id}] The new version of {display} {why}; kept the original (remembered in history)"));
                self.send_state(true);
                return Outcome::Failed;
            }
        }
        let saved_mb = (in_size - out_size) / (1024.0 * 1024.0);
        let saved_pct = ((in_size - out_size) / in_size * 100.0).round() as i64;

        let in_ext = args::ext_lower(&input);
        let tmp_ext = args::ext_lower(&tmp);
        let final_target = if in_ext == tmp_ext {
            input.clone()
        } else {
            let stem = input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = tmp
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            input.with_file_name(format!("{stem}{ext}"))
        };

        // Claimed atomically: x.mkv and x.avi running at once can't both become x.mp4. If a
        // file has that name already: the same video (VMAF, same length) keeps the smaller
        // copy; another video makes this one "x (2).mp4".
        let mut final_target = final_target;
        let claimed = final_target != input;
        let mut tries = 0;
        if claimed {
            loop {
                let t = final_target.clone();
                if self.blocking(move || system::claim_new_name(&t)).await.is_ok() {
                    break;
                }
                tries += 1;
                if tries == 1 {
                    match self.same_video(&input, &final_target, attempt.diag.duration_sec).await {
                        Err(Stopped) => {
                            self.log_remove(&tmp);
                            completion.armed = false;
                            return self.ended_early(worker_id, &input, &display);
                        }
                        // The other copy is being encoded right now: leave both, compared next time.
                        Ok(true) if self.lock().in_progress.contains(&final_target) => {
                            self.log_remove(&tmp);
                            self.lock().run.skipped_larger += 1;
                            self.log(format!(
                                "[Worker {worker_id}] Skipped {display}: {} has the same content and is being encoded at the same time; compared next time",
                                final_target.file_name().unwrap_or_default().to_string_lossy()
                            ));
                            self.send_state(false);
                            return Outcome::Skipped;
                        }
                        Ok(true) => return self.keep_smaller_video(worker_id, &input, &final_target, &tmp, &display, in_size).await,
                        Ok(false) => {}
                    }
                }
                if tries > 20 {
                    self.log_remove(&tmp);
                    self.remember(&input, &input).await;
                    self.remember_without_gif_pass(&input);
                    self.lock().run.skipped_larger += 1;
                    self.log(format!("[Worker {worker_id}] No free name for the new version of {display}; kept the original"));
                    self.send_state(false);
                    return Outcome::Skipped;
                }
                final_target = system::unused_name(&final_target);
            }
        }

        let path = input.clone();
        if self.blocking(move || file_state(&path)).await != source_state {
            if claimed {
                system::release_claim(&final_target);
            }
            self.lock().run.skipped_larger += 1;
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] {display} changed while it was being encoded; left unchanged (it will be processed next time)"
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }

        let attrs = OriginalAttributes::from_metadata(&in_meta);
        let disposal = self.lock().disposal_for(&input);
        let (tmp_c, target_c, input_c) = (tmp.clone(), final_target.clone(), input.clone());
        let replaced = self
            .blocking({
                let engine = Arc::clone(self);
                move || replace_original(&tmp_c, &target_c, &input_c, &disposal, &|p| engine.note_content_now(p, history::VIDEO_MODE))
            })
            .await;
        if let Err(err) = replaced {
            if claimed {
                system::release_claim(&final_target);
            }
            {
                let mut inner = self.lock();
                inner.failed.insert(input.clone());
                inner.run.failed += 1;
            }
            self.log(format!("[Worker {worker_id}] {display}: {err}"));
            self.send_state(true);
            return Outcome::Failed;
        }

        // By content, remember the new file: that is what will be found on disk next time.
        self.remember(&input, &final_target).await;
        {
            let mut inner = self.lock();
            if final_target != input {
                inner.history.add(&final_target);
            }
            inner.run.compressed += 1;
            inner.space_saved_mb += saved_mb;
        }

        let target_c = final_target.clone();
        let restored = self
            .blocking(move || system::restore_attributes(&target_c, &attrs))
            .await;
        if let Err(err) = restored {
            self.log(format!("Could not restore dates/attributes for {display}: {err}"));
        }

        self.log(format!(
            "[Worker {worker_id}] Saved {} MB ({saved_pct}%) from {display}",
            round1(saved_mb)
        ));
        self.send_state(false);
        Outcome::Processed
    }

    /// Duration in seconds of a media file, from ffmpeg's report.
    pub(super) async fn probe_duration(&self, path: &Path) -> Option<f64> {
        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(["-hide_banner", "-i"]).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
        system::configure_background_command(&mut cmd, true);
        let out = cmd.output().await.ok()?;
        let text = String::from_utf8_lossy(&out.stderr);
        let d = text.split("Duration: ").nth(1)?.split(',').next()?.trim();
        let mut secs = 0.0;
        for part in d.split(':') {
            secs = secs * 60.0 + part.parse::<f64>().ok()?;
        }
        Some(secs)
    }

    /// Whether `existing` is the same video as `original`: about the same length and VMAF at
    /// or above the quality floor (worst 5%: floor - 5). `Err` if Stop was pressed.
    pub(super) async fn same_video(&self, original: &Path, existing: &Path, duration_sec: f64) -> Result<bool, Stopped> {
        let theirs = self.probe_duration(existing).await;
        let close = |d: f64| duration_sec > 0.0 && (d - duration_sec).abs() <= (duration_sec * 0.02).max(0.5);
        if !theirs.is_some_and(close) {
            return Ok(false);
        }
        let floor = self.lock().options.vmaf_floor.unwrap_or(93.0);
        Ok(match self.measure_vmaf(original, existing, duration_sec, None).await? {
            Ok((vmaf, worst)) => vmaf >= floor && worst >= floor - 5.0,
            Err(_) => false,
        })
    }

    /// `existing` already holds the same video as `input`: the smaller of it and the new
    /// encode (`tmp`) stays under its name; the original and the other copy go as the
    /// replaced-originals setting says.
    pub(super) async fn keep_smaller_video(self: &Arc<Self>, worker_id: u32, input: &Path, existing: &Path, tmp: &Path, display: &str, in_size: f64) -> Outcome {
        let disposal = self.lock().disposal_for(input);
        let (input_c, existing_c, tmp_c) = (input.to_path_buf(), existing.to_path_buf(), tmp.to_path_buf());
        let engine = Arc::clone(self);
        let result = self
            .blocking(move || -> Result<(bool, f64), String> {
                let theirs = std::fs::metadata(&existing_c).map_err(|e| e.to_string())?.len();
                let ours = std::fs::metadata(&tmp_c).map_err(|e| e.to_string())?.len();
                let replaced = ours < theirs;
                if replaced {
                    let attrs = std::fs::metadata(&input_c).map(|m| OriginalAttributes::from_metadata(&m));
                    replace_original(&tmp_c, &existing_c, &existing_c, &disposal.for_sibling(&existing_c), &|p| engine.note_content_now(p, history::VIDEO_MODE))?;
                    if let Ok(a) = attrs {
                        let _ = system::restore_attributes(&existing_c, &a);
                    }
                } else {
                    let _ = system::remove_file_if_exists(&tmp_c);
                }
                system::dispose(&input_c, &disposal).map_err(|e| format!("could not remove the original: {e}"))?;
                let freed = if replaced { (theirs - ours) as f64 } else { 0.0 };
                Ok((replaced, freed))
            })
            .await;
        let name = existing.file_name().unwrap_or_default().to_string_lossy().into_owned();
        match result {
            Ok((replaced, freed)) => {
                self.remember(existing, existing).await;
                {
                    let mut inner = self.lock();
                    inner.run.compressed += 1;
                    inner.space_saved_mb += (in_size + freed) / (1024.0 * 1024.0);
                }
                self.log(if replaced {
                    format!("[Worker {worker_id}] {display}: {name} was the same video; it now holds the smaller new version and the original was removed")
                } else {
                    format!("[Worker {worker_id}] {display}: {name} is the same video and smaller; the original was removed")
                });
                self.send_state(false);
                Outcome::Processed
            }
            Err(err) => {
                self.log_remove(tmp);
                {
                    let mut inner = self.lock();
                    inner.failed.insert(input.to_path_buf());
                    inner.run.failed += 1;
                }
                self.log(format!("[Worker {worker_id}] {display}: {err}"));
                self.send_state(true);
                Outcome::Failed
            }
        }
    }

    /// A job ended by Stop, or by a Pause that gave its memory back: then the file goes
    /// back to the front of the queue and starts over on Resume.
    fn ended_early(self: &Arc<Self>, worker_id: u32, input: &Path, display: &str) -> Outcome {
        self.ended_early_keeping(worker_id, input, display, None)
    }

    /// [`Self::ended_early`] keeping what was finished (`memo`) for Resume.
    fn ended_early_keeping(self: &Arc<Self>, worker_id: u32, input: &Path, display: &str, memo: Option<vmaf::VideoResume>) -> Outcome {
        let (released, kept) = {
            let mut inner = self.lock();
            inner.workers.remove(&worker_id);
            inner.in_progress.remove(input);
            let released = inner.working && !inner.stop_requested;
            let mut kept = false;
            if released {
                inner.requeue_video(input);
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

    /// Waits until another encoder fits in memory (RAM and, beside other encoders, GPU
    /// memory). CPU jobs and VMAF measurements count as running work. False on Stop.
    pub(super) async fn wait_for_memory(&self, worker_id: u32, display: &str, cancel: &CancellationToken) -> bool {
        let mut announced = false;
        let step = self.worker_step(worker_id);
        loop {
            let (running, gpu_users, nvenc) = {
                let inner = self.lock();
                let encoders = inner.running_processes;
                (encoders + inner.images_active, encoders, inner.options.encoder.ends_with("_nvenc"))
            };
            let memory = system::memory_status();
            let vram = if nvenc && gpu_users > 0 { batch::free_vram_mb().await } else { None };
            if may_start_encoder(running, memory, vram) {
                if announced {
                    self.reset_worker(worker_id, display, &step);
                    self.log(format!("[Worker {worker_id}] Memory available again; starting {display}"));
                }
                return true;
            }
            if !announced {
                announced = true;
                let free_gb = memory.map_or(0.0, |m| m.available_commit as f64 / 1_073_741_824.0);
                let gpu = vram.map(|v| format!(", {:.1} GB on the GPU", v as f64 / 1024.0)).unwrap_or_default();
                self.log(format!(
                    "[Worker {worker_id}] Waiting for free memory before starting {display} ({free_gb:.1} GB available{gpu})"
                ));
                self.reset_worker(worker_id, display, "waiting for memory");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = cancel.cancelled() => return false,
            }
        }
    }
}
