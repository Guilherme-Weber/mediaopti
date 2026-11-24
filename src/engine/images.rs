//! CPU image workers (PNG, JPEG, small GIFs), running next to the GPU video workers.
//!
//! Each image is optimized on a private copy (ASCII name, in the temp folder), and the
//! original is replaced only when the verified result is at least 0.5% smaller and the
//! original did not change meanwhile. Hard-linked files are left alone. Replacement,
//! Recycle Bin, dates and attributes work exactly as for videos.

use super::{file_state, Engine, Inner};
use crate::history::{self, ContentHash};
use crate::image::{self, tools::RunError};
use crate::{args, system};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Free commit (RAM + page file) an image worker needs before starting another file. When the
/// machine runs out of commit, allocations fail and processes (this app included) abort.
const IMAGE_MIN_FREE_COMMIT: u64 = 2 * 1024 * 1024 * 1024;
/// How long a CPU job just let in counts as about to take memory that doesn't show yet.
const ADMISSION_WARMUP: Duration = Duration::from_secs(1);
/// What each of those counts for. Small on purpose: 2 GB each for 5 s held a run of small
/// files (~1 s each, 14 slots) to a few files a second on 8 GB of free commit; this spreads
/// a burst (14 workers waking together at a stage change) without slowing a steady run.
const ADMISSION_RESERVE: u64 = 512 * 1024 * 1024;

/// Free commit memory needed to let one more CPU job in, with `recent` let in within the
/// last `ADMISSION_WARMUP`.
fn image_admission_need(recent: usize) -> u64 {
    IMAGE_MIN_FREE_COMMIT + ADMISSION_RESERVE * recent as u64
}

/// Whether one more CPU job may start now. One always may when nothing else runs (a
/// low-memory machine still makes progress); jobs let in a moment ago count as running
/// (`images_active` grows only once their file is claimed) and raise what the next needs.
fn may_admit(inner: &mut Inner, memory: Option<&system::MemoryStatus>) -> bool {
    let now = std::time::Instant::now();
    inner.cpu_admitted.retain(|t| now.duration_since(*t) < ADMISSION_WARMUP);
    let busy = inner.images_active + inner.running_processes + inner.cpu_admitted.len();
    busy == 0 || memory.is_none_or(|m| m.available_commit >= image_admission_need(inner.cpu_admitted.len()))
}



/// What a "Saved" line says about quality: the score the lossy result was measured at (its
/// most changed picture or page), nothing for a lossless result.
fn quality_note(lossy: bool, score: Option<f64>) -> String {
    match (lossy, score) {
        (false, _) => String::new(),
        (true, Some(s)) => format!(" (lossy, SSIMULACRA2 {s:.1})"),
        (true, None) => " (lossy)".into(),
    }
}

/// Log a progress line every this many images (one line per image would flood the log).
const PROGRESS_EVERY: usize = 250;

pub(super) enum ImageResult {
    Smaller {
        before: u64,
        after: u64,
        /// A lossy encoder won (quality search labels look like `beeim-quantizer=81~87.4`).
        lossy: bool,
        /// SSIMULACRA2 of its most changed picture (`image::lowest_score`), when lossy.
        score: Option<f64>,
        content: Option<(ContentHash, u64)>,
        /// Converted: the new file's path (the original's name is gone).
        renamed: Option<PathBuf>,
    },
    /// Nothing smaller (or not a valid image): remembered so it is not retried.
    Kept {
        content: Option<(ContentHash, u64)>,
        note: Option<String>,
    },
    /// Left alone and not remembered (hard link, changed meanwhile).
    Skipped(String),
    /// Only renamed to its real format (nothing smaller).
    Renamed { to: PathBuf, content: Option<(ContentHash, u64)> },
    /// A file with the same content already had the new name: the smaller copy (`kept`) stays,
    /// the original is gone; `before` bytes were freed. `replaced`: the new version replaced it.
    Duplicate { before: u64, kept: PathBuf, content: Option<(ContentHash, u64)>, replaced: bool },
    /// A tool ran out of memory (even after waiting for memory and trying again): its result
    /// would differ from a normal run's, so nothing is changed and nothing remembered.
    OutOfMemory,
    /// Could not be done (disk, permissions): not remembered, tried again next run.
    Failed(String),
    /// A tool could not run (missing, would not start, timed out): not the file's fault, so
    /// not remembered; many in a row leave the rest of the CPU queue for the next run.
    ToolUnavailable(String),
    /// Processing this file failed: remembered, not retried.
    Broken(String),
    Stopped,
}

/// A batch's CPU settings: the image pipeline's for pictures and documents, and the same
/// with the audio setting's lossiness for audio files (their cover art). Both share the CPU
/// slots and Stop/Pause.
pub(super) struct CpuCtx {
    pub image: image::Ctx,
    pub audio: image::Ctx,
    pub slots: usize,
}

impl CpuCtx {
    pub(super) fn new(image: image::Ctx, options: &crate::args::Options, slots: usize) -> Self {
        // Cover art follows the picture quality floor (`image.target`); the audio setting
        // only says whether it may be lossy.
        let audio = image::Ctx { lossy: options.audio == crate::args::ImageMode::Lossy, ..image.clone() };
        Self { image, audio, slots }
    }

    fn for_file(&self, path: &Path) -> &image::Ctx {
        if image::kind_of(path).is_some_and(image::Kind::is_audio) { &self.audio } else { &self.image }
    }
}

/// How many CPU files of each stage run at once with `slots` CPU slots: audio half the slots
/// (decoding is one core per file; the files are read once, sequentially), documents half the
/// slots (a PDF's renders and qpdf use memory; comic pages and PDF images borrow the idle
/// slots), images every slot.
fn stage_limits(slots: usize) -> [usize; 3] {
    let half = (slots / 2).max(2);
    [half, half, usize::MAX]
}

/// When audio or documents are the active CPU stage (and images haven't started yet), returns
/// `(stage_limit, stage_label)` so the UI's "CPU jobs" tile can explain why fewer than all
/// `slots` files are running at once (e.g. `7 of 7 (documents)` instead of `7 of 15 slots`).
pub(crate) fn active_stage_limit(type_progress: &[(usize, usize, usize); 4], slots: usize) -> Option<(usize, &'static str)> {
    // type_progress: [0]=videos, [1]=audio, [2]=documents, [3]=images -> (done, total, active).
    if type_progress[3].2 > 0 {
        return None;
    }
    let limits = stage_limits(slots);
    if type_progress[1].0 < type_progress[1].1 && type_progress[1].2 > 0 && type_progress[2].2 == 0 {
        return Some((limits[0].min(slots), "audio"));
    }
    if type_progress[2].0 < type_progress[2].1 && type_progress[2].2 > 0 {
        return Some((limits[1].min(slots), "documents"));
    }
    None
}

/// Images at or above this file size are treated as heavy (likely high-megapixel / slow /
/// memory-hungry): at most [`max_heavy_images`] run at once while smaller images are left,
/// so the remaining workers stay busy on fast files instead of piling onto huge ones.
const HEAVY_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// How many heavy images run at once alongside lighter ones (`slots / 4`, in `2..=4`).
fn max_heavy_images(slots: usize) -> usize {
    (slots / 4).clamp(2, 4)
}

enum CpuClaim {
    /// A file, its stage, whether it counts as a heavy image, and the log line announcing a new stage.
    File(PathBuf, usize, bool, Option<String>),
    /// An earlier stage still has files to start, or this stage is at its limit.
    Wait,
    /// Nothing left to start.
    Done,
}

impl Inner {
    /// The CPU jobs' settings under the current options (a chosen temp folder that is gone
    /// falls back to the system one).
    pub(super) fn job_opts(&self) -> JobOpts {
        JobOpts {
            temp: self.options.temp_dir.clone().filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir),
            fix_ext: self.options.fix_ext,
            busy: None,
            note: None,
            warn: None,
            lossless_only: Arc::default(),
        }
    }

    /// The next CPU file, by stage (rolling): nothing while videos are still to be started,
    /// then audio (at most `STAGE_LIMITS[0]` at once), then documents, then images. A stage
    /// begins as soon as the one before has nothing left to start.
    ///
    /// In the images stage (sorted largest-first), once [`max_heavy_images`] heavy images
    /// (>= 4 MiB) are already running, additional workers claim from the lighter end of the
    /// queue so all cores stay productive instead of piling onto huge memory-heavy pictures.
    fn claim_cpu(&mut self) -> CpuClaim {
        let videos_waiting = self.queue_cursor < self.pending_queue.len()
            && !self.video_ids.is_empty()
            && !self.fatal.load(std::sync::atomic::Ordering::SeqCst);
        if videos_waiting {
            return CpuClaim::Wait;
        }
        let slots = self.cpu.as_ref().map_or(2, |c| c.slots);
        let limits = stage_limits(slots);
        let max_heavy = max_heavy_images(slots);
        for (stage, limit) in limits.into_iter().enumerate() {
            while let Some(&front) = self.cpu_queues[stage].front() {
                let file = &self.files[front as usize];
                if self.in_progress.contains(file) || self.failed.contains(file) {
                    self.cpu_queues[stage].pop_front();
                } else {
                    break;
                }
            }
            if self.cpu_queues[stage].is_empty() {
                continue;
            }
            if self.cpu_active[stage] >= limit {
                return CpuClaim::Wait;
            }
            let front_heavy = stage == 2
                && self.cpu_queues[stage]
                    .front()
                    .is_some_and(|&i| self.file_sizes.get(i as usize).copied().unwrap_or(0) >= HEAVY_IMAGE_BYTES);
            let from_back = if front_heavy && self.heavy_images_active >= max_heavy {
                while let Some(&back) = self.cpu_queues[stage].back() {
                    let file = &self.files[back as usize];
                    if self.in_progress.contains(file) || self.failed.contains(file) {
                        self.cpu_queues[stage].pop_back();
                    } else {
                        break;
                    }
                }
                self.cpu_queues[stage]
                    .back()
                    .is_some_and(|&i| self.file_sizes.get(i as usize).copied().unwrap_or(0) < HEAVY_IMAGE_BYTES)
            } else {
                false
            };
            let left = self.cpu_queues[stage].len();
            let Some(idx) = (if from_back {
                self.cpu_queues[stage].pop_back()
            } else {
                self.cpu_queues[stage].pop_front()
            }) else {
                continue;
            };
            let heavy = stage == 2 && self.file_sizes.get(idx as usize).copied().unwrap_or(0) >= HEAVY_IMAGE_BYTES;
            let file = self.files[idx as usize].clone();
            self.cpu_active[stage] += 1;
            if heavy {
                self.heavy_images_active += 1;
            }
            self.in_progress.insert(file.clone());
            self.cpu_started.insert(file.clone(), std::time::Instant::now());
            let announce = (!self.stage_logged[stage]).then(|| {
                self.stage_logged[stage] = true;
                let (what, how) = match stage {
                    0 => ("audio files", format!("{} at a time", limits[0])),
                    1 => ("documents", format!("{} at a time, pages/images on idle cores", limits[1])),
                    _ => ("images", "on every free core".to_string()),
                };
                format!("Now processing {left} {what}, {how}")
            });
            return CpuClaim::File(file, stage, heavy, announce);
        }
        CpuClaim::Done
    }
}

impl Engine {
    /// Settings for the image pipeline under the current options.
    /// `slots`: how many tools may run at once (one per worker; idle ones are borrowed).
    pub(super) fn image_ctx(&self, options: &crate::args::Options, slots: usize, cancel: tokio_util::sync::CancellationToken) -> image::Ctx {
        image::Ctx {
            tools: self.tools(),
            lossy: options.images == crate::args::ImageMode::Lossy,
            target: f64::from(options.image_target),
            lossy_min_gain: 0.03,
            strip: options.strip,
            strict_alpha: false,
            lossy_docs: options.lossy_docs,
            threads: if slots > 1 { 1 } else { 0 },
            timeout: std::time::Duration::from_secs(900),
            gate: image::tools::Gate::new(cancel, Some(self.paused.subscribe())),
            max: max_parts(options),
            slots: (slots > 1).then(|| image::tools::Slots::new(slots)),
            progress: None,
        }
    }

    /// A big GIF whose video wasn't smaller: optimize it as a GIF instead (from a video
    /// worker, so it counts as that worker's file).
    pub(super) async fn gif_as_image(self: &Arc<Self>, worker_id: u32, input: &Path, display: &str) -> super::Outcome {
        // The job's release token: a Pause ends the optimizer (and frees its memory) like any
        // CPU job, instead of leaving it suspended.
        let release = self.release_token();
        let (ctx, disposal, lossy, strip, mut opts) = {
            let inner = self.lock();
            (
                Arc::new(self.image_ctx(&inner.options, 2, release)),
                inner.disposal_for(input),
                inner.images_lossy(),
                inner.options.strip,
                inner.job_opts(),
            )
        };
        // As in `run_image_worker`: the new version's content is noted durably before the
        // original is set aside, and a file another job holds is left alone.
        let engine = Arc::clone(self);
        opts.busy = Some(Arc::new(move |p: &Path| engine.lock().in_progress.contains(p)));
        let (engine, refused) = (Arc::clone(self), Arc::clone(&opts.lossless_only));
        opts.note = Some(Arc::new(move |p: &Path| {
            // The floor of the file written (a WAV made into FLAC takes FLAC's).
            let target = engine.lock().target_for(p);
            engine.note_content_now(p, image::mode_at(lossy && !refused.load(Ordering::Relaxed), strip, target))
        }));
        let engine = Arc::clone(self);
        opts.warn = Some(Arc::new(move |line: String| engine.log(line)));
        self.reset_worker(worker_id, display, "optimizing as GIF");
        self.send_state(false);
        let p = input.to_path_buf();
        let lossless_only = Arc::clone(&opts.lossless_only);
        // Video worker ids are 1-4; image-temp names use a separate range.
        let result = self.blocking(move || process_image(&ctx, &p, worker_id + 100, &disposal, &opts)).await;
        self.lock().workers.remove(&worker_id);
        let outcome = match &result {
            ImageResult::Stopped => super::Outcome::Stopped,
            ImageResult::Failed(_) | ImageResult::ToolUnavailable(_) => super::Outcome::Failed,
            ImageResult::Smaller { before, after, .. } => {
                self.log(format!(
                    "[Worker {worker_id}] {display} stays a GIF: {:.0}% smaller as an image",
                    100.0 * (before - after) as f64 / *before as f64
                ));
                super::Outcome::Processed
            }
            _ => super::Outcome::Skipped,
        };
        // Recorded like any finished image (history, savings, log); the video job doesn't
        // count it as completed (see `Completion`).
        {
            let mut inner = self.lock();
            inner.images_active += 1; // finish_image decrements it
            inner.run.images_total += 1;
            inner.run.stage_total[2] += 1;
        }
        // Its new content (or the kept GIF's) went through the video attempt as well: moved
        // or renamed later, it is recognised as done for video too, not encoded again.
        let tried_as_video = match &result {
            ImageResult::Smaller { content, .. } | ImageResult::Kept { content, .. } => *content,
            _ => None,
        };
        // Stopped by a Pause: `finish_image` puts it back in the CPU queue, so the GIF pass
        // runs again on Resume (the video attempt is not redone: see `gif_fallback`).
        self.finish_image(input, lossy && !lossless_only.load(Ordering::Relaxed), result);
        if let Some((hash, size)) = tried_as_video {
            self.lock().history.add_content(hash, size, crate::history::VIDEO_MODE);
            self.schedule_history_save();
        }
        outcome
    }

    /// Waits until starting another image looks safe for memory (`may_admit`; the claim
    /// checks again under its lock and records the admission). Only a real shortage (less
    /// than 2 GB free) is shown and logged, not the spacing of a burst. Returns false on Stop.
    async fn wait_for_image_memory(&self, cancel: &tokio_util::sync::CancellationToken) -> bool {
        loop {
            let memory = system::memory_status();
            if may_admit(&mut self.lock(), memory.as_ref()) {
                self.lock().cpu_waiting_memory = false;
                return true;
            }
            let short = memory.is_some_and(|m| m.available_commit < IMAGE_MIN_FREE_COMMIT);
            // Shown like a video waiting for memory: once in the log, and on its progress row.
            let first = short && !std::mem::replace(&mut self.lock().cpu_waiting_memory, true);
            if first {
                let free = memory.map_or(0.0, |m| m.available_commit as f64 / 1_073_741_824.0);
                self.log(format!("[Memory] CPU work waits for free memory ({free:.1} GB available, 2 GB needed per new file)"));
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
                () = cancel.cancelled() => return false,
            }
        }
    }

    pub(super) async fn run_image_worker(self: Arc<Self>, worker_id: u32, cpu: Arc<CpuCtx>) {
        loop {
            let cancel = self.lock().cancel.clone();
            if !self.wait_while_paused(&cancel).await || !self.wait_for_image_memory(&cancel).await {
                self.lock().cpu_ids.remove(&worker_id);
                break;
            }
            let memory = system::memory_status();
            let claimed = {
                let mut inner = self.lock();
                if !inner.working || inner.stop_requested {
                    inner.cpu_ids.remove(&worker_id);
                    break;
                }
                // Checked again in the claim's own lock: another worker may have been let in
                // since. A worker that only waits for its stage takes nothing.
                let claim = if may_admit(&mut inner, memory.as_ref()) { inner.claim_cpu() } else { CpuClaim::Wait };
                match claim {
                    CpuClaim::File(path, stage, heavy, announce) => {
                        inner.cpu_admitted.push(std::time::Instant::now());
                        inner.images_active += 1;
                        let disposal = inner.disposal_for(&path);
                        Some((path.clone(), stage, heavy, announce, inner.lossy_for(&path), disposal, inner.job_opts()))
                    }
                    CpuClaim::Wait => None,
                    // Leaving in the same lock as the empty check: files added later start
                    // a new worker.
                    CpuClaim::Done => {
                        inner.cpu_ids.remove(&worker_id);
                        break;
                    }
                }
            };
            let Some((path, stage, heavy, announce, lossy, disposal, mut opts)) = claimed else {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(300)) => {}
                    () = cancel.cancelled() => {}
                }
                continue;
            };
            let engine = Arc::clone(&self);
            opts.busy = Some(Arc::new(move |p: &Path| engine.lock().in_progress.contains(p)));
            let engine = Arc::clone(&self);
            // The mode as the file was really done: a big picture may get the lossless pass
            // only (decided inside process_image), and must then be noted as lossless.
            // Its floor is the written file's (a WAV made into FLAC takes FLAC's).
            let (strip, refused) = (self.lock().options.strip, Arc::clone(&opts.lossless_only));
            opts.note = Some(Arc::new(move |p: &Path| {
                let target = engine.lock().target_for(p);
                engine.note_content_now(p, image::mode_at(lossy && !refused.load(Ordering::Relaxed), strip, target))
            }));
            let engine = Arc::clone(&self);
            opts.warn = Some(Arc::new(move |line: String| engine.log(line)));
            if let Some(line) = announce {
                self.log(line);
            }
            self.send_state(false);
            let progress = Arc::new(image::tools::Progress::default());
            self.lock().cpu_progress.insert(path.clone(), Arc::clone(&progress));
            let (cpu2, p2) = (Arc::clone(&cpu), path.clone());
            let lossless_only = Arc::clone(&opts.lossless_only);
            let job_cancel = self.lock().release.clone();
            let result = self
                .blocking(move || {
                    let base = cpu2.for_file(&p2);
                    let gate = image::tools::Gate {
                        cancel: job_cancel,
                        progress: Some(Arc::clone(&progress)),
                        short_of_memory: Arc::default(),
                        ..base.gate.clone()
                    };
                    let ctx2 = &image::Ctx { progress: Some(progress), gate, ..base.clone() };
                    // A slot is free unless a big job borrowed it: then wait for it back.
                    if ctx2.slots.as_ref().is_some_and(|s| !s.acquire(&ctx2.gate)) {
                        return ImageResult::Stopped;
                    }
                    let mut r = isolated(|| process_image(ctx2, &p2, worker_id, &disposal, &opts));
                    // Out of memory: wait for memory and try the file again (twice at most).
                    for _ in 0..2 {
                        if !matches!(r, ImageResult::OutOfMemory) || !wait_for_job_memory(&ctx2.gate) {
                            break;
                        }
                        ctx2.gate.short_of_memory.store(false, std::sync::atomic::Ordering::Relaxed);
                        r = isolated(|| process_image(ctx2, &p2, worker_id, &disposal, &opts));
                    }
                    if let Some(s) = &ctx2.slots {
                        s.release(1);
                    }
                    r
                })
                .await;
            self.finish_image(&path, lossy && !lossless_only.load(Ordering::Relaxed), result);
            let mut inner = self.lock();
            inner.cpu_active[stage] = inner.cpu_active[stage].saturating_sub(1);
            if heavy {
                inner.heavy_images_active = inner.heavy_images_active.saturating_sub(1);
            }
        }
    }

    fn finish_image(self: &Arc<Self>, path: &Path, lossy: bool, result: ImageResult) {
        // The floor of the file now holding the content: a WAV made into FLAC is remembered
        // at FLAC's, else the FLAC would be queued again at a lower picture floor.
        let written = match &result {
            ImageResult::Smaller { renamed: Some(new), .. } => new.as_path(),
            ImageResult::Renamed { to, .. } => to.as_path(),
            ImageResult::Duplicate { kept, .. } => kept.as_path(),
            _ => path,
        };
        let mode = {
            let inner = self.lock();
            image::mode_at(lossy, inner.options.strip, inner.target_for(written))
        };
        let mut remembered = false;
        let (mut log, mut extra) = (None, None);
        let tag = match image::kind_of(path).map(image::Kind::stage) {
            Some(0) => "[Audio]",
            Some(1) => "[Documents]",
            _ => "[Images]",
        };
        let progress = {
            let mut inner = self.lock();
            inner.images_active = inner.images_active.saturating_sub(1);
            inner.in_progress.remove(path);
            inner.cpu_started.remove(path);
            let display = inner.display_name(path);
            let kind = image::kind_of(path).unwrap_or(image::Kind::Png);
            if !matches!(result, ImageResult::Stopped | ImageResult::ToolUnavailable(_)) {
                inner.run.tools_failing[kind as usize] = 0;
            }
            if !matches!(result, ImageResult::Stopped) {
                inner.run.completed += 1;
                inner.run.images_done += 1;
                let stage = image::kind_of(path).map_or(2, image::Kind::stage);
                inner.run.stage_done[stage] += 1;
            }
            inner.cpu_progress.remove(path);
            // A GIF here that is known as a video went through the video attempt: its new
            // content counts as tried for video too (moved later, it isn't encoded again).
            let tried_as_video = args::is_gif(path) && inner.history.contains(path);
            let mut remember = |inner: &mut Inner, content: Option<(ContentHash, u64)>| {
                inner.history.add_image(path, mode);
                if let Some((hash, size)) = content {
                    inner.history.add_content(hash, size, mode);
                    if tried_as_video {
                        inner.history.add_content(hash, size, crate::history::VIDEO_MODE);
                    }
                }
                remembered = true;
            };
            match result {
                ImageResult::Smaller { before, after, lossy, score, content, renamed } => {
                    inner.run.compressed += 1;
                    inner.run.images_lossy += usize::from(lossy);
                    inner.space_saved_mb += (before - after) as f64 / (1024.0 * 1024.0);
                    remember(&mut inner, content);
                    let saved = format!("{} ({:.0}%)", format_size(before - after), 100.0 * (before - after) as f64 / before.max(1) as f64);
                    let quality = quality_note(lossy, score);
                    log = Some(match renamed {
                        Some(new) => {
                            inner.history.add_image(&new, mode);
                            inner.moved(path, &new, Some(after));
                            format!("{tag} Converted {display} to {}, saved {saved}{quality}", new.file_name().unwrap_or_default().to_string_lossy())
                        }
                        None => format!("{tag} Saved {saved} from {display}{quality}"),
                    });
                }
                ImageResult::Kept { content, note } => {
                    inner.run.kept += 1;
                    remember(&mut inner, content);
                    log = Some(match note {
                        Some(note) => format!("{tag} Kept {display}: {note}"),
                        None => format!("{tag} Kept {display} (already optimal)"),
                    });
                }
                ImageResult::Renamed { to, content } => {
                    inner.run.kept += 1;
                    inner.history.add_image(&to, mode);
                    inner.moved(path, &to, None);
                    if let Some((hash, size)) = content {
                        inner.history.add_content(hash, size, mode);
                    }
                    remembered = true;
                    log = Some(format!("{tag} Renamed {display} to {} (its real format)", to.file_name().unwrap_or_default().to_string_lossy()));
                }
                ImageResult::Duplicate { before, kept, content, replaced } => {
                    inner.run.compressed += 1;
                    inner.space_saved_mb += before as f64 / (1024.0 * 1024.0);
                    inner.history.add_image(&kept, mode);
                    inner.moved(path, &kept, content.as_ref().map(|(_, size)| *size));
                    if let Some((hash, size)) = content {
                        inner.history.add_content(hash, size, mode);
                    }
                    remembered = true;
                    let name = kept.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    log = Some(if replaced {
                        format!("{tag} {display}: {name} had the same content; it now holds the smaller version and the original was removed")
                    } else {
                        format!("{tag} {display}: {name} already has the same content and is smaller; the original was removed")
                    });
                }
                ImageResult::Skipped(why) => {
                    inner.run.skipped += 1;
                    log = Some(format!("{tag} Skipped {display}: {why}"));
                }
                ImageResult::Failed(err) => {
                    inner.run.failed += 1;
                    inner.failed.insert(path.to_path_buf());
                    log = Some(format!("{tag} Failed on {display}: {err}"));
                }
                ImageResult::ToolUnavailable(err) => {
                    inner.run.failed += 1;
                    inner.failed.insert(path.to_path_buf());
                    inner.run.tools_failing[kind as usize] += 1;
                    log = Some(format!("{tag} Failed on {display}: {err} (a tool problem, not the file's: tried again next run)"));
                    if inner.run.tools_failing[kind as usize] == TOOL_FAILURES_TO_STOP {
                        // Only this kind's files not started yet leave the queue: the others'
                        // tools may work.
                        let s = kind.stage();
                        let rest = std::mem::take(&mut inner.cpu_queues[s]);
                        let (dropped, kept): (std::collections::VecDeque<u32>, std::collections::VecDeque<u32>) =
                            rest.into_iter().partition(|&i| image::kind_of(&inner.files[i as usize]) == Some(kind));
                        inner.cpu_queues[s] = kept;
                        let bytes: u64 = dropped.iter().map(|&i| inner.file_sizes.get(i as usize).copied().unwrap_or(0)).sum();
                        inner.queue_bytes[s + 1] = inner.queue_bytes[s + 1].saturating_sub(bytes);
                        inner.queue_revision += 1;
                        let left = dropped.len();
                        inner.run.stage_total[s] = inner.run.stage_total[s].saturating_sub(left);
                        inner.run.total = inner.run.total.saturating_sub(left);
                        inner.run.images_total = inner.run.images_total.saturating_sub(left);
                        let what = match kind {
                            image::Kind::LosslessAudio => "ALAC/AIFF/APE/WavPack/TTA".to_string(),
                            image::Kind::Doc => "Office/EPUB".to_string(),
                            other => format!("{other:?}").to_uppercase(),
                        };
                        extra = Some(format!(
                            "Tools kept failing to start or finish ({TOOL_FAILURES_TO_STOP} {what} files in a row); \
                             the {left} {what} file(s) not started are left for the next run"
                        ));
                    }
                }
                ImageResult::Broken(err) => {
                    inner.run.failed += 1;
                    inner.failed.insert(path.to_path_buf());
                    remember(&mut inner, None);
                    log = Some(format!("{tag} Failed on {display}: {err} (remembered in history, will not retry)"));
                }
                ImageResult::OutOfMemory => {
                    inner.run.failed += 1;
                    inner.failed.insert(path.to_path_buf());
                    log = Some(format!("{tag} Failed on {display}: not enough memory; left as it was, tried again next run"));
                }
                // Stop, or a Pause that released the memory: back to the front of the queue
                // (it starts over on Resume).
                ImageResult::Stopped => {
                    if inner.working && !inner.stop_requested {
                        inner.requeue_cpu(path);
                        // A big GIF's GIF pass may come back here in a batch with no CPU
                        // worker (only videos, or theirs gone): one is started to take it.
                        self.spawn_workers(&mut inner);
                    }
                }
            }
            let r = &inner.run;
            (r.images_done.is_multiple_of(PROGRESS_EVERY) && r.images_done > 0)
                .then(|| format!("[Progress] {}/{} audio, documents and images done, {:.0} MB saved so far", r.images_done, r.images_total, inner.space_saved_mb))
        };
        if remembered {
            self.schedule_history_save();
        }
        for line in log.into_iter().chain(extra).chain(progress) {
            self.log(line);
        }
        self.send_state(false);
    }
}

#[cfg(test)]
use image::{big_lossy_fits, picture_threads, tool_memory, BIG_LOSSY_JOB};
use image::LOSSY_BYTES_PER_PIXEL;

/// Runs one CPU job so that a panic in it (a bug, a malformed file nobody foresaw) fails that
/// file only, not remembered: the worker, its CPU slot and the counters carry on as for any
/// failure.
fn isolated(job: impl FnOnce() -> ImageResult) -> ImageResult {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
        .unwrap_or_else(|payload| ImageResult::Failed(format!("internal error: {}", panic_text(payload.as_ref()))))
}

/// A panic's message.
pub(super) fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// CPU files in a row whose tools could not run before the rest of the CPU queue is left for
/// the next run (a quarantined or broken tool would otherwise fail every file).
const TOOL_FAILURES_TO_STOP: usize = 20;

/// Before a CPU job that ran out of memory is tried again: up to a minute for
/// `IMAGE_MIN_FREE_COMMIT` twice over. False on Stop (or a Pause that released the job).
fn wait_for_job_memory(gate: &image::tools::Gate) -> bool {
    for _ in 0..60 {
        if !gate.wait_while_paused() {
            return false;
        }
        if system::memory_status().is_none_or(|m| m.available_commit >= 2 * IMAGE_MIN_FREE_COMMIT) {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    !gate.is_cancelled()
}

/// Kept free on a drive that holds work folders (C: is often nearly full).
const DISK_RESERVE: u64 = 4 * 1024 * 1024 * 1024;

/// Space reserved on each drive by the jobs running now (by drive root), so several big
/// files at once can't fill a drive between their free-space checks.
static RESERVED: std::sync::Mutex<Vec<(PathBuf, u64)>> = std::sync::Mutex::new(Vec::new());

/// The drive (or share) root of `path`, e.g. `F:\`.
pub(super) fn volume_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().last().filter(|r| r.has_root()).map(Path::to_path_buf)
}

/// Space a job holds on a drive while it runs (a video's new version written next to its
/// original), counted in `RESERVED` with the work folders' so jobs at once share the space.
pub(super) struct DiskReservation {
    root: PathBuf,
    bytes: u64,
}

impl DiskReservation {
    /// Reserves `bytes` on `path`'s drive if its free space, less what other jobs reserved
    /// there, leaves `keep` more. `Err(free)`: the drive's free space for them now.
    pub(super) fn take(path: &Path, bytes: u64, keep: u64) -> Result<Self, u64> {
        let Some(root) = volume_root(path) else { return Ok(Self { root: PathBuf::new(), bytes: 0 }) };
        let mut reserved = RESERVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let taken: u64 = reserved.iter().filter(|(r, _)| *r == root).map(|(_, n)| n).sum();
        if let Some(free) = system::free_space(path) {
            if free < taken + bytes + keep {
                return Err(free.saturating_sub(taken));
            }
        }
        reserved.push((root.clone(), bytes));
        Ok(Self { root, bytes })
    }
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        let mut reserved = RESERVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = reserved.iter().position(|(r, n)| *r == self.root && *n == self.bytes) {
            reserved.swap_remove(i);
        }
    }
}

/// Hidden folder on a file's own drive for its work files when the temp drive is short on
/// space (its name starts with a dot, so scans skip it).
pub(super) const DRIVE_WORK_DIR: &str = ".mediaopti-temp";

/// The private work folder of one job, deleted however `process_image` returns, with its
/// disk-space reservation.
struct WorkDir {
    path: PathBuf,
    root: PathBuf,
    reserved: u64,
}

impl WorkDir {
    /// A work folder for a file of `size` bytes: in the temp folder, or on the file's own
    /// drive when the temp drive hasn't room for `factor` times the file (copy, candidates,
    /// output; a PDF's streams are written out decompressed).
    fn new(path: &Path, size: u64, factor: u64, temp: &Path, name: &str) -> Result<Self, String> {
        let need = size.saturating_mul(factor).saturating_add(64 << 20);
        let mut places = vec![temp.to_path_buf()];
        if let Some(root) = volume_root(path) {
            places.push(root.join(DRIVE_WORK_DIR));
        }
        let mut reserved = RESERVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for place in places {
            let Some(root) = volume_root(&place) else { continue };
            let taken: u64 = reserved.iter().filter(|(r, _)| *r == root).map(|(_, n)| n).sum();
            let fits = system::free_space(&root).is_none_or(|free| free >= taken + need + DISK_RESERVE);
            if fits && std::fs::create_dir_all(place.join(name)).is_ok() {
                if place.ends_with(DRIVE_WORK_DIR) {
                    system::set_hidden(&place);
                }
                reserved.push((root.clone(), need));
                return Ok(Self { path: place.join(name), root, reserved: need });
            }
        }
        Err("not enough free disk space for its temporary files (it will be tried next time)".into())
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        if let Some(parent) = self.path.parent().filter(|p| p.ends_with(DRIVE_WORK_DIR)) {
            let _ = std::fs::remove_dir(parent); // only if empty
        }
        let mut reserved = RESERVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = reserved.iter().position(|(r, n)| *r == self.root && *n == self.reserved) {
            reserved.swap_remove(i);
        }
    }
}

/// The maximum-effort parts in use (Advanced options). Benchmarks can pick parts with the
/// `MEDIAOPTI_MAX_PARTS` environment variable (comma-separated `Max` field names; test builds only).
fn max_parts(options: &crate::args::Options) -> image::Max {
    if cfg!(test) {
        if let Ok(names) = std::env::var("MEDIAOPTI_MAX_PARTS") {
            return image::Max::from_names(&names.split(',').map(str::trim).collect::<Vec<_>>());
        }
    }
    options.max
}

/// "38 B", "812 KB", "4.5 MB", "1.2 GB".
fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", (bytes as f64 / 1024.0).min(1023.0))
    } else {
        format!("{bytes} B")
    }
}

fn content_of(path: &Path) -> Option<(ContentHash, u64)> {
    let size = std::fs::metadata(path).ok()?.len();
    Some((history::hash_file(path).ok()?, size))
}

/// Optimizes one image and, if worthwhile, replaces the original. Blocking.
/// Per-run settings of the CPU jobs besides the image pipeline's.
/// Answers "is another worker processing this file now?".
pub(super) type BusyCheck = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

pub(super) struct JobOpts {
    /// Where work folders go first.
    pub temp: PathBuf,
    /// Rename pictures whose extension doesn't match their format.
    pub fix_ext: bool,
    /// Whether another worker is processing a file now (a duplicate must not be replaced or
    /// removed under it).
    pub busy: Option<BusyCheck>,
    /// Records a new version's content in the history before it replaces an original.
    pub note: Option<NoteContent>,
    /// Logs a problem that doesn't change the result (dates that could not be restored).
    pub warn: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// Set when the file got the lossless pass only (a lossy search too big for the free
    /// memory): it is remembered as done losslessly, so a later run can try the lossy pass.
    pub lossless_only: Arc<std::sync::atomic::AtomicBool>,
}

/// Restores a replaced file's dates and flags, logging (through `opts.warn`) when it can't.
fn restore_or_warn(target: &Path, attrs: &system::OriginalAttributes, opts: &JobOpts) {
    if let Err(err) = system::restore_attributes(target, attrs) {
        if let Some(warn) = &opts.warn {
            warn(format!("Could not restore the dates and attributes of {}: {err}", target.display()));
        }
    }
}

pub(super) type NoteContent = Arc<dyn Fn(&Path) -> std::io::Result<(ContentHash, u64)> + Send + Sync>;

/// Whether `existing` holds the same content as the original (`src`, its private copy):
/// the same picture with the same metadata, the same decoded audio with the same tags and
/// cover art, a CBZ with the CBR's files, else the same bytes. Only then may one of the two
/// go: whichever stays must hold everything the other has.
fn same_content(ctx: &image::Ctx, kind: image::Kind, src: &Path, existing: &Path, work: &Path) -> Result<bool, RunError> {
    let copy = work.join(format!("existing.{}", args::ext_lower(existing)));
    if std::fs::copy(existing, &copy).is_err() {
        return Ok(false);
    }
    match args::ext_lower(existing).as_str() {
        "flac" => {
            let want = crate::audio::audio_hash(ctx, src)?;
            Ok(want.is_some() && crate::audio::audio_hash(ctx, &copy)? == want && crate::audio::same_tags_and_art(ctx, src, &copy)?)
        }
        "cbz" if kind == image::Kind::Cbr => crate::archive::same_as_folder(ctx, &work.join("x"), &copy, work),
        _ if image::real_picture_ext(&copy).is_some() && image::real_picture_ext(src).is_some() => {
            Ok(image::same_metadata(ctx, src, &copy) && image::same_picture(ctx, src, &copy)?)
        }
        _ => Ok(history::hash_file(src).ok().is_some_and(|h| history::hash_file(&copy).ok() == Some(h))),
    }
}

/// Optimizes one image (or audio file, document) and, if worthwhile, replaces the original.
/// Conversions and extension fixes save under the new name; if that name is taken by a file
/// with the same content, the smaller of the two is kept (the other and the original go
/// as `disposal` says), else the new file gets a number ("a (2).png"). Blocking.
pub(super) fn process_image(ctx: &image::Ctx, path: &Path, worker_id: u32, disposal: &system::Disposal, opts: &JobOpts) -> ImageResult {
    system::panic_here_in_tests(path);
    // Decided anew by each attempt (an attempt retried after waiting for memory may fit).
    opts.lossless_only.store(false, Ordering::Relaxed);
    static NEXT_JOB: AtomicU64 = AtomicU64::new(0);
    let Some(named) = image::kind_of(path) else {
        return ImageResult::Skipped("not an image".into());
    };
    let ext = args::ext_lower(path);
    // A misnamed picture (a JPEG called .png) is optimized as what it is.
    let pictures = [image::Kind::Png, image::Kind::Jpg, image::Kind::Gif];
    let kind = if pictures.contains(&named) { image::sniff_picture(path).filter(|k| pictures.contains(k)).unwrap_or(named) } else { named };
    // ...and renamed to its real format if asked (also WebP and BMP: renamed, not optimized).
    let fix_to = if opts.fix_ext && matches!(named, image::Kind::Png | image::Kind::Jpg | image::Kind::Gif | image::Kind::Bmp) {
        image::real_picture_ext(path).filter(|real| !image::ext_is(real, &ext))
    } else {
        None
    };
    let Some(state) = file_state(path) else {
        return ImageResult::Failed("could not read the file".into());
    };
    if system::hard_link_count(path).unwrap_or(1) > 1 {
        return ImageResult::Skipped("it is a hard link (several names for one file)".into());
    }
    if let Some(tool) = image::missing_tool(ctx, kind) {
        return ImageResult::ToolUnavailable(format!("{tool} is missing"));
    }
    // Most music has nothing to try: don't copy (to C:) or decode it.
    if kind.is_audio() && !crate::audio::worth_trying(ctx, kind, path) {
        if ctx.gate.short_of_memory.load(std::sync::atomic::Ordering::Relaxed) {
            return ImageResult::OutOfMemory;
        }
        return ImageResult::Kept { content: content_of(path), note: None };
    }
    // PDF pictures keep the default floor: the picture setting's lower ones are for pictures
    // (a page must stay readable).
    let pdf_ctx;
    let ctx = if kind == image::Kind::Pdf && ctx.target < f64::from(image::DEFAULT_TARGET) {
        pdf_ctx = image::Ctx { target: f64::from(image::DEFAULT_TARGET), ..ctx.clone() };
        &pdf_ctx
    } else {
        ctx
    };
    let pictured = matches!(kind, image::Kind::Png | image::Kind::Jpg | image::Kind::Bmp | image::Kind::Tga);
    // Without its tools the lossy part can't run: lossless only, noted as lossless (said once
    // at the start of the batch, in the tools line).
    let no_lossy_tools = image::has_lossy_part(ctx, kind) && !image::lossy_tools_present(ctx, kind);
    let lossy_need = if ctx.lossy && pictured && !no_lossy_tools {
        image::pixel_count(path).map_or(0, |px| px.saturating_mul(LOSSY_BYTES_PER_PIXEL))
    } else {
        0
    };
    let lossless_ctx;
    let forced_lossless = system::lossless_only_in_tests(path);
    let ctx = if ctx.lossy && (forced_lossless || no_lossy_tools) {
        opts.lossless_only.store(true, Ordering::Relaxed);
        if let Some(warn) = opts.warn.as_ref().filter(|_| forced_lossless) {
            warn(format!(
                "{}: its visually lossless pass needs about {:.0} GB of free memory; lossless only this time",
                path.display(),
                lossy_need as f64 / 1_073_741_824.0
            ));
        }
        lossless_ctx = image::Ctx { lossy: false, ..ctx.clone() };
        &lossless_ctx
    } else {
        ctx
    };
    // Standalone pictures use every core (oxipng/ECT compute and promise their thread budget
    // only while `png_chain` runs; JPEGs don't run oxipng/ECT-mt and allow side-by-side lossy
    // methods). Documents and comic pages keep `threads: 1`.
    let threaded_ctx;
    let ctx = if pictured && ctx.threads == 1 {
        threaded_ctx = image::Ctx { threads: 0, ..ctx.clone() };
        &threaded_ctx
    } else {
        ctx
    };
    // Pictures carrying trailing data (motion photo, Ultra HDR, MPO, appended archive) are
    // skipped before creating a temp folder or copying the file to C:.
    if matches!(kind, image::Kind::Png | image::Kind::Jpg | image::Kind::Gif) && image::has_data_after_end(kind, path) {
        return ImageResult::Kept {
            content: content_of(path),
            note: Some("it has data after the end of the picture (a motion photo, second picture or gain map): kept as it is".into()),
        };
    }
    let job = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    // FLAC, MP3, Ogg, and short ASCII pictures with matching extension are read directly where
    // they are (nothing writes to `src`: in-place tools copy to `work` first, write-to tools
    // write into `work`), saving millions of redundant temp copies on C:.
    let ascii_short = path.as_os_str().len() < 248 && path.to_str().is_some_and(str::is_ascii);
    let in_place = (matches!(kind, image::Kind::Flac | image::Kind::Mp3 | image::Kind::Ogg | image::Kind::Opus) && path.as_os_str().len() < 248)
        || (matches!(kind, image::Kind::Png | image::Kind::Jpg | image::Kind::Gif) && kind == named && ascii_short);
    let factor = match kind {
        image::Kind::Pdf => 16,
        _ if matches!(kind, image::Kind::Flac | image::Kind::Mp3 | image::Kind::Ogg | image::Kind::Opus) && in_place => 2,
        _ => 4,
    };
    let workdir = match WorkDir::new(path, state.0, factor, &opts.temp, &format!("mediaopti-img-{}-{job}", std::process::id())) {
        Ok(w) => w,
        Err(why) => return ImageResult::Skipped(why),
    };
    let work = workdir.path.clone();
    // Otherwise a copy with an ASCII name: some tools can't open Unicode paths. A misnamed
    // picture's copy gets its real extension (tools pick the format from it).
    let src = if in_place {
        path.to_path_buf()
    } else {
        let copy = work.join(format!("orig.{}", if kind == named { ext.as_str() } else { image::ext_of(kind) }));
        if let Err(err) = std::fs::copy(path, &copy) {
            return ImageResult::Failed(format!("could not copy it to the temp folder: {err}"));
        }
        copy
    };
    let before = state.0;

    let optimized = image::optimize(ctx, kind, &src, &work);
    if matches!(optimized, Err(RunError::Cancelled)) {
        return ImageResult::Stopped;
    }
    if ctx.gate.short_of_memory.load(std::sync::atomic::Ordering::Relaxed) {
        return ImageResult::OutOfMemory;
    }
    let result = match optimized {
        Ok(r) => r,
        Err(RunError::Cancelled) => return ImageResult::Stopped,
        // The file itself couldn't be processed (a tool failed on it): remembered, like a
        // video whose encode failed, so it isn't retried on every run.
        Err(RunError::Failed(err)) => return ImageResult::Broken(err),
        Err(RunError::Unavailable(err)) => return ImageResult::ToolUnavailable(err),
    };
    if result.lossless_only {
        opts.lossless_only.store(true, Ordering::Relaxed);
        if let Some(warn) = &opts.warn {
            warn(format!(
                "{}: its visually lossless pass needs about {:.0} GB of free memory; lossless only this time",
                path.display(),
                lossy_need as f64 / 1_073_741_824.0
            ));
        }
    }
    // At least 0.5% smaller. Audio also counts 64 KiB: cover art is a small part of a
    // song, but its saving is lossless and verified, and adds up over a music library.
    let min_gain = if kind.is_audio() { (before / 200).min(crate::audio::MIN_GAIN) } else { before / 200 };
    let smaller = result.best.as_ref().map(|b| (b.clone(), std::fs::metadata(b).map_or(before, |m| m.len()))).filter(|(_, after)| before.saturating_sub(*after) >= min_gain.max(1));
    // The new name: a conversion's (only when it is smaller), or the real format's.
    let new_ext = match (&smaller, result.new_ext) {
        (Some(_), Some(e)) => Some(e),
        (_, Some(_)) => None, // a conversion that isn't smaller isn't made
        (_, None) => fix_to,
    };
    if smaller.is_none() && new_ext.is_none() {
        // A cut-off picture is "kept" too, but not because it is already optimal.
        let note = result.note.or_else(|| {
            image::looks_cut_off(kind, path).then(|| "it looks cut off (damaged: its data ends early); left as it is".to_string())
        });
        return ImageResult::Kept { content: content_of(path), note };
    }
    if file_state(path) != Some(state) {
        return ImageResult::Skipped("it changed while it was being optimized (it will be processed next time)".into());
    }
    let mut target = new_ext.map_or_else(|| path.to_path_buf(), |e| path.with_extension(e));
    // The new name is claimed atomically (a.bmp and a.tga running at once can't both become
    // a.png). If a file has it already: the same content keeps the smaller copy, other content
    // makes this one "a (2).png".
    if target != path {
        let mut tries = 0;
        while system::claim_new_name(&target).is_err() {
            tries += 1;
            if tries == 1 {
                let same = match same_content(ctx, kind, &src, &target, &work) {
                    Ok(s) => s,
                    Err(RunError::Cancelled) => return ImageResult::Stopped,
                    Err(RunError::Failed(e)) => return ImageResult::Failed(e),
                    Err(RunError::Unavailable(e)) => return ImageResult::ToolUnavailable(e),
                };
                if same {
                    // The other copy is being processed right now (a.wav and a.flac in one
                    // batch): leave both; they are compared again next time.
                    if opts.busy.as_ref().is_some_and(|busy| busy(&target)) {
                        let name = target.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        return ImageResult::Skipped(format!("{name} has the same content and is being processed at the same time; compared next time"));
                    }
                    return keep_smaller_copy(path, &target, smaller.as_ref(), before, disposal, worker_id, opts);
                }
            }
            if tries > 20 {
                return ImageResult::Kept { content: content_of(path), note: Some("no free name for the converted file".into()) };
            }
            target = system::unused_name(&target);
        }
    }
    let claimed = (target != path).then(|| target.clone());
    let unclaim = || {
        if let Some(t) = &claimed {
            system::release_claim(t);
        }
    };

    let Some((best, after)) = smaller else {
        // Only renamed (the extension fixed, nothing smaller): the file itself moves.
        return match std::fs::rename(path, &target) {
            Ok(()) => ImageResult::Renamed { to: target.clone(), content: content_of(&target) },
            Err(err) => {
                unclaim();
                ImageResult::Failed(format!("could not rename it: {err}"))
            }
        };
    };
    // The new version goes next to the original first (same volume), then replaces it.
    let stem = path.file_stem().map_or_default(|s| s.to_string_lossy().into_owned());
    let out_ext = args::ext_lower(&target);
    let sibling = super::replace::unclaimed_temp(|n| path.with_file_name(format!("{stem}.mediaopti.i{n}.{out_ext}")), worker_id);
    let attrs = match std::fs::metadata(path) {
        Ok(m) => system::OriginalAttributes::from_metadata(&m),
        Err(err) => {
            unclaim();
            return ImageResult::Failed(err.to_string());
        }
    };
    if let Err(err) = std::fs::copy(&best, &sibling) {
        let _ = system::remove_file_if_exists(&sibling);
        unclaim();
        return ImageResult::Failed(format!("could not write next to the original: {err}"));
    }
    let noted = std::sync::Mutex::new(None::<(ContentHash, u64)>);
    if let Err(err) = super::replace_original(&sibling, &target, path, disposal, &|p| note(opts, p, &noted)) {
        unclaim();
        return ImageResult::Failed(err);
    }
    restore_or_warn(&target, &attrs, opts);
    let lossy = result.steps.iter().any(|s| s.contains('='));
    let score = image::lowest_score(&result.steps).filter(|_| lossy);
    let renamed = (target != path).then(|| target.clone());
    let content = noted.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner).or_else(|| content_of(&target));
    ImageResult::Smaller { before, after, lossy, score, content, renamed }
}

/// `existing` (the new name's current file) has the same content as `path`: keeps the smaller
/// of `existing` and the new version (`smaller`, if there is one); the original and the other
/// copy go as `disposal` says.
fn note(opts: &JobOpts, file: &Path, out: &std::sync::Mutex<Option<(ContentHash, u64)>>) -> std::io::Result<()> {
    if let Some(n) = &opts.note {
        *out.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(n(file)?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn keep_smaller_copy(path: &Path, existing: &Path, smaller: Option<&(PathBuf, u64)>, before: u64, disposal: &system::Disposal, worker_id: u32, opts: &JobOpts) -> ImageResult {
    let theirs = std::fs::metadata(existing).map_or(u64::MAX, |m| m.len());
    // A hard-linked copy is never replaced (that would break the link): both are kept then.
    let replaceable = system::hard_link_count(existing).unwrap_or(1) <= 1;
    match smaller {
        Some((best, after)) if *after < theirs && replaceable => {
            // Ours is smaller: it replaces the existing copy, then the original goes.
            let stem = existing.file_stem().map_or_default(|s| s.to_string_lossy().into_owned());
            let ext = args::ext_lower(existing);
            let sibling = super::replace::unclaimed_temp(|n| existing.with_file_name(format!("{stem}.mediaopti.i{n}.{ext}")), worker_id);
            let attrs = std::fs::metadata(path).map(|m| system::OriginalAttributes::from_metadata(&m));
            if let Err(err) = std::fs::copy(best, &sibling) {
                let _ = system::remove_file_if_exists(&sibling);
                return ImageResult::Failed(format!("could not write next to the original: {err}"));
            }
            let noted = std::sync::Mutex::new(None::<(ContentHash, u64)>);
            if let Err(err) = super::replace_original(&sibling, existing, existing, &disposal.for_sibling(existing), &|p| note(opts, p, &noted)) {
                return ImageResult::Failed(err);
            }
            if let Ok(a) = attrs {
                restore_or_warn(existing, &a, opts);
            }
            let content = noted.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner).or_else(|| content_of(existing));
            match system::dispose(path, disposal) {
                Ok(()) => ImageResult::Duplicate { before: before + theirs - after, kept: existing.to_path_buf(), content, replaced: true },
                Err(err) => ImageResult::Failed(format!("{} now has the smaller version, but the original could not be removed: {err}", existing.display())),
            }
        }
        _ if theirs <= smaller.map_or(before, |s| s.1) => match system::dispose(path, disposal) {
            Ok(()) => ImageResult::Duplicate { before, kept: existing.to_path_buf(), content: content_of(existing), replaced: false },
            Err(err) => ImageResult::Failed(format!("could not remove it (same content as {}): {err}", existing.display())),
        },
        // Same content, but the existing copy is bigger than the original and nothing new
        // was made: leave both.
        _ => ImageResult::Kept { content: content_of(path), note: Some(format!("{} has the same content but is bigger; both kept", existing.display())) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When many heavy images (>= 4 MiB) sit at the front of the queue, at most
    /// `max_heavy_images(slots)` run at once while lighter images are available at the back;
    /// once no light images remain, workers still take remaining heavy ones.
    #[test]
    fn heavy_images_pair_with_light_images_in_the_queue() {
        struct Quiet;
        impl crate::engine::Ui for Quiet {
            fn log(&self, _: String) {}
            fn state(&self, _: &crate::engine::StateDto) {}
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let data = std::env::temp_dir().join(format!("mo-heavy-pair-{}", std::process::id()));
        let engine = Engine::new(Box::new(Quiet), &data, PathBuf::from("ffmpeg.exe"), rt.handle().clone());
        {
            let mut inner = engine.lock();
            let slots = 15;
            let ctx = engine.image_ctx(&inner.options, slots, tokio_util::sync::CancellationToken::new());
            inner.cpu = Some(Arc::new(CpuCtx::new(ctx, &inner.options, slots)));
            // Sorted largest-first: 5 heavy (20..=16 MiB), then 4 light (500..=200 KiB).
            for i in 0..5u64 {
                inner.add_file(PathBuf::from(format!("heavy{i}.png")), (20 - i) * 1024 * 1024);
            }
            for i in 0..4u64 {
                inner.add_file(PathBuf::from(format!("light{i}.png")), (500 - i * 100) * 1024);
            }
            inner.rebuild_queue();

            let mut claimed = Vec::new();
            while let CpuClaim::File(p, _, heavy, _) = inner.claim_cpu() {
                claimed.push((p.file_name().unwrap().to_string_lossy().into_owned(), heavy));
            }
            // First 3 claims = max_heavy_images(15) = 3 heavy images from the front.
            assert_eq!(claimed[..3], [
                ("heavy0.png".into(), true),
                ("heavy1.png".into(), true),
                ("heavy2.png".into(), true),
            ]);
            // Next 4 claims = light images from the back while heavy cap is full.
            assert_eq!(claimed[3..7], [
                ("light3.png".into(), false),
                ("light2.png".into(), false),
                ("light1.png".into(), false),
                ("light0.png".into(), false),
            ]);
            // Remaining 2 claims = remaining heavy images (no light files left, never idle).
            assert_eq!(claimed[7..], [
                ("heavy3.png".into(), true),
                ("heavy4.png".into(), true),
            ]);
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Threads by memory, never a drop from all to one: the 31 MP picture measured runs on
    /// every core; a 100 MP one gets as many as fit; only a real shortage leaves one.
    #[test]
    fn picture_threads_follow_the_memory() {
        const GB: u64 = 1 << 30;
        assert_eq!(picture_threads(31_200_000, 16, Some(4 * GB)), 0, "1.3 GB past the reserve: every core");
        assert_eq!(picture_threads(31_200_000, 16, Some(5 * GB / 2)), 1, "only 0.5 GB past it: one thread's base");
        assert_eq!(tool_memory(31_200_000, 16) / (1 << 20), 1286, "about the 1.33 GB measured");
        assert_eq!(tool_memory(u64::MAX, 200), u64::MAX, "a damaged header can't overflow");
        assert_eq!(picture_threads(100_000_000, 16, Some(6 * GB)), 15, "4 GB past the reserve");
        assert_eq!(picture_threads(100_000_000, 16, Some(12 * GB)), 0, "enough for all");
        assert_eq!(picture_threads(100_000_000, 16, Some(3 * GB)), 1, "less than one thread's base");
        assert_eq!(picture_threads(100_000_000, 16, None), 0, "unknown memory: as the tools default");
        let some = picture_threads(160_000_000, 16, Some(8 * GB));
        assert!((2..16).contains(&some), "{some}");
    }

    /// Workers waking together used to see the same free memory and all start: each one let
    /// in a moment ago now raises what the next needs, a little.
    #[test]
    fn a_burst_of_cpu_jobs_is_spread() {
        assert_eq!(image_admission_need(0), IMAGE_MIN_FREE_COMMIT);
        assert_eq!(image_admission_need(4), IMAGE_MIN_FREE_COMMIT + 4 * ADMISSION_RESERVE);
        // A steady run of ~1 s files on 14 slots needs ~9 GB, not 30.
        assert!(image_admission_need(14) < 10 << 30);
    }

    /// Jobs on one drive share its free space: a second reservation of most of it waits,
    /// and a reservation's space comes back when it ends.
    #[test]
    fn disk_reservations_share_a_drive() {
        let dir = std::env::temp_dir();
        let free = system::free_space(&dir).unwrap();
        let most = free / 10 * 6;
        let first = DiskReservation::take(&dir, most, 0).unwrap();
        assert!(DiskReservation::take(&dir, most, 0).is_err(), "the drive is already 60% promised");
        drop(first);
        assert!(DiskReservation::take(&dir, most, 0).is_ok());
    }

    /// The 164-megapixel JPEG of the 2026-11-17 corpus needs ~23 GB for its lossy search: with
    /// the ~8 GB of commit this machine has free at idle it gets the lossless pass only; with
    /// enough free it runs. Unknown free memory doesn't stop it.
    #[test]
    fn big_lossy_searches_need_their_memory_free() {
        let need = std::hint::black_box(13_481 * 12_136) * LOSSY_BYTES_PER_PIXEL;
        assert!(need > 22 << 30, "a big job");
        assert!(!big_lossy_fits(need, Some(8 << 30)));
        assert!(big_lossy_fits(need, Some(40 << 30)));
        assert!(big_lossy_fits(need, None));
        // A 24-megapixel photo is no big job.
        assert!(std::hint::black_box(6000 * 4000) * LOSSY_BYTES_PER_PIXEL < BIG_LOSSY_JOB);
    }

    /// A job too big for the temp drive's free space works on the file's own drive, in a
    /// hidden folder that disappears with the job. (Only a reservation: nothing is written.)
    #[test]
    fn work_folder_moves_to_the_files_drive_when_temp_is_short() {
        let file = Path::new(r"D:\mediaopti-test\some file.wav");
        // The temp drive is made short for this thread (2 GB), whatever the machine has:
        // the test used to skip once C: had room (2026-11-18).
        let Some(temp_root) = volume_root(&std::env::temp_dir()) else {
            skip_test!("the temp folder has no drive root");
            return;
        };
        system::FREE_SPACE_ON.with_borrow_mut(|fake| fake.push((temp_root.clone(), 2 << 30)));
        let (Some(temp_free), Some(d_free)) = (system::free_space(&temp_root), system::free_space(Path::new(r"D:\"))) else {
            skip_test!("no D: drive");
            return;
        };
        // 4x this must exceed the temp drive's free space but fit on D:.
        let size = temp_free / 4 + (1 << 30);
        if volume_root(&std::env::temp_dir()) == volume_root(file) || size * 4 + DISK_RESERVE + (1 << 30) > d_free {
            skip_test!("needs a temp drive with less free space than D:");
            return;
        }
        let w = WorkDir::new(file, size, 4, &std::env::temp_dir(), "mediaopti-img-test-0").expect("fits on D:");
        assert!(w.path.starts_with(r"D:\.mediaopti-temp"), "{}", w.path.display());
        #[cfg(windows)]
        assert!(std::os::windows::fs::MetadataExt::file_attributes(&std::fs::metadata(r"D:\.mediaopti-temp").unwrap()) & 0x2 != 0, "hidden");
        // A second one doesn't fit on either drive while the first holds its reservation.
        assert!(WorkDir::new(file, d_free / 4, 4, &std::env::temp_dir(), "mediaopti-img-test-1").is_err());
        drop(w);
        assert!(!Path::new(r"D:\.mediaopti-temp").exists(), "removed when empty");
    }

    #[test]
    fn format_size_scales_down_to_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(38), "38 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1 KB");
        assert_eq!(format_size(831_488), "812 KB");
        assert_eq!(format_size(1024 * 1024 - 1), "1023 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }
}
