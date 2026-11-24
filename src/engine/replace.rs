//! Temp file names and the shared step that replaces an original with its new version.

use super::*;
use std::collections::HashMap;

pub(super) fn is_generated_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map_or_default(|n| n.to_string_lossy().to_lowercase());
    is_orphan_temp(path) || name.contains(".tmp.") || name.contains("-h264.")
}

/// What became of a leftover temp file.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Settled {
    /// A half-made version whose original is still in place: removed.
    Removed,
    /// The new version of an original that had already been set aside: given its name.
    Recovered(PathBuf),
    /// Neither could be told for sure: left as it is (never deleted on a guess).
    LeftAlone,
}

/// Settles a leftover temp file found by a scan (the app stopped while making or placing it).
///
/// A temp in the replacement journal had been verified and flushed before its original was
/// touched: if that exact original is still there it was never set aside, so the temp goes;
/// otherwise the temp IS the file now and gets the name it was meant to have.
///
/// Without a journal entry (the app stopped before replacing, or a version before 3.1 wrote
/// it), the temp goes only when a plausible original is beside it: same stem, and a file the
/// same job could have come from (a video for a video temp; the same kind of file, picture,
/// audio or document, for the others). A same-stem sibling of another kind (`IMG_1.JPG` beside
/// a video's temp, `album.cue` beside a FLAC's) doesn't count. With no original, a temp that
/// `complete` accepts (ffmpeg can read it) is given the stem's name; an unreadable video,
/// audio or picture temp is half-made and goes; a document's temp is left alone, and so is
/// any temp `complete` couldn't check (`None`: ffmpeg didn't run).
pub(super) fn settle_orphan_temp(temp: &Path, complete: &dyn Fn(&Path) -> Option<bool>) -> std::io::Result<Settled> {
    let size = std::fs::metadata(temp)?.len();
    if let Some(p) = pending_for(temp).filter(|p| p.size == size) {
        if p.original.is_file() {
            system::remove_file_if_exists(temp)?;
            journal_end(temp);
            return Ok(Settled::Removed);
        }
        let target = free_target(&p.target)?;
        std::fs::rename(temp, &target)?;
        journal_end(temp);
        return Ok(Settled::Recovered(target));
    }
    let name = temp.file_name().map_or_default(|n| n.to_string_lossy().into_owned());
    // ASCII lowercase keeps the byte offsets, so the cut always lands on a character boundary.
    let lower = name.to_ascii_lowercase();
    let cut = lower.find(".mediaopti.").or_else(|| lower.find(".batchviproc.")).unwrap_or(0);
    let stem = &name[..cut];
    if stem.is_empty() {
        return Ok(Settled::LeftAlone);
    }
    let video_temp = lower[cut..].starts_with(".mediaopti.w") || lower[cut..].starts_with(".batchviproc.");
    let dir = temp.parent().unwrap_or(Path::new("."));
    let original_exists = std::fs::read_dir(dir)?.filter_map(Result::ok).any(|e| {
        let p = e.path();
        p != temp
            && !is_orphan_temp(&p)
            && p.file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(stem))
            && could_be_source(&p, temp, video_temp)
            && e.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
    });
    if original_exists {
        system::remove_file_if_exists(temp)?;
        return Ok(Settled::Removed);
    }
    match complete(temp) {
        Some(true) => {}
        // Not checked: nothing to go by, so nothing is deleted.
        None => return Ok(Settled::LeftAlone),
        Some(false) => {
            // A video, audio or picture temp ffmpeg can't read is half-made: an older version
            // set an original aside only after its new version was verified (and readable). A
            // document's temp can't be checked that way: never deleted on a guess.
            let document = image::kind_of(temp).is_some_and(image::Kind::is_document);
            if document {
                return Ok(Settled::LeftAlone);
            }
            system::remove_file_if_exists(temp)?;
            return Ok(Settled::Removed);
        }
    }
    let ext = temp.extension().map_or_default(|e| e.to_string_lossy().into_owned());
    let target = free_target(&dir.join(format!("{stem}.{ext}")))?;
    std::fs::rename(temp, &target)?;
    Ok(Settled::Recovered(target))
}

/// `target`, or an unused variant of it when another file has the name (an empty placeholder
/// left by the same interruption is removed and its name used).
fn free_target(target: &Path) -> std::io::Result<PathBuf> {
    if std::fs::metadata(target).is_ok_and(|m| m.len() == 0) {
        system::remove_file_if_exists(target)?;
        return Ok(target.to_path_buf());
    }
    Ok(system::unused_name(target))
}

/// Whether `file` could be the original the job writing `temp` started from.
fn could_be_source(file: &Path, temp: &Path, video_temp: bool) -> bool {
    crate::format::could_be_source(file, temp, video_temp)
}

/// A replacement that had started (its new version verified, flushed and noted) and had not
/// finished when the app last stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pending {
    pub temp: PathBuf,
    pub size: u64,
    pub original: PathBuf,
    pub target: PathBuf,
}

impl Pending {
    fn line(&self) -> String {
        format!("+\t{}\t{}\t{}\t{}\n", self.size, self.temp.display(), self.original.display(), self.target.display())
    }
}

/// The replacement journal (`pending-replacements.txt` in the data folder): a durable
/// `+ size temp original target` line before an original is set aside, `- temp` once that
/// replacement is over. The entries without their `-` are what a crash interrupted. It is
/// emptied whenever nothing is pending and compacted every `COMPACT_EVERY` endings, so a long
/// run doesn't grow it.
#[derive(Default)]
struct Journal {
    path: Option<PathBuf>,
    live: HashMap<String, Pending>,
    ended: usize,
}

const COMPACT_EVERY: usize = 1000;

static JOURNAL: std::sync::LazyLock<Mutex<Journal>> = std::sync::LazyLock::new(Default::default);

fn journal() -> std::sync::MutexGuard<'static, Journal> {
    JOURNAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn journal_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn pending_for(temp: &Path) -> Option<Pending> {
    journal().live.get(&journal_key(temp)).cloned()
}

/// Whether `temp` is a new version kept for its name: its replacement failed after the
/// original was set aside (in the journal; the next scan names it). Never to be removed.
pub(super) fn kept_for_its_name(temp: &Path) -> bool {
    pending_for(temp).is_some()
}

/// The temp name `name(id)` for a job, unless a new version kept for its name has it (a file
/// with the same stem and output type in that folder, earlier in this batch): then the same
/// name with a higher number (+10000; still a name `is_orphan_temp` knows).
pub(super) fn unclaimed_temp(name: impl Fn(u32) -> PathBuf, id: u32) -> PathBuf {
    let mut n = id;
    loop {
        let candidate = name(n);
        if !kept_for_its_name(&candidate) {
            return candidate;
        }
        n += 10_000;
    }
}

impl Journal {
    /// Rewrites the file with only the pending entries. Emptying it needn't be durable: what a
    /// lost truncation leaves are entries whose temps are gone, dropped at the next start.
    fn compact(&mut self) {
        self.ended = 0;
        if let Some(path) = &self.path {
            if self.live.is_empty() {
                let _ = std::fs::write(path, b"");
            } else {
                let text: String = self.live.values().map(Pending::line).collect();
                let _ = history::write_atomically(path, text.as_bytes());
            }
        }
    }
}

/// Whether the volume/drive holding `path` is currently mounted (if a removable drive is
/// unplugged, we must NOT drop its pending journal entry on startup).
fn volume_mounted(path: &Path) -> bool {
    match crate::budget::volume_root(path) {
        Some(root) => root.is_dir(),
        None => true,
    }
}

/// Sets the journal (once per process; later calls are ignored) and loads what earlier runs
/// left unfinished. Entries whose temp no longer exists are dropped only when their drive
/// is mounted (an unplugged drive's entry stays until the drive returns).
pub(super) fn init_pending_journal(path: &Path) {
    let mut j = journal();
    if j.path.is_some() {
        return;
    }
    j.path = Some(path.to_path_buf());
    let text = std::fs::read(path).map_or_default(|b| String::from_utf8_lossy(&b).into_owned());
    j.live = parse_journal(&text);
    j.live.retain(|_, p| p.temp.is_file() || !volume_mounted(&p.temp));
    if !text.is_empty() {
        j.compact();
    }
}

/// The entries of a journal's text that have no ending line (a damaged line is skipped).
fn parse_journal(text: &str) -> HashMap<String, Pending> {
    let mut live = HashMap::new();
    for line in text.lines() {
        match line.split('\t').collect::<Vec<_>>().as_slice() {
            ["+", size, temp, original, target] => {
                let Ok(size) = size.parse() else { continue };
                let entry = Pending { temp: PathBuf::from(temp), size, original: PathBuf::from(original), target: PathBuf::from(target) };
                live.insert(journal_key(Path::new(temp)), entry);
            }
            ["-", temp] => {
                live.remove(&journal_key(Path::new(temp)));
            }
            _ => {}
        }
    }
    live
}

/// Notes, durably, that `temp` is about to replace `original` (as `target`).
fn journal_start(temp: &Path, original: &Path, target: &Path) -> std::io::Result<()> {
    let entry = Pending { temp: temp.to_path_buf(), size: std::fs::metadata(temp)?.len(), original: original.to_path_buf(), target: target.to_path_buf() };
    let mut j = journal();
    if let Some(path) = &j.path {
        history::append_lines(path, &entry.line(), true)?;
    }
    j.live.insert(journal_key(temp), entry);
    Ok(())
}

/// Notes that `temp`'s replacement is over (placed, or removed with its original kept). Not
/// durable: a lost line only leaves an entry whose temp is gone, dropped at the next start.
fn journal_end(temp: &Path) {
    let mut j = journal();
    if j.live.remove(&journal_key(temp)).is_none() {
        return;
    }
    j.ended += 1;
    if j.live.is_empty() || j.ended >= COMPACT_EVERY {
        j.compact();
    } else if let Some(path) = &j.path {
        let _ = history::append_lines(path, &format!("-\t{}\n", temp.display()), false);
    }
}

/// Temp outputs of this app (`clip.mediaopti.w1.mp4`, `photo.mediaopti.i3.png`), including
/// ones left by batchviproc (`clip.batchviproc.w1.mp4`). Only this exact shape matches, so a
/// user's own file that merely contains ".mediaopti." in its name is never deleted.
pub(super) fn is_orphan_temp(path: &Path) -> bool {
    static TEMP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\.(mediaopti\.[wi]|batchviproc\.w)\d+\.[a-z0-9]+$").unwrap()
    });
    path.file_name().is_some_and(|n| TEMP.is_match(&n.to_string_lossy()))
}

/// Replacements running now: quitting waits for them (an original already in the Recycle Bin
/// with its new version not yet renamed must not be cut off).
pub(super) static REPLACING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) struct ReplacingGuard;

impl ReplacingGuard {
    fn new() -> Self {
        REPLACING.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ReplacingGuard {
    fn drop(&mut self) {
        REPLACING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Puts the new version `tmp` in place of `original` (as `final_target`), disposing of the
/// original as `disposal` says. On failure the error says what happened; if the original was
/// already set aside (Recycle Bin or backup folder), the new version is never deleted.
/// Blocking. Shared by the video and image workers.
///
/// Before the original is touched, the new version is flushed to the disk, `note` records
/// its content in the history and the replacement journal records what `tmp` is for (both
/// durably): after a crash, a power cut or a forced quit at any point, the file at the
/// original's name is either the untouched original or a complete new version that the next
/// scan recognises as done (never optimized a second time), and a new version left under its
/// temp name gets the right name at the next scan (`settle_orphan_temp`).
pub(super) fn replace_original(tmp: &Path, final_target: &Path, original: &Path, disposal: &system::Disposal, note: &dyn Fn(&Path) -> std::io::Result<()>) -> Result<(), String> {
    let _replacing = ReplacingGuard::new();
    // The Mark of the Web and other streams the tools didn't carry over.
    if let Err(err) = system::copy_alternate_streams(original, tmp) {
        let _ = system::remove_file_if_exists(tmp);
        return Err(format!("could not copy its alternate data streams: {err}"));
    }
    if let Err(err) = system::flush_to_disk(tmp) {
        let _ = system::remove_file_if_exists(tmp);
        return Err(format!("could not write the new version to the disk: {err}"));
    }
    if let Err(err) = note(tmp) {
        let _ = system::remove_file_if_exists(tmp);
        return Err(format!("could not note it in the history: {err}"));
    }
    if let Err(err) = journal_start(tmp, original, final_target) {
        let _ = system::remove_file_if_exists(tmp);
        return Err(format!("could not note the replacement: {err}"));
    }
    system::die_here_in_tests("noted");
    let Err(err) = system::replace_file_with_retry(tmp, final_target, original, disposal) else {
        journal_end(tmp);
        return Ok(());
    };
    if original.exists() {
        let _ = system::remove_file_if_exists(tmp);
        journal_end(tmp);
        return Err(format!("could not replace it: {err}"));
    }
    let gone_to = match disposal {
        system::Disposal::RecycleBin => "is in the Recycle Bin".to_string(),
        system::Disposal::MoveTo(dest) => format!("was moved to {}", dest.parent().unwrap_or(dest).display()),
        system::Disposal::Delete => "was deleted".to_string(),
    };
    Err(format!(
        "could not finish replacing it: {err}. The original {gone_to} and the new version is kept as {} (it gets its name at the next scan of this folder)",
        tmp.display()
    ))
}

impl Engine {
    pub(super) fn log_remove(&self, path: &Path) {
        if let Err(err) = system::remove_file_if_exists(path) {
            // Error 32: still open, mostly by an ffmpeg that is quitting after Stop. The next
            // scan's cleanup (`settle_orphan_temp`) removes it.
            let when = if err.raw_os_error() == Some(32) { "still open by another process" } else { "error" };
            self.log(format!(
                "Could not remove temporary file {} ({when}: {err}); the next scan removes it",
                path.display()
            ));
        }
    }
}

// ------------------------------------------------------------------- commit transaction

/// The candidate file produced by an optimization pass, ready to commit.
pub(super) enum Candidate<'a> {
    /// Already sits next to the original on the same volume (videos: ffmpeg wrote directly).
    /// Removed on abort/discard unless kept in the pending journal for crash recovery.
    SiblingTemp { path: &'a Path, size: u64 },
    /// Sits in a private work folder (images/audio/docs); copied next to the target right
    /// before [`replace_original`].
    WorkFile { path: &'a Path, size: u64, worker_id: u32 },
}

impl Candidate<'_> {
    fn size(&self) -> u64 {
        match *self {
            Self::SiblingTemp { size, .. } | Self::WorkFile { size, .. } => size,
        }
    }

    fn discard_temp(&self, warn: &dyn Fn(String)) {
        if let Self::SiblingTemp { path, .. } = *self {
            if !kept_for_its_name(path) {
                if let Err(err) = system::remove_file_if_exists(path) {
                    let when = if err.raw_os_error() == Some(32) { "still open by another process" } else { "error" };
                    warn(format!(
                        "Could not remove temporary file {} ({when}: {err}); the next scan removes it",
                        path.display()
                    ));
                }
            }
        }
    }
}

/// First step of claiming a new target name (when `desired_target != original`).
pub(super) enum ClaimStep {
    /// Target name is ready (`claimed`: true when an empty placeholder was created at `target`).
    Ready { target: PathBuf, claimed: bool },
    /// `desired_target` already exists: caller should check whether it holds the same content.
    Collision(PathBuf),
}

/// Outcome of committing an optimization candidate to disk.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommitOutcome {
    /// Replaced original (in place or at a newly claimed target name).
    Replaced { target: PathBuf, renamed: bool, size: u64 },
    /// Extension-fix rename only (no smaller candidate was produced).
    Renamed(PathBuf),
    /// Existing file at target had the same content and our candidate replaced it; original disposed.
    DuplicateReplaced { existing: PathBuf, saved_bytes: u64, final_size: u64 },
    /// Existing file at target had the same content and was already smaller/equal; original disposed.
    DuplicateKeptExisting { existing: PathBuf, saved_bytes: u64, final_size: u64 },
    /// Existing file at target had the same content, was bigger than ours, and could not be replaced
    /// (hard link, or no smaller candidate): both files kept untouched (Option A).
    DuplicateKeptBoth { existing: PathBuf, hard_linked: bool },
    /// Existing file at target has the same content and another worker is processing it now.
    BusyDuplicate(PathBuf),
    /// Original changed on disk while being processed (or during same-content check).
    OriginalChanged,
    /// Could not find an unused numbered name after 20 attempts.
    NoFreeName,
    /// I/O or replacement error (original untouched unless message notes otherwise).
    Failed(String),
}

/// Phase 1: if `desired_target == original`, returns `Ready { claimed: false }`; otherwise tries
/// to atomically claim `desired_target`, returning `Collision` if a file already holds that name.
pub(super) fn try_claim_first(original: &Path, desired_target: &Path) -> ClaimStep {
    if desired_target == original {
        return ClaimStep::Ready { target: original.to_path_buf(), claimed: false };
    }
    if system::claim_new_name(desired_target).is_ok() {
        ClaimStep::Ready { target: desired_target.to_path_buf(), claimed: true }
    } else {
        ClaimStep::Collision(desired_target.to_path_buf())
    }
}

/// Phase 1b (after a collision was confirmed NOT to be the same content): claims the first free
/// numbered variant `"stem (2).ext"` .. up to 20 tries.
pub(super) fn claim_numbered(initial_target: &Path) -> Option<PathBuf> {
    let mut target = system::unused_name(initial_target);
    for _ in 0..20 {
        if system::claim_new_name(&target).is_ok() {
            return Some(target);
        }
        target = system::unused_name(&target);
    }
    None
}

/// Phase 2a: commits a candidate to a claimed (or in-place) target after re-checking `file_state`.
#[allow(clippy::too_many_arguments)]
pub(super) fn commit_claimed(
    original: &Path,
    target: &Path,
    claimed: bool,
    initial_state: Option<(u64, Option<std::time::SystemTime>)>,
    candidate: Option<Candidate<'_>>,
    disposal: &system::Disposal,
    note: &dyn Fn(&Path) -> std::io::Result<()>,
    warn: &dyn Fn(String),
) -> CommitOutcome {
    let unclaim = || {
        if claimed {
            system::release_claim(target);
        }
    };
    if file_state(original) != initial_state {
        unclaim();
        if let Some(c) = &candidate {
            c.discard_temp(warn);
        }
        return CommitOutcome::OriginalChanged;
    }
    let Some(cand) = candidate else {
        return match std::fs::rename(original, target) {
            Ok(()) => CommitOutcome::Renamed(target.to_path_buf()),
            Err(err) => {
                unclaim();
                CommitOutcome::Failed(format!("could not rename it: {err}"))
            }
        };
    };
    let attrs = match std::fs::metadata(original) {
        Ok(m) => OriginalAttributes::from_metadata(&m),
        Err(err) => {
            unclaim();
            cand.discard_temp(warn);
            return CommitOutcome::Failed(err.to_string());
        }
    };
    let sibling = match cand {
        Candidate::SiblingTemp { path, .. } => path.to_path_buf(),
        Candidate::WorkFile { path, worker_id, .. } => {
            let stem = original.file_stem().map_or_default(|s| s.to_string_lossy().into_owned());
            let out_ext = args::ext_lower(target);
            let s = unclaimed_temp(|n| original.with_file_name(format!("{stem}.mediaopti.i{n}.{out_ext}")), worker_id);
            if let Err(err) = std::fs::copy(path, &s) {
                let _ = system::remove_file_if_exists(&s);
                unclaim();
                return CommitOutcome::Failed(format!("could not write next to the original: {err}"));
            }
            s
        }
    };
    if let Err(err) = replace_original(&sibling, target, original, disposal, note) {
        unclaim();
        if !kept_for_its_name(&sibling) {
            let _ = system::remove_file_if_exists(&sibling);
        }
        return CommitOutcome::Failed(err);
    }
    if let Err(err) = system::restore_attributes(target, &attrs) {
        warn(format!("Could not restore the dates and attributes of {}: {err}", target.display()));
    }
    CommitOutcome::Replaced { target: target.to_path_buf(), renamed: claimed, size: cand.size() }
}

/// Phase 2b: resolves a collision where `existing` holds the same content as `original`.
///
/// Always re-checks `file_state(original)` after the (potentially slow) content comparison,
/// respects `is_busy(existing)`, and applies Option A: when `existing` is a hard link bigger
/// than our candidate, keeps BOTH files untouched rather than deleting the original.
#[allow(clippy::too_many_arguments)]
pub(super) fn commit_same_content(
    original: &Path,
    existing: &Path,
    initial_state: Option<(u64, Option<std::time::SystemTime>)>,
    before_size: u64,
    candidate: Option<Candidate<'_>>,
    disposal: &system::Disposal,
    is_busy: &dyn Fn(&Path) -> bool,
    note: &dyn Fn(&Path) -> std::io::Result<()>,
    warn: &dyn Fn(String),
) -> CommitOutcome {
    if is_busy(existing) {
        if let Some(c) = &candidate {
            c.discard_temp(warn);
        }
        return CommitOutcome::BusyDuplicate(existing.to_path_buf());
    }
    if file_state(original) != initial_state {
        if let Some(c) = &candidate {
            c.discard_temp(warn);
        }
        return CommitOutcome::OriginalChanged;
    }
    let theirs = match std::fs::metadata(existing) {
        Ok(m) => m.len(),
        Err(err) => {
            if let Some(c) = &candidate {
                c.discard_temp(warn);
            }
            return CommitOutcome::Failed(err.to_string());
        }
    };
    let replaceable = system::hard_link_count(existing).unwrap_or(1) <= 1;
    match candidate {
        Some(cand) if cand.size() < theirs && replaceable => {
            let attrs = std::fs::metadata(original).map(|m| OriginalAttributes::from_metadata(&m));
            let sibling = match cand {
                Candidate::SiblingTemp { path, .. } => path.to_path_buf(),
                Candidate::WorkFile { path, worker_id, .. } => {
                    let stem = existing.file_stem().map_or_default(|s| s.to_string_lossy().into_owned());
                    let ext = args::ext_lower(existing);
                    let s = unclaimed_temp(|n| existing.with_file_name(format!("{stem}.mediaopti.i{n}.{ext}")), worker_id);
                    if let Err(err) = std::fs::copy(path, &s) {
                        let _ = system::remove_file_if_exists(&s);
                        return CommitOutcome::Failed(format!("could not write next to the original: {err}"));
                    }
                    s
                }
            };
            if let Err(err) = replace_original(&sibling, existing, existing, &disposal.for_sibling(existing), note) {
                if !kept_for_its_name(&sibling) {
                    let _ = system::remove_file_if_exists(&sibling);
                }
                return CommitOutcome::Failed(err);
            }
            if let Err(err) = attrs.and_then(|a| system::restore_attributes(existing, &a)) {
                warn(format!("Could not restore the dates and attributes of {}: {err}", existing.display()));
            }
            match system::dispose(original, disposal) {
                Ok(()) => CommitOutcome::DuplicateReplaced {
                    existing: existing.to_path_buf(),
                    saved_bytes: before_size + theirs - cand.size(),
                    final_size: cand.size(),
                },
                Err(err) => CommitOutcome::Failed(format!(
                    "{} now has the smaller version, but the original could not be removed: {err}",
                    existing.display()
                )),
            }
        }
        cand if theirs <= cand.as_ref().map_or(before_size, Candidate::size) => {
            if let Some(c) = &cand {
                c.discard_temp(warn);
            }
            match system::dispose(original, disposal) {
                Ok(()) => CommitOutcome::DuplicateKeptExisting {
                    existing: existing.to_path_buf(),
                    saved_bytes: before_size,
                    final_size: theirs,
                },
                Err(err) => CommitOutcome::Failed(format!("could not remove it (same content as {}): {err}", existing.display())),
            }
        }
        cand => {
            // Option A: existing copy is bigger and either hard-linked (replacing would break
            // its other names) or no smaller candidate was produced -> keep both untouched.
            if let Some(c) = &cand {
                c.discard_temp(warn);
            }
            CommitOutcome::DuplicateKeptBoth { existing: existing.to_path_buf(), hard_linked: !replaceable }
        }
    }
}

/// Synchronous all-in-one commit transaction (used by CPU workers and unit tests).
#[allow(clippy::too_many_arguments)]
pub(super) fn commit_candidate<E>(
    original: &Path,
    desired_target: &Path,
    initial_state: Option<(u64, Option<std::time::SystemTime>)>,
    before_size: u64,
    candidate: Option<Candidate<'_>>,
    disposal: &system::Disposal,
    is_busy: &dyn Fn(&Path) -> bool,
    mut same_content: impl FnMut(&Path) -> Result<bool, E>,
    note: &dyn Fn(&Path) -> std::io::Result<()>,
    warn: &dyn Fn(String),
) -> Result<CommitOutcome, E> {
    if file_state(original) != initial_state {
        if let Some(c) = &candidate {
            c.discard_temp(warn);
        }
        return Ok(CommitOutcome::OriginalChanged);
    }
    let (target, claimed) = match try_claim_first(original, desired_target) {
        ClaimStep::Ready { target, claimed } => (target, claimed),
        ClaimStep::Collision(existing) => {
            if same_content(&existing)? {
                return Ok(commit_same_content(original, &existing, initial_state, before_size, candidate, disposal, is_busy, note, warn));
            }
            let Some(numbered) = claim_numbered(&existing) else {
                if let Some(c) = &candidate {
                    c.discard_temp(warn);
                }
                return Ok(CommitOutcome::NoFreeName);
            };
            (numbered, true)
        }
    };
    Ok(commit_claimed(original, &target, claimed, initial_state, candidate, disposal, note, warn))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Leftover temp files after a forced quit: removed when the original is still there,
    /// renamed into place when the original was already set aside (an empty placeholder of
    /// that name, left by the same interruption, is replaced).
    #[test]
    fn leftover_temps_are_removed_or_recovered() {
        let dir = std::env::temp_dir().join(format!("mo-orphans-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let w = |n: &str, d: &[u8]| std::fs::write(dir.join(n), d).unwrap();
        w("clip.mkv", b"original video");
        w("clip.mediaopti.w1.mp4", b"half-made encode");
        w("pic.mediaopti.i3.png", b"finished new version");
        w("sky.png", b""); // the conversion's placeholder
        w("sky.mediaopti.i2.png", b"finished conversion");
        let yes = |_: &Path| Some(true);
        assert_eq!(settle_orphan_temp(&dir.join("clip.mediaopti.w1.mp4"), &yes).unwrap(), Settled::Removed);
        assert!(!dir.join("clip.mediaopti.w1.mp4").exists() && dir.join("clip.mkv").exists());
        assert_eq!(settle_orphan_temp(&dir.join("pic.mediaopti.i3.png"), &yes).unwrap(), Settled::Recovered(dir.join("pic.png")));
        assert_eq!(std::fs::read(dir.join("pic.png")).unwrap(), b"finished new version");
        assert_eq!(settle_orphan_temp(&dir.join("sky.mediaopti.i2.png"), &yes).unwrap(), Settled::Recovered(dir.join("sky.png")));
        assert_eq!(std::fs::read(dir.join("sky.png")).unwrap(), b"finished conversion");
        // Unreadable media, original gone, not in the journal: half-made, removed.
        w("cut.mediaopti.w2.mp4", b"truncated");
        assert_eq!(settle_orphan_temp(&dir.join("cut.mediaopti.w2.mp4"), &|_: &Path| Some(false)).unwrap(), Settled::Removed);
        assert!(!dir.join("cut.mediaopti.w2.mp4").exists() && !dir.join("cut.mp4").exists());
        // A document's temp can't be checked: left alone, never deleted on a guess.
        w("book.mediaopti.i4.pdf", b"maybe the only copy");
        assert_eq!(settle_orphan_temp(&dir.join("book.mediaopti.i4.pdf"), &|_: &Path| Some(false)).unwrap(), Settled::LeftAlone);
        assert!(dir.join("book.mediaopti.i4.pdf").exists());
        // ffmpeg couldn't run (missing, out of memory, out of time): nothing was learned
        // about the temp, which may be the only copy under that name: left alone.
        w("song.mediaopti.i1.flac", b"maybe the only copy");
        let no_ffmpeg = |p: &Path| super::super::scan::media_readable(&dir.join("missing-ffmpeg.exe"), p);
        assert_eq!(settle_orphan_temp(&dir.join("song.mediaopti.i1.flac"), &no_ffmpeg).unwrap(), Settled::LeftAlone);
        assert!(dir.join("song.mediaopti.i1.flac").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file with the temp's stem but of another kind is not its original: a video's temp
    /// beside `IMG_1.JPG` (its own `IMG_1.MOV` already in the Recycle Bin) is the video now,
    /// and so is a FLAC's temp beside `album.cue`.
    #[test]
    fn same_stem_files_of_another_kind_are_not_the_original() {
        let dir = std::env::temp_dir().join(format!("mo-siblings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let w = |n: &str, d: &[u8]| std::fs::write(dir.join(n), d).unwrap();
        w("IMG_1.JPG", b"live photo still");
        w("IMG_1.mediaopti.w1.mp4", b"finished video");
        w("album.cue", b"cue sheet");
        w("album.mediaopti.i2.flac", b"finished flac");
        let yes = |_: &Path| Some(true);
        let video = settle_orphan_temp(&dir.join("IMG_1.mediaopti.w1.mp4"), &yes).unwrap();
        assert_eq!(video, Settled::Recovered(dir.join("IMG_1.mp4")));
        assert_eq!(std::fs::read(dir.join("IMG_1.mp4")).unwrap(), b"finished video");
        let flac = settle_orphan_temp(&dir.join("album.mediaopti.i2.flac"), &yes).unwrap();
        assert_eq!(flac, Settled::Recovered(dir.join("album.flac")));
        // A same-kind original still there: the temp is a half-made version.
        w("IMG_2.MOV", b"original");
        w("IMG_2.mediaopti.w1.mp4", b"half-made");
        assert_eq!(settle_orphan_temp(&dir.join("IMG_2.mediaopti.w1.mp4"), &yes).unwrap(), Settled::Removed);
        assert!(dir.join("IMG_2.MOV").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A temp in the replacement journal whose original was set aside is the file now, even
    /// when ffmpeg can't read it (a PDF or an Office file): it gets its name. With its
    /// original still in place it is removed.
    #[test]
    fn journaled_temps_follow_their_original() {
        let dir = std::env::temp_dir().join(format!("mo-journaled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (orig, tmp) = (dir.join("manual.pdf"), dir.join("manual.mediaopti.i1.pdf"));
        std::fs::write(&orig, b"original pdf").unwrap();
        std::fs::write(&tmp, b"new pdf").unwrap();
        journal_start(&tmp, &orig, &orig).unwrap();
        std::fs::remove_file(&orig).unwrap(); // set aside, then the app stopped
        let unreadable = |_: &Path| Some(false);
        assert_eq!(settle_orphan_temp(&tmp, &unreadable).unwrap(), Settled::Recovered(orig.clone()));
        assert_eq!(std::fs::read(&orig).unwrap(), b"new pdf");
        assert!(pending_for(&tmp).is_none());

        let (orig, tmp) = (dir.join("book.epub"), dir.join("book.mediaopti.i2.epub"));
        std::fs::write(&orig, b"original").unwrap();
        std::fs::write(&tmp, b"new").unwrap();
        journal_start(&tmp, &orig, &orig).unwrap();
        assert_eq!(settle_orphan_temp(&tmp, &unreadable).unwrap(), Settled::Removed);
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A new version kept for its name keeps it: a later job with the same temp name gets
    /// another number instead of overwriting or removing it.
    #[test]
    fn kept_temps_are_never_reused() {
        let dir = std::env::temp_dir().join(format!("mo-kept-temp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (orig, tmp) = (dir.join("clip.mp4"), dir.join("clip.mediaopti.w1.mp4"));
        let name = |n: u32| crate::args::temp_name_with(&orig, n, "mp4");
        std::fs::write(&tmp, b"new").unwrap();
        assert_eq!(unclaimed_temp(name, 1), tmp, "free: its own name");
        journal_start(&tmp, &orig, &orig).unwrap(); // its original was then set aside
        assert!(kept_for_its_name(&tmp));
        let other = unclaimed_temp(name, 1);
        assert_eq!(other, dir.join("clip.mediaopti.w10001.mp4"));
        assert!(is_orphan_temp(&other), "the cleanup still knows it");
        journal_end(&tmp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The new version carries the original's alternate data streams: the Mark of the Web
    /// (`Zone.Identifier`, which keeps Office's Protected View and macro blocking on a
    /// downloaded file) and any other.
    #[cfg(windows)]
    #[test]
    fn the_new_version_keeps_the_originals_streams() {
        let dir = std::env::temp_dir().join(format!("mo-streams-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (orig, tmp) = (dir.join("report.docm"), dir.join("report.mediaopti.i1.docm"));
        std::fs::write(&orig, b"original").unwrap();
        let zone = b"[ZoneTransfer]\r\nZoneId=3\r\n";
        std::fs::write(dir.join("report.docm:Zone.Identifier"), zone).unwrap();
        std::fs::write(dir.join("report.docm:app.tags"), b"red").unwrap();
        std::fs::write(&tmp, b"new").unwrap();
        replace_original(&tmp, &orig, &orig, &system::Disposal::Delete, &|_: &Path| Ok(())).unwrap();
        assert_eq!(std::fs::read(&orig).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.join("report.docm:Zone.Identifier")).unwrap(), zone);
        assert_eq!(std::fs::read(dir.join("report.docm:app.tags")).unwrap(), b"red");
        // The volume check that lets FAT32/exFAT drives skip the copy says yes on NTFS (and
        // on the long-path form).
        assert!(system::has_named_streams(&orig));
        let deep = dir.join("d".repeat(120)).join("e".repeat(120)).join("f.bin");
        assert!(system::has_named_streams(&deep), "unknown or long: yes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_lines_without_an_ending_are_pending() {
        let text = "+\t3\tC:/v/a.mediaopti.w1.mp4\tC:\\v\\a.mkv\tC:\\v\\a.mp4\n\
                    +\t5\tC:\\v\\b.mediaopti.i2.png\tC:\\v\\b.png\tC:\\v\\b.png\n\
                    -\tC:\\V\\A.mediaopti.w1.mp4\n\
                    +\tx\tdamaged line\n";
        let live = parse_journal(text);
        assert_eq!(live.len(), 1);
        let b = &live[&journal_key(Path::new(r"C:\v\b.mediaopti.i2.png"))];
        assert_eq!((b.size, b.target.as_path()), (5, Path::new(r"C:\v\b.png")));
    }

    /// A Unicode lowercase can shorten a character (the Kelvin sign, 3 bytes, becomes `k`):
    /// a cut found in the lowercased name then fell inside a character of the real one, which
    /// panicked (and a panic aborts the app in release).
    #[test]
    fn stems_with_non_ascii_letters_are_cut_safely() {
        let dir = std::env::temp_dir().join(format!("mo-stem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stem = "\u{212A}\u{212A}\u{e9}";
        std::fs::write(dir.join(format!("{stem}.mov")), b"original").unwrap();
        let temp = dir.join(format!("{stem}.mediaopti.w1.mp4"));
        std::fs::write(&temp, b"half-made").unwrap();
        assert_eq!(settle_orphan_temp(&temp, &|_: &Path| Some(true)).unwrap(), Settled::Removed);
        assert!(dir.join(format!("{stem}.mov")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_and_orphan_names() {
        assert!(is_generated_name(Path::new(r"C:\v\a.mediaopti.w1.mp4")));
        assert!(is_generated_name(Path::new(r"C:\v\clip.TMP.mp4")));
        assert!(is_generated_name(Path::new(r"C:\v\clip-H264.mkv")));
        assert!(!is_generated_name(Path::new(r"C:\v\clip.mp4")));
        assert!(is_orphan_temp(Path::new(r"C:\v\x.BatchViProc.w2.mp4")));
        assert!(is_orphan_temp(Path::new(r"C:\v\x.mediaopti.i12.png")));
        assert!(!is_orphan_temp(Path::new(r"C:\v\notes.mediaopti.final.mp4")), "user file");
        assert!(!is_orphan_temp(Path::new(r"C:\v\my.batchviproc.mp4")), "user file");
    }

    /// A pending entry on an unplugged drive must not be dropped when the journal loads.
    #[test]
    fn volume_mounted_distinguishes_unplugged_drives() {
        assert!(volume_mounted(&std::env::temp_dir().join("x.tmp")));
        assert!(!volume_mounted(Path::new(r"Q:\unplugged-drive-test\a.mediaopti.i1.png")));
    }

    #[test]
    fn commit_candidate_handles_replace_collision_duplicates_option_a_freshness_and_busy() {
        let dir = std::env::temp_dir().join(format!("mo-commit-tx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let no_busy = |_: &Path| false;
        let ok_note = |_: &Path| Ok(());
        let quiet_warn = |_: String| {};

        // 1) In-place replacement via SiblingTemp.
        let orig = dir.join("a.mp4");
        let tmp = dir.join("a.mediaopti.w1.mp4");
        std::fs::write(&orig, b"original-100-bytes-long").unwrap();
        std::fs::write(&tmp, b"small").unwrap();
        let st = file_state(&orig);
        let r = commit_candidate::<()>(
            &orig,
            &orig,
            st,
            23,
            Some(Candidate::SiblingTemp { path: &tmp, size: 5 }),
            &system::Disposal::Delete,
            &no_busy,
            |_| Ok(false),
            &ok_note,
            &quiet_warn,
        )
        .unwrap();
        assert_eq!(r, CommitOutcome::Replaced { target: orig.clone(), renamed: false, size: 5 });
        assert_eq!(std::fs::read(&orig).unwrap(), b"small");
        assert!(!tmp.exists());

        // 2) Collision with different content -> numbered "b (2).png".
        let b_bmp = dir.join("b.bmp");
        let b_png = dir.join("b.png");
        let b_work = dir.join("b_out.png");
        std::fs::write(&b_bmp, b"bmp-original-data").unwrap();
        std::fs::write(&b_png, b"unrelated-png").unwrap();
        std::fs::write(&b_work, b"newpng").unwrap();
        let r = commit_candidate::<()>(
            &b_bmp,
            &b_png,
            file_state(&b_bmp),
            17,
            Some(Candidate::WorkFile { path: &b_work, size: 6, worker_id: 1 }),
            &system::Disposal::Delete,
            &no_busy,
            |_| Ok(false),
            &ok_note,
            &quiet_warn,
        )
        .unwrap();
        assert_eq!(r, CommitOutcome::Replaced { target: dir.join("b (2).png"), renamed: true, size: 6 });
        assert!(!b_bmp.exists());
        assert_eq!(std::fs::read(&b_png).unwrap(), b"unrelated-png");
        assert_eq!(std::fs::read(dir.join("b (2).png")).unwrap(), b"newpng");

        // 3) Same-content smaller candidate replaces sibling and disposes original.
        let c_mov = dir.join("c.mov");
        let c_mp4 = dir.join("c.mp4");
        let c_tmp = dir.join("c.mediaopti.w1.mp4");
        std::fs::write(&c_mov, b"big-original-video-data").unwrap();
        std::fs::write(&c_mp4, b"bigger-sibling-mp4").unwrap();
        std::fs::write(&c_tmp, b"tiny").unwrap();
        let r = commit_candidate::<()>(
            &c_mov,
            &c_mp4,
            file_state(&c_mov),
            23,
            Some(Candidate::SiblingTemp { path: &c_tmp, size: 4 }),
            &system::Disposal::Delete,
            &no_busy,
            |_| Ok(true),
            &ok_note,
            &quiet_warn,
        )
        .unwrap();
        assert_eq!(r, CommitOutcome::DuplicateReplaced { existing: c_mp4.clone(), saved_bytes: 23 + 18 - 4, final_size: 4 });
        assert!(!c_mov.exists());
        assert_eq!(std::fs::read(&c_mp4).unwrap(), b"tiny");

        // 4) Same-content existing copy already smaller -> discards temp and disposes original.
        let d_mov = dir.join("d.mov");
        let d_mp4 = dir.join("d.mp4");
        let d_tmp = dir.join("d.mediaopti.w1.mp4");
        std::fs::write(&d_mov, b"original-video-data").unwrap();
        std::fs::write(&d_mp4, b"ok").unwrap();
        std::fs::write(&d_tmp, b"bigger-tmp").unwrap();
        let r = commit_candidate::<()>(
            &d_mov,
            &d_mp4,
            file_state(&d_mov),
            19,
            Some(Candidate::SiblingTemp { path: &d_tmp, size: 10 }),
            &system::Disposal::Delete,
            &no_busy,
            |_| Ok(true),
            &ok_note,
            &quiet_warn,
        )
        .unwrap();
        assert_eq!(r, CommitOutcome::DuplicateKeptExisting { existing: d_mp4.clone(), saved_bytes: 19, final_size: 2 });
        assert!(!d_mov.exists() && !d_tmp.exists());
        assert_eq!(std::fs::read(&d_mp4).unwrap(), b"ok");

        // 5) Option A: same-content existing copy is a BIGGER hard link -> keep BOTH untouched.
        let e_mov = dir.join("e.mov");
        let e_mp4 = dir.join("e.mp4");
        let e_link = dir.join("e-link.mp4");
        let e_tmp = dir.join("e.mediaopti.w1.mp4");
        std::fs::write(&e_mov, b"original-mov-bytes").unwrap();
        std::fs::write(&e_mp4, b"large-hard-linked-existing").unwrap();
        std::fs::hard_link(&e_mp4, &e_link).unwrap();
        std::fs::write(&e_tmp, b"tiny").unwrap();
        let r = commit_candidate::<()>(
            &e_mov,
            &e_mp4,
            file_state(&e_mov),
            18,
            Some(Candidate::SiblingTemp { path: &e_tmp, size: 4 }),
            &system::Disposal::Delete,
            &no_busy,
            |_| Ok(true),
            &ok_note,
            &quiet_warn,
        )
        .unwrap();
        assert_eq!(r, CommitOutcome::DuplicateKeptBoth { existing: e_mp4.clone(), hard_linked: true });
        assert!(e_mov.exists(), "Option A: original must not be deleted when hard-linked duplicate is bigger");
        assert!(!e_tmp.exists(), "candidate temp discarded");
        assert_eq!(std::fs::read(&e_mp4).unwrap(), b"large-hard-linked-existing");
        assert_eq!(system::hard_link_count(&e_link).ok(), Some(2));

        // 6) Original modified during same_content check -> aborts safely.
        let f_bmp = dir.join("f.bmp");
        let f_png = dir.join("f.png");
        let f_tmp = dir.join("f.mediaopti.w1.png");
        std::fs::write(&f_bmp, b"before-edit").unwrap();
        std::fs::write(&f_png, b"x").unwrap();
        std::fs::write(&f_tmp, b"cand").unwrap();
        let f_st = file_state(&f_bmp);
        let r = commit_candidate::<()>(
            &f_bmp,
            &f_png,
            f_st,
            11,
            Some(Candidate::SiblingTemp { path: &f_tmp, size: 4 }),
            &system::Disposal::Delete,
            &no_busy,
            |_| {
                std::fs::write(&f_bmp, b"edited-while-checking-same-content").unwrap();
                Ok(true)
            },
            &ok_note,
            &quiet_warn,
        )
        .unwrap();
        assert_eq!(r, CommitOutcome::OriginalChanged);
        assert!(f_bmp.exists() && !f_tmp.exists());

        // 7) Busy duplicate -> skips safely without touching either file.
        let g_mov = dir.join("g.mov");
        let g_mp4 = dir.join("g.mp4");
        let g_tmp = dir.join("g.mediaopti.w1.mp4");
        std::fs::write(&g_mov, b"mov").unwrap();
        std::fs::write(&g_mp4, b"mp4").unwrap();
        std::fs::write(&g_tmp, b"tmp").unwrap();
        let r = commit_candidate::<()>(
            &g_mov,
            &g_mp4,
            file_state(&g_mov),
            3,
            Some(Candidate::SiblingTemp { path: &g_tmp, size: 3 }),
            &system::Disposal::Delete,
            &|p| p == g_mp4,
            |_| Ok(true),
            &ok_note,
            &quiet_warn,
        )
        .unwrap();
        assert_eq!(r, CommitOutcome::BusyDuplicate(g_mp4.clone()));
        assert!(g_mov.exists() && g_mp4.exists() && !g_tmp.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
