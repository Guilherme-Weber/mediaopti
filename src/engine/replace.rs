//! Temp file names and the shared step that replaces an original with its new version.

use super::*;

pub(super) fn is_generated_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    is_orphan_temp(path) || name.contains(".tmp.") || name.contains("-h264.")
}

/// Removes a leftover temp file whose original still exists, or, when the original is gone and
/// the leftover is complete (`complete`: ffmpeg can read it), renames it to the original's stem
/// with its own extension (an empty placeholder of that name, left by the same interruption, is
/// replaced). A half-written leftover is removed. Returns the new name when renamed.
pub(super) fn settle_orphan_temp(temp: &Path, complete: &dyn Fn(&Path) -> bool) -> std::io::Result<Option<PathBuf>> {
    let name = temp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = name.to_lowercase();
    let cut = lower.find(".mediaopti.").or_else(|| lower.find(".batchviproc.")).unwrap_or(0);
    let stem = &name[..cut];
    let dir = temp.parent().unwrap_or(Path::new("."));
    let original_exists = std::fs::read_dir(dir)?.filter_map(Result::ok).any(|e| {
        let p = e.path();
        p != temp
            && !is_orphan_temp(&p)
            && p.file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(stem))
            && e.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
    });
    if original_exists || stem.is_empty() || !complete(temp) {
        system::remove_file_if_exists(temp)?;
        return Ok(None);
    }
    let ext = temp.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let mut target = dir.join(format!("{stem}.{ext}"));
    if std::fs::metadata(&target).is_ok_and(|m| m.len() == 0) {
        system::remove_file_if_exists(&target)?;
    } else {
        target = system::unused_name(&target);
    }
    std::fs::rename(temp, &target)?;
    Ok(Some(target))
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

/// Puts the new version `tmp` in place of `original` (as `final_target`), disposing of the
/// original as `disposal` says. On failure the error says what happened; if the original was
/// already set aside (Recycle Bin or backup folder), the new version is never deleted.
/// Blocking. Shared by the video and image workers.
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

pub(super) fn replace_original(tmp: &Path, final_target: &Path, original: &Path, disposal: &system::Disposal) -> Result<(), String> {
    let _replacing = ReplacingGuard::new();
    let Err(err) = system::replace_file_with_retry(tmp, final_target, original, disposal) else {
        return Ok(());
    };
    if original.exists() {
        let _ = system::remove_file_if_exists(tmp);
        return Err(format!("could not replace it: {err}"));
    }
    let gone_to = match disposal {
        system::Disposal::RecycleBin => "is in the Recycle Bin".to_string(),
        system::Disposal::MoveTo(dest) => format!("was moved to {}", dest.parent().unwrap_or(dest).display()),
        system::Disposal::Delete => "was deleted".to_string(),
    };
    Err(format!(
        "could not finish replacing it: {err}. The original {gone_to} and the new version is kept as {}",
        tmp.display()
    ))
}

impl Engine {
    pub(super) fn log_remove(&self, path: &Path) {
        if let Err(err) = system::remove_file_if_exists(path) {
            self.log(format!(
                "Could not remove temporary file {}: {err}",
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
        let yes = |_: &Path| true;
        assert_eq!(settle_orphan_temp(&dir.join("clip.mediaopti.w1.mp4"), &yes).unwrap(), None);
        assert!(!dir.join("clip.mediaopti.w1.mp4").exists() && dir.join("clip.mkv").exists());
        assert_eq!(settle_orphan_temp(&dir.join("pic.mediaopti.i3.png"), &yes).unwrap(), Some(dir.join("pic.png")));
        assert_eq!(std::fs::read(dir.join("pic.png")).unwrap(), b"finished new version");
        assert_eq!(settle_orphan_temp(&dir.join("sky.mediaopti.i2.png"), &yes).unwrap(), Some(dir.join("sky.png")));
        assert_eq!(std::fs::read(dir.join("sky.png")).unwrap(), b"finished conversion");
        // Half-written, original gone: removed, not given a name.
        w("cut.mediaopti.w2.mp4", b"truncated");
        assert_eq!(settle_orphan_temp(&dir.join("cut.mediaopti.w2.mp4"), &|_: &Path| false).unwrap(), None);
        assert!(!dir.join("cut.mediaopti.w2.mp4").exists() && !dir.join("cut.mp4").exists());
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
}
