//! Running a batch: starting and stopping workers, Pause, options, GPU polling, encoder admission.

use super::*;

/// Whether another encoder may start now. The first encoder always may (otherwise a
/// low-memory machine would wait forever).
/// `free_vram_mb`: `None` when unknown (no NVIDIA GPU, or a CPU encoder).
pub fn may_start_encoder(running: usize, memory: Option<system::MemoryStatus>, free_vram_mb: Option<u64>) -> bool {
    if running == 0 {
        return true;
    }
    let ram = memory.is_none_or(|m| m.available_commit >= MIN_FREE_COMMIT && m.available_physical >= MIN_FREE_PHYSICAL);
    ram && free_vram_mb.is_none_or(|v| v >= MIN_FREE_VRAM_MB)
}

/// Free GPU memory now, in MB (one `nvidia-smi` query, ~50 ms; asked only when an encoder
/// starts). `None` without an NVIDIA driver.
pub(super) async fn free_vram_mb() -> Option<u64> {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    system::configure_background_command(&mut cmd, false);
    let out = tokio::time::timeout(Duration::from_secs(5), cmd.output()).await.ok()?.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

fn mode_label(mode: ImageMode) -> &'static str {
    match mode {
        ImageMode::Off => "off",
        ImageMode::Lossless => "lossless",
        ImageMode::Lossy => "visually lossless (SSIMULACRA2 >= 93)",
    }
}

impl Engine {
    /// [`Self::apply_options`] off the calling thread (the window's): with a million files,
    /// rebuilding the queue takes a moment. The newest settings always win.
    pub fn apply_options_in_background(self: &Arc<Self>, raw: RawOptions) {
        static LATEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let generation = LATEST.fetch_add(1, Ordering::SeqCst) + 1;
        let engine = Arc::clone(self);
        self.rt.spawn_blocking(move || {
            if LATEST.load(Ordering::SeqCst) == generation {
                engine.apply_options(raw);
            }
        });
    }

    /// Applies settings outside a run, so the queue shown before Start already reflects
    /// them (e.g. which GIFs are queued).
    pub fn apply_options(self: &Arc<Self>, raw: RawOptions) {
        {
            let mut inner = self.lock();
            if inner.working {
                return;
            }
            inner.options = Options::normalize(raw);
            if inner.dir.is_some() {
                inner.rebuild_queue();
            }
        }
        self.send_state(true);
    }

    pub fn start(self: &Arc<Self>, raw: RawOptions) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.run_batch(raw).await });
    }

    pub async fn run_batch(self: Arc<Self>, raw: RawOptions) {
        let (total, options) = {
            let mut inner = self.lock();
            if inner.dir.is_none() {
                drop(inner);
                self.log("Select a folder before starting");
                self.send_state(true);
                return;
            }
            if inner.working {
                drop(inner);
                self.log("Processing is already running");
                self.send_state(true);
                return;
            }
            inner.options = Options::normalize(raw);
            inner.reset_run_tracking();
            inner.rebuild_queue();
            inner.stop_requested = false;
            self.paused.send_replace(false);
            inner.run = RunStats {
                total: inner.pending_queue.len() + inner.cpu_total(),
                images_total: inner.cpu_total(),
                stage_total: [inner.cpu_queues[0].len(), inner.cpu_queues[1].len(), inner.cpu_queues[2].len()],
                ..RunStats::default()
            };
            if inner.run.total > 0 {
                inner.working = true;
                inner.status = Status::Running;
                inner.cancel = CancellationToken::new();
                inner.release = inner.cancel.child_token();
                inner.power = Some(PowerGuard::acquire());
            } else {
                inner.status = Status::Done;
            }
            (inner.run.total, inner.options.clone())
        };

        if total == 0 {
            self.log("No new files to process");
            self.send_state(true);
            return;
        }

        let (videos, images) = {
            let inner = self.lock();
            (inner.pending_queue.len(), inner.cpu_total())
        };
        if videos > 0 {
            self.log(format!(
                "Starting {videos} videos | Encoder: {} | Workers: {} | Base CRF: {} | Preset: {}",
                options.encoder, options.concurrency, options.crf, options.preset
            ));
        }
        if images > 0 {
            self.log(format!(
                "Starting {images} images and other files | images {}, audio {}",
                mode_label(options.images),
                mode_label(options.audio)
            ));
            self.log(self.tools().summary());
        }
        // Keeps "running for" of long CPU files current when nothing else updates the window.
        let engine = Arc::clone(&self);
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !engine.lock().working {
                    break;
                }
                engine.send_state(false);
            }
        });
        {
            let mut inner = self.lock();
            inner.fatal = Arc::new(AtomicBool::new(false));
            inner.cpu = None;
            inner.cpu_active = [0; 3];
            inner.stage_logged = [false; 3];
            self.spawn_workers(&mut inner);
        }
        self.send_state(true);
        // Workers started later (files dropped during the run) are awaited too; the batch
        // ends, atomically with that check, when none is left.
        let (was_stopped, status, run, saved) = loop {
            let next = {
                let mut inner = self.lock();
                let next = inner.handles.pop();
                if next.is_none() {
                    break self.finish_batch(&mut inner);
                }
                next
            };
            if let Some((name, handle)) = next {
                // A worker that panicked must not vanish silently (release builds abort instead).
                if let Err(err) = handle.await {
                    self.log(format!("[{name}] stopped unexpectedly: {err}"));
                }
            }
        };
        self.flush_history();

        if was_stopped {
            self.log(format!(
                "Stopped. Completed {}/{}; saved {saved} MB",
                run.completed, run.total
            ));
        } else if status == Status::Error {
            self.log(format!("Stopped after an ffmpeg startup error; saved {saved} MB"));
        } else {
            self.log(format!(
                "Done. Made smaller {}, kept as they were {}, failed {}. Saved {saved} MB total",
                run.compressed, run.skipped_larger, run.failed
            ));
            if run.images_total > 0 {
                self.log(format!(
                    "Images and other files: {} processed, {} made smaller with a visually lossless (lossy) pass",
                    run.images_done, run.images_lossy
                ));
            }
        }
        self.send_state(true);
    }

    /// Ends a batch's bookkeeping (no worker is left). Returns what the summary needs.
    pub(super) fn finish_batch(&self, inner: &mut Inner) -> (bool, Status, RunStats, f64) {
        {
            let was_stopped = inner.stop_requested;
            inner.cpu = None;
            for (_, memo) in inner.resume.drain() {
                memo.discard();
            }
            inner.gpu_polling = false;
            inner.video_ids.clear();
            inner.cpu_ids.clear();
            inner.working = false;
            inner.stop_requested = false;
            self.paused.send_replace(false);
            inner.power = None;
            inner.gpu_enc = 0.0;
            inner.gpu_vram_mb = 0.0;
            inner.in_progress.clear();
            inner.workers.clear();
            inner.rebuild_queue();
            if was_stopped {
                inner.status = Status::Ready;
            } else if inner.status != Status::Error {
                inner.status = Status::Done;
            }
            (was_stopped, inner.status, inner.run, round1(inner.space_saved_mb))
        }
    }

    /// Starts the workers the queues need: video workers up to the job count, CPU workers up
    /// to the CPU slots (once per batch: their settings are made with the first CPU file).
    /// Called with the lock held, at the start of a batch and when files are added to it.
    pub(super) fn spawn_workers(self: &Arc<Self>, inner: &mut Inner) {
        let videos = inner.pending_queue.len().saturating_sub(inner.queue_cursor);
        let cpu_files = inner.cpu_left();
        let want_video = (inner.options.concurrency as usize).min(videos + inner.video_ids.len());
        while inner.video_ids.len() < want_video {
            let id = (1..).find(|i| !inner.video_ids.contains(i)).unwrap_or(1);
            inner.video_ids.insert(id);
            let (engine, fatal) = (Arc::clone(self), Arc::clone(&inner.fatal));
            inner.handles.push((format!("Worker {id}"), self.rt.spawn(async move { engine.run_worker(id, fatal).await })));
        }
        if videos > 0 && !inner.gpu_polling {
            inner.gpu_polling = true;
            self.start_gpu_polling_with(inner.cancel.clone());
        }
        if cpu_files == 0 {
            return;
        }
        let cpu = if let Some(c) = &inner.cpu {
            Arc::clone(c)
        } else {
            {
                // Leave some CPU for the desktop. The stages run after the videos (rolling), so
                // they get every other core; slots beyond the active workers stay idle for big
                // jobs (comic pages) to borrow.
                let cores = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
                let slots = cores.saturating_sub(2).max(1);
                let c = Arc::new(images::CpuCtx::new(self.image_ctx(&inner.options, slots, inner.cancel.clone()), &inner.options, slots));
                inner.cpu = Some(Arc::clone(&c));
                c
            }
        };
        let want_cpu = cpu.slots.min(cpu_files + inner.cpu_ids.len());
        let _ = videos;
        while inner.cpu_ids.len() < want_cpu {
            let id = (1..).find(|i| !inner.cpu_ids.contains(i)).unwrap_or(1);
            inner.cpu_ids.insert(id);
            let (engine, cpu) = (Arc::clone(self), Arc::clone(&cpu));
            inner.handles.push((format!("Image worker {id}"), self.rt.spawn(async move { engine.run_image_worker(id, cpu).await })));
        }
    }

    pub(super) async fn run_worker(self: Arc<Self>, worker_id: u32, fatal: Arc<AtomicBool>) {
        loop {
            {
                let mut inner = self.lock();
                if !inner.working || inner.stop_requested || fatal.load(Ordering::SeqCst) {
                    inner.video_ids.remove(&worker_id);
                    break;
                }
            }
            match self.process_one(worker_id).await {
                // Empty: the id was released with the empty-queue check, under one lock.
                Outcome::Empty => break,
                Outcome::Stopped => {
                    self.lock().video_ids.remove(&worker_id);
                    break;
                }
                Outcome::Fatal => {
                    fatal.store(true, Ordering::SeqCst);
                    let mut inner = self.lock();
                    inner.status = Status::Error;
                    inner.video_ids.remove(&worker_id);
                    break;
                }
                Outcome::Processed | Outcome::Skipped | Outcome::Failed | Outcome::Released => {}
            }
        }
    }

    pub fn stop(self: &Arc<Self>) {
        let running = {
            let mut inner = self.lock();
            if inner.working {
                inner.stop_requested = true;
                inner.status = Status::Stopping;
                inner.cancel.cancel();
                Some(inner.running_processes)
            } else {
                None
            }
        };
        if let Some(n) = running {
            self.log(match n {
                1 => "Stopping 1 active ffmpeg process...".to_string(),
                n => format!("Stopping {n} active ffmpeg processes..."),
            });
        }
        self.send_state(true);
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// Pauses or resumes the running batch. Pausing suspends the active ffmpeg processes
    /// (they keep their progress), starts no new files and lets the PC sleep again.
    pub fn set_paused(self: &Arc<Self>, pause: bool) {
        let active = {
            let mut inner = self.lock();
            let allowed = if pause {
                inner.status == Status::Running
            } else {
                inner.status == Status::Paused
            };
            if !inner.working || inner.stop_requested || !allowed {
                return;
            }
            if pause {
                inner.status = Status::Paused;
                inner.power = None;
            } else {
                inner.status = Status::Running;
                inner.power = Some(PowerGuard::acquire());
            }
            self.paused.send_replace(pause);
            // Every process ends (encoders asked to quit), freeing RAM, GPU memory, CPU and
            // GPU; their files continue from their last finished step on Resume.
            if pause {
                inner.release.cancel();
                inner.release = inner.cancel.child_token();
            }
            inner.running_processes + inner.images_active
        };
        self.log(if pause {
            match active {
                0 => "Paused. Press Resume to continue.".to_string(),
                n => format!("Paused: {n} file(s) in progress stopped to free memory, CPU and GPU. Press Resume to continue."),
            }
        } else {
            "Resumed".to_string()
        });
        self.send_state(true);
    }

    /// Ends the files in progress of a paused batch again (Pause already does). Does nothing
    /// unless paused.
    #[cfg(test)]
    pub fn release_paused_jobs(&self) {
        let mut inner = self.lock();
        if inner.status == Status::Paused && inner.working && !inner.stop_requested {
            inner.release.cancel();
            inner.release = inner.cancel.child_token();
        }
    }

    pub fn toggle_pause(self: &Arc<Self>) {
        self.set_paused(!self.is_paused());
    }

    /// Waits while the batch is paused. Returns false if Stop was pressed.
    pub(super) async fn wait_while_paused(&self, cancel: &CancellationToken) -> bool {
        let mut rx = self.paused.subscribe();
        tokio::select! {
            r = rx.wait_for(|paused| !*paused) => r.is_ok(),
            _ = cancel.cancelled() => false,
        }
    }

    /// Called when the app exits: stops ffmpeg and saves history.
    pub fn shutdown(&self) {
        {
            let mut inner = self.lock();
            inner.stop_requested = true;
            inner.cancel.cancel();
            inner.power = None;
        }
        // Encoders were asked to quit (see `ask_to_quit`); killing them outright when the app
        // exits can make the NVIDIA driver reset the GPU.
        let until = Instant::now() + Duration::from_secs(12);
        while self.lock().running_processes > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        // A file being replaced finishes first (a few seconds at most), so quitting never
        // leaves an original in the Recycle Bin with its new version still under a temp name.
        let until = Instant::now() + Duration::from_secs(15);
        while REPLACING.load(Ordering::SeqCst) > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        self.flush_history();
    }

    /// Reads NVENC load and VRAM from one long-running `nvidia-smi -lms` process
    /// (spawning nvidia-smi for every sample cost ~50 ms of CPU each time).
    pub(super) fn start_gpu_polling_with(self: &Arc<Self>, cancel: CancellationToken) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move {
            let mut cmd = tokio::process::Command::new("nvidia-smi");
            cmd.args([
                "--query-gpu=utilization.encoder,memory.used",
                "--format=csv,noheader,nounits",
                "-lms",
                "1500",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
            system::configure_background_command(&mut cmd, false);
            let Ok(mut child) = cmd.spawn() else {
                return; // no NVIDIA driver: telemetry stays at zero
            };
            system::assign_to_kill_on_exit_job(&child);
            let Some(stdout) = child.stdout.take() else {
                return;
            };
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let mut parts = line.split(',');
                        let enc = parts.next().and_then(|v| v.trim().parse::<f64>().ok());
                        let vram = parts.next().and_then(|v| v.trim().parse::<f64>().ok());
                        if let Some(enc) = enc {
                            {
                                let mut inner = engine.lock();
                                if !inner.working {
                                    break;
                                }
                                inner.gpu_enc = enc;
                                inner.gpu_vram_mb = vram.unwrap_or(0.0);
                            }
                            engine.send_state(false);
                        }
                    }
                    _ = cancel.cancelled() => break,
                }
            }
            let _ = child.start_kill();
        });
    }

    /// Shows `display` on the worker's row doing `step`, from 0%. The file's time keeps
    /// counting from its first step.
    pub(super) fn reset_worker(&self, worker_id: u32, display: &str, step: &str) {
        let mut inner = self.lock();
        let started = match inner.worker_files.get(&worker_id) {
            Some((file, t)) if file == display => *t,
            _ => {
                inner.worker_files.insert(worker_id, (display.to_string(), Instant::now()));
                Instant::now()
            }
        };
        let mut w = WorkerProgress::new(display);
        w.step = step.to_string();
        w.started = started;
        inner.workers.insert(worker_id, w);
    }

    /// The worker's step now ("encoding" if it has none).
    pub(super) fn worker_step(&self, worker_id: u32) -> String {
        self.lock().workers.get(&worker_id).map_or_else(|| "encoding".to_string(), |w| w.step.clone())
    }

    pub(super) fn release_tracking(&self, worker_id: u32, input: &Path) {
        let mut inner = self.lock();
        inner.in_progress.remove(input);
        inner.workers.remove(&worker_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn mem(commit_gb: u64, phys_gb: u64) -> Option<system::MemoryStatus> {
        Some(system::MemoryStatus {
            available_commit: commit_gb * GB,
            available_physical: phys_gb * GB,
        })
    }

    #[test]
    fn memory_admission() {
        // The first encoder always starts, even when memory is tight.
        assert!(may_start_encoder(0, mem(1, 1), Some(100)));
        // Further encoders need headroom for a 4K encode.
        assert!(!may_start_encoder(1, mem(4, 8), None));
        assert!(!may_start_encoder(1, mem(10, 1), None));
        assert!(may_start_encoder(1, mem(10, 4), None));
        assert!(may_start_encoder(3, None, None));
        // ... in GPU memory too.
        assert!(!may_start_encoder(1, mem(10, 4), Some(2048)));
        assert!(may_start_encoder(1, mem(10, 4), Some(4096)));
    }

}
