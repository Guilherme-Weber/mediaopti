//! Loading folders and dropped files into the queue: scanning, readability checks, file state.

use super::*;

/// The deepest folder holding all `paths` (a file counts as its folder). Paths on different
/// drives have none: the first one's folder is used.
pub(super) fn common_root(paths: &[PathBuf]) -> PathBuf {
    let folder = |p: &PathBuf| if p.is_dir() { p.clone() } else { p.parent().map_or_else(|| p.clone(), Path::to_path_buf) };
    let mut iter = paths.iter().map(folder);
    let Some(mut root) = iter.next() else { return PathBuf::new() };
    for p in iter {
        while !p.starts_with(&root) {
            match root.parent() {
                Some(up) => root = up.to_path_buf(),
                None => return paths.first().map(folder).unwrap_or_default(),
            }
        }
    }
    root
}

/// Default backup folder: next to the scanned folder, named `<folder> (originals)`.
/// For a drive root (no parent), a `mediaopti originals` folder inside it.
pub(super) fn default_backup_root(scanned: &Path) -> PathBuf {
    match (scanned.parent(), scanned.file_name()) {
        (Some(parent), Some(name)) => parent.join(format!("{} (originals)", name.to_string_lossy())),
        _ => scanned.join("mediaopti originals"),
    }
}

/// Size and modification time, to detect a file that changed during processing.
pub(super) fn file_state(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
}

pub(super) struct ScanResult {
    pub(super) files: Vec<PathBuf>,
    /// Single-frame GIFs (handled as images).
    pub(super) static_gifs: HashSet<PathBuf>,
    /// Size of each entry of `files`, same order.
    pub(super) sizes: Vec<u64>,
    pub(super) cleaned_orphans: usize,
    pub(super) unreadable: usize,
    /// Empty (0-byte) media files: nothing to optimize, left out of the queue.
    pub(super) empty: usize,
    pub(super) messages: Vec<String>,
}

/// Recursively finds videos (skipping hidden folders like the old glob did), deletes leftover
/// temp files and sorts the queue by size, largest first. File sizes come from the directory
/// listing itself (on Windows `DirEntry::metadata` needs no extra system call).
/// The scanned files not already in `loaded` (compared by reference: no copy of the
/// library's paths is made under the engine's lock).
fn only_new(loaded: &[PathBuf], files: Vec<PathBuf>, sizes: Vec<u64>) -> Vec<(PathBuf, u64)> {
    let known: HashSet<&Path> = loaded.iter().map(PathBuf::as_path).collect();
    files.into_iter().zip(sizes).filter(|(f, _)| !known.contains(f.as_path())).collect()
}

/// Whether ffmpeg can read `path` as media (a half-written encode, e.g. an MP4 without its
/// index, can't).
/// Runs through `tools::run` with a time limit: a file that makes ffmpeg hang can't hold up
/// the scan.
pub(super) fn media_readable(ffmpeg: &Path, path: &Path) -> bool {
    let args = crate::os_args!["-hide_banner", "-v", "error", "-i", path, "-map", "0", "-c", "copy", "-f", "null", "-"];
    let gate = image::tools::Gate::new(CancellationToken::new(), None);
    image::tools::run(ffmpeg, args, None, image::tools::Capture::None, Duration::from_secs(600), &gate)
        .is_ok_and(|r| r.success && r.stderr.is_empty())
}

/// `settle_temps`: leftover temp files are settled (`settle_orphan_temp`); off while a batch
/// runs, whose workers' temps are not leftovers.
pub(super) fn scan_folder(roots: &[PathBuf], exclude: Option<&Path>, temp: Option<&Path>, found: &std::sync::atomic::AtomicUsize, ffmpeg: &Path, settle_temps: bool) -> ScanResult {
    let mut messages = Vec::new();
    let mut cleaned_orphans = 0;
    // Work folders of runs that were killed or crashed.
    let mut stale = vec![std::env::temp_dir()];
    stale.extend(temp.map(Path::to_path_buf));
    for root in roots {
        stale.extend(images::volume_root(root).map(|r| r.join(images::DRIVE_WORK_DIR)));
    }
    stale.sort();
    stale.dedup();
    let cleaned_work: usize = stale.iter().map(|d| system::clean_stale_work(d)).sum();
    if cleaned_work > 0 {
        messages.push(format!("Removed {cleaned_work} leftover work folder(s) from an earlier run that did not finish"));
    }
    let mut unreadable = 0;
    let mut empty = 0;
    let mut with_sizes = Vec::new();
    let mut static_gifs = HashSet::new();

    // Folders are walked; a file given directly is its own one-entry walk.
    let walker = roots.iter().flat_map(|root| {
        walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || (!e.file_name().to_string_lossy().starts_with('.') && Some(e.path()) != exclude))
    });
    let mut seen = HashSet::new();

    for entry in walker {
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        if !entry.file_type().is_file() || !(args::is_video_file(entry.path()) || image::kind_of(entry.path()).is_some()) {
            continue;
        }
        if !seen.insert(entry.path().to_path_buf()) {
            continue; // dropped twice (a folder and a file inside it)
        }
        if is_orphan_temp(entry.path()) {
            if !settle_temps {
                continue;
            }
            // A leftover with its original still there is a half-made version: removed. If
            // the original is gone, the app was stopped after setting it aside but before the
            // new version got its name: the leftover IS the file now, so it gets the name.
            match settle_orphan_temp(entry.path(), &|p| media_readable(ffmpeg, p)) {
                Ok(Settled::Removed) => cleaned_orphans += 1,
                Ok(Settled::Recovered(named)) => messages.push(format!(
                    "Recovered {} (the app had stopped while replacing it; its original is in the Recycle Bin or backup folder)",
                    named.display()
                )),
                Ok(Settled::LeftAlone) => messages.push(format!(
                    "Left {} as it is: a temporary file from an interrupted run whose original could not be found (check it, then rename or delete it)",
                    entry.path().display()
                )),
                Err(err) => messages.push(format!("Could not tidy up temporary file {}: {err}", entry.path().display())),
            }
            continue;
        }
        match entry.metadata() {
            Ok(meta) if meta.len() > 0 => {
                found.fetch_add(1, Ordering::Relaxed);
                with_sizes.push((entry.into_path(), meta.len()));
            }
            Ok(_) => empty += 1,
            Err(err) => messages.push(format!("Could not read {}: {err}", entry.path().display())),
        }
    }

    // Still or animated: each GIF's header is read, on several threads (a library can hold
    // tens of thousands of GIFs, and one at a time is slow on a hard disk).
    let gifs: Vec<&PathBuf> = with_sizes.iter().map(|(p, _)| p).filter(|p| args::is_gif(p)).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|scope| {
        let handles: Vec<_> = gifs
            .chunks(gifs.len().div_ceil(threads).max(1))
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .filter(|p| !std::fs::File::open(p).is_ok_and(|f| system::is_animated_and_valid_gif(std::io::BufReader::new(f))))
                        .map(|p| (*p).clone())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            static_gifs.extend(h.join().unwrap_or_default());
        }
    });

    with_sizes.sort_by_key(|&(_, size)| std::cmp::Reverse(size));
    let (files, sizes) = with_sizes.into_iter().unzip();
    ScanResult {
        files,
        static_gifs,
        sizes,
        cleaned_orphans,
        unreadable,
        empty,
        messages,
    }
}

impl Engine {
    /// Selects a folder and scans it (replacing what was loaded).
    pub async fn load_dir(self: &Arc<Self>, dir: PathBuf) {
        if !dir.is_dir() {
            self.log(format!("Folder not found: {}", dir.display()));
            return;
        }
        self.load_items(vec![dir], false).await;
    }

    /// Several folders chosen at once: they replace what was loaded, like one folder (the start
    /// folder becomes their common folder).
    pub fn spawn_load_items(self: &Arc<Self>, items: Vec<PathBuf>) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.load_items(items, false).await });
    }

    /// Files and folders dropped on the window: added to the running batch's queues, or to
    /// what is loaded (the start folder becomes their common folder).
    pub fn spawn_add_items(self: &Arc<Self>, items: Vec<PathBuf>) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.load_items(items, true).await });
    }

    /// Scans `items` (folders and files). `add`: keep what is loaded (or queued, during a
    /// run) and add to it; otherwise they replace it.
    pub async fn load_items(self: &Arc<Self>, items: Vec<PathBuf>, add: bool) {
        let items: Vec<PathBuf> = items.into_iter().filter(|p| p.exists()).collect();
        if items.is_empty() {
            self.log("Nothing to add (the dropped items no longer exist)");
            return;
        }
        let (working, old_dir) = {
            let inner = self.lock();
            (inner.working, inner.dir.clone())
        };
        if working && !add {
            self.log("Stop processing before choosing another folder");
            return;
        }
        if !working && self.lock().status == Status::Scanning {
            self.log("A scan is running; add the files again when it has finished");
            return;
        }
        // The common folder of everything loaded: names in the window and the backup folder's
        // layout are relative to it. During a run it stays as it was.
        let mut all = items.clone();
        if add {
            all.extend(old_dir.clone());
        }
        let root = if working { old_dir.clone().unwrap_or_else(|| common_root(&items)) } else { common_root(&all) };
        if !working {
            let mut inner = self.lock();
            inner.dir = Some(root.clone());
            if !add {
                inner.files.clear();
                inner.file_sizes.clear();
                inner.static_gifs.clear();
            }
            inner.pending_queue.clear();
            inner.queue_cursor = 0;
            inner.cpu_queues = Default::default();
            inner.cpu_cursors = [0; 3];
            inner.reset_run_tracking();
            inner.run = RunStats::default();
            inner.status = Status::Scanning;
        }
        let what = match items.as_slice() {
            [one] => one.display().to_string(),
            many => format!("{} dropped items", many.len()),
        };
        self.log(format!("Scanning {what}"));
        self.send_state(true);

        // A backup folder inside the scanned folder must not be scanned (or optimized again).
        let (exclude, temp) = {
            let inner = self.lock();
            (inner.backup_root(), inner.options.temp_dir.clone())
        };
        let roots = items.clone();
        // The window shows the count as it grows (nothing else updates it during a scan).
        let found = Arc::clone(&self.scan_found);
        found.store(0, Ordering::Relaxed);
        let ticker = {
            let engine = Arc::clone(self);
            self.rt.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    engine.send_state(false);
                }
            })
        };
        let ffmpeg = self.ffmpeg_path.clone();
        let scan = self.rt.spawn_blocking(move || scan_folder(&roots, exclude.as_deref(), temp.as_deref(), &found, &ffmpeg, !working)).await;
        ticker.abort();
        let result = match scan {
            Ok(r) => r,
            Err(err) => {
                if !working {
                    self.lock().status = Status::Error;
                }
                self.log(format!("Could not scan: {err}"));
                self.send_state(true);
                return;
            }
        };
        for msg in &result.messages {
            self.log(msg.clone());
        }
        if result.empty > 0 {
            self.log(format!("{} empty (0-byte) file(s) left out: nothing to optimize", result.empty));
        }
        if result.unreadable > 0 {
            self.log(format!("{} files or folders could not be read (permissions or broken links) and were skipped", result.unreadable));
        }
        if result.cleaned_orphans > 0 {
            self.log(format!("Removed {} leftover temporary (.mediaopti) file(s)", result.cleaned_orphans));
        }
        let recognised = self.recognise_by_content(&result.files, &result.sizes).await;
        if recognised > 0 {
            self.log(format!("{recognised} moved, renamed or copied files recognised as already processed"));
        }

        if working {
            // Appended to the running batch; more workers start if the queues need them.
            let (videos, cpu) = {
                let mut inner = self.lock();
                let new = only_new(&inner.files, result.files, result.sizes);
                let (mut videos, mut cpu) = (0, 0);
                for (file, size) in new {
                    if result.static_gifs.contains(&file) {
                        inner.static_gifs.insert(file.clone());
                    }
                    let place = inner.place(&file, size);
                    let stage = cpu_stage(&file);
                    let i = inner.add_file(file, size);
                    match place {
                        Place::Video => {
                            inner.pending_queue.push(i);
                            inner.queue_bytes[0] += size;
                            inner.queue_revision += 1;
                            videos += 1;
                        }
                        Place::Cpu => {
                            inner.run.stage_total[stage] += 1;
                            inner.queue_cpu(i, size);
                            cpu += 1;
                        }
                        _ => {}
                    }
                }
                inner.run.total += videos + cpu;
                inner.run.images_total += cpu;
                if inner.working {
                    self.spawn_workers(&mut inner);
                }
                (videos, cpu)
            };
            self.log(if videos + cpu == 0 {
                "Nothing new to add to the queue (already processed, or switched off)".to_string()
            } else {
                format!("Added to the queue: {videos} videos and {cpu} images, audio or documents")
            });
            self.send_state(true);
            return;
        }

        let (known, generated, small_gifs, videos, cpu, total) = {
            let mut inner = self.lock();
            if add {
                for (file, size) in only_new(&inner.files, result.files, result.sizes) {
                    inner.add_file(file, size);
                }
                inner.static_gifs.extend(result.static_gifs);
            } else {
                inner.files = result.files;
                inner.file_sizes = result.sizes;
                inner.static_gifs = result.static_gifs;
            }
            inner.reset_run_tracking();
            inner.rebuild_queue();
            inner.run = RunStats::default();
            inner.status = Status::Ready;
            (inner.skipped_known, inner.skipped_generated, inner.skipped_small_gifs, inner.pending_queue.len(), inner.cpu_total(), inner.files.len())
        };
        self.log(format!(
            "{} {total} files in {} ({videos} videos and {cpu} images, audio or documents to process)",
            if add { "Now" } else { "Found" },
            root.display()
        ));
        if known > 0 {
            self.log(format!("{known} files are already in the processed history"));
        }
        if small_gifs > 0 {
            self.log(format!("{small_gifs} animated GIFs are under the size limit and will stay GIFs"));
        }
        if generated > 0 {
            self.log(format!("{generated} generated or temporary files will be ignored"));
        }
        self.send_state(true);
    }

    /// Scans a folder in the background.
    pub fn spawn_load_dir(self: &Arc<Self>, dir: PathBuf) {
        let engine = Arc::clone(self);
        self.rt.spawn(async move { engine.load_dir(dir).await });
    }
}
