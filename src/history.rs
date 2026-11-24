//! Processed-file history, remembered two ways:
//!
//! - **by location** (`processedFiles.txt`): one SHA-256 hash per line (hex). For videos it is
//!   the hash of the absolute path, the same as batchviproc and its Electron predecessor, so
//!   existing history carries over. For images it is the hash of path + mode, so a lossless
//!   pass doesn't hide a file from a later lossy one. Instant to check, but a file is
//!   forgotten once it is moved or renamed.
//! - **by content** (`processed-content.tsv`): `<BLAKE2b-128 hex>\t<size>\t<mode>` per line,
//!   the same format imageopti writes (its history is imported once). It recognises a
//!   processed file wherever it is moved or copied to. Only files whose size matches a
//!   remembered size are ever hashed, so checking costs almost nothing for new files.
//!
//! Hashes are kept in memory as raw bytes. New entries are appended; the files are only
//! rewritten when the history is cleared, so huge histories cost almost nothing to save.

use blake2::digest::consts::U16;
use blake2::Blake2b;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

type Hash = [u8; 32];
pub type ContentHash = [u8; 16];

/// Wire string recorded for videos in `processed-content.tsv`.
const VIDEO_MODE: &str = "video";

/// Visually-lossless SSIMULACRA2 quality floors stored in history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum QualityFloor {
    Q80 = 80,
    Q85 = 85,
    Q93 = 93,
}

impl QualityFloor {
    const ALL: [Self; 3] = [Self::Q93, Self::Q85, Self::Q80];

    pub const fn from_target(target: u32) -> Self {
        if target <= 80 {
            Self::Q80
        } else if target <= 85 {
            Self::Q85
        } else {
            Self::Q93
        }
    }

    const fn as_u32(self) -> u32 {
        self as u32
    }
}

/// What pass a file was processed with (or is about to be processed with).
/// Wire strings (`"video"`, `"lossy-normal-t85-strip"`, ...) stay private to this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    Video,
    Image {
        /// `None` = lossless; `Some(floor)` = lossy at that SSIMULACRA2 floor.
        floor: Option<QualityFloor>,
        /// Whether metadata was stripped (`-strip` modes).
        strip: bool,
    },
}

impl Pass {
    pub const fn image(lossy: bool, strip: bool, target: u32) -> Self {
        Self::Image { floor: if lossy { Some(QualityFloor::from_target(target)) } else { None }, strip }
    }

    /// Exact wire string stored in `processedFiles.txt` keys and `processed-content.tsv`
    /// (compatible with imageopti and earlier mediaopti versions).
    const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Video => VIDEO_MODE,
            Self::Image { floor: None, strip: false } => "lossless-normal",
            Self::Image { floor: None, strip: true } => "lossless-normal-strip",
            Self::Image { floor: Some(QualityFloor::Q85), strip: false } => "lossy-normal-t85",
            Self::Image { floor: Some(QualityFloor::Q85), strip: true } => "lossy-normal-t85-strip",
            Self::Image { floor: Some(QualityFloor::Q80), strip: false } => "lossy-normal-t80",
            Self::Image { floor: Some(QualityFloor::Q80), strip: true } => "lossy-normal-t80-strip",
            Self::Image { floor: Some(QualityFloor::Q93), strip: false } => "lossy-normal-t93",
            Self::Image { floor: Some(QualityFloor::Q93), strip: true } => "lossy-normal-t93-strip",
        }
    }

    /// Same rule as imageopti: a lossy pass covers lossless requests and lossy ones at its
    /// floor or above (lossy output is never re-encoded at a higher floor, so loss doesn't
    /// stack needlessly); a lossless pass covers lossless.
    fn covered_by(self, flags: u8) -> bool {
        match self {
            // Only a video pass: a big GIF optimized as an image (imageopti's history) is
            // still to be made a video. (`gif_as_image` marks its result as tried too.)
            Self::Video => flags & F_VIDEO != 0,
            Self::Image { floor: None, strip } => flags & (F_LOSSLESS | ANY_LOSSY) != 0 && (!strip || flags & F_STRIP != 0),
            Self::Image { floor: Some(target), strip } => flags & lossy_flag(target.as_u32()) != 0 && (!strip || flags & F_STRIP != 0),
        }
    }
}

// What a remembered file was processed as (bit flags; a file can have several).
const F_VIDEO: u8 = 1;
const F_LOSSLESS: u8 = 2;
/// A lossy pass at floor 93 or lower (imageopti's `lossy-normal-t93`).
const F_LOSSY: u8 = 4;
/// The pass removed metadata (imageopti's `-strip` modes).
const F_STRIP: u8 = 8;
/// A lossy pass at floor 85 or lower, and at 80.
const F_LOSSY_85: u8 = 16;
const F_LOSSY_80: u8 = 32;
const ANY_LOSSY: u8 = F_LOSSY | F_LOSSY_85 | F_LOSSY_80;

/// The bit a lossy request at floor `target` needs: a pass covers requests at its own floor
/// and above (one done at 80 isn't redone at 93; one done at 93 is redone at 80).
fn lossy_flag(target: u32) -> u8 {
    match target {
        t if t <= 80 => F_LOSSY_80,
        t if t <= 85 => F_LOSSY_85,
        _ => F_LOSSY,
    }
}

fn mode_flag(mode: &str) -> u8 {
    let strip = if mode.ends_with("-strip") { F_STRIP } else { 0 };
    if mode == VIDEO_MODE {
        F_VIDEO
    } else if mode.starts_with("lossy") {
        // A pass at a floor covers it and every higher one.
        let floor = if mode.contains("-t80") {
            F_LOSSY_80 | F_LOSSY_85 | F_LOSSY
        } else if mode.contains("-t85") {
            F_LOSSY_85 | F_LOSSY
        } else {
            F_LOSSY
        };
        floor | strip
    } else {
        F_LOSSLESS | strip
    }
}

pub struct History {
    path: PathBuf,
    hashes: HashSet<Hash>,
    /// Added since the last save, in insertion order.
    unsaved: Vec<Hash>,
    content_path: PathBuf,
    content: HashMap<ContentHash, u8>,
    /// Sizes of processed content, each with the passes made on content of that size: a
    /// file is hashed only when its size had the pass it wants (switching to lossy, with ~1M
    /// pictures done lossless, hashed every one of them for nothing).
    sizes: HashMap<u64, u8>,
    unsaved_content: Vec<(ContentHash, u64, &'static str)>,
    /// The files no longer match memory (after clear) and must be rewritten.
    needs_rewrite: bool,
}

fn hash_path(file: &Path) -> Hash {
    Sha256::digest(file.to_string_lossy().as_bytes()).into()
}

/// Location key of an image: path plus mode. (Videos keep the plain path hash.)
fn hash_image_path(file: &Path, mode: &str) -> Hash {
    let mut h = Sha256::new();
    h.update(file.to_string_lossy().as_bytes());
    h.update([0u8]);
    h.update(mode.as_bytes());
    h.finalize().into()
}

/// BLAKE2b with a 16-byte digest (identical to Python's `hashlib.blake2b(digest_size=16)`).
pub fn hash_file(file: &Path) -> std::io::Result<ContentHash> {
    let mut f = std::fs::File::open(file)?;
    let mut hasher = Blake2b::<U16>::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    hex::decode_to_slice(text, &mut out).ok()?;
    Some(out)
}

fn parse_content_line(line: &str) -> Option<(ContentHash, u64, u8)> {
    let mut parts = line.trim().split('\t');
    let hash = parse_hex::<16>(parts.next()?)?;
    let size = parts.next()?.parse().ok()?;
    Some((hash, size, mode_flag(parts.next().unwrap_or(VIDEO_MODE))))
}

impl History {
    /// `path` is the location history; the content history sits next to it.
    /// Streams both files line-by-line with pre-allocated table capacities (avoids reading
    /// tens of megabytes twice into memory and repeated rehashes on 1M+ entries).
    pub fn load(path: PathBuf) -> Self {
        use std::io::BufRead;
        let mut hashes = HashSet::new();
        if let Ok(file) = std::fs::File::open(&path) {
            if let Ok(meta) = file.metadata() {
                hashes.reserve((meta.len() / 65) as usize);
            }
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
            let mut buf = Vec::with_capacity(128);
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(line) = std::str::from_utf8(&buf) {
                            if let Some(h) = parse_hex::<32>(line.trim()) {
                                hashes.insert(h);
                            }
                        }
                    }
                }
            }
        }
        let content_path = path.with_file_name("processed-content.tsv");
        let (mut content, mut sizes) = (HashMap::new(), HashMap::new());
        if let Ok(file) = std::fs::File::open(&content_path) {
            if let Ok(meta) = file.metadata() {
                let est = (meta.len() / 48) as usize;
                content.reserve(est);
                sizes.reserve(est / 2);
            }
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
            let mut buf = Vec::with_capacity(128);
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(line) = std::str::from_utf8(&buf) {
                            if let Some((hash, size, flag)) = parse_content_line(line) {
                                *content.entry(hash).or_insert(0) |= flag;
                                *sizes.entry(size).or_insert(0) |= flag;
                            }
                        }
                    }
                }
            }
        }
        Self {
            path,
            hashes,
            unsaved: Vec::new(),
            content_path,
            content,
            sizes,
            unsaved_content: Vec::new(),
            needs_rewrite: false,
        }
    }

    pub fn len(&self) -> usize {
        self.hashes.len().max(self.content.len())
    }

    /// Location check for any pass (video or image). A lossy pass covers lossless requests and
    /// lossy requests at its own floor or higher; with `strip`, only stripped passes count.
    pub fn contains_pass(&self, file: &Path, want: Pass) -> bool {
        match want {
            Pass::Video => self.hashes.contains(&hash_path(file)),
            Pass::Image { floor, strip } => {
                let stripped: &[bool] = if strip { &[true] } else { &[true, false] };
                let mut covering = QualityFloor::ALL
                    .iter()
                    .copied()
                    .filter(|&f| floor.is_none_or(|req| f <= req))
                    .map(Some)
                    .chain(floor.is_none().then_some(None));
                // The path is hashed once; each mode continues from that state (a queue rebuild
                // checks every file of the library, a few modes each).
                let mut path_part = Sha256::new();
                path_part.update(file.to_string_lossy().as_bytes());
                path_part.update([0u8]);
                covering.any(|f| {
                    stripped.iter().any(|&s| {
                        let mut h = path_part.clone();
                        h.update(Pass::Image { floor: f, strip: s }.as_wire_str().as_bytes());
                        let key: Hash = h.finalize().into();
                        self.hashes.contains(&key)
                    })
                })
            }
        }
    }

    /// Location check for a video.
    pub fn contains(&self, file: &Path) -> bool {
        self.contains_pass(file, Pass::Video)
    }

    /// Whether content of exactly this size had the pass `want` asks for (only such files are
    /// worth hashing: a hash can't match otherwise).
    pub fn has_size(&self, size: u64, want: Pass) -> bool {
        self.sizes.get(&size).is_some_and(|&f| want.covered_by(f))
    }

    pub fn contains_content(&self, hash: &ContentHash, want: Pass) -> bool {
        self.content.get(hash).is_some_and(|&f| want.covered_by(f))
    }

    /// Remembers a file by location for `done`. Returns true if newly added.
    pub fn add_pass(&mut self, file: &Path, done: Pass) -> bool {
        let hash = match done {
            Pass::Video => hash_path(file),
            Pass::Image { .. } => hash_image_path(file, done.as_wire_str()),
        };
        let added = self.hashes.insert(hash);
        if added {
            self.unsaved.push(hash);
        }
        added
    }

    /// Remembers a video by location. Returns true if it was newly added.
    pub fn add(&mut self, file: &Path) -> bool {
        self.add_pass(file, Pass::Video)
    }

    /// Marks `file` as covered for every image pass (used for a GIF that failed on the video
    /// path so `place` never sends it to the GIF optimizer later).
    pub fn add_all_image_passes(&mut self, file: &Path) {
        let lowest = Some(QualityFloor::Q80);
        for (floor, strip) in [(lowest, false), (None, false), (lowest, true), (None, true)] {
            self.add_pass(file, Pass::Image { floor, strip });
        }
    }

    /// The strongest picture pass this content had (`Q80` before `Q85` before `Q93` before
    /// lossless; `strip` when any pass removed metadata), so a moved file is remembered for
    /// what it really had, not only for the floor asked at this scan.
    pub fn strongest_image_pass(&self, hash: &ContentHash) -> Option<Pass> {
        let flags = *self.content.get(hash)?;
        let strip = flags & F_STRIP != 0;
        let floor = if flags & F_LOSSY_80 != 0 {
            Some(QualityFloor::Q80)
        } else if flags & F_LOSSY_85 != 0 {
            Some(QualityFloor::Q85)
        } else if flags & F_LOSSY != 0 {
            Some(QualityFloor::Q93)
        } else if flags & F_LOSSLESS != 0 {
            None
        } else {
            return None;
        };
        Some(Pass::Image { floor, strip })
    }

    /// If `hash` satisfies `want`, records `file` in the location history under the strongest
    /// matching pass (and, for a GIF recognized as video, also records its image pass when
    /// `gif_image` is covered). Returns true when recognized.
    pub fn recognise_content(&mut self, file: &Path, hash: &ContentHash, want: Pass, gif_image: Option<Pass>) -> bool {
        if !self.contains_content(hash, want) {
            return false;
        }
        match want {
            Pass::Video => {
                self.add_pass(file, Pass::Video);
                if let Some(as_image) = gif_image.filter(|&p| self.contains_content(hash, p)) {
                    let done = self.strongest_image_pass(hash).unwrap_or(as_image);
                    self.add_pass(file, done);
                }
            }
            Pass::Image { .. } => {
                let done = self.strongest_image_pass(hash).unwrap_or(want);
                self.add_pass(file, done);
            }
        }
        true
    }

    pub fn add_content(&mut self, hash: ContentHash, size: u64, done: Pass) {
        let mode = done.as_wire_str();
        let flag = mode_flag(mode);
        let flags = self.content.entry(hash).or_insert(0);
        if *flags & flag != flag {
            *flags |= flag;
            *self.sizes.entry(size).or_insert(0) |= flag;
            self.unsaved_content.push((hash, size, mode));
        }
    }

    pub fn clear(&mut self) {
        self.hashes = HashSet::new();
        self.unsaved = Vec::new();
        self.content = HashMap::new();
        self.sizes = HashMap::new();
        self.unsaved_content = Vec::new();
        self.needs_rewrite = true;
    }

    /// Takes and writes a save at once (tests; the engine writes outside its lock).
    #[cfg(test)]
    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(job) = self.take_save() else { return Ok(()) };
        job.write().inspect_err(|_| self.give_back(job))
    }

    /// What is to be written, taken out of the history (quick, under its owner's lock) so the
    /// writing can happen outside that lock. `None` if nothing is to be saved. Saves must be
    /// written in the order they were taken.
    pub fn take_save(&mut self) -> Option<SaveJob> {
        if !self.needs_rewrite && self.unsaved.is_empty() && self.unsaved_content.is_empty() {
            return None;
        }
        let rewrite = std::mem::take(&mut self.needs_rewrite).then(|| self.hashes.iter().copied().collect());
        Some(SaveJob {
            path: self.path.clone(),
            content_path: self.content_path.clone(),
            rewrite,
            append: std::mem::take(&mut self.unsaved),
            content: std::mem::take(&mut self.unsaved_content),
        })
    }

    /// Puts back a save that could not be written: the next save writes it.
    pub fn give_back(&mut self, job: SaveJob) {
        self.needs_rewrite |= job.rewrite.is_some();
        let mut append = job.append;
        append.append(&mut self.unsaved);
        self.unsaved = append;
        let mut content = job.content;
        content.append(&mut self.unsaved_content);
        self.unsaved_content = content;
    }
}

/// One save of the history files (see [`History::take_save`]).
pub struct SaveJob {
    path: PathBuf,
    content_path: PathBuf,
    /// After a clear: every location entry, written as the whole file.
    rewrite: Option<Vec<Hash>>,
    append: Vec<Hash>,
    content: Vec<(ContentHash, u64, &'static str)>,
}

impl SaveJob {
    pub fn write(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(all) = &self.rewrite {
            // `all` already holds the entries added since the clear (`append`).
            let lines: Vec<String> = all.iter().map(hex::encode).collect();
            write_atomically(&self.path, lines.join("\n").as_bytes())?;
            // Cleared: the content history restarts with what was added since.
            write_atomically(&self.content_path, b"")?;
        } else if !self.append.is_empty() {
            // The file has no trailing newline, so separate from existing content first.
            let has_content = std::fs::metadata(&self.path).is_ok_and(|m| m.len() > 0);
            let mut data = String::with_capacity(self.append.len() * 65);
            for (i, h) in self.append.iter().enumerate() {
                if has_content || i > 0 {
                    data.push('\n');
                }
                data.push_str(&hex::encode(h));
            }
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
            file.write_all(data.as_bytes())?;
        }
        if !self.content.is_empty() {
            let mut data = String::with_capacity(self.content.len() * 56);
            for (hash, size, mode) in &self.content {
                data.push_str(&content_line(hash, *size, mode));
            }
            append_lines(&self.content_path, &data, false)?;
        }
        Ok(())
    }
}

impl History {

    /// Adds content now and returns where and what to append so the caller can write it
    /// durably outside its lock ([`append_lines`]); `None` if it was already known.
    pub fn add_content_now(&mut self, hash: ContentHash, size: u64, done: Pass) -> Option<(PathBuf, String)> {
        let mode = done.as_wire_str();
        let flag = mode_flag(mode);
        let flags = self.content.entry(hash).or_insert(0);
        // Every bit of `flag` (a stripped lossy pass is F_LOSSY | F_STRIP), as `add_content`.
        if *flags & flag == flag {
            return None;
        }
        *flags |= flag;
        *self.sizes.entry(size).or_insert(0) |= flag;
        Some((self.content_path.clone(), content_line(&hash, size, mode)))
    }
}

fn content_line(hash: &ContentHash, size: u64, mode: &str) -> String {
    format!("{}\t{size}\t{mode}\n", hex::encode(hash))
}

/// Appends whole lines to a history file. A line cut off by a crash is ended first (it is
/// then one unreadable line, instead of spoiling the next one too). `durable`: on the disk
/// before returning.
pub fn append_lines(path: &Path, lines: &str, durable: bool) -> std::io::Result<()> {
    static APPENDING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = APPENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    use std::io::{Read, Seek};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).read(true).append(true).open(path)?;
    let len = file.metadata()?.len();
    let mut last = *b"\n";
    if len > 0 {
        file.seek(std::io::SeekFrom::Start(len - 1))?;
        file.read_exact(&mut last)?;
    }
    if last[0] != b'\n' {
        file.write_all(b"\n")?;
    }
    file.write_all(lines.as_bytes())?;
    if durable {
        file.sync_data()?;
    }
    Ok(())
}

/// Replaces a file's content so that a crash leaves either the old or the new one whole.
pub fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("saving");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// One-time migration from batchviproc: copies its processed-files list and UI settings
/// into this app's data folder if this app has none yet. The path-hash format is the same.
pub fn import_batchviproc(old_dir: &Path, data_dir: &Path) {
    for name in ["processedFiles.txt", "ui-settings.json"] {
        let (from, to) = (old_dir.join(name), data_dir.join(name));
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

/// One-time import of imageopti's content history (same line format), so images it
/// already optimized are recognised.
pub fn import_imageopti(imageopti_dir: &Path, data_dir: &Path) {
    let marker = data_dir.join("imageopti-imported");
    let from = imageopti_dir.join("history-hashes.tsv");
    if marker.exists() || !from.is_file() {
        return;
    }
    let Ok(data) = std::fs::read_to_string(&from) else { return };
    let lines: String = data
        .lines()
        .filter(|l| parse_content_line(l).is_some())
        .flat_map(|l| [l, "\n"])
        .collect();
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("processed-content.tsv"))
        .and_then(|mut f| f.write_all(lines.as_bytes()));
    if appended.is_ok() {
        let _ = std::fs::write(marker, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checking an image hashes its path once and continues per mode: every mode must still
    /// find the entries written with the full key (the history's format is unchanged).
    #[test]
    fn image_checks_find_every_mode() {
        let file = Path::new(r"F:\Pictures\Á pasta\foto 1.png");
        for lossy in [false, true] {
            for strip in [false, true] {
                let mut h = History::load(temp(&format!("modes-{lossy}-{strip}")).join("processedFiles.txt"));
                let done = Pass::image(lossy, strip, 93);
                h.hashes.insert(hash_image_path(file, done.as_wire_str()));
                // A lossy entry covers both requests; a stripped one covers any.
                assert!(h.contains_pass(file, done), "{lossy} {strip}");
                assert_eq!(h.contains_pass(file, Pass::image(true, strip, 93)), lossy, "{lossy} {strip}");
                assert_eq!(h.contains_pass(file, Pass::image(lossy, true, 93)), strip, "{lossy} {strip}");
                assert!(!h.contains_pass(Path::new(r"F:\other.png"), Pass::image(false, false, 93)));
            }
        }
        // Wire strings stay bit-for-bit compatible with imageopti and earlier mediaopti versions.
        assert_eq!(Pass::Video.as_wire_str(), "video");
        assert_eq!(Pass::image(false, false, 93).as_wire_str(), "lossless-normal");
        assert_eq!(Pass::image(false, true, 93).as_wire_str(), "lossless-normal-strip");
        assert_eq!(Pass::image(true, false, 93).as_wire_str(), "lossy-normal-t93");
        assert_eq!(Pass::image(true, true, 85).as_wire_str(), "lossy-normal-t85-strip");
        assert_eq!(Pass::image(true, false, 80).as_wire_str(), "lossy-normal-t80");
    }

    /// A save is taken under the engine's lock and written outside it; one that can't be
    /// written goes back into the history, so nothing is lost and the next save retries it.
    #[test]
    fn a_save_taken_and_written_later_loses_nothing() {
        let dir = temp("savejob");
        let mut h = History::load(dir.join("processedFiles.txt"));
        h.add(Path::new(r"C:\v\a.mp4"));
        let job = h.take_save().unwrap();
        assert!(h.take_save().is_none(), "nothing left to save");
        h.add(Path::new(r"C:\v\b.mp4"));
        job.write().unwrap();
        h.save().unwrap();
        let again = History::load(dir.join("processedFiles.txt"));
        assert!(again.contains(Path::new(r"C:\v\a.mp4")) && again.contains(Path::new(r"C:\v\b.mp4")));

        // Its folder is a file: the save fails and the entry stays to be saved.
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"").unwrap();
        let mut h = History::load(blocked.join("processedFiles.txt"));
        h.add(Path::new(r"C:\v\c.mp4"));
        let job = h.take_save().unwrap();
        assert!(job.write().is_err());
        h.give_back(job);
        assert!(h.take_save().is_some(), "kept for the next save");
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mo-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a crash or power cut can leave: bytes that are not text and a line cut off.
    /// Only those lines are lost; the next lines are written whole.
    #[test]
    fn damaged_history_keeps_every_whole_line() {
        let dir = temp("damaged");
        let path = dir.join("processedFiles.txt");
        let (a, b) = (hash_path(Path::new(r"C:\a.mp4")), hash_path(Path::new(r"C:\b.mp4")));
        let mut data = hex::encode(a).into_bytes();
        data.extend_from_slice(b"\n\xff\xfe\x00garbage\n");
        data.extend_from_slice(hex::encode(b).as_bytes());
        std::fs::write(&path, &data).unwrap();
        let (h1, h2) = ([1u8; 16], [2u8; 16]);
        std::fs::write(dir.join("processed-content.tsv"), format!("{}\t10\tvideo\n{}", hex::encode(h1), &hex::encode(h2)[..10])).unwrap();
        let mut h = History::load(path.clone());
        assert!(h.contains(Path::new(r"C:\a.mp4")) && h.contains(Path::new(r"C:\b.mp4")), "lines lost around unreadable bytes");
        assert!(h.contains_content(&h1, Pass::Video) && !h.contains_content(&h2, Pass::Video));
        // The cut-off content line is ended before the next one is added.
        let (path3, line) = h.add_content_now([3u8; 16], 30, Pass::Video).unwrap();
        append_lines(&path3, &line, true).unwrap();
        assert!(History::load(path.clone()).contains_content(&[3u8; 16], Pass::Video));

        // A stripped pass on content known from a plain lossy pass is noted durably too
        // (the check took any shared bit as "already known").
        let mut h = History::load(path.clone());
        h.add_content([4u8; 16], 40, Pass::image(true, false, 93));
        let (p, line) = h.add_content_now([4u8; 16], 40, Pass::image(true, true, 93)).expect("the strip bit is new");
        append_lines(&p, &line, true).unwrap();
        assert!(h.add_content_now([4u8; 16], 40, Pass::image(true, true, 93)).is_none(), "now known");
        assert!(History::load(path).contains_content(&[4u8; 16], Pass::image(true, true, 93)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_matches_node_sha256_of_path() {
        // Same as crypto.createHash('sha256').update('C:\\videos\\a.mp4').digest('hex') in Node.
        let expected = hex::encode(Sha256::digest(r"C:\videos\a.mp4".as_bytes()));
        assert_eq!(hex::encode(hash_path(Path::new(r"C:\videos\a.mp4"))), expected);
    }

    #[test]
    fn content_hash_matches_python_blake2b_16() {
        // python: hashlib.blake2b(b"hello mediaopti", digest_size=16).hexdigest()
        let dir = temp("blake");
        let f = dir.join("x.bin");
        std::fs::write(&f, b"hello mediaopti").unwrap();
        assert_eq!(hex::encode(hash_file(&f).unwrap()), "04fec7e7c4008ac37dc243373d9252fe");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn appends_new_entries_and_rewrites_only_after_clear() {
        let dir = temp("append");
        let file = dir.join("processedFiles.txt");
        let a = hex::encode(hash_path(Path::new(r"C:\v\a.mp4")));
        std::fs::write(&file, format!("{a}\n\n  zz-not-hex  ")).unwrap();

        let mut h = History::load(file.clone());
        assert_eq!(h.len(), 1);
        assert!(h.contains(Path::new(r"C:\v\a.mp4")));
        assert!(!h.add(Path::new(r"C:\v\a.mp4")));
        assert!(h.add(Path::new(r"C:\v\b.mp4")));
        assert!(h.add(Path::new(r"C:\v\c.mp4")));
        h.save().unwrap();

        // Appended without touching existing lines; reloads to the same set.
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.starts_with(&format!("{a}\n\n  zz-not-hex  \n")));
        assert!(!saved.ends_with('\n'));
        assert_eq!(History::load(file.clone()).len(), 3);

        h.clear();
        assert!(h.add(Path::new(r"C:\v\d.mp4")));
        h.save().unwrap();
        let reloaded = History::load(file);
        assert_eq!(reloaded.len(), 1);
        assert!(reloaded.contains(Path::new(r"C:\v\d.mp4")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn content_history_modes_and_imageopti_lines() {
        let dir = temp("content");
        let file = dir.join("processedFiles.txt");
        // A line as imageopti writes it, plus junk that must be ignored.
        std::fs::write(
            dir.join("processed-content.tsv"),
            "e67a4fc97c7ac57455fe91c3a7a91279\t336517\tlossless-normal\nnot a line\n",
        )
        .unwrap();
        let mut h = History::load(file.clone());
        let img = parse_hex::<16>("e67a4fc97c7ac57455fe91c3a7a91279").unwrap();
        assert!(h.contains_content(&img, Pass::image(false, false, 93)));
        assert!(!h.contains_content(&img, Pass::image(true, false, 93)), "lossless doesn't cover lossy");
        assert!(!h.contains_content(&img, Pass::image(false, true, 93)), "not stripped");
        // A big GIF imageopti optimized as an image is still to be made a video.
        assert!(!h.contains_content(&img, Pass::Video), "an image pass is not a video pass");
        assert!(h.has_size(336_517, Pass::image(false, false, 93)));
        // Its size was only ever done lossless: a lossy request doesn't hash files of it.
        assert!(!h.has_size(336_517, Pass::image(true, false, 93)) && !h.has_size(336_517, Pass::Video));

        let vid = [7u8; 16];
        h.add_content(vid, 1234, Pass::Video);
        h.add_content(vid, 1234, Pass::Video); // duplicate: written once
        h.add_content(img, 336_517, Pass::image(true, false, 93));
        h.add_content(img, 336_517, Pass::image(true, true, 93)); // flag added to known content
        h.save().unwrap();
        let text = std::fs::read_to_string(dir.join("processed-content.tsv")).unwrap();
        assert_eq!(text.matches("\tvideo\n").count(), 1);

        let mut r = History::load(file);
        assert!(r.contains_content(&vid, Pass::Video));
        assert!(r.contains_content(&img, Pass::image(true, false, 93)), "lossy pass survives reload");
        assert!(r.contains_content(&img, Pass::image(true, true, 93)), "stripped pass survives reload");
        assert!(r.has_size(1234, Pass::Video) && !r.has_size(1235, Pass::Video));
        assert!(r.has_size(336_517, Pass::image(true, true, 93)), "the stripped lossy pass reached its size");

        // Floors in the content history: content done at 93 is not "done" for a request at
        // 80 (it was taken as done, so pictures done at 93 never got the smaller level), and
        // a durable note at 80 is written even when 93 was known.
        let at = |t| Pass::image(true, false, t);
        let c93 = [5u8; 16];
        r.add_content(c93, 50, at(93));
        assert!(r.contains_content(&c93, at(93)) && !r.contains_content(&c93, at(85)) && !r.contains_content(&c93, at(80)));
        assert!(!r.has_size(50, at(80)), "nothing to hash for a request at 80");
        assert!(r.add_content_now(c93, 50, at(80)).is_some(), "the 80 pass is new");
        assert!(r.contains_content(&c93, at(80)) && r.contains_content(&c93, at(85)));
        let c80 = [6u8; 16];
        r.add_content(c80, 60, at(80));
        assert!(r.contains_content(&c80, at(93)) && r.contains_content(&c80, Pass::image(false, false, 93)), "a lower floor covers higher ones");
        assert_eq!(mode_flag("lossy-normal-t85-strip"), F_LOSSY_85 | F_LOSSY | F_STRIP);
        assert_eq!(r.strongest_image_pass(&c80), Some(at(80)), "the strongest pass it had");
        assert_eq!(r.strongest_image_pass(&[9u8; 16]), None);

        // Image location keys are per mode and don't collide with video keys.
        let f = Path::new(r"C:\p\a.png");
        r.add_pass(f, Pass::image(false, false, 93));
        assert!(r.contains_pass(f, Pass::image(false, false, 93)) && !r.contains_pass(f, at(93)) && !r.contains(f));
        assert!(!r.contains_pass(f, Pass::image(false, true, 93)), "the redo option asks for a stripped pass");
        r.add_pass(f, at(93));
        assert!(r.contains_pass(f, at(93)));
        // Done at 93: a lower floor asks for more and redoes it; done at 80, nothing does.
        assert!(!r.contains_pass(f, at(85)) && !r.contains_pass(f, at(80)));
        let g = Path::new(r"F:\pics\b.png");
        r.add_pass(g, at(80));
        assert!(r.contains_pass(g, at(80)) && r.contains_pass(g, at(85)) && r.contains_pass(g, at(93)));
        assert!(r.contains_pass(g, Pass::image(false, false, 93)), "a lossy pass covers a lossless request");
        r.add_pass(f, Pass::image(true, true, 93));
        assert!(r.contains_pass(f, Pass::image(false, true, 93)) && r.contains_pass(f, Pass::image(true, true, 93)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn imports_batchviproc_and_imageopti_once() {
        let dir = temp("import");
        let (old, new, io) = (dir.join("old"), dir.join("new"), dir.join("imageopti"));
        for d in [&old, &new, &io] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(old.join("processedFiles.txt"), "aa").unwrap();
        import_batchviproc(&old, &new);
        assert_eq!(std::fs::read_to_string(new.join("processedFiles.txt")).unwrap(), "aa");
        std::fs::write(old.join("processedFiles.txt"), "bb").unwrap();
        import_batchviproc(&old, &new); // never overwrites this app's own history
        assert_eq!(std::fs::read_to_string(new.join("processedFiles.txt")).unwrap(), "aa");

        let line = "e67a4fc97c7ac57455fe91c3a7a91279\t336517\tlossy-normal-t93";
        std::fs::write(io.join("history-hashes.tsv"), format!("{line}\njunk\n")).unwrap();
        import_imageopti(&io, &new);
        import_imageopti(&io, &new); // second call does nothing
        let text = std::fs::read_to_string(new.join("processed-content.tsv")).unwrap();
        assert_eq!(text, format!("{line}\n"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
