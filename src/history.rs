//! Processed-file history, remembered two ways:
//!
//! - **by location** (`processedFiles.txt`): one SHA-256 hash of the absolute path per line
//!   (hex). Same file and hashing as batchviproc and its Electron predecessor, so existing
//!   history carries over. Instant to check, but forgets a file once it is moved or renamed.
//! - **by content** (`processed-content.tsv`): `<BLAKE2b-128 hex>\t<size>\t<mode>` per line,
//!   the same format imageopti writes, so the two histories can be merged. It recognises a
//!   processed file wherever it is moved or copied to. Only files whose size matches a
//!   remembered size are ever hashed, so checking costs almost nothing for new files.
//!
//! Hashes are kept in memory as raw bytes. New entries are appended; the files are only
//! rewritten when the history is cleared, so huge histories cost almost nothing to save.

use blake2::digest::consts::U16;
use blake2::Blake2b;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

type Hash = [u8; 32];
pub type ContentHash = [u8; 16];

/// Mode recorded for videos in the content history (imageopti uses e.g. `lossless-normal`).
pub const VIDEO_MODE: &str = "video";

pub struct History {
    path: PathBuf,
    hashes: HashSet<Hash>,
    /// Added since the last save, in insertion order.
    unsaved: Vec<Hash>,
    content_path: PathBuf,
    content: HashSet<ContentHash>,
    sizes: HashSet<u64>,
    unsaved_content: Vec<(ContentHash, u64, &'static str)>,
    /// The files no longer match memory (after clear) and must be rewritten.
    needs_rewrite: bool,
}

fn hash_path(file: &Path) -> Hash {
    Sha256::digest(file.to_string_lossy().as_bytes()).into()
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

fn parse_content_line(line: &str) -> Option<(ContentHash, u64)> {
    let mut parts = line.trim().split('\t');
    let hash = parse_hex::<16>(parts.next()?)?;
    let size = parts.next()?.parse().ok()?;
    Some((hash, size))
}

impl History {
    /// `path` is the location history; the content history sits next to it.
    pub fn load(path: PathBuf) -> Self {
        let hashes = std::fs::read_to_string(&path)
            .map(|data| data.split('\n').filter_map(|l| parse_hex::<32>(l.trim())).collect())
            .unwrap_or_default();
        let content_path = path.with_file_name("processed-content.tsv");
        let (mut content, mut sizes) = (HashSet::new(), HashSet::new());
        if let Ok(data) = std::fs::read_to_string(&content_path) {
            for (hash, size) in data.lines().filter_map(parse_content_line) {
                content.insert(hash);
                sizes.insert(size);
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

    pub fn contains(&self, file: &Path) -> bool {
        self.hashes.contains(&hash_path(file))
    }

    /// Whether some processed file had exactly this size (only those are worth hashing).
    pub fn has_size(&self, size: u64) -> bool {
        self.sizes.contains(&size)
    }

    pub fn contains_content(&self, hash: &ContentHash) -> bool {
        self.content.contains(hash)
    }

    /// Returns true if the file was newly added.
    pub fn add(&mut self, file: &Path) -> bool {
        let hash = hash_path(file);
        let added = self.hashes.insert(hash);
        if added {
            self.unsaved.push(hash);
        }
        added
    }

    pub fn add_content(&mut self, hash: ContentHash, size: u64, mode: &'static str) {
        if self.content.insert(hash) {
            self.sizes.insert(size);
            self.unsaved_content.push((hash, size, mode));
        }
    }

    pub fn clear(&mut self) {
        self.hashes = HashSet::new();
        self.unsaved = Vec::new();
        self.content = HashSet::new();
        self.sizes = HashSet::new();
        self.unsaved_content = Vec::new();
        self.needs_rewrite = true;
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        if !self.needs_rewrite && self.unsaved.is_empty() && self.unsaved_content.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if self.needs_rewrite {
            let lines: Vec<String> = self.hashes.iter().map(hex::encode).collect();
            std::fs::write(&self.path, lines.join("\n"))?;
            // Cleared: the content history restarts with what was added since.
            std::fs::write(&self.content_path, "")?;
            self.needs_rewrite = false;
        } else if !self.unsaved.is_empty() {
            // The file has no trailing newline, so separate from existing content first.
            let has_content = std::fs::metadata(&self.path).is_ok_and(|m| m.len() > 0);
            let mut data = String::with_capacity(self.unsaved.len() * 65);
            for (i, h) in self.unsaved.iter().enumerate() {
                if has_content || i > 0 {
                    data.push('\n');
                }
                data.push_str(&hex::encode(h));
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            file.write_all(data.as_bytes())?;
        }
        self.unsaved.clear();

        if !self.unsaved_content.is_empty() {
            let mut data = String::with_capacity(self.unsaved_content.len() * 56);
            for (hash, size, mode) in &self.unsaved_content {
                data.push_str(&format!("{}\t{size}\t{mode}\n", hex::encode(hash)));
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.content_path)?;
            file.write_all(data.as_bytes())?;
            self.unsaved_content.clear();
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mo-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn content_history_survives_reload_and_reads_imageopti_lines() {
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
        assert!(h.contains_content(&img));
        assert!(h.has_size(336_517));

        let vid = [7u8; 16];
        h.add_content(vid, 1234, VIDEO_MODE);
        h.add_content(vid, 1234, VIDEO_MODE); // duplicate: written once
        h.save().unwrap();
        let text = std::fs::read_to_string(dir.join("processed-content.tsv")).unwrap();
        assert_eq!(text.matches("\tvideo\n").count(), 1);

        let reloaded = History::load(file);
        assert!(reloaded.contains_content(&vid) && reloaded.contains_content(&img));
        assert!(reloaded.has_size(1234) && !reloaded.has_size(1235));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn imports_batchviproc_once() {
        let dir = temp("import");
        let (old, new) = (dir.join("old"), dir.join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("processedFiles.txt"), "aa").unwrap();
        import_batchviproc(&old, &new);
        assert_eq!(std::fs::read_to_string(new.join("processedFiles.txt")).unwrap(), "aa");
        std::fs::write(old.join("processedFiles.txt"), "bb").unwrap();
        import_batchviproc(&old, &new); // never overwrites this app's own history
        assert_eq!(std::fs::read_to_string(new.join("processedFiles.txt")).unwrap(), "aa");
        let _ = std::fs::remove_dir_all(dir);
    }
}
