//! Running one ffmpeg encode attempt with live progress, and finding ffmpeg.

use super::*;

pub(super) enum AttemptStatus {
    Exited(i32),
    Stopped,
    SpawnError(String),
}

pub(super) struct Attempt {
    pub(super) status: AttemptStatus,
    pub(super) diag: Diagnostics,
}

/// Time limits for the short ffmpeg probes (reading a file's listing, its first frame) and
/// the long ones (decoding a whole audio track, reading every packet).
const PROBE_LIMIT: Duration = Duration::from_secs(120);
const LONG_PROBE_LIMIT: Duration = Duration::from_secs(3600);

tokio::task_local! {
    /// The running video job's release token, taken when the job starts. A Pause cancels it
    /// and puts a new one in `Inner::release` for the jobs after Resume, so the job must keep
    /// its own to notice that it was released.
    pub(super) static JOB_TOKEN: CancellationToken;
}

/// A probe that didn't finish (out of time, ffmpeg couldn't start, Stop or Pause): it says
/// nothing about the file.
#[derive(Debug)]
pub(super) struct Unfinished;

/// Runs an ffmpeg probe. `None` when it can't start, `cancel` (the job's release token:
/// Stop or Pause) fires first, or it runs past `limit`; the process is then killed
/// (`kill_on_drop`). A `None` after a cancel is not the file's fault: callers check
/// `Engine::job_released` before concluding anything from it.
pub(super) async fn probe_output(cmd: &mut tokio::process::Command, cancel: Option<CancellationToken>, limit: Duration) -> Option<std::process::Output> {
    let cancelled = async {
        match cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        out = cmd.output() => out.ok(),
        () = cancelled => None,
        () = tokio::time::sleep(limit) => None,
    }
}

/// What a failure to start ffmpeg means. Windows refusing for lack of memory
/// (ERROR_NOT_ENOUGH_MEMORY, ERROR_COMMITMENT_LIMIT) is a memory shortage: an attempt that
/// failed out of memory, which `Engine::encode` waits out and retries. Anything else (ffmpeg
/// missing, access denied) stops the videos of the batch.
pub(super) fn spawn_failure(err: &std::io::Error, diag: &mut Diagnostics) -> AttemptStatus {
    if matches!(err.raw_os_error(), Some(8 | 1455)) {
        diag.out_of_memory = true;
        AttemptStatus::Exited(1)
    } else {
        AttemptStatus::SpawnError(err.to_string())
    }
}

/// Finds the ffmpeg binary: next to the executable (release zip), in the project's `bin/`
/// folder (tests and debug builds only), otherwise on PATH.
pub fn locate_ffmpeg() -> PathBuf {
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe_name)));
    #[cfg(any(test, debug_assertions))]
    let dev_copy = Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join(exe_name));
    #[cfg(not(any(test, debug_assertions)))]
    let dev_copy: Option<PathBuf> = None;
    beside_exe
        .into_iter()
        .chain(dev_copy)
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(exe_name))
}

impl Engine {
    /// Runs ffmpeg once, streaming its stderr into the worker's progress, with a stall
    /// watchdog and cancellation on Stop.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_attempt(
        self: &Arc<Self>,
        worker_id: u32,
        input: &Path,
        tmp: &Path,
        display: &str,
        mode: Mode,
        cq_boost: u32,
        gpu_decode: Option<(u32, u32)>,
    ) -> Attempt {
        let mut diag = Diagnostics::default();
        let (mut options, cancel) = (self.lock().options.clone(), self.release_token());
        options.cq_boost = cq_boost;
        options.gpu_decode = gpu_decode;
        options.streams = self.lock().job_streams.get(input).cloned().unwrap_or_default();
        let jobs = u64::from(options.concurrency.max(1));
        let nvenc = options.encoder.ends_with("_nvenc");
        let ffmpeg_args = args::build_ffmpeg_args(input, tmp, &options, mode);

        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        // stdin is a pipe so ffmpeg can be asked to quit ("q") instead of being killed.
        cmd.args(&ffmpeg_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        system::configure_background_command(&mut cmd, true);

        // Wait for enough free memory before adding another encoder.
        // Encoders start one at a time; show why a worker sits at 0% meanwhile.
        let permit = if let Ok(permit) = self.admission.clone().try_lock_owned() {
            permit
        } else {
            let step = self.worker_step(worker_id);
            self.reset_worker(worker_id, display, "waiting to start");
            self.send_state(false);
            let permit = tokio::select! {
                permit = self.admission.clone().lock_owned() => permit,
                _ = cancel.cancelled() => {
                    return Attempt { status: AttemptStatus::Stopped, diag };
                }
            };
            self.reset_worker(worker_id, display, &step);
            permit
        };
        // A retry (AAC / CPU fallback) may come up while the batch is paused.
        if self.is_paused() {
            let step = self.worker_step(worker_id);
            self.reset_worker(worker_id, display, "paused");
            self.send_state(false);
            if !self.wait_while_paused(&cancel).await {
                return Attempt { status: AttemptStatus::Stopped, diag };
            }
            self.reset_worker(worker_id, display, &step);
        }
        if !self.wait_for_memory(worker_id, display, &cancel).await {
            return Attempt {
                status: AttemptStatus::Stopped,
                diag,
            };
        }
        let mut pause_rx = self.paused.subscribe();

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let status = spawn_failure(&err, &mut diag);
                return Attempt { status, diag };
            }
        };
        system::assign_to_kill_on_exit_job(&child);
        // Counted until it exits, also on the way out of a panic.
        let running = super::vmaf::RunningProcess::new(self);

        // Give the new encoder time to allocate before the next one checks memory, unless
        // memory is plentiful. The hold ends early at the first progress line (buffers are
        // allocated by then) or when ffmpeg exits, so short files keep running in parallel.
        let warmup_done = CancellationToken::new();
        let vram_plentiful = !nvenc || free_vram_mb().await.is_none_or(|v| v >= MIN_FREE_VRAM_MB * jobs);
        let plentiful =
            vram_plentiful && system::memory_status().is_some_and(|m| m.available_commit >= MIN_FREE_COMMIT * jobs);
        if plentiful {
            drop(permit);
        } else {
            let done = warmup_done.clone();
            self.rt.spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(ADMISSION_WARMUP) => {}
                    _ = done.cancelled() => {}
                }
                drop(permit);
            });
        }

        let mut started = Instant::now();
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let mut buf = vec![0u8; 16 * 1024];
        let mut watchdog = tokio::time::interval(Duration::from_secs(5));
        watchdog.tick().await;
        let mut killed_for_stop = false;
        let mut killed_for_stall = false;
        let mut stdin = child.stdin.take();
        // Set once ffmpeg was asked to quit: killed only if it hasn't by then.
        let mut kill_at: Option<tokio::time::Instant> = None;
        // Pause pressed between the check above and the spawn.
        let mut suspended =
            *pause_rx.borrow_and_update() && system::set_process_suspended(&child, true);

        loop {
            tokio::select! {
                changed = pause_rx.changed() => {
                    if changed.is_err() {
                        continue; // the engine owns the sender; cannot happen
                    }
                    let pause = *pause_rx.borrow_and_update();
                    if kill_at.is_some() {
                        continue; // quitting: stays running until it has
                    }
                    if pause && !suspended {
                        suspended = system::set_process_suspended(&child, true);
                    } else if !pause && suspended {
                        system::set_process_suspended(&child, false);
                        suspended = false;
                        // Time spent paused must not count as a stall.
                        started = Instant::now();
                        if let Some(w) = self.lock().workers.get_mut(&worker_id) {
                            w.last_progress_at = Instant::now();
                        }
                        watchdog.reset();
                    }
                }
                read = stderr.read(&mut buf) => {
                    let n = match read {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    let (saw_progress, encoding) = {
                        let mut inner = self.lock();
                        let progress = inner
                            .workers
                            .entry(worker_id)
                            .or_insert_with(|| WorkerProgress::new(display));
                        let saw = progress::parse_chunk(progress, &mut diag, worker_id, display, &chunk);
                        (saw, progress.last_encoded_sec > 0.0)
                    };
                    if saw_progress {
                        // Only once frames are actually coming out (time= advancing) are the
                        // encoder's buffers and lookahead fully allocated.
                        if encoding {
                            warmup_done.cancel();
                        }
                        self.send_state(false);
                    }
                }
                _ = watchdog.tick(), if !killed_for_stall && !suspended => {
                    let (idle, percent) = {
                        let inner = self.lock();
                        inner.workers.get(&worker_id).map_or(
                            (started.elapsed(), 0.0),
                            |w| (w.last_progress_at.elapsed(), w.percent),
                        )
                    };
                    // At >=99% ffmpeg may be rewriting the MP4 moov atom (+faststart) on a large
                    // HDD output, where no new frame progress prints for a while.
                    let limit = if percent == 0.0 { 30 } else if percent >= 99.0 { 180 } else { 60 };
                    if idle > Duration::from_secs(limit) {
                        self.log(format!(
                            "[Worker {worker_id}] Stall watchdog triggered on {display} ({}s without frame progress). Terminating process.",
                            idle.as_secs()
                        ));
                        killed_for_stall = true;
                        kill_at = Some(ask_to_quit(&child, &mut stdin, &mut suspended).await);
                    }
                }
                _ = cancel.cancelled(), if !killed_for_stop => {
                    killed_for_stop = true;
                    if kill_at.is_none() {
                        kill_at = Some(ask_to_quit(&child, &mut stdin, &mut suspended).await);
                    }
                }
                _ = tokio::time::sleep_until(kill_at.unwrap_or_else(tokio::time::Instant::now)), if kill_at.is_some() => {
                    self.log(format!("[Worker {worker_id}] ffmpeg did not quit on {display}; ending it"));
                    let _ = child.start_kill();
                    kill_at = None;
                }
            }
        }

        warmup_done.cancel();
        let exit = child.wait().await;
        drop(running);

        let status = if killed_for_stop || self.lock().stop_requested {
            AttemptStatus::Stopped
        } else {
            match exit {
                // Asked to quit, ffmpeg ends cleanly (exit 0) with a valid but cut-off
                // output: never a result.
                Ok(s) if killed_for_stall => AttemptStatus::Exited(s.code().filter(|&c| c != 0).unwrap_or(1)),
                Ok(s) => AttemptStatus::Exited(s.code().unwrap_or(1)),
                Err(err) => AttemptStatus::SpawnError(err.to_string()),
            }
        };
        diag.duration_sec = self.lock().workers.get(&worker_id).map_or(0.0, |w| w.duration_sec);
        Attempt { status, diag }
    }

    /// One encode with the retries that keep the result the same: out of memory waits for
    /// another encoder to finish and tries again (not a reason for the CPU fallback), and
    /// a failed GPU decode is redone with the CPU decoding.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn encode(
        self: &Arc<Self>,
        worker_id: u32,
        input: &Path,
        tmp: &Path,
        display: &str,
        mode: Mode,
        cq_boost: u32,
        gpu_decode: Option<(u32, u32)>,
    ) -> Attempt {
        const MEMORY_RETRIES: u32 = 5;
        let mut gpu = gpu_decode.filter(|_| mode != Mode::CpuFallback);
        let mut memory_retries = 0;
        loop {
            let mut attempt = self.run_attempt(worker_id, input, tmp, display, mode, cq_boost, gpu).await;
            if attempt.diag.out_of_memory && matches!(attempt.status, AttemptStatus::Exited(0)) {
                // Frames were dropped for lack of memory: never a result to keep.
                self.log(format!("[Worker {worker_id}] {display}: ffmpeg ran out of memory mid-encode; discarding its output"));
                attempt.status = AttemptStatus::Exited(1);
            }
            let (stopping, others) = {
                let inner = self.lock();
                (inner.stop_requested || !inner.working, inner.running_processes + inner.images_active)
            };
            // The frame size changed mid-stream: scale_cuda's scaling differs a little from
            // the CPU's, so such a file is redone the way it always was.
            if gpu.is_some() && attempt.diag.reconfigured && matches!(attempt.status, AttemptStatus::Exited(0)) && !stopping {
                gpu = None;
                self.log_remove(tmp);
                self.log(format!("[Worker {worker_id}] {display} changes frame size mid-stream; encoding again with CPU decoding"));
                continue;
            }
            if !matches!(attempt.status, AttemptStatus::Exited(code) if code != 0) || stopping || attempt.diag.no_video_stream {
                return attempt;
            }
            if attempt.diag.out_of_memory && others > 0 && memory_retries < MEMORY_RETRIES {
                memory_retries += 1;
                self.log_remove(tmp);
                self.log(format!(
                    "[Worker {worker_id}] Out of memory starting {display} next to {others} other job(s); retrying when one finishes"
                ));
                if !self.wait_for_fewer_encoders(worker_id, display, others).await {
                    return Attempt { status: AttemptStatus::Stopped, diag: attempt.diag };
                }
                continue;
            }
            if gpu.is_some() {
                gpu = None;
                self.log_remove(tmp);
                self.log(format!("[Worker {worker_id}] GPU decoding failed on {display}; decoding on the CPU instead"));
                continue;
            }
            return attempt;
        }
    }

    /// Waits until fewer than `others` jobs (ffmpeg processes and CPU files) run, or 2
    /// minutes pass. False on Stop.
    async fn wait_for_fewer_encoders(self: &Arc<Self>, worker_id: u32, display: &str, others: usize) -> bool {
        let step = self.worker_step(worker_id);
        self.reset_worker(worker_id, display, "waiting for memory");
        self.send_state(false);
        let cancel = self.release_token();
        let until = Instant::now() + Duration::from_secs(120);
        let busy = || {
            let inner = self.lock();
            inner.running_processes + inner.images_active
        };
        while busy() >= others && Instant::now() < until {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = cancel.cancelled() => return false,
            }
        }
        self.reset_worker(worker_id, display, &step);
        true
    }

    /// Whether `input` may be decoded on the GPU, using the `gpu_size` already parsed by
    /// [`Self::probe_streams`] (no second `ffmpeg -i` spawn): a 10-bit NVENC encoder
    /// (AV1/HEVC) and a first video stream in H.264, HEVC, AV1 or VP9 at 4:2:0, not rotated.
    /// For these the frames NVDEC + scale_cuda hand the encoder were checked to be
    /// bit-identical to the CPU path; MPEG-4 Part 2 decodes differently and 4:2:2 is
    /// converted differently, so they stay on the CPU.
    pub(super) fn gpu_decode_for(&self, streams: &args::Streams, input: &Path) -> Option<(u32, u32)> {
        let (encoder, allowed) = {
            let inner = self.lock();
            (inner.options.encoder.clone(), inner.options.gpu_decoding)
        };
        // Off in Advanced options, or MEDIAOPTI_GPU_DECODE=0 (comparisons, a driver that misbehaves).
        let off = !allowed || std::env::var_os("MEDIAOPTI_GPU_DECODE").is_some_and(|v| v == "0");
        if off || !matches!(encoder.as_str(), "av1_nvenc" | "hevc_nvenc") || args::is_animated_picture(input) {
            return None;
        }
        streams.gpu_size
    }

    /// The token that releases the running job: its own inside a video job (`JOB_TOKEN`),
    /// else the current one.
    pub(super) fn release_token(&self) -> CancellationToken {
        JOB_TOKEN.try_with(CancellationToken::clone).unwrap_or_else(|_| self.lock().release.clone())
    }

    /// The running job's release token (Stop, or a Pause that ends the processes), if a batch
    /// runs.
    pub(super) fn job_token(&self) -> Option<CancellationToken> {
        if let Ok(token) = JOB_TOKEN.try_with(CancellationToken::clone) {
            return Some(token);
        }
        let inner = self.lock();
        inner.working.then(|| inner.release.clone())
    }

    /// Whether the running job was released (Stop, or a Pause that ends the processes): an
    /// unfinished probe then says nothing about the file.
    pub(super) fn job_released(&self) -> bool {
        let own = JOB_TOKEN.try_with(CancellationToken::is_cancelled);
        let inner = self.lock();
        own.unwrap_or_else(|_| inner.release.is_cancelled()) || inner.stop_requested
    }

    /// What `path` holds besides its picture (audio, subtitles, attachments, covers,
    /// chapters) and its length, from `ffmpeg -i`. `Unfinished` when a probe didn't finish:
    /// never read as "nothing there" (an encode would drop the tracks it didn't know about).
    pub(super) async fn probe_streams(&self, path: &Path) -> Result<args::Streams, Unfinished> {
        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(["-hide_banner", "-i"]).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
        system::configure_background_command(&mut cmd, true);
        if system::probe_fails_in_tests(path) {
            return Err(Unfinished);
        }
        let out = probe_output(&mut cmd, self.job_token(), PROBE_LIMIT).await.ok_or(Unfinished)?;
        let listing = String::from_utf8_lossy(&out.stderr);
        if !listing_opened(&listing) {
            // Busy (another program had it open), no access, gone: an empty listing would
            // read as "no tracks" and the encode would drop them.
            return Err(Unfinished);
        }
        let mut streams = args::Streams::parse(&listing);
        streams.gpu_size = gpu_decodable_stream(&listing);
        if streams.hdr {
            // HDR10's static metadata travels with the frames: read from the first one.
            let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
            cmd.args(["-hide_banner", "-i"])
                .arg(path)
                .args(["-map", "0:V:0", "-frames:v", "1", "-vf", "showinfo", "-f", "null", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            system::configure_background_command(&mut cmd, true);
            let out = probe_output(&mut cmd, self.job_token(), PROBE_LIMIT).await.ok_or(Unfinished)?;
            let text = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
            if !first_frame_read(out.status.success(), &text) {
                // The first frame wasn't decoded: "no HDR10+" would let its dynamic metadata
                // be lost, "no mastering data" would let that be lost unchecked.
                return Err(Unfinished);
            }
            streams.mastering = text.contains("mastering display");
            streams.light_level = text.contains("content light level");
            streams.hdr10_plus = text.contains("hdr10+") || text.contains("2094-40");
        }
        Ok(streams)
    }

    /// Whether audio track `index` of `a` and `b` decodes to the same samples (compared as
    /// 32-bit PCM, so 16- and 24-bit sources and their FLAC compare alike). `Unfinished` when
    /// a decode didn't finish; a failed decode is "not the same".
    pub(super) async fn same_audio_track(&self, a: &Path, b: &Path, index: usize) -> Result<bool, Unfinished> {
        let md5 = |p: &Path| {
            let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
            cmd.args(["-hide_banner", "-v", "error", "-i"])
                .arg(p)
                .args(["-map", &format!("0:a:{index}"), "-c:a", "pcm_s32le", "-f", "md5", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            system::configure_background_command(&mut cmd, true);
            let token = self.job_token();
            async move {
                let out = probe_output(&mut cmd, token, LONG_PROBE_LIMIT).await.ok_or(Unfinished)?;
                Ok(out.status.success().then_some(out.stdout))
            }
        };
        let (x, y) = (md5(a).await?, md5(b).await?);
        Ok(x.is_some() && x == y)
    }

    /// Frame count and length in seconds of the first video stream, read from its packets
    /// (no decoding, so fast even for big files). A GIF's frames are counted by decoding: its
    /// demuxer hands extension blocks after the last picture (Photoshop's XMP) over as one
    /// more packet, which counted as a frame the video "lost" (11 false failures on
    /// 2026-11-19). `Ok(None)` when it can't be read, `Unfinished` when the read didn't finish.
    pub(super) async fn video_timing(&self, path: &Path) -> Result<Option<(u64, f64)>, Unfinished> {
        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(["-hide_banner", "-v", "error", "-i"]).arg(path).args(["-map", "0:v:0"]);
        if args::is_animated_picture(path) {
            // Each decoded frame keeps the GIF/WebP's own delay; wrapped_avframe skips any encoding.
            cmd.args(["-fps_mode", "passthrough", "-enc_time_base", "demux", "-c:v", "wrapped_avframe", "-f", "framecrc", "-"]);
        } else {
            cmd.args(["-c", "copy", "-f", "framemd5", "-"]);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        system::configure_background_command(&mut cmd, true);
        let out = probe_output(&mut cmd, self.job_token(), LONG_PROBE_LIMIT).await.ok_or(Unfinished)?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(packet_timing(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// Whether `ffmpeg -i` opened the input: its listing has "Input #0". A file it says is
/// damaged counts as opened (the encode then reports it as corrupt, as before).
pub(super) fn listing_opened(listing: &str) -> bool {
    listing.contains("Input #0") || crate::progress::says_corrupt(listing)
}

/// Whether the showinfo probe decoded the first frame (`text` in lower case): a frame's
/// line (`n: 0 pts: ... pts_time:`), not only the filter's "config in" line.
pub(super) fn first_frame_read(exited_ok: bool, text: &str) -> bool {
    exited_ok && text.lines().any(|l| l.contains("parsed_showinfo") && l.contains("pts_time:"))
}

/// Frame count and length from ffmpeg's framemd5 listing of packets: `#tb 0: 1/100`, then
/// `stream, dts, pts, duration, size, hash` per packet.
pub(super) fn packet_timing(listing: &str) -> Option<(u64, f64)> {
    let mut tb = None;
    let (mut count, mut start, mut end) = (0u64, i64::MAX, i64::MIN);
    for line in listing.lines() {
        if let Some(rest) = line.strip_prefix("#tb 0:") {
            let (n, d) = rest.trim().split_once('/')?;
            tb = Some(n.trim().parse::<f64>().ok()? / d.trim().parse::<f64>().ok()?);
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() < 5 || f[0] != "0" {
            continue;
        }
        let (pts, duration) = (f[2].parse::<i64>().ok()?, f[3].parse::<i64>().ok()?);
        count += 1;
        start = start.min(pts);
        end = end.max(pts + duration);
    }
    (count > 0).then(|| (count, (end - start) as f64 * tb.unwrap_or(0.0)))
}

/// What a new version of a video lost compared with the original, if anything: audio
/// tracks, subtitles, attachments, cover pictures, chapters, or more than a second of its
/// length (1% when the original's container only estimates it: TS, AVI...). Only shorter
/// counts: old containers often declare a wrong length, and the new one states the real one.
pub(super) fn streams_lost(before: &args::Streams, after: &args::Streams) -> Option<String> {
    let mut lost = Vec::new();
    for (what, b, a) in [
        ("audio tracks", before.audio, after.audio),
        ("extra video tracks", before.extra_videos, after.extra_videos),
        ("subtitles", before.subtitles.len(), after.subtitles.len()),
        ("attachments", before.attachments, after.attachments),
        ("cover pictures", before.covers.len(), after.covers.len()),
        ("chapters", before.chapters, after.chapters),
    ] {
        if a < b {
            lost.push(format!("{} of {b} {what}", b - a));
        }
    }
    let tmcd = |s: &args::Streams| s.data.iter().filter(|t| *t == "tmcd").count();
    // MKV has no timecode tracks: the muxer keeps the value as a TIMECODE tag instead.
    let timecode_kept = before.timecode.is_some() && after.timecode == before.timecode;
    if tmcd(after) < tmcd(before) && !timecode_kept {
        lost.push("its timecode track".into());
    }
    for (what, b, a) in [
        ("its HDR transfer", before.hdr, after.hdr),
        ("its HDR10 mastering display metadata", before.mastering, after.mastering),
        ("its HDR10 content light level", before.light_level, after.light_level),
    ] {
        if b && !a {
            lost.push(what.into());
        }
    }
    let missing: Vec<&str> = before
        .tags
        .iter()
        .filter(|(k, v)| {
            // MKV keeps the date to the second, MP4 with fractions: compared to the second.
            let day = |t: &str| t.split(';').next().unwrap_or_default().chars().take(19).collect::<String>();
            let same_value = |av: &str| if k == "creation_time" { day(av) == day(v) } else { av == v };
            // Key and value together: "a b" and "a_b" may both be there with other values.
            !after.tags.iter().any(|(ak, av)| same_tag_key(ak, k) && same_value(av))
        })
        .map(|(k, _)| k.as_str())
        .collect();
    if !missing.is_empty() {
        lost.push(format!("its tags ({})", missing.join(", ")));
    }
    // A second of slack (audio priming, the last frame's duration); 1% when the original's
    // length was only its container's estimate.
    let slack = |b: f64| if before.duration_is_estimate { (b * 0.01).max(1.0) } else { 1.0 };
    match (before.duration, after.duration) {
        (Some(b), Some(a)) if b - a > slack(b) => lost.push(format!("its length ({b:.1} s -> {a:.1} s)")),
        (Some(_), None) => lost.push("its length (unreadable)".into()),
        _ => {}
    }
    (!lost.is_empty()).then(|| lost.join(", "))
}

/// Tag keys as the same tag: the Matroska muxer writes a key's spaces as underscores (a WMV's
/// "VBR Peak" comes back as `VBR_PEAK`; keys are already in lower case).
fn same_tag_key(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(x, y)| x == y || matches!((x, y), (b' ', b'_') | (b'_', b' ')))
}

/// Why a GIF's video doesn't keep its animation, if it doesn't: another number of frames, or
/// a length off by more than half a frame (at most 50 ms). Unreadable counts as a mismatch.
pub(super) fn timing_mismatch(gif: Option<(u64, f64)>, video: Option<(u64, f64)>) -> Option<String> {
    let ((gf, gs), (vf, vs)) = match (gif, video) {
        (Some(g), Some(v)) => (g, v),
        (None, _) => return Some("the GIF's frames could not be read".into()),
        (_, None) => return Some("the video's frames could not be read".into()),
    };
    let tolerance = (gs / gf.max(1) as f64 / 2.0).min(0.05);
    if gf != vf || (gs - vs).abs() > tolerance {
        Some(format!("GIF {gf} frames, {gs:.2} s; video {vf} frames, {vs:.2} s"))
    } else {
        None
    }
}

/// How long ffmpeg gets to quit after "q" before it is killed (it took ~1.6 s for a 4K
/// encode).
const QUIT_GRACE: Duration = Duration::from_secs(10);

/// Asks ffmpeg to stop by sending "q" (resumed first if suspended): it ends the encoder
/// properly. Killing an NVENC encode outright can make the NVIDIA driver reset the GPU
/// (nvlddmkm event 153 within a few kills; once it left the card disabled, code 43). Returns
/// when to kill it if it hasn't quit.
async fn ask_to_quit(child: &tokio::process::Child, stdin: &mut Option<tokio::process::ChildStdin>, suspended: &mut bool) -> tokio::time::Instant {
    use tokio::io::AsyncWriteExt;
    if *suspended {
        system::set_process_suspended(child, false);
        *suspended = false;
    }
    if let Some(s) = stdin.as_mut() {
        let _ = s.write_all(b"q").await;
        let _ = s.flush().await;
    }
    tokio::time::Instant::now() + QUIT_GRACE
}

/// See [`Engine::gpu_decodable`]: reads the first video stream (not cover art) of ffmpeg's
/// report. Rotated video stays on the CPU (autorotate inserts a CPU filter).
pub(super) fn gpu_decodable_stream(report: &str) -> Option<(u32, u32)> {
    let mut lines = report.lines();
    let line = lines.find(|l| l.contains("Video: ") && !l.contains("(attached pic)"))?;
    // The stream's side data (rotation) follows on the next lines, up to the next stream.
    if lines.take_while(|l| !l.trim_start().starts_with("Stream #")).any(|l| l.contains("rotation")) {
        return None;
    }
    let desc = line.split("Video: ").nth(1)?;
    let codec = desc.split([' ', ',']).next()?;
    // The pixel format is the first field after the codec's own, then the size, e.g.
    // "h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709, progressive), 1920x1080 [SAR...".
    let pix_fmt = desc.split(", ").nth(1).map_or("", |f| f.split('(').next().unwrap_or_default().trim());
    if !matches!(codec, "h264" | "hevc" | "av1" | "vp9") || !matches!(pix_fmt, "yuv420p" | "yuv420p10le") {
        return None;
    }
    let size = desc.split(", ").find_map(|f| {
        let (w, h) = f.split(' ').next()?.split_once('x')?;
        Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
    })?;
    let even = |v: u32| (v / 2 * 2).max(2);
    Some((even(size.0), even(size.1)))
}

#[cfg(test)]
mod tests {
    use super::gpu_decodable_stream as ok;
    use super::{packet_timing, timing_mismatch};

    /// A Pause swaps the release token for the jobs after Resume; a job running when it came
    /// still sees that it was released (it used to look at the new token and go on).
    #[test]
    fn a_pause_is_seen_by_the_job_it_released() {
        struct Quiet;
        impl super::Ui for Quiet {
            fn log(&self, _: String) {}
            fn state(&self, _: &super::StateDto) {}
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let data = std::env::temp_dir().join(format!("mo-job-token-{}", std::process::id()));
        let engine = super::Engine::new(Box::new(Quiet), &data, std::path::PathBuf::from("ffmpeg.exe"), rt.handle().clone());
        let token = {
            let mut inner = engine.lock();
            inner.working = true;
            inner.status = super::Status::Running;
            inner.release.clone()
        };
        rt.block_on(super::JOB_TOKEN.scope(token, async {
            assert!(!engine.job_released());
            engine.set_paused(true);
            assert!(engine.job_released(), "the job's own token was cancelled by the Pause");
            assert!(engine.job_token().is_some_and(|t| t.is_cancelled()), "its probes end too");
        }));
        assert!(!engine.job_released(), "jobs started after Resume use the new token");
        engine.lock().working = false;
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A probe that never ends (a pathological file) gives up at its time limit, and Stop or
    /// Pause ends it at once; the process is killed either way.
    #[test]
    fn probes_end_at_their_limit_and_on_stop() {
        use std::process::Stdio;
        use std::time::Duration;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let endless = || {
                let mut cmd = tokio::process::Command::new("ping");
                cmd.args(["-n", "60", "127.0.0.1"]).stdout(Stdio::null()).kill_on_drop(true);
                cmd
            };
            let t = std::time::Instant::now();
            assert!(super::probe_output(&mut endless(), None, Duration::from_secs(1)).await.is_none());
            assert!(t.elapsed() < Duration::from_secs(10), "time limit");
            let stop = tokio_util::sync::CancellationToken::new();
            let later = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                later.cancel();
            });
            let t = std::time::Instant::now();
            assert!(super::probe_output(&mut endless(), Some(stop), Duration::from_secs(600)).await.is_none());
            assert!(t.elapsed() < Duration::from_secs(10), "Stop");
            let mut quick = tokio::process::Command::new("cmd");
            quick.args(["/c", "echo", "hi"]).kill_on_drop(true);
            let out = super::probe_output(&mut quick, None, Duration::from_secs(60)).await.unwrap();
            assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
        });
    }

    /// Windows refusing to start ffmpeg for lack of memory (commit) is a memory shortage,
    /// waited out like one; it used to end every video of the batch.
    #[test]
    fn starting_without_memory_is_a_memory_shortage() {
        for code in [8, 1455] {
            let mut diag = crate::progress::Diagnostics::default();
            let status = super::spawn_failure(&std::io::Error::from_raw_os_error(code), &mut diag);
            assert!(matches!(status, super::AttemptStatus::Exited(1)) && diag.out_of_memory, "os error {code}");
        }
        let mut diag = crate::progress::Diagnostics::default();
        let status = super::spawn_failure(&std::io::Error::from_raw_os_error(2), &mut diag);
        assert!(matches!(status, super::AttemptStatus::SpawnError(_)) && !diag.out_of_memory, "a missing ffmpeg is fatal");
    }

    #[test]
    fn lost_streams_are_named() {
        use crate::args::Streams;
        let before = Streams { audio: 2, subtitles: vec!["subrip".into()], attachments: 1, covers: vec![4], chapters: 3, duration: Some(600.0), ..Streams::default() };
        assert_eq!(super::streams_lost(&before, &before), None);
        let longer = Streams { audio: 3, duration: Some(900.0), ..before.clone() };
        assert_eq!(super::streams_lost(&before, &longer), None, "more is fine");
        let close = Streams { duration: Some(599.5), ..before.clone() };
        assert_eq!(super::streams_lost(&before, &close), None, "within a second");
        // A two-hour film cut 70 s short: lost, unless the original's length was only an
        // estimate (TS, AVI... headers), where 1% is allowed.
        let film = Streams { duration: Some(7200.0), ..before.clone() };
        let cut = Streams { duration: Some(7130.0), ..before.clone() };
        assert!(super::streams_lost(&film, &cut).unwrap().contains("7200.0 s -> 7130.0 s"));
        let ts = Streams { duration_is_estimate: true, ..film };
        assert_eq!(super::streams_lost(&ts, &cut), None, "within 1% of an estimate");
        let after = Streams { audio: 1, subtitles: vec![], chapters: 0, duration: Some(500.0), ..before.clone() };
        let why = super::streams_lost(&before, &after).unwrap();
        assert!(why.contains("1 of 2 audio tracks") && why.contains("1 of 1 subtitles") && why.contains("3 of 3 chapters") && why.contains("600.0 s -> 500.0 s"), "{why}");
        assert!(super::streams_lost(&before, &Streams::default()).unwrap().contains("unreadable"));
        let tagged = Streams {
            tags: vec![("creation_time".into(), "2024-05-01T10:20:30.000000Z;2024-05-01T10:20:30Z".into()), ("com.apple.quicktime.make".into(), "Apple".into())],
            ..before.clone()
        };
        let mkv = Streams {
            tags: vec![("com.apple.quicktime.make".into(), "Apple".into()), ("creation_time".into(), "2024-05-01T10:20:30.000000Z".into())],
            ..before.clone()
        };
        assert_eq!(super::streams_lost(&tagged, &mkv), None, "same tags, date to the second");
        let why = super::streams_lost(&tagged, &before).unwrap();
        assert!(why.contains("its tags (creation_time, com.apple.quicktime.make)"), "{why}");
        // A WMV's "VBR Peak" comes back from the Matroska muxer as VBR_PEAK (PART3.wmv).
        let wmv = Streams { tags: vec![("vbr peak".into(), "4871679".into()), ("toolname".into(), "WMEU".into())], ..before.clone() };
        let mkv = Streams { tags: vec![("toolname".into(), "WMEU".into()), ("vbr_peak".into(), "4871679".into())], ..before.clone() };
        assert_eq!(super::streams_lost(&wmv, &mkv), None, "space written as underscore");
        let changed = Streams { tags: vec![("toolname".into(), "WMEU".into()), ("vbr_peak".into(), "1".into())], ..before.clone() };
        assert!(super::streams_lost(&wmv, &changed).unwrap().contains("its tags (vbr peak)"));
        assert!(!super::same_tag_key("vbr peak", "vbrpeak"));
        // A timecode track that became an MKV tag with the same value is kept; another value
        // or none is lost.
        let mov = Streams { data: vec!["tmcd".into()], timecode: Some("01:00:00:00".into()), ..before.clone() };
        let mkv = Streams { timecode: Some("01:00:00:00".into()), ..before.clone() };
        assert_eq!(super::streams_lost(&mov, &mkv), None);
        assert!(super::streams_lost(&mov, &before).unwrap().contains("timecode track"));
        let other = Streams { timecode: Some("02:00:00:00".into()), ..before.clone() };
        assert!(super::streams_lost(&mov, &other).unwrap().contains("timecode track"));
        // Both spellings present with different values: each finds its own.
        let both = Streams { tags: vec![("foo bar".into(), "A".into()), ("foo_bar".into(), "B".into())], ..before };
        assert_eq!(super::streams_lost(&both, &both), None);
    }

    /// A probe that couldn't open the file (another program held it) read as "no tracks":
    /// the encode would have dropped covers and tracks unchecked.
    #[test]
    fn probes_that_did_not_read_the_file_say_nothing() {
        let locked = "[in#0 @ 000002bba4911b40] Error opening input: Permission denied\nError opening input file C:\\v\\a.mp4.\nError opening input files: Permission denied\n";
        assert!(!super::listing_opened(locked));
        assert!(!super::listing_opened(""));
        assert!(super::listing_opened("Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'a.mp4':\n  Duration: 00:00:01.00\n"));
        // Damaged files go on to the encode, which reports them as corrupt.
        assert!(super::listing_opened("[mov,mp4,m4a,3gp,3g2,mj2 @ 0] moov atom not found\nError opening input files: Invalid data found when processing input\n"));
        let config = "[parsed_showinfo_0 @ 0] config in time_base: 1/12800, frame_rate: 25/1\n";
        let frame = "[parsed_showinfo_0 @ 0] n:   0 pts:      0 pts_time:0       duration:    512\n";
        assert!(super::first_frame_read(true, &format!("{config}{frame}")));
        assert!(!super::first_frame_read(true, config), "no frame decoded");
        assert!(!super::first_frame_read(false, &format!("{config}{frame}")));
    }

    #[test]
    fn gif_and_video_timing_are_compared() {
        // A GIF (1/100 s) of 3 frames: 10, 10 and 50 hundredths.
        let gif = "#format: frame checksums\n#tb 0: 1/100\n#stream#, dts, pts, duration, size, hash\n\
                   0, 0, 0, 10, 100, aa\n0, 10, 10, 10, 90, bb\n0, 20, 20, 50, 80, cc\n";
        let close = |t: Option<(u64, f64)>| t.is_some_and(|(n, s)| n == 3 && (s - 0.7).abs() < 1e-9);
        assert!(close(packet_timing(gif)), "{:?}", packet_timing(gif));
        // The same timing in an MP4 (1/12800), B-frames out of order.
        let mp4 = "#tb 0: 1/12800\n0, -2560, 0, 1280, 50, a\n0, -1280, 2560, 6400, 40, b\n0, 0, 1280, 1280, 30, c\n";
        assert!(close(packet_timing(mp4)), "{:?}", packet_timing(mp4));
        assert_eq!(timing_mismatch(Some((3, 0.7)), Some((3, 0.7))), None);
        assert_eq!(timing_mismatch(Some((3, 0.7)), Some((3, 0.72))), None, "within half a frame");
        assert!(timing_mismatch(Some((3, 0.7)), Some((2, 0.7))).is_some(), "a frame lost");
        assert!(timing_mismatch(Some((3, 0.7)), Some((3, 1.2))).is_some(), "timing changed");
        assert!(timing_mismatch(Some((3, 0.7)), None).is_some(), "unreadable video");
        assert_eq!(packet_timing("#tb 0: 1/100\n"), None);
    }

    #[test]
    fn gpu_decoding_only_for_checked_streams() {
        let s = |v: &str| format!("Input #0, mov,mp4\n  Stream #0:0[0x1](und): Video: {v}\n");
        assert_eq!(ok(&s("h264 (High) (avc1 / 0x31637661), yuv420p(tv, smpte170m, progressive), 1920x1080 [SAR 1:1 DAR 16:9], 60 fps")), Some((1920, 1080)));
        assert_eq!(ok(&s("av1 (libaom-av1) (Main), yuv420p10le(tv, bt709/unknown/unknown, progressive), 2560x1920, 59.94 fps")), Some((2560, 1920)));
        assert_eq!(ok(&s("vp9 (Profile 0), yuv420p(tv), 641x359")), Some((640, 358)), "even, like the CPU path");
        assert!(ok(&s("hevc (Main 10), yuv420p10le(tv), 3840x2160")).is_some());
        // A phone video shot upright: rotated by a display matrix.
        let rotated = "  Stream #0:0[0x1](und): Video: h264 (High), yuv420p(tv, bt709), 1920x1080, 30 fps\n      Metadata:\n        handler_name    : VideoHandle\n      Side data:\n        Display Matrix: rotation of -90.00 degrees\n  Stream #0:1[0x2](und): Audio: aac\n";
        assert!(ok(rotated).is_none());
        let audio_rotation = "  Stream #0:0: Video: h264 (High), yuv420p(tv), 640x360\n  Stream #0:1: Audio: aac\n      Side data:\n        Display Matrix: rotation of 90.00 degrees\n";
        assert!(ok(audio_rotation).is_some(), "only the video stream's own side data counts");
        // Full-range 4:2:0, 4:2:2, 4:4:4, MPEG-4 Part 2, WMV: CPU.
        assert!(ok(&s("h264 (High), yuvj420p(pc, bt470bg/bt470bg/smpte170m), 1080x1920")).is_none());
        assert!(ok(&s("h264 (High 4:2:2), yuv422p(progressive), 640x360")).is_none());
        assert!(ok(&s("h264 (High 4:4:4 Predictive), yuv444p(progressive), 640x360")).is_none());
        assert!(ok(&s("mpeg4 (Simple Profile) (FMP4 / 0x34504D46), yuv420p, 640x360")).is_none());
        assert!(ok(&s("wmv2 (WMV2 / 0x32564D57), yuv420p, 640x360")).is_none());
        // Cover art first, then the real stream.
        let both = "  Stream #0:0: Video: mjpeg (Baseline), yuvj420p(pc), 600x600 (attached pic)\n  Stream #0:1: Video: hevc (Main), yuv420p(tv), 1920x1080\n";
        assert_eq!(ok(both), Some((1920, 1080)));
        assert!(ok("Stream #0:0: Audio: aac\n").is_none());
    }
}
