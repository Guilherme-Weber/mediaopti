//! End-to-end tests of the engine with the bundled ffmpeg and a real GPU.
//! They are skipped when bin/ffmpeg.exe is missing (run scripts/fetch-ffmpeg.ps1).

use crate::args::RawOptions;
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

fn opts(encoder: &str, concurrency: f64) -> RawOptions {
    RawOptions {
        crf: Some(23.0),
        preset: Some("slow".into()),
        encoder: Some(encoder.into()),
        concurrency: Some(concurrency),
        // Tests must not fill the user's Recycle Bin; GIF limit and VMAF retry off so the
        // older tests keep checking what they were written for.
        originals: Some(0),
        backup_dir: None,
        gif_min_mb: Some(0),
        vmaf_floor: Some(0),
        images: Some(0),
        convert: Some(0),
        lossy_docs: Some(0),
        effort: None,
        temp_dir: None,
        fix_ext: None,
    }
}

#[test]
fn full_batch_handles_every_file_kind() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("full");
    let videos = root.join("videos");
    std::fs::create_dir_all(videos.join("sub")).unwrap();
    std::fs::create_dir_all(videos.join(".hidden")).unwrap();

    let big = videos.join("big.mp4");
    lossless_video(&ff, 4, "1280x720", &big);
    let mov = videos.join("sub").join("clip.mov");
    lossless_video(&ff, 2, "640x360", &mov);
    make(&ff, &["-f", "lavfi", "-i", "sine=duration=2", "-c:a", "aac"], &videos.join("voice.mp4"));
    std::fs::write(videos.join("broken.mp4"), vec![7u8; 50_000]).unwrap();
    make(&ff, &["-f", "lavfi", "-i", "color=red:size=64x64:duration=0.04", "-frames:v", "1"], &videos.join("static.gif"));
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10:duration=2"], &videos.join("anim.gif"));
    lossless_video(&ff, 1, "320x240", &videos.join(".hidden").join("secret.mp4"));
    std::fs::write(videos.join("old.mediaopti.w1.mp4"), b"leftover").unwrap();
    std::fs::copy(&big, videos.join("export.tmp.mp4")).unwrap();

    // Old timestamp + read-only, like files copied from a phone backup.
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800); // 2020-01-01
    std::fs::File::options().write(true).open(&big).unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old).set_accessed(old)).unwrap();
    let mut perms = std::fs::metadata(&big).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&big, perms).unwrap();
    let big_before = std::fs::metadata(&big).unwrap().len();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        // Like the app at startup: saved settings are applied before the folder is scanned.
        engine.apply_options(opts("av1_nvenc", 2.0));
        engine.load_dir(videos.clone()).await;
        let s = engine.snapshot();
        assert_eq!(s.total_files, 7, "logs: {:#?}", rec.logs.lock().unwrap());
        assert_eq!(s.skipped_generated, 1);
        // static.gif is a still image: it goes to the image workers (off here), never to video.
        assert_eq!(s.pending_files, 5);
        assert_eq!(s.skipped_small_gifs, 1);
        assert!(!videos.join("old.mediaopti.w1.mp4").exists(), "orphan temp not removed");

        engine.clone().run_batch(opts("av1_nvenc", 2.0)).await;
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Done, "{logs}");
    assert_eq!(s.completed_this_run, 5, "{logs}");
    assert_eq!(s.compressed_this_run, 3, "{logs}"); // big.mp4, clip.mov, anim.gif
    assert_eq!(s.failed_this_run, 1, "{logs}"); // broken.mp4
    assert_eq!(s.skipped_larger_this_run, 1, "{logs}"); // voice.mp4

    let big_after = std::fs::metadata(&big).unwrap();
    assert!(big_after.len() < big_before, "big.mp4 was not compressed");
    assert_eq!(big_after.modified().unwrap(), old, "modified date not preserved");
    assert!(!videos.join("sub").join("clip.mov").exists());
    assert!(videos.join("sub").join("clip.mp4").exists());
    assert!(videos.join("anim.mp4").exists() && !videos.join("anim.gif").exists());
    assert!(videos.join("static.gif").exists() && videos.join("voice.mp4").exists());
    assert!(videos.join(".hidden").join("secret.mp4").exists());
    let leftovers: Vec<_> = walkdir::WalkDir::new(&videos).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    // History persisted: a fresh engine on the same folder finds nothing new.
    let history = std::fs::read_to_string(root.join("appdata").join("processedFiles.txt")).unwrap();
    assert!(history.lines().count() >= 6);
    let (engine2, _) = new_engine(&rt, &root, ff.clone());
    engine2.apply_options(opts("av1_nvenc", 2.0)); // same settings as the run
    rt.block_on(engine2.load_dir(videos.clone()));
    assert_eq!(engine2.snapshot().pending_files, 0);

    // Content history: processed videos moved, renamed or copied elsewhere are still
    // recognised; a genuinely new video is not.
    let content = std::fs::read_to_string(root.join("appdata").join("processed-content.tsv")).unwrap();
    assert!(content.lines().count() >= 5, "{content}");
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::rename(&big, elsewhere.join("renamed.mp4")).unwrap();
    std::fs::copy(videos.join("sub").join("clip.mp4"), elsewhere.join("copy of clip.mp4")).unwrap();
    lossless_video(&ff, 1, "320x240", &elsewhere.join("new.mp4"));
    let (engine3, rec3) = new_engine(&rt, &root, ff);
    rt.block_on(engine3.load_dir(elsewhere));
    let s3 = engine3.snapshot();
    let logs3 = rec3.logs.lock().unwrap().join("\n");
    assert_eq!(s3.total_files, 3, "{logs3}");
    assert_eq!(s3.pending_files, 1, "only new.mp4 should be queued: {logs3}");
    assert!(logs3.contains("2 moved, renamed or copied files recognised"), "{logs3}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stop_kills_ffmpeg_and_keeps_originals() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("stop");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    let long = videos.join("long.mp4");
    lossless_video(&ff, 40, "1920x1080", &long);
    let size_before = std::fs::metadata(&long).unwrap().len();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 1.0)));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(engine.snapshot().status, Status::Running);
        engine.stop();
        tokio::time::timeout(Duration::from_secs(15), runner).await
            .expect("batch did not stop within 15s").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Ready, "{logs}");
    assert_eq!(std::fs::metadata(&long).unwrap().len(), size_before, "original changed");
    assert!(!videos.join("long.mediaopti.w1.mp4").exists(), "temp file left behind");
    assert!(logs.contains("Stopped while processing long.mp4"), "{logs}");

    let _ = std::fs::remove_dir_all(&root);
}

/// Audio codec of the first audio stream, read from ffmpeg's stream listing.
/// Waits until the single worker reports progress above 0%.
async fn wait_for_progress(engine: &Arc<Engine>) -> i64 {
    for _ in 0..200 {
        let pct = engine.snapshot().telemetry.workers.first().map_or(0, |w| w.percent);
        if pct > 0 {
            return pct;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("encoding never made progress");
}

/// Pause suspends ffmpeg (no progress, no output growth, no new file started); Resume
/// finishes the batch normally. Set BATCHVIPROC_PAUSE_SECS=70 to also prove that a long
/// pause does not trip the 30/60 s stall watchdog, and BATCHVIPROC_PAUSE_ENCODER=av1_nvenc
/// to pause a GPU encode.
#[test]
fn pause_suspends_ffmpeg_and_resume_finishes() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let pause_secs: u64 = std::env::var("BATCHVIPROC_PAUSE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let encoder = std::env::var("BATCHVIPROC_PAUSE_ENCODER").unwrap_or_else(|_| "libx264".into());
    let root = test_dir("pause");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    lossless_video(&ff, 12, "1280x720", &videos.join("a.mp4"));
    lossless_video(&ff, 4, "640x360", &videos.join("b.mp4"));
    let temp = videos.join("a.mediaopti.w1.mp4");

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(opts(&encoder, 1.0)));
        wait_for_progress(&engine).await;

        engine.set_paused(true);
        tokio::time::sleep(Duration::from_millis(700)).await; // let in-flight output settle
        let before = engine.snapshot();
        let pct_before = before.telemetry.workers[0].percent;
        let size_before = std::fs::metadata(&temp).map_or(0, |m| m.len());
        assert_eq!(before.status, Status::Paused);

        tokio::time::sleep(Duration::from_secs(pause_secs)).await;
        let after = engine.snapshot();
        assert_eq!(after.status, Status::Paused);
        assert_eq!(after.telemetry.workers[0].percent, pct_before, "progress moved while paused");
        assert_eq!(after.telemetry.total_fps, 0);
        assert_eq!(std::fs::metadata(&temp).map_or(0, |m| m.len()), size_before, "output grew while paused");
        assert_eq!(after.completed_this_run, 0);
        let files: Vec<_> = after.telemetry.workers.iter().map(|w| w.file.as_str()).collect();
        assert_eq!(files, ["a.mp4"], "a new file started while paused");

        engine.set_paused(false);
        assert_eq!(engine.snapshot().status, Status::Running);
        tokio::time::timeout(Duration::from_secs(120), runner).await
            .expect("batch did not finish after resume").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Done, "{logs}");
    assert_eq!(s.compressed_this_run + s.skipped_larger_this_run, 2, "{logs}");
    assert_eq!(s.failed_this_run, 0, "{logs}");
    assert!(logs.contains("Paused (1 ffmpeg process suspended)"), "{logs}");
    assert!(logs.contains("Resumed"), "{logs}");
    assert!(!logs.contains("Stall watchdog"), "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Stop works while paused: the suspended ffmpeg is killed and originals are untouched.
#[test]
fn stop_while_paused_keeps_originals() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("pause-stop");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    let long = videos.join("long.mp4");
    lossless_video(&ff, 30, "1920x1080", &long);
    let size_before = std::fs::metadata(&long).unwrap().len();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 1.0)));
        wait_for_progress(&engine).await;
        engine.set_paused(true);
        tokio::time::sleep(Duration::from_secs(1)).await;
        engine.stop();
        tokio::time::timeout(Duration::from_secs(15), runner).await
            .expect("batch did not stop within 15s").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    assert_eq!(engine.snapshot().status, Status::Ready, "{logs}");
    assert!(!engine.is_paused());
    assert_eq!(std::fs::metadata(&long).unwrap().len(), size_before, "original changed");
    assert!(!videos.join("long.mediaopti.w1.mp4").exists(), "temp file left behind");
    let _ = std::fs::remove_dir_all(&root);
}

fn audio_codec(ffmpeg: &Path, file: &Path) -> Option<String> {
    let out = Command::new(ffmpeg).args(["-hide_banner", "-i"]).arg(file).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text.lines().find(|l| l.contains("Audio:"))?;
    let codec = line.split("Audio:").nth(1)?.split_whitespace().next()?;
    Some(codec.trim_end_matches(',').to_string())
}

/// Regression test for the real-world files from the project history ("old fixes"):
/// each of these once broke a batch.
#[test]
fn historical_problem_files_are_handled() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("history");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();

    // 1. Old game bonus video: WMV container with WMA audio (can't be stream-copied to MP4).
    make(
        &ff,
        &["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=4", "-f", "lavfi", "-i",
          "sine=duration=4", "-c:v", "wmv2", "-q:v", "2", "-c:a", "wmav2", "-b:a", "128k", "-shortest"],
        &videos.join("Developer Diary.wmv"),
    );
    // 2. Small animated sticker GIF (above the 16x16 skip threshold).
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=40x40:rate=10:duration=2"], &videos.join("sticker.gif"));
    // 3. "Troll" video that changes resolution mid-stream (two MPEG-TS segments glued together).
    let seg_a = root.join("a.ts");
    let seg_b = root.join("b.ts");
    for (seg, size) in [(&seg_a, "640x360"), (&seg_b, "320x240")] {
        let src = format!("testsrc2=size={size}:rate=30:duration=2");
        make(&ff, &["-f", "lavfi", "-i", &src, "-f", "lavfi", "-i", "sine=duration=2", "-c:v",
                    "libx264", "-preset", "ultrafast", "-qp", "0", "-c:a", "aac", "-shortest"], seg);
    }
    let mut troll = std::fs::read(&seg_a).unwrap();
    troll.extend(std::fs::read(&seg_b).unwrap());
    std::fs::write(videos.join("troll.ts"), troll).unwrap();
    // 4. Accented name with an upper-case extension.
    lossless_video(&ff, 2, "640x360", &videos.join("Vídeo Ação.MP4"));
    // 5. Remux target already exists: dup.mov would become dup.mp4, which is another file.
    lossless_video(&ff, 2, "640x360", &videos.join("dup.mov"));
    std::fs::write(videos.join("dup.mp4"), b"not really a video").unwrap();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        engine.clone().run_batch(opts("av1_nvenc", 2.0)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");

    // WMV: converted to MP4 with AAC audio, original removed.
    let wmv_out = videos.join("Developer Diary.mp4");
    assert!(wmv_out.exists() && !videos.join("Developer Diary.wmv").exists(), "{logs}");
    assert_eq!(audio_codec(&ff, &wmv_out).as_deref(), Some("aac"), "{logs}");
    // Sticker GIF: below NVENC's minimum size, so it must take the CPU fallback and finish
    // cleanly (converted, or kept when the MP4 isn't smaller), never fail.
    assert!(logs.contains("NVENC hardware limit on sticker.gif"), "{logs}");
    assert!(!logs.contains("exited with code") , "{logs}");
    assert!(videos.join("sticker.mp4").exists() != videos.join("sticker.gif").exists(), "{logs}");
    // The fake dup.mp4 is detected as unreadable and remembered, not retried forever.
    assert!(logs.contains("Corrupt/unreadable file remembered in history (will not retry): dup.mp4"), "{logs}");
    // Resolution change: encoded instead of failing.
    assert!(videos.join("troll.mp4").exists() && !videos.join("troll.ts").exists(), "{logs}");
    // Accented name: processed in place and remembered.
    assert!(videos.join("Vídeo Ação.MP4").exists(), "{logs}");
    assert!(logs.contains("Saved") && logs.contains("Vídeo Ação.MP4"), "{logs}");
    // Existing target with other content: the new version gets a number, the unrelated .mp4
    // stays untouched.
    assert!(!videos.join("dup.mov").exists() && videos.join("dup (2).mp4").exists(), "{logs}");
    assert_eq!(std::fs::read(videos.join("dup.mp4")).unwrap(), b"not really a video");
    // No temp files left anywhere.
    let leftovers: Vec<_> = walkdir::WalkDir::new(&videos).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    // Everything is remembered: a second scan finds nothing new.
    let (engine2, _) = new_engine(&rt, &root, ff);
    engine2.apply_options(opts("av1_nvenc", 2.0)); // same settings as the run
    rt.block_on(engine2.load_dir(videos));
    assert_eq!(engine2.snapshot().pending_files, 0, "{logs}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hard_links_are_left_alone_and_small_gifs_stay_gifs() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("links");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    let a = videos.join("a.mp4");
    lossless_video(&ff, 2, "640x360", &a);
    std::fs::hard_link(&a, videos.join("a-link.mp4")).unwrap();
    assert_eq!(crate::system::hard_link_count(&a).unwrap(), 2);
    let size_before = std::fs::metadata(&a).unwrap().len();
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10:duration=2"], &videos.join("small.gif"));

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    // Default GIF limit (1 MB): the small animated GIF must not be queued.
    let raw = RawOptions { gif_min_mb: None, ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw);
        engine.load_dir(videos.clone()).await;
        let s = engine.snapshot();
        assert_eq!(s.skipped_small_gifs, 1);
        assert_eq!(s.pending_files, 2);
        engine.clone().run_batch(RawOptions { gif_min_mb: None, ..opts("av1_nvenc", 1.0) }).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert_eq!(engine.snapshot().compressed_this_run, 0, "{logs}");
    assert_eq!(logs.matches("it is a hard link").count(), 2, "{logs}");
    assert_eq!(std::fs::metadata(&a).unwrap().len(), size_before, "hard-linked file was changed");
    assert!(videos.join("small.gif").exists(), "small GIF must stay a GIF");
    let _ = std::fs::remove_dir_all(&root);
}

/// An already-compressed H.264 that AV1 at the preset cannot shrink.
fn compressed_h264(ffmpeg: &Path, noise: u32, crf: u32, out: &Path) {
    let src = format!("testsrc2=size=1280x720:rate=30:duration=8,noise=alls={noise}:allf=t");
    let crf = crf.to_string();
    make(
        ffmpeg,
        &["-f", "lavfi", "-i", &src, "-c:v", "libx264", "-preset", "slow", "-crf", &crf, "-pix_fmt", "yuv420p"],
        out,
    );
}

#[test]
fn quality_retry_accepts_only_above_the_vmaf_floor() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let root = test_dir("vmaf");
    let (good, bad) = (root.join("good"), root.join("bad"));
    std::fs::create_dir_all(&good).unwrap();
    std::fs::create_dir_all(&bad).unwrap();
    // Light noise: AV1 at CQ 32 is bigger, at CQ 40 ~68% with VMAF ~96 -> accepted.
    let g = good.join("light.mp4");
    compressed_h264(&ff, 4, 24, &g);
    // Heavy noise: only CQ 44 gets smaller, at VMAF ~92 -> rejected, original kept.
    let b = bad.join("noisy.mp4");
    compressed_h264(&ff, 12, 30, &b);
    let (g_before, b_before) = (std::fs::metadata(&g).unwrap().len(), std::fs::metadata(&b).unwrap().len());

    let rt = runtime();
    let run = |dir: &Path, crf: f64| {
        let (engine, rec) = new_engine(&rt, &root.join(dir.file_name().unwrap()), ff.clone());
        let raw = || RawOptions { crf: Some(crf), vmaf_floor: Some(93), ..opts("av1_nvenc", 1.0) };
        rt.block_on(async {
            engine.apply_options(raw());
            engine.load_dir(dir.to_path_buf()).await;
            engine.clone().run_batch(raw()).await;
        });
        let logs = rec.logs.lock().unwrap().join("\n");
        println!("{logs}");
        (engine.snapshot(), logs)
    };

    let (s, logs) = run(&good, 20.0);
    assert_eq!(s.compressed_this_run, 1, "{logs}");
    assert!(logs.contains("Quality retry for light.mp4"), "{logs}");
    let after = std::fs::metadata(&g).unwrap().len();
    assert!(after < g_before, "not smaller: {after} vs {g_before}");

    let (s, logs) = run(&bad, 23.0);
    assert_eq!(s.compressed_this_run, 0, "{logs}");
    assert!(logs.contains("is below 93"), "{logs}");
    assert_eq!(std::fs::metadata(&b).unwrap().len(), b_before, "original changed");
    let leftovers: Vec<_> = walkdir::WalkDir::new(&root).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn images_and_videos_in_one_batch() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "jpegtran", "djpeg"].iter().all(|t| tools.has(t)) {
        eprintln!("skipping: image tools not found");
        return;
    }
    let root = test_dir("images");
    let media = root.join("media");
    std::fs::create_dir_all(media.join("fotos")).unwrap();
    lossless_video(&ff, 2, "640x360", &media.join("clip.mp4"));
    let png = media.join("fotos").join("Ação.png");
    make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=640x480", "-frames:v", "1", "-compression_level", "1"], &png);
    let jpg = media.join("photo.jpg");
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=s=800x600", "-frames:v", "1", "-q:v", "1"], &jpg);
    let linked = media.join("linked.png");
    std::fs::copy(&png, &linked).unwrap();
    std::fs::hard_link(&linked, root.join("other-name.png")).unwrap();
    // Old date + read-only, like files from a phone backup.
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
    std::fs::File::options().write(true).open(&jpg).unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
    let mut perms = std::fs::metadata(&jpg).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&jpg, perms).unwrap();
    let (png_before, jpg_before, linked_before) = (
        std::fs::metadata(&png).unwrap().len(),
        std::fs::metadata(&jpg).unwrap().len(),
        std::fs::metadata(&linked).unwrap().len(),
    );

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    let raw = || RawOptions { images: Some(1), ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        let s = engine.snapshot();
        assert_eq!(s.pending_files, 4, "1 video + 3 images");
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.images_total_this_run, 3, "{logs}");
    assert_eq!(s.images_done_this_run, 3, "{logs}");
    assert_eq!(s.compressed_this_run, 3, "video + PNG + JPEG: {logs}");
    assert!(std::fs::metadata(&png).unwrap().len() < png_before, "{logs}");
    let jpg_meta = std::fs::metadata(&jpg).unwrap();
    assert!(jpg_meta.len() < jpg_before, "{logs}");
    assert_eq!(jpg_meta.modified().unwrap(), old, "date not kept");
    assert!(jpg_meta.permissions().readonly(), "read-only flag not kept");
    assert_eq!(std::fs::metadata(&linked).unwrap().len(), linked_before, "hard link must be left alone");
    assert!(logs.contains("Skipped linked.png: it is a hard link"), "{logs}");
    let leftovers: Vec<_> = walkdir::WalkDir::new(&media).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    // Same settings, fresh engine: only the hard-linked image is still pending.
    let (engine2, _) = new_engine(&rt, &root, ff.clone());
    engine2.apply_options(raw());
    rt.block_on(engine2.load_dir(media.clone()));
    assert_eq!(engine2.snapshot().pending_files, 1, "{logs}");

    // Lossless history doesn't cover a lossy run: the optimized images are queued again.
    let (engine3, _) = new_engine(&rt, &root, ff.clone());
    engine3.apply_options(RawOptions { images: Some(2), ..opts("av1_nvenc", 1.0) });
    rt.block_on(engine3.load_dir(media));
    assert_eq!(engine3.snapshot().pending_files, 3);

    // A moved image is recognised by content.
    let moved = root.join("moved");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::rename(&png, moved.join("renamed.png")).unwrap();
    let (engine4, rec4) = new_engine(&rt, &root, ff);
    engine4.apply_options(raw());
    rt.block_on(engine4.load_dir(moved));
    let logs4 = rec4.logs.lock().unwrap().join("\n");
    assert_eq!(engine4.snapshot().pending_files, 0, "{logs4}");
    assert!(logs4.contains("1 moved, renamed or copied files recognised"), "{logs4}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Benchmark on a copy of a real folder: `MEDIAOPTI_BENCH_DIR=<folder> cargo test --release
/// bench_folder -- --ignored --nocapture`. The folder itself is never modified.
#[test]
#[ignore = "benchmark on a real folder (MEDIAOPTI_BENCH_DIR)"]
fn bench_folder() {
    let (Some(ff), Ok(src)) = (ffmpeg(), std::env::var("MEDIAOPTI_BENCH_DIR")) else { return };
    let root = test_dir("bench");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let mut before = 0u64;
    for e in std::fs::read_dir(&src).unwrap().filter_map(Result::ok) {
        if e.path().is_file() {
            before += std::fs::copy(e.path(), media.join(e.file_name())).unwrap();
        }
    }
    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    // MEDIAOPTI_BENCH_EFFORT=2: maximum effort.
    let effort = std::env::var("MEDIAOPTI_BENCH_EFFORT").ok().and_then(|e| e.parse().ok());
    // MEDIAOPTI_BENCH_CONVERT=1: conversions on (BMP/TGA -> PNG, WAV -> FLAC, CBR -> CBZ).
    let convert = std::env::var("MEDIAOPTI_BENCH_CONVERT").ok().and_then(|e| e.parse().ok());
    // MEDIAOPTI_BENCH_DOCS=1: visually lossless PDF images.
    let docs = std::env::var("MEDIAOPTI_BENCH_DOCS").ok().and_then(|e| e.parse().ok());
    let raw = || RawOptions { images: Some(2), effort, convert: convert.or(Some(0)), lossy_docs: docs.or(Some(0)), ..opts("av1_nvenc", 2.0) };
    let t = std::time::Instant::now();
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    let secs = t.elapsed().as_secs_f64();
    let after: u64 = std::fs::read_dir(&media).unwrap().filter_map(Result::ok).map(|e| e.metadata().unwrap().len()).sum();
    let s = engine.snapshot();
    let logs = rec.logs.lock().unwrap();
    for l in logs.iter().filter(|l| l.contains("Failed") || l.starts_with("Done") || l.starts_with("Images:")) {
        println!("{l}");
    }
    println!(
        "{} images in {secs:.0}s ({:.2} s/image wall), {:.1} MB -> {:.1} MB ({:.2}% saved)",
        s.images_done_this_run,
        secs / s.images_done_this_run.max(1) as f64,
        before as f64 / 1048576.0,
        after as f64 / 1048576.0,
        100.0 * (before - after) as f64 / before as f64
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn originals_go_to_a_mirrored_backup_folder() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !tools.has("oxipng") {
        eprintln!("skipping: image tools not found");
        return;
    }
    let root = test_dir("backup");
    let media = root.join("media");
    std::fs::create_dir_all(media.join("sub")).unwrap();
    let clip = media.join("clip.mp4");
    lossless_video(&ff, 2, "640x360", &clip);
    let png = media.join("sub").join("a.png");
    make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=320x240", "-frames:v", "1", "-compression_level", "1"], &png);
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
    std::fs::File::options().write(true).open(&png).unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
    let (clip_before, png_before) = (std::fs::read(&clip).unwrap(), std::fs::read(&png).unwrap());

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    let raw = || RawOptions { originals: Some(2), images: Some(1), ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    assert_eq!(engine.snapshot().compressed_this_run, 2, "{logs}");
    // Default: next to the scanned folder, same subfolders, untouched bytes and dates.
    let backup = root.join("media (originals)");
    assert_eq!(std::fs::read(backup.join("clip.mp4")).unwrap(), clip_before, "{logs}");
    let png_backup = backup.join("sub").join("a.png");
    assert_eq!(std::fs::read(&png_backup).unwrap(), png_before, "{logs}");
    assert_eq!(std::fs::metadata(&png_backup).unwrap().modified().unwrap(), old, "backup date changed");
    assert!(std::fs::metadata(&png).unwrap().len() < png_before.len() as u64);

    // A backup folder chosen inside the scanned folder is never scanned.
    let inside = media.join("old versions");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::copy(&png_backup, inside.join("x.png")).unwrap();
    let (engine2, _) = new_engine(&rt, &root, ff);
    engine2.apply_options(RawOptions {
        backup_dir: Some(inside.to_string_lossy().into_owned()),
        ..raw()
    });
    rt.block_on(engine2.load_dir(media));
    assert_eq!(engine2.snapshot().total_files, 2, "the backup folder must be excluded");
    let _ = std::fs::remove_dir_all(&root);
}

/// WAV -> FLAC: integer PCM becomes a .flac with the original's date (the WAV goes to the
/// backup folder); float PCM stays; a WAV whose .flac name is taken stays. With
/// conversions off, WAVs are not queued at all.
#[test]
fn wav_becomes_flac_only_when_safe() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    if !crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir()).has("flac") {
        eprintln!("skipping: flac not found");
        return;
    }
    let root = test_dir("wav");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let tone = ["-f", "lavfi", "-i", "sine=frequency=440:duration=3"];
    let (pcm, float, taken) = (media.join("pcm.wav"), media.join("float.wav"), media.join("taken.wav"));
    make(&ff, &[&tone[..], &["-c:a", "pcm_s16le"]].concat(), &pcm);
    make(&ff, &[&tone[..], &["-c:a", "pcm_f32le"]].concat(), &float);
    make(&ff, &[&tone[..], &["-c:a", "pcm_s24le"]].concat(), &taken);
    std::fs::write(media.join("taken.flac"), b"someone else's file").unwrap();
    // A FLAC of the same audio, poorly compressed: the new one replaces it.
    let same = media.join("same.wav");
    make(&ff, &[&tone[..], &["-c:a", "pcm_s16le"]].concat(), &same);
    make(&ff, &["-i", same.to_str().unwrap(), "-c:a", "flac", "-compression_level", "0"], &media.join("same.flac"));
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
    std::fs::File::options().write(true).open(&pcm).unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
    let pcm_before = std::fs::read(&pcm).unwrap();

    let rt = runtime();
    let (off, _) = new_engine(&rt, &root, ff.clone());
    off.apply_options(opts("av1_nvenc", 1.0));
    rt.block_on(off.load_dir(media.clone()));
    assert_eq!(off.snapshot().pending_files, 0, "conversions off: no WAV queued");

    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    let raw = || RawOptions { originals: Some(2), convert: Some(1), ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    assert_eq!(engine.snapshot().compressed_this_run, 3, "{logs}");
    let flac = media.join("pcm.flac");
    assert!(!pcm.exists() && flac.exists(), "{logs}");
    assert_eq!(std::fs::metadata(&flac).unwrap().modified().unwrap(), old, "date not kept");
    assert_eq!(std::fs::read(root.join("media (originals)").join("pcm.wav")).unwrap(), pcm_before);
    let decode = |p: &Path| {
        std::process::Command::new(&ff).args(["-v", "error", "-i"]).arg(p).args(["-f", "s32le", "-"]).output().unwrap().stdout
    };
    assert_eq!(decode(&flac), decode(&root.join("media (originals)").join("pcm.wav")), "samples changed");
    assert!(float.exists(), "float WAV must stay");
    // Another file had the name: the conversion gets a number, that file is untouched.
    assert!(std::fs::read(media.join("taken.flac")).unwrap() == b"someone else's file", "{logs}");
    assert!(!taken.exists() && media.join("taken (2).flac").exists(), "{logs}");
    // The same audio under the name: one FLAC stays (the smaller), the WAV went to the backup.
    assert!(!same.exists() && media.join("same.flac").exists(), "{logs}");
    assert!(!media.join("same (2).flac").exists(), "{logs}");
    assert_eq!(decode(&media.join("same.flac")), decode(&root.join("media (originals)").join("same.wav")));
    let _ = std::fs::remove_dir_all(&root);
}

/// Same name, same or other content (pictures and a GIF + MP4), and wrong extensions fixed.
#[test]
fn same_names_and_wrong_extensions() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skipping: bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "ssimulacra2", "djpeg"].iter().all(|t| tools.has(t)) {
        eprintln!("skipping: image tools not found");
        return;
    }
    let root = test_dir("samename");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let pic = |src: &str, extra: &[&str], out: &str| {
        let mut a = vec!["-f", "lavfi", "-i", src, "-frames:v", "1"];
        a.extend(extra);
        make(&ff, &a, &media.join(out));
    };
    // sky: the same picture as an uncompressed PNG already exists -> one sky.png stays.
    pic("testsrc=s=320x240", &[], "sky.bmp");
    pic("testsrc=s=320x240", &["-compression_level", "0"], "sky.png");
    // other: a different picture has the name -> "other (2).png".
    pic("testsrc=s=320x240", &[], "other.bmp");
    pic("mandelbrot=s=320x240", &[], "other.png");
    // Wrong extensions: a JPEG called .png, a WebP called .jpg.
    pic("testsrc2=s=320x240", &["-q:v", "2", "-f", "mjpeg"], "photo.png");
    pic("testsrc2=s=320x240", &["-c:v", "libwebp", "-f", "webp"], "web.jpg");
    // anim.gif and anim.mp4 made from it: the same video -> one anim.mp4 stays.
    make(&ff, &["-f", "lavfi", "-i", "testsrc=s=320x240:r=10:d=3", "-vf", "split[a][b];[a]palettegen[p];[b][p]paletteuse"], &media.join("anim.gif"));
    make(&ff, &["-i", media.join("anim.gif").to_str().unwrap(), "-fps_mode", "passthrough", "-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv444p"], &media.join("anim.mp4"));

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    let raw = || RawOptions { originals: Some(2), convert: Some(1), images: Some(1), fix_ext: Some(1), ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    let names: std::collections::BTreeSet<String> =
        std::fs::read_dir(&media).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    println!("{logs}");
    let want = ["anim.mp4", "other (2).png", "other.png", "photo.jpg", "sky.png", "web.webp"];
    assert_eq!(names, want.iter().map(ToString::to_string).collect(), "{logs}");
    let backup = root.join("media (originals)");
    assert!(backup.join("sky.bmp").exists() && backup.join("other.bmp").exists() && backup.join("anim.gif").exists(), "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Real run, in place, on a folder of copies: `MEDIAOPTI_REAL_DIR=<folder>
/// MEDIAOPTI_REAL_DATA=<history folder> cargo test --release real_folder_run -- --ignored
/// --nocapture`. Originals go to "<folder> (originals)". Never point it at real files.
#[test]
#[ignore = "real run on a folder of copies (MEDIAOPTI_REAL_DIR)"]
fn real_folder_run() {
    let (Some(ff), Ok(dir), Ok(data)) = (ffmpeg(), std::env::var("MEDIAOPTI_REAL_DIR"), std::env::var("MEDIAOPTI_REAL_DATA")) else {
        return;
    };
    let rt = runtime();
    let rec = Arc::new(Recorder::default());
    let engine = Engine::new(Box::new(RecorderUi(rec.clone())), Path::new(&data), ff, rt.handle().clone());
    // The author's batchviproc settings (AV1, quality 20) and mediaopti's defaults otherwise.
    let raw = || RawOptions {
        crf: Some(20.0),
        preset: Some("slow".into()),
        encoder: Some("av1_nvenc".into()),
        concurrency: Some(2.0),
        originals: Some(2),
        backup_dir: None,
        gif_min_mb: Some(1),
        vmaf_floor: Some(93),
        images: Some(2),
        convert: Some(1),
        lossy_docs: Some(1),
        effort: None,
        temp_dir: None,
        fix_ext: None,
    };
    let t = std::time::Instant::now();
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(PathBuf::from(&dir)).await;
        engine.clone().run_batch(raw()).await;
    });
    for line in rec.logs.lock().unwrap().iter() {
        println!("{line}");
    }
    let s = engine.snapshot();
    println!(
        "== {:.0}s | done {} (images {}) | smaller {} | kept {} | failed {} | saved {} MB",
        t.elapsed().as_secs_f64(), s.completed_this_run, s.images_done_this_run,
        s.compressed_this_run, s.skipped_larger_this_run, s.failed_this_run, s.space_saved_mb
    );
}
