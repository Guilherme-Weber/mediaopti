//! Batch engine: folder scanning, the work queue, parallel ffmpeg workers with retries
//! and fallbacks, GPU telemetry and state broadcasting to the UI.

use crate::args::{self, ImageMode, Mode, Options, Originals};
use crate::image;
use crate::history::{self, ContentHash, History};
use crate::progress::{self, Diagnostics, WorkerProgress};
use crate::system::{self, OriginalAttributes, PowerGuard};

use std::collections::{BTreeMap, HashSet, VecDeque};
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

pub use batch::cpu_slot_count;
pub use ffmpeg::locate_ffmpeg;
pub(crate) use queue::active_stage_limit;

const STATE_THROTTLE: Duration = Duration::from_millis(250);
const HISTORY_SAVE_DELAY: Duration = Duration::from_secs(2);

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
    /// Left as they were and remembered: nothing smaller to make, or never to be processed.
    kept: usize,
    /// Left for later, not remembered (no disk space, changed meanwhile, a hard link...).
    skipped: usize,
    failed: usize,
    images_total: usize,
    images_done: usize,
    images_lossy: usize,
    /// CPU files by stage (audio, documents, images): queued this run, and finished.
    stage_total: [usize; 3],
    stage_done: [usize; 3],
    /// Files of each kind (`format::Kind::index`) in a row whose tools could not run
    /// (`ImageResult::ToolUnavailable`): a missing qpdf leaves the PDFs, not the pictures.
    tools_failing: [usize; crate::format::Kind::COUNT],
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
    pub kept_this_run: usize,
    pub skipped_this_run: usize,
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
    queue: WorkQueue,
    /// When each CPU file being processed was started.
    cpu_started: std::collections::HashMap<PathBuf, Instant>,
    /// The progress of the CPU files that report it (comics, PDFs).
    cpu_progress: std::collections::HashMap<PathBuf, Arc<image::tools::Progress>>,
    cpu_waiting_memory: bool,
    history: History,
    history_save_pending: bool,
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
    /// Set by `cancel_scan` to stop an in-progress folder scan cleanly.
    scan_cancel: Arc<AtomicBool>,
    inner: Mutex<Inner>,
    budget: Arc<crate::budget::ResourceBudget>,
    video_runner: Arc<dyn ffmpeg::VideoRunner>,
    emit: Mutex<EmitThrottle>,
    /// Serializes encoder start-up so memory checks see the previous encoder's usage.
    admission: Arc<tokio::sync::Mutex<()>>,
    /// The newest settings sent by `apply_options_in_background` (older ones still waiting
    /// for the lock are dropped).
    options_generation: std::sync::atomic::AtomicU64,
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
        Self::new_with_runner(ui, data_dir, ffmpeg_path, rt, Arc::new(ffmpeg::FfmpegAdapter))
    }

    fn new_with_runner(
        ui: Box<dyn Ui>,
        data_dir: &Path,
        ffmpeg_path: PathBuf,
        rt: tokio::runtime::Handle,
        video_runner: Arc<dyn ffmpeg::VideoRunner>,
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
            scan_cancel: Arc::new(AtomicBool::new(false)),
            inner: Mutex::new(Inner {
                queue: WorkQueue::new(),
                cpu_started: std::collections::HashMap::new(),
                cpu_progress: std::collections::HashMap::new(),
                cpu_waiting_memory: false,
                history,
                history_save_pending: false,
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
                power: None,
                handles: Vec::new(),
                video_ids: std::collections::BTreeSet::new(),
                cpu_ids: std::collections::BTreeSet::new(),
                cpu: None,
                fatal: Arc::new(AtomicBool::new(false)),
                gpu_polling: false,
            }),
            budget: crate::budget::ResourceBudget::os(),
            video_runner,
            emit: Mutex::new(EmitThrottle {
                last_sent: None,
                pending: false,
            }),
            admission: Arc::new(tokio::sync::Mutex::new(())),
            options_generation: std::sync::atomic::AtomicU64::new(0),
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

    /// Runs an action of the window's off its thread: it may wait for the engine's lock while
    /// a long step holds it (a queue rebuild over a library of a million files takes most of
    /// a second), and the window must stay responsive meanwhile.
    pub fn in_background(self: &Arc<Self>, action: impl FnOnce(&Arc<Self>) + Send + 'static) {
        let engine = Arc::clone(self);
        self.rt.spawn_blocking(move || action(&engine));
    }

    /// Cancels an in-progress folder scan (safe to call anytime).
    pub fn cancel_scan(&self) {
        self.scan_cancel.store(true, Ordering::SeqCst);
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
