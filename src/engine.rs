//! Batch engine: folder scanning, the work queue, parallel ffmpeg workers with retries
//! and fallbacks, GPU telemetry and state broadcasting to the UI.

use crate::args::{self, ImageMode, Mode, Options, Originals, RawOptions};
use crate::image;
use crate::history::{self, ContentHash, History};

#[path = "engine_images.rs"]
mod images;
use crate::progress::{self, Diagnostics, WorkerProgress};
use crate::system::{self, OriginalAttributes, PowerGuard};

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio_util::sync::CancellationToken;

const STATE_THROTTLE: Duration = Duration::from_millis(250);
/// Kept free on a video's drive besides room for its new version.
const VIDEO_DISK_RESERVE: u64 = 4 * 1024 * 1024 * 1024;
/// CPU files running at least this long get their own progress row.
const SLOW_JOB_SECS: u64 = 15;
const HISTORY_SAVE_DELAY: Duration = Duration::from_secs(2);

/// Memory needed before starting another ffmpeg while others run. A single 4K AV1 encode
/// (multipass + 32-frame lookahead) was measured at up to ~5.3 GB of commit; running several
/// in parallel once exhausted the system commit limit and crashed every process.
const MIN_FREE_COMMIT: u64 = 6 * 1024 * 1024 * 1024;
const MIN_FREE_PHYSICAL: u64 = 2 * 1024 * 1024 * 1024;
/// How long a newly started ffmpeg is given to allocate its buffers before the next
/// worker re-checks free memory (only when memory is not plentiful).
const ADMISSION_WARMUP: Duration = Duration::from_secs(4);
const PLENTIFUL_COMMIT: u64 = 16 * 1024 * 1024 * 1024;

/// Whether another encoder may start now. The first encoder always may (otherwise a
/// low-memory machine would wait forever).
pub fn may_start_encoder(running: usize, memory: Option<system::MemoryStatus>) -> bool {
    match memory {
        _ if running == 0 => true,
        None => true,
        Some(m) => m.available_commit >= MIN_FREE_COMMIT && m.available_physical >= MIN_FREE_PHYSICAL,
    }
}

/// Where the engine sends logs and state (the window, or a recorder in tests).
pub trait Ui: Send + Sync + 'static {
    fn log(&self, message: String);
    fn state(&self, state: &StateDto);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Scanning,
    Ready,
    Running,
    Paused,
    Stopping,
    Done,
    Error,
}

#[derive(Default, Clone, Copy)]
struct RunStats {
    total: usize,
    completed: usize,
    compressed: usize,
    skipped_larger: usize,
    failed: usize,
    images_total: usize,
    images_done: usize,
    images_lossy: usize,
    /// CPU files by stage (audio, documents, images): queued this run, and finished.
    stage_total: [usize; 3],
    stage_done: [usize; 3],
}

#[derive(Clone, Debug)]
pub struct WorkerDto {
    pub worker_id: u32,
    pub file: String,
    pub fps: i64,
    pub speed: f64,
    pub percent: i64,
}

#[derive(Clone, Debug)]
pub struct TelemetryDto {
    pub total_fps: i64,
    pub total_speed: f64,
    pub active_progress_sum: f64,
    pub nvenc_util: f64,
    pub vram_gb: f64,
    pub workers: Vec<WorkerDto>,
}

#[derive(Clone, Debug)]
pub struct StateDto {
    pub status: Status,
    pub dir: Option<String>,
    pub total_files: usize,
    pub pending_files: usize,
    pub skipped_known: usize,
    pub skipped_generated: usize,
    /// Animated GIFs under the size limit, kept as GIFs.
    pub skipped_small_gifs: usize,
    pub history_count: usize,
    pub run_total: usize,
    pub completed_this_run: usize,
    pub compressed_this_run: usize,
    pub skipped_larger_this_run: usize,
    pub failed_this_run: usize,
    pub space_saved_mb: f64,
    /// Files waiting, by type (videos, audio, documents, images): count and bytes.
    pub queue_counts: [(usize, u64); 4],
    /// Changes whenever the queue's content changes (the window then reloads its list).
    pub queue_revision: u64,
    /// CPU files running for a while (a big comic, a long FLAC): name and seconds so far,
    /// longest first.
    pub slow_jobs: Vec<SlowJob>,
    /// This run by type (videos, audio, documents, images): finished, total, running now.
    pub type_progress: [(usize, usize, usize); 4],
    /// CPU work is waiting for free memory.
    pub cpu_waiting_memory: bool,
    pub telemetry: TelemetryDto,
}

/// A CPU file running for a while: its name, seconds so far, and its own progress (pages of
/// a comic, images of a PDF) when known.
#[derive(Clone, Debug)]
pub struct SlowJob {
    pub name: String,
    pub secs: u64,
    pub progress: Option<(usize, usize)>,
}

struct Inner {
    dir: Option<PathBuf>,
    files: Vec<PathBuf>,
    /// Size of each entry of `files` at scan time, same order.
    file_sizes: Vec<u64>,
    pending_queue: Vec<PathBuf>,
    queue_cursor: usize,
    /// Single-frame GIFs found by the scan: always images, whatever their size.
    static_gifs: HashSet<PathBuf>,
    /// PNG/JPEG and small GIFs, processed by the CPU image workers.
    /// CPU files by stage (audio, documents, images: see `Kind::stage`), each with its cursor
    /// and how many run now. A stage starts when the one before has nothing left to start.
    cpu_queues: [Vec<PathBuf>; 3],
    cpu_cursors: [usize; 3],
    cpu_active: [usize; 3],
    /// The stages announced in the log this batch.
    stage_logged: [bool; 3],
    /// Bytes waiting by type (videos, audio, documents, images).
    queue_bytes: [u64; 4],
    queue_revision: u64,
    images_active: usize,
    /// When each CPU file being processed was started.
    cpu_started: std::collections::HashMap<PathBuf, Instant>,
    /// The progress of the CPU files that report it (comics, PDFs).
    cpu_progress: std::collections::HashMap<PathBuf, Arc<image::tools::Progress>>,
    cpu_waiting_memory: bool,
    skipped_known: usize,
    skipped_generated: usize,
    skipped_small_gifs: usize,
    history: History,
    history_save_pending: bool,
    failed: HashSet<PathBuf>,
    in_progress: HashSet<PathBuf>,
    workers: BTreeMap<u32, WorkerProgress>,
    running_processes: usize,
    gpu_enc: f64,
    gpu_vram_mb: f64,
    working: bool,
    stop_requested: bool,
    status: Status,
    space_saved_mb: f64,
    options: Options,
    run: RunStats,
    cancel: CancellationToken,
    power: Option<PowerGuard>,
    /// Workers of the running batch, awaited by `run_batch` (files added during a run can
    /// start more).
    handles: Vec<(String, tokio::task::JoinHandle<()>)>,
    /// Ids of the video and CPU workers running now.
    video_ids: std::collections::BTreeSet<u32>,
    cpu_ids: std::collections::BTreeSet<u32>,
    /// The running batch's CPU settings (made when the first CPU file needs them).
    cpu: Option<Arc<images::CpuCtx>>,
    fatal: Arc<AtomicBool>,
    gpu_polling: bool,
}

/// The CPU stage of a queued file (see `Kind::stage`).
fn cpu_stage(file: &Path) -> usize {
    image::kind_of(file).map_or(2, image::Kind::stage)
}

/// Where a scanned file goes (see `Inner::place`).
enum Place {
    Generated,
    /// Its category is switched off (`gif`: an animated GIF kept as a GIF).
    Off { gif: bool },
    Known,
    Failed,
    Cpu,
    Video,
}

struct EmitThrottle {
    last_sent: Option<Instant>,
    pending: bool,
}

pub struct Engine {
    ui: Box<dyn Ui>,
    /// Runtime that runs workers, ffmpeg I/O and timers (the UI thread stays free).
    rt: tokio::runtime::Handle,
    ffmpeg_path: PathBuf,
    /// External image optimizers found at start-up.
    image_tools: image::tools::Tools,
    inner: Mutex<Inner>,
    emit: Mutex<EmitThrottle>,
    /// Serializes encoder start-up so memory checks see the previous encoder's usage.
    admission: Arc<tokio::sync::Mutex<()>>,
    /// True while the batch is paused: running ffmpeg processes are suspended and no
    /// new file is started.
    paused: tokio::sync::watch::Sender<bool>,
}

enum Outcome {
    Processed,
    Skipped,
    Failed,
    Stopped,
    Empty,
    Fatal,
}

enum AttemptStatus {
    Exited(i32),
    Stopped,
    SpawnError(String),
}

struct Attempt {
    status: AttemptStatus,
    diag: Diagnostics,
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn is_generated_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    is_orphan_temp(path) || name.contains(".tmp.") || name.contains("-h264.")
}

/// Temp outputs of this app (`clip.mediaopti.w1.mp4`, `photo.mediaopti.i3.png`), including
/// ones left by batchviproc (`clip.batchviproc.w1.mp4`). Only this exact shape matches, so a
/// user's own file that merely contains ".mediaopti." in its name is never deleted.
fn is_orphan_temp(path: &Path) -> bool {
    static TEMP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\.(mediaopti\.[wi]|batchviproc\.w)\d+\.[a-z0-9]+$").unwrap()
    });
    path.file_name().is_some_and(|n| TEMP.is_match(&n.to_string_lossy()))
}

/// Puts the new version `tmp` in place of `original` (as `final_target`), disposing of the
/// original as `disposal` says. On failure the error says what happened; if the original was
/// already set aside (Recycle Bin or backup folder), the new version is never deleted.
/// Blocking. Shared by the video and image workers.
fn replace_original(tmp: &Path, final_target: &Path, original: &Path, disposal: &system::Disposal) -> Result<(), String> {
    let Err(err) = system::replace_file_with_retry(tmp, final_target, original, disposal) else {
        return Ok(());
    };
    if original.exists() {
        let _ = system::remove_file_if_exists(tmp);
        return Err(format!("could not replace it: {err}"));
    }
    let gone_to = match disposal {
        system::Disposal::RecycleBin => "is in the Recycle Bin".to_string(),
        system::Disposal::MoveTo(dest) => format!("was moved to {}", dest.parent().unwrap_or(dest).display()),
        system::Disposal::Delete => "was deleted".to_string(),
    };
    Err(format!(
        "could not finish replacing it: {err}. The original {gone_to} and the new version is kept as {}",
        tmp.display()
    ))
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

impl Inner {
    /// Whether `file` (of `size` bytes) is handled by the image workers: PNG/JPEG, still
    /// GIFs, and animated GIFs under the size limit (bigger ones become video).
    fn is_image_job(&self, file: &Path, size: u64) -> bool {
        match image::kind_of(file) {
            // With videos switched off, big animated GIFs are optimized as GIFs.
            Some(image::Kind::Gif) => size < self.options.gif_min_bytes || self.static_gifs.contains(file) || !self.options.videos,
            Some(_) => true,
            None => false,
        }
    }

    /// Whether the settings let the CPU workers take this file: conversions (WAV) follow the
    /// conversions option, everything else (images, cover art) the image option.
    fn cpu_job_enabled(&self, file: &Path) -> bool {
        let Some(kind) = image::kind_of(file) else { return false };
        let on = match kind.stage() {
            0 => self.options.audio != ImageMode::Off,
            1 => self.options.docs,
            _ => self.options.images != ImageMode::Off,
        };
        let c = self.options.convert;
        let conversion_on = match kind {
            image::Kind::Wav => c.wav,
            image::Kind::Bmp | image::Kind::Tga => c.images,
            image::Kind::Cbr => c.cbr,
            _ => true,
        };
        on && conversion_on
    }

    /// Whether `file` is processed visually lossless (its category's setting: audio files by
    /// the audio setting, the rest by the image setting).
    fn lossy_for(&self, file: &Path) -> bool {
        let audio = image::kind_of(file).is_some_and(image::Kind::is_audio);
        (if audio { self.options.audio } else { self.options.images }) == ImageMode::Lossy
    }

    /// Content-history request for a file under the current settings.
    fn want(&self, file: &Path, size: u64) -> history::Want {
        if !self.is_image_job(file, size) {
            history::Want::Video
        } else if self.lossy_for(file) {
            history::Want::ImageLossy
        } else {
            history::Want::ImageLossless
        }
    }

    fn images_lossy(&self) -> bool {
        self.options.images == ImageMode::Lossy
    }

    /// Root of the backup folder for replaced originals, if that option is chosen.
    fn backup_root(&self) -> Option<PathBuf> {
        let Originals::BackupFolder(chosen) = &self.options.originals else { return None };
        chosen.clone().or_else(|| self.dir.as_deref().map(default_backup_root))
    }

    /// What happens to `original` when it is replaced. Backups mirror the scanned folder's
    /// structure, so originals can be found (and restored) where they belong.
    fn disposal_for(&self, original: &Path) -> system::Disposal {
        match &self.options.originals {
            Originals::Delete => system::Disposal::Delete,
            Originals::RecycleBin => system::Disposal::RecycleBin,
            Originals::BackupFolder(_) => {
                let root = self.backup_root().unwrap_or_else(|| default_backup_root(original));
                let rel = self
                    .dir
                    .as_deref()
                    .and_then(|d| original.strip_prefix(d).ok())
                    .map_or_else(|| PathBuf::from(original.file_name().unwrap_or_default()), Path::to_path_buf);
                system::Disposal::MoveTo(root.join(rel))
            }
        }
    }

    /// Where a scanned file goes under the current settings and history.
    fn place(&self, file: &Path, size: u64) -> Place {
        if is_generated_name(file) {
            Place::Generated
        } else if self.is_image_job(file, size) {
            if !self.cpu_job_enabled(file) {
                // Not remembered: they may be optimized once switched on.
                Place::Off { gif: args::is_gif(file) }
            } else if self.history.contains_image(file, self.lossy_for(file)) {
                Place::Known
            } else if self.failed.contains(file) {
                Place::Failed
            } else {
                Place::Cpu
            }
        } else if !self.options.videos {
            Place::Off { gif: false }
        } else if self.history.contains(file) {
            Place::Known
        } else if self.failed.contains(file) {
            Place::Failed
        } else {
            Place::Video
        }
    }

    /// Rebuilds the work queues from the scanned files, history and this run's failures.
    fn rebuild_queue(&mut self) {
        let (mut queue, mut cpu): (Vec<PathBuf>, [Vec<PathBuf>; 3]) = (Vec::new(), Default::default());
        let mut bytes = [0u64; 4];
        let (mut known, mut generated, mut small_gifs) = (0, 0, 0);
        for (i, file) in self.files.iter().enumerate() {
            let size = self.file_sizes.get(i).copied().unwrap_or(u64::MAX);
            match self.place(file, size) {
                Place::Generated => generated += 1,
                Place::Off { gif } => small_gifs += usize::from(gif),
                Place::Known => known += 1,
                Place::Failed => {}
                Place::Cpu => {
                    let stage = cpu_stage(file);
                    bytes[stage + 1] += size;
                    cpu[stage].push(file.clone());
                }
                Place::Video => {
                    bytes[0] += size;
                    queue.push(file.clone());
                }
            }
        }
        self.pending_queue = queue;
        self.queue_cursor = 0;
        self.cpu_queues = cpu;
        self.cpu_cursors = [0; 3];
        self.queue_bytes = bytes;
        self.queue_revision += 1;
        self.skipped_known = known;
        self.skipped_generated = generated;
        self.skipped_small_gifs = small_gifs;
    }

    fn remaining_pending(&self) -> usize {
        self.pending_queue.len().saturating_sub(self.queue_cursor) + self.cpu_left() + self.in_progress.len()
    }

    /// CPU files queued in all stages.
    fn cpu_total(&self) -> usize {
        self.cpu_queues.iter().map(Vec::len).sum()
    }

    /// CPU files not started yet.
    fn cpu_left(&self) -> usize {
        self.cpu_queues.iter().zip(self.cpu_cursors).map(|(q, c)| q.len().saturating_sub(c)).sum()
    }

    /// Adds a CPU file to its stage's queue (and the byte counts).
    fn queue_cpu(&mut self, file: PathBuf, size: u64) {
        let stage = cpu_stage(&file);
        self.queue_bytes[stage + 1] += size;
        self.cpu_queues[stage].push(file);
        self.queue_revision += 1;
    }

    fn display_name(&self, path: &Path) -> String {
        if let Some(dir) = &self.dir {
            if let Ok(rel) = path.strip_prefix(dir) {
                let rel = rel.to_string_lossy();
                if !rel.is_empty() {
                    return rel.into_owned();
                }
            }
        }
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn claim_next(&mut self) -> Option<PathBuf> {
        while self.queue_cursor < self.pending_queue.len() {
            let candidate = self.pending_queue[self.queue_cursor].clone();
            self.queue_cursor += 1;
            if !self.in_progress.contains(&candidate)
                && !self.failed.contains(&candidate)
                && !self.history.contains(&candidate)
            {
                self.in_progress.insert(candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    fn reset_run_tracking(&mut self) {
        self.failed.clear();
        self.in_progress.clear();
        self.cpu_started.clear();
        self.cpu_progress.clear();
        self.cpu_waiting_memory = false;
        self.workers.clear();
        self.space_saved_mb = 0.0;
    }

    fn snapshot(&self) -> StateDto {
        let mut total_fps = 0.0;
        let mut total_speed = 0.0;
        let mut progress_sum = 0.0;
        // While paused, the last parsed fps/speed would be stale.
        let live = self.status != Status::Paused;
        let workers = self
            .workers
            .iter()
            .map(|(id, w)| {
                let (fps, speed) = if live { (w.fps, w.speed) } else { (0.0, 0.0) };
                total_fps += fps;
                total_speed += speed;
                let pct = w.percent.clamp(0.0, 100.0);
                progress_sum += pct / 100.0;
                WorkerDto {
                    worker_id: *id,
                    file: w.file.clone(),
                    fps: fps.round() as i64,
                    speed: round1(speed),
                    percent: pct.round() as i64,
                }
            })
            .collect();

        let mut slow_jobs: Vec<SlowJob> = self
            .cpu_started
            .iter()
            .filter(|(_, t)| t.elapsed().as_secs() >= SLOW_JOB_SECS)
            .map(|(p, t)| SlowJob {
                name: self.display_name(p),
                secs: t.elapsed().as_secs(),
                progress: self.cpu_progress.get(p).and_then(|pr| pr.get()),
            })
            .collect();
        slow_jobs.sort_by_key(|j| std::cmp::Reverse(j.secs));
        slow_jobs.truncate(4);
        let r = &self.run;
        let videos_running = self.workers.len();
        let type_progress = [
            (r.completed.saturating_sub(r.images_done), r.total.saturating_sub(r.images_total), videos_running),
            (r.stage_done[0], r.stage_total[0], self.cpu_active[0]),
            (r.stage_done[1], r.stage_total[1], self.cpu_active[1]),
            (r.stage_done[2], r.stage_total[2], self.cpu_active[2]),
        ];

        StateDto {
            status: self.status,
            dir: self.dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
            total_files: self.files.len(),
            pending_files: self.remaining_pending(),
            skipped_known: self.skipped_known,
            skipped_generated: self.skipped_generated,
            skipped_small_gifs: self.skipped_small_gifs,
            history_count: self.history.len(),
            run_total: self.run.total,
            completed_this_run: self.run.completed,
            compressed_this_run: self.run.compressed,
            skipped_larger_this_run: self.run.skipped_larger,
            failed_this_run: self.run.failed,
            space_saved_mb: round1(self.space_saved_mb),
            queue_counts: [
                (self.pending_queue.len().saturating_sub(self.queue_cursor), self.queue_bytes[0]),
                (self.cpu_queues[0].len().saturating_sub(self.cpu_cursors[0]), self.queue_bytes[1]),
                (self.cpu_queues[1].len().saturating_sub(self.cpu_cursors[1]), self.queue_bytes[2]),
                (self.cpu_queues[2].len().saturating_sub(self.cpu_cursors[2]), self.queue_bytes[3]),
            ],
            queue_revision: self.queue_revision,
            slow_jobs,
            type_progress,
            cpu_waiting_memory: self.cpu_waiting_memory,
            telemetry: TelemetryDto {
                total_fps: total_fps.round() as i64,
                total_speed: round1(total_speed),
                active_progress_sum: progress_sum,
                nvenc_util: self.gpu_enc,
                vram_gb: round1(self.gpu_vram_mb / 1024.0),
                workers,
            },
        }
    }
}

impl Engine {
    pub fn new(
        ui: Box<dyn Ui>,
        data_dir: &Path,
        ffmpeg_path: PathBuf,
        rt: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let history = History::load(data_dir.join("processedFiles.txt"));
        let image_tools = image::tools::Tools::discover(ffmpeg_path.clone(), data_dir);
        Arc::new(Self {
            ui,
            rt,
            ffmpeg_path,
            image_tools,
            inner: Mutex::new(Inner {
                dir: None,
                files: Vec::new(),
                file_sizes: Vec::new(),
                pending_queue: Vec::new(),
                queue_cursor: 0,
                static_gifs: HashSet::new(),
                cpu_queues: Default::default(),
                cpu_cursors: [0; 3],
                cpu_active: [0; 3],
                stage_logged: [false; 3],
                queue_bytes: [0; 4],
                queue_revision: 0,
                images_active: 0,
                cpu_started: std::collections::HashMap::new(),
                cpu_progress: std::collections::HashMap::new(),
                cpu_waiting_memory: false,
                skipped_known: 0,
                skipped_generated: 0,
                skipped_small_gifs: 0,
                history,
                history_save_pending: false,
                failed: HashSet::new(),
                in_progress: HashSet::new(),
                workers: BTreeMap::new(),
                running_processes: 0,
                gpu_enc: 0.0,
                gpu_vram_mb: 0.0,
                working: false,
                stop_requested: false,
                status: Status::Idle,
                space_saved_mb: 0.0,
                options: Options::default(),
                run: RunStats::default(),
                cancel: CancellationToken::new(),
                power: None,
                handles: Vec::new(),
                video_ids: std::collections::BTreeSet::new(),
                cpu_ids: std::collections::BTreeSet::new(),
                cpu: None,
                fatal: Arc::new(AtomicBool::new(false)),
                gpu_polling: false,
            }),
            emit: Mutex::new(EmitThrottle {
                last_sent: None,
                pending: false,
            }),
            admission: Arc::new(tokio::sync::Mutex::new(())),
            paused: tokio::sync::watch::Sender::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs blocking disk work off the async worker threads.
    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> T {
        self.rt
            .spawn_blocking(f)
            .await
            .expect("blocking file task panicked")
    }

    pub fn log(&self, message: impl Into<String>) {
        self.ui.log(message.into());
    }

    pub fn snapshot(&self) -> StateDto {
        self.lock().snapshot()
    }

    /// Broadcasts state to the UI; non-forced updates are throttled to one per 250 ms.
    pub fn send_state(self: &Arc<Self>, force: bool) {
        {
            let mut emit = self.emit.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let recent = emit
                .last_sent
                .is_some_and(|t| t.elapsed() < STATE_THROTTLE);
            if !force && recent {
                if !emit.pending {
                    emit.pending = true;
                    let engine = Arc::clone(self);
                    self.rt.spawn(async move {
                        tokio::time::sleep(STATE_THROTTLE).await;
                        engine.send_state(true);
                    });
                }
                return;
            }
            emit.pending = false;
            emit.last_sent = Some(Instant::now());
        }

        let state = self.snapshot();
        self.ui.state(&state);
    }

    /// Saves history after a short delay so bursts of completions cause one write.
    fn schedule_history_save(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.history_save_pending {
                return;
            }
            inner.history_save_pending = true;
        }
        let engine = Arc::clone(self);
        self.rt.spawn(async move {
            tokio::time::sleep(HISTORY_SAVE_DELAY).await;
            engine.flush_history();
        });
    }

    pub fn flush_history(&self) {
        let mut inner = self.lock();
        inner.history_save_pending = false;
        if let Err(err) = inner.history.save() {
            eprintln!("Error saving processed files: {err}");
        }
    }

    fn add_processed(self: &Arc<Self>, file: &Path) {
        let added = {
            let mut inner = self.lock();
            let added = inner.history.add(file);
            if added {
                inner.skipped_known += 1;
            }
            added
        };
        if added {
            self.schedule_history_save();
        }
    }

    /// Remembers a finished file: `file` by location, `content_of` (the file now on disk)
    /// by content hash, so it is still recognised after being moved or renamed.
    async fn remember(self: &Arc<Self>, file: &Path, content_of: &Path) {
        self.add_processed(file);
        let path = content_of.to_path_buf();
        let hashed = self
            .blocking(move || -> std::io::Result<(ContentHash, u64)> {
                let size = std::fs::metadata(&path)?.len();
                Ok((history::hash_file(&path)?, size))
            })
            .await;
        if let Ok((hash, size)) = hashed {
            self.lock().history.add_content(hash, size, history::VIDEO_MODE);
            self.schedule_history_save();
        }
    }

    /// Finds files that are new to the location history but whose content was processed
    /// before (moved, renamed or copied), and adds them to the location history.
    /// Only files whose size matches a remembered size are hashed.
    async fn recognise_by_content(self: &Arc<Self>, files: &[PathBuf], sizes: &[u64]) -> usize {
        let (candidates, wants): (Vec<PathBuf>, Vec<history::Want>) = {
            let inner = self.lock();
            files
                .iter()
                .zip(sizes)
                .filter(|(f, s)| inner.history.has_size(**s) && !is_generated_name(f))
                .map(|(f, s)| (f, inner.want(f, *s)))
                .filter(|(f, want)| match want {
                    history::Want::Video => !inner.history.contains(f),
                    _ => !inner.history.contains_image(f, inner.lossy_for(f)),
                })
                .map(|(f, want)| (f.clone(), want))
                .unzip()
        };
        if candidates.is_empty() {
            return 0;
        }
        self.log(format!("Checking {} files by content (same size as a processed one)", candidates.len()));
        let hashed = self.blocking(move || hash_files_parallel(&candidates)).await;
        let hits = {
            let mut inner = self.lock();
            let mut hits = 0;
            for ((file, hash), want) in hashed.into_iter().zip(wants) {
                if hash.is_some_and(|h| inner.history.contains_content(&h, want)) {
                    match want {
                        history::Want::Video => {
                            inner.history.add(&file);
                        }
                        history::Want::ImageLossy => inner.history.add_image(&file, image::mode(true)),
                        history::Want::ImageLossless => inner.history.add_image(&file, image::mode(false)),
                    }
                    hits += 1;
                }
            }
            hits
        };
        if hits > 0 {
            self.schedule_history_save();
        }
        hits
    }

    // ---------------------------------------------------------------- folder selection

    /// Selects a folder and scans it (replacing what was loaded).
    pub async fn load_dir(self: &Arc<Self>, dir: PathBuf) {
        if !dir.is_dir() {
            self.log(format!("Folder not found: {}", dir.display()));
            return;
        }
        self.load_items(vec![dir], false).await;
    }

    /// Files and folders dropped on the window: added to the running batch's queues, or to
    /// what is loaded (the start folder becomes their common folder).
    pub fn spawn_add_items(self: &Arc<Self>, items: Vec<PathBuf>) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.load_items(items, true).await });
    }

    /// Scans `items` (folders and files). `add`: keep what is loaded (or queued, during a
    /// run) and add to it; otherwise they replace it.
    pub async fn load_items(self: &Arc<Self>, items: Vec<PathBuf>, add: bool) {
        let items: Vec<PathBuf> = items.into_iter().filter(|p| p.exists()).collect();
        if items.is_empty() {
            self.log("Nothing to add (the dropped items no longer exist)");
            return;
        }
        let (working, old_dir) = {
            let inner = self.lock();
            (inner.working, inner.dir.clone())
        };
        if working && !add {
            self.log("Stop processing before choosing another folder");
            return;
        }
        if !working && self.lock().status == Status::Scanning {
            self.log("A scan is running; add the files again when it has finished");
            return;
        }
        // The common folder of everything loaded: names in the window and the backup folder's
        // layout are relative to it. During a run it stays as it was.
        let mut all = items.clone();
        if add {
            all.extend(old_dir.clone());
        }
        let root = if working { old_dir.clone().unwrap_or_else(|| common_root(&items)) } else { common_root(&all) };
        if !working {
            let mut inner = self.lock();
            inner.dir = Some(root.clone());
            if !add {
                inner.files.clear();
                inner.file_sizes.clear();
                inner.static_gifs.clear();
            }
            inner.pending_queue.clear();
            inner.queue_cursor = 0;
            inner.cpu_queues = Default::default();
            inner.cpu_cursors = [0; 3];
            inner.reset_run_tracking();
            inner.run = RunStats::default();
            inner.status = Status::Scanning;
        }
        let what = match items.as_slice() {
            [one] => one.display().to_string(),
            many => format!("{} dropped items", many.len()),
        };
        self.log(format!("Scanning {what}"));
        self.send_state(true);

        // A backup folder inside the scanned folder must not be scanned (or optimized again).
        let (exclude, temp) = {
            let inner = self.lock();
            (inner.backup_root(), inner.options.temp_dir.clone())
        };
        let roots = items.clone();
        let scan = self.rt.spawn_blocking(move || scan_folder(&roots, exclude.as_deref(), temp.as_deref())).await;
        let result = match scan {
            Ok(r) => r,
            Err(err) => {
                if !working {
                    self.lock().status = Status::Error;
                }
                self.log(format!("Could not scan: {err}"));
                self.send_state(true);
                return;
            }
        };
        for msg in &result.messages {
            self.log(msg.clone());
        }
        if result.unreadable > 0 {
            self.log(format!("{} files or folders could not be read (permissions or broken links) and were skipped", result.unreadable));
        }
        if result.cleaned_orphans > 0 {
            self.log(format!("Removed {} leftover temporary (.mediaopti) file(s)", result.cleaned_orphans));
        }
        let recognised = self.recognise_by_content(&result.files, &result.sizes).await;
        if recognised > 0 {
            self.log(format!("{recognised} moved, renamed or copied files recognised as already processed"));
        }

        if working {
            // Appended to the running batch; more workers start if the queues need them.
            let (videos, cpu) = {
                let mut inner = self.lock();
                let loaded: HashSet<PathBuf> = inner.files.iter().cloned().collect();
                let (mut videos, mut cpu) = (0, 0);
                for (file, size) in result.files.into_iter().zip(result.sizes) {
                    if loaded.contains(&file) {
                        continue;
                    }
                    if result.static_gifs.contains(&file) {
                        inner.static_gifs.insert(file.clone());
                    }
                    match inner.place(&file, size) {
                        Place::Video => {
                            inner.pending_queue.push(file.clone());
                            inner.queue_bytes[0] += size;
                            inner.queue_revision += 1;
                            videos += 1;
                        }
                        Place::Cpu => {
                            inner.run.stage_total[cpu_stage(&file)] += 1;
                            inner.queue_cpu(file.clone(), size);
                            cpu += 1;
                        }
                        _ => {}
                    }
                    inner.files.push(file);
                    inner.file_sizes.push(size);
                }
                inner.run.total += videos + cpu;
                inner.run.images_total += cpu;
                if inner.working {
                    self.spawn_workers(&mut inner);
                }
                (videos, cpu)
            };
            self.log(if videos + cpu == 0 {
                "Nothing new to add to the queue (already processed, or switched off)".to_string()
            } else {
                format!("Added to the queue: {videos} videos and {cpu} images, audio or documents")
            });
            self.send_state(true);
            return;
        }

        let (known, generated, small_gifs, videos, cpu, total) = {
            let mut inner = self.lock();
            if add {
                let loaded: HashSet<PathBuf> = inner.files.iter().cloned().collect();
                for (file, size) in result.files.into_iter().zip(result.sizes) {
                    if !loaded.contains(&file) {
                        inner.files.push(file);
                        inner.file_sizes.push(size);
                    }
                }
                inner.static_gifs.extend(result.static_gifs);
            } else {
                inner.files = result.files;
                inner.file_sizes = result.sizes;
                inner.static_gifs = result.static_gifs;
            }
            inner.reset_run_tracking();
            inner.rebuild_queue();
            inner.run = RunStats::default();
            inner.status = Status::Ready;
            (inner.skipped_known, inner.skipped_generated, inner.skipped_small_gifs, inner.pending_queue.len(), inner.cpu_total(), inner.files.len())
        };
        self.log(format!(
            "{} {total} files in {} ({videos} videos and {cpu} images, audio or documents to process)",
            if add { "Now" } else { "Found" },
            root.display()
        ));
        if known > 0 {
            self.log(format!("{known} files are already in the processed history"));
        }
        if small_gifs > 0 {
            self.log(format!("{small_gifs} animated GIFs are under the size limit and will stay GIFs"));
        }
        if generated > 0 {
            self.log(format!("{generated} generated or temporary files will be ignored"));
        }
        self.send_state(true);
    }

    /// The external tools found at start-up, and the missing ones.
    pub fn tool_lists(&self) -> (Vec<&'static str>, Vec<&'static str>) {
        image::tools::NAMES.iter().partition(|n| self.image_tools.has(n))
    }

    /// Empties the queue (nothing is deleted on disk). Not during a run.
    pub fn clear_queue(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.working || inner.status == Status::Scanning {
                drop(inner);
                self.log("Stop processing before clearing the queue");
                return;
            }
            inner.dir = None;
            inner.files.clear();
            inner.file_sizes.clear();
            inner.static_gifs.clear();
            inner.pending_queue.clear();
            inner.queue_cursor = 0;
            inner.cpu_queues = Default::default();
            inner.cpu_cursors = [0; 3];
            inner.queue_bytes = [0; 4];
            inner.queue_revision += 1;
            inner.reset_run_tracking();
            inner.run = RunStats::default();
            inner.skipped_known = 0;
            inner.skipped_generated = 0;
            inner.skipped_small_gifs = 0;
            inner.status = Status::Idle;
        }
        self.log("Queue cleared");
        self.send_state(true);
    }

    /// The first `limit` files waiting, in processing order: (name, type, size).
    pub fn queue_preview(&self, limit: usize) -> Vec<(String, &'static str, u64)> {
        let inner = self.lock();
        let mut picked: Vec<(&PathBuf, &'static str)> = Vec::new();
        let videos = inner.pending_queue.iter().skip(inner.queue_cursor).map(|p| (p, "Video"));
        let cpu = ["Audio", "Document", "Image"]
            .iter()
            .enumerate()
            .flat_map(|(i, label)| inner.cpu_queues[i].iter().skip(inner.cpu_cursors[i]).map(move |p| (p, *label)));
        picked.extend(videos.chain(cpu).take(limit));
        let wanted: HashSet<&PathBuf> = picked.iter().map(|(p, _)| *p).collect();
        let sizes: std::collections::HashMap<&PathBuf, u64> =
            inner.files.iter().zip(&inner.file_sizes).filter(|(f, _)| wanted.contains(f)).map(|(f, s)| (f, *s)).collect();
        picked.into_iter().map(|(p, kind)| (inner.display_name(p), kind, sizes.get(p).copied().unwrap_or(0))).collect()
    }

    // ---------------------------------------------------------------- batch control

    /// Scans a folder in the background.
    pub fn spawn_load_dir(self: &Arc<Self>, dir: PathBuf) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.load_dir(dir).await });
    }

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
            self.log(self.image_tools.summary());
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
    fn finish_batch(&self, inner: &mut Inner) -> (bool, Status, RunStats, f64) {
        {
            let was_stopped = inner.stop_requested;
            inner.cpu = None;
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
    fn spawn_workers(self: &Arc<Self>, inner: &mut Inner) {
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

    async fn run_worker(self: Arc<Self>, worker_id: u32, fatal: Arc<AtomicBool>) {
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
                Outcome::Processed | Outcome::Skipped | Outcome::Failed => {}
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
            inner.running_processes
        };
        self.log(if pause {
            match active {
                0 => "Paused. Press Resume to continue.".to_string(),
                1 => "Paused (1 ffmpeg process suspended). Press Resume to continue.".to_string(),
                n => format!("Paused ({n} ffmpeg processes suspended). Press Resume to continue."),
            }
        } else {
            "Resumed".to_string()
        });
        self.send_state(true);
    }

    pub fn toggle_pause(self: &Arc<Self>) {
        self.set_paused(!self.is_paused());
    }

    /// Waits while the batch is paused. Returns false if Stop was pressed.
    async fn wait_while_paused(&self, cancel: &CancellationToken) -> bool {
        let mut rx = self.paused.subscribe();
        tokio::select! {
            r = rx.wait_for(|paused| !*paused) => r.is_ok(),
            _ = cancel.cancelled() => false,
        }
    }

    pub fn clear_history(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.working {
                drop(inner);
                self.log("Stop processing before clearing processed history");
                return;
            }
            inner.history.clear();
            inner.rebuild_queue();
            inner.status = if inner.dir.is_some() {
                Status::Ready
            } else {
                Status::Idle
            };
        }
        self.flush_history();
        self.log("Processed history cleared");
        self.send_state(true);
    }

    /// Called when the app exits: stops ffmpeg and saves history.
    pub fn shutdown(&self) {
        {
            let mut inner = self.lock();
            inner.stop_requested = true;
            inner.cancel.cancel();
            inner.power = None;
        }
        self.flush_history();
    }

    /// Reads NVENC load and VRAM from one long-running `nvidia-smi -lms` process
    /// (spawning nvidia-smi for every sample cost ~50 ms of CPU each time).
    fn start_gpu_polling_with(self: &Arc<Self>, cancel: CancellationToken) {
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

    // ---------------------------------------------------------------- per-file pipeline

    fn reset_worker(&self, worker_id: u32, label: String) {
        self.lock()
            .workers
            .insert(worker_id, WorkerProgress::new(label));
    }

    fn release_tracking(&self, worker_id: u32, input: &Path) {
        let mut inner = self.lock();
        inner.in_progress.remove(input);
        inner.workers.remove(&worker_id);
    }

    fn log_remove(&self, path: &Path) {
        if let Err(err) = system::remove_file_if_exists(path) {
            self.log(format!(
                "Could not remove temporary file {}: {err}",
                path.display()
            ));
        }
    }

    async fn process_one(self: &Arc<Self>, worker_id: u32) -> Outcome {
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
        self.reset_worker(worker_id, display.clone());
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
            self.reset_worker(worker_id, format!("{display} (CPU fallback)"));
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
            self.reset_worker(worker_id, format!("{display} (AAC fallback)"));
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
            self.reset_worker(worker_id, format!("{display} (CPU fallback)"));
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

        self.lock().run.completed += 1;

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

    /// Re-encodes with stronger settings (higher CQ/CRF) when the preset output was not
    /// smaller. A step is accepted only if its output is smaller and its VMAF against the
    /// original stays at or above `floor` (worst 5% of frames: `floor - 5`). It keeps
    /// climbing while that holds, and the strongest passing step wins.
    ///
    /// On the author's library each +4 CQ made AV1 output about 28% smaller, so the first
    /// step is the one predicted to get under the original size. If its quality fails, one
    /// gentler step is tried once.
    #[allow(clippy::too_many_arguments)]
    async fn quality_retry(
        self: &Arc<Self>,
        worker_id: u32,
        input: &Path,
        tmp: &Path,
        display: &str,
        mode: Mode,
        in_size: f64,
        out_size: f64,
        duration_sec: f64,
        floor: f64,
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
        let keep = args::temp_name(input, worker_id + 1000);
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
            self.reset_worker(worker_id, format!("{display} (quality retry +{boost})"));
            let attempt = self.run_attempt(worker_id, input, tmp, display, mode, boost).await;
            match attempt.status {
                AttemptStatus::Stopped => {
                    self.log_remove(&keep);
                    return Retry::Stopped;
                }
                AttemptStatus::Exited(0) => {}
                _ => {
                    notes.push(format!("the +{boost} encode failed"));
                    self.log_remove(tmp);
                    return finish(self, best, &notes).await;
                }
            }
            let t = tmp.to_path_buf();
            let size = self.blocking(move || std::fs::metadata(t).map_or(f64::MAX, |m| m.len() as f64)).await;
            if size >= in_size * 0.99 {
                notes.push(format!("+{boost}: still {:.0}% of the original", size / in_size * 100.0));
                self.log_remove(tmp);
                if best.is_none() && !stepped_back && k + 1 < STEPS.len() && !tried[k + 1] {
                    k += 1;
                    continue;
                }
                return finish(self, best, &notes).await;
            }
            self.reset_worker(worker_id, format!("{display} (measuring quality at +{boost})"));
            self.send_state(false);
            let mut measured = match self.measure_vmaf(input, tmp, duration_sec, None).await {
                Err(Stopped) => {
                    self.log_remove(&keep);
                    return Retry::Stopped;
                }
                Ok(m) => m,
            };
            // libvmaf fails to allocate when memory is tight (parallel 4K encodes): wait for
            // free memory and measure once more on fewer threads.
            if measured.as_ref().is_err_and(|e| e.contains("alloc")) {
                self.log(format!("[Worker {worker_id}] {display}: not enough memory to measure quality; waiting to retry"));
                let cancel = self.lock().cancel.clone();
                for _ in 0..60 {
                    if system::memory_status().is_none_or(|m| m.available_commit >= 4 << 30) {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        _ = cancel.cancelled() => {
                            self.log_remove(&keep);
                            return Retry::Stopped;
                        }
                    }
                }
                measured = match self.measure_vmaf(input, tmp, duration_sec, Some(2)).await {
                    Err(Stopped) => {
                        self.log_remove(&keep);
                        return Retry::Stopped;
                    }
                    Ok(m) => m,
                };
            }
            match measured {
                Ok((vmaf, worst)) if vmaf >= floor && worst >= floor - 5.0 => {
                    let (kp, t) = (keep.clone(), tmp.to_path_buf());
                    let kept = self.blocking(move || system::remove_file_if_exists(&kp).and_then(|()| std::fs::rename(&t, &kp))).await;
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

    /// Duration in seconds of a media file, from ffmpeg's report.
    async fn probe_duration(&self, path: &Path) -> Option<f64> {
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
    async fn same_video(&self, original: &Path, existing: &Path, duration_sec: f64) -> Result<bool, Stopped> {
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
    async fn keep_smaller_video(self: &Arc<Self>, worker_id: u32, input: &Path, existing: &Path, tmp: &Path, display: &str, in_size: f64) -> Outcome {
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

    /// VMAF of `encoded` against `original`: mean and 5th percentile over the frames of
    /// three 8-second windows (20%, 50%, 80% of the video), or of the whole video when it
    /// is short. The original gets the same even-size scaling the encode used, so frames
    /// line up. `Ok(Err(reason))` if it could not be measured; `Err` if Stop was pressed.
    async fn measure_vmaf(
        &self,
        original: &Path,
        encoded: &Path,
        duration_sec: f64,
        threads: Option<usize>,
    ) -> Result<Result<(f64, f64), String>, Stopped> {
        let cancel = self.lock().cancel.clone();
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

    /// Waits until another encoder fits in memory. Returns false if Stop was pressed.
    async fn wait_for_memory(&self, worker_id: u32, display: &str, cancel: &CancellationToken) -> bool {
        let mut announced = false;
        loop {
            let running = self.lock().running_processes;
            let memory = system::memory_status();
            if may_start_encoder(running, memory) {
                if announced {
                    self.reset_worker(worker_id, display.to_string());
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
                self.reset_worker(worker_id, format!("{display} (waiting for memory)"));
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = cancel.cancelled() => return false,
            }
        }
    }

    /// Runs ffmpeg once, streaming its stderr into the worker's progress, with a stall
    /// watchdog and cancellation on Stop.
    async fn run_attempt(
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
            self.reset_worker(worker_id, format!("{display} (waiting to start)"));
            self.send_state(false);
            let permit = tokio::select! {
                permit = self.admission.clone().lock_owned() => permit,
                _ = cancel.cancelled() => {
                    return Attempt { status: AttemptStatus::Stopped, diag };
                }
            };
            self.reset_worker(worker_id, display.to_string());
            permit
        };
        // A retry (AAC / CPU fallback) may come up while the batch is paused.
        if self.is_paused() {
            self.reset_worker(worker_id, format!("{display} (paused)"));
            self.send_state(false);
            if !self.wait_while_paused(&cancel).await {
                return Attempt { status: AttemptStatus::Stopped, diag };
            }
            self.reset_worker(worker_id, display.to_string());
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

fn mode_label(mode: ImageMode) -> &'static str {
    match mode {
        ImageMode::Off => "off",
        ImageMode::Lossless => "lossless",
        ImageMode::Lossy => "visually lossless (SSIMULACRA2 >= 93)",
    }
}

/// Scales a video down to at most 1920x1080 worth of pixels (never up), even dimensions.
const VMAF_FIT: &str = "scale=w='trunc(iw*min(1\\,sqrt(2073600/(iw*ih)))/2)*2':h='trunc(ih*min(1\\,sqrt(2073600/(iw*ih)))/2)*2':flags=bicubic";

/// Outcome of the quality-checked retry.
enum Retry {
    Accepted { boost: u32, vmaf: f64, worst: f64 },
    Rejected(String),
    Stopped,
}

/// Stop was pressed during a measurement.
struct Stopped;

/// Per-frame VMAF scores from a libvmaf JSON log.
fn vmaf_frame_scores(json: &str) -> Option<Vec<f64>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("frames")?
        .as_array()?
        .iter()
        .map(|f| f.get("metrics")?.get("vmaf")?.as_f64())
        .collect()
}

/// Mean and 5th-percentile (worst 5% of frames) VMAF.
fn summarize_vmaf(mut scores: Vec<f64>) -> Option<(f64, f64)> {
    if scores.is_empty() {
        return None;
    }
    scores.sort_by(f64::total_cmp);
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let p5 = scores[(scores.len() / 20).saturating_sub(1).min(scores.len() - 1)];
    Some((mean, p5))
}

/// The deepest folder holding all `paths` (a file counts as its folder). Paths on different
/// drives have none: the first one's folder is used.
fn common_root(paths: &[PathBuf]) -> PathBuf {
    let folder = |p: &PathBuf| if p.is_dir() { p.clone() } else { p.parent().map_or_else(|| p.clone(), Path::to_path_buf) };
    let mut iter = paths.iter().map(folder);
    let Some(mut root) = iter.next() else { return PathBuf::new() };
    for p in iter {
        while !p.starts_with(&root) {
            match root.parent() {
                Some(up) => root = up.to_path_buf(),
                None => return paths.first().map(folder).unwrap_or_default(),
            }
        }
    }
    root
}

/// Default backup folder: next to the scanned folder, named `<folder> (originals)`.
/// For a drive root (no parent), a `mediaopti originals` folder inside it.
fn default_backup_root(scanned: &Path) -> PathBuf {
    match (scanned.parent(), scanned.file_name()) {
        (Some(parent), Some(name)) => parent.join(format!("{} (originals)", name.to_string_lossy())),
        _ => scanned.join("mediaopti originals"),
    }
}

/// Size and modification time, to detect a file that changed during processing.
fn file_state(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
}

/// Content-hashes files on a few threads (disk-bound; more threads would only seek more).
fn hash_files_parallel(files: &[PathBuf]) -> Vec<(PathBuf, Option<ContentHash>)> {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|f| (f.clone(), history::hash_file(f).ok()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

struct ScanResult {
    files: Vec<PathBuf>,
    /// Single-frame GIFs (handled as images).
    static_gifs: HashSet<PathBuf>,
    /// Size of each entry of `files`, same order.
    sizes: Vec<u64>,
    cleaned_orphans: usize,
    unreadable: usize,
    messages: Vec<String>,
}

/// Recursively finds videos (skipping hidden folders like the old glob did), deletes leftover
/// temp files and sorts the queue by size, largest first. File sizes come from the directory
/// listing itself (on Windows `DirEntry::metadata` needs no extra system call).
fn scan_folder(roots: &[PathBuf], exclude: Option<&Path>, temp: Option<&Path>) -> ScanResult {
    let mut messages = Vec::new();
    let mut cleaned_orphans = 0;
    // Work folders of runs that were killed or crashed.
    let mut stale = vec![std::env::temp_dir()];
    stale.extend(temp.map(Path::to_path_buf));
    for root in roots {
        stale.extend(images::volume_root(root).map(|r| r.join(images::DRIVE_WORK_DIR)));
    }
    stale.sort();
    stale.dedup();
    let cleaned_work: usize = stale.iter().map(|d| system::clean_stale_work(d)).sum();
    if cleaned_work > 0 {
        messages.push(format!("Removed {cleaned_work} leftover work folder(s) from an earlier run that did not finish"));
    }
    let mut unreadable = 0;
    let mut with_sizes = Vec::new();
    let mut static_gifs = HashSet::new();

    // Folders are walked; a file given directly is its own one-entry walk.
    let walker = roots.iter().flat_map(|root| {
        walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || (!e.file_name().to_string_lossy().starts_with('.') && Some(e.path()) != exclude))
    });
    let mut seen = HashSet::new();

    for entry in walker {
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        if !entry.file_type().is_file() || !(args::is_video_file(entry.path()) || image::kind_of(entry.path()).is_some()) {
            continue;
        }
        if !seen.insert(entry.path().to_path_buf()) {
            continue; // dropped twice (a folder and a file inside it)
        }
        if is_orphan_temp(entry.path()) {
            match system::remove_file_if_exists(entry.path()) {
                Ok(()) => cleaned_orphans += 1,
                Err(err) => messages.push(format!(
                    "Could not remove temporary file {}: {err}",
                    entry.path().display()
                )),
            }
            continue;
        }
        match entry.metadata() {
            Ok(meta) if meta.len() > 0 => with_sizes.push((entry.into_path(), meta.len())),
            Ok(_) => {}
            Err(err) => messages.push(format!("Could not read {}: {err}", entry.path().display())),
        }
    }

    // Still or animated: each GIF's header is read, on several threads (a library can hold
    // tens of thousands of GIFs, and one at a time is slow on a hard disk).
    let gifs: Vec<&PathBuf> = with_sizes.iter().map(|(p, _)| p).filter(|p| args::is_gif(p)).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|scope| {
        let handles: Vec<_> = gifs
            .chunks(gifs.len().div_ceil(threads).max(1))
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .filter(|p| !std::fs::File::open(p).is_ok_and(|f| system::is_animated_and_valid_gif(std::io::BufReader::new(f))))
                        .map(|p| (*p).clone())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            static_gifs.extend(h.join().unwrap_or_default());
        }
    });

    with_sizes.sort_by_key(|&(_, size)| std::cmp::Reverse(size));
    let (files, sizes) = with_sizes.into_iter().unzip();
    ScanResult {
        files,
        static_gifs,
        sizes,
        cleaned_orphans,
        unreadable,
        messages,
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
        assert!(may_start_encoder(0, mem(1, 1)));
        // Further encoders need headroom for a 4K encode.
        assert!(!may_start_encoder(1, mem(4, 8)));
        assert!(!may_start_encoder(1, mem(10, 1)));
        assert!(may_start_encoder(1, mem(10, 4)));
        assert!(may_start_encoder(3, None));
    }

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

    #[test]
    fn generated_and_orphan_names() {
        assert!(is_generated_name(Path::new(r"C:\v\a.mediaopti.w1.mp4")));
        assert!(is_generated_name(Path::new(r"C:\v\clip.TMP.mp4")));
        assert!(is_generated_name(Path::new(r"C:\v\clip-H264.mkv")));
        assert!(!is_generated_name(Path::new(r"C:\v\clip.mp4")));
        assert!(is_orphan_temp(Path::new(r"C:\v\x.BatchViProc.w2.mp4")));
        assert!(is_orphan_temp(Path::new(r"C:\v\x.mediaopti.i12.png")));
        assert!(!is_orphan_temp(Path::new(r"C:\v\notes.mediaopti.final.mp4")), "user file");
        assert!(!is_orphan_temp(Path::new(r"C:\v\my.batchviproc.mp4")), "user file");
    }
}
