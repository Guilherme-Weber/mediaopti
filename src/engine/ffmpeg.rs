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

/// Finds the ffmpeg binary: next to the executable (release zip), in the project's `bin/`
/// folder (development), otherwise on PATH.
pub fn locate_ffmpeg() -> PathBuf {
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe_name)));
    let dev_copy = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join(exe_name);
    beside_exe
        .into_iter()
        .chain(std::iter::once(dev_copy))
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
        let (mut options, cancel) = {
            let inner = self.lock();
            (inner.options.clone(), inner.release.clone())
        };
        options.cq_boost = cq_boost;
        options.gpu_decode = gpu_decode;
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
                return Attempt {
                    status: AttemptStatus::SpawnError(err.to_string()),
                    diag,
                }
            }
        };
        system::assign_to_kill_on_exit_job(&child);
        self.lock().running_processes += 1;

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
                    let limit = if percent == 0.0 { 30 } else { 60 };
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
        self.lock().running_processes -= 1;

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
        let cancel = self.lock().release.clone();
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

    /// Whether `input` may be decoded on the GPU, and the (even) size to scale it to: a
    /// 10-bit NVENC encoder (AV1/HEVC) and a first video stream in H.264, HEVC, AV1 or VP9
    /// at 4:2:0, not rotated. For these the frames NVDEC +
    /// scale_cuda hand the encoder were checked to be bit-identical to the CPU path; MPEG-4
    /// Part 2 decodes differently and 4:2:2 is converted differently, so they stay on the CPU.
    pub(super) async fn gpu_decodable(&self, input: &Path) -> Option<(u32, u32)> {
        let encoder = self.lock().options.encoder.clone();
        // MEDIAOPTI_GPU_DECODE=0 turns it off (comparisons, or a driver that misbehaves).
        let off = std::env::var_os("MEDIAOPTI_GPU_DECODE").is_some_and(|v| v == "0");
        if off || !matches!(encoder.as_str(), "av1_nvenc" | "hevc_nvenc") || args::is_gif(input) {
            return None;
        }
        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(["-hide_banner", "-i"]).arg(input).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
        system::configure_background_command(&mut cmd, true);
        let out = cmd.output().await.ok()?;
        gpu_decodable_stream(&String::from_utf8_lossy(&out.stderr))
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
