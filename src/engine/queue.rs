//! The queue: where each file goes (video or CPU stage), claiming the next one, the UI snapshot.

use super::*;

/// The CPU stage of a queued file (see `Kind::stage`).
pub(super) fn cpu_stage(file: &Path) -> usize {
    image::kind_of(file).map_or(2, image::Kind::stage)
}

/// Where a scanned file goes (see `Inner::place`).
pub(super) enum Place {
    Generated,
    /// Its category is switched off (`gif`: an animated GIF kept as a GIF).
    Off { gif: bool },
    Known,
    Failed,
    Cpu,
    Video,
}

impl Inner {
    /// Whether `file` (of `size` bytes) is handled by the image workers: PNG/JPEG, still
    /// GIFs, and animated GIFs under the size limit (bigger ones become video).
    pub(super) fn is_image_job(&self, file: &Path, size: u64) -> bool {
        match image::kind_of(file) {
            // With videos switched off, big animated GIFs are optimized as GIFs.
            Some(image::Kind::Gif) => size < self.options.gif_min_bytes || self.static_gifs.contains(file) || !self.options.videos,
            Some(_) => true,
            None => false,
        }
    }

    /// Whether the settings let the CPU workers take this file: conversions (WAV) follow the
    /// conversions option, everything else (images, cover art) the image option.
    pub(super) fn cpu_job_enabled(&self, file: &Path) -> bool {
        let Some(kind) = image::kind_of(file) else { return false };
        let on = match kind.stage() {
            0 => self.options.audio != ImageMode::Off,
            1 => self.options.docs,
            _ => self.options.images != ImageMode::Off,
        };
        let c = self.options.convert;
        let conversion_on = match kind {
            image::Kind::Wav | image::Kind::LosslessAudio => c.wav,
            image::Kind::Bmp | image::Kind::Tga => c.images,
            image::Kind::Cbr => c.cbr,
            _ => true,
        };
        on && conversion_on
    }

    /// Whether `file` is processed visually lossless (its category's setting: audio files by
    /// the audio setting, the rest by the image setting).
    pub(super) fn lossy_for(&self, file: &Path) -> bool {
        let audio = image::kind_of(file).is_some_and(image::Kind::is_audio);
        (if audio { self.options.audio } else { self.options.images }) == ImageMode::Lossy
    }

    /// Content-history request for a file under the current settings.
    pub(super) fn want(&self, file: &Path, size: u64) -> history::Want {
        if !self.is_image_job(file, size) {
            history::Want::Video
        } else if self.lossy_for(file) {
            history::Want::ImageLossy
        } else {
            history::Want::ImageLossless
        }
    }

    pub(super) fn images_lossy(&self) -> bool {
        self.options.images == ImageMode::Lossy
    }

    /// Root of the backup folder for replaced originals, if that option is chosen.
    pub(super) fn backup_root(&self) -> Option<PathBuf> {
        let Originals::BackupFolder(chosen) = &self.options.originals else { return None };
        chosen.clone().or_else(|| self.dir.as_deref().map(default_backup_root))
    }

    /// What happens to `original` when it is replaced. Backups mirror the scanned folder's
    /// structure, so originals can be found (and restored) where they belong.
    pub(super) fn disposal_for(&self, original: &Path) -> system::Disposal {
        match &self.options.originals {
            Originals::Delete => system::Disposal::Delete,
            Originals::RecycleBin => system::Disposal::RecycleBin,
            Originals::BackupFolder(_) => {
                let root = self.backup_root().unwrap_or_else(|| default_backup_root(original));
                let rel = self
                    .dir
                    .as_deref()
                    .and_then(|d| original.strip_prefix(d).ok())
                    .map_or_else(|| PathBuf::from(original.file_name().unwrap_or_default()), Path::to_path_buf);
                system::Disposal::MoveTo(root.join(rel))
            }
        }
    }

    /// Where a scanned file goes under the current settings and history.
    pub(super) fn place(&self, file: &Path, size: u64) -> Place {
        if is_generated_name(file) {
            Place::Generated
        } else if self.is_image_job(file, size) {
            if !self.cpu_job_enabled(file) {
                // Not remembered: they may be optimized once switched on.
                Place::Off { gif: args::is_gif(file) }
            } else if self.history.contains_image(file, self.lossy_for(file)) {
                Place::Known
            } else if self.failed.contains(file) {
                Place::Failed
            } else {
                Place::Cpu
            }
        } else if !self.options.videos {
            Place::Off { gif: false }
        } else if self.history.contains(file) {
            // A GIF still here was not replaced by a video. If it never went through the GIF
            // optimizer (images were off, or older versions), it goes there now, without
            // being encoded again.
            if args::is_gif(file)
                && self.cpu_job_enabled(file)
                && !self.history.contains_image(file, self.lossy_for(file))
                && !self.failed.contains(file)
            {
                Place::Cpu
            } else {
                Place::Known
            }
        } else if self.failed.contains(file) {
            Place::Failed
        } else {
            Place::Video
        }
    }

    /// Rebuilds the work queues from the scanned files, history and this run's failures.
    pub(super) fn rebuild_queue(&mut self) {
        let (mut queue, mut cpu): (Vec<PathBuf>, [Vec<PathBuf>; 3]) = (Vec::new(), Default::default());
        let mut bytes = [0u64; 4];
        let (mut known, mut generated, mut small_gifs) = (0, 0, 0);
        for (i, file) in self.files.iter().enumerate() {
            let size = self.file_sizes.get(i).copied().unwrap_or(u64::MAX);
            match self.place(file, size) {
                Place::Generated => generated += 1,
                Place::Off { gif } => small_gifs += usize::from(gif),
                Place::Known => known += 1,
                Place::Failed => {}
                Place::Cpu => {
                    let stage = cpu_stage(file);
                    bytes[stage + 1] += size;
                    cpu[stage].push(file.clone());
                }
                Place::Video => {
                    bytes[0] += size;
                    queue.push(file.clone());
                }
            }
        }
        self.pending_queue = queue;
        self.queue_cursor = 0;
        self.cpu_queues = cpu;
        self.cpu_cursors = [0; 3];
        self.queue_bytes = bytes;
        self.queue_revision += 1;
        self.skipped_known = known;
        self.skipped_generated = generated;
        self.skipped_small_gifs = small_gifs;
    }

    pub(super) fn remaining_pending(&self) -> usize {
        self.pending_queue.len().saturating_sub(self.queue_cursor) + self.cpu_left() + self.in_progress.len()
    }

    /// CPU files queued in all stages.
    pub(super) fn cpu_total(&self) -> usize {
        self.cpu_queues.iter().map(Vec::len).sum()
    }

    /// CPU files not started yet.
    pub(super) fn cpu_left(&self) -> usize {
        self.cpu_queues.iter().zip(self.cpu_cursors).map(|(q, c)| q.len().saturating_sub(c)).sum()
    }

    /// A video whose job was ended by a Pause (to free memory): next in line again.
    pub(super) fn requeue_video(&mut self, file: &Path) {
        self.in_progress.remove(file);
        self.pending_queue.insert(self.queue_cursor, file.to_path_buf());
    }

    /// A CPU file whose job was ended by a Pause: next in line in its stage again.
    pub(super) fn requeue_cpu(&mut self, file: &Path) {
        let stage = cpu_stage(file);
        self.in_progress.remove(file);
        self.cpu_queues[stage].insert(self.cpu_cursors[stage], file.to_path_buf());
    }

    /// Adds a CPU file to its stage's queue (and the byte counts).
    pub(super) fn queue_cpu(&mut self, file: PathBuf, size: u64) {
        let stage = cpu_stage(&file);
        self.queue_bytes[stage + 1] += size;
        self.cpu_queues[stage].push(file);
        self.queue_revision += 1;
    }

    pub(super) fn display_name(&self, path: &Path) -> String {
        if let Some(dir) = &self.dir {
            if let Ok(rel) = path.strip_prefix(dir) {
                let rel = rel.to_string_lossy();
                if !rel.is_empty() {
                    return rel.into_owned();
                }
            }
        }
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub(super) fn claim_next(&mut self) -> Option<PathBuf> {
        while self.queue_cursor < self.pending_queue.len() {
            let candidate = self.pending_queue[self.queue_cursor].clone();
            self.queue_cursor += 1;
            if !self.in_progress.contains(&candidate)
                && !self.failed.contains(&candidate)
                && !self.history.contains(&candidate)
            {
                self.in_progress.insert(candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    pub(super) fn reset_run_tracking(&mut self) {
        self.failed.clear();
        self.in_progress.clear();
        self.cpu_started.clear();
        self.cpu_progress.clear();
        self.cpu_waiting_memory = false;
        self.workers.clear();
        self.worker_files.clear();
        self.space_saved_mb = 0.0;
    }

    pub(super) fn snapshot(&self) -> StateDto {
        let mut total_fps = 0.0;
        let mut total_speed = 0.0;
        let mut progress_sum = 0.0;
        // While paused, the last parsed fps/speed would be stale.
        let live = self.status != Status::Paused;
        let workers = self
            .workers
            .values()
            .map(|w| {
                let (fps, speed) = if live { (w.fps, w.speed) } else { (0.0, 0.0) };
                total_fps += fps;
                total_speed += speed;
                let pct = w.percent.clamp(0.0, 100.0);
                progress_sum += pct / 100.0;
                WorkerDto {
                    file: w.file.clone(),
                    fps: fps.round() as i64,
                    speed: round1(speed),
                    percent: pct.round() as i64,
                    step: w.step.clone(),
                    secs: w.started.elapsed().as_secs(),
                }
            })
            .collect();

        let mut started: Vec<(&PathBuf, &Instant)> = self.cpu_started.iter().collect();
        started.sort_by_key(|(_, t)| **t);
        let cpu_jobs = started
            .into_iter()
            .map(|(p, t)| {
                let progress = self.cpu_progress.get(p);
                CpuJob {
                    name: self.display_name(p),
                    secs: t.elapsed().as_secs(),
                    parts: progress.and_then(|pr| pr.get()),
                    phase: progress.and_then(|pr| pr.phase()),
                }
            })
            .collect();
        let r = &self.run;
        let videos_running = self.workers.len();
        let type_progress = [
            (r.completed.saturating_sub(r.images_done), r.total.saturating_sub(r.images_total), videos_running),
            (r.stage_done[0], r.stage_total[0], self.cpu_active[0]),
            (r.stage_done[1], r.stage_total[1], self.cpu_active[1]),
            (r.stage_done[2], r.stage_total[2], self.cpu_active[2]),
        ];

        StateDto {
            status: self.status,
            dir: self.dir.as_ref().map(|d| d.to_string_lossy().into_owned()),
            total_files: self.files.len(),
            pending_files: self.remaining_pending(),
            skipped_known: self.skipped_known,
            skipped_generated: self.skipped_generated,
            skipped_small_gifs: self.skipped_small_gifs,
            history_count: self.history.len(),
            run_total: self.run.total,
            completed_this_run: self.run.completed,
            compressed_this_run: self.run.compressed,
            skipped_larger_this_run: self.run.skipped_larger,
            failed_this_run: self.run.failed,
            space_saved_mb: round1(self.space_saved_mb),
            queue_counts: [
                (self.pending_queue.len().saturating_sub(self.queue_cursor), self.queue_bytes[0]),
                (self.cpu_queues[0].len().saturating_sub(self.cpu_cursors[0]), self.queue_bytes[1]),
                (self.cpu_queues[1].len().saturating_sub(self.cpu_cursors[1]), self.queue_bytes[2]),
                (self.cpu_queues[2].len().saturating_sub(self.cpu_cursors[2]), self.queue_bytes[3]),
            ],
            queue_revision: self.queue_revision,
            cpu_jobs,
            type_progress,
            cpu_waiting_memory: self.cpu_waiting_memory,
            scan_found: 0,
            telemetry: TelemetryDto {
                total_fps: total_fps.round() as i64,
                total_speed: round1(total_speed),
                active_progress_sum: progress_sum,
                nvenc_util: self.gpu_enc,
                vram_gb: round1(self.gpu_vram_mb / 1024.0),
                workers,
            },
        }
    }
}

impl Engine {
    /// Empties the queue (nothing is deleted on disk). Not during a run.
    pub fn clear_queue(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.working || inner.status == Status::Scanning {
                drop(inner);
                self.log("Stop processing before clearing the queue");
                return;
            }
            inner.dir = None;
            inner.files.clear();
            inner.file_sizes.clear();
            inner.static_gifs.clear();
            inner.pending_queue.clear();
            inner.queue_cursor = 0;
            inner.cpu_queues = Default::default();
            inner.cpu_cursors = [0; 3];
            inner.queue_bytes = [0; 4];
            inner.queue_revision += 1;
            inner.reset_run_tracking();
            inner.run = RunStats::default();
            inner.skipped_known = 0;
            inner.skipped_generated = 0;
            inner.skipped_small_gifs = 0;
            inner.status = Status::Idle;
        }
        self.log("Queue cleared");
        self.send_state(true);
    }

    /// The first `limit` files waiting, in processing order: (name, type, size).
    pub fn queue_preview(&self, limit: usize) -> Vec<(String, &'static str, u64)> {
        let inner = self.lock();
        let mut picked: Vec<(&PathBuf, &'static str)> = Vec::new();
        let videos = inner.pending_queue.iter().skip(inner.queue_cursor).map(|p| (p, "Video"));
        let cpu = ["Audio", "Document", "Image"]
            .iter()
            .enumerate()
            .flat_map(|(i, label)| inner.cpu_queues[i].iter().skip(inner.cpu_cursors[i]).map(move |p| (p, *label)));
        picked.extend(videos.chain(cpu).take(limit));
        let wanted: HashSet<&PathBuf> = picked.iter().map(|(p, _)| *p).collect();
        let sizes: std::collections::HashMap<&PathBuf, u64> =
            inner.files.iter().zip(&inner.file_sizes).filter(|(f, _)| wanted.contains(f)).map(|(f, s)| (f, *s)).collect();
        picked.into_iter().map(|(p, kind)| (inner.display_name(p), kind, sizes.get(p).copied().unwrap_or(0))).collect()
    }
}
