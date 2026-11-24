//! Unified resource budget: commit/physical memory admission, big lossy turn gate,
//! CPU slot borrowing, and per-drive disk space reservations.
//!
//! All OS memory/disk queries go through [`SystemProbe`] (`OsProbe` in production,
//! `FakeProbe` in unit tests), and all tracking state is instance-scoped inside
//! [`ResourceBudget`] (no process-global statics).

use crate::image::tools::{Gate, RunError};
use crate::system::{self, MemoryStatus};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------- constants

/// Free commit needed before starting another ffmpeg while others run. A single 4K AV1 encode
/// (multipass + 32-frame lookahead) was measured at up to ~5.3 GB of commit.
pub const MIN_FREE_COMMIT: u64 = 6 << 30;
pub const MIN_FREE_PHYSICAL: u64 = 2 << 30;
/// Free GPU memory needed before starting another NVENC encode.
pub const MIN_FREE_VRAM_MB: u64 = 3 * 1024;
/// How long a newly started ffmpeg is given to allocate its buffers before the next
/// worker re-checks free memory (when memory is not plentiful for all jobs at once).
pub const ENCODER_ADMISSION_WARMUP: Duration = Duration::from_secs(4);
/// Free commit needed before starting a VMAF measurement while other work runs.
pub const VMAF_MIN_FREE_COMMIT: u64 = 4 << 30;

/// Free commit (RAM + page file) a CPU worker needs before starting another file.
pub const IMAGE_MIN_FREE_COMMIT: u64 = 2 << 30;
/// How long a CPU job just let in counts as about to take memory that doesn't show yet.
const CPU_ADMISSION_WARMUP: Duration = Duration::from_secs(1);
/// Reserve per recently-admitted CPU job to spread stage-change bursts without slowing
/// steady small-file runs.
const CPU_ADMISSION_RESERVE: u64 = 512 << 20;

/// Memory a visually lossless search takes per pixel (measured on a 164 MP JPEG: ~141 B/px).
pub const LOSSY_BYTES_PER_PIXEL: u64 = 150;
/// From this estimate on, a lossy search is a big job gated by [`BigLossyGate`].
pub const BIG_LOSSY_JOB: u64 = 4 << 30;
/// Commit memory left for everything else while a big lossy search or extra threads run.
pub const BIG_LOSSY_RESERVE: u64 = 2 << 30;

/// Free commit needed per extra parallel archive page or PDF image/page render.
pub const PAGE_COMMIT: u64 = 1 << 30;

/// oxipng's commit memory model (measured on 31.2 MP PNG: 0.52 GB on 1 thread, 1.33 GB on 16).
const MT_BASE_BYTES_PER_PIXEL: u64 = 17;
const MT_EXTRA_PER_THREAD_PER_100_PIXELS: u64 = 175;

/// Kept free on a drive that holds work folders (C: is often nearly full).
pub const DISK_RESERVE: u64 = 4 << 30;
/// Kept free on a video's drive besides room for its new version.
pub const VIDEO_DISK_RESERVE: u64 = 4 << 30;
/// Hidden folder on a file's own drive for work files when the temp drive is short on space.
pub const DRIVE_WORK_DIR: &str = ".mediaopti-temp";

// ------------------------------------------------------------------- probe seam

/// Seam for querying host memory and volume free space.
pub trait SystemProbe: Send + Sync {
    fn memory_status(&self) -> Option<MemoryStatus>;
    fn free_space(&self, path: &Path) -> Option<u64>;
}

/// Production probe backed by Windows memory and disk APIs.
pub struct OsProbe;

impl SystemProbe for OsProbe {
    fn memory_status(&self) -> Option<MemoryStatus> {
        system::memory_status()
    }

    fn free_space(&self, path: &Path) -> Option<u64> {
        system::free_space(path)
    }
}

// ------------------------------------------------------------------- CPU slots

/// CPU slots shared by the image workers: each file being processed holds one, and a job
/// with many parts (a comic's pages, a PDF's images) borrows the idle ones so parts run in
/// parallel without running more tools at once than there are workers.
#[derive(Debug)]
pub struct Slots {
    free: Mutex<usize>,
    freed: Condvar,
}

impl Slots {
    pub fn new(n: usize) -> Arc<Self> {
        Arc::new(Self { free: Mutex::new(n), freed: Condvar::new() })
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        self.free.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits for a slot. Blocking; false if Stop was pressed meanwhile.
    pub fn acquire(&self, gate: &Gate) -> bool {
        let mut free = self.lock();
        loop {
            if gate.is_cancelled() {
                return false;
            }
            if *free > 0 {
                *free -= 1;
                return true;
            }
            free = self
                .freed
                .wait_timeout(free, Duration::from_millis(200))
                .map_or_else(|e| e.into_inner().0, |r| r.0);
        }
    }

    /// Takes up to `max` idle slots without waiting; returns how many.
    pub fn borrow_idle(&self, max: usize) -> usize {
        let mut free = self.lock();
        let n = (*free).min(max);
        *free -= n;
        n
    }

    pub fn release(&self, n: usize) {
        if n > 0 {
            *self.lock() += n;
            self.freed.notify_all();
        }
    }
}

/// Gives borrowed CPU slots back on drop, even on panic.
pub struct SlotGuard<'a> {
    slots: Option<&'a Slots>,
    extra: usize,
}

impl SlotGuard<'_> {
    /// Number of extra idle slots borrowed beyond the caller's own slot.
    pub fn extra(&self) -> usize {
        self.extra
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        if let Some(slots) = self.slots {
            slots.release(self.extra);
        }
    }
}

// ------------------------------------------------------------------- promises & big lossy gate

/// Memory promised to one job; given back when dropped (waking any waiting big lossy job).
pub struct Promise {
    amount: u64,
    counter: Arc<AtomicU64>,
    notify: Arc<BigLossyGate>,
}

impl Drop for Promise {
    fn drop(&mut self) {
        if self.amount > 0 {
            self.counter.fetch_sub(self.amount, Ordering::SeqCst);
            self.notify.notify();
        }
    }
}

struct BigLossyQueue {
    next_ticket: u64,
    waiting: VecDeque<u64>,
    active: usize,
}

/// Strictly-FIFO, memory-budgeted gate for big lossy searches (`need >= BIG_LOSSY_JOB`).
struct BigLossyGate {
    state: Mutex<BigLossyQueue>,
    changed: Condvar,
    promised: Arc<AtomicU64>,
}

/// Active turn in [`BigLossyGate`]; releases its memory promise and wakes waiters on drop.
pub struct LossyTurn {
    promise: Option<Promise>,
    gate: Arc<BigLossyGate>,
}

impl Drop for LossyTurn {
    fn drop(&mut self) {
        // Drop the memory promise first so waiters waking up see the freed budget at once.
        self.promise.take();
        let mut q = self.gate.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        q.active = q.active.saturating_sub(1);
        self.gate.changed.notify_all();
    }
}

struct TicketGuard<'a> {
    gate: &'a BigLossyGate,
    ticket: Option<u64>,
}

impl Drop for TicketGuard<'_> {
    fn drop(&mut self) {
        if let Some(t) = self.ticket.take() {
            let mut q = self.gate.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            q.waiting.retain(|&id| id != t);
            self.gate.changed.notify_all();
        }
    }
}

impl BigLossyGate {
    fn new(promised: Arc<AtomicU64>) -> Self {
        Self {
            state: Mutex::new(BigLossyQueue {
                next_ticket: 0,
                waiting: VecDeque::new(),
                active: 0,
            }),
            changed: Condvar::new(),
            promised,
        }
    }

    fn notify(&self) {
        self.changed.notify_all();
    }

    fn promise_on(self: &Arc<Self>, amount: u64, available: Option<u64>, keep: u64) -> Option<Promise> {
        let Some(available) = available else {
            return Some(Promise {
                amount: 0,
                counter: Arc::clone(&self.promised),
                notify: Arc::clone(self),
            });
        };
        let mut current = self.promised.load(Ordering::SeqCst);
        loop {
            if available.saturating_sub(current) < amount.saturating_add(keep) {
                return None;
            }
            match self.promised.compare_exchange(current, current.saturating_add(amount), Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    return Some(Promise {
                        amount,
                        counter: Arc::clone(&self.promised),
                        notify: Arc::clone(self),
                    });
                }
                Err(now) => current = now,
            }
        }
    }

    fn acquire_with(
        self: &Arc<Self>,
        need: u64,
        gate: &Gate,
        available_fn: impl Fn() -> Option<u64>,
        on_wait: impl Fn(),
    ) -> Result<Option<LossyTurn>, RunError> {
        let ticket = {
            let mut q = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let t = q.next_ticket;
            q.next_ticket += 1;
            q.waiting.push_back(t);
            t
        };
        // Declared before the mutex guard so `q` always drops before `TicketGuard`.
        let mut guard = TicketGuard { gate: self, ticket: Some(ticket) };
        let mut notified_wait = false;
        let mut q = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if gate.is_cancelled() {
                drop(q);
                return Err(RunError::Cancelled);
            }
            if gate.is_paused() {
                drop(q);
                if !gate.wait_while_paused() {
                    return Err(RunError::Cancelled);
                }
                q = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            }
            if q.waiting.front() == Some(&ticket) {
                let available = available_fn();
                if let Some(p) = self.promise_on(need, available, BIG_LOSSY_RESERVE) {
                    q.waiting.pop_front();
                    guard.ticket = None;
                    q.active += 1;
                    self.changed.notify_all();
                    drop(q);
                    return Ok(Some(LossyTurn { promise: Some(p), gate: Arc::clone(self) }));
                }
                if q.active == 0 && !big_lossy_fits(need, available) {
                    q.waiting.pop_front();
                    guard.ticket = None;
                    self.changed.notify_all();
                    drop(q);
                    return Ok(None);
                }
            }
            if !notified_wait {
                notified_wait = true;
                on_wait();
            }
            q = self
                .changed
                .wait_timeout(q, Duration::from_millis(250))
                .map_or_else(|e| e.into_inner().0, |r| r.0);
        }
    }
}

// ------------------------------------------------------------------- disk reservations & work dirs

/// The drive (or share) root of `path`, e.g. `F:\`.
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().last().filter(|r| r.has_root()).map(Path::to_path_buf)
}

/// Space a job holds on a drive while it runs (a video's new version written next to its
/// original, or a CPU job's work folder), released on drop.
pub struct DiskReservation {
    reserved: Arc<Mutex<Vec<(PathBuf, u64)>>>,
    root: PathBuf,
    bytes: u64,
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut reserved = self.reserved.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = reserved.iter().position(|(r, n)| *r == self.root && *n == self.bytes) {
            reserved.swap_remove(i);
        }
    }
}

/// The private work folder of one CPU job, deleted on drop together with its disk reservation.
pub struct WorkDir {
    pub path: PathBuf,
    _reservation: DiskReservation,
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        if let Some(parent) = self.path.parent().filter(|p| p.ends_with(DRIVE_WORK_DIR)) {
            let _ = std::fs::remove_dir(parent); // only if empty
        }
    }
}

// ------------------------------------------------------------------- pure helpers

/// Whether a big lossy search needing `need` bytes fits in `available` commit memory.
pub fn big_lossy_fits(need: u64, available: Option<u64>) -> bool {
    available.is_none_or(|a| a >= need.saturating_add(BIG_LOSSY_RESERVE))
}

/// Commit memory a PNG's lossless tools take on `threads` threads.
pub fn tool_memory(pixels: u64, threads: u64) -> u64 {
    pixels
        .saturating_mul(MT_BASE_BYTES_PER_PIXEL)
        .saturating_add((pixels.saturating_mul(MT_EXTRA_PER_THREAD_PER_100_PIXELS) / 100).saturating_mul(threads.saturating_sub(1)))
}

/// Threads a PNG's lossless tools may use with `free` commit memory (free now, less what
/// other jobs were promised): 0 = every core (the tool's default).
pub fn picture_threads(pixels: u64, cores: u64, free: Option<u64>) -> usize {
    let Some(free) = free.map(|a| a.saturating_sub(BIG_LOSSY_RESERVE)) else { return 0 };
    let per_thread = (pixels.saturating_mul(MT_EXTRA_PER_THREAD_PER_100_PIXELS) / 100).max(1);
    let base = tool_memory(pixels, 1);
    let threads = if free <= base { 1 } else { 1 + (free - base) / per_thread };
    if threads >= cores { 0 } else { usize::try_from(threads).unwrap_or(1).max(1) }
}

/// Free commit memory needed to let one more CPU job in, with `recent` admitted within
/// [`CPU_ADMISSION_WARMUP`].
pub fn image_admission_need(recent: usize) -> u64 {
    IMAGE_MIN_FREE_COMMIT + CPU_ADMISSION_RESERVE * recent as u64
}

/// Pure encoder admission check: the first running job always starts.
pub fn encoder_fits(running: usize, memory: Option<MemoryStatus>, free_vram_mb: Option<u64>) -> bool {
    if running == 0 {
        return true;
    }
    let ram = memory.is_none_or(|m| m.available_commit >= MIN_FREE_COMMIT && m.available_physical >= MIN_FREE_PHYSICAL);
    ram && free_vram_mb.is_none_or(|v| v >= MIN_FREE_VRAM_MB)
}

// ------------------------------------------------------------------- ResourceBudget

/// Instance-scoped coordinator for commit memory, CPU slots, big lossy turns, and disk space.
pub struct ResourceBudget {
    probe: Arc<dyn SystemProbe>,
    promised: Arc<AtomicU64>,
    big_lossy: Arc<BigLossyGate>,
    disk_reserved: Arc<Mutex<Vec<(PathBuf, u64)>>>,
    cpu_admitted: Mutex<Vec<Instant>>,
}

impl ResourceBudget {
    pub fn os() -> Arc<Self> {
        Self::with_probe(Arc::new(OsProbe))
    }

    pub fn with_probe(probe: Arc<dyn SystemProbe>) -> Arc<Self> {
        let promised = Arc::new(AtomicU64::new(0));
        let big_lossy = Arc::new(BigLossyGate::new(Arc::clone(&promised)));
        Arc::new(Self {
            probe,
            promised,
            big_lossy,
            disk_reserved: Arc::new(Mutex::new(Vec::new())),
            cpu_admitted: Mutex::new(Vec::new()),
        })
    }

    pub fn promised(&self) -> u64 {
        self.promised.load(Ordering::SeqCst)
    }

    /// Effective free commit memory: host available commit minus promises held by active jobs.
    fn effective_commit(&self, status: Option<MemoryStatus>) -> Option<u64> {
        status.map(|m| m.available_commit.saturating_sub(self.promised()))
    }

    fn effective_memory(&self) -> Option<MemoryStatus> {
        self.probe.memory_status().map(|m| MemoryStatus {
            available_commit: m.available_commit.saturating_sub(self.promised()),
            available_physical: m.available_physical,
        })
    }

    // ---- video & VMAF admission ----

    pub fn may_start_encoder(&self, running: usize, free_vram_mb: Option<u64>) -> (bool, Option<MemoryStatus>) {
        let mem = self.effective_memory();
        (encoder_fits(running, mem, free_vram_mb), mem)
    }

    pub fn encoder_plentiful(&self, jobs: u64, vram_plentiful: bool) -> bool {
        vram_plentiful && self.effective_memory().is_some_and(|m| m.available_commit >= MIN_FREE_COMMIT * jobs)
    }

    pub fn may_start_vmaf(&self, busy: usize) -> bool {
        busy == 0 || self.effective_memory().is_none_or(|m| m.available_commit >= VMAF_MIN_FREE_COMMIT)
    }

    // ---- CPU job admission ----

    /// Checks whether one more CPU job may start now. Does not record an admission (call
    /// [`Self::try_admit_cpu`] under the claim lock so admission and claim stay atomic).
    pub fn can_admit_cpu(&self, active_jobs: usize) -> (bool, Option<MemoryStatus>) {
        let mem = self.effective_memory();
        let mut admitted = self.cpu_admitted.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        admitted.retain(|t| now.duration_since(*t) < CPU_ADMISSION_WARMUP);
        let busy = active_jobs + admitted.len();
        let ok = busy == 0 || mem.is_none_or(|m| m.available_commit >= image_admission_need(admitted.len()));
        (ok, mem)
    }

    /// Records a CPU job admission timestamp (called under the claim lock once a file is claimed).
    pub fn record_cpu_admission(&self) {
        let mut admitted = self.cpu_admitted.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        admitted.push(Instant::now());
    }

    /// Before retrying a CPU job that hit OOM: wait up to 60 s for `2 * IMAGE_MIN_FREE_COMMIT`.
    pub fn wait_for_job_memory(&self, gate: &Gate) -> bool {
        for _ in 0..60 {
            if !gate.wait_while_paused() {
                return false;
            }
            if self.effective_memory().is_none_or(|m| m.available_commit >= 2 * IMAGE_MIN_FREE_COMMIT) {
                return true;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        !gate.is_cancelled()
    }

    // ---- CPU slot borrowing (comics, PDFs) ----

    /// Borrows up to `items - 1` idle CPU slots, capped by effective free commit memory
    /// (keeping `2 * per_item_commit` in reserve). Released automatically when [`SlotGuard`] drops.
    pub fn borrow_slots<'a>(&self, slots: Option<&'a Arc<Slots>>, items: usize, per_item_commit: u64) -> SlotGuard<'a> {
        let mut extra = 0;
        if let Some(s) = slots {
            let commit = self.effective_commit(self.probe.memory_status()).unwrap_or(u64::MAX);
            let affordable = usize::try_from(commit.saturating_sub(2 * per_item_commit) / per_item_commit.max(1)).unwrap_or(usize::MAX);
            extra = s.borrow_idle(items.saturating_sub(1).min(affordable));
        }
        SlotGuard { slots: slots.map(Arc::as_ref), extra }
    }

    // ---- picture threads & lossy search memory ----

    /// A PNG's lossless threads, promising its extra-thread memory for the lossless chain.
    pub fn picture_threads_promised(&self, pixels: u64, cores: u64) -> (usize, Option<Promise>) {
        for _ in 0..4 {
            let available = self.probe.memory_status().map(|m| m.available_commit);
            let threads = picture_threads(pixels, cores, available.map(|a| a.saturating_sub(self.promised())));
            let used = if threads == 0 { cores } else { threads as u64 };
            let extra = tool_memory(pixels, used).saturating_sub(tool_memory(pixels, 1));
            if extra == 0 {
                return (threads, None);
            }
            if let Some(p) = self.big_lossy.promise_on(extra, available, tool_memory(pixels, 1).saturating_add(BIG_LOSSY_RESERVE)) {
                return (threads, Some(p));
            }
        }
        (1, None)
    }

    /// How many lossy methods (`2..=max_k`) may search side by side for a picture of `pixels`
    /// pixels, promising the extra searches' memory for the duration of the search.
    pub fn parallel_methods(&self, pixels: u64, max_k: usize, base_promised: bool) -> Option<(usize, Promise)> {
        if max_k <= 1 {
            return None;
        }
        let one = pixels.saturating_mul(LOSSY_BYTES_PER_PIXEL);
        let available = self.probe.memory_status().map(|m| m.available_commit)?;
        let keep = if base_promised { BIG_LOSSY_RESERVE } else { one.saturating_add(BIG_LOSSY_RESERVE) };
        for k in (2..=max_k).rev() {
            if let Some(p) = self.big_lossy.promise_on(one.saturating_mul(k as u64 - 1), Some(available), keep) {
                return Some((k, p));
            }
        }
        None
    }

    /// Acquires a FIFO memory-budgeted [`LossyTurn`] for a big lossy search (`need >= BIG_LOSSY_JOB`).
    /// Returns `Ok(None)` when no other big search is active and commit memory cannot fit it alone.
    pub fn acquire_lossy_turn(&self, need: u64, gate: &Gate, on_wait: impl Fn()) -> Result<Option<LossyTurn>, RunError> {
        self.big_lossy.acquire_with(
            need,
            gate,
            || self.probe.memory_status().map(|m| m.available_commit),
            on_wait,
        )
    }

    // ---- disk reservations & work folders ----

    /// Reserves `bytes` on `path`'s drive if its free space, less active reservations there,
    /// leaves `keep` more. `Err(free)`: the drive's remaining free space for this job.
    pub fn reserve_disk(&self, path: &Path, bytes: u64, keep: u64) -> Result<DiskReservation, u64> {
        let Some(root) = volume_root(path) else {
            return Ok(DiskReservation { reserved: Arc::clone(&self.disk_reserved), root: PathBuf::new(), bytes: 0 });
        };
        let mut reserved = self.disk_reserved.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let taken: u64 = reserved.iter().filter(|(r, _)| *r == root).map(|(_, n)| n).sum();
        if let Some(free) = self.probe.free_space(path) {
            if free < taken + bytes + keep {
                return Err(free.saturating_sub(taken));
            }
        }
        reserved.push((root.clone(), bytes));
        Ok(DiskReservation { reserved: Arc::clone(&self.disk_reserved), root, bytes })
    }

    /// Selects the work base directory (`temp` first, else `<file_volume>\.mediaopti-temp`),
    /// invokes `prepare(&place)` to verify/create it, and reserves `need` bytes atomically.
    fn pick_work_place(&self, file: &Path, temp: &Path, need: u64, prepare: impl Fn(&Path) -> bool) -> Option<(PathBuf, DiskReservation)> {
        let mut places = vec![temp.to_path_buf()];
        if let Some(root) = volume_root(file) {
            places.push(root.join(DRIVE_WORK_DIR));
        }
        let mut reserved = self.disk_reserved.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for place in places {
            let Some(root) = volume_root(&place) else { continue };
            let taken: u64 = reserved.iter().filter(|(r, _)| *r == root).map(|(_, n)| n).sum();
            let fits = self.probe.free_space(&root).is_none_or(|free| free >= taken + need + DISK_RESERVE);
            if fits && prepare(&place) {
                reserved.push((root.clone(), need));
                let res = DiskReservation { reserved: Arc::clone(&self.disk_reserved), root, bytes: need };
                return Some((place, res));
            }
        }
        None
    }

    /// Creates a private work folder for a file of `size` bytes (reserving `size * factor + 64 MB`),
    /// trying `temp` first and falling back to the file's own drive when `temp` is short on space.
    pub fn create_work_dir(&self, path: &Path, size: u64, factor: u64, temp: &Path, name: &str) -> Result<WorkDir, String> {
        let need = size.saturating_mul(factor).saturating_add(64 << 20);
        let Some((place, reservation)) = self.pick_work_place(path, temp, need, |place| {
            if std::fs::create_dir_all(place.join(name)).is_ok() {
                if place.ends_with(DRIVE_WORK_DIR) {
                    system::set_hidden(place);
                }
                true
            } else {
                false
            }
        }) else {
            return Err("not enough free disk space for its temporary files (it will be tried next time)".into());
        };
        Ok(WorkDir { path: place.join(name), _reservation: reservation })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const GB: u64 = 1 << 30;

    struct FakeProbe {
        mem: Mutex<Option<MemoryStatus>>,
        space: Mutex<HashMap<PathBuf, u64>>,
    }

    impl FakeProbe {
        fn new(commit_gb: u64, phys_gb: u64) -> Arc<Self> {
            Arc::new(Self {
                mem: Mutex::new(Some(MemoryStatus {
                    available_commit: commit_gb * GB,
                    available_physical: phys_gb * GB,
                })),
                space: Mutex::new(HashMap::new()),
            })
        }

        fn with_disk(self: Arc<Self>, root: &str, bytes: u64) -> Arc<Self> {
            self.space.lock().unwrap().insert(PathBuf::from(root), bytes);
            self
        }

        fn set_commit(&self, commit: Option<u64>) {
            *self.mem.lock().unwrap() = commit.map(|c| MemoryStatus { available_commit: c, available_physical: c });
        }
    }

    impl SystemProbe for FakeProbe {
        fn memory_status(&self) -> Option<MemoryStatus> {
            *self.mem.lock().unwrap()
        }

        fn free_space(&self, path: &Path) -> Option<u64> {
            let root = volume_root(path)?;
            self.space.lock().unwrap().get(&root).copied()
        }
    }

    fn mem(commit_gb: u64, phys_gb: u64) -> Option<MemoryStatus> {
        Some(MemoryStatus { available_commit: commit_gb * GB, available_physical: phys_gb * GB })
    }

    #[test]
    fn memory_admission() {
        assert!(encoder_fits(0, mem(1, 1), Some(100)));
        assert!(!encoder_fits(1, mem(4, 8), None));
        assert!(!encoder_fits(1, mem(10, 1), None));
        assert!(encoder_fits(1, mem(10, 4), None));
        assert!(encoder_fits(3, None, None));
        assert!(!encoder_fits(1, mem(10, 4), Some(2048)));
        assert!(encoder_fits(1, mem(10, 4), Some(4096)));
    }

    #[test]
    fn picture_threads_follow_the_memory() {
        assert_eq!(picture_threads(31_200_000, 16, Some(4 * GB)), 0, "1.3 GB past the reserve: every core");
        assert_eq!(picture_threads(31_200_000, 16, Some(5 * GB / 2)), 1, "only 0.5 GB past it: one thread's base");
        assert_eq!(tool_memory(31_200_000, 16) / (1 << 20), 1286, "about the 1.33 GB measured");
        assert_eq!(tool_memory(u64::MAX, 200), u64::MAX, "a damaged header can't overflow");
        assert_eq!(picture_threads(100_000_000, 16, Some(6 * GB)), 15, "4 GB past the reserve");
        assert_eq!(picture_threads(100_000_000, 16, Some(12 * GB)), 0, "enough for all");
        assert_eq!(picture_threads(100_000_000, 16, Some(3 * GB)), 1, "less than one thread's base");
        assert_eq!(picture_threads(100_000_000, 16, None), 0, "unknown memory: as the tools default");
        let some = picture_threads(160_000_000, 16, Some(8 * GB));
        assert!((2..16).contains(&some), "{some}");
    }

    #[test]
    fn a_burst_of_cpu_jobs_is_spread() {
        assert_eq!(image_admission_need(0), IMAGE_MIN_FREE_COMMIT);
        assert_eq!(image_admission_need(4), IMAGE_MIN_FREE_COMMIT + 4 * CPU_ADMISSION_RESERVE);
        assert!(image_admission_need(14) < 10 * GB);
    }

    #[test]
    fn disk_reservations_share_a_drive() {
        let probe = FakeProbe::new(16, 16).with_disk(r"C:\", 100 * GB);
        let budget = ResourceBudget::with_probe(probe);
        let first = budget.reserve_disk(Path::new(r"C:\work\a.mp4"), 60 * GB, 0).unwrap();
        assert_eq!(budget.reserve_disk(Path::new(r"C:\work\b.mp4"), 60 * GB, 0).err(), Some(40 * GB));
        drop(first);
        assert!(budget.reserve_disk(Path::new(r"C:\work\b.mp4"), 60 * GB, 0).is_ok());
    }

    #[test]
    fn work_folder_moves_to_the_files_drive_when_temp_is_short() {
        let probe = FakeProbe::new(16, 16)
            .with_disk(r"C:\", 2 * GB)
            .with_disk(r"D:\", 50 * GB);
        let budget = ResourceBudget::with_probe(probe);
        let file = Path::new(r"D:\mediaopti-test\some file.wav");
        let temp = Path::new(r"C:\Users\me\AppData\Local\Temp");

        let (place, res) = budget.pick_work_place(file, temp, 30 * GB, |_| true).expect("fits on D:");
        assert_eq!(place, PathBuf::from(r"D:\.mediaopti-temp"));
        // A second 30 GB job does not fit on either drive while the first holds its reservation.
        assert!(budget.pick_work_place(file, temp, 30 * GB, |_| true).is_none());
        drop(res);
        assert!(budget.pick_work_place(file, temp, 30 * GB, |_| true).is_some());
    }

    #[test]
    fn big_lossy_searches_need_their_memory_free() {
        let need = std::hint::black_box(13_481 * 12_136) * LOSSY_BYTES_PER_PIXEL;
        assert!(need > 22 * GB, "a big job");
        assert!(!big_lossy_fits(need, Some(8 * GB)));
        assert!(big_lossy_fits(need, Some(40 * GB)));
        assert!(big_lossy_fits(need, None));
        assert!(std::hint::black_box(6000 * 4000) * LOSSY_BYTES_PER_PIXEL < BIG_LOSSY_JOB);
    }

    #[test]
    fn big_lossy_gate_is_fifo_and_memory_budgeted() {
        let probe = FakeProbe::new(8, 8);
        let budget = ResourceBudget::with_probe(Arc::clone(&probe) as Arc<dyn SystemProbe>);
        let g = Gate::default();

        // 1) Immediate refusal when active == 0 and raw commit cannot fit need + 2 GB reserve.
        let refused = budget.acquire_lossy_turn(20 * GB, &g, || {}).unwrap();
        assert!(refused.is_none(), "20 GB job must refuse immediately on 8 GB free");

        // 2) Concurrent admission: two 5 GB jobs fit in 20 GB free (5 + 5 + 2 <= 20).
        probe.set_commit(Some(20 * GB));
        let t1 = budget.acquire_lossy_turn(5 * GB, &g, || {}).unwrap().expect("first fits");
        let t2 = budget.acquire_lossy_turn(5 * GB, &g, || {}).unwrap().expect("second fits concurrently");
        assert_eq!(budget.promised(), 10 * GB);
        // CPU slot borrowing sees the 10 GB promised (20 - 10 = 10 GB effective -> 8 extra slots affordable).
        let slots = Slots::new(10);
        let guard = budget.borrow_slots(Some(&slots), 10, PAGE_COMMIT);
        assert_eq!(guard.extra(), 8);
        drop(guard);
        drop((t1, t2));
        assert_eq!(budget.promised(), 0);

        // 3) Tight memory (9 GB free): one 5 GB job runs, the next two queue in strict FIFO,
        //    and a cancelled middle waiter unblocks the one behind it.
        probe.set_commit(Some(9 * GB));
        let first = budget.acquire_lossy_turn(5 * GB, &g, || {}).unwrap().expect("first runs");
        let order = Arc::new(Mutex::new(Vec::<u32>::new()));
        let waited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let g_cancel = Gate::new(tokio_util::sync::CancellationToken::new(), None);
        let cancel_tok = g_cancel.cancel.clone();

        let wait_until_queued = |n: usize| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while budget.big_lossy.state.lock().unwrap().waiting.len() < n && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(budget.big_lossy.state.lock().unwrap().waiting.len(), n);
        };

        let (b_a, b_b, b_c) = (Arc::clone(&budget), Arc::clone(&budget), Arc::clone(&budget));
        let (order_a, order_c) = (Arc::clone(&order), Arc::clone(&order));
        let waited_a = Arc::clone(&waited);

        let h_a = std::thread::spawn(move || {
            let _t = b_a
                .acquire_lossy_turn(5 * GB, &Gate::default(), || {
                    waited_a.store(true, Ordering::SeqCst);
                })
                .unwrap()
                .expect("a runs after first");
            order_a.lock().unwrap().push(1);
            std::thread::sleep(Duration::from_millis(30));
        });
        wait_until_queued(1);

        let h_b = std::thread::spawn(move || b_b.acquire_lossy_turn(5 * GB, &g_cancel, || {}));
        wait_until_queued(2);

        let h_c = std::thread::spawn(move || {
            let _t = b_c.acquire_lossy_turn(5 * GB, &Gate::default(), || {}).unwrap().expect("c runs after a");
            order_c.lock().unwrap().push(3);
        });
        wait_until_queued(3);

        assert!(waited.load(Ordering::SeqCst), "wait callback fired");
        cancel_tok.cancel();
        assert!(matches!(h_b.join().unwrap(), Err(RunError::Cancelled)));

        drop(first);
        h_a.join().unwrap();
        h_c.join().unwrap();
        assert_eq!(*order.lock().unwrap(), vec![1, 3], "strict FIFO order preserved");
    }

    #[test]
    fn slots_are_borrowed_and_returned() {
        let s = Slots::new(3);
        let g = Gate::default();
        assert!(s.acquire(&g));
        assert_eq!(s.borrow_idle(5), 2);
        assert_eq!(s.borrow_idle(1), 0);
        s.release(2);
        assert_eq!(s.borrow_idle(1), 1);
        s.release(2);
    }
}
