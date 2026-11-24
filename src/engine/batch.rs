//! Running a batch: starting and stopping workers, Pause, options, GPU polling, encoder admission.

use super::*;

/// The CPU slots a batch's image, audio and document workers share: every core but one (the
/// user's choice, 2026-11-19; it was two). The tools run at below-normal priority
/// (`tools::run`), so the desktop and games still come first. The stages run after the
/// videos (rolling), so they get the cores; slots beyond the active workers stay idle for big
/// jobs (comic pages, big pictures: `images::Lent`) to borrow.
pub fn cpu_slot_count() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    cores.saturating_sub(1).max(1)
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

fn mode_label(mode: ImageMode, target: u32) -> String {
    match mode {
        ImageMode::Off => "off".into(),
        ImageMode::Lossless => "lossless".into(),
        ImageMode::Lossy if target >= crate::image::DEFAULT_TARGET => format!("visually lossless (SSIMULACRA2 >= {target})"),
        ImageMode::Lossy => format!("lossy (SSIMULACRA2 >= {target})"),
    }
}

impl Engine {
    /// [`Self::apply_options`] off the calling thread (the window's): with a million files,
    /// rebuilding the queue takes a moment. The newest settings always win.
    pub fn apply_options_in_background(self: &Arc<Self>, opts: Options) {
        let generation = self.options_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let engine = Arc::clone(self);
        self.rt.spawn_blocking(move || engine.apply_options_of(opts, Some(generation)));
    }

    /// Applies settings outside a run, so the queue shown before Start already reflects
    /// them (e.g. which GIFs are queued).
    pub fn apply_options(self: &Arc<Self>, opts: Options) {
        self.apply_options_of(opts, None);
    }

    /// [`Self::apply_options`]; with `generation`, only if no newer settings were sent
    /// since, checked under the lock: two changes waiting for it may get it in any order.
    fn apply_options_of(self: &Arc<Self>, opts: Options, generation: Option<u64>) {
        // If Videos was turned on after a scan done with Videos off, inspect GIF headers now
        // (outside the engine lock) so still GIFs stay out of the video queue.
        let opts = opts.validated();
        let need_gifs = {
            let inner = self.lock();
            if generation.is_some_and(|g| self.options_generation.load(Ordering::SeqCst) != g) || inner.working {
                return;
            }
            (opts.videos && !inner.queue.gifs_checked() && inner.queue.has_dir())
                .then(|| inner.queue.animated_candidates())
        };
        let checked_gifs = need_gifs.map(|gifs| super::scan::find_static_gifs(&gifs, None));
        {
            let mut inner = self.lock();
            if generation.is_some_and(|g| self.options_generation.load(Ordering::SeqCst) != g) {
                return;
            }
            if inner.working {
                return;
            }
            inner.options = opts;
            if let Some(statics) = checked_gifs {
                inner.queue.set_static_gifs(statics);
            }
            if inner.queue.has_dir() {
                inner.rebuild_queue();
            }
        }
        self.send_state(true);
    }

    pub fn start(self: &Arc<Self>, opts: Options) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.run_batch(opts).await });
    }

    pub async fn run_batch(self: Arc<Self>, opts: Options) {
        // Ensure GIF headers are checked if Videos was enabled since the scan.
        let opts = opts.validated();
        let lazy_gifs = {
            let inner = self.lock();
            (!inner.working && inner.queue.has_dir() && opts.videos && !inner.queue.gifs_checked())
                .then(|| inner.queue.animated_candidates())
        };
        let checked_gifs = match lazy_gifs {
            Some(gifs) => Some(self.blocking(move || super::scan::find_static_gifs(&gifs, None)).await),
            None => None,
        };
        let (total, options) = {
            let mut inner = self.lock();
            if !inner.queue.has_dir() {
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
            if let Some(statics) = checked_gifs {
                inner.queue.set_static_gifs(statics);
            }
            inner.options = opts;
            inner.reset_run_tracking();
            inner.rebuild_queue();
            inner.stop_requested = false;
            self.paused.send_replace(false);
            let cpu_total = inner.queue.cpu_waiting();
            inner.run = RunStats {
                total: inner.queue.videos_waiting() + cpu_total,
                images_total: cpu_total,
                stage_total: inner.queue.cpu_stage_counts(),
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
        // Its originals land there: later scans leave it out whatever the option says then.
        if let Some(folder) = self.lock().backup_root().filter(|_| total > 0) {
            super::scan::note_backup_folder(&self.data_dir, &folder);
        }

        if total == 0 {
            self.log("No new files to process");
            self.send_state(true);
            return;
        }

        let (videos, images) = {
            let inner = self.lock();
            (inner.queue.videos_waiting(), inner.queue.cpu_waiting())
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
                mode_label(options.images, options.image_target),
                mode_label(options.audio, options.image_target)
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
                // A worker that panicked must not vanish silently. (A panic in one file's job
                // fails that file and the worker goes on; this catches one outside a job.)
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
                "Done. Made smaller {}, kept as they were {}, skipped for now {}, failed {}. Saved {saved} MB total",
                run.compressed, run.kept, run.skipped, run.failed
            ));
            if run.images_total > 0 {
                self.log(format!(
                    "Images and other files: {} processed, {} made smaller with a lossy pass (scores in their lines)",
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
            inner.queue.clear_in_progress();
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
        let videos = inner.queue.videos_waiting();
        let cpu_files = inner.queue.cpu_waiting();
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
                let slots = cpu_slot_count();
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
            inner.running_processes + inner.queue.active_cpu_total()
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
    /// Whether closing now would have to wait: a batch runs, an encoder or CPU job is
    /// stopping, or an original is being replaced.
    pub fn busy(&self) -> bool {
        let inner = self.lock();
        inner.working || inner.running_processes > 0 || inner.queue.active_cpu_total() > 0 || REPLACING.load(Ordering::SeqCst) > 0
    }

    pub fn shutdown(&self) {
        {
            let mut inner = self.lock();
            inner.stop_requested = true;
            inner.cancel.cancel();
            inner.power = None;
        }
        // Encoders were asked to quit (see `ask_to_quit`); killing them outright when the app
        // exits can make the NVIDIA driver reset the GPU. CPU jobs also exit cleanly and
        // remove their temp work folders once cancelled.
        let until = Instant::now() + Duration::from_secs(12);
        while {
            let inner = self.lock();
            inner.running_processes > 0 || inner.queue.active_cpu_total() > 0
        } && Instant::now() < until
        {
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
        inner.queue.release_video(input);
        inner.workers.remove(&worker_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two settings changes sent in a row both wait for the engine (a long queue rebuild
    /// holds it) and may get it in any order: the older one, arriving last, is dropped.
    #[test]
    fn late_settings_never_replace_newer_ones() {
        struct Quiet;
        impl Ui for Quiet {
            fn log(&self, _: String) {}
            fn state(&self, _: &StateDto) {}
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let data = std::env::temp_dir().join(format!("mo-settings-order-{}", std::process::id()));
        let engine = Engine::new(Box::new(Quiet), &data, PathBuf::from("ffmpeg.exe"), rt.handle().clone());
        let jobs = |n: u32| Options { concurrency: n, ..Options::default() };
        // Sent: 1 then 3. Applied: 3 first, then the older 1.
        engine.options_generation.store(2, Ordering::SeqCst);
        engine.apply_options_of(jobs(3), Some(2));
        engine.apply_options_of(jobs(1), Some(1));
        assert_eq!(engine.lock().options.concurrency, 3);
        // Straight calls (tests, the start of a batch) always apply.
        engine.apply_options(jobs(2));
        assert_eq!(engine.lock().options.concurrency, 2);
        let _ = std::fs::remove_dir_all(&data);
    }
}
