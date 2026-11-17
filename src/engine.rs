//! Batch engine: folder scanning, the work queue, parallel ffmpeg workers with retries
//! and fallbacks, GPU telemetry and state broadcasting to the UI.

use crate::args::{self, ImageMode, Mode, Options, Originals, RawOptions};
use crate::image;
use crate::history::{self, ContentHash, History};
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

mod batch;
mod ffmpeg;
mod images;
mod queue;
mod records;
mod replace;
mod scan;
mod video;
mod vmaf;
use batch::*;
use ffmpeg::*;
use queue::*;
use replace::*;
use scan::*;
use vmaf::*;

pub use ffmpeg::locate_ffmpeg;

const STATE_THROTTLE: Duration = Duration::from_millis(250);
/// Kept free on a video's drive besides room for its new version.
const VIDEO_DISK_RESERVE: u64 = 4 * 1024 * 1024 * 1024;
const HISTORY_SAVE_DELAY: Duration = Duration::from_secs(2);

/// Memory needed before starting another ffmpeg while others run. A single 4K AV1 encode
/// (multipass + 32-frame lookahead) was measured at up to ~5.3 GB of commit; running several
/// in parallel once exhausted the system commit limit and crashed every process. Measured
/// again 2026-11-16 (peak commit): 4K 4.9 GB (CPU decode) / 4.1 GB (GPU decode), 1440p AV1
/// 3.4 / 2.6 GB, 1080p 1.4 GB.
const MIN_FREE_COMMIT: u64 = 6 * 1024 * 1024 * 1024;
const MIN_FREE_PHYSICAL: u64 = 2 * 1024 * 1024 * 1024;
/// How long a newly started ffmpeg is given to allocate its buffers before the next
/// worker re-checks free memory (only when memory is not plentiful: enough for every job
/// to start at once, `MIN_FREE_COMMIT` and `MIN_FREE_VRAM_MB` per job).
const ADMISSION_WARMUP: Duration = Duration::from_secs(4);
/// Free commit needed before starting a VMAF measurement while other work runs (a 4K
/// measurement peaked at ~2.5 GB).
const VMAF_MIN_FREE_COMMIT: u64 = 4 * 1024 * 1024 * 1024;
/// Free GPU memory needed before starting another NVENC encode. One 4K AV1 encode with GPU
/// decoding was measured at ~2.5 GB (1080p: ~0.9 GB); 6-8 parallel 4K encodes ran a 16 GB
/// RTX 5070 Ti out of memory.
const MIN_FREE_VRAM_MB: u64 = 3 * 1024;

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
    /// CPU files in a row whose tools could not run (`ImageResult::ToolUnavailable`).
    tools_failing: usize,
}

#[derive(Clone, Debug)]
pub struct WorkerDto {
    pub file: String,
    pub fps: i64,
    pub speed: f64,
    pub percent: i64,
    /// What it does now ("encoding", "measuring quality at +4"...).
    pub step: String,
    /// Seconds since it started this file.
    pub secs: u64,
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
    /// CPU files running now, in the order they started.
    pub cpu_jobs: Vec<CpuJob>,
    /// This run by type (videos, audio, documents, images): finished, total, running now.
    pub type_progress: [(usize, usize, usize); 4],
    /// CPU work is waiting for free memory.
    pub cpu_waiting_memory: bool,
    /// Media files found so far by the scan running now.
    pub scan_found: usize,
    pub telemetry: TelemetryDto,
}

/// A CPU file running now: its name, seconds so far, its parts done (pages of a comic, images
/// of a PDF) and its current step with the share done, when known.
#[derive(Clone, Debug)]
pub struct CpuJob {
    pub name: String,
    pub secs: u64,
    pub parts: Option<(usize, usize)>,
    pub phase: Option<(&'static str, Option<f32>)>,
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
    /// Each video worker's file and when it started it (kept while its row comes and goes).
    worker_files: BTreeMap<u32, (String, Instant)>,
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
    /// The running jobs' token (a child of `cancel`): Pause cancels it (every process ends
    /// and frees its memory), and the jobs' files go back to the front of the queue.
    release: CancellationToken,
    /// Video jobs that Pause ended, with what they had finished (taken up on Resume).
    resume: std::collections::HashMap<PathBuf, vmaf::VideoResume>,
    /// What each video being encoded holds besides its picture (`args::Streams`), read once
    /// per job: every attempt carries it over and the result is checked against it.
    job_streams: std::collections::HashMap<PathBuf, args::Streams>,
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
    /// Found at start-up, found again after pingo/beeim are downloaded.
    image_tools: Mutex<image::tools::Tools>,
    data_dir: PathBuf,
    /// Media files found so far by the running scan (shown while scanning).
    scan_found: Arc<std::sync::atomic::AtomicUsize>,
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
    /// Ended by a Pause that gave its memory back; the file is queued again.
    Released,
    Empty,
    Fatal,
}

impl Engine {
    pub fn new(
        ui: Box<dyn Ui>,
        data_dir: &Path,
        ffmpeg_path: PathBuf,
        rt: tokio::runtime::Handle,
    ) -> Arc<Self> {
        system::put_app_in_kill_on_exit_job();
        let history = History::load(data_dir.join("processedFiles.txt"));
        let placeholders = system::init_claims_journal(&data_dir.join("claimed-names.txt"));
        replace::init_pending_journal(&data_dir.join("pending-replacements.txt"));
        let image_tools = image::tools::Tools::discover(ffmpeg_path.clone(), data_dir);
        if placeholders > 0 {
            ui.log(format!("Removed {placeholders} empty name placeholder(s) left by an interrupted run"));
        }
        Arc::new(Self {
            ui,
            rt,
            ffmpeg_path,
            image_tools: Mutex::new(image_tools),
            data_dir: data_dir.to_path_buf(),
            scan_found: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
                worker_files: BTreeMap::new(),
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
                release: CancellationToken::new(),
                resume: std::collections::HashMap::new(),
                job_streams: std::collections::HashMap::new(),
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

    /// Runs blocking disk work off the async worker threads. A panic in `f` goes on in the
    /// caller, with its own message (a video job's panic then fails that file only).
    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> T {
        match self.rt.spawn_blocking(f).await {
            Ok(v) => v,
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            Err(err) => panic!("blocking file task ended: {err}"),
        }
    }

    pub fn log(&self, message: impl Into<String>) {
        self.ui.log(message.into());
    }

    pub fn snapshot(&self) -> StateDto {
        let mut s = self.lock().snapshot();
        s.scan_found = self.scan_found.load(Ordering::Relaxed);
        s
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

    // ---------------------------------------------------------------- external tools

    /// The external tools in use.
    pub(crate) fn tools(&self) -> image::tools::Tools {
        self.image_tools.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Downloads pingo and beeim if they are missing (they may not be bundled), then looks
    /// for the tools again. Blocking; returns the log lines.
    pub fn download_missing_tools(&self) -> Vec<String> {
        let lines = image::tools::download_missing(&self.tools(), &self.data_dir);
        if !lines.is_empty() {
            let found = image::tools::Tools::discover(self.ffmpeg_path.clone(), &self.data_dir);
            *self.image_tools.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = found;
        }
        lines
    }

    /// Looks for the tools again (after new versions were put in place).
    pub fn reload_tools(&self) {
        let found = image::tools::Tools::discover(self.ffmpeg_path.clone(), &self.data_dir);
        *self.image_tools.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = found;
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// The external tools found at start-up, and the missing ones.
    pub fn tool_lists(&self) -> (Vec<&'static str>, Vec<&'static str>) {
        let tools = self.tools();
        image::tools::NAMES.iter().partition(|n| tools.has(n))
    }
}
