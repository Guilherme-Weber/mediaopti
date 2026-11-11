// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
mod progress;
mod system;
mod taskbar;

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
    let Some(_instance) = system::SingleInstance::acquire(r"Local\mediaopti-single-instance") else {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Info)
            .set_title("mediaopti")
            .set_description("mediaopti is already running.")
            .show();
        return Ok(());
    };

    app::run(&data_dir, app::Launch::from_args(std::env::args().skip(1)))
}
