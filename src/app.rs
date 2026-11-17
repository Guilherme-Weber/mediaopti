//! Connects the Slint window (ui/app.slint) to the engine.
//!
//! The engine runs on tokio threads and reports through the [`Ui`] trait; updates are
//! collected in an inbox and applied on the UI thread in one batch per event-loop turn.

use crate::args::{RawOptions, NO_GIFS};
use crate::engine::{Engine, StateDto, Status, Ui};
use crate::system;
use crate::taskbar::{Taskbar, TaskbarProgress};

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

slint::include_modules!();

const MAX_LOG_LINES: usize = 500;

const ENCODERS: &[(&str, &str)] = &[
    ("av1_nvenc", "AV1 NVENC (10-bit GPU)"),
    ("hevc_nvenc", "HEVC NVENC (10-bit GPU)"),
    ("h264_nvenc", "H.264 NVENC (GPU)"),
    ("libx264", "x264 (CPU)"),
];
const JOBS: &[(u32, &str)] = &[
    (1, "1 (single stream)"),
    (2, "2 streams"),
    (3, "3 streams"),
    (4, "4 streams (fastest)"),
];
const QUALITY: &[(u32, &str)] = &[
    (20, "High quality (camera / raw)"),
    (23, "Balanced (web / re-encode)"),
    (26, "Compact (max space saver)"),
];
const SPEEDS: &[(&str, &str)] = &[
    ("slow", "Slow (NVENC p7, best)"),
    ("medium", "Medium (NVENC p7)"),
    ("veryfast", "Very fast (NVENC p5)"),
];
// Keep "Move to a backup folder" at index 1: the window shows the folder line for it.
const ORIGINALS: &[(u32, &str)] = &[
    (1, "Move to the Recycle Bin"),
    (2, "Move to a backup folder"),
    (0, "Delete permanently"),
];
const GIFS: &[(u32, &str)] = &[
    (1, "Over 1 MB to video"),
    (5, "Over 5 MB to video"),
    (0, "All animated GIFs to video"),
    (NO_GIFS, "Keep all GIFs as GIF"),
];
const IMAGES: &[(u32, &str)] = &[
    (2, "Visually lossless (SSIMULACRA2 ≥ 93)"),
    (1, "Lossless only (pixel-identical)"),
];
const PDFS: &[(u32, &str)] = &[
    (1, "Visually lossless images (pages checked)"),
    (0, "Lossless only"),
];
const AUDIO: &[(u32, &str)] = &[
    (2, "Visually lossless (≥ 93)"),
    (1, "Lossless only"),
];
const FIX_EXT: &[(u32, &str)] = &[
    (0, "Keep file names"),
    (1, "Rename to the real format"),
];
const QUALITY_CHECK: &[(u32, &str)] = &[
    (93, "VMAF 93 (recommended)"),
    (95, "VMAF 95 (stricter)"),
    (0, "Off (preset only)"),
];

// ------------------------------------------------------------------- settings

/// Saved settings. `serde(default)`: settings from batchviproc or older versions lack the
/// newer fields, which then take their defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    encoder: String,
    concurrency: u32,
    crf: u32,
    preset: String,
    /// 0 = delete, 1 = Recycle Bin, 2 = backup folder.
    originals: u32,
    /// Chosen backup folder; empty = next to the scanned folder.
    backup_dir: String,
    gif_min_mb: u32,
    vmaf_floor: u32,
    /// 1 = lossless, 2 = visually lossless.
    images: u32,
    /// Each type on or off.
    videos: bool,
    images_on: bool,
    audio_on: bool,
    docs: bool,
    /// Each lossless conversion on or off.
    conv_wav: bool,
    conv_img: bool,
    conv_cbr: bool,
    /// 1 = visually lossless PDF images, 0 = lossless only.
    lossy_docs: u32,
    /// Cover art: 1 = lossless, 2 = visually lossless.
    audio: u32,
    /// Folder for work files; empty = the system temp folder.
    temp_dir: String,
    /// 1 = rename misnamed pictures (a JPEG called .png) to their real format.
    fix_ext: u32,
    /// The side navigation shows only icons.
    nav_compact: bool,
    /// Advanced: GPU decoding allowed.
    gpu_decoding: bool,
    /// Advanced: the maximum-effort parts in use (`image::Max` field names).
    max_parts: Vec<String>,
    /// Advanced: leave out data tracks no container can carry over.
    drop_data: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            encoder: "av1_nvenc".into(),
            concurrency: 4,
            crf: 23,
            preset: "slow".into(),
            originals: 1,
            backup_dir: String::new(),
            gif_min_mb: 1,
            vmaf_floor: 93,
            images: 2,
            videos: true,
            images_on: true,
            audio_on: true,
            docs: true,
            conv_wav: true,
            conv_img: true,
            conv_cbr: true,
            lossy_docs: 1,
            audio: 2,
            temp_dir: String::new(),
            fix_ext: 0,
            nav_compact: false,
            gpu_decoding: true,
            max_parts: crate::image::Max::chosen().names().into_iter().map(String::from).collect(),
            drop_data: false,
        }
    }
}

/// Position of `value` in `choices`, falling back to the position of `default`.
fn position<K: PartialEq>(choices: &[(K, &str)], value: &K, default: &K) -> i32 {
    let find = |v: &K| choices.iter().position(|(k, _)| k == v);
    find(value).or_else(|| find(default)).unwrap_or(0) as i32
}

impl Prefs {
    fn load(path: &Path) -> Self {
        let Some(text) = std::fs::read_to_string(path).ok() else { return Self::default() };
        let Ok(mut p) = serde_json::from_str::<Prefs>(&text) else { return Self::default() };
        // Older settings: "Don't process" was a quality choice, conversions one switch.
        let old: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if p.images == 0 {
            (p.images_on, p.images) = (false, 2);
        }
        if p.audio == 0 {
            (p.audio_on, p.audio) = (false, 2);
        }
        if old.get("conv_wav").is_none() && old.get("convert").and_then(serde_json::Value::as_u64) == Some(0) {
            (p.conv_wav, p.conv_img, p.conv_cbr) = (false, false, false);
        }
        p
    }

    fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Reads the current selection from the window.
    fn from_window(w: &AppWindow) -> Self {
        let pick = |i: i32, len: usize| (i.max(0) as usize).min(len - 1);
        Self {
            encoder: ENCODERS[pick(w.get_encoder_index(), ENCODERS.len())].0.into(),
            concurrency: JOBS[pick(w.get_jobs_index(), JOBS.len())].0,
            crf: QUALITY[pick(w.get_quality_index(), QUALITY.len())].0,
            preset: SPEEDS[pick(w.get_speed_index(), SPEEDS.len())].0.into(),
            originals: ORIGINALS[pick(w.get_originals_index(), ORIGINALS.len())].0,
            backup_dir: w.get_backup_dir().into(),
            gif_min_mb: GIFS[pick(w.get_gifs_index(), GIFS.len())].0,
            vmaf_floor: QUALITY_CHECK[pick(w.get_check_index(), QUALITY_CHECK.len())].0,
            images: IMAGES[pick(w.get_images_index(), IMAGES.len())].0,
            videos: w.get_videos_on(),
            images_on: w.get_images_on(),
            audio_on: w.get_audio_on(),
            docs: w.get_docs_on(),
            conv_wav: w.get_conv_wav(),
            conv_img: w.get_conv_img(),
            conv_cbr: w.get_conv_cbr(),
            lossy_docs: PDFS[pick(w.get_pdf_index(), PDFS.len())].0,
            audio: AUDIO[pick(w.get_audio_index(), AUDIO.len())].0,
            temp_dir: w.get_temp_dir().into(),
            fix_ext: FIX_EXT[pick(w.get_fix_ext_index(), FIX_EXT.len())].0,
            nav_compact: w.get_nav_compact(),
            gpu_decoding: w.get_gpu_decoding(),
            max_parts: w.get_adv_parts().iter().filter(|p| p.on).map(|p| p.name.to_string()).collect(),
            drop_data: w.get_drop_data(),
        }
    }

    /// Shows the selection in the window; unknown saved values (e.g. the removed OpenCL
    /// encoder) fall back to the defaults.
    fn apply_to_window(&self, w: &AppWindow) {
        let d = Prefs::default();
        w.set_encoder_index(position(ENCODERS, &self.encoder.as_str(), &d.encoder.as_str()));
        w.set_jobs_index(position(JOBS, &self.concurrency, &d.concurrency));
        w.set_quality_index(position(QUALITY, &self.crf, &d.crf));
        w.set_speed_index(position(SPEEDS, &self.preset.as_str(), &d.preset.as_str()));
        w.set_originals_index(position(ORIGINALS, &self.originals, &d.originals));
        w.set_backup_dir(self.backup_dir.as_str().into());
        w.set_gifs_index(position(GIFS, &self.gif_min_mb, &d.gif_min_mb));
        w.set_check_index(position(QUALITY_CHECK, &self.vmaf_floor, &d.vmaf_floor));
        w.set_images_index(position(IMAGES, &self.images, &d.images));
        w.set_videos_on(self.videos);
        w.set_images_on(self.images_on);
        w.set_audio_on(self.audio_on);
        w.set_docs_on(self.docs);
        w.set_conv_wav(self.conv_wav);
        w.set_conv_img(self.conv_img);
        w.set_conv_cbr(self.conv_cbr);
        w.set_pdf_index(position(PDFS, &self.lossy_docs, &d.lossy_docs));
        w.set_audio_index(position(AUDIO, &self.audio, &d.audio));
        w.set_temp_dir(self.temp_dir.as_str().into());
        w.set_fix_ext_index(position(FIX_EXT, &self.fix_ext, &d.fix_ext));
        w.set_nav_compact(self.nav_compact);
        w.set_gpu_decoding(self.gpu_decoding);
        w.set_drop_data(self.drop_data);
        let parts: Vec<AdvPart> = crate::image::Max::PARTS
            .iter()
            .map(|(name, label, detail)| AdvPart {
                name: (*name).into(),
                label: (*label).into(),
                detail: (*detail).into(),
                on: self.max_parts.iter().any(|p| p == name),
            })
            .collect();
        w.set_adv_parts(ModelRc::new(VecModel::from(parts)));
        w.set_adv_default(self.advanced_are_defaults());
    }

    fn advanced_are_defaults(&self) -> bool {
        let d = Prefs::default();
        self.gpu_decoding == d.gpu_decoding && self.drop_data == d.drop_data && self.max_parts_sorted() == d.max_parts_sorted()
    }

    fn max_parts_sorted(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.max_parts.iter().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    fn to_raw(&self) -> RawOptions {
        RawOptions {
            crf: Some(self.crf as f64),
            preset: Some(self.preset.clone()),
            encoder: Some(self.encoder.clone()),
            concurrency: Some(self.concurrency as f64),
            originals: Some(self.originals),
            backup_dir: Some(self.backup_dir.clone()),
            gif_min_mb: Some(self.gif_min_mb),
            vmaf_floor: Some(self.vmaf_floor),
            images: Some(if self.images_on { self.images } else { 0 }),
            convert: None,
            conversions: Some(u32::from(self.conv_wav) | u32::from(self.conv_img) << 1 | u32::from(self.conv_cbr) << 2),
            videos: Some(u32::from(self.videos)),
            docs: Some(u32::from(self.docs)),
            lossy_docs: Some(self.lossy_docs),
            audio: Some(if self.audio_on { self.audio } else { 0 }),
            temp_dir: Some(self.temp_dir.clone()),
            fix_ext: Some(self.fix_ext),
            gpu_decoding: Some(u32::from(self.gpu_decoding)),
            max_parts: Some(self.max_parts.clone()),
            drop_data: Some(u32::from(self.drop_data)),
        }
    }
}

fn labels<K>(choices: &[(K, &str)]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        choices.iter().map(|(_, l)| SharedString::from(*l)).collect::<Vec<_>>(),
    ))
}

// ------------------------------------------------------------------- engine -> UI bridge

#[derive(Default)]
struct Inbox {
    state: Mutex<Option<StateDto>>,
    logs: Mutex<Vec<String>>,
    scheduled: AtomicBool,
}

struct SlintUi {
    inbox: Arc<Inbox>,
    file_log: crate::logfile::FileLog,
}

impl SlintUi {
    /// Queues one UI-thread update; further messages before it runs share it.
    fn schedule(&self) {
        if !self.inbox.scheduled.swap(true, Ordering::AcqRel) {
            let _ = slint::invoke_from_event_loop(|| with_controller(Controller::drain));
        }
    }
}

impl Ui for SlintUi {
    fn log(&self, message: String) {
        self.file_log.write_line(&message);
        let line = format!("[{}] {message}", system::local_time_hms());
        self.inbox.logs.lock().unwrap_or_else(PoisonError::into_inner).push(line);
        self.schedule();
    }

    fn state(&self, state: &StateDto) {
        *self.inbox.state.lock().unwrap_or_else(PoisonError::into_inner) = Some(state.clone());
        self.schedule();
    }
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

fn with_controller(f: impl FnOnce(&mut Controller)) {
    CONTROLLER.with(|c| {
        if let Some(controller) = c.borrow_mut().as_mut() {
            f(controller);
        }
    });
}

/// Lives on the UI thread; owns the window models.
struct Controller {
    engine: Arc<Engine>,
    queue_model: Rc<VecModel<QueueItem>>,
    /// The queue revision (and running state) the list was last loaded for.
    queue_shown: Option<(u64, bool)>,
    window: slint::Weak<AppWindow>,
    inbox: Arc<Inbox>,
    state: StateDto,
    log_model: Rc<VecModel<SharedString>>,
    worker_model: Rc<VecModel<WorkerRow>>,
    files_left: Rc<VecModel<FileRow>>,
    files_right: Rc<VecModel<FileRow>>,
    metric_model: Rc<VecModel<Metric>>,
    log_revision: i32,
    taskbar: Option<Taskbar>,
    taskbar_tried: bool,
    clock: RunClock,
}

/// Elapsed processing time of the current batch, not counting time spent paused.
#[derive(Default)]
struct RunClock {
    banked: std::time::Duration,
    running_since: Option<std::time::Instant>,
    active: bool,
}

impl RunClock {
    fn update(&mut self, status: Status) {
        let ticking = status == Status::Running;
        let in_batch = matches!(status, Status::Running | Status::Paused | Status::Stopping);
        if in_batch && !self.active {
            *self = RunClock { active: true, ..RunClock::default() };
        }
        if !in_batch {
            self.active = false;
        }
        match (ticking, self.running_since) {
            (true, None) => self.running_since = Some(std::time::Instant::now()),
            (false, Some(t)) => {
                self.banked += t.elapsed();
                self.running_since = None;
            }
            _ => {}
        }
    }

    fn elapsed(&self) -> std::time::Duration {
        self.banked + self.running_since.map(|t| t.elapsed()).unwrap_or_default()
    }
}

/// 75 s -> "1:15", 3725 s -> "1:02:05"
fn format_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Rough time left from the share done so far: "about 3 h 20 min", or nothing while too
/// early to tell. (Files run largest first, so early estimates run high.)
fn format_eta(elapsed: std::time::Duration, fraction: f64) -> Option<String> {
    if fraction < 0.02 || fraction >= 1.0 || elapsed.as_secs() < 60 {
        return None;
    }
    let left = (elapsed.as_secs_f64() * (1.0 - fraction) / fraction) as u64;
    let m = left.div_ceil(60);
    Some(match m {
        0..=1 => "about a minute".to_string(),
        2..=59 => format!("about {m} min"),
        60..=1439 => format!("about {} h {:02} min", m / 60, m % 60),
        _ => format!("about {} days {} h", m / 1440, m / 60 % 24),
    })
}

/// "1 video" / "3 videos"
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", group_thousands(n), if n == 1 { one } else { many })
}

impl Controller {
    fn drain(&mut self) {
        self.inbox.scheduled.store(false, Ordering::Release);
        let state = self.inbox.state.lock().unwrap_or_else(PoisonError::into_inner).take();
        let logs = std::mem::take(&mut *self.inbox.logs.lock().unwrap_or_else(PoisonError::into_inner));

        if !logs.is_empty() {
            for line in logs {
                self.log_model.push(line.into());
            }
            let excess = self.log_model.row_count().saturating_sub(MAX_LOG_LINES);
            for _ in 0..excess {
                self.log_model.remove(0);
            }
            self.log_revision += 1;
        }
        if let Some(state) = state {
            self.state = state;
        }
        self.render();
    }

    fn clear_log(&mut self) {
        self.log_model.set_vec(Vec::new());
        self.render();
    }

    /// The queue summary by type, and the file list when the queue changed (not while
    /// running: the list is hidden then, and reloading it would cost time on every change).
    fn render_queue(&mut self, w: &AppWindow) {
        let s = &self.state;
        let labels = [("Videos", "\u{E714}"), ("Audio", "\u{E8D6}"), ("Documents", "\u{E8A5}"), ("Images", "\u{EB9F}")];
        let counts: Vec<QueueCount> = labels
            .iter()
            .zip(s.queue_counts)
            .map(|((label, glyph), (n, bytes))| QueueCount {
                label: (*label).into(),
                count: group_thousands(n).into(),
                size: if n == 0 { "—".into() } else { format_bytes(bytes).into() },
                glyph: (*glyph).into(),
            })
            .collect();
        w.set_queue_counts(ModelRc::new(VecModel::from(counts)));
        let running = matches!(s.status, Status::Running | Status::Paused | Status::Stopping);
        let key = (s.queue_revision, running);
        if running || self.queue_shown == Some(key) {
            return;
        }
        self.queue_shown = Some(key);
        let items = self.engine.queue_preview(QUEUE_LIST_MAX);
        let waiting: usize = s.queue_counts.iter().map(|c| c.0).sum();
        w.set_queue_more(if waiting > items.len() {
            format!("first {} of {}", group_thousands(items.len()), group_thousands(waiting)).into()
        } else {
            plural(waiting, "file", "files").into()
        });
        self.queue_model.set_vec(
            items
                .into_iter()
                .map(|(name, kind, size)| QueueItem { name: name.into(), kind: kind.into(), size: format_bytes(size).into() })
                .collect::<Vec<_>>(),
        );
    }

    fn render(&mut self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        self.render_queue(&w);
        self.clock.update(self.state.status);
        let s = &self.state;
        let busy = matches!(
            s.status,
            Status::Running | Status::Paused | Status::Stopping | Status::Scanning
        );
        let processing = matches!(s.status, Status::Running | Status::Paused | Status::Stopping);
        let paused = s.status == Status::Paused;
        let has_folder = s.dir.is_some();
        let has_run = s.run_total > 0 || s.completed_this_run > 0;
        let has_logs = self.log_model.row_count() > 0;

        // A scanned folder without media has nothing to start: not "Ready".
        let empty_folder = s.status == Status::Ready && s.total_files == 0;
        if empty_folder {
            w.set_status("idle".into());
            w.set_status_label("Nothing to do".into());
        } else {
            w.set_status(status_key(s.status).into());
            w.set_status_label(status_label(s.status).into());
        }
        w.set_has_folder(has_folder);
        w.set_empty_folder(empty_folder);
        w.set_folder_title(match s.dir.as_deref() {
            Some(dir) => folder_name(dir).into(),
            None => "No folder selected".into(),
        });
        w.set_folder_path(s.dir.as_deref().unwrap_or_default().into());
        w.set_folder_summary(if has_folder || s.status == Status::Scanning {
            file_count_text(s).into()
        } else {
            "Choose one or more folders, or drop files and folders on this window. Subfolders are included.".into()
        });
        w.set_busy(busy);
        w.set_scanning(s.status == Status::Scanning);
        w.set_processing(processing);
        w.set_can_start(has_folder && !busy && s.pending_files > 0);
        w.set_stop_enabled(matches!(s.status, Status::Running | Status::Paused));
        w.set_paused(paused);
        // Settings can be changed whenever nothing runs, also before choosing a folder.
        w.set_show_options(!busy);
        w.set_show_stats(processing || has_run || s.status == Status::Error);
        w.set_show_log(processing || s.status == Status::Error || (has_run && has_logs));
        w.set_show_history(s.history_count > 0);
        w.set_history_text(history_text(s.history_count).into());
        w.set_log_revision(self.log_revision);

        // Metrics
        let t = &s.telemetry;
        let speed = if paused {
            "Paused".into()
        } else if processing && t.total_fps > 0 {
            format!("{} fps ({}x)", t.total_fps, t.total_speed)
        } else if processing && s.type_progress[0].0 < s.type_progress[0].1 {
            "Starting…".into()
        } else {
            "—".into()
        };
        let gpu = if processing {
            format!("{}% · {} GB", t.nvenc_util, t.vram_gb)
        } else {
            "—".into()
        };
        let metrics = [
            ("Queued", s.pending_files.to_string(), "\u{E8FD}"),
            ("Completed", format!("{}/{}", s.completed_this_run, s.run_total), "\u{E73E}"),
            ("Space saved", format_saved(s.space_saved_mb), "\u{E74E}"),
            ("Speed", speed, "\u{EC4A}"),
            ("NVENC / VRAM", gpu, "\u{E950}"),
            ("Failed", s.failed_this_run.to_string(), "\u{E783}"),
        ];
        sync_model(
            &self.metric_model,
            metrics
                .into_iter()
                .map(|(label, value, glyph)| Metric {
                    label: label.into(),
                    value: value.into(),
                    glyph: glyph.into(),
                })
                .collect(),
        );

        // Progress
        let active = if processing { t.active_progress_sum } else { 0.0 };
        let fraction = if s.run_total > 0 {
            ((s.completed_this_run as f64 + active) / s.run_total as f64).min(1.0)
        } else {
            0.0
        };
        w.set_overall_progress(fraction as f32);
        w.set_overall_percent(format!("{}%", (fraction * 100.0).round()).into());
        // While processing, the worker rows name the files; this line gives the time.
        w.set_current_file(if processing {
            let elapsed = format_duration(self.clock.elapsed());
            let running: usize = s.type_progress.iter().map(|t| t.2).sum();
            let files = format!(
                "All files · {} of {} · {running} running",
                group_thousands(s.completed_this_run),
                group_thousands(s.run_total)
            );
            let left = format_eta(self.clock.elapsed(), fraction).map(|eta| format!(" · {eta} left")).unwrap_or_default();
            match s.status {
                Status::Paused => format!("Paused · {files} · {elapsed} elapsed{left}"),
                Status::Stopping => "Stopping…".to_string(),
                _ => format!("{files} · {elapsed} elapsed{left}"),
            }
            .into()
        } else {
            idle_file_text(s).as_str().into()
        });
        // The taskbar thumbnail and Alt+Tab show progress while a batch runs.
        let pct = (fraction * 100.0).round();
        w.set_window_title(match s.status {
            Status::Running => format!("{pct}% · mediaopti"),
            Status::Paused => format!("Paused {pct}% · mediaopti"),
            _ => "mediaopti".to_string(),
        }
        .into());
        // Rows: all files, then each type of this run (finished of total), then the files
        // being worked on (video encodes with their %, long CPU files with their pages).
        let mut workers: Vec<WorkerRow> = Vec::new();
        if processing {
            let row = |title: String, done: usize, total: usize, meta: String| WorkerRow {
                title: format!("{title} · {} of {}", group_thousands(done), group_thousands(total)).into(),
                meta: meta.into(),
                progress: if total == 0 { 0.0 } else { done as f32 / total as f32 },
            };
            let names = [("Videos", "encoding"), ("Audio", "running"), ("Documents", "running"), ("Images", "running")];
            for (i, ((name, verb), (done, total, active))) in names.iter().zip(s.type_progress).enumerate() {
                if total == 0 {
                    continue;
                }
                let meta = if done >= total {
                    "done".to_string()
                } else if i > 0 && s.cpu_waiting_memory && active == 0 {
                    "waiting for memory".to_string()
                } else if active > 0 {
                    format!("{active} {verb}")
                } else if i > 0 {
                    "waiting for its turn".to_string()
                } else {
                    String::new()
                };
                workers.push(row((*name).into(), done, total, meta));
            }
        }
        sync_model(&self.worker_model, workers);
        // Every file being worked on, videos first, one line each in the same form: its step,
        // its share done when known, and its time so far.
        let mut files: Vec<FileRow> = Vec::new();
        if processing {
            for wk in &t.workers {
                let share = (wk.percent > 0).then(|| wk.percent as f32 / 100.0);
                let fps = (wk.fps > 0).then(|| format!("{} fps ({}x)", wk.fps, wk.speed));
                files.push(file_row(&wk.file, &wk.step, share, fps, wk.secs));
            }
            for job in &s.cpu_jobs {
                let (step, share) = match (job.phase, job.parts) {
                    (Some((phase, _)), Some((done, total))) => (format!("{phase} {done} of {total}"), Some(done as f32 / total.max(1) as f32)),
                    (Some((phase, share)), None) => (phase.to_string(), share),
                    (None, _) => ("starting".to_string(), None),
                };
                files.push(file_row(&job.name, &step, share, None, job.secs));
            }
        }
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for (i, row) in files.into_iter().enumerate() {
            if i % 2 == 0 { left.push(row) } else { right.push(row) }
        }
        sync_model(&self.files_left, left);
        sync_model(&self.files_right, right);

        // Taskbar button progress
        let taskbar_pct = (fraction * 100.0).clamp(1.0, 99.0) as u64;
        let progress = if s.status == Status::Running && s.run_total > 0 {
            TaskbarProgress::Normal(taskbar_pct)
        } else if paused && s.run_total > 0 {
            TaskbarProgress::Paused(taskbar_pct)
        } else if s.status == Status::Scanning {
            TaskbarProgress::Indeterminate
        } else if s.status == Status::Error {
            TaskbarProgress::Error
        } else {
            TaskbarProgress::None
        };
        // The window may not exist yet on the first renders: try again until it does (once
        // it does, a failure to get the taskbar is final).
        if self.taskbar.is_none() && !self.taskbar_tried {
            if let Some(h) = hwnd(&w) {
                self.taskbar_tried = true;
                self.taskbar = Taskbar::new(h);
            }
        }
        if let Some(taskbar) = &mut self.taskbar {
            taskbar.set(progress);
        }
    }
}

/// Updates rows in place when the length is unchanged (avoids rebuilding list items).
/// One file's line: "verifying 45% · 120 fps · 0:12"; the bar only when the share is known
/// (a step with parts already says "12 of 150", so no percentage is added).
fn file_row(name: &str, step: &str, share: Option<f32>, extra: Option<String>, secs: u64) -> FileRow {
    let mut meta = step.to_string();
    if let Some(f) = share.filter(|_| !step.contains(" of ")) {
        meta += &format!(" {:.0}%", f * 100.0);
    }
    for part in extra.into_iter().chain(std::iter::once(format_duration(std::time::Duration::from_secs(secs)))) {
        meta += " · ";
        meta += &part;
    }
    FileRow { name: name.into(), meta: meta.into(), progress: share.unwrap_or(-1.0) }
}

fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    } else {
        model.set_vec(rows);
    }
}

fn hwnd(w: &AppWindow) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = w.window().window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

// ------------------------------------------------------------------- run

/// Command-line options: `mediaopti.exe [--start] [folder]`.
#[derive(Default)]
pub struct Launch {
    pub folder: Option<PathBuf>,
    pub start: bool,
    /// The page shown first (`--page=settings` / `--page=about` / `--page=advanced`).
    pub page: i32,
}

impl Launch {
    pub fn from_args(args: impl Iterator<Item = String>) -> Self {
        let mut launch = Launch::default();
        for arg in args {
            if arg == "--start" {
                launch.start = true;
            } else if let Some(page) = arg.strip_prefix("--page=") {
                launch.page = match page {
                    "settings" => 1,
                    "about" => 2,
                    "advanced" => 3,
                    _ => 0,
                };
            } else if arg.starts_with("--") {
                // Unknown flag: ignore rather than treating it as a folder.
            } else if launch.folder.is_none() {
                launch.folder = Some(arg.into());
            }
        }
        launch
    }
}

pub fn run(data_dir: &Path, launch: Launch) -> Result<(), slint::PlatformError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("mediaopti-worker")
        .build()
        .expect("failed to start the async runtime");

    let window = AppWindow::new()?;
    window.global::<Theme>().set_icon_font(icon_font().into());

    let inbox = Arc::new(Inbox::default());
    let engine = Engine::new(
        Box::new(SlintUi {
            inbox: inbox.clone(),
            file_log: crate::logfile::FileLog::open(&data_dir.join("logs")),
        }),
        data_dir,
        crate::engine::locate_ffmpeg(),
        runtime.handle().clone(),
    );

    // Settings
    let prefs_path = data_dir.join("ui-settings.json");
    window.set_encoder_options(labels(ENCODERS));
    window.set_jobs_options(labels(JOBS));
    window.set_quality_options(labels(QUALITY));
    window.set_speed_options(labels(SPEEDS));
    window.set_originals_options(labels(ORIGINALS));
    window.set_gifs_options(labels(GIFS));
    window.set_check_options(labels(QUALITY_CHECK));
    window.set_images_options(labels(IMAGES));
    window.set_pdf_options(labels(PDFS));
    window.set_fix_ext_options(labels(FIX_EXT));
    window.set_system_temp(std::env::temp_dir().to_string_lossy().trim_end_matches('\\').into());
    window.set_audio_options(labels(AUDIO));
    let prefs = Prefs::load(&prefs_path);
    prefs.apply_to_window(&window);
    engine.apply_options(prefs.to_raw());

    // Models
    let log_model = Rc::new(VecModel::<SharedString>::default());
    let worker_model = Rc::new(VecModel::<WorkerRow>::default());
    let metric_model = Rc::new(VecModel::<Metric>::default());
    window.set_log_lines(log_model.clone().into());
    window.set_workers(worker_model.clone().into());
    let (files_left, files_right) = (Rc::new(VecModel::<FileRow>::default()), Rc::new(VecModel::<FileRow>::default()));
    window.set_files_left(files_left.clone().into());
    window.set_files_right(files_right.clone().into());
    window.set_metrics(metric_model.clone().into());
    let queue_model = Rc::new(VecModel::<QueueItem>::default());
    window.set_queue_items(queue_model.clone().into());

    // About page.
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    let (found, missing) = engine.tool_lists();
    window.set_tools_found(if found.is_empty() { "none".into() } else { found.join(", ").into() });
    window.set_tools_missing(missing.join(", ").into());
    window.set_data_dir(data_dir.to_string_lossy().as_ref().into());

    // pingo and beeim may not be bundled: if they are missing, download them from their
    // author's site in the background, then show the tools found again.
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let lines = engine.download_missing_tools();
            if lines.is_empty() {
                return;
            }
            for line in lines {
                engine.log(line);
            }
            let (found, missing) = engine.tool_lists();
            let (found, missing) = (found.join(", "), missing.join(", "));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_tools_found(found.as_str().into());
                    w.set_tools_missing(missing.as_str().into());
                }
            });
        });
    }

    // Advanced options (reached from About).
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_toggle_part(move |index| {
            let Some(w) = weak.upgrade() else { return };
            let parts = w.get_adv_parts();
            let Some(mut part) = parts.row_data(index.max(0) as usize) else { return };
            part.on = !part.on;
            parts.set_row_data(index as usize, part);
            save_advanced(&w, &engine, &prefs_path);
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_advanced_changed(move || {
            if let Some(w) = weak.upgrade() {
                save_advanced(&w, &engine, &prefs_path);
            }
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_restore_advanced(move || {
            let Some(w) = weak.upgrade() else { return };
            let d = Prefs::default();
            let prefs = Prefs { gpu_decoding: d.gpu_decoding, max_parts: d.max_parts, drop_data: d.drop_data, ..Prefs::from_window(&w) };
            prefs.apply_to_window(&w);
            prefs.save(&prefs_path);
            engine.apply_options_in_background(prefs.to_raw());
        });
    }

    // Updates: a check when the app opens (it also removes what the last update left).
    crate::update::app_exe();
    spawn_update_check(&engine, window.as_weak(), data_dir.to_path_buf());
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let scratch = data_dir.to_path_buf();
        window.on_check_update(move || {
            if let Some(w) = weak.upgrade() {
                w.set_update_text("Checking for updates…".into());
            }
            spawn_update_check(&engine, weak.clone(), scratch.clone());
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        window.on_install_update(move || install_update(&engine, &weak));
    }
    window.on_open_release_page(|| {
        if let Some(release) = crate::update::found() {
            let _ = system::spawn_detached(std::process::Command::new("explorer").arg(&release.page));
        }
    });

    CONTROLLER.with(|c| {
        *c.borrow_mut() = Some(Controller {
            engine: engine.clone(),
            queue_model,
            queue_shown: None,
            window: window.as_weak(),
            inbox,
            state: engine.snapshot(),
            log_model,
            worker_model,
            files_left,
            files_right,
            metric_model,
            log_revision: 0,
            taskbar: None,
            taskbar_tried: false,
            clock: RunClock::default(),
        });
    });

    // Callbacks
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        window.on_choose_folder(move || {
            let Some(w) = weak.upgrade() else { return };
            let handle = w.window().window_handle();
            // Several folders can be picked at once (Ctrl or Shift + click).
            let picked = rfd::FileDialog::new()
                .set_title("Choose one or more folders (Ctrl + click for several)")
                .set_parent(&handle)
                .pick_folders()
                .unwrap_or_default();
            match picked.len() {
                0 => engine.log("No folder selected"),
                1 => engine.spawn_load_dir(picked.into_iter().next().unwrap_or_default()),
                _ => engine.spawn_load_items(picked),
            }
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_start(move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_update_running() {
                return;
            }
            let prefs = Prefs::from_window(&w);
            prefs.save(&prefs_path);
            engine.start(prefs.to_raw());
        });
    }
    {
        let engine = engine.clone();
        window.on_stop(move || engine.stop());
    }
    {
        let engine = engine.clone();
        window.on_toggle_pause(move || engine.toggle_pause());
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        window.on_forget_history(move || {
            let Some(w) = weak.upgrade() else { return };
            let handle = w.window().window_handle();
            let confirmed = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("Forget processed history")
                .set_description(
                    "Forget the processed history for this app? Existing video files will not be changed.",
                )
                .set_buttons(rfd::MessageButtons::YesNo)
                .set_parent(&handle)
                .show()
                == rfd::MessageDialogResult::Yes;
            if confirmed {
                engine.clear_history();
            }
        });
    }
    window.on_clear_log(|| with_controller(Controller::clear_log));
    {
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_nav_toggled(move || {
            if let Some(w) = weak.upgrade() {
                Prefs::from_window(&w).save(&prefs_path);
            }
        });
    }
    {
        let engine = engine.clone();
        window.on_clear_queue(move || engine.clear_queue());
    }
    {
        let dir = data_dir.to_path_buf();
        window.on_open_data_folder(move || {
            let _ = system::spawn_detached(std::process::Command::new("explorer").arg(&dir));
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_restore_defaults(move || {
            let Some(w) = weak.upgrade() else { return };
            // The layout of the window isn't a setting, and Advanced options have their own
            // Restore: both stay as they are.
            let current = Prefs::from_window(&w);
            let prefs = Prefs { nav_compact: current.nav_compact, gpu_decoding: current.gpu_decoding, max_parts: current.max_parts, drop_data: current.drop_data, ..Prefs::default() };
            prefs.apply_to_window(&w);
            prefs.save(&prefs_path);
            engine.apply_options_in_background(prefs.to_raw());
        });
    }
    {
        // The full activity log and any crash reports live here.
        let logs_dir = data_dir.join("logs");
        window.on_open_logs(move || {
            let _ = std::fs::create_dir_all(&logs_dir);
            let _ = system::spawn_detached(std::process::Command::new("explorer").arg(&logs_dir));
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        let last_encoder = RefCell::new(prefs.encoder.clone());
        window.on_settings_changed(move || {
            let Some(w) = weak.upgrade() else { return };
            let mut prefs = Prefs::from_window(&w);
            // Switching to/from the CPU encoder adjusts parallel jobs.
            if *last_encoder.borrow() != prefs.encoder {
                if prefs.encoder == "libx264" {
                    prefs.concurrency = 1;
                } else if prefs.concurrency == 1 {
                    prefs.concurrency = Prefs::default().concurrency;
                }
                prefs.apply_to_window(&w);
                last_encoder.borrow_mut().clone_from(&prefs.encoder);
            }
            prefs.save(&prefs_path);
            engine.apply_options_in_background(prefs.to_raw());
        });
    }

    {
        let engine = engine.clone();
        let weak = window.as_weak();
        let prefs_path = prefs_path.clone();
        window.on_choose_temp_folder({
            let engine = engine.clone();
            let weak = window.as_weak();
            let prefs_path = prefs_path.clone();
            move || {
                let Some(w) = weak.upgrade() else { return };
                let handle = w.window().window_handle();
                let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose the folder for temporary files")
                    .set_parent(&handle)
                    .pick_folder()
                else {
                    return;
                };
                w.set_temp_dir(dir.to_string_lossy().as_ref().into());
                let prefs = Prefs::from_window(&w);
                prefs.save(&prefs_path);
                engine.apply_options_in_background(prefs.to_raw());
            }
        });
        window.on_choose_backup_folder(move || {
            let Some(w) = weak.upgrade() else { return };
            let handle = w.window().window_handle();
            let Some(dir) = rfd::FileDialog::new()
                .set_title("Choose the folder for replaced originals")
                .set_parent(&handle)
                .pick_folder()
            else {
                return;
            };
            w.set_backup_dir(dir.to_string_lossy().as_ref().into());
            let prefs = Prefs::from_window(&w);
            prefs.save(&prefs_path);
            engine.apply_options_in_background(prefs.to_raw());
        });
    }

    // Files and folders dropped on the window are added to the queue (during a run too).
    // Each file arrives as its own event: they are collected for a moment, then added together.
    {
        use slint::winit_030::{winit::event::WindowEvent, EventResult, WinitWindowAccessor};
        let engine = engine.clone();
        let weak = window.as_weak();
        let dropped: Rc<std::cell::RefCell<Vec<PathBuf>>> = Rc::default();
        let timer = Rc::new(slint::Timer::default());
        window.window().on_winit_window_event(move |_, event| {
            let hover = |on: bool| {
                if let Some(w) = weak.upgrade() {
                    w.set_drop_hover(on);
                }
            };
            match event {
                // The software renderer repaints only what changed; after a minimize/restore
                // or resize Windows has dropped the window's pixels, so everything is redrawn.
                WindowEvent::Resized(_) | WindowEvent::Occluded(false) | WindowEvent::Focused(true) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_repaint_all(!w.get_repaint_all());
                    }
                }
                WindowEvent::HoveredFile(_) => hover(true),
                WindowEvent::HoveredFileCancelled => hover(false),
                WindowEvent::DroppedFile(path) => {
                    hover(false);
                    dropped.borrow_mut().push(path.clone());
                    let (dropped, engine) = (dropped.clone(), engine.clone());
                    timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(250), move || {
                        let items = std::mem::take(&mut *dropped.borrow_mut());
                        if !items.is_empty() {
                            engine.spawn_add_items(items);
                        }
                    });
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    window.set_page(launch.page);

    // First paint, then honour command-line options.
    with_controller(Controller::render);
    if let Some(dir) = launch.folder {
        let engine = engine.clone();
        let raw = prefs.to_raw();
        runtime.spawn(async move {
            engine.load_dir(dir).await;
            if launch.start {
                engine.run_batch(raw).await;
            }
        });
    }

    let result = window.run();

    engine.shutdown();
    Prefs::from_window(&window).save(&prefs_path);
    CONTROLLER.with(|c| c.borrow_mut().take());
    drop(window);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// Windows 11 ships "Segoe Fluent Icons"; Windows 10 has the same glyphs in "Segoe MDL2 Assets".
/// Saves the settings after a change on the Advanced page and shows whether they are the defaults.
fn save_advanced(w: &AppWindow, engine: &Arc<Engine>, prefs_path: &Path) {
    let prefs = Prefs::from_window(w);
    prefs.save(prefs_path);
    w.set_adv_default(prefs.advanced_are_defaults());
    engine.apply_options_in_background(prefs.to_raw());
}

/// Asks GitHub for a newer release in the background and shows the answer on the About page.
fn spawn_update_check(engine: &Arc<Engine>, weak: slint::Weak<AppWindow>, scratch: PathBuf) {
    let engine = engine.clone();
    std::thread::spawn(move || {
        crate::update::remove_leftovers();
        let result = crate::update::check(&scratch);
        if let Ok(Some(release)) = &result {
            engine.log(format!("mediaopti {} is available (About > Update and restart)", release.version));
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            match result {
                Ok(Some(release)) => {
                    w.set_update_version(release.version.as_str().into());
                    w.set_update_text(format!("Version {} is available.", release.version).into());
                }
                Ok(None) => {
                    w.set_update_version(SharedString::new());
                    w.set_update_text("mediaopti is up to date.".into());
                }
                Err(why) => w.set_update_text(format!("Could not check for updates: {why}.").into()),
            }
        });
    });
}

/// Downloads and installs the release found, then closes the app; `main` starts the new one.
fn install_update(engine: &Arc<Engine>, weak: &slint::Weak<AppWindow>) {
    let Some(w) = weak.upgrade() else { return };
    let Some(release) = crate::update::found() else { return };
    if w.get_busy() || w.get_processing() || w.get_update_running() {
        return;
    }
    w.set_update_running(true);
    w.set_update_text(format!("Downloading and installing version {}…", release.version).into());
    let (engine, weak) = (engine.clone(), weak.clone());
    std::thread::spawn(move || {
        engine.log(format!("Updating to mediaopti {}", release.version));
        let result = crate::update::install(&release);
        if let Err(why) = &result {
            engine.log(format!("Update failed: {why}; nothing was changed"));
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            match result {
                Ok(()) => {
                    crate::update::request_restart();
                    w.set_update_text("Restarting…".into());
                    let _ = slint::quit_event_loop();
                }
                Err(why) => {
                    w.set_update_running(false);
                    w.set_update_text(format!("The update failed: {why}. Nothing was changed.").into());
                }
            }
        });
    });
}

fn icon_font() -> &'static str {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    if Path::new(&windir).join("Fonts").join("SegoeIcons.ttf").exists() {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    }
}

// ------------------------------------------------------------------- text helpers

fn status_key(status: Status) -> &'static str {
    match status {
        Status::Idle => "idle",
        Status::Scanning => "scanning",
        Status::Ready => "ready",
        Status::Running => "running",
        Status::Paused => "paused",
        Status::Stopping => "stopping",
        Status::Done => "done",
        Status::Error => "error",
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Idle => "Idle",
        Status::Scanning => "Scanning",
        Status::Ready => "Ready",
        Status::Running => "Running",
        Status::Paused => "Paused",
        Status::Stopping => "Stopping",
        Status::Done => "Done",
        Status::Error => "Needs attention",
    }
}

fn format_mb(value: f64) -> String {
    if value >= 10.0 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.1}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Bytes as "812 KB", "45.2 MB", "1.3 GB".
fn format_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", (b / 1024.0).ceil())
    }
}

/// Files shown in the queue list (more would slow the window down).
const QUEUE_LIST_MAX: usize = 5000;

/// Space saved: MB below 1 GB, otherwise GB with one decimal.
fn format_saved(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{} MB", format_mb(mb))
    }
}

fn file_count_text(s: &StateDto) -> String {
    if s.status == Status::Scanning {
        return format!("Scanning… {} found so far", plural(s.scan_found, "media file", "media files"));
    }
    if s.total_files == 0 {
        return "No videos or images in this folder or its subfolders. Choose another folder.".into();
    }
    let mut parts = vec![
        plural(s.total_files, "file found", "files found"),
        format!("{} ready", group_thousands(s.pending_files)),
    ];
    if s.skipped_known > 0 {
        parts.push(format!("{} already processed", group_thousands(s.skipped_known)));
    }
    if s.skipped_small_gifs > 0 {
        parts.push(format!("{} small GIFs kept as GIF", group_thousands(s.skipped_small_gifs)));
    }
    if s.skipped_generated > 0 {
        parts.push(format!("{} ignored", group_thousands(s.skipped_generated)));
    }
    parts.join(" · ")
}

fn history_text(count: usize) -> String {
    format!("{} remembered", plural(count, "processed file is", "processed files are"))
}

/// 337426 -> "337,426"
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Last path component ("D:\\Videos\\Trip" -> "Trip"); a drive root stays as it is.
fn folder_name(dir: &str) -> String {
    Path::new(dir)
        .file_name()
        .map_or_else(|| dir.to_string(), |n| n.to_string_lossy().into_owned())
}

fn idle_file_text(s: &StateDto) -> String {
    match s.status {
        Status::Done if s.run_total > 0 => format!(
            "Batch complete · {} compressed · {} skipped · {} failed",
            s.compressed_this_run, s.skipped_larger_this_run, s.failed_this_run
        ),
        Status::Done => "Batch complete".into(),
        Status::Ready if s.pending_files > 0 => "Ready to process".into(),
        Status::Error => "Check the activity log".into(),
        _ => "No active file".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_megabytes_like_the_old_ui() {
        assert_eq!(format_mb(0.0), "0");
        assert_eq!(format_mb(3.04), "3");
        assert_eq!(format_mb(3.46), "3.5");
        assert_eq!(format_mb(192.4), "192");
        assert_eq!(format_saved(192.4), "192 MB");
        assert_eq!(format_saved(1611.1), "1.6 GB");
    }

    #[test]
    fn groups_thousands_and_parses_launch_args() {
        assert_eq!(group_thousands(7), "7");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(337426), "337,426");
        let l = Launch::from_args(["--verbose", "--start", r"D:\Videos"].map(String::from).into_iter());
        assert!(l.start);
        assert_eq!(l.folder, Some(PathBuf::from(r"D:\Videos")));
    }

    #[test]
    fn plurals_durations_and_folder_names() {
        assert_eq!(plural(1, "video", "videos"), "1 video");
        assert_eq!(plural(1200, "video", "videos"), "1,200 videos");
        assert_eq!(history_text(1), "1 processed file is remembered");
        assert_eq!(history_text(337426), "337,426 processed files are remembered");
        assert_eq!(format_duration(std::time::Duration::from_secs(75)), "1:15");
        assert_eq!(format_duration(std::time::Duration::from_secs(3725)), "1:02:05");
        assert_eq!(folder_name(r"D:\Videos\Family 2024"), "Family 2024");
        assert_eq!(folder_name(r"D:\"), r"D:\");
    }

    #[test]
    fn unknown_saved_values_fall_back_to_defaults() {
        let d = Prefs::default();
        // e.g. the removed OpenCL encoder or an out-of-range job count
        assert_eq!(position(ENCODERS, &"libx264_opencl", &d.encoder.as_str()), 0);
        assert_eq!(position(JOBS, &9, &d.concurrency), 3);
        assert_eq!(position(SPEEDS, &"medium", &d.preset.as_str()), 1);
    }

    #[test]
    fn advanced_options_default_to_the_measured_choice() {
        // Settings saved before Advanced options existed take the defaults.
        let old: Prefs = serde_json::from_str(r#"{"encoder":"av1_nvenc","concurrency":2}"#).unwrap();
        assert!(old.gpu_decoding);
        assert_eq!(old.max_parts, ["png_ect"]);
        let o = crate::args::Options::normalize(old.to_raw());
        assert!(o.gpu_decoding);
        assert!(!old.drop_data && !o.drop_data);
        assert_eq!(o.max, crate::image::Max::chosen());
        // Chosen parts reach the engine; unknown names are ignored.
        let p = Prefs { gpu_decoding: false, drop_data: true, max_parts: vec!["flac_e".into(), "bogus".into()], ..Prefs::default() };
        let o = crate::args::Options::normalize(p.to_raw());
        assert!(!o.gpu_decoding && o.drop_data);
        assert_eq!(o.max, crate::image::Max { flac_e: true, ..crate::image::Max::default() });
        // Every part has a name that round-trips.
        for (name, _, _) in crate::image::Max::PARTS {
            assert_eq!(crate::image::Max::from_names(&[name]).names(), [*name]);
        }
        assert_eq!(crate::image::Max::PARTS.len(), 10);
    }
}
