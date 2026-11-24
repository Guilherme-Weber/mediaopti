//! The work queue: scanned files, classification (video or CPU stage), claiming, and previews.

use super::*;
use crate::format::Kind;
use std::collections::HashMap;

/// The CPU stage of a queued file (see `Kind::stage`).
pub(super) fn cpu_stage(file: &Path) -> usize {
    image::kind_of(file).map_or(2, Kind::stage)
}

/// How many CPU files of each stage run at once with `slots` CPU slots: audio half the slots
/// (decoding is one core per file; the files are read once, sequentially), documents half the
/// slots (a PDF's renders and qpdf use memory; comic pages and PDF images borrow the idle
/// slots), images every slot.
pub(super) fn stage_limits(slots: usize) -> [usize; 3] {
    let half = (slots / 2).max(2);
    [half, half, usize::MAX]
}

/// When audio or documents are the active CPU stage (and images haven't started yet), returns
/// `(stage_limit, stage_label)` so the UI's "CPU jobs" tile can explain why fewer than all
/// `slots` files are running at once (e.g. `7 of 7 (documents)` instead of `7 of 15 slots`).
pub(crate) fn active_stage_limit(type_progress: &[(usize, usize, usize); 4], slots: usize) -> Option<(usize, &'static str)> {
    // type_progress: [0]=videos, [1]=audio, [2]=documents, [3]=images -> (done, total, active).
    if type_progress[3].2 > 0 {
        return None;
    }
    let limits = stage_limits(slots);
    if type_progress[1].0 < type_progress[1].1 && type_progress[1].2 > 0 && type_progress[2].2 == 0 {
        return Some((limits[0].min(slots), "audio"));
    }
    if type_progress[2].0 < type_progress[2].1 && type_progress[2].2 > 0 {
        return Some((limits[1].min(slots), "documents"));
    }
    None
}

/// Images at or above this file size are treated as heavy (likely high-megapixel / slow /
/// memory-hungry): at most [`max_heavy_images`] run at once while smaller images are left,
/// so the remaining workers stay busy on fast files instead of piling onto huge ones.
const HEAVY_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// How many heavy images run at once alongside lighter ones (`slots / 4`, in `2..=4`).
fn max_heavy_images(slots: usize) -> usize {
    (slots / 4).clamp(2, 4)
}

/// Where a scanned file goes (see `WorkQueue::place`).
pub(super) enum Place {
    Generated,
    /// Its category is switched off (`gif`: an animated GIF kept as a GIF).
    Off { gif: bool },
    Known,
    Failed,
    Cpu,
    Video,
}

/// Result of trying to claim the next CPU file.
pub(super) enum CpuClaim {
    /// A file, its stage, whether it counts as a heavy image, and the log line announcing a new stage.
    File(PathBuf, usize, bool, Option<String>),
    /// An earlier stage still has files to start, or this stage is at its limit.
    Wait,
    /// Nothing left to start.
    Done,
}

/// The scanned files, their pending queues, and in-flight/failed tracking.
///
/// All fields are private so `files`, `file_sizes`, `file_index`, `pending_queue`,
/// `cpu_queues`, `queue_bytes`, `queue_revision`, and active counts always stay consistent.
pub(super) struct WorkQueue {
    dir: Option<PathBuf>,
    files: Vec<PathBuf>,
    /// Size of each entry of `files` at scan time, same order.
    file_sizes: Vec<u64>,
    /// Lazy O(1) lookup from path hash to `files` index (`None` until first `index_of`/`moved`/`gone`,
    /// so runs without conversions/renames/pauses take zero extra memory).
    file_index: Option<HashMap<u64, u32>>,
    /// Single-frame GIFs/WebPs found by the scan: never treated as animated video.
    static_gifs: HashSet<PathBuf>,
    /// Whether `static_gifs` has been populated for the current `files` (skipped during
    /// scan when Videos is off to avoid opening tens of thousands of GIFs on an HDD, and
    /// computed lazily if Videos is later turned on).
    gifs_checked: bool,
    /// Videos to do, as indices into `files` (a queued file costs 4 bytes, not a second copy
    /// of its path: a library holds about a million files).
    pending_queue: Vec<u32>,
    queue_cursor: usize,
    /// CPU files by stage (audio, documents, images: see `Kind::stage`), sorted largest-first.
    /// `VecDeque` lets the images stage claim heavy files from the front and normal files from
    /// the back and requeue paused files at the front in O(1).
    cpu_queues: [VecDeque<u32>; 3],
    cpu_active: [usize; 3],
    /// Heavy images (front-of-queue, >= `HEAVY_IMAGE_BYTES`) currently running.
    heavy_images_active: usize,
    /// The stages announced in the log this batch.
    stage_logged: [bool; 3],
    /// Bytes waiting by type (videos, audio, documents, images).
    queue_bytes: [u64; 4],
    queue_revision: u64,
    skipped_known: usize,
    skipped_generated: usize,
    skipped_small_gifs: usize,
    failed: HashSet<PathBuf>,
    in_progress: HashSet<PathBuf>,
}

/// The scanned files not already in `loaded` (compared by reference).
fn only_new(loaded: &[PathBuf], files: Vec<PathBuf>, sizes: Vec<u64>) -> Vec<(PathBuf, u64)> {
    let known: HashSet<&Path> = loaded.iter().map(PathBuf::as_path).collect();
    files.into_iter().zip(sizes).filter(|(f, _)| !known.contains(f.as_path())).collect()
}

impl WorkQueue {
    pub(super) fn new() -> Self {
        Self {
            dir: None,
            files: Vec::new(),
            file_sizes: Vec::new(),
            file_index: None,
            static_gifs: HashSet::new(),
            gifs_checked: true,
            pending_queue: Vec::new(),
            queue_cursor: 0,
            cpu_queues: Default::default(),
            cpu_active: [0; 3],
            heavy_images_active: 0,
            stage_logged: [false; 3],
            queue_bytes: [0; 4],
            queue_revision: 0,
            skipped_known: 0,
            skipped_generated: 0,
            skipped_small_gifs: 0,
            failed: HashSet::new(),
            in_progress: HashSet::new(),
        }
    }

    pub(super) fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub(super) fn has_dir(&self) -> bool {
        self.dir.is_some()
    }

    pub(super) fn total_files(&self) -> usize {
        self.files.len()
    }

    pub(super) fn gifs_checked(&self) -> bool {
        self.gifs_checked
    }

    pub(super) fn animated_candidates(&self) -> Vec<PathBuf> {
        self.files.iter().filter(|f| args::is_animated_picture(f)).cloned().collect()
    }

    pub(super) fn set_static_gifs(&mut self, statics: HashSet<PathBuf>) {
        self.static_gifs = statics;
        self.gifs_checked = true;
    }

    pub(super) fn videos_waiting(&self) -> usize {
        self.pending_queue.len().saturating_sub(self.queue_cursor)
    }

    pub(super) fn cpu_waiting(&self) -> usize {
        self.cpu_queues.iter().map(VecDeque::len).sum()
    }

    pub(super) fn cpu_stage_counts(&self) -> [usize; 3] {
        [self.cpu_queues[0].len(), self.cpu_queues[1].len(), self.cpu_queues[2].len()]
    }

    pub(super) fn cpu_active(&self) -> [usize; 3] {
        self.cpu_active
    }

    pub(super) fn active_cpu_total(&self) -> usize {
        self.cpu_active.iter().sum()
    }

    pub(super) fn remaining_pending(&self) -> usize {
        self.videos_waiting() + self.cpu_waiting() + self.in_progress.len()
    }

    pub(super) fn queue_counts(&self) -> [(usize, u64); 4] {
        [
            (self.videos_waiting(), self.queue_bytes[0]),
            (self.cpu_queues[0].len(), self.queue_bytes[1]),
            (self.cpu_queues[1].len(), self.queue_bytes[2]),
            (self.cpu_queues[2].len(), self.queue_bytes[3]),
        ]
    }

    pub(super) fn revision(&self) -> u64 {
        self.queue_revision
    }

    pub(super) fn skipped_known(&self) -> usize {
        self.skipped_known
    }

    pub(super) fn skipped_generated(&self) -> usize {
        self.skipped_generated
    }

    pub(super) fn skipped_small_gifs(&self) -> usize {
        self.skipped_small_gifs
    }

    pub(super) fn note_skipped_known(&mut self) {
        self.skipped_known += 1;
    }

    pub(super) fn is_in_progress(&self, path: &Path) -> bool {
        self.in_progress.contains(path)
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
            .map_or_default(|n| n.to_string_lossy().into_owned())
    }

    /// The first `limit` files waiting, in processing order: (name, type, size).
    pub(super) fn preview(&self, limit: usize) -> Vec<(String, &'static str, u64)> {
        let videos = self.pending_queue.iter().skip(self.queue_cursor).map(|&i| (i, "Video"));
        let cpu = ["Audio", "Document", "Image"]
            .iter()
            .enumerate()
            .flat_map(|(s, label)| self.cpu_queues[s].iter().map(move |&i| (i, *label)));
        videos
            .chain(cpu)
            .take(limit)
            .map(|(i, kind)| {
                let i = i as usize;
                (self.display_name(&self.files[i]), kind, self.file_sizes.get(i).copied().unwrap_or(0))
            })
            .collect()
    }

    // --------------------------------------------------------- classification

    /// Whether `file` (of `size` bytes) is handled by the image workers: PNG/JPEG, still
    /// GIFs, and animated GIFs under the size limit (bigger ones become video).
    pub(super) fn is_image_job(&self, file: &Path, size: u64, options: &Options) -> bool {
        match image::kind_of(file) {
            // With videos switched off, big animated GIFs are optimized as GIFs.
            Some(Kind::Gif) => size < options.gif_min_bytes || self.static_gifs.contains(file) || !options.videos,
            Some(_) => true,
            None => false,
        }
    }

    /// Whether the settings let the CPU workers take this file: conversions follow their
    /// conversion gate, everything else its category switch.
    pub(super) fn cpu_job_enabled(file: &Path, options: &Options) -> bool {
        let Some(kind) = image::kind_of(file) else { return false };
        let on = match kind.stage() {
            0 => options.audio != ImageMode::Off,
            1 => options.docs,
            _ => options.images != ImageMode::Off,
        };
        let c = options.convert;
        let conversion_on = match kind.spec().conversion {
            Some(crate::format::ConversionGate::Wav) => c.wav,
            Some(crate::format::ConversionGate::Images) => c.images,
            Some(crate::format::ConversionGate::Cbr) => c.cbr,
            None => true,
        };
        on && conversion_on
    }

    /// Whether `file` is processed visually lossless (audio files by the audio setting,
    /// the rest by the image setting).
    pub(super) fn lossy_for(file: &Path, options: &Options) -> bool {
        let audio = image::kind_of(file).is_some_and(Kind::is_audio);
        (if audio { options.audio } else { options.images }) == ImageMode::Lossy
    }

    /// The lossy floor a file is done at: the picture setting for formats whose policy is
    /// `PictureFloor`, and the default for PDFs and lossless-only formats.
    pub(super) fn target_for(file: &Path, options: &Options) -> u32 {
        match image::kind_of(file).map(|k| k.spec().quality_policy) {
            Some(crate::format::QualityPolicy::PictureFloor) => options.image_target,
            _ => image::DEFAULT_TARGET,
        }
    }

    /// The image pass requested for `file` under `options`.
    pub(super) fn image_pass_for(file: &Path, options: &Options) -> history::Pass {
        history::Pass::image(Self::lossy_for(file, options), options.strip && options.redo_stripped, Self::target_for(file, options))
    }

    /// History pass request for a file under `options`.
    pub(super) fn want(&self, file: &Path, size: u64, options: &Options) -> history::Pass {
        if self.is_image_job(file, size, options) {
            Self::image_pass_for(file, options)
        } else {
            history::Pass::Video
        }
    }

    /// Where a scanned file goes under `options` and `history`.
    pub(super) fn place(&self, file: &Path, size: u64, options: &Options, history: &History) -> Place {
        if is_generated_name(file) {
            Place::Generated
        } else if self.is_image_job(file, size, options) {
            if !Self::cpu_job_enabled(file, options) {
                // Not remembered: they may be optimized once switched on. Count GIFs that stay
                // GIFs (still, or under the video size limit) only while videos are enabled.
                Place::Off { gif: args::is_gif(file) && options.videos }
            } else if history.contains_pass(file, Self::image_pass_for(file, options)) {
                Place::Known
            } else if self.failed.contains(file) {
                Place::Failed
            } else {
                Place::Cpu
            }
        } else if !options.videos || (args::is_webp(file) && self.static_gifs.contains(file)) {
            Place::Off { gif: false }
        } else if history.contains(file) {
            // A GIF still here was not replaced by a video. If it never went through the GIF
            // optimizer (images were off, or older versions), it goes there now, without
            // being encoded again.
            if args::is_gif(file)
                && Self::cpu_job_enabled(file, options)
                && !history.contains_pass(file, Self::image_pass_for(file, options))
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

    // --------------------------------------------------------- mutations

    /// Prepares the queue for an idle folder scan (`add` keeps previously loaded files).
    pub(super) fn begin_scan(&mut self, root: PathBuf, add: bool) {
        self.dir = Some(root);
        if !add {
            self.files.clear();
            self.file_sizes.clear();
            self.file_index = None;
            self.static_gifs.clear();
        }
        self.pending_queue.clear();
        self.queue_cursor = 0;
        self.cpu_queues = Default::default();
        self.reset_run_state();
    }

    /// Restores or clears the queue after a cancelled idle scan. Returns `true` if existing
    /// files were kept (ready), `false` if emptied (idle).
    pub(super) fn cancel_scan(&mut self, old_dir: Option<PathBuf>, add: bool, options: &Options, history: &History) -> bool {
        if add && !self.files.is_empty() {
            self.dir = old_dir;
            self.rebuild(options, history);
            true
        } else {
            self.dir = None;
            self.files.clear();
            self.file_sizes.clear();
            self.file_index = None;
            self.static_gifs.clear();
            false
        }
    }

    /// Applies an idle scan's results (either replacing or merging into existing files) and rebuilds.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_scan(
        &mut self,
        files: Vec<PathBuf>,
        sizes: Vec<u64>,
        static_gifs: HashSet<PathBuf>,
        check_gifs: bool,
        add: bool,
        options: &Options,
        history: &History,
    ) {
        if add {
            for (file, size) in only_new(&self.files, files, sizes) {
                self.add_file(file, size);
            }
            self.static_gifs.extend(static_gifs);
            self.gifs_checked = self.gifs_checked && check_gifs;
        } else {
            self.files = files;
            self.file_sizes = sizes;
            self.file_index = None;
            self.static_gifs = static_gifs;
            self.gifs_checked = check_gifs;
        }
        self.reset_run_state();
        self.rebuild(options, history);
    }

    /// Appends newly dropped files to a running batch, returning `(videos_added, stage_added)`.
    pub(super) fn append_during_run(
        &mut self,
        files: Vec<PathBuf>,
        sizes: Vec<u64>,
        static_gifs: &HashSet<PathBuf>,
        options: &Options,
        history: &History,
    ) -> (usize, [usize; 3]) {
        let new = only_new(&self.files, files, sizes);
        let mut videos = 0;
        let mut stages = [0usize; 3];
        for (file, size) in new {
            if static_gifs.contains(&file) {
                self.static_gifs.insert(file.clone());
            }
            let place = self.place(&file, size, options, history);
            let stage = cpu_stage(&file);
            let i = self.add_file(file, size);
            match place {
                Place::Video => {
                    self.pending_queue.push(i);
                    self.queue_bytes[0] += size;
                    self.queue_revision += 1;
                    videos += 1;
                }
                Place::Cpu => {
                    stages[stage] += 1;
                    self.queue_bytes[stage + 1] += size;
                    self.cpu_queues[stage].push_back(i);
                    self.queue_revision += 1;
                }
                _ => {}
            }
        }
        (videos, stages)
    }

    /// Empties every list and counter (called by `clear_queue`).
    pub(super) fn clear(&mut self) {
        self.dir = None;
        self.files.clear();
        self.file_sizes.clear();
        self.file_index = None;
        self.static_gifs.clear();
        self.gifs_checked = true;
        self.pending_queue.clear();
        self.queue_cursor = 0;
        self.cpu_queues = Default::default();
        self.queue_bytes = [0; 4];
        self.queue_revision += 1;
        self.skipped_known = 0;
        self.skipped_generated = 0;
        self.skipped_small_gifs = 0;
        self.reset_run_state();
    }

    /// Resets per-run tracking (failed, in-progress, active counts, stage announcements).
    pub(super) fn reset_run_state(&mut self) {
        self.failed.clear();
        self.in_progress.clear();
        self.cpu_active = [0; 3];
        self.heavy_images_active = 0;
        self.stage_logged = [false; 3];
    }

    /// Clears only in-progress entries at batch end (keeping `failed` for post-run rebuild).
    pub(super) fn clear_in_progress(&mut self) {
        self.in_progress.clear();
        self.cpu_active = [0; 3];
        self.heavy_images_active = 0;
    }

    /// Rebuilds the work queues from the scanned files, history and this run's failures.
    /// Never during a run: entries marked gone (`moved`) are dropped, which renumbers them.
    pub(super) fn rebuild(&mut self, options: &Options, history: &History) {
        if self.files.iter().any(|f| f.as_os_str().is_empty()) {
            let mut sizes = std::mem::take(&mut self.file_sizes).into_iter();
            let (files, kept): (Vec<PathBuf>, Vec<u64>) = std::mem::take(&mut self.files)
                .into_iter()
                .map(|f| (f, sizes.next().unwrap_or(u64::MAX)))
                .filter(|(f, _)| !f.as_os_str().is_empty())
                .unzip();
            self.files = files;
            self.file_sizes = kept;
            self.file_index = None;
        }
        let (mut queue, mut cpu): (Vec<u32>, [VecDeque<u32>; 3]) = (Vec::new(), Default::default());
        let mut bytes = [0u64; 4];
        let (mut known, mut generated, mut small_gifs) = (0, 0, 0);
        for (i, file) in self.files.iter().enumerate() {
            let size = self.file_sizes.get(i).copied().unwrap_or(u64::MAX);
            match self.place(file, size, options, history) {
                Place::Generated => generated += 1,
                Place::Off { gif } => small_gifs += usize::from(gif),
                Place::Known => known += 1,
                Place::Failed => {}
                Place::Cpu => {
                    let stage = cpu_stage(file);
                    bytes[stage + 1] += size;
                    cpu[stage].push_back(i as u32);
                }
                Place::Video => {
                    bytes[0] += size;
                    queue.push(i as u32);
                }
            }
        }
        self.pending_queue = queue;
        self.queue_cursor = 0;
        self.cpu_queues = cpu;
        self.queue_bytes = bytes;
        self.queue_revision += 1;
        self.skipped_known = known;
        self.skipped_generated = generated;
        self.skipped_small_gifs = small_gifs;
    }

    /// Stable 64-bit hash of a path's raw `OsStr` bytes (used for the lazy O(1) `file_index`).
    fn path_hash(path: &Path) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.as_os_str().hash(&mut h);
        h.finish()
    }

    /// Ensures the lazy path-hash index is built (0 extra bytes when never called).
    fn ensure_file_index(&mut self) {
        if self.file_index.is_some() {
            return;
        }
        let mut map = HashMap::with_capacity(self.files.len());
        for (i, f) in self.files.iter().enumerate() {
            if !f.as_os_str().is_empty() {
                map.entry(Self::path_hash(f)).or_insert(i as u32);
            }
        }
        self.file_index = Some(map);
    }

    /// Where `file` is in `files` (O(1) via the lazy `file_index`, with linear fallback only
    /// on the rare 64-bit hash collision).
    fn index_of(&mut self, file: &Path) -> Option<u32> {
        self.ensure_file_index();
        let h = Self::path_hash(file);
        if let Some(&i) = self.file_index.as_ref().and_then(|m| m.get(&h)) {
            if self.files.get(i as usize).is_some_and(|f| f.as_os_str() == file.as_os_str()) {
                return Some(i);
            }
        }
        self.files.iter().position(|f| f.as_os_str() == file.as_os_str()).map(|i| i as u32)
    }

    /// Adds a scanned file to `files`, returning its index for the queues.
    pub(super) fn add_file(&mut self, file: PathBuf, size: u64) -> u32 {
        let idx = self.files.len() as u32;
        if let Some(map) = &mut self.file_index {
            map.entry(Self::path_hash(&file)).or_insert(idx);
        }
        self.files.push(file);
        self.file_sizes.push(size);
        idx
    }

    /// `file` is no longer on the disk (gone since the scan): its entry is marked gone.
    pub(super) fn mark_gone(&mut self, file: &Path) {
        if let Some(i) = self.index_of(file) {
            if let Some(map) = &mut self.file_index {
                map.remove(&Self::path_hash(file));
            }
            self.files[i as usize] = PathBuf::new();
            self.queue_revision += 1;
        }
    }

    /// `old` is no longer on disk under its name: a conversion/rename gave it `new`, or a
    /// same-content sibling (`new`) took its place. Updates `files` so post-batch rebuilds
    /// reflect disk state accurately.
    pub(super) fn mark_moved(&mut self, old: &Path, new: &Path, size: Option<u64>) {
        let Some(i) = self.index_of(old) else { return };
        let listed = old.as_os_str() != new.as_os_str() && self.index_of(new).is_some();
        let idx = i as usize;
        let scanned = crate::format::is_scanned_media(new) && !args::is_animated_picture(new);
        if let Some(map) = &mut self.file_index {
            map.remove(&Self::path_hash(old));
        }
        if scanned && !listed {
            self.files[idx] = new.to_path_buf();
            if let Some(map) = &mut self.file_index {
                map.insert(Self::path_hash(new), i);
            }
            if let (Some(s), Some(size)) = (self.file_sizes.get_mut(idx), size) {
                *s = size;
            }
        } else {
            self.files[idx] = PathBuf::new();
        }
        self.queue_revision += 1;
    }

    /// Claims the next pending video, marking it in-progress.
    pub(super) fn claim_video(&mut self, history: &History) -> Option<PathBuf> {
        while self.queue_cursor < self.pending_queue.len() {
            let candidate = self.files[self.pending_queue[self.queue_cursor] as usize].clone();
            self.queue_cursor += 1;
            if !self.in_progress.contains(&candidate)
                && !self.failed.contains(&candidate)
                && !history.contains(&candidate)
            {
                self.in_progress.insert(candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    /// Releases a video from in-progress tracking.
    pub(super) fn release_video(&mut self, path: &Path) {
        self.in_progress.remove(path);
    }

    /// Marks a file as failed for this run so it is not re-claimed before the next batch.
    pub(super) fn mark_failed(&mut self, path: &Path) {
        self.failed.insert(path.to_path_buf());
    }

    /// A video whose job was ended by a Pause: back to the front of the video queue.
    pub(super) fn requeue_video(&mut self, file: &Path) {
        self.in_progress.remove(file);
        if let Some(i) = self.index_of(file) {
            self.pending_queue.insert(self.queue_cursor, i);
        }
    }

    /// A CPU file whose job was ended by a Pause: back to the front of its stage queue.
    pub(super) fn requeue_cpu(&mut self, file: &Path) {
        let stage = cpu_stage(file);
        self.in_progress.remove(file);
        if let Some(i) = self.index_of(file) {
            self.cpu_queues[stage].push_front(i);
        }
    }

    /// Claims the next CPU file, by rolling stage (audio -> documents -> images).
    ///
    /// In the images stage (sorted largest-first), once [`max_heavy_images`] heavy images
    /// (>= 4 MiB) are already running, additional workers claim from the lighter end of the
    /// queue so all cores stay productive instead of piling onto huge memory-heavy pictures.
    pub(super) fn claim_cpu(&mut self, videos_still_running: bool, slots: usize) -> CpuClaim {
        if self.videos_waiting() > 0 && videos_still_running {
            return CpuClaim::Wait;
        }
        let limits = stage_limits(slots);
        let max_heavy = max_heavy_images(slots);
        for (stage, limit) in limits.into_iter().enumerate() {
            while let Some(&front) = self.cpu_queues[stage].front() {
                let file = &self.files[front as usize];
                if self.in_progress.contains(file) || self.failed.contains(file) {
                    self.cpu_queues[stage].pop_front();
                } else {
                    break;
                }
            }
            if self.cpu_queues[stage].is_empty() {
                continue;
            }
            if self.cpu_active[stage] >= limit {
                return CpuClaim::Wait;
            }
            let front_heavy = stage == 2
                && self.cpu_queues[stage]
                    .front()
                    .is_some_and(|&i| self.file_sizes.get(i as usize).copied().unwrap_or(0) >= HEAVY_IMAGE_BYTES);
            let from_back = if front_heavy && self.heavy_images_active >= max_heavy {
                while let Some(&back) = self.cpu_queues[stage].back() {
                    let file = &self.files[back as usize];
                    if self.in_progress.contains(file) || self.failed.contains(file) {
                        self.cpu_queues[stage].pop_back();
                    } else {
                        break;
                    }
                }
                self.cpu_queues[stage]
                    .back()
                    .is_some_and(|&i| self.file_sizes.get(i as usize).copied().unwrap_or(0) < HEAVY_IMAGE_BYTES)
            } else {
                false
            };
            let left = self.cpu_queues[stage].len();
            let Some(idx) = (if from_back {
                self.cpu_queues[stage].pop_back()
            } else {
                self.cpu_queues[stage].pop_front()
            }) else {
                continue;
            };
            let heavy = stage == 2 && self.file_sizes.get(idx as usize).copied().unwrap_or(0) >= HEAVY_IMAGE_BYTES;
            let file = self.files[idx as usize].clone();
            self.cpu_active[stage] += 1;
            if heavy {
                self.heavy_images_active += 1;
            }
            self.in_progress.insert(file.clone());
            let announce = (!self.stage_logged[stage]).then(|| {
                self.stage_logged[stage] = true;
                let (what, how) = match stage {
                    0 => ("audio files", format!("{} at a time", limits[0])),
                    1 => ("documents", format!("{} at a time, pages/images on idle cores", limits[1])),
                    _ => ("images", "on every free core".to_string()),
                };
                format!("Now processing {left} {what}, {how}")
            });
            return CpuClaim::File(file, stage, heavy, announce);
        }
        CpuClaim::Done
    }

    /// Completes a CPU job, atomically releasing in-progress and active stage/heavy counters.
    /// `claimed` is `Some((stage, heavy))` for CPU workers and `None` for video-worker GIF fallback.
    pub(super) fn finish_cpu(&mut self, path: &Path, claimed: Option<(usize, bool)>) {
        self.in_progress.remove(path);
        if let Some((stage, heavy)) = claimed {
            self.cpu_active[stage] = self.cpu_active[stage].saturating_sub(1);
            if heavy {
                self.heavy_images_active = self.heavy_images_active.saturating_sub(1);
            }
        }
    }

    /// Drops every unstarted file of `kind` from its stage queue (after repeated tool startup
    /// failures) and updates `queue_bytes` and `queue_revision`. Returns how many were dropped.
    pub(super) fn drop_cpu_kind(&mut self, kind: Kind) -> usize {
        let s = kind.stage();
        let rest = std::mem::take(&mut self.cpu_queues[s]);
        let (dropped, kept): (VecDeque<u32>, VecDeque<u32>) =
            rest.into_iter().partition(|&i| image::kind_of(&self.files[i as usize]) == Some(kind));
        self.cpu_queues[s] = kept;
        let bytes: u64 = dropped.iter().map(|&i| self.file_sizes.get(i as usize).copied().unwrap_or(0)).sum();
        self.queue_bytes[s + 1] = self.queue_bytes[s + 1].saturating_sub(bytes);
        self.queue_revision += 1;
        dropped.len()
    }
}

impl Inner {
    pub(super) fn lossy_for(&self, file: &Path) -> bool {
        WorkQueue::lossy_for(file, &self.options)
    }

    pub(super) fn target_for(&self, file: &Path) -> u32 {
        WorkQueue::target_for(file, &self.options)
    }

    pub(super) fn image_pass_for(&self, file: &Path) -> history::Pass {
        WorkQueue::image_pass_for(file, &self.options)
    }

    pub(super) fn want(&self, file: &Path, size: u64) -> history::Pass {
        self.queue.want(file, size, &self.options)
    }

    pub(super) fn images_lossy(&self) -> bool {
        self.options.images == ImageMode::Lossy
    }

    pub(super) fn rebuild_queue(&mut self) {
        self.queue.rebuild(&self.options, &self.history);
    }

    /// Root of the backup folder for replaced originals, if that option is chosen.
    pub(super) fn backup_root(&self) -> Option<PathBuf> {
        let Originals::BackupFolder(chosen) = &self.options.originals else { return None };
        chosen.clone().or_else(|| self.queue.dir().map(default_backup_root))
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
                    .queue
                    .dir()
                    .and_then(|d| original.strip_prefix(d).ok())
                    .map_or_else(|| PathBuf::from(original.file_name().unwrap_or_default()), Path::to_path_buf);
                system::Disposal::MoveTo(root.join(rel))
            }
        }
    }

    pub(super) fn reset_run_tracking(&mut self) {
        self.queue.reset_run_state();
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
                    name: self.queue.display_name(p),
                    secs: t.elapsed().as_secs(),
                    parts: progress.and_then(|pr| pr.get()),
                    phase: progress.and_then(|pr| pr.phase()),
                }
            })
            .collect();
        let r = &self.run;
        let videos_running = self.workers.len();
        let cpu_active = self.queue.cpu_active();
        let type_progress = [
            (r.completed.saturating_sub(r.images_done), r.total.saturating_sub(r.images_total), videos_running),
            (r.stage_done[0], r.stage_total[0], cpu_active[0]),
            (r.stage_done[1], r.stage_total[1], cpu_active[1]),
            (r.stage_done[2], r.stage_total[2], cpu_active[2]),
        ];

        StateDto {
            status: self.status,
            dir: self.queue.dir().map(|d| d.to_string_lossy().into_owned()),
            total_files: self.queue.total_files(),
            pending_files: self.queue.remaining_pending(),
            skipped_known: self.queue.skipped_known(),
            skipped_generated: self.queue.skipped_generated(),
            skipped_small_gifs: self.queue.skipped_small_gifs(),
            history_count: self.history.len(),
            run_total: self.run.total,
            completed_this_run: self.run.completed,
            compressed_this_run: self.run.compressed,
            kept_this_run: self.run.kept,
            skipped_this_run: self.run.skipped,
            failed_this_run: self.run.failed,
            space_saved_mb: round1(self.space_saved_mb),
            queue_counts: self.queue.queue_counts(),
            queue_revision: self.queue.revision(),
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
            inner.queue.clear();
            inner.reset_run_tracking();
            inner.run = RunStats::default();
            inner.status = Status::Idle;
        }
        self.log("Queue cleared");
        self.send_state(true);
    }

    /// The first `limit` files waiting, in processing order: (name, type, size).
    pub fn queue_preview(&self, limit: usize) -> Vec<(String, &'static str, u64)> {
        self.lock().queue.preview(limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_history() -> History {
        History::load(PathBuf::from("nonexistent-queue-test-history.txt"))
    }

    /// When many heavy images (>= 4 MiB) sit at the front of the queue, at most
    /// `max_heavy_images(slots)` run at once while lighter images are available at the back;
    /// once no light images remain, workers still take remaining heavy ones.
    #[test]
    fn heavy_images_pair_with_light_images_in_the_queue() {
        let mut q = WorkQueue::new();
        for i in 0..5u64 {
            q.add_file(PathBuf::from(format!("heavy{i}.png")), (20 - i) * 1024 * 1024);
        }
        for i in 0..4u64 {
            q.add_file(PathBuf::from(format!("light{i}.png")), (500 - i * 100) * 1024);
        }
        q.rebuild(&Options::default(), &empty_history());

        let mut claimed = Vec::new();
        while let CpuClaim::File(p, _, heavy, _) = q.claim_cpu(false, 15) {
            claimed.push((p.file_name().unwrap().to_string_lossy().into_owned(), heavy));
        }
        assert_eq!(claimed[..3], [
            ("heavy0.png".into(), true),
            ("heavy1.png".into(), true),
            ("heavy2.png".into(), true),
        ]);
        assert_eq!(claimed[3..7], [
            ("light3.png".into(), false),
            ("light2.png".into(), false),
            ("light1.png".into(), false),
            ("light0.png".into(), false),
        ]);
        assert_eq!(claimed[7..], [
            ("heavy3.png".into(), true),
            ("heavy4.png".into(), true),
        ]);
    }

    #[test]
    fn drop_cpu_kind_updates_counts_and_bytes_atomically() {
        let mut q = WorkQueue::new();
        q.add_file(PathBuf::from("a.png"), 1_000);
        q.add_file(PathBuf::from("b.png"), 2_000);
        q.add_file(PathBuf::from("c.jpg"), 3_000);
        q.rebuild(&Options::default(), &empty_history());
        let rev0 = q.revision();
        assert_eq!(q.queue_counts()[3], (3, 6_000));

        let dropped = q.drop_cpu_kind(Kind::Png);
        assert_eq!(dropped, 2);
        assert_eq!(q.queue_counts()[3], (1, 3_000));
        assert!(q.revision() > rev0);
        assert_eq!(q.preview(10).into_iter().map(|(n, _, _)| n).collect::<Vec<_>>(), vec!["c.jpg"]);
    }

    #[test]
    fn mark_moved_and_mark_gone_keep_index_and_rebuild_consistent() {
        let mut q = WorkQueue::new();
        q.add_file(PathBuf::from("old.bmp"), 5_000);
        q.add_file(PathBuf::from("gone.png"), 2_000);
        q.add_file(PathBuf::from("keep.jpg"), 1_000);
        let opts = Options::default();
        let hist = empty_history();
        q.rebuild(&opts, &hist);

        q.mark_moved(Path::new("old.bmp"), Path::new("old.png"), Some(1_200));
        q.mark_gone(Path::new("gone.png"));
        q.rebuild(&opts, &hist);

        assert_eq!(q.total_files(), 2);
        assert_eq!(q.queue_counts()[3], (2, 2_200));
        let names: Vec<_> = q.preview(10).into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(names, vec!["old.png", "keep.jpg"]);
    }

    #[test]
    fn append_during_run_deduplicates_and_routes_by_stage() {
        let mut q = WorkQueue::new();
        let opts = Options::default();
        let hist = empty_history();
        q.apply_scan(vec![PathBuf::from("a.mp4")], vec![10_000], HashSet::new(), true, false, &opts, &hist);

        let (v, stages) = q.append_during_run(
            vec![PathBuf::from("a.mp4"), PathBuf::from("b.mp4"), PathBuf::from("s.flac"), PathBuf::from("d.pdf"), PathBuf::from("p.png")],
            vec![10_000, 8_000, 4_000, 3_000, 2_000],
            &HashSet::new(),
            &opts,
            &hist,
        );
        assert_eq!(v, 1);
        assert_eq!(stages, [1, 1, 1]);
        assert_eq!(q.videos_waiting(), 2);
        assert_eq!(q.cpu_stage_counts(), [1, 1, 1]);
    }

    #[test]
    fn finish_cpu_and_requeue_maintain_active_counts() {
        let mut q = WorkQueue::new();
        let opts = Options::default();
        let hist = empty_history();
        q.add_file(PathBuf::from("big.png"), 10 * 1024 * 1024);
        q.rebuild(&opts, &hist);

        let CpuClaim::File(path, stage, heavy, announce) = q.claim_cpu(false, 8) else {
            panic!("expected file claim");
        };
        assert_eq!((stage, heavy, announce.is_some()), (2, true, true));
        assert_eq!(q.active_cpu_total(), 1);
        assert!(q.is_in_progress(&path));

        q.finish_cpu(&path, Some((stage, heavy)));
        assert_eq!(q.active_cpu_total(), 0);
        assert!(!q.is_in_progress(&path));

        // Requeue puts it back at the front so it can be claimed again.
        q.requeue_cpu(&path);
        assert!(matches!(q.claim_cpu(false, 8), CpuClaim::File(p, 2, true, None) if p == path));
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    struct Quiet;
    impl Ui for Quiet {
        fn log(&self, _: String) {}
        fn state(&self, _: &StateDto) {}
    }

    /// This process's private memory in MB (PowerShell, good enough for a benchmark).
    fn private_mb() -> f64 {
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("(Get-Process -Id {}).PrivateMemorySize64", std::process::id())])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1_048_576.0
    }

    /// Queue work with a library-sized folder (1 million files, no real files needed): how long
    /// the engine's lock is held by a queue rebuild, the queue preview and clearing the history,
    /// and the memory the queued files take. Run:
    /// `cargo test --release queue_scale -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn queue_scale() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let data = std::env::temp_dir().join(format!("mo-queue-bench-{}", std::process::id()));
        let engine = Engine::new(Box::new(Quiet), &data, PathBuf::from("ffmpeg.exe"), rt.handle().clone());
        engine.apply_options(Options { images: ImageMode::Lossy, audio: ImageMode::Lossy, ..Options::default() });
        const N: usize = 1_000_000;
        let exts = ["png", "png", "png", "png", "png", "png", "png", "jpg", "jpg", "mp4", "flac", "pdf"];
        let m0 = private_mb();
        {
            let mut inner = engine.lock();
            inner.queue.begin_scan(PathBuf::from(r"F:\USER\Pictures"), false);
            for i in 0..N {
                inner.queue.add_file(
                    PathBuf::from(format!(r"F:\USER\Pictures\Some Folder {}\Another Level\file number {i}.{}", i / 1000, exts[i % exts.len()])),
                    100_000 + i as u64,
                );
            }
        }
        let m1 = private_mb();
        let time = |what: &str, f: &mut dyn FnMut()| {
            let t = std::time::Instant::now();
            f();
            println!("{what}: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
        };
        time("rebuild_queue (lock held)", &mut || engine.lock().rebuild_queue());
        let m2 = private_mb();
        time("queue_preview(5000)", &mut || drop(engine.queue_preview(5000)));
        time("history clear + rebuild (clear_history)", &mut || {
            let mut inner = engine.lock();
            inner.history.clear();
            inner.rebuild_queue();
        });
        let inner = engine.lock();
        println!("queued: {} videos, {} CPU files", inner.queue.videos_waiting(), inner.queue.cpu_waiting());
        println!("memory: files {:.0} MB ({:.0} B/file), queues {:.0} MB ({:.0} B/file)", m1 - m0, (m1 - m0) * 1_048_576.0 / N as f64, m2 - m1, (m2 - m1) * 1_048_576.0 / N as f64);
        drop(inner);
        let _ = std::fs::remove_dir_all(&data);
    }
}
