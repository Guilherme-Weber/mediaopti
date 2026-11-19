//! Video jobs: problem files, streams and tags kept, unfinished probes, the quality retry.

use super::*;

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
    assert!(logs.contains("Failed on dup.mp4: corrupt or unreadable, ffmpeg can't decode it ("), "{logs}");
    assert!(!logs.contains("(no details)"), "the reason is in ffmpeg's words: {logs}");
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

/// A stream probe that doesn't finish (out of time, ffmpeg couldn't start, or a Pause) says
/// nothing about the file: the original stays, the file fails this run only (not
/// remembered), and an input probe never lets the encode go on without knowing the tracks.
#[test]
fn unfinished_probes_never_decide() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("probe-fails");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let srt = root.join("s.srt");
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:02,000\nhello\n").unwrap();
    let two_tracks = |out: &Path| {
        make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=3", "-f", "lavfi", "-i", "sine=f=440:d=3",
            "-f", "lavfi", "-i", "sine=f=880:d=3", "-i", srt.to_str().unwrap(),
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s", "-c:v", "libx264", "-qp", "0", "-c:a", "aac", "-c:s", "srt"], out);
    };
    // infail: its own probe fails; outfail: the probe of its new version fails.
    let (infail, outfail) = (media.join("infail.mkv"), media.join("outfail.mkv"));
    two_tracks(&infail);
    two_tracks(&outfail);
    let sizes = [std::fs::metadata(&infail).unwrap().len(), std::fs::metadata(&outfail).unwrap().len()];
    crate::system::PROBE_FAILS_ON.lock().unwrap().extend(["infail.".to_string(), "outfail.mediaopti.".to_string()]);

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.apply_options(opts("libx264", 2.0));
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(opts("libx264", 2.0)).await;
    });
    crate::system::PROBE_FAILS_ON.lock().unwrap().retain(|n| n != "infail." && n != "outfail.mediaopti.");
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    let s = engine.snapshot();
    assert_eq!((s.failed_this_run, s.compressed_this_run), (2, 0), "{logs}");
    assert_eq!([std::fs::metadata(&infail).unwrap().len(), std::fs::metadata(&outfail).unwrap().len()], sizes, "originals untouched: {logs}");
    let leftovers: Vec<_> = std::fs::read_dir(&media).unwrap().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    // Not remembered: a new run tries both again.
    let (engine2, _) = new_engine(&rt, &root, ff);
    engine2.apply_options(opts("libx264", 2.0));
    rt.block_on(engine2.load_dir(media));
    assert_eq!(engine2.snapshot().pending_files, 2, "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The same video under the new name, ours smaller: if the rename fails after that copy was
/// set aside, the new version stays (as its temp, named at the next scan), never deleted.
#[test]
fn a_new_version_kept_after_a_failed_rename_survives() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("kept-temp");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    // Unique names: the rename hook is shared by the tests running at once.
    let (gif, mp4) = (media.join("kepttemp.gif"), media.join("kepttemp.mp4"));
    make(&ff, &["-f", "lavfi", "-i", "testsrc=s=320x240:r=10:d=3", "-vf", "split[a][b];[a]palettegen[p];[b][p]paletteuse"], &gif);
    make(&ff, &["-i", gif.to_str().unwrap(), "-fps_mode", "passthrough", "-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv444p"], &mp4);
    crate::system::RENAME_FAILS_ON.lock().unwrap().push("kepttemp.mp4".to_string());

    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    let raw = || RawOptions { originals: Some(2), ..opts("av1_nvenc", 1.0) };
    rt.block_on(async {
        engine.apply_options(raw());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(raw()).await;
    });
    crate::system::RENAME_FAILS_ON.lock().unwrap().retain(|n| n != "kepttemp.mp4");
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert_eq!(engine.snapshot().failed_this_run, 1, "{logs}");
    assert!(root.join("media (originals)").join("kepttemp.mp4").exists(), "the bigger copy was set aside: {logs}");
    assert!(gif.exists(), "the GIF stays until its video has the name: {logs}");
    let kept: Vec<_> = std::fs::read_dir(&media).unwrap().filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".mediaopti.")).collect();
    assert_eq!(kept.len(), 1, "the new version is kept as its temp: {logs}");
    assert!(logs.contains("Skipped kepttemp.mp4: it is no longer there"), "{logs}");
    assert_eq!(engine.snapshot().pending_files, 0, "{:?}", engine.queue_preview(5));

    // The next scan gives it the name.
    let (engine2, rec2) = new_engine(&rt, &root, ff);
    engine2.apply_options(raw());
    rt.block_on(engine2.load_dir(media));
    let logs2 = rec2.logs.lock().unwrap().join("\n");
    assert!(mp4.exists(), "{logs2}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A file with two cameras (dashcam front + rear) is left as it is: only the first video
/// track was encoded, and the second was lost without the check noticing.
#[test]
fn a_second_video_track_is_never_dropped() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("two-cameras");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let file = media.join("dashcam.mkv");
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=s=640x360:d=2", "-f", "lavfi", "-i", "testsrc=s=320x240:d=2",
        "-map", "0", "-map", "1", "-c:v", "libx264", "-qp", "0"], &file);
    let before = std::fs::read(&file).unwrap();
    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff);
    rt.block_on(async {
        engine.apply_options(opts("av1_nvenc", 1.0));
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(opts("av1_nvenc", 1.0)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert!(logs.contains("Kept dashcam.mkv: it holds more than one video track"), "{logs}");
    assert!(!logs.contains("Processing: dashcam.mkv"), "never encoded: {logs}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
    let _ = std::fs::remove_dir_all(&root);
}

/// A camera MOV with PCM audio and a timecode track becomes an MKV (with FLAC): MKV has no
/// timecode tracks but keeps the value as a TIMECODE tag. That counted as "lost its
/// timecode track", a failure remembered after the whole encode.
#[test]
fn a_camera_timecode_survives_as_an_mkv_tag() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let root = test_dir("timecode-mkv");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=2,noise=alls=30:allf=t", "-f", "lavfi", "-i", "sine=d=2",
        "-c:v", "libx264", "-qp", "0", "-c:a", "pcm_s16le", "-timecode", "01:00:00:00"], &media.join("C0001.MOV"));
    let rt = runtime();
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.apply_options(opts("av1_nvenc", 1.0));
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(opts("av1_nvenc", 1.0)).await;
    });
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert!(!logs.contains("timecode track"), "{logs}");
    assert_eq!(engine.snapshot().failed_this_run, 0, "{logs}");
    let out = media.join("C0001.mkv");
    assert!(out.exists(), "{logs}");
    let listing = Command::new(&ff).args(["-hide_banner", "-i"]).arg(&out).output().unwrap();
    let after = crate::args::Streams::parse(&String::from_utf8_lossy(&listing.stderr));
    assert_eq!(after.timecode.as_deref(), Some("01:00:00:00"));
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
    // The measurement Pause ended was waited for: its encode could be parked and removed.
    assert!(!logs.contains("Could not remove temporary file"), "{logs}");
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
