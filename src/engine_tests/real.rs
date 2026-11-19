//! Opt-in runs on real folders (ignored by default): the benchmark and a real-file run.

use super::*;

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
