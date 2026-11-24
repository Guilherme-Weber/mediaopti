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
    if video_temp {
        return args::is_video_file(file) || image::kind_of(file) == Some(image::Kind::Gif);
    }
    let family = |p: &Path| {
        image::kind_of(p).map(|k| if k.is_audio() { 1 } else if k.is_document() { 2 } else { 3 })
    };
    family(file).is_some() && family(file) == family(temp)
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
    match super::images::volume_root(path) {
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
}
