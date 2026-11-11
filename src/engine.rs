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
    pub images_total_this_run: usize,
    pub images_done_this_run: usize,
    /// Image workers busy right now.
    pub images_active: usize,
    pub telemetry: TelemetryDto,
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
    image_queue: Vec<PathBuf>,
    image_cursor: usize,
    images_active: usize,
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
            Some(image::Kind::Gif) => size < self.options.gif_min_bytes || self.static_gifs.contains(file),
            Some(_) => true,
            None => false,
        }
    }

    /// Content-history request for a file under the current settings.
    fn want(&self, file: &Path, size: u64) -> history::Want {
        if !self.is_image_job(file, size) {
            history::Want::Video
        } else if self.images_lossy() {
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

    /// Rebuilds the work queues from the scanned files, history and this run's failures.
    fn rebuild_queue(&mut self) {
        let (mut queue, mut images) = (Vec::new(), Vec::new());
        let (mut known, mut generated, mut small_gifs) = (0, 0, 0);
        let lossy = self.images_lossy();
        for (i, file) in self.files.iter().enumerate() {
            let size = self.file_sizes.get(i).copied().unwrap_or(u64::MAX);
            if is_generated_name(file) {
                generated += 1;
            } else if self.is_image_job(file, size) {
                if self.options.images == ImageMode::Off {
                    // Not remembered: they may be optimized once images are switched on.
                    if args::is_gif(file) {
                        small_gifs += 1;
                    }
                } else if self.history.contains_image(file, lossy) {
                    known += 1;
                } else if !self.failed.contains(file) {
                    images.push(file.clone());
                }
            } else if self.history.contains(file) {
                known += 1;
            } else if !self.failed.contains(file) {
                queue.push(file.clone());
            }
        }
        self.pending_queue = queue;
        self.queue_cursor = 0;
        self.image_queue = images;
        self.image_cursor = 0;
        self.skipped_known = known;
        self.skipped_generated = generated;
        self.skipped_small_gifs = small_gifs;
    }

    fn remaining_pending(&self) -> usize {
        self.pending_queue.len().saturating_sub(self.queue_cursor)
            + self.image_queue.len().saturating_sub(self.image_cursor)
            + self.in_progress.len()
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
            images_total_this_run: self.run.images_total,
            images_done_this_run: self.run.images_done,
            images_active: self.images_active,
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
                image_queue: Vec::new(),
                image_cursor: 0,
                images_active: 0,
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
            let lossy = inner.images_lossy();
            files
                .iter()
                .zip(sizes)
                .filter(|(f, s)| inner.history.has_size(**s) && !is_generated_name(f))
                .map(|(f, s)| (f, inner.want(f, *s)))
                .filter(|(f, want)| match want {
                    history::Want::Video => !inner.history.contains(f),
                    _ => !inner.history.contains_image(f, lossy),
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

    pub fn is_working(&self) -> bool {
        self.lock().working
    }

    /// Selects a folder and scans it for videos.
    pub async fn load_dir(self: &Arc<Self>, dir: PathBuf) {
        if self.is_working() {
            self.log("Stop processing before choosing another folder");
            return;
        }
        if !dir.is_dir() {
            self.log(format!("Folder not found: {}", dir.display()));
            return;
        }

        {
            let mut inner = self.lock();
            inner.dir = Some(dir.clone());
            inner.files.clear();
            inner.file_sizes.clear();
            inner.pending_queue.clear();
            inner.queue_cursor = 0;
            inner.image_queue.clear();
            inner.image_cursor = 0;
            inner.static_gifs.clear();
            inner.reset_run_tracking();
            inner.run = RunStats::default();
            inner.status = Status::Scanning;
        }
        self.log(format!("Scanning {}", dir.display()));
        self.send_state(true);

        let scan_dir = dir.clone();
        // A backup folder inside the scanned folder must not be scanned (or optimized again).
        let exclude = self.lock().backup_root();
        let scan = self.rt.spawn_blocking(move || scan_folder(&scan_dir, exclude.as_deref())).await;

        match scan {
            Ok(result) => {
                for msg in &result.messages {
                    self.log(msg.clone());
                }
                if result.unreadable > 0 {
                    self.log(format!(
                        "{} files or folders could not be read (permissions or broken links) and were skipped",
                        result.unreadable
                    ));
                }
                let recognised = self.recognise_by_content(&result.files, &result.sizes).await;
                if recognised > 0 {
                    self.log(format!(
                        "{recognised} moved, renamed or copied files recognised as already processed"
                    ));
                }
                let (known, generated) = {
                    let mut inner = self.lock();
                    inner.files = result.files;
                    inner.file_sizes = result.sizes;
                    inner.static_gifs = result.static_gifs;
                    inner.reset_run_tracking();
                    inner.rebuild_queue();
                    inner.run = RunStats::default();
                    inner.status = Status::Ready;
                    (inner.skipped_known, inner.skipped_generated)
                };
                let found = {
                    let inner = self.lock();
                    format!(
                        "Found {} files in {} ({} videos and {} images to process)",
                        inner.files.len(),
                        dir.display(),
                        inner.pending_queue.len(),
                        inner.image_queue.len()
                    )
                };
                self.log(found);
                if result.cleaned_orphans > 0 {
                    self.log(format!(
                        "Removed {} leftover temporary (.mediaopti) file(s)",
                        result.cleaned_orphans
                    ));
                }
                if known > 0 {
                    self.log(format!("{known} files are already in the processed history"));
                }
                let small_gifs = self.lock().skipped_small_gifs;
                if small_gifs > 0 {
                    self.log(format!(
                        "{small_gifs} animated GIFs are under the size limit and will stay GIFs"
                    ));
                }
                if generated > 0 {
                    self.log(format!(
                        "{generated} generated or temporary files will be ignored"
                    ));
                }
            }
            Err(err) => {
                self.lock().status = Status::Error;
                self.log(format!("Could not scan folder: {err}"));
            }
        }
        self.send_state(true);
    }

    // ---------------------------------------------------------------- batch control

    /// Scans a folder in the background.
    pub fn spawn_load_dir(self: &Arc<Self>, dir: PathBuf) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.load_dir(dir).await });
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
                total: inner.pending_queue.len() + inner.image_queue.len(),
                images_total: inner.image_queue.len(),
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

        let (videos, images, cancel) = {
            let inner = self.lock();
            (inner.pending_queue.len(), inner.image_queue.len(), inner.cancel.clone())
        };
        let mut handles = Vec::new();
        let fatal = Arc::new(AtomicBool::new(false));
        if videos > 0 {
            self.start_gpu_polling();
            self.log(format!(
                "Starting {videos} videos | Encoder: {} | Workers: {} | Base CRF: {} | Preset: {}",
                options.encoder, options.concurrency, options.crf, options.preset
            ));
            let worker_count = (options.concurrency as usize).min(videos);
            for id in 1..=worker_count as u32 {
                let engine = Arc::clone(&self);
                let fatal = Arc::clone(&fatal);
                handles.push((format!("Worker {id}"), self.rt.spawn(async move { engine.run_worker(id, fatal).await })));
            }
        }
        if images > 0 {
            // Leave some CPU for the video encoders' decoding and the desktop.
            let cores = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
            let workers = cores.saturating_sub(2).clamp(1, images);
            let ctx = Arc::new(image::Ctx {
                tools: self.image_tools.clone(),
                lossy: options.images == ImageMode::Lossy,
                target: 93.0,
                lossy_min_gain: 0.03,
                strip: false,
                strict_alpha: false,
                parallel: workers > 1,
                timeout: Duration::from_secs(900),
                cancel,
            });
            self.log(format!(
                "Starting {images} images | {} | {workers} CPU workers",
                if ctx.lossy { "visually lossless (SSIMULACRA2 >= 93)" } else { "lossless" }
            ));
            self.log(self.image_tools.summary());
            for id in 1..=workers as u32 {
                let engine = Arc::clone(&self);
                let ctx = Arc::clone(&ctx);
                handles.push((format!("Image worker {id}"), self.rt.spawn(async move { engine.run_image_worker(id, ctx).await })));
            }
        }
        self.send_state(true);
        for (name, handle) in handles {
            // A worker that panicked must not vanish silently (release builds abort instead).
            if let Err(err) = handle.await {
                self.log(format!("[{name}] stopped unexpectedly: {err}"));
            }
        }

        let (was_stopped, status, run, saved) = {
            let mut inner = self.lock();
            let was_stopped = inner.stop_requested;
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
                    "Images: {} processed, {} made smaller with a visually lossless (lossy) pass",
                    run.images_done, run.images_lossy
                ));
            }
        }
        self.send_state(true);
    }

    async fn run_worker(self: Arc<Self>, worker_id: u32, fatal: Arc<AtomicBool>) {
        loop {
            {
                let inner = self.lock();
                if !inner.working || inner.stop_requested || fatal.load(Ordering::SeqCst) {
                    break;
                }
            }
            match self.process_one(worker_id).await {
                Outcome::Empty | Outcome::Stopped => break,
                Outcome::Fatal => {
                    fatal.store(true, Ordering::SeqCst);
                    self.lock().status = Status::Error;
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
    fn start_gpu_polling(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        let cancel = self.lock().cancel.clone();
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
            inner
                .claim_next()
                .map(|input| (inner.display_name(&input), input))
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
                    out_size = std::fs::metadata(&tmp).map_or(in_size, |m| m.len() as f64);
                    self.log(format!(
                        "[Worker {worker_id}] Quality retry for {display}: +{boost} CQ, {:.0}% of the original, VMAF {vmaf:.1} (worst 5%: {worst:.1})",
                        out_size / in_size * 100.0
                    ));
                }
                Retry::Rejected(reason) => {
                    self.remember(&input, &input).await;
                    self.lock().run.skipped_larger += 1;
                    self.log_remove(&tmp);
                    self.log(format!(
                        "[Worker {worker_id}] Output was not smaller for {display}; kept the original file ({reason})"
                    ));
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

        if final_target != input && final_target.exists() {
            self.remember(&input, &input).await;
            self.lock().run.skipped_larger += 1;
            self.log_remove(&tmp);
            self.log(format!(
                "[Worker {worker_id}] Target {} already exists; kept original {display}",
                final_target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
            self.send_state(false);
            return Outcome::Skipped;
        }

        if file_state(&input) != source_state {
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
        let finish = |engine: &Self, best: Option<(u32, f64, f64)>, notes: &[String]| -> Retry {
            match best {
                Some((boost, vmaf, worst)) => match std::fs::rename(&keep, tmp) {
                    Ok(()) => Retry::Accepted { boost, vmaf, worst },
                    Err(err) => {
                        engine.log_remove(&keep);
                        Retry::Rejected(format!("could not keep the retry output: {err}"))
                    }
                },
                None => Retry::Rejected(notes.join("; ")),
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
                    return finish(self, best, &notes);
                }
            }
            let size = std::fs::metadata(tmp).map_or(f64::MAX, |m| m.len() as f64);
            if size >= in_size * 0.99 {
                notes.push(format!("+{boost}: still {:.0}% of the original", size / in_size * 100.0));
                self.log_remove(tmp);
                if best.is_none() && !stepped_back && k + 1 < STEPS.len() && !tried[k + 1] {
                    k += 1;
                    continue;
                }
                return finish(self, best, &notes);
            }
            self.reset_worker(worker_id, format!("{display} (measuring quality at +{boost})"));
            self.send_state(false);
            let measured = match self.measure_vmaf(input, tmp, duration_sec).await {
                Err(Stopped) => {
                    self.log_remove(&keep);
                    return Retry::Stopped;
                }
                Ok(m) => m,
            };
            match measured {
                Ok((vmaf, worst)) if vmaf >= floor && worst >= floor - 5.0 => {
                    if let Err(err) = system::remove_file_if_exists(&keep).and_then(|()| std::fs::rename(tmp, &keep)) {
                        notes.push(format!("could not keep the +{boost} output: {err}"));
                        self.log_remove(tmp);
                        return finish(self, best, &notes);
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
                    return finish(self, best, &notes);
                }
                Ok((vmaf, worst)) => {
                    notes.push(format!("+{boost}: VMAF {vmaf:.1} (worst 5%: {worst:.1}) is below {floor:.0}"));
                    self.log_remove(tmp);
                    if best.is_none() && k > 0 && !tried[k - 1] && !stepped_back {
                        stepped_back = true;
                        k -= 1;
                        continue;
                    }
                    return finish(self, best, &notes);
                }
                Err(why) => {
                    notes.push(format!("the quality could not be measured: {why}"));
                    self.log_remove(tmp);
                    return finish(self, best, &notes);
                }
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
    ) -> Result<Result<(f64, f64), String>, Stopped> {
        let cancel = self.lock().cancel.clone();
        let windows: Vec<(f64, f64)> = if duration_sec > 30.0 {
            [0.2, 0.5, 0.8].iter().map(|f| ((duration_sec * f - 4.0).max(0.0), 8.0)).collect()
        } else {
            vec![(0.0, 0.0)] // whole video
        };
        let threads = std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).max(1));
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
                format!(
                    "[0:v:0]setpts=PTS-STARTPTS,format=yuv420p10le[d];\
                     [1:v:0]scale=max(2\\,trunc(iw/2)*2):max(2\\,trunc(ih/2)*2),setpts=PTS-STARTPTS,format=yuv420p10le[r];\
                     [d][r]libvmaf=shortest=1:n_subsample=2:n_threads={threads}:log_fmt=json:log_path={log_name}"
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
            let child = match cmd.spawn() {
                Ok(child) => child,
                Err(err) => return Ok(Err(format!("could not start ffmpeg: {err}"))),
            };
            system::assign_to_kill_on_exit_job(&child);
            // Dropping the future on Stop drops the child, which kills it (kill_on_drop).
            let output = tokio::select! {
                o = child.wait_with_output() => o,
                _ = cancel.cancelled() => {
                    let _ = system::remove_file_if_exists(&log_dir.join(&log_name));
                    return Err(Stopped);
                }
            };
            let log_path = log_dir.join(&log_name);
            let parsed = std::fs::read_to_string(&log_path).ok().and_then(|t| vmaf_frame_scores(&t));
            let _ = system::remove_file_if_exists(&log_path);
            match (output, parsed) {
                (Ok(o), Some(frames)) if o.status.success() && !frames.is_empty() => scores.extend(frames),
                (Ok(o), _) => {
                    let err = String::from_utf8_lossy(&o.stderr);
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
fn scan_folder(dir: &Path, exclude: Option<&Path>) -> ScanResult {
    let mut messages = Vec::new();
    let mut cleaned_orphans = 0;
    let mut unreadable = 0;
    let mut with_sizes = Vec::new();
    let mut static_gifs = HashSet::new();

    let walker = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0 || (!e.file_name().to_string_lossy().starts_with('.') && Some(e.path()) != exclude)
        });

    for entry in walker {
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        if !entry.file_type().is_file() || !(args::is_video_file(entry.path()) || image::kind_of(entry.path()).is_some()) {
            continue;
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
        if args::is_gif(entry.path()) {
            let animated = std::fs::File::open(entry.path())
                .is_ok_and(|f| system::is_animated_and_valid_gif(std::io::BufReader::new(f)));
            if !animated {
                static_gifs.insert(entry.path().to_path_buf());
            }
        }
        match entry.metadata() {
            Ok(meta) if meta.len() > 0 => with_sizes.push((entry.into_path(), meta.len())),
            Ok(_) => {}
            Err(err) => messages.push(format!("Could not read {}: {err}", entry.path().display())),
        }
    }

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
