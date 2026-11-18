//! The VMAF-checked quality retry for videos and the VMAF measurement.

use super::*;

pub(super) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Scales a video down to at most 1920x1080 worth of pixels (never up), even dimensions.
pub(super) const VMAF_FIT: &str = "scale=w='trunc(iw*min(1\\,sqrt(2073600/(iw*ih)))/2)*2':h='trunc(ih*min(1\\,sqrt(2073600/(iw*ih)))/2)*2':flags=bicubic";

/// Outcome of the quality-checked retry.
pub(super) enum Retry {
    Accepted { boost: u32, vmaf: f64, worst: f64 },
    Rejected(String),
    Stopped,
}

/// Stop was pressed during a measurement.
pub(super) struct Stopped;

/// What a video job had finished when Pause ended it (Pause stops every process to free
/// memory). On Resume the job carries on from there, taking the same decisions, so its
/// result is the one an uninterrupted run gives; only the step in progress is redone.
#[derive(Debug, Default)]
pub(super) struct VideoResume {
    /// The settings it was made with (`resume_key`): changed while paused, it is dropped.
    pub(super) settings: String,
    /// The preset encode was done and not smaller: its size, the input's duration, the
    /// mode and GPU decoding it used (the quality retry reuses them).
    pub(super) primary: Option<PrimaryDone>,
    /// Quality-retry steps done, by CQ boost.
    pub(super) steps: std::collections::BTreeMap<u32, Step>,
    /// The best passing step so far: its boost and its output, parked.
    pub(super) keep: Option<(u32, PathBuf)>,
}

/// The preset encode's size, the input's duration, its mode and GPU decoding.
pub(super) type PrimaryDone = (f64, f64, Mode, Option<(u32, u32)>);

#[derive(Debug, Clone)]
pub(super) enum Step {
    Failed,
    /// Encoded, not smaller enough: its size.
    Size(f64),
    /// Encoded (output parked), not measured yet.
    Encoded(f64, PathBuf),
    Measured(f64, Result<(f64, f64), String>),
}

impl VideoResume {
    /// Removes its parked files (dropped, or the batch ended without resuming it).
    pub(super) fn discard(&self) {
        for p in self.keep.iter().map(|(_, p)| p).chain(self.steps.values().filter_map(|s| match s {
            Step::Encoded(_, p) => Some(p),
            _ => None,
        })) {
            let _ = system::remove_file_if_exists(p);
        }
    }
}

/// The settings a video's result depends on.
pub(super) fn resume_key(o: &Options) -> String {
    format!("{}|{}|{}|{:?}", o.encoder, o.crf, o.preset, o.vmaf_floor)
}

/// Per-frame VMAF scores from a libvmaf JSON log.
pub(super) fn vmaf_frame_scores(json: &str) -> Option<Vec<f64>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("frames")?
        .as_array()?
        .iter()
        .map(|f| f.get("metrics")?.get("vmaf")?.as_f64())
        .collect()
}

/// Mean and 5th-percentile (worst 5% of frames) VMAF.
pub(super) fn summarize_vmaf(mut scores: Vec<f64>) -> Option<(f64, f64)> {
    if scores.is_empty() {
        return None;
    }
    scores.sort_by(f64::total_cmp);
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let p5 = scores[(scores.len() / 20).saturating_sub(1).min(scores.len() - 1)];
    Some((mean, p5))
}

impl Engine {
    /// Re-encodes with stronger settings (higher CQ/CRF) when the preset output was not
    /// smaller. A step is accepted only if its output is smaller and its VMAF against the
    /// original stays at or above `floor` (worst 5% of frames: `floor - 5`). It keeps
    /// climbing while that holds, and the strongest passing step wins.
    ///
    /// On the author's library each +4 CQ made AV1 output about 28% smaller, so the first
    /// step is the one predicted to get under the original size. If its quality fails, one
    /// gentler step is tried once.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn quality_retry(
        self: &Arc<Self>,
        worker_id: u32,
        input: &Path,
        tmp: &Path,
        display: &str,
        mode: Mode,
        gpu_decode: Option<(u32, u32)>,
        in_size: f64,
        out_size: f64,
        duration_sec: f64,
        floor: f64,
        memo: &mut VideoResume,
    ) -> Retry {
        const STEPS: [u32; 4] = [4, 8, 12, 16];
        const SHRINK_PER_STEP: f64 = 0.72;
        // Climb further only with some quality to spare: each step costs an encode.
        const CLIMB_MARGIN: f64 = 1.0;
        let ratio = out_size / in_size;
        let mut k = 0;
        while k + 1 < STEPS.len() && ratio * SHRINK_PER_STEP.powi(k as i32 + 1) > 0.95 {
            k += 1;
        }
        self.log(format!(
            "[Worker {worker_id}] {display} was {:.0}% of the original at the preset; retrying stronger settings with a VMAF {floor:.0} check",
            ratio * 100.0
        ));
        // The best passing output is parked under another temp name while stronger steps run.
        let ext = args::ext_lower(tmp);
        let keep = args::temp_name_with(input, worker_id + 1000, &ext);
        // Parked outputs while paused: named after the file, not the worker (another worker
        // may resume it).
        let (park_keep, park_encoded) = (args::temp_name_with(input, 7000, &ext), args::temp_name_with(input, 7001, &ext));
        let parked_keep = memo.keep.take();
        let mut best: Option<(u32, f64, f64)> = None;
        let mut tried = [false; STEPS.len()];
        let mut stepped_back = false;
        let mut notes: Vec<String> = Vec::new();
        let finish = |engine: &Arc<Self>, best: Option<(u32, f64, f64)>, notes: &[String]| {
            let (engine, keep, tmp, notes) = (Arc::clone(engine), keep.clone(), tmp.to_path_buf(), notes.join("; "));
            async move {
                let Some((boost, vmaf, worst)) = best else { return Retry::Rejected(notes) };
                let k = keep.clone();
                match engine.blocking(move || std::fs::rename(&k, &tmp)).await {
                    Ok(()) => Retry::Accepted { boost, vmaf, worst },
                    Err(err) => {
                        engine.log_remove(&keep);
                        Retry::Rejected(format!("could not keep the retry output: {err}"))
                    }
                }
            }
        };
        loop {
            let boost = STEPS[k];
            tried[k] = true;
            self.log_remove(tmp);
            let done = memo.steps.get(&boost).cloned();
            let size = match &done {
                Some(Step::Failed) => {
                    notes.push(format!("the +{boost} encode failed"));
                    return finish(self, best, &notes).await;
                }
                Some(Step::Size(s) | Step::Measured(s, _)) => *s,
                Some(Step::Encoded(s, parked)) => {
                    let (from, to) = (parked.clone(), tmp.to_path_buf());
                    if self.blocking(move || std::fs::rename(from, to)).await.is_err() {
                        notes.push(format!("the +{boost} output kept during the pause is gone"));
                        return finish(self, best, &notes).await;
                    }
                    self.log(format!("[Worker {worker_id}] {display}: the +{boost} encode was done before the pause; measuring it"));
                    *s
                }
                None => {
                    self.reset_worker(worker_id, display, &format!("quality retry +{boost}"));
                    let attempt = self.encode(worker_id, input, tmp, display, mode, boost, gpu_decode).await;
                    match attempt.status {
                        AttemptStatus::Stopped => {
                            self.park(&keep, &park_keep, best.map(|b| b.0), memo).await;
                            return Retry::Stopped;
                        }
                        AttemptStatus::Exited(0) => {}
                        _ => {
                            memo.steps.insert(boost, Step::Failed);
                            notes.push(format!("the +{boost} encode failed"));
                            self.log_remove(tmp);
                            return finish(self, best, &notes).await;
                        }
                    }
                    let t = tmp.to_path_buf();
                    self.blocking(move || std::fs::metadata(t).map_or(f64::MAX, |m| m.len() as f64)).await
                }
            };
            if size >= in_size * 0.99 {
                memo.steps.insert(boost, Step::Size(size));
                notes.push(format!("+{boost}: still {:.0}% of the original", size / in_size * 100.0));
                self.log_remove(tmp);
                if best.is_none() && !stepped_back && k + 1 < STEPS.len() && !tried[k + 1] {
                    k += 1;
                    continue;
                }
                return finish(self, best, &notes).await;
            }
            let mut measured = if let Some(Step::Measured(_, m)) = &done {
                m.clone()
            } else {
                self.reset_worker(worker_id, display, &format!("measuring quality at +{boost}"));
                self.send_state(false);
                match self.measure_vmaf(input, tmp, duration_sec, None).await {
                    Err(Stopped) => {
                        self.park_measuring(tmp, &park_encoded, boost, size, memo).await;
                        self.park(&keep, &park_keep, best.map(|b| b.0), memo).await;
                        return Retry::Stopped;
                    }
                    Ok(m) => m,
                }
            };
            // libvmaf fails to allocate when memory is tight (parallel 4K encodes): wait for
            // free memory and measure once more on fewer threads.
            if measured.as_ref().is_err_and(|e| e.contains("alloc")) {
                self.log(format!("[Worker {worker_id}] {display}: not enough memory to measure quality; waiting to retry"));
                let cancel = self.release_token();
                for _ in 0..60 {
                    if system::memory_status().is_none_or(|m| m.available_commit >= 4 << 30) {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        _ = cancel.cancelled() => {
                            self.park_measuring(tmp, &park_encoded, boost, size, memo).await;
                            self.park(&keep, &park_keep, best.map(|b| b.0), memo).await;
                            return Retry::Stopped;
                        }
                    }
                }
                measured = match self.measure_vmaf(input, tmp, duration_sec, Some(2)).await {
                    Err(Stopped) => {
                        self.park_measuring(tmp, &park_encoded, boost, size, memo).await;
                        self.park(&keep, &park_keep, best.map(|b| b.0), memo).await;
                        return Retry::Stopped;
                    }
                    Ok(m) => m,
                };
            }
            memo.steps.insert(boost, Step::Measured(size, measured.clone()));
            match measured {
                Ok((vmaf, worst)) if vmaf >= floor && worst >= floor - 5.0 => {
                    // Replayed: the output is the parked best (if this step was it) or gone
                    // (a later step beat it before the pause, and will again).
                    let source = match (&done, &parked_keep) {
                        (Some(Step::Measured(..)), Some((b, parked))) if *b == boost => Some(parked.clone()),
                        (Some(Step::Measured(..)), _) => None,
                        _ => Some(tmp.to_path_buf()),
                    };
                    let kp = keep.clone();
                    let kept = match source {
                        Some(t) => self.blocking(move || system::remove_file_if_exists(&kp).and_then(|()| std::fs::rename(&t, &kp))).await,
                        None => Ok(()),
                    };
                    if let Err(err) = kept {
                        notes.push(format!("could not keep the +{boost} output: {err}"));
                        self.log_remove(tmp);
                        return finish(self, best, &notes).await;
                    }
                    best = Some((boost, vmaf, worst));
                    self.log(format!(
                        "[Worker {worker_id}] {display}: +{boost} CQ gives {:.0}% of the original at VMAF {vmaf:.1} (worst 5%: {worst:.1})",
                        size / in_size * 100.0
                    ));
                    if !stepped_back && vmaf >= floor + CLIMB_MARGIN && k + 1 < STEPS.len() {
                        k += 1;
                        continue;
                    }
                    return finish(self, best, &notes).await;
                }
                Ok((vmaf, worst)) => {
                    notes.push(format!("+{boost}: VMAF {vmaf:.1} (worst 5%: {worst:.1}) is below {floor:.0}"));
                    self.log_remove(tmp);
                    if best.is_none() && k > 0 && !tried[k - 1] && !stepped_back {
                        stepped_back = true;
                        k -= 1;
                        continue;
                    }
                    return finish(self, best, &notes).await;
                }
                Err(why) => {
                    notes.push(format!("the quality could not be measured: {why}"));
                    self.log_remove(tmp);
                    return finish(self, best, &notes).await;
                }
            }
        }
    }

    /// Pause ended a measurement: the step's output is parked, to be measured on Resume.
    async fn park_measuring(&self, tmp: &Path, parked: &Path, boost: u32, size: f64, memo: &mut VideoResume) {
        let (from, to) = (tmp.to_path_buf(), parked.to_path_buf());
        if self.blocking(move || std::fs::rename(from, to)).await.is_ok() {
            memo.steps.insert(boost, Step::Encoded(size, parked.to_path_buf()));
        }
    }

    /// Pause ended the retry: the best output so far is parked for Resume.
    async fn park(&self, keep: &Path, park_keep: &Path, best_boost: Option<u32>, memo: &mut VideoResume) {
        match best_boost {
            Some(b) => {
                let (from, to) = (keep.to_path_buf(), park_keep.to_path_buf());
                if self.blocking(move || std::fs::rename(from, to)).await.is_ok() {
                    memo.keep = Some((b, park_keep.to_path_buf()));
                }
            }
            None => self.log_remove(keep),
        }
    }

    /// VMAF of `encoded` against `original`: mean and 5th percentile over the frames of
    /// three 8-second windows (20%, 50%, 80% of the video), or of the whole video when it
    /// is short. The original gets the same even-size scaling the encode used, so frames
    /// line up. `Ok(Err(reason))` if it could not be measured; `Err` if Stop was pressed.
    pub(super) async fn measure_vmaf(
        &self,
        original: &Path,
        encoded: &Path,
        duration_sec: f64,
        threads: Option<usize>,
    ) -> Result<Result<(f64, f64), String>, Stopped> {
        let cancel = self.release_token();
        let windows: Vec<(f64, f64)> = if duration_sec > 30.0 {
            [0.2, 0.5, 0.8].iter().map(|f| ((duration_sec * f - 4.0).max(0.0), 8.0)).collect()
        } else {
            vec![(0.0, 0.0)] // whole video
        };
        let threads = threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).max(1)));
        let log_dir = std::env::temp_dir();
        // Unique per measurement; only [A-Za-z0-9-.] so it is safe inside the filter graph.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let log_name = format!("mediaopti-vmaf-{}-{n}.json", std::process::id());
        let mut scores = Vec::new();
        for (start, length) in windows {
            if !self.wait_while_paused(&cancel).await {
                return Err(Stopped);
            }
            let cut = |args: &mut Vec<String>| {
                if length > 0.0 {
                    args.extend(["-ss".into(), format!("{start:.2}"), "-t".into(), format!("{length:.2}")]);
                }
            };
            let mut a: Vec<String> = vec!["-hide_banner".into(), "-v".into(), "error".into()];
            cut(&mut a);
            a.extend(["-i".into(), encoded.to_string_lossy().into_owned()]);
            cut(&mut a);
            a.extend(["-i".into(), original.to_string_lossy().into_owned()]);
            a.extend([
                "-lavfi".into(),
                // Both sides are brought to at most 1080p (same pixel count, aspect kept): the
                // VMAF model is made for 1080p viewing, and 4K measurements can exhaust memory.
                format!(
                    "[0:v:0]setpts=PTS-STARTPTS,{fit},format=yuv420p10le[d];\
                     [1:v:0]scale=max(2\\,trunc(iw/2)*2):max(2\\,trunc(ih/2)*2),setpts=PTS-STARTPTS,{fit},format=yuv420p10le[r];\
                     [d][r]libvmaf=shortest=1:n_subsample=2:n_threads={threads}:log_fmt=json:log_path={log_name}",
                    fit = VMAF_FIT
                ),
                "-f".into(),
                "null".into(),
                "-".into(),
            ]);
            if !self.wait_for_vmaf_memory(&cancel).await {
                return Err(Stopped);
            }
            let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
            cmd.args(&a)
                .current_dir(&log_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            system::configure_background_command(&mut cmd, true);
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(err) => return Ok(Err(format!("could not start ffmpeg: {err}"))),
            };
            system::assign_to_kill_on_exit_job(&child);
            let _running = RunningProcess::new(self);
            let stderr = child.stderr.take();
            let err_task = self.rt.spawn(async move {
                let mut text = Vec::new();
                if let Some(mut e) = stderr {
                    let _ = e.read_to_end(&mut text).await;
                }
                text
            });
            // Pause suspends the measurement like an encode; returning on Stop drops the
            // child, which kills it (kill_on_drop).
            let mut pause_rx = self.paused.subscribe();
            let mut suspended = false;
            let status = loop {
                let pause = *pause_rx.borrow_and_update();
                if pause != suspended && system::set_process_suspended(&child, pause) {
                    suspended = pause;
                }
                tokio::select! {
                    s = child.wait() => break s,
                    changed = pause_rx.changed() => {
                        if changed.is_err() {
                            break child.wait().await;
                        }
                    }
                    _ = cancel.cancelled() => {
                        drop(child);
                        let _ = system::remove_file_if_exists(&log_dir.join(&log_name));
                        return Err(Stopped);
                    }
                }
            };
            let stderr = err_task.await.unwrap_or_default();
            let log_path = log_dir.join(&log_name);
            let parsed = std::fs::read_to_string(&log_path).ok().and_then(|t| vmaf_frame_scores(&t));
            let _ = system::remove_file_if_exists(&log_path);
            match (status, parsed) {
                (Ok(s), Some(frames)) if s.success() && !frames.is_empty() => scores.extend(frames),
                (Ok(_), _) => {
                    let err = String::from_utf8_lossy(&stderr);
                    let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("no frames scored");
                    return Ok(Err(line.trim().to_string()));
                }
                (Err(err), _) => return Ok(Err(err.to_string())),
            }
        }
        Ok(summarize_vmaf(scores).ok_or_else(|| "no frames scored".to_string()))
    }
}

/// Counts a process in `running_processes` while it lives, so encoder admission and the
/// out-of-memory retry see it.
pub(super) struct RunningProcess<'a>(&'a Engine);

impl<'a> RunningProcess<'a> {
    pub(super) fn new(engine: &'a Engine) -> Self {
        engine.lock().running_processes += 1;
        Self(engine)
    }
}

impl Drop for RunningProcess<'_> {
    fn drop(&mut self) {
        let mut inner = self.0.lock();
        inner.running_processes = inner.running_processes.saturating_sub(1);
    }
}

impl Engine {
    /// Waits for `VMAF_MIN_FREE_COMMIT` before a measurement while other work runs (alone,
    /// it always starts). False on Stop.
    async fn wait_for_vmaf_memory(&self, cancel: &CancellationToken) -> bool {
        loop {
            let busy = {
                let inner = self.lock();
                inner.running_processes + inner.images_active
            };
            if busy == 0 || system::memory_status().is_none_or(|m| m.available_commit >= VMAF_MIN_FREE_COMMIT) {
                return true;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = cancel.cancelled() => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmaf_log_parsing_and_summary() {
        let json = r#"{"frames":[{"frameNum":0,"metrics":{"vmaf":90.0}},{"frameNum":1,"metrics":{"vmaf":100.0}}],"pooled_metrics":{}}"#;
        assert_eq!(vmaf_frame_scores(json), Some(vec![90.0, 100.0]));
        assert_eq!(vmaf_frame_scores("not json"), None);
        assert_eq!(vmaf_frame_scores(r#"{"frames":[{"metrics":{}}]}"#), None);
        let (mean, worst) = summarize_vmaf((1..=100).map(f64::from).collect()).unwrap();
        assert!((mean - 50.5).abs() < 1e-9);
        assert!((worst - 5.0).abs() < 1e-9, "5th percentile of 1..=100 is 5, got {worst}");
        assert_eq!(summarize_vmaf(vec![]), None);
    }
}

