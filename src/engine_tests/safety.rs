//! Safety: the backup folder, long paths, a panic fails only its own file.

use super::*;

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
    let (engine2, _) = new_engine(&rt, &root, ff.clone());
    engine2.apply_options(RawOptions {
        backup_dir: Some(inside.to_string_lossy().into_owned()),
        ..raw()
    });
    rt.block_on(engine2.load_dir(media.clone()));
    assert_eq!(engine2.snapshot().total_files, 2, "the backup folder must be excluded");

    // The option back to its default, the parent folder scanned: the backup folder used by
    // the first run is still left out, and so is a system folder (the Recycle Bin of a drive
    // root, hidden + system: its items keep their extensions).
    std::fs::remove_dir_all(&inside).unwrap();
    let bin = media.join("$RECYCLE.BIN");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(&png_backup, bin.join("$R1A2B3C.png")).unwrap();
    assert!(Command::new("attrib").args(["+h", "+s"]).arg(&bin).status().unwrap().success());
    let (engine3, _) = new_engine(&rt, &root, ff);
    engine3.apply_options(opts("av1_nvenc", 1.0));
    rt.block_on(engine3.load_dir(root.clone()));
    let total = engine3.snapshot().total_files;
    let _ = Command::new("attrib").args(["-h", "-s"]).arg(&bin).status();
    assert_eq!(total, 2, "only media's own files: no backups, nothing from the system folder");
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
