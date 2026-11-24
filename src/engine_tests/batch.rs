//! Whole batches and the queue: every file kind in one run, dropped items, stages, chosen folders.

use super::*;

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
        engine.apply_options(opts("av1_nvenc", 2));
        engine.load_dir(videos.clone()).await;
        let s = engine.snapshot();
        assert_eq!(s.total_files, 7, "logs: {:#?}", rec.logs.lock().unwrap());
        assert_eq!(s.skipped_generated, 1);
        // static.gif is a still image: it goes to the image workers (off here), never to video.
        assert_eq!(s.pending_files, 5);
        assert_eq!(s.skipped_small_gifs, 1);
        assert!(!videos.join("old.mediaopti.w1.mp4").exists(), "orphan temp not removed");

        engine.clone().run_batch(opts("av1_nvenc", 2)).await;
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
    engine2.apply_options(opts("av1_nvenc", 2)); // same settings as the run
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
    let raw = || Options { images: ImageMode::Lossless, ..opts("av1_nvenc", 1) };
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
    let off = Options {
        images: ImageMode::Lossless,
        audio: ImageMode::Lossless,
        videos: false,
        convert: Conversions { wav: true, ..Conversions::NONE },
        ..opts("av1_nvenc", 1)
    };
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
    let all = || Options { images: ImageMode::Lossless, audio: ImageMode::Lossless, convert: Conversions::ALL, ..opts("av1_nvenc", 1) };
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
    engine.apply_options(Options { images: ImageMode::Lossless, ..opts("av1_nvenc", 1) });
    rt.block_on(async {
        engine.load_dir(a.clone()).await;
        engine.load_items(vec![b.clone(), c.clone()], false).await;
    });
    let s = engine.snapshot();
    assert_eq!(s.total_files, 2, "a's file replaced by b's and c's");
    assert_eq!(s.dir.as_deref(), Some(root.join("lib").to_string_lossy().as_ref()));
    let _ = std::fs::remove_dir_all(&root);
}
