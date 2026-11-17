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
        gif_min_mb: Some(0),
        vmaf_floor: Some(0),
        images: Some(0),
        convert: Some(0),
        lossy_docs: Some(0),
        audio: Some(0),
        ..RawOptions::default()
    }
}

#[test]
fn full_batch_handles_every_file_kind() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
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
    assert_eq!(s.kept_this_run, 1, "{logs}"); // voice.mp4

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

/// NVENC encodes ended by Stop (MEDIAOPTI_STOP_MODE=stop) or by a Pause that frees their
/// memory (=release), MEDIAOPTI_STOP_ROUNDS times on copies in MEDIAOPTI_STOP_DIR. They must
/// quit, not be killed (killing NVENC encodes made the NVIDIA driver reset the GPU): watch
/// the System log for nvlddmkm events meanwhile.
#[test]
#[ignore = "needs MEDIAOPTI_STOP_DIR and an NVIDIA GPU"]
fn nvenc_encodes_quit_cleanly() {
    let (Some(ff), Ok(src)) = (ffmpeg(), std::env::var("MEDIAOPTI_STOP_DIR")) else { return };
    let rounds: u32 = std::env::var("MEDIAOPTI_STOP_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let release = std::env::var("MEDIAOPTI_STOP_MODE").is_ok_and(|m| m == "release");
    for round in 1..=rounds {
        let root = test_dir("nvenc-quit");
        let media = root.join("media");
        std::fs::create_dir_all(&media).unwrap();
        for e in std::fs::read_dir(&src).unwrap().filter_map(Result::ok) {
            std::fs::copy(e.path(), media.join(e.file_name())).unwrap();
        }
        let rt = runtime();
        let (engine, rec) = new_engine(&rt, &root, ff.clone());
        let t = std::time::Instant::now();
        rt.block_on(async {
            engine.load_dir(media.clone()).await;
            let runner = tokio::spawn(engine.clone().run_batch(opts("av1_nvenc", 4.0)));
            tokio::time::sleep(Duration::from_secs(10)).await;
            if release {
                engine.set_paused(true);
                tokio::time::sleep(Duration::from_secs(2)).await;
                engine.release_paused_jobs();
                while !engine.snapshot().telemetry.workers.is_empty() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                engine.stop();
            } else {
                engine.stop();
            }
            tokio::time::timeout(Duration::from_secs(60), runner).await.expect("did not stop").unwrap();
        });
        let logs = rec.logs.lock().unwrap().join("\n");
        let killed = logs.matches("did not quit").count();
        let temps = std::fs::read_dir(&media).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).count();
        println!("round {round}: ended in {:.1} s, {killed} encode(s) had to be killed, {temps} temp file(s) left", t.elapsed().as_secs_f64() - 10.0);
        assert_eq!(killed, 0, "{logs}");
        drop(rt);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn stop_kills_ffmpeg_and_keeps_originals() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
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

/// Pause ends every process (freeing their memory, CPU and GPU) and starts nothing new;
/// Resume finishes the batch normally. BATCHVIPROC_PAUSE_SECS sets how long it stays paused,
/// BATCHVIPROC_PAUSE_ENCODER=av1_nvenc pauses a GPU encode.
#[test]
fn pause_ends_ffmpeg_and_resume_finishes() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
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
        assert_eq!(engine.snapshot().status, Status::Paused);
        let until = std::time::Instant::now() + Duration::from_secs(15);
        while !engine.snapshot().telemetry.workers.is_empty() && std::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_secs(pause_secs)).await;
        let after = engine.snapshot();
        assert_eq!(after.status, Status::Paused);
        assert!(after.telemetry.workers.is_empty(), "a file is still being processed while paused");
        assert_eq!(after.telemetry.total_fps, 0);
        assert!(!temp.exists(), "the stopped encode's output was left");
        assert_eq!(after.completed_this_run, 0);

        engine.set_paused(false);
        assert_eq!(engine.snapshot().status, Status::Running);
        tokio::time::timeout(Duration::from_secs(120), runner).await
            .expect("batch did not finish after resume").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Done, "{logs}");
    assert_eq!(s.compressed_this_run + s.kept_this_run, 2, "{logs}");
    assert_eq!(s.failed_this_run, 0, "{logs}");
    assert!(logs.contains("Paused: 1 file(s) in progress stopped"), "{logs}");
    assert!(logs.contains("Resumed"), "{logs}");
    assert!(!logs.contains("Stall watchdog"), "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A Pause that must give memory back (a game started): the files in progress are ended,
/// their temp output removed, and on Resume they start over and finish like any other.
#[test]
fn pause_releases_memory_and_resume_redoes_the_files() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("pause-release");
    let videos = root.join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    lossless_video(&ff, 12, "1280x720", &videos.join("a.mp4"));
    lossless_video(&ff, 12, "1280x720", &videos.join("b.mp4"));
    lossless_video(&ff, 3, "640x360", &videos.join("c.mp4"));
    let temps = [videos.join("a.mediaopti.w1.mp4"), videos.join("a.mediaopti.w2.mp4"), videos.join("b.mediaopti.w1.mp4"), videos.join("b.mediaopti.w2.mp4")];

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.load_dir(videos.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 2.0)));
        wait_for_progress(&engine).await;
        engine.set_paused(true);
        tokio::time::sleep(Duration::from_millis(500)).await;
        engine.release_paused_jobs();
        // The jobs end (their processes killed, temp files gone) while still paused.
        let until = std::time::Instant::now() + Duration::from_secs(20);
        while !engine.snapshot().telemetry.workers.is_empty() && std::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let s = engine.snapshot();
        assert!(s.telemetry.workers.is_empty(), "jobs still running after release");
        assert_eq!(s.status, Status::Paused);
        assert_eq!(s.completed_this_run, 0);
        assert!(temps.iter().all(|t| !t.exists()), "a temp output was left behind");

        engine.set_paused(false);
        tokio::time::timeout(Duration::from_secs(180), runner).await
            .expect("batch did not finish after resume").unwrap();
    });

    let logs = rec.logs.lock().unwrap().join("\n");
    let s = engine.snapshot();
    assert_eq!(s.status, Status::Done, "{logs}");
    // Every file done exactly once: none lost, none counted twice.
    assert_eq!(s.completed_this_run, 3, "{logs}");
    assert_eq!(s.compressed_this_run + s.kept_this_run, 3, "{logs}");
    assert_eq!(s.failed_this_run, 0, "{logs}");
    assert!(logs.contains("on Resume its encode starts again"), "{logs}");
    let left: Vec<_> = std::fs::read_dir(&videos).unwrap().filter_map(Result::ok).map(|e| e.file_name()).collect();
    assert_eq!(left.len(), 3, "{left:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A real-size Pause: MEDIAOPTI_PAUSE_DIR=<folder of copies> is processed (4 jobs), paused
/// after MEDIAOPTI_PAUSE_AT seconds for MEDIAOPTI_PAUSE_FOR seconds (start a game, or a
/// memory hog, meanwhile), then resumed; results go to MEDIAOPTI_BENCH_KEEP if set.
#[test]
#[ignore = "needs MEDIAOPTI_PAUSE_DIR; long"]
fn pause_for_a_game() {
    let (Some(ff), Ok(src)) = (ffmpeg(), std::env::var("MEDIAOPTI_PAUSE_DIR")) else { return };
    let secs = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (at, for_) = (secs("MEDIAOPTI_PAUSE_AT", 15), secs("MEDIAOPTI_PAUSE_FOR", 60));
    let root = test_dir("pause-game");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    for e in std::fs::read_dir(&src).unwrap().filter_map(Result::ok) {
        if e.path().is_file() {
            std::fs::copy(e.path(), media.join(e.file_name())).unwrap();
        }
    }
    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    let raw = || RawOptions { images: Some(2), audio: Some(2), ..opts("av1_nvenc", 4.0) };
    let t = std::time::Instant::now();
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        let runner = tokio::spawn(engine.clone().run_batch(raw()));
        tokio::time::sleep(Duration::from_secs(at)).await;
        engine.set_paused(true);
        println!("PAUSED at {:.0}s", t.elapsed().as_secs_f64());
        tokio::time::sleep(Duration::from_secs(for_)).await;
        engine.set_paused(false);
        println!("RESUMED at {:.0}s", t.elapsed().as_secs_f64());
        tokio::time::timeout(Duration::from_secs(1800), runner).await.expect("did not finish").unwrap();
    });
    for l in rec.logs.lock().unwrap().iter() {
        println!("{l}");
    }
    let s = engine.snapshot();
    println!("END at {:.0}s: completed {} of {}, failed {}", t.elapsed().as_secs_f64(), s.completed_this_run, s.run_total, s.failed_this_run);
    if let Some(keep) = std::env::var_os("MEDIAOPTI_BENCH_KEEP") {
        let _ = std::fs::remove_dir_all(&keep);
        std::fs::rename(&media, &keep).unwrap();
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Stop works while paused: the suspended ffmpeg is killed and originals are untouched.
#[test]
fn stop_while_paused_keeps_originals() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
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
        skip_test!("bundled ffmpeg not found");
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

    // WMV: its WMA audio can't go into MP4 as it is, so it becomes an MKV with the audio
    // copied (never re-encoded), original removed.
    let wmv_out = videos.join("Developer Diary.mkv");
    assert!(wmv_out.exists() && !videos.join("Developer Diary.wmv").exists(), "{logs}");
    assert!(audio_codec(&ff, &wmv_out).is_some_and(|c| c.starts_with("wma")), "{logs}");
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
        skip_test!("bundled ffmpeg not found");
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

/// Only a GIF whose video was not smaller goes through the GIF optimizer. One whose video
/// encode fails (its temp name is taken by a folder, so ffmpeg can't write) or that is no
/// longer animated when its turn comes is left as it is, now and in later runs.
#[test]
fn gifs_the_video_path_fails_on_are_left_alone() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("gif-fallback");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let (broken, still) = (media.join("broken.gif"), media.join("still.gif"));
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10:duration=3"], &broken);
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10:duration=3"], &still);
    // Every video attempt (NVENC, then x264) writes to this name: a folder makes them fail.
    let blocked = crate::args::temp_name(&broken, 1);
    std::fs::create_dir_all(&blocked).unwrap();
    let (broken_before, still_bytes) = (std::fs::read(&broken).unwrap(), {
        make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=1:duration=1", "-frames:v", "1"], &root.join("still-frame.gif"));
        std::fs::read(root.join("still-frame.gif")).unwrap()
    });

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    // Every GIF is a video job (limit 0); images on, so the GIF optimizer could run.
    let raw = || RawOptions { images: Some(1), ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        assert_eq!(engine.snapshot().pending_files, 2);
        // Queued as an animated GIF; a single still frame by the time it is encoded.
        std::fs::write(&still, &still_bytes).unwrap();
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert!(logs.contains("for broken.gif (remembered in history)"), "{logs}");
    assert!(logs.contains("Skipped still.gif: it is no longer an animated GIF"), "{logs}");
    assert!(!logs.contains("[Images]") && !logs.contains("stays a GIF"), "no GIF pass: {logs}");
    assert_eq!(engine.snapshot().failed_this_run, 1, "{logs}");
    assert_eq!(std::fs::read(&broken).unwrap(), broken_before);
    assert_eq!(std::fs::read(&still).unwrap(), still_bytes);
    std::fs::remove_dir(&blocked).unwrap();

    // Next scan: the failed GIF is not sent to the GIF optimizer; the changed one (not
    // remembered) is placed as what it is now, a still image.
    let (engine2, _) = new_engine(&rt, &root, ff);
    engine2.apply_options(raw());
    rt.block_on(engine2.load_dir(media));
    let s = engine2.snapshot();
    assert_eq!(s.pending_files, 1, "{logs}");
    let queued = engine2.queue_preview(10);
    assert!(queued.len() == 1 && queued[0].0.contains("still.gif") && queued[0].1 == "Image", "{queued:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A big GIF whose video was not smaller while images were off gets the GIF optimizer once
/// images are on, without another video encode; after that it is known.
#[test]
fn a_gif_remembered_as_video_gets_the_gif_optimizer_later() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("gif-later");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let gif = media.join("anim.gif");
    // A few hundred bytes of flat colour: no MP4 is that small.
    make(&ff, &["-f", "lavfi", "-i", "color=c=red:size=320x240:rate=2:duration=2"], &gif);
    let rt = runtime();
    let run = |images: u32| {
        let (engine, rec) = new_engine(&rt, &root, ff.clone());
        let raw = || RawOptions { images: Some(images), ..opts("av1_nvenc", 1.0) };
        rt.block_on(async {
            engine.apply_options(raw());
            engine.load_dir(media.clone()).await;
            let pending = engine.snapshot().pending_files;
            engine.clone().run_batch(raw()).await;
            (pending, rec.logs.lock().unwrap().join("\n"))
        })
    };
    // Images off: not smaller as a video, remembered as one.
    let (pending, logs) = run(0);
    println!("{logs}");
    assert_eq!(pending, 1, "{logs}");
    assert!(logs.contains("Video was not smaller for anim.gif"), "{logs}");
    // Images on: straight to the GIF optimizer, no encode.
    let (pending, logs) = run(1);
    println!("{logs}");
    assert_eq!(pending, 1, "{logs}");
    assert!(!logs.contains("Processing: anim.gif"), "no second video encode: {logs}");
    assert!(logs.contains("[Images]") && logs.contains("anim.gif"), "{logs}");
    // Done now.
    let (pending, logs) = run(1);
    assert_eq!(pending, 0, "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Nothing besides the picture is lost: an MKV keeps both audio tracks, its subtitles,
/// chapters and attached font; an AVI (audio re-encoded) keeps both audio tracks; a WebM
/// with WebVTT subtitles (not possible in MP4) becomes an MKV that keeps them.
#[test]
fn videos_keep_audio_tracks_subtitles_attachments_and_chapters() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("keep-streams");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let (srt, vtt, chap, font) = (root.join("s.srt"), root.join("s.vtt"), root.join("c.txt"), root.join("f.ttf"));
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:02,000\nhello\n").unwrap();
    std::fs::write(&vtt, "WEBVTT\n\n00:00.000 --> 00:02.000\nhello\n").unwrap();
    std::fs::write(&chap, ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=2000\ntitle=One\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=2000\nEND=4000\ntitle=Two\n").unwrap();
    std::fs::write(&font, "not really a font").unwrap();
    let (src, a1, a2) = ("testsrc2=size=640x360:rate=30:duration=4,noise=alls=30:allf=t", "sine=f=440:d=4", "sine=f=880:d=4");
    let (s, v, c, f) = (srt.to_str().unwrap(), vtt.to_str().unwrap(), chap.to_str().unwrap(), font.to_str().unwrap());
    // Lossless video so every one gets smaller.
    make(&ff, &["-f", "lavfi", "-i", src, "-f", "lavfi", "-i", a1, "-f", "lavfi", "-i", a2, "-i", s, "-i", c,
        "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s", "-map_metadata", "4", "-map_chapters", "4",
        "-attach", f, "-metadata:s:t", "mimetype=application/x-truetype-font",
        "-c:v", "libx264", "-qp", "0", "-c:a", "aac", "-c:s", "srt"], &media.join("show.mkv"));
    make(&ff, &["-f", "lavfi", "-i", src, "-f", "lavfi", "-i", a1, "-f", "lavfi", "-i", a2,
        "-map", "0:v", "-map", "1:a", "-map", "2:a", "-c:v", "mpeg4", "-q:v", "1", "-c:a", "mp3"], &media.join("old.avi"));
    make(&ff, &["-f", "lavfi", "-i", src, "-f", "lavfi", "-i", a1, "-i", v,
        "-map", "0:v", "-map", "1:a", "-map", "2:s", "-c:v", "libvpx-vp9", "-lossless", "1", "-c:a", "libopus", "-c:s", "webvtt"], &media.join("clip.webm"));

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.apply_options(opts("av1_nvenc", 2.0));
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(opts("av1_nvenc", 2.0)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert_eq!(engine.snapshot().failed_this_run, 0, "{logs}");
    let probe = |p: &Path| {
        let out = Command::new(&ff).args(["-hide_banner", "-i"]).arg(p).output().unwrap();
        crate::args::Streams::parse(&String::from_utf8_lossy(&out.stderr))
    };
    let show = probe(&media.join("show.mkv"));
    assert_eq!((show.audio, show.subtitles.len(), show.attachments, show.chapters), (2, 1, 1, 2), "{show:?}\n{logs}");
    let old = probe(&media.join("old.mp4"));
    assert_eq!(old.audio, 2, "{old:?}\n{logs}");
    assert!(!media.join("old.avi").exists() && !media.join("clip.webm").exists(), "{logs}");
    let clip = probe(&media.join("clip.mkv"));
    assert_eq!((clip.audio, clip.subtitles.as_slice()), (1, ["webvtt".to_string()].as_slice()), "{clip:?}\n{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A phone video's tags survive: recording date, place, make and model in the MP4, and the
/// date in an MKV (a QuickTime date listed twice used to be dropped there).
#[test]
fn videos_keep_their_tags() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("keep-tags");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let src = "testsrc2=size=640x360:rate=30:duration=2,noise=alls=30:allf=t";
    let tags = ["-metadata", "creation_time=2024-05-01T10:20:30Z", "-metadata", "com.apple.quicktime.location.ISO6709=+12.3456-045.6789+010.000/",
        "-metadata", "com.apple.quicktime.make=Apple", "-metadata", "com.apple.quicktime.model=iPhone 13", "-metadata", "title=Viagem"];
    make(&ff, &[&["-f", "lavfi", "-i", src, "-f", "lavfi", "-i", "sine=d=2", "-c:v", "libx264", "-qp", "0", "-c:a", "aac", "-movflags", "use_metadata_tags"][..], &tags].concat(), &media.join("IMG_0001.MOV"));
    // PCM makes the output MKV (with FLAC).
    make(&ff, &[&["-f", "lavfi", "-i", src, "-f", "lavfi", "-i", "sine=d=2", "-c:v", "libx264", "-qp", "0", "-c:a", "pcm_s16le", "-movflags", "use_metadata_tags"][..], &tags].concat(), &media.join("IMG_0002.MOV"));

    let probe = |p: &Path| {
        let out = Command::new(&ff).args(["-hide_banner", "-i"]).arg(p).output().unwrap();
        crate::args::Streams::parse(&String::from_utf8_lossy(&out.stderr))
    };
    let before = probe(&media.join("IMG_0001.MOV"));
    assert_eq!(before.tags.len(), 5, "{:?}", before.tags);
    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.apply_options(opts("av1_nvenc", 2.0));
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(opts("av1_nvenc", 2.0)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert_eq!(engine.snapshot().failed_this_run, 0, "{logs}");
    for out in ["IMG_0001.mp4", "IMG_0002.mkv"] {
        let after = probe(&media.join(out));
        for (k, v) in before.tags.iter().filter(|(k, _)| k != "creation_time") {
            assert!(after.tags.iter().any(|(ak, av)| ak == k && av == v), "{out}: {k} lost: {:?}\n{logs}", after.tags);
        }
        assert!(after.creation_time().is_some_and(|t| t.starts_with("2024-05-01T10:20:30")), "{out}: {:?}", after.tags);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Audio is never lost or downmixed: PCM in a MOV becomes FLAC in an MKV (checked to
/// decode to the same samples), AC3 5.1 in an AVI is copied into the MP4 with its six
/// channels, and an HDR10 video keeps its transfer and static metadata.
#[test]
fn audio_is_kept_or_made_flac_and_hdr10_is_kept() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("audio-hdr");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let src = "testsrc2=size=640x360:rate=30:duration=3,noise=alls=30:allf=t";
    make(&ff, &["-f", "lavfi", "-i", src, "-f", "lavfi", "-i", "sine=f=440:d=3", "-c:v", "libx264", "-qp", "0", "-c:a", "pcm_s16le"], &media.join("camera.mov"));
    make(&ff, &["-f", "lavfi", "-i", src, "-f", "lavfi", "-i", "sine=f=440:d=3", "-c:v", "mpeg4", "-q:v", "1", "-af", "aformat=channel_layouts=5.1", "-c:a", "ac3"], &media.join("film.avi"));
    make(&ff, &["-f", "lavfi", "-i", src, "-vf", "format=yuv420p10le", "-c:v", "libx265", "-x265-params",
        "log-level=0:lossless=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400:hdr10=1"],
        &media.join("hdr10.mkv"));

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.apply_options(opts("av1_nvenc", 2.0));
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(opts("av1_nvenc", 2.0)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert_eq!(engine.snapshot().failed_this_run, 0, "{logs}");
    assert_eq!(engine.snapshot().compressed_this_run, 3, "{logs}");
    let probe = |p: &Path| {
        let out = Command::new(&ff).args(["-hide_banner", "-i"]).arg(p).output().unwrap();
        crate::args::Streams::parse(&String::from_utf8_lossy(&out.stderr))
    };
    let camera = probe(&media.join("camera.mkv"));
    assert_eq!(camera.audio_codecs, ["flac"], "{camera:?}\n{logs}");
    let film = probe(&media.join("film.mp4"));
    assert_eq!((film.audio_codecs.as_slice(), film.audio_channels.as_slice()), (["ac3".to_string()].as_slice(), [6].as_slice()), "{film:?}\n{logs}");
    let hdr = probe(&media.join("hdr10.mkv"));
    assert!(hdr.hdr, "{hdr:?}\n{logs}");
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

/// Paused during the quality retry (after its first step was encoded), the video carries
/// on from there on Resume: the steps done are not encoded again and the result is byte for
/// byte the one of a run without a pause.
#[test]
fn pause_during_quality_retry_resumes_from_the_last_step() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("vmaf-resume");
    let src = root.join("src.mp4");
    compressed_h264(&ff, 4, 24, &src);
    let rt = runtime();
    let run = |name: &str, pause: bool| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("light.mp4");
        std::fs::copy(&src, &file).unwrap();
        let (engine, rec) = new_engine(&rt, &root.join(format!("{name}-data")), ff.clone());
        let raw = || RawOptions { crf: Some(20.0), vmaf_floor: Some(93), ..opts("av1_nvenc", 1.0) };
        rt.block_on(async {
            engine.apply_options(raw());
            engine.load_dir(dir.clone()).await;
            let runner = tokio::spawn(engine.clone().run_batch(raw()));
            if pause {
                // Wait for the first quality measurement (the +4 step is encoded by then).
                loop {
                    let s = engine.snapshot();
                    if s.telemetry.workers.iter().any(|w| w.step.starts_with("measuring quality")) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                engine.set_paused(true);
                while !engine.snapshot().telemetry.workers.is_empty() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let parked: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).map(|e| e.file_name()).collect();
                println!("while paused: {parked:?}");
                engine.set_paused(false);
            }
            tokio::time::timeout(Duration::from_secs(300), runner).await.expect("did not finish").unwrap();
        });
        let logs = rec.logs.lock().unwrap().join("\n");
        (std::fs::read(&file).unwrap(), logs)
    };
    let (plain, plain_logs) = run("plain", false);
    let (paused, logs) = run("paused", true);
    println!("{logs}");
    assert!(plain_logs.contains("Quality retry for light.mp4"), "{plain_logs}");
    assert!(logs.contains("continues from its last finished step"), "{logs}");
    assert!(logs.contains("the preset encode was done before the pause"), "{logs}");
    assert!(logs.contains("was done before the pause; measuring it"), "{logs}");
    assert!(plain == paused, "the paused run's result differs from the plain run's");
    let leftovers: Vec<_> = walkdir::WalkDir::new(&root).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn quality_retry_accepts_only_above_the_vmaf_floor() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
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
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "jpegtran", "djpeg"].iter().all(|t| tools.has(t)) {
        skip_test!("image tools not found");
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
    let cpu = |t: &[(usize, usize, usize); 4]| t[1..].iter().fold((0, 0), |a, x| (a.0 + x.0, a.1 + x.1));
    assert_eq!(cpu(&s.type_progress), (3, 3), "{logs}");
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
    rt.block_on(engine3.load_dir(media.clone()));
    assert_eq!(engine3.snapshot().pending_files, 3);

    // Removing metadata doesn't redo processed files, unless asked to.
    let strip = |redo: u32| RawOptions { strip: Some(1), redo_stripped: Some(redo), ..raw() };
    let (engine5, _) = new_engine(&rt, &root, ff.clone());
    engine5.apply_options(strip(0));
    rt.block_on(engine5.load_dir(media.clone()));
    assert_eq!(engine5.snapshot().pending_files, 1, "only the hard link");
    let (engine6, _) = new_engine(&rt, &root, ff.clone());
    engine6.apply_options(strip(1));
    rt.block_on(engine6.load_dir(media.clone()));
    assert_eq!(engine6.snapshot().pending_files, 3, "processed without stripping: redone");
    rt.block_on(engine6.run_batch(strip(1)));
    let (engine8, _) = new_engine(&rt, &root, ff.clone());
    engine8.apply_options(strip(1));
    rt.block_on(engine8.load_dir(media.clone()));
    assert_eq!(engine8.snapshot().pending_files, 1, "done once with stripping: not again");
    // The redo option alone (metadata kept) changes nothing.
    let (engine7, _) = new_engine(&rt, &root, ff.clone());
    engine7.apply_options(RawOptions { redo_stripped: Some(1), ..raw() });
    rt.block_on(engine7.load_dir(media));
    assert_eq!(engine7.snapshot().pending_files, 1);

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
    // MEDIAOPTI_BENCH_CONVERT=1: conversions on (BMP/TGA -> PNG, WAV -> FLAC, CBR -> CBZ).
    let convert = std::env::var("MEDIAOPTI_BENCH_CONVERT").ok().and_then(|e| e.parse().ok());
    // MEDIAOPTI_BENCH_DOCS=1: visually lossless PDF images.
    let docs = std::env::var("MEDIAOPTI_BENCH_DOCS").ok().and_then(|e| e.parse().ok());
    // MEDIAOPTI_BENCH_JOBS=<n>: parallel video encodes (default 2).
    let jobs = std::env::var("MEDIAOPTI_BENCH_JOBS").ok().and_then(|e| e.parse().ok()).unwrap_or(2.0);
    let raw = || RawOptions { images: Some(2), audio: Some(2), convert: convert.or(Some(0)), lossy_docs: docs.or(Some(0)), ..opts("av1_nvenc", jobs) };
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
    // MEDIAOPTI_BENCH_LOG=1: the whole log, not only failures and totals.
    let all = std::env::var_os("MEDIAOPTI_BENCH_LOG").is_some();
    for l in logs.iter().filter(|l| all || l.contains("Failed") || l.starts_with("Done") || l.starts_with("Images:")) {
        println!("{l}");
    }
    println!(
        "{} images in {secs:.0}s ({:.2} s/image wall), {:.1} MB -> {:.1} MB ({:.2}% saved)",
        s.type_progress[1..].iter().map(|t| t.0).sum::<usize>(),
        secs / s.type_progress[1..].iter().map(|t| t.0).sum::<usize>().max(1) as f64,
        before as f64 / 1048576.0,
        after as f64 / 1048576.0,
        100.0 * (before - after) as f64 / before as f64
    );
    // MEDIAOPTI_BENCH_KEEP=<dir>: move the results there instead of deleting them.
    if let Some(keep) = std::env::var_os("MEDIAOPTI_BENCH_KEEP") {
        let _ = std::fs::remove_dir_all(&keep);
        std::fs::rename(&media, &keep).unwrap();
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn originals_go_to_a_mirrored_backup_folder() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !tools.has("oxipng") {
        skip_test!("image tools not found");
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
        skip_test!("bundled ffmpeg not found");
        return;
    };
    if !crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir()).has("flac") {
        skip_test!("flac not found");
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
    let raw = || RawOptions { originals: Some(2), convert: Some(1), audio: Some(1), ..opts("av1_nvenc", 1.0) };
    // Twice: same.wav and same.flac may run at the same time, and then the duplicate is left
    // for the next run (one copy must never be replaced while the other is processed).
    let mut made_smaller = 0;
    for _ in 0..2 {
        rt.block_on(async {
            engine.apply_options(raw());
            engine.load_dir(media.clone()).await;
            engine.clone().run_batch(raw()).await;
        });
        made_smaller += engine.snapshot().compressed_this_run;
    }
    let logs = rec.logs.lock().unwrap().join("\n");
    assert!(!logs.contains("Failed"), "{logs}");
    // pcm and taken converted, same.flac re-encoded (an old encoder), same.wav a duplicate.
    assert_eq!(made_smaller, 4, "{logs}");
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

/// What a batch tells: empty files are counted when scanned, a cut-off picture says so
/// instead of "already optimal", and files left for later (hard links) count as skipped, not
/// as kept.
#[test]
fn empty_cut_off_and_skipped_files_are_told_apart() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["jpegtran", "djpeg", "oxipng"].iter().all(|t| tools.has(t)) {
        skip_test!("image tools not found");
        return;
    }
    let root = test_dir("tellapart");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("empty.png"), b"").unwrap();
    let whole = root.join("whole.jpg");
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=s=640x480", "-frames:v", "1", "-q:v", "2"], &whole);
    let data = std::fs::read(&whole).unwrap();
    std::fs::write(media.join("cut.jpg"), &data[..data.len() * 55 / 100]).unwrap();
    make(&ff, &["-f", "lavfi", "-i", "testsrc=s=64x48", "-frames:v", "1", "-compression_level", "0"], &media.join("linked.png"));
    std::fs::hard_link(media.join("linked.png"), media.join("linked again.png")).unwrap();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    let raw = || RawOptions { images: Some(1), ..opts("libx264", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert!(logs.contains("1 empty (0-byte) file(s) left out"), "{logs}");
    assert!(logs.contains("Kept cut.jpg: it looks cut off"), "{logs}");
    let s = engine.snapshot();
    assert_eq!(s.skipped_this_run, 2, "the two names of one file\n{logs}");
    assert!(logs.contains("skipped for now 2"), "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Files in a folder whose path is longer than 260 characters are processed like any other:
/// tools that can't open such paths (flac.exe) work on a short copy, and the originals go to
/// a (just as long) backup folder.
#[test]
fn files_under_long_paths_are_processed() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "flac"].iter().all(|t| tools.has(t)) {
        skip_test!("oxipng or flac not found");
        return;
    }
    let root = test_dir("longpath");
    let mut media = root.join("media");
    for i in 0..5 {
        media.push(format!("folder {i} with a long name to pass the old limit of Windows paths"));
    }
    std::fs::create_dir_all(&media).unwrap();
    assert!(media.as_os_str().len() > 300);
    // ffmpeg itself handles long paths: it makes the files in place.
    let art = root.join("art.png");
    make(&ff, &["-f", "lavfi", "-i", "testsrc=s=300x300", "-frames:v", "1", "-compression_level", "0"], &art);
    make(&ff, &["-f", "lavfi", "-i", "sine=duration=3", "-i", art.to_str().unwrap(), "-map", "0", "-map", "1", "-c:a", "flac",
        "-c:v", "png", "-disposition:v", "attached_pic"], &media.join("song.flac"));
    // No cover: only flac.exe's re-encode can make it smaller.
    make(&ff, &["-f", "lavfi", "-i", "sine=duration=3", "-c:a", "flac", "-compression_level", "0"], &media.join("plain.flac"));
    make(&ff, &["-f", "lavfi", "-i", "testsrc=s=200x150", "-frames:v", "1", "-compression_level", "0"], &media.join("picture.png"));
    lossless_video(&ff, 2, "320x240", &media.join("clip.mp4"));

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    let raw = || RawOptions { images: Some(1), audio: Some(1), originals: Some(2), ..opts("libx264", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(root.join("media")).await;
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    let s = engine.snapshot();
    assert_eq!((s.completed_this_run, s.failed_this_run), (4, 0), "{logs}");
    assert_eq!(s.compressed_this_run, 4, "all four were made smaller\n{logs}");
    let backup = root.join("media (originals)").join(media.strip_prefix(root.join("media")).unwrap());
    for name in ["song.flac", "plain.flac", "picture.png", "clip.mp4"] {
        assert!(backup.join(name).exists(), "{name} in the backup folder\n{logs}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A panic while processing one file (a bug, a malformed file nobody foresaw) fails that
/// file only: the batch goes on and ends, its counters come back to zero, and the file is
/// not remembered (tried again next run).
#[test]
fn a_panic_fails_only_its_own_file() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !tools.has("oxipng") {
        skip_test!("oxipng not found");
        return;
    }
    let root = test_dir("panic");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    for name in ["panic-me.png", "fine.png"] {
        make(&ff, &["-f", "lavfi", "-i", "testsrc=s=200x150", "-frames:v", "1", "-compression_level", "0"], &media.join(name));
    }
    lossless_video(&ff, 2, "320x240", &media.join("panic-me-too.mp4"));
    lossless_video(&ff, 2, "320x240", &media.join("fine.mp4"));
    crate::system::PANIC_ON.lock().unwrap().extend(["panic-me.png".to_string(), "panic-me-too.mp4".to_string()]);

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    let raw = || RawOptions { images: Some(1), ..opts("libx264", 1.0) };
    let finished = rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        tokio::time::timeout(Duration::from_secs(300), engine.clone().run_batch(raw())).await.is_ok()
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    assert!(finished, "the batch must end\n{logs}");
    assert_eq!(logs.matches("internal error").count(), 2, "{logs}");
    let s = engine.snapshot();
    assert_eq!((s.failed_this_run, s.completed_this_run), (2, 4), "{logs}");
    // The two that panicked are still to do; the others are done.
    rt.block_on(engine.load_dir(media));
    assert_eq!(engine.snapshot().pending_files, 2, "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Tools that can't start (quarantined by an antivirus, a missing DLL) are not the files'
/// fault: the files are not remembered as broken, and after a run of such failures the
/// remaining CPU files are left for the next run instead of all failing.
#[test]
fn tools_that_cannot_start_do_not_mark_files_broken() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("toolfail");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    // More files than can be running when the 20th failure ends the queue (one per core).
    const FILES: usize = 60;
    for i in 0..FILES {
        make(&ff, &["-f", "lavfi", "-i", &format!("color=c=0x{:02x}6040:s=64x48", i * 4), "-frames:v", "1", "-compression_level", "0"], &media.join(format!("p{i:02}.png")));
    }
    // Every PNG optimizer is "installed" but is not a program.
    let data = root.join("appdata");
    std::fs::create_dir_all(&data).unwrap();
    let bad = data.join("broken.exe");
    std::fs::write(&bad, b"not a program").unwrap();
    let tools: std::collections::HashMap<&str, &Path> = ["pingo", "beeim", "oxipng", "ect"].iter().map(|t| (*t, bad.as_path())).collect();
    std::fs::write(data.join("tools.json"), serde_json::to_string(&tools).unwrap()).unwrap();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    let raw = || RawOptions { images: Some(1), ..opts("libx264", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    assert!(!logs.contains("will not retry"), "{logs}");
    assert!(logs.contains("could not start"), "{logs}");
    assert!(logs.contains("kept failing to start"), "{logs}");
    let failed = engine.snapshot().failed_this_run;
    assert!((20..FILES).contains(&failed), "the rest is left for the next run: {failed} failed\n{logs}");
    // Nothing was remembered: all of them are still to do.
    rt.block_on(engine.load_dir(media));
    assert_eq!(engine.snapshot().pending_files, FILES, "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A file already at the new name with the same picture or video, but without something the
/// original has (an audio track, a JPEG comment), is not "the same content": the original's
/// new version gets its own name and nothing is lost.
#[test]
fn same_content_keeps_what_the_original_has() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["jpegtran", "djpeg"].iter().all(|t| tools.has(t)) {
        skip_test!("image tools not found");
        return;
    }
    let root = test_dir("samekeep");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    // clip.mov: lossless video and two audio tracks; clip.mp4: the same video, no audio.
    let mov = media.join("clip.mov");
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=4", "-f", "lavfi", "-i", "sine=duration=4",
        "-f", "lavfi", "-i", "sine=frequency=880:duration=4", "-map", "0:v", "-map", "1:a", "-map", "2:a",
        "-c:v", "libx264", "-preset", "ultrafast", "-qp", "0", "-c:a", "aac"], &mov);
    make(&ff, &["-i", mov.to_str().unwrap(), "-map", "0:v", "-c:v", "libx264", "-preset", "slow", "-crf", "23"], &media.join("clip.mp4"));
    // photo.png: a JPEG with a long comment; photo.jpg: the same picture without it.
    let src = root.join("src.jpg");
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=s=320x240", "-frames:v", "1", "-q:v", "2"], &src);
    let data = std::fs::read(&src).unwrap();
    std::fs::write(media.join("photo.png"), crate::image::jpeg::with_segments(&data, &vec![(0xFE, vec![b'x'; 20_000])])).unwrap();
    std::fs::write(media.join("photo.jpg"), crate::image::jpeg::with_segments(&data, &Vec::new())).unwrap();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    let raw = || RawOptions { originals: Some(2), images: Some(1), fix_ext: Some(1), ..opts("libx264", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    let names: std::collections::BTreeSet<String> =
        std::fs::read_dir(&media).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    let want = ["clip (2).mp4", "clip.mp4", "photo (2).jpg", "photo.jpg"];
    assert_eq!(names, want.iter().map(ToString::to_string).collect(), "{logs}");
    let comment = crate::image::jpeg::segments(&media.join("photo (2).jpg")).unwrap();
    assert!(comment.iter().any(|(m, d)| *m == 0xFE && d.len() == 20_000), "the comment survives");
    let _ = std::fs::remove_dir_all(&root);
}

/// Same name, same or other content (pictures and a GIF + MP4), and wrong extensions fixed.
#[test]
fn same_names_and_wrong_extensions() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "ssimulacra2", "djpeg"].iter().all(|t| tools.has(t)) {
        skip_test!("image tools not found");
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
        concurrency: Some(std::env::var("MEDIAOPTI_BENCH_JOBS").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0)),
        originals: Some(2),
        gif_min_mb: Some(1),
        vmaf_floor: Some(93),
        images: Some(2),
        convert: Some(1),
        lossy_docs: Some(1),
        audio: Some(2),
        ..RawOptions::default()
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
        "== {:.0}s | done {} (images {}) | smaller {} | kept {} | skipped {} | failed {} | saved {} MB",
        t.elapsed().as_secs_f64(), s.completed_this_run, s.type_progress[1..].iter().map(|t| t.0).sum::<usize>(),
        s.compressed_this_run, s.kept_this_run, s.skipped_this_run, s.failed_this_run, s.space_saved_mb
    );
}

/// Dropped files and folders: added to what is loaded (the start folder becomes their common
/// folder), and added to a running batch, whose new CPU files start a worker of their own.
#[test]
fn dropped_items_join_the_queue() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !tools.has("oxipng") {
        skip_test!("oxipng not found");
        return;
    }
    let root = test_dir("drop");
    let (a, b, c) = (root.join("media").join("a"), root.join("media").join("b"), root.join("later"));
    for d in [&a, &b, &c] {
        std::fs::create_dir_all(d).unwrap();
    }
    lossless_video(&ff, 4, "1280x720", &a.join("clip.mp4"));
    let png = |dir: &Path, name: &str| make(&ff, &["-f", "lavfi", "-i", "testsrc=s=200x150", "-frames:v", "1", "-compression_level", "0"], &dir.join(name));
    png(&b, "one.png");
    png(&c, "two.png");
    png(&c, "three.png");

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    let raw = || RawOptions { images: Some(1), ..opts("av1_nvenc", 1.0) };
    engine.apply_options(raw());
    rt.block_on(async {
        engine.load_dir(a.clone()).await;
        assert_eq!(engine.snapshot().pending_files, 1);
        // Idle: a folder and a single file are added; the start folder becomes "media".
        engine.load_items(vec![b.clone(), a.join("clip.mp4")], true).await;
        let s = engine.snapshot();
        assert_eq!(s.pending_files, 2, "the video once, plus one.png");
        assert_eq!(s.dir.as_deref(), Some(root.join("media").to_string_lossy().as_ref()));

        // Running (only a video worker so far): later/ is added; a CPU worker starts for it.
        let run = tokio::spawn(engine.clone().run_batch(raw()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        engine.load_items(vec![c.clone()], true).await;
        run.await.unwrap();
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    let s = engine.snapshot();
    assert!(logs.contains("Added to the queue: 0 videos and 2 images"), "{logs}");
    assert_eq!(s.completed_this_run, 4, "{logs}");
    assert_eq!(s.failed_this_run, 0, "{logs}");
    assert_eq!(s.compressed_this_run, 4, "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The queue runs in stages: videos first, then (as soon as no video is left to start)
/// audio, documents, images. Types can be switched off, and each conversion on its own.
#[test]
fn queue_runs_in_stages_and_types_can_be_skipped() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "flac"].iter().all(|t| tools.has(t)) {
        skip_test!("tools not found");
        return;
    }
    let root = test_dir("stages");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    lossless_video(&ff, 3, "640x360", &media.join("clip.mp4"));
    let tone = ["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "pcm_s16le"];
    make(&ff, &tone, &media.join("song.wav"));
    make(&ff, &tone, &media.join("other.wav"));
    make(&ff, &["-f", "lavfi", "-i", "testsrc=s=200x150", "-frames:v", "1", "-compression_level", "0"], &media.join("pic.png"));
    make(&ff, &["-f", "lavfi", "-i", "testsrc=s=200x150", "-frames:v", "1"], &media.join("old.bmp"));

    let rt = runtime();
    // Switches: videos off, BMP -> PNG off (WAV -> FLAC on): only audio and the PNG queue.
    let (engine, _) = new_engine(&rt, &root, ff.clone());
    let off = RawOptions { images: Some(1), audio: Some(1), videos: Some(0), conversions: Some(1), ..opts("av1_nvenc", 1.0) };
    engine.apply_options(off);
    rt.block_on(engine.load_dir(media.clone()));
    let s = engine.snapshot();
    assert_eq!(s.queue_counts.map(|c| c.0), [0, 2, 0, 1], "videos off, BMP conversion off");
    assert_eq!(engine.queue_preview(10).iter().map(|q| q.1).collect::<Vec<_>>(), ["Audio", "Audio", "Image"]);
    engine.clear_queue();
    let s = engine.snapshot();
    assert!(s.dir.is_none() && s.pending_files == 0 && s.queue_counts.iter().all(|c| c.0 == 0));

    // Everything on: audio first, images after (the CPU stages start once no video is left to
    // start; the video worker logs its file a moment after claiming it).
    let (engine, rec) = new_engine(&rt, &root, ff);
    let all = || RawOptions { images: Some(1), audio: Some(1), convert: Some(1), ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(all());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(all()).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    let at = |needle: &str| logs.find(needle).unwrap_or_else(|| panic!("no '{needle}' in:\n{logs}"));
    assert!(at("Now processing 2 audio files") < at("Now processing 2 images"), "{logs}");
    let s = engine.snapshot();
    assert_eq!(s.failed_this_run, 0, "{logs}");
    // Each type finished all of its files.
    assert_eq!(s.type_progress.map(|t| (t.0, t.1)), [(1, 1), (2, 2), (0, 0), (2, 2)], "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Several folders picked at once replace what was loaded; the start folder is their common one.
#[test]
fn several_chosen_folders_replace_the_queue() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("multi");
    let (a, b, c) = (root.join("lib").join("a"), root.join("lib").join("b"), root.join("lib").join("c"));
    for (d, n) in [(&a, "1.png"), (&b, "2.png"), (&c, "3.png")] {
        std::fs::create_dir_all(d).unwrap();
        make(&ff, &["-f", "lavfi", "-i", "testsrc=s=64x48", "-frames:v", "1"], &d.join(n));
    }
    let rt = runtime();
    let (engine, _) = new_engine(&rt, &root, ff);
    engine.apply_options(RawOptions { images: Some(1), ..opts("av1_nvenc", 1.0) });
    rt.block_on(async {
        engine.load_dir(a.clone()).await;
        engine.load_items(vec![b.clone(), c.clone()], false).await;
    });
    let s = engine.snapshot();
    assert_eq!(s.total_files, 2, "a's file replaced by b's and c's");
    assert_eq!(s.dir.as_deref(), Some(root.join("lib").to_string_lossy().as_ref()));
    let _ = std::fs::remove_dir_all(&root);
}
