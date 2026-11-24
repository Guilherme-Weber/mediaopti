//! Animated GIFs: small ones stay GIFs, the video path and the GIF optimizer fallback.

use super::*;

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
    let raw = Options { gif_min_bytes: 1024 * 1024, ..opts("av1_nvenc", 1) };
    rt.block_on(async {
        engine.apply_options(raw);
        engine.load_dir(videos.clone()).await;
        let s = engine.snapshot();
        assert_eq!(s.skipped_small_gifs, 1);
        assert_eq!(s.pending_files, 2);
        engine.clone().run_batch(Options { gif_min_bytes: 1024 * 1024, ..opts("av1_nvenc", 1) }).await;
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
    let raw = || Options { images: ImageMode::Lossless, ..opts("av1_nvenc", 1) };
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
    assert!(logs.contains("Failed on broken.gif: ffmpeg exited with code") && logs.contains("); remembered in history"), "{logs}");
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

/// A GIF with an extension block after its last picture (Photoshop and After Effects put
/// their XMP there) becomes a video: ffmpeg's GIF demuxer hands the block over as one more
/// packet, and counting packets took it for a frame the video lost (11 real GIFs failed so
/// on 2026-11-19).
#[test]
fn a_gif_with_data_after_its_last_frame_becomes_a_video() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("gif-xmp");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let gif = media.join("xmp.gif");
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10:duration=3"], &gif);
    // An application extension ("XMP DataXMP", one 255-byte sub-block) before the trailer.
    let mut bytes = std::fs::read(&gif).unwrap();
    assert_eq!(bytes.pop(), Some(0x3B), "a GIF ends with its trailer");
    bytes.extend_from_slice(&[0x21, 0xFF, 0x0B]);
    bytes.extend_from_slice(b"XMP DataXMP");
    bytes.push(0xFF);
    bytes.extend(std::iter::repeat_n(b' ', 255));
    bytes.extend_from_slice(&[0x00, 0x3B]);
    std::fs::write(&gif, &bytes).unwrap();

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.apply_options(opts("av1_nvenc", 1));
        engine.load_dir(media.clone()).await;
        assert_eq!(engine.snapshot().pending_files, 1);
        engine.clone().run_batch(opts("av1_nvenc", 1)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert!(!logs.contains("doesn't match the GIF"), "{logs}");
    assert_eq!(engine.snapshot().failed_this_run, 0, "{logs}");
    assert!(media.join("xmp.mp4").exists() && !gif.exists(), "{logs}");
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
    let run = |images: ImageMode| {
        let (engine, rec) = new_engine(&rt, &root, ff.clone());
        let raw = || Options { images, ..opts("av1_nvenc", 1) };
        rt.block_on(async {
            engine.apply_options(raw());
            engine.load_dir(media.clone()).await;
            let pending = engine.snapshot().pending_files;
            engine.clone().run_batch(raw()).await;
            (pending, rec.logs.lock().unwrap().join("\n"))
        })
    };
    // Images off: not smaller as a video, remembered as one.
    let (pending, logs) = run(ImageMode::Off);
    println!("{logs}");
    assert_eq!(pending, 1, "{logs}");
    assert!(logs.contains("Video was not smaller for anim.gif"), "{logs}");
    // Images on: straight to the GIF optimizer, no encode.
    let (pending, logs) = run(ImageMode::Lossless);
    println!("{logs}");
    assert_eq!(pending, 1, "{logs}");
    assert!(!logs.contains("Processing: anim.gif"), "no second video encode: {logs}");
    assert!(logs.contains("[Images]") && logs.contains("anim.gif"), "{logs}");
    // Done now.
    let (pending, logs) = run(ImageMode::Lossless);
    assert_eq!(pending, 0, "{logs}");
    // Renamed, it is known by its content for both passes: neither encoded nor optimized
    // again (it was known only as a video, so it went back to the GIF optimizer).
    std::fs::rename(&gif, media.join("renamed.gif")).unwrap();
    let (pending, logs) = run(ImageMode::Lossless);
    assert_eq!(pending, 0, "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}
