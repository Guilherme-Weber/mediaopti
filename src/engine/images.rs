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



/// Log a progress line every this many images (one line per image would flood the log).
const PROGRESS_EVERY: usize = 250;

pub(super) enum ImageResult {
    Smaller {
        before: u64,
        after: u64,
        /// A lossy encoder won (quality search labels look like `beeim-quantizer=81`).
        lossy: bool,
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
        let audio = image::Ctx { lossy: options.audio == crate::args::ImageMode::Lossy, ..image.clone() };
        Self { image, audio, slots }
    }

    fn for_file(&self, path: &Path) -> &image::Ctx {
        if image::kind_of(path).is_some_and(image::Kind::is_audio) { &self.audio } else { &self.image }
    }
}

/// How many CPU files of each stage run at once with `slots` CPU slots: audio half the slots
/// (decoding is one core per file; the files are read once, sequentially), documents half the
/// slots (a PDF's renders and qpdf use memory; comic pages borrow the idle slots), images every
/// slot.
fn stage_limits(slots: usize) -> [usize; 3] {
    let half = (slots / 2).max(2);
    [half, half, usize::MAX]
}

enum CpuClaim {
    /// A file, its stage, and the log line announcing a new stage.
    File(PathBuf, usize, Option<String>),
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
        }
    }

    /// The next CPU file, by stage (rolling): nothing while videos are still to be started,
    /// then audio (at most `STAGE_LIMITS[0]` at once), then documents, then images. A stage
    /// begins as soon as the one before has nothing left to start.
    fn claim_cpu(&mut self) -> CpuClaim {
        let videos_waiting = self.queue_cursor < self.pending_queue.len()
            && !self.video_ids.is_empty()
            && !self.fatal.load(std::sync::atomic::Ordering::SeqCst);
        if videos_waiting {
            return CpuClaim::Wait;
        }
        let limits = stage_limits(self.cpu.as_ref().map_or(2, |c| c.slots));
        for (stage, limit) in limits.into_iter().enumerate() {
            let queue = &self.cpu_queues[stage];
            let mut cursor = self.cpu_cursors[stage];
            while cursor < queue.len() && (self.in_progress.contains(&queue[cursor]) || self.failed.contains(&queue[cursor])) {
                cursor += 1;
            }
            self.cpu_cursors[stage] = cursor;
            if cursor == self.cpu_queues[stage].len() {
                continue;
            }
            if self.cpu_active[stage] >= limit {
                return CpuClaim::Wait;
            }
            let file = self.cpu_queues[stage][cursor].clone();
            self.cpu_cursors[stage] += 1;
            self.cpu_active[stage] += 1;
            self.in_progress.insert(file.clone());
            self.cpu_started.insert(file.clone(), std::time::Instant::now());
            let announce = (!self.stage_logged[stage]).then(|| {
                self.stage_logged[stage] = true;
                let left = self.cpu_queues[stage].len() - cursor;
                let (what, how) = match stage {
                    0 => ("audio files", format!("{} at a time", limits[0])),
                    1 => ("documents", format!("{} at a time, comic pages on idle cores", limits[1])),
                    _ => ("images", "on every free core".to_string()),
                };
                format!("Now processing {left} {what}, {how}")
            });
            return CpuClaim::File(file, stage, announce);
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
            target: 93.0,
            lossy_min_gain: 0.03,
            strip: options.strip,
            strict_alpha: false,
            lossy_docs: options.lossy_docs,
            parallel: slots > 1,
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
        let (ctx, disposal, lossy, opts) = {
            let inner = self.lock();
            (
                Arc::new(self.image_ctx(&inner.options, 2, inner.cancel.clone())),
                inner.disposal_for(input),
                inner.images_lossy(),
                inner.job_opts(),
            )
        };
        self.reset_worker(worker_id, display, "optimizing as GIF");
        self.send_state(false);
        let p = input.to_path_buf();
        // Video worker ids are 1-4; image-temp names use a separate range.
        let result = self.blocking(move || process_image(&ctx, &p, worker_id + 100, &disposal, &opts)).await;
        self.lock().workers.remove(&worker_id);
        let outcome = match &result {
            ImageResult::Stopped => super::Outcome::Stopped,
            ImageResult::Failed(_) => super::Outcome::Failed,
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
        self.finish_image(input, lossy, result);
        outcome
    }

    /// Waits until starting another image is safe for memory. One image worker always may
    /// run (so a low-memory machine still makes progress). Returns false on Stop.
    async fn wait_for_image_memory(&self, cancel: &tokio_util::sync::CancellationToken) -> bool {
        loop {
            let busy = self.lock().images_active;
            let memory = system::memory_status();
            if busy == 0 || memory.is_none_or(|m| m.available_commit >= IMAGE_MIN_FREE_COMMIT) {
                self.lock().cpu_waiting_memory = false;
                return true;
            }
            // Shown like a video waiting for memory: once in the log, and on its progress row.
            let first = !std::mem::replace(&mut self.lock().cpu_waiting_memory, true);
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
            let claimed = {
                let mut inner = self.lock();
                if !inner.working || inner.stop_requested {
                    inner.cpu_ids.remove(&worker_id);
                    break;
                }
                match inner.claim_cpu() {
                    CpuClaim::File(path, stage, announce) => {
                        inner.images_active += 1;
                        let disposal = inner.disposal_for(&path);
                        Some((path.clone(), stage, announce, inner.lossy_for(&path), disposal, inner.job_opts()))
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
            let Some((path, stage, announce, lossy, disposal, mut opts)) = claimed else {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(300)) => {}
                    () = cancel.cancelled() => {}
                }
                continue;
            };
            let engine = Arc::clone(&self);
            opts.busy = Some(Arc::new(move |p: &Path| engine.lock().in_progress.contains(p)));
            let engine = Arc::clone(&self);
            let mode = image::mode(lossy, self.lock().options.strip);
            opts.note = Some(Arc::new(move |p: &Path| engine.note_content_now(p, mode)));
            if let Some(line) = announce {
                self.log(line);
            }
            self.send_state(false);
            let progress = Arc::new(image::tools::Progress::default());
            self.lock().cpu_progress.insert(path.clone(), Arc::clone(&progress));
            let (cpu2, p2) = (Arc::clone(&cpu), path.clone());
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
                    let mut r = process_image(ctx2, &p2, worker_id, &disposal, &opts);
                    // Out of memory: wait for memory and try the file again (twice at most).
                    for _ in 0..2 {
                        if !matches!(r, ImageResult::OutOfMemory) || !wait_for_job_memory(&ctx2.gate) {
                            break;
                        }
                        ctx2.gate.short_of_memory.store(false, std::sync::atomic::Ordering::Relaxed);
                        r = process_image(ctx2, &p2, worker_id, &disposal, &opts);
                    }
                    if let Some(s) = &ctx2.slots {
                        s.release(1);
                    }
                    r
                })
                .await;
            self.finish_image(&path, lossy, result);
            let mut inner = self.lock();
            inner.cpu_active[stage] = inner.cpu_active[stage].saturating_sub(1);
        }
    }

    fn finish_image(self: &Arc<Self>, path: &Path, lossy: bool, result: ImageResult) {
        let mode = image::mode(lossy, self.lock().options.strip);
        let mut remembered = false;
        let mut log = None;
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
            if !matches!(result, ImageResult::Stopped) {
                inner.run.completed += 1;
                inner.run.images_done += 1;
                let stage = image::kind_of(path).map_or(2, image::Kind::stage);
                inner.run.stage_done[stage] += 1;
            }
            inner.cpu_progress.remove(path);
            let mut remember = |inner: &mut Inner, content: Option<(ContentHash, u64)>| {
                inner.history.add_image(path, mode);
                if let Some((hash, size)) = content {
                    inner.history.add_content(hash, size, mode);
                }
                remembered = true;
            };
            match result {
                ImageResult::Smaller { before, after, lossy, content, renamed } => {
                    inner.run.compressed += 1;
                    inner.run.images_lossy += usize::from(lossy);
                    inner.space_saved_mb += (before - after) as f64 / (1024.0 * 1024.0);
                    remember(&mut inner, content);
                    let saved = format!("{} ({:.0}%)", format_size(before - after), 100.0 * (before - after) as f64 / before.max(1) as f64);
                    log = Some(match renamed {
                        Some(new) => {
                            inner.history.add_image(&new, mode);
                            format!("{tag} Converted {display} to {}, saved {saved}", new.file_name().unwrap_or_default().to_string_lossy())
                        }
                        None => format!("{tag} Saved {saved} from {display}{}", if lossy { " (visually lossless)" } else { "" }),
                    });
                }
                ImageResult::Kept { content, note } => {
                    inner.run.skipped_larger += 1;
                    remember(&mut inner, content);
                    log = Some(match note {
                        Some(note) => format!("{tag} Skipped {display}: {note}"),
                        None => format!("{tag} Kept {display} (already optimal)"),
                    });
                }
                ImageResult::Renamed { to, content } => {
                    inner.run.skipped_larger += 1;
                    inner.history.add_image(&to, mode);
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
                    inner.run.skipped_larger += 1;
                    log = Some(format!("{tag} Skipped {display}: {why}"));
                }
                ImageResult::Failed(err) => {
                    inner.run.failed += 1;
                    inner.failed.insert(path.to_path_buf());
                    log = Some(format!("{tag} Failed on {display}: {err}"));
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
                    log = Some(format!("{tag} Not enough memory for {display}; left as it was, tried again next run"));
                }
                // Stop, or a Pause that released the memory: back to the front of the queue
                // (it starts over on Resume).
                ImageResult::Stopped => {
                    if inner.working && !inner.stop_requested {
                        inner.requeue_cpu(path);
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
        for line in log.into_iter().chain(progress) {
            self.log(line);
        }
        self.send_state(false);
    }
}

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

/// "812 KB", "4.5 MB".
fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
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
}

pub(super) type NoteContent = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

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
    // Most music has nothing to try: don't copy (to C:) or decode it.
    if kind.is_audio() && !crate::audio::worth_trying(ctx, kind, path) {
        if ctx.gate.short_of_memory.load(std::sync::atomic::Ordering::Relaxed) {
            return ImageResult::OutOfMemory;
        }
        return ImageResult::Kept { content: content_of(path), note: None };
    }
    let job = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    // FLAC, MP3 and Ogg are read where they are (ffmpeg, flac and OptiVorbis open any path,
    // and nothing writes to the original): a long album isn't copied to the temp folder first.
    let in_place = matches!(kind, image::Kind::Flac | image::Kind::Mp3 | image::Kind::Ogg);
    let factor = match kind {
        image::Kind::Pdf => 16,
        _ if in_place => 2,
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
    };
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
        return ImageResult::Kept { content: content_of(path), note: result.note };
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
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let out_ext = args::ext_lower(&target);
    let sibling = path.with_file_name(format!("{stem}.mediaopti.i{worker_id}.{out_ext}"));
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
    if let Err(err) = super::replace_original(&sibling, &target, path, disposal, &|p| note(opts, p)) {
        unclaim();
        return ImageResult::Failed(err);
    }
    let _ = system::restore_attributes(&target, &attrs);
    let lossy = result.steps.iter().any(|s| s.contains('='));
    let renamed = (target != path).then(|| target.clone());
    ImageResult::Smaller { before, after, lossy, content: content_of(&target), renamed }
}

/// `existing` (the new name's current file) has the same content as `path`: keeps the smaller
/// of `existing` and the new version (`smaller`, if there is one); the original and the other
/// copy go as `disposal` says.
fn note(opts: &JobOpts, file: &Path) -> std::io::Result<()> {
    opts.note.as_ref().map_or(Ok(()), |n| n(file))
}

#[allow(clippy::too_many_arguments)]
fn keep_smaller_copy(path: &Path, existing: &Path, smaller: Option<&(PathBuf, u64)>, before: u64, disposal: &system::Disposal, worker_id: u32, opts: &JobOpts) -> ImageResult {
    let theirs = std::fs::metadata(existing).map_or(u64::MAX, |m| m.len());
    match smaller {
        Some((best, after)) if *after < theirs => {
            // Ours is smaller: it replaces the existing copy, then the original goes.
            let stem = existing.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let sibling = existing.with_file_name(format!("{stem}.mediaopti.i{worker_id}.{}", args::ext_lower(existing)));
            let attrs = std::fs::metadata(path).map(|m| system::OriginalAttributes::from_metadata(&m));
            if let Err(err) = std::fs::copy(best, &sibling) {
                let _ = system::remove_file_if_exists(&sibling);
                return ImageResult::Failed(format!("could not write next to the original: {err}"));
            }
            if let Err(err) = super::replace_original(&sibling, existing, existing, &disposal.for_sibling(existing), &|p| note(opts, p)) {
                return ImageResult::Failed(err);
            }
            if let Ok(a) = attrs {
                let _ = system::restore_attributes(existing, &a);
            }
            match system::dispose(path, disposal) {
                Ok(()) => ImageResult::Duplicate { before: before + theirs - after, kept: existing.to_path_buf(), content: content_of(existing), replaced: true },
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

    /// A job too big for the temp drive's free space works on the file's own drive, in a
    /// hidden folder that disappears with the job. (Only a reservation: nothing is written.)
    #[test]
    fn work_folder_moves_to_the_files_drive_when_temp_is_short() {
        let file = Path::new(r"D:\mediaopti-test\some file.wav");
        let (Some(temp_free), Some(d_free)) = (system::free_space(&std::env::temp_dir()), system::free_space(Path::new(r"D:\"))) else {
            eprintln!("skipping: no D: drive");
            return;
        };
        // 4x this must exceed the temp drive's free space but fit on D:.
        let size = temp_free / 4 + (1 << 30);
        if volume_root(&std::env::temp_dir()) == volume_root(file) || size * 4 + DISK_RESERVE + (1 << 30) > d_free {
            eprintln!("skipping: needs a temp drive with less free space than D:");
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
}
