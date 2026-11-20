//! The CPU workers: images, audio and documents, conversions, same names and contents.

use super::*;

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

/// A picture done at the default floor (93) is done again when a lower one (80) is chosen,
/// even moved meanwhile (recognised by its content, which knew only "lossy": the 93 pass was
/// taken for an 80 one and the picture never got the smaller level); done at 80, it is known
/// at 93 too.
#[test]
fn a_lower_quality_floor_redoes_pictures_done_at_a_higher_one() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "ssimulacra2"].iter().all(|t| tools.has(t)) {
        skip_test!("image tools not found");
        return;
    }
    let root = test_dir("floors");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let png = media.join("photo.png");
    make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=320x240", "-frames:v", "1", "-compression_level", "1"], &png);
    let rt = runtime();
    let at = |images: u32| RawOptions { images: Some(images), ..opts("av1_nvenc", 1.0) };
    let run = |images: u32| {
        let (engine, rec) = new_engine(&rt, &root, ff.clone());
        rt.block_on(async {
            engine.apply_options(at(images));
            engine.load_dir(media.clone()).await;
            let pending = engine.snapshot().pending_files;
            engine.clone().run_batch(at(images)).await;
            (pending, rec.logs.lock().unwrap().join("\n"))
        })
    };
    let (pending, logs) = run(2);
    assert_eq!(pending, 1, "{logs}");
    // Moved: only its content can say it was done.
    let moved = media.join("sub");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::rename(&png, moved.join("photo.png")).unwrap();
    let (pending, logs) = run(4);
    assert_eq!(pending, 1, "done at 93, redone at 80: {logs}");
    // The "Saved" line gives the score the lossy result was measured at.
    let score: f64 = logs
        .split("(lossy, SSIMULACRA2 ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no score in the log: {logs}"));
    assert!((80.0..100.0).contains(&score), "{score}: {logs}");
    let (pending, logs) = run(2);
    assert_eq!(pending, 0, "done at 80, known at 93: {logs}");
    // Moved again and scanned at 93, then 80 chosen before Start (no new scan): it was
    // remembered as done at 93 only, and 80 would have compressed it again.
    let again = media.join("other");
    std::fs::create_dir_all(&again).unwrap();
    std::fs::rename(moved.join("photo.png"), again.join("photo.png")).unwrap();
    let (engine, _) = new_engine(&rt, &root, ff.clone());
    engine.apply_options(at(2));
    rt.block_on(engine.load_dir(media.clone()));
    engine.apply_options(at(4));
    assert_eq!(engine.snapshot().pending_files, 0, "known for the 80 pass its content had");
    let _ = std::fs::remove_dir_all(&root);
}

/// Cover art follows the picture floor: a FLAC done at 93 is done again at 80 (its cover may
/// get smaller), one done at 80 is known at 93, and the batch log says which floor covers use.
#[test]
fn cover_art_follows_the_picture_quality_floor() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    // Everything a FLAC's lossy cover pass needs (`missing_tool`, `lossy_tools_present`).
    let jpeg_encoder = tools.has("djpeg") && (tools.has("cjpegli") || tools.has("cjpeg"));
    let lossy = (tools.has("beeim") || tools.has("pingo")) && (jpeg_encoder || tools.has("pingo"));
    if !["oxipng", "jpegtran", "flac", "ssimulacra2"].iter().all(|t| tools.has(t)) || !lossy {
        skip_test!("tools for lossy cover art not found");
        return;
    }
    let root = test_dir("cover-floor");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let art = root.join("art.png");
    make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=320x240", "-frames:v", "1", "-compression_level", "1"], &art);
    make(&ff, &["-f", "lavfi", "-i", "sine=duration=2", "-i", art.to_str().unwrap(), "-map", "0", "-map", "1", "-c:a", "flac",
        "-c:v", "png", "-disposition:v", "attached_pic"], &media.join("song.flac"));
    let rt = runtime();
    let at = |images: u32| RawOptions { images: Some(images), audio: Some(2), conversions: Some(1), ..opts("av1_nvenc", 1.0) };
    let run = |images: u32| {
        let (engine, rec) = new_engine(&rt, &root, ff.clone());
        rt.block_on(async {
            engine.apply_options(at(images));
            engine.load_dir(media.clone()).await;
            let pending = engine.snapshot().pending_files;
            engine.clone().run_batch(at(images)).await;
            (pending, rec.logs.lock().unwrap().join("\n"))
        })
    };
    let (pending, logs) = run(2);
    assert_eq!(pending, 1, "{logs}");
    // A WAV made into FLAC at 80 is remembered at FLAC's floor, not WAV's (93): it isn't
    // queued again.
    make(&ff, &["-f", "lavfi", "-i", "sine=duration=2", "-c:a", "pcm_s16le"], &media.join("tone.wav"));
    let (pending, logs) = run(4);
    assert_eq!(pending, 2, "done at 93, redone at 80, and the new WAV: {logs}");
    assert!(logs.contains("audio lossy (SSIMULACRA2 >= 80)"), "{logs}");
    assert!(media.join("tone.flac").exists(), "{logs}");
    let (pending, logs) = run(4);
    assert_eq!(pending, 0, "both known at 80: {logs}");
    let (pending, logs) = run(2);
    assert_eq!(pending, 0, "done at 80, known at 93: {logs}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A picture whose visually lossless search couldn't have its memory gets the lossless pass
/// only, remembered as lossless: a later lossy run (with the memory free) still does it, even
/// though its new content was noted before it replaced the original.
#[test]
fn a_lossless_only_picture_is_redone_lossy_later() {
    let Some(ff) = ffmpeg() else {
        skip_test!("bundled ffmpeg not found");
        return;
    };
    let tools = crate::image::tools::Tools::discover(ff.clone(), &std::env::temp_dir());
    if !["oxipng", "ssimulacra2"].iter().all(|t| tools.has(t)) {
        skip_test!("image tools not found");
        return;
    }
    let root = test_dir("lossless-only");
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let png = media.join("toobig.png");
    make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=640x480", "-frames:v", "1", "-compression_level", "1"], &png);
    let before = std::fs::metadata(&png).unwrap().len();
    crate::system::LOSSLESS_ONLY_ON.lock().unwrap().push("toobig.png".to_string());

    let rt = runtime();
    let lossy = || RawOptions { images: Some(2), ..opts("av1_nvenc", 1.0) };
    let (engine, rec) = new_engine(&rt, &root, ff.clone());
    rt.block_on(async {
        engine.apply_options(lossy());
        engine.load_dir(media.clone()).await;
        engine.clone().run_batch(lossy()).await;
    });
    crate::system::LOSSLESS_ONLY_ON.lock().unwrap().retain(|n| n != "toobig.png");
    let logs = rec.logs.lock().unwrap().join("\n");
    println!("{logs}");
    assert!(logs.contains("lossless only this time"), "{logs}");
    assert!(std::fs::metadata(&png).unwrap().len() < before, "the lossless pass made it smaller: {logs}");

    // Next run, lossy again: the picture is queued, not recognised by its content.
    let (engine2, rec2) = new_engine(&rt, &root, ff);
    engine2.apply_options(lossy());
    rt.block_on(engine2.load_dir(media));
    let logs2 = rec2.logs.lock().unwrap().join("\n");
    assert_eq!(engine2.snapshot().pending_files, 1, "{logs2}");
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
    // JPEGs, whose tools work, queued after them in the same stage (smaller files go later).
    const JPEGS: usize = 4;
    for i in 0..JPEGS {
        make(&ff, &["-f", "lavfi", "-i", &format!("color=c=0x40{:02x}60:s=32x24", i * 8), "-frames:v", "1", "-q:v", "2"], &media.join(format!("j{i}.jpg")));
    }
    // The PNG-only optimizer is "installed" but is not a program (pingo and ECT, which JPEGs
    // use too, work).
    let data = root.join("appdata");
    std::fs::create_dir_all(&data).unwrap();
    let bad = data.join("broken.exe");
    std::fs::write(&bad, b"not a program").unwrap();
    let tools: std::collections::HashMap<&str, &Path> = [("oxipng", bad.as_path())].into_iter().collect();
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
    // No PNG was remembered: all of them are still to do. Only the PNGs were left: the
    // JPEGs, whose tools work, were done (and remembered).
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
    // wave.gif: nothing has its new name -> wave.mp4.
    make(&ff, &["-f", "lavfi", "-i", "testsrc2=s=320x240:r=10:d=3"], &media.join("wave.gif"));
    // linked: same picture, but the existing PNG has a second name elsewhere (a hard link):
    // it is never replaced (the link would break), so both stay.
    pic("testsrc=s=320x240", &[], "linked.bmp");
    pic("testsrc=s=320x240", &["-compression_level", "0"], "linked.png");
    let elsewhere = root.join("elsewhere.png");
    std::fs::hard_link(media.join("linked.png"), &elsewhere).unwrap();
    let linked_before = std::fs::read(&elsewhere).unwrap();

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
    let want = ["anim.mp4", "linked.bmp", "linked.png", "other (2).png", "other.png", "photo.jpg", "sky.png", "wave.mp4", "web.webp"];
    assert_eq!(names, want.iter().map(ToString::to_string).collect(), "{logs}");
    assert_eq!(std::fs::read(media.join("linked.png")).unwrap(), linked_before, "the hard-linked copy keeps its bytes");
    assert_eq!(crate::system::hard_link_count(&elsewhere).ok(), Some(2), "the link is intact");
    let backup = root.join("media (originals)");
    assert!(backup.join("sky.bmp").exists() && backup.join("other.bmp").exists() && backup.join("anim.gif").exists(), "{logs}");
    // The queue rebuilt after the batch lists what is on the disk now: nothing converted,
    // renamed or removed as a duplicate comes back as waiting (a GIF made into a video used
    // to show as "1 ready"). web.webp is not a type the scan takes.
    let s = engine.snapshot();
    // linked.png itself is a hard link: skipped, so still waiting.
    assert_eq!(s.pending_files, 1, "{:?}", engine.queue_preview(10));
    assert_eq!(s.total_files, want.len() - 1, "{logs}");
    let _ = std::fs::remove_dir_all(&root);
}
