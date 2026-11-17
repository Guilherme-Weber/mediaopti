//! The bridge from the engine to the window (`SlintUi`, `Controller`) and the texts it shows.

use super::*;

// ------------------------------------------------------------------- engine -> UI bridge

#[derive(Default)]
pub(super) struct Inbox {
    pub(super) state: Mutex<Option<StateDto>>,
    pub(super) logs: Mutex<Vec<String>>,
    pub(super) scheduled: AtomicBool,
}

pub(super) struct SlintUi {
    pub(super) inbox: Arc<Inbox>,
    pub(super) file_log: crate::logfile::FileLog,
}

impl SlintUi {
    /// Queues one UI-thread update; further messages before it runs share it.
    pub(super) fn schedule(&self) {
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
    pub(super) static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

pub(super) fn with_controller(f: impl FnOnce(&mut Controller)) {
    CONTROLLER.with(|c| {
        if let Some(controller) = c.borrow_mut().as_mut() {
            f(controller);
        }
    });
}

/// Lives on the UI thread; owns the window models.
pub(super) struct Controller {
    pub(super) engine: Arc<Engine>,
    pub(super) queue_model: Rc<VecModel<QueueItem>>,
    /// The queue revision (and running state) the list was last loaded for.
    pub(super) queue_shown: Option<(u64, bool)>,
    pub(super) window: slint::Weak<AppWindow>,
    pub(super) inbox: Arc<Inbox>,
    pub(super) state: StateDto,
    pub(super) log_model: Rc<VecModel<SharedString>>,
    pub(super) worker_model: Rc<VecModel<WorkerRow>>,
    pub(super) files_left: Rc<VecModel<FileRow>>,
    pub(super) files_right: Rc<VecModel<FileRow>>,
    pub(super) metric_model: Rc<VecModel<Metric>>,
    pub(super) count_model: Rc<VecModel<QueueCount>>,
    pub(super) log_revision: i32,
    pub(super) taskbar: Option<Taskbar>,
    pub(super) taskbar_tried: bool,
    pub(super) clock: RunClock,
}

/// Elapsed processing time of the current batch, not counting time spent paused.
#[derive(Default)]
pub(super) struct RunClock {
    pub(super) banked: std::time::Duration,
    pub(super) running_since: Option<std::time::Instant>,
    pub(super) active: bool,
}

impl RunClock {
    pub(super) fn update(&mut self, status: Status) {
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

    pub(super) fn elapsed(&self) -> std::time::Duration {
        self.banked + self.running_since.map(|t| t.elapsed()).unwrap_or_default()
    }
}

/// 75 s -> "1:15", 3725 s -> "1:02:05"
pub(super) fn format_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Rough time left from the share done so far: "about 3 h 20 min", or nothing while too
/// early to tell. (Files run largest first, so early estimates run high.)
pub(super) fn format_eta(elapsed: std::time::Duration, fraction: f64) -> Option<String> {
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
pub(super) fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", group_thousands(n), if n == 1 { one } else { many })
}

impl Controller {
    pub(super) fn drain(&mut self) {
        self.inbox.scheduled.store(false, Ordering::Release);
        let state = self.inbox.state.lock().unwrap_or_else(PoisonError::into_inner).take();
        let logs = std::mem::take(&mut *self.inbox.logs.lock().unwrap_or_else(PoisonError::into_inner));

        if !logs.is_empty() {
            append_log(&self.log_model, logs);

            self.log_revision += 1;
        }
        if let Some(state) = state {
            self.state = state;
        }
        self.render();
    }

    pub(super) fn clear_log(&mut self) {
        self.log_model.set_vec(Vec::new());
        self.render();
    }

    /// The queue summary by type, and the file list when the queue changed (not while
    /// running: the list is hidden then, and reloading it would cost time on every change).
    pub(super) fn render_queue(&mut self, w: &AppWindow) {
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
        sync_model(&self.count_model, counts);
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

    pub(super) fn render(&mut self) {
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
            Status::Stopping => "Stopping… · mediaopti".to_string(),
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
pub(super) fn file_row(name: &str, step: &str, share: Option<f32>, extra: Option<String>, secs: u64) -> FileRow {
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

/// Adds log lines, keeping the last `MAX_LOG_LINES`: past the limit, in one change (removing
/// the oldest one by one shifted the whole list for each).
pub(super) fn append_log(model: &VecModel<SharedString>, logs: Vec<String>) {
    if model.row_count() + logs.len() <= MAX_LOG_LINES {
        for line in logs {
            model.push(line.into());
        }
        return;
    }
    let keep = MAX_LOG_LINES.saturating_sub(logs.len());
    let old = model.row_count();
    let mut lines: Vec<SharedString> = (old.saturating_sub(keep)..old).filter_map(|i| model.row_data(i)).collect();
    lines.extend(logs.into_iter().map(Into::into));
    let from = lines.len().saturating_sub(MAX_LOG_LINES);
    model.set_vec(lines.split_off(from));
}

pub(super) fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
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

// ------------------------------------------------------------------- text helpers

pub(super) fn status_key(status: Status) -> &'static str {
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

pub(super) fn status_label(status: Status) -> &'static str {
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

pub(super) fn format_mb(value: f64) -> String {
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
pub(super) fn format_bytes(b: u64) -> String {
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
pub(super) const QUEUE_LIST_MAX: usize = 5000;

/// Space saved: MB below 1 GB, otherwise GB with one decimal.
pub(super) fn format_saved(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{} MB", format_mb(mb))
    }
}

pub(super) fn file_count_text(s: &StateDto) -> String {
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

pub(super) fn history_text(count: usize) -> String {
    format!("{} remembered", plural(count, "processed file is", "processed files are"))
}

/// 337426 -> "337,426"
pub(super) fn group_thousands(n: usize) -> String {
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
pub(super) fn folder_name(dir: &str) -> String {
    Path::new(dir)
        .file_name()
        .map_or_else(|| dir.to_string(), |n| n.to_string_lossy().into_owned())
}

pub(super) fn idle_file_text(s: &StateDto) -> String {
    match s.status {
        Status::Done if s.run_total > 0 => format!(
            "Batch complete · {} made smaller · {} kept · {} skipped · {} failed",
            s.compressed_this_run, s.kept_this_run, s.skipped_this_run, s.failed_this_run
        ),
        Status::Done => "Batch complete".into(),
        Status::Ready if s.pending_files > 0 => "Ready to process".into(),
        Status::Error => "Check the activity log".into(),
        _ => "No active file".into(),
    }
}
