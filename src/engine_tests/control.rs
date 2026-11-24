//! Stop and Pause: ffmpeg quits cleanly, originals stay, Resume finishes the files.

use super::*;

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
            let runner = tokio::spawn(engine.clone().run_batch(opts("av1_nvenc", 4)));
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
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 1)));
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
        let runner = tokio::spawn(engine.clone().run_batch(opts(&encoder, 1)));
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
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 2)));
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
    let raw = || Options { images: ImageMode::Lossy, audio: ImageMode::Lossy, ..opts("av1_nvenc", 4) };
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
        let runner = tokio::spawn(engine.clone().run_batch(opts("libx264", 1)));
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
