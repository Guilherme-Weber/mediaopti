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

    pub fn flush_history(&self) {
        let mut inner = self.lock();
        inner.history_save_pending = false;
        if let Err(err) = inner.history.save() {
            eprintln!("Error saving processed files: {err}");
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
                .filter(|(f, s)| inner.history.has_size(**s) && !is_generated_name(f))
                .map(|(f, s)| (f, inner.want(f, *s)))
                .filter(|(f, want)| match want {
                    history::Want::Video => !inner.history.contains(f),
                    _ => !inner.history.contains_image(f, inner.lossy_for(f)),
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
                if hash.is_some_and(|h| inner.history.contains_content(&h, want)) {
                    match want {
                        history::Want::Video => {
                            inner.history.add(&file);
                        }
                        history::Want::ImageLossy => inner.history.add_image(&file, image::mode(true)),
                        history::Want::ImageLossless => inner.history.add_image(&file, image::mode(false)),
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
