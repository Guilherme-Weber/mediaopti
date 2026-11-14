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
    pub(super) async fn run_attempt(
        self: &Arc<Self>,
        worker_id: u32,
        input: &Path,
        tmp: &Path,
        display: &str,
        mode: Mode,
        cq_boost: u32,
    ) -> Attempt {
        let mut diag = Diagnostics::default();
        let (mut options, cancel) = {
            let inner = self.lock();
            (inner.options.clone(), inner.cancel.clone())
        };
        options.cq_boost = cq_boost;
        let ffmpeg_args = args::build_ffmpeg_args(input, tmp, &options, mode);

        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(&ffmpeg_args)
            .stdin(Stdio::null())
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
        let plentiful = system::memory_status().is_some_and(|m| m.available_commit >= PLENTIFUL_COMMIT);
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
                        let _ = child.start_kill();
                    }
                }
                _ = cancel.cancelled(), if !killed_for_stop => {
                    killed_for_stop = true;
                    let _ = child.start_kill();
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
                Ok(s) => AttemptStatus::Exited(s.code().unwrap_or(1)),
                Err(err) => AttemptStatus::SpawnError(err.to_string()),
            }
        };
        diag.duration_sec = self.lock().workers.get(&worker_id).map_or(0.0, |w| w.duration_sec);
        Attempt { status, diag }
    }
}
