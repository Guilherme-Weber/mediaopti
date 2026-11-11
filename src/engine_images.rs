//! CPU image workers (PNG, JPEG, small GIFs), running next to the GPU video workers.
//!
//! Each image is optimized on a private copy (ASCII name, in the temp folder), and the
//! original is replaced only when the verified result is at least 0.5% smaller and the
//! original did not change meanwhile. Hard-linked files are left alone. Replacement,
//! Recycle Bin, dates and attributes work exactly as for videos.

use super::{file_state, Engine, Inner};
use crate::history::{self, ContentHash};
use crate::image::{self, tools::RunError};
use crate::{args, system};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Log a progress line every this many images (one line per image would flood the log).
const PROGRESS_EVERY: usize = 250;

pub(super) enum ImageResult {
    Smaller {
        before: u64,
        after: u64,
        /// A lossy encoder won (quality search labels look like `beeim-quantizer=81`).
        lossy: bool,
        content: Option<(ContentHash, u64)>,
    },
    /// Nothing smaller (or not a valid image): remembered so it is not retried.
    Kept {
        content: Option<(ContentHash, u64)>,
        note: Option<String>,
    },
    /// Left alone and not remembered (hard link, changed meanwhile).
    Skipped(String),
    Failed(String),
    Stopped,
}

impl Inner {
    fn claim_image(&mut self) -> Option<PathBuf> {
        while self.image_cursor < self.image_queue.len() {
            let candidate = self.image_queue[self.image_cursor].clone();
            self.image_cursor += 1;
            if !self.in_progress.contains(&candidate) && !self.failed.contains(&candidate) {
                self.in_progress.insert(candidate.clone());
                return Some(candidate);
            }
        }
        None
    }
}

impl Engine {
    pub(super) async fn run_image_worker(self: Arc<Self>, worker_id: u32, ctx: Arc<image::Ctx>) {
        loop {
            let cancel = self.lock().cancel.clone();
            if !self.wait_while_paused(&cancel).await {
                break;
            }
            let (path, lossy, disposal) = {
                let mut inner = self.lock();
                if !inner.working || inner.stop_requested {
                    break;
                }
                let Some(path) = inner.claim_image() else { break };
                inner.images_active += 1;
                let disposal = inner.disposal_for(&path);
                (path, inner.images_lossy(), disposal)
            };
            self.send_state(false);
            let (ctx2, p2) = (Arc::clone(&ctx), path.clone());
            let result = self.blocking(move || process_image(&ctx2, &p2, worker_id, &disposal)).await;
            self.finish_image(&path, lossy, result);
        }
    }

    fn finish_image(self: &Arc<Self>, path: &Path, lossy: bool, result: ImageResult) {
        let mode = image::mode(lossy);
        let mut remembered = false;
        let mut log = None;
        let progress = {
            let mut inner = self.lock();
            inner.images_active = inner.images_active.saturating_sub(1);
            inner.in_progress.remove(path);
            let display = inner.display_name(path);
            if !matches!(result, ImageResult::Stopped) {
                inner.run.completed += 1;
                inner.run.images_done += 1;
            }
            let mut remember = |inner: &mut Inner, content: Option<(ContentHash, u64)>| {
                inner.history.add_image(path, mode);
                if let Some((hash, size)) = content {
                    inner.history.add_content(hash, size, mode);
                }
                remembered = true;
            };
            match result {
                ImageResult::Smaller { before, after, lossy, content } => {
                    inner.run.compressed += 1;
                    inner.run.images_lossy += usize::from(lossy);
                    inner.space_saved_mb += (before - after) as f64 / (1024.0 * 1024.0);
                    remember(&mut inner, content);
                }
                ImageResult::Kept { content, note } => {
                    inner.run.skipped_larger += 1;
                    remember(&mut inner, content);
                    if let Some(note) = note {
                        log = Some(format!("[Images] Skipped {display}: {note}"));
                    }
                }
                ImageResult::Skipped(why) => {
                    inner.run.skipped_larger += 1;
                    log = Some(format!("[Images] Skipped {display}: {why}"));
                }
                ImageResult::Failed(err) => {
                    inner.run.failed += 1;
                    inner.failed.insert(path.to_path_buf());
                    log = Some(format!("[Images] Failed on {display}: {err}"));
                }
                ImageResult::Stopped => {}
            }
            let r = &inner.run;
            (r.images_done.is_multiple_of(PROGRESS_EVERY) && r.images_done > 0)
                .then(|| format!("[Images] {}/{} done, {:.0} MB saved so far", r.images_done, r.images_total, inner.space_saved_mb))
        };
        if remembered {
            self.schedule_history_save();
        }
        for line in log.into_iter().chain(progress) {
            self.log(line);
        }
        self.send_state(false);
    }
}

/// Deletes the private work folder however `process_image` returns.
struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn content_of(path: &Path) -> Option<(ContentHash, u64)> {
    let size = std::fs::metadata(path).ok()?.len();
    Some((history::hash_file(path).ok()?, size))
}

/// Optimizes one image and, if worthwhile, replaces the original. Blocking.
pub(super) fn process_image(ctx: &image::Ctx, path: &Path, worker_id: u32, disposal: &system::Disposal) -> ImageResult {
    static NEXT_JOB: AtomicU64 = AtomicU64::new(0);
    let Some(kind) = image::kind_of(path) else {
        return ImageResult::Skipped("not an image".into());
    };
    let Some(state) = file_state(path) else {
        return ImageResult::Failed("could not read the file".into());
    };
    if system::hard_link_count(path).unwrap_or(1) > 1 {
        return ImageResult::Skipped("it is a hard link (several names for one file)".into());
    }
    let job = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let work = std::env::temp_dir().join(format!("mediaopti-img-{}-{job}", std::process::id()));
    let _cleanup = WorkDir(work.clone());
    let ext = args::ext_lower(path);
    // ASCII name: some tools can't open Unicode paths.
    let src = work.join(format!("orig.{ext}"));
    if let Err(err) = std::fs::create_dir_all(&work).and_then(|()| std::fs::copy(path, &src).map(drop)) {
        return ImageResult::Failed(format!("could not copy it to the temp folder: {err}"));
    }
    let before = state.0;

    let result = match image::optimize(ctx, kind, &src, &work) {
        Ok(r) => r,
        Err(RunError::Cancelled) => return ImageResult::Stopped,
        Err(RunError::Failed(err)) => return ImageResult::Failed(err),
    };
    let Some(best) = result.best else {
        return ImageResult::Kept { content: content_of(path), note: result.note };
    };
    let after = std::fs::metadata(&best).map_or(before, |m| m.len());
    if before.saturating_sub(after) < (before / 200).max(1) {
        return ImageResult::Kept { content: content_of(path), note: None };
    }
    if file_state(path) != Some(state) {
        return ImageResult::Skipped("it changed while it was being optimized (it will be processed next time)".into());
    }

    // The new version goes next to the original first (same volume), then replaces it.
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let sibling = path.with_file_name(format!("{stem}.mediaopti.i{worker_id}.{ext}"));
    let attrs = match std::fs::metadata(path) {
        Ok(m) => system::OriginalAttributes::from_metadata(&m),
        Err(err) => return ImageResult::Failed(err.to_string()),
    };
    if let Err(err) = std::fs::copy(&best, &sibling) {
        let _ = system::remove_file_if_exists(&sibling);
        return ImageResult::Failed(format!("could not write next to the original: {err}"));
    }
    if let Err(err) = super::replace_original(&sibling, path, path, disposal) {
        return ImageResult::Failed(err);
    }
    let _ = system::restore_attributes(path, &attrs);
    let lossy = result.steps.iter().any(|s| s.contains('='));
    ImageResult::Smaller { before, after, lossy, content: content_of(path) }
}
