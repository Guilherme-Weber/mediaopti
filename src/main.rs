// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// NVENC through ffmpeg.exe, the Recycle Bin, job objects, %APPDATA%: Windows only.
#[cfg(not(windows))]
compile_error!("mediaopti is a Windows app");

mod app;
mod archive;
mod args;
mod audio;
mod engine;
#[cfg(test)]
mod engine_tests;
mod history;
mod image;
mod logfile;
mod pdf;
mod progress;
mod system;
mod taskbar;
mod toolver;
mod update;

fn main() -> Result<(), slint::PlatformError> {
    let appdata = std::env::var_os("APPDATA").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let data_dir = appdata.join("mediaopti");
    let _ = std::fs::create_dir_all(&data_dir);
    logfile::install_panic_hook(data_dir.join("logs"));
    // mediaopti replaces batchviproc: carry its processed history and settings over once.
    history::import_batchviproc(&appdata.join("batchviproc"), &data_dir);
    // ...and imageopti's content history, so images it already optimized are recognised.
    history::import_imageopti(&appdata.join("imageopti"), &data_dir);

    // Two instances would append to the same history and fight over the same files.
    // Started by an update: the old version may still be closing.
    let after_update = std::env::args().any(|a| a == update::UPDATED_FLAG);
    let started = std::time::Instant::now();
    let instance = loop {
        let instance = system::SingleInstance::acquire(r"Local\mediaopti-single-instance");
        if instance.is_some() || !after_update || started.elapsed() > std::time::Duration::from_secs(30) {
            break instance;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let Some(_instance) = instance else {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Info)
            .set_title("mediaopti")
            .set_description("mediaopti is already running.")
            .show();
        return Ok(());
    };

    let result = app::run(&data_dir, app::Launch::from_args(std::env::args().skip(1)));
    update::restart_if_requested();
    result
}
