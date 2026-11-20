//! The engine's side of the history: recording processed files, recognising known content, saving.

use super::*;

/// Content-hashes files on a few threads (disk-bound; more threads would only seek more).
pub(super) fn hash_files_parallel(files: &[PathBuf]) -> Vec<(PathBuf, Option<ContentHash>)> {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|f| (f.clone(), history::hash_file(f).ok()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

impl Engine {
    /// Saves history after a short delay so bursts of completions cause one write.
    pub(super) fn schedule_history_save(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.history_save_pending {
                return;
            }
            inner.history_save_pending = true;
        }
        let engine = Arc::clone(self);
        self.rt.spawn(async move {
            tokio::time::sleep(HISTORY_SAVE_DELAY).await;
            engine.flush_history();
        });
    }

    /// Writes what the history gained since the last save. The engine's lock is held only to
    /// take it; the disk writes happen outside (after a clear, a library's whole history is
    /// rewritten). `SAVING` keeps saves in the order they were taken.
    pub fn flush_history(&self) {
        static SAVING: Mutex<()> = Mutex::new(());
        let _in_order = SAVING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let job = {
            let mut inner = self.lock();
            inner.history_save_pending = false;
            inner.history.take_save()
        };
        if let Some(job) = job {
            if let Err(err) = job.write() {
                self.log(format!("Could not save the processed history (tried again at the next save): {err}"));
                self.lock().history.give_back(job);
            }
        }
    }

    pub(super) fn add_processed(self: &Arc<Self>, file: &Path) {
        let added = {
            let mut inner = self.lock();
            let added = inner.history.add(file);
            if added {
                inner.skipped_known += 1;
            }
            added
        };
        if added {
            self.schedule_history_save();
        }
    }

    /// Remembers a finished file: `file` by location, `content_of` (the file now on disk)
    /// by content hash, so it is still recognised after being moved or renamed.
    /// Records `file`'s content in the history on the disk now (before it replaces an
    /// original: see `replace_original`).
    pub(super) fn note_content_now(&self, file: &Path, mode: &'static str) -> std::io::Result<()> {
        let size = std::fs::metadata(file)?.len();
        let hash = history::hash_file(file)?;
        let line = self.lock().history.add_content_now(hash, size, mode);
        match line {
            Some((path, line)) => history::append_lines(&path, &line, true),
            None => Ok(()),
        }
    }

    pub(super) async fn remember(self: &Arc<Self>, file: &Path, content_of: &Path) {
        self.add_processed(file);
        let path = content_of.to_path_buf();
        let hashed = self
            .blocking(move || -> std::io::Result<(ContentHash, u64)> {
                let size = std::fs::metadata(&path)?.len();
                Ok((history::hash_file(&path)?, size))
            })
            .await;
        if let Ok((hash, size)) = hashed {
            self.lock().history.add_content(hash, size, history::VIDEO_MODE);
            self.schedule_history_save();
        }
    }

    /// Finds files that are new to the location history but whose content was processed
    /// before (moved, renamed or copied), and adds them to the location history.
    /// Only files whose size matches a remembered size are hashed.
    pub(super) async fn recognise_by_content(self: &Arc<Self>, files: &[PathBuf], sizes: &[u64]) -> usize {
        let (candidates, wants): (Vec<PathBuf>, Vec<history::Want>) = {
            let inner = self.lock();
            files
                .iter()
                .zip(sizes)
                .filter(|(f, _)| !is_generated_name(f))
                .map(|(f, s)| {
                    let want = match inner.want(f, *s) {
                        // A big GIF known here as a video lacks only its GIF pass (`place` sends
                        // it to the optimizer): that is what its content may already have.
                        history::Want::Video if args::is_gif(f) && inner.history.contains(f) => {
                            let strip = inner.redo_stripped();
                            if inner.lossy_for(f) { history::Want::ImageLossy { strip, target: inner.target_for(f) } } else { history::Want::ImageLossless { strip } }
                        }
                        w => w,
                    };
                    (f, *s, want)
                })
                // Only a size that had the wanted pass can match. Types switched off are
                // checked too: switched on before Start, their moved files are known.
                .filter(|(_, s, want)| inner.history.has_size(*s, *want))
                .map(|(f, _, want)| (f, want))
                .filter(|(f, want)| match want {
                    history::Want::Video => !inner.history.contains(f),
                    _ => !inner.history.contains_image(f, inner.lossy_for(f), inner.redo_stripped(), inner.target_for(f)),
                })
                .map(|(f, want)| (f.clone(), want))
                .unzip()
        };
        if candidates.is_empty() {
            return 0;
        }
        self.log(format!("Checking {} files by content (same size as a processed one)", candidates.len()));
        let hashed = self.blocking(move || hash_files_parallel(&candidates)).await;
        let hits = {
            let mut inner = self.lock();
            let mut hits = 0;
            for ((file, hash), want) in hashed.into_iter().zip(wants) {
                if let Some(h) = hash.filter(|h| inner.history.contains_content(h, want)) {
                    match want {
                        history::Want::Video => {
                            inner.history.add(&file);
                            // A GIF that went through the video attempt and the GIF optimizer
                            // (moved or renamed since): known for its GIF pass too, else
                            // `place` sends it to the optimizer again.
                            if args::is_gif(&file) {
                                let (lossy, strip, target) = (inner.lossy_for(&file), inner.redo_stripped(), inner.target_for(&file));
                                let as_image = if lossy { history::Want::ImageLossy { strip, target } } else { history::Want::ImageLossless { strip } };
                                if inner.history.contains_content(&h, as_image) {
                                    let mode = inner.history.content_image_mode(&h).unwrap_or(image::mode_at(lossy, strip, target));
                                    inner.history.add_image(&file, mode);
                                }
                            }
                        }
                        // Remembered for the strongest pass its content had (covering the
                        // one asked for: `covered_by`), not just the one asked at this scan.
                        history::Want::ImageLossy { strip, target } => {
                            let mode = inner.history.content_image_mode(&h).unwrap_or(image::mode_at(true, strip, target));
                            inner.history.add_image(&file, mode);
                        }
                        history::Want::ImageLossless { strip } => {
                            let mode = inner.history.content_image_mode(&h).unwrap_or(image::mode(false, strip));
                            inner.history.add_image(&file, mode);
                        }
                    }
                    hits += 1;
                }
            }
            hits
        };
        if hits > 0 {
            self.schedule_history_save();
        }
        hits
    }

    pub fn clear_history(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.working {
                drop(inner);
                self.log("Stop processing before clearing processed history");
                return;
            }
            inner.history.clear();
            inner.rebuild_queue();
            inner.status = if inner.dir.is_some() {
                Status::Ready
            } else {
                Status::Idle
            };
        }
        self.flush_history();
        self.log("Processed history cleared");
        self.send_state(true);
    }
}
