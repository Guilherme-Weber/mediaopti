//! End-to-end tests of the engine with the bundled ffmpeg and a real GPU.
//! They are skipped when bin/ffmpeg.exe is missing (run scripts/fetch-ffmpeg.ps1).

use crate::args::{Conversions, ImageMode, Options, Originals};
use crate::engine::{Engine, StateDto, Status, Ui};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Default)]
struct Recorder {
    logs: Mutex<Vec<String>>,
}

struct RecorderUi(Arc<Recorder>);

impl Ui for RecorderUi {
    fn log(&self, message: String) {
        self.0.logs.lock().unwrap().push(message);
    }
    fn state(&self, _state: &StateDto) {}
}

fn ffmpeg() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
    p.exists().then_some(p)
}

fn make(ffmpeg: &Path, args: &[&str], out: &Path) {
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(out)
        .status()
        .unwrap();
    assert!(status.success(), "could not create {}", out.display());
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mediaopti-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn lossless_video(ffmpeg: &Path, seconds: u32, size: &str, out: &Path) {
    let src = format!("testsrc2=size={size}:rate=30:duration={seconds}");
    let sine = format!("sine=duration={seconds}");
    make(
        ffmpeg,
        &["-f", "lavfi", "-i", &src, "-f", "lavfi", "-i", &sine, "-c:v", "libx264", "-preset",
          "ultrafast", "-qp", "0", "-c:a", "aac", "-shortest"],
        out,
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn new_engine(
    rt: &tokio::runtime::Runtime,
    root: &Path,
    ffmpeg: PathBuf,
) -> (Arc<Engine>, Arc<Recorder>) {
    let rec = Arc::new(Recorder::default());
    let ui = Box::new(RecorderUi(rec.clone()));
    let engine = Engine::new(ui, &root.join("appdata"), ffmpeg, rt.handle().clone());
    (engine, rec)
}

fn opts(encoder: &str, concurrency: u32) -> Options {
    Options {
        crf: 23,
        preset: "slow".into(),
        encoder: encoder.into(),
        concurrency,
        // Tests must not fill the user's Recycle Bin; GIF limit and VMAF retry off so the
        // older tests keep checking what they were written for.
        originals: Originals::Delete,
        gif_min_bytes: 0,
        vmaf_floor: None,
        images: ImageMode::Off,
        convert: Conversions::NONE,
        lossy_docs: false,
        audio: ImageMode::Off,
        ..Options::default()
    }
}

mod batch;
mod control;
mod video;
mod gif;
mod cpu;
mod safety;
mod real;
