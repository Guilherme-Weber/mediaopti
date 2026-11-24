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

mod prefs;
use prefs::*;

mod controller;
use controller::*;

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
    /// Debug builds: `--preview-confirm` opens the in-app question at start (screenshots of
    /// the dialog without clicking: `scripts/ui-shots.ps1`).
    pub preview_confirm: bool,
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
            } else if cfg!(debug_assertions) && arg == "--preview-confirm" {
                launch.preview_confirm = true;
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
    let (prefs, prefs_problem) = Prefs::load(&prefs_path);
    if let Some(problem) = prefs_problem {
        engine.log(problem);
    }
    prefs.apply_to_window(&window);
    engine.apply_options(prefs.to_raw());

    // Models
    let log_model = Rc::new(VecModel::<LogLine>::default());
    let worker_model = Rc::new(VecModel::<WorkerRow>::default());
    let metric_model = Rc::new(VecModel::<Metric>::default());
    window.set_log_lines(log_model.clone().into());
    window.set_workers(worker_model.clone().into());
    let (files_left, files_right) = (Rc::new(VecModel::<FileRow>::default()), Rc::new(VecModel::<FileRow>::default()));
    window.set_files_left(files_left.clone().into());
    window.set_files_right(files_right.clone().into());
    window.set_metrics(metric_model.clone().into());
    let count_model = Rc::new(VecModel::<QueueCount>::default());
    window.set_queue_counts(count_model.clone().into());
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
            for line in lines {
                engine.log(line);
            }
            let (found, missing) = engine.tool_lists();
            let (found, missing) = (found.join(", "), missing.join(", "));
            let w2 = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = w2.upgrade() {
                    w.set_tools_found(found.as_str().into());
                    w.set_tools_missing(missing.as_str().into());
                }
            });
            // Then the versions, and new pingo/beeim versions (at most once a week).
            tool_info(&engine, &weak, false);
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        window.on_check_tools(move || {
            let (engine, weak) = (engine.clone(), weak.clone());
            std::thread::spawn(move || tool_info(&engine, &weak, true));
        });
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        window.on_install_tools(move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_busy() || w.get_processing() || w.get_tools_checking() {
                return;
            }
            w.set_tools_checking(true);
            let (engine, weak) = (engine.clone(), weak.clone());
            std::thread::spawn(move || {
                match crate::toolver::install_newer(engine.data_dir()) {
                    Ok(done) if !done.is_empty() => {
                        engine.reload_tools();
                        engine.log(format!("Installed the new {} from css-ig.net", done.join(" and ")));
                    }
                    Ok(_) => {}
                    Err(why) => engine.log(format!("Could not install the new tool versions ({why})")),
                }
                tool_info(&engine, &weak, false);
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
            let Ok(index) = usize::try_from(index) else { return };
            let Some(mut part) = parts.row_data(index) else { return };
            part.on = !part.on;
            parts.set_row_data(index, part);
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
            let prefs = Prefs { gpu_decoding: d.gpu_decoding, max_parts: d.max_parts, drop_data: d.drop_data, strip: d.strip, redo_stripped: d.redo_stripped, ..Prefs::from_window(&w) };
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
            count_model,
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
        window.on_stop(move || engine.in_background(Engine::stop));
    }
    {
        let engine = engine.clone();
        window.on_cancel_scan(move || engine.cancel_scan());
    }
    {
        let engine = engine.clone();
        window.on_toggle_pause(move || engine.in_background(Engine::toggle_pause));
    }
    {
        let engine = engine.clone();
        let weak = window.as_weak();
        // Asked in the app's own dialog (themed), answered through `confirm-accepted`.
        window.on_forget_history(move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_confirm_title("Forget the processed history?".into());
            w.set_confirm_text(
                "Every file will be processed again at the next run, as if it were new. Nothing changes on disk now. \
                 Videos and lossy pictures already made smaller would be compressed again on top of the previous pass."
                    .into(),
            );
            w.set_confirm_yes("Forget history".into());
            w.set_confirm_open(true);
        });
        window.on_confirm_accepted(move || engine.in_background(Engine::clear_history));
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
        window.on_clear_queue(move || engine.in_background(Engine::clear_queue));
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
            let prefs = Prefs { nav_compact: current.nav_compact, gpu_decoding: current.gpu_decoding, max_parts: current.max_parts, drop_data: current.drop_data, strip: current.strip, redo_stripped: current.redo_stripped, ..Prefs::default() };
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
    if launch.preview_confirm {
        window.invoke_forget_history();
    }

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

    // Closing during a batch: the window stays (titled "Stopping…") until the encoders have
    // quit and any replacement in progress has finished, then closes. It used to vanish at
    // once while the app went on for up to half a minute, and starting it again meanwhile
    // said it was already running.
    {
        let engine = engine.clone();
        let closing = std::cell::Cell::new(false);
        window.window().on_close_requested(move || {
            if closing.get() {
                return slint::CloseRequestResponse::KeepWindowShown;
            }
            if !engine.busy() {
                return slint::CloseRequestResponse::HideWindow;
            }
            closing.set(true);
            engine.log("Closing once the running jobs have stopped (encoders are asked to quit; this can take a few seconds)");
            engine.in_background(|e| {
                e.stop();
                e.shutdown();
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            });
            slint::CloseRequestResponse::KeepWindowShown
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

/// Shows each tool's version on the About page, and newer pingo/beeim versions (looked for
/// when `force` or a week has passed). Blocking: call from a background thread.
fn tool_info(engine: &Arc<Engine>, weak: &slint::Weak<AppWindow>, force: bool) {
    let w0 = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = w0.upgrade() {
            w.set_tools_checking(true);
        }
    });
    let tools = engine.tools();
    let rows: Vec<(String, String, String)> = crate::toolver::list(&tools, engine.data_dir())
        .into_iter()
        .map(|t| (t.name.to_string(), t.version, t.source.to_string()))
        .collect();
    let newer = crate::toolver::check_downloaded(&tools, engine.data_dir(), force);
    let text = match &newer {
        Ok(n) if n.is_empty() => "pingo and beeim are up to date.".to_string(),
        Ok(n) => {
            let what: Vec<String> = n
                .iter()
                .map(|t| match &t.have {
                    Some(h) => format!("{} {} (now {h})", t.name, t.available),
                    None => format!("{} {}", t.name, t.available),
                })
                .collect();
            let line = format!("New version available: {}", what.join(", "));
            engine.log(format!("{line} (About > Tools > Install)"));
            line
        }
        Err(why) => format!("Could not look for new pingo and beeim versions ({why})."),
    };
    let available = newer.as_ref().is_ok_and(|n| !n.is_empty());
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let mut model: Vec<ToolRow> = rows
            .into_iter()
            .map(|(name, version, source)| ToolRow { name: name.into(), version: version.into(), source: source.into() })
            .collect();
        // Two columns, each read top to bottom (the first half on the left).
        let right = model.split_off(model.len().div_ceil(2));
        w.set_tool_rows(ModelRc::new(VecModel::from(model)));
        w.set_tool_rows_right(ModelRc::new(VecModel::from(right)));
        w.set_tools_new_text(text.into());
        w.set_tools_new(available);
        w.set_tools_checking(false);
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The log keeps its last MAX_LOG_LINES lines, in order, however many arrive at once.
    #[test]
    fn the_log_keeps_its_last_lines() {
        let model = VecModel::<SharedString>::default();
        append_log(&model, (0..MAX_LOG_LINES - 2).map(|i| i.to_string()).collect());
        assert_eq!(model.row_count(), MAX_LOG_LINES - 2);
        append_log(&model, vec!["a", "b", "c"]);
        assert_eq!(model.row_count(), MAX_LOG_LINES);
        assert_eq!(model.row_data(0).unwrap(), "1");
        assert_eq!(model.row_data(MAX_LOG_LINES - 1).unwrap(), "c");
        append_log(&model, (0..MAX_LOG_LINES + 7).map(|i| format!("n{i}")).collect());
        assert_eq!(model.row_count(), MAX_LOG_LINES);
        assert_eq!(model.row_data(0).unwrap(), "n7");
    }

    /// The window's log line splits its time off, so a wrapped message stays indented.
    #[test]
    fn log_lines_split_their_time() {
        let line = LogLine::from("[19:49:19] [Documents] Kept a.pdf (already optimal)".to_string());
        assert_eq!((line.time.as_str(), line.text.as_str()), ("19:49:19", "[Documents] Kept a.pdf (already optimal)"));
        // Anything else stays whole, as the message.
        for odd in ["[Documents] Kept a.pdf", "no time here", "[19:49] x", ""] {
            let line = LogLine::from(odd.to_string());
            let want = if odd == "[19:49] x" { ("19:49", "x") } else { ("", odd) };
            assert_eq!((line.time.as_str(), line.text.as_str()), want, "{odd:?}");
        }
    }

    /// The log's result lines get their tone from the engine's wording.
    #[test]
    fn log_lines_get_a_tone() {
        let cases = [
            ("[Images] Saved 38 B (18%) from a.png", "saved"),
            ("[Images] Saved 22 KB (1%) from a.png", "saved"),
            ("[Worker 1] Saved 1.4 MB (5%) from v.mp4", "saved"),
            ("[Images] Converted x.bmp to x.png, saved 1 MB (6%)", "saved"),
            ("[Documents] Kept a.pdf (already optimal)", "kept"),
            ("[Worker 3] Video was not smaller for v.webm; kept the original file", "kept"),
            ("[Images] Failed on a.png: could not read the file", "failed"),
            ("[Worker 2] Processing: v.mp4", ""),
            ("Found 39 files in D:/x", ""),
            ("Saved by the user", "saved"),
        ];
        for (message, tone) in cases {
            assert_eq!(log_tone(message), tone, "{message}");
        }
        assert_eq!(LogLine::from("[10:00:00] [Images] Kept a.png (already optimal)".to_string()).tone, "kept");
    }

    /// A running file shows its name first and only its last two folders.
    #[test]
    fn running_files_show_their_name_first() {
        let cases = [
            (r"Pictures\Jogos\S.T.A.L.K.E.R. 2\_AAA_\concept\Artbook (12).pdf", r"…\_AAA_\concept", "Artbook (12).pdf"),
            (r"Jogos\Fallout 4\Art.pdf", r"Jogos\Fallout 4", "Art.pdf"),
            (r"Skyrim\Guide.pdf", "Skyrim", "Guide.pdf"),
            ("a/b/c/d.png", "…/b/c", "d.png"),
            ("loose.png", "", "loose.png"),
        ];
        for (path, folder, name) in cases {
            assert_eq!(split_display_path(path), (folder.to_string(), name), "{path}");
        }
        let row = file_row(r"Jogos\Doom\x.cbz", "pictures 3 of 9", Some(0.33), None, 75);
        assert_eq!((row.name.as_str(), row.folder.as_str(), row.step.as_str(), row.time.as_str()), ("x.cbz", r"Jogos\Doom", "pictures 3 of 9", "1:15"));
        let row = file_row("v.mp4", "encoding", Some(0.45), Some("120 fps (4.0x)".into()), 12);
        assert_eq!((row.step.as_str(), row.time.as_str()), ("encoding 45% · 120 fps (4.0x)", "0:12"));
    }

    #[test]
    fn files_per_minute_after_the_first_minute() {
        use std::time::Duration;
        assert_eq!(files_per_minute(50, Duration::from_secs(30)), None);
        assert_eq!(files_per_minute(0, Duration::from_secs(600)), None);
        assert_eq!(files_per_minute(12_343, Duration::from_secs(8_131)).as_deref(), Some("91"));
        assert_eq!(files_per_minute(5, Duration::from_secs(120)).as_deref(), Some("2.5"));
        assert_eq!(files_per_minute(120_000, Duration::from_secs(60)).as_deref(), Some("120,000"));
    }

    #[test]
    fn cpu_jobs_metric_explains_limits() {
        let mut s = StateDto {
            status: Status::Running,
            dir: None,
            total_files: 0,
            pending_files: 0,
            skipped_known: 0,
            skipped_generated: 0,
            skipped_small_gifs: 0,
            history_count: 0,
            run_total: 100,
            completed_this_run: 0,
            compressed_this_run: 0,
            kept_this_run: 0,
            skipped_this_run: 0,
            failed_this_run: 0,
            space_saved_mb: 0.0,
            queue_counts: [(0, 0); 4],
            queue_revision: 0,
            cpu_jobs: (0..7)
                .map(|i| crate::engine::CpuJob { name: format!("doc{i}.pdf"), secs: 5, parts: None, phase: Some(("pictures", None)) })
                .collect(),
            type_progress: [(0, 0, 0), (0, 0, 0), (1, 20, 7), (0, 50, 0)],
            cpu_waiting_memory: false,
            scan_found: 0,
            telemetry: crate::engine::TelemetryDto {
                total_fps: 0,
                total_speed: 0.0,
                active_progress_sum: 0.0,
                nvenc_util: 0.0,
                vram_gb: 0.0,
                workers: Vec::new(),
            },
        };
        assert_eq!(cpu_jobs_metric(&s, 15), ("7".into(), "of 7 (documents)".into()));
        s.cpu_waiting_memory = true;
        assert_eq!(cpu_jobs_metric(&s, 15), ("7".into(), "of 15 · memory limit".into()));
        s.cpu_waiting_memory = false;
        s.type_progress = [(0, 0, 0), (0, 0, 0), (20, 20, 0), (10, 50, 7)];
        s.cpu_jobs[0].phase = Some(("waiting for memory", None));
        s.cpu_jobs[1].phase = Some(("waiting for memory", None));
        assert_eq!(cpu_jobs_metric(&s, 15), ("7".into(), "of 15 · 2 waiting".into()));
        s.cpu_jobs[0].phase = Some(("lossless", None));
        s.cpu_jobs[1].phase = Some(("lossless", None));
        assert_eq!(cpu_jobs_metric(&s, 15), ("7".into(), "of 15 slots".into()));
    }

    #[test]
    fn formats_megabytes_like_the_old_ui() {
        assert_eq!(format_mb(0.0), "0");
        assert_eq!(format_mb(3.04), "3");
        assert_eq!(format_mb(3.46), "3.5");
        assert_eq!(format_mb(192.4), "192");
        assert_eq!(format_saved(0.0), "0 MB");
        assert_eq!(format_saved(512.0 / (1024.0 * 1024.0)), "512 B");
        assert_eq!(format_saved(0.5), "512 KB");
        assert_eq!(format_saved(192.4), "192 MB");
        assert_eq!(format_saved(1611.1), "1.6 GB");
        assert_eq!(format_bytes(174), "174 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
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
}
