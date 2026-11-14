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
                self.lock().in_progress.remove(&input);
                self.remember(&input, &input).await;
                {
                    let mut inner = self.lock();
                    inner.run.completed += 1;
                    inner.run.skipped_larger += 1;
                }
                self.log(format!(
                    "[Worker {worker_id}] Skipped static or tiny spacer GIF: {display}"
                ));
                self.send_state(false);
                return Outcome::Skipped;
            }
        }

        let tmp = args::temp_name(&input, worker_id);
        self.reset_worker(worker_id, &display, "encoding");
        self.log_remove(&tmp);
        self.send_state(true);
        self.log(format!("[Worker {worker_id}] Processing: {display}"));

        let initial_mode = if args::needs_immediate_audio_transcode(&input) {
            Mode::TranscodeAudio
        } else {
            Mode::Primary
        };
        let mut attempt = self
            .run_attempt(worker_id, &input, &tmp, &display, initial_mode, 0)
            .await;
        // The settings the final output was made with; the quality retry reuses them.
        let mut used_mode = initial_mode;

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

        // Corrupt or incomplete file: remember it so it is never retried.
        if attempt.diag.corrupt_input {
            self.release_tracking(worker_id, &input);
            self.log_remove(&tmp);
            self.remember(&input, &input).await;
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
                .run_attempt(worker_id, &input, &tmp, &display, Mode::CpuFallback, 0)
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
                .run_attempt(worker_id, &input, &tmp, &display, Mode::TranscodeAudio, 0)
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
                .run_attempt(worker_id, &input, &tmp, &display, Mode::CpuFallback, 0)
                .await;
            used_mode = Mode::CpuFallback;
        }

        self.release_tracking(worker_id, &input);

        let stop_requested = self.lock().stop_requested;
        if matches!(attempt.status, AttemptStatus::Stopped) || stop_requested {
            self.log_remove(&tmp);
            self.log(format!("[Worker {worker_id}] Stopped while processing {display}"));
            self.send_state(true);
            return Outcome::Stopped;
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

        if code != 0 {
            self.lock().failed.insert(input.clone());
            self.remember(&input, &input).await; // Remember unrecoverable files so future runs skip them
            self.lock().run.failed += 1;
            self.log_remove(&tmp);
            for line in &attempt.diag.error_lines {
                self.log(line.clone());
            }
            self.log(format!(
                "[Worker {worker_id}] ffmpeg exited with code {code} for {display} (remembered in history)"
            ));
            self.send_state(true);
            return Outcome::Failed;
        }

        let (in_path, tmp_path) = (input.clone(), tmp.clone());
        let metas = self
            .blocking(move || (std::fs::metadata(&in_path), std::fs::metadata(&tmp_path)))
            .await;
        let (in_meta, out_meta) = match metas {
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
        let mut out_size = out_meta.len() as f64;

        if out_size >= in_size {
            // The preset could not shrink it (typically a source that is already
            // compressed): retry stronger settings, keeping the measured quality.
            let floor = self.lock().options.vmaf_floor;
            let retry = match floor {
                Some(floor) if in_size > 0.0 => {
                    let r = self
                        .quality_retry(worker_id, &input, &tmp, &display, used_mode, in_size, out_size, attempt.diag.duration_sec, floor)
                        .await;
                    self.lock().workers.remove(&worker_id);
                    r
                }
                _ => Retry::Rejected("quality check off".into()),
            };
            match retry {
                Retry::Stopped => {
                    self.log_remove(&tmp);
                    self.log(format!("[Worker {worker_id}] Stopped while processing {display}"));
                    self.send_state(true);
                    return Outcome::Stopped;
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
                    if args::is_gif(&input) && self.lock().options.images != ImageMode::Off {
                        completion.armed = false; // counted as an image instead
                        return self.gif_as_image(worker_id, &input, &display).await;
                    }
                    self.remember(&input, &input).await;
                    self.lock().run.skipped_larger += 1;
                    self.send_state(false);
                    return Outcome::Skipped;
                }
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
                            return Outcome::Stopped;
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
                    self.remember(&input, &input).await;
                    self.lock().run.skipped_larger += 1;
                    self.log_remove(&tmp);
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
            .blocking(move || replace_original(&tmp_c, &target_c, &input_c, &disposal))
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
        let result = self
            .blocking(move || -> Result<(bool, f64), String> {
                let theirs = std::fs::metadata(&existing_c).map_err(|e| e.to_string())?.len();
                let ours = std::fs::metadata(&tmp_c).map_err(|e| e.to_string())?.len();
                let replaced = ours < theirs;
                if replaced {
                    let attrs = std::fs::metadata(&input_c).map(|m| OriginalAttributes::from_metadata(&m));
                    replace_original(&tmp_c, &existing_c, &existing_c, &disposal.for_sibling(&existing_c))?;
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

    /// Waits until another encoder fits in memory. Returns false if Stop was pressed.
    pub(super) async fn wait_for_memory(&self, worker_id: u32, display: &str, cancel: &CancellationToken) -> bool {
        let mut announced = false;
        let step = self.worker_step(worker_id);
        loop {
            let running = self.lock().running_processes;
            let memory = system::memory_status();
            if may_start_encoder(running, memory) {
                if announced {
                    self.reset_worker(worker_id, display, &step);
                    self.log(format!("[Worker {worker_id}] Memory available again; starting {display}"));
                }
                return true;
            }
            if !announced {
                announced = true;
                let free_gb = memory.map_or(0.0, |m| m.available_commit as f64 / 1_073_741_824.0);
                self.log(format!(
                    "[Worker {worker_id}] Waiting for free memory before starting {display} ({free_gb:.1} GB available)"
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
