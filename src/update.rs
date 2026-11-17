//! Updates from the repository's GitHub releases: a check when the app opens, and an install
//! that downloads the release zip, checks its SHA-256, swaps the app's files and restarts.
//!
//! The swap renames each file it replaces to `<name>.mediaopti-old` (Windows allows renaming
//! a running program) and puts the new one in its place; if any step fails, everything done
//! so far is undone. The old copies are deleted at the next start (`remove_leftovers`).

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

const LATEST: &str = "https://api.github.com/repos/Guilherme-Weber/mediaopti/releases/latest";
/// Folder in the app's folder where the download is unpacked (same drive: renames, no copies).
const STAGE: &str = ".mediaopti-update";
/// Suffix of a replaced file until the next start removes it.
const OLD_SUFFIX: &str = ".mediaopti-old";
/// The program a release must contain at its root.
const EXE: &str = "mediaopti.exe";
/// Command-line flag of the instance started after an update: it waits for the old one to quit.
pub const UPDATED_FLAG: &str = "--updated";

/// A release newer than the running app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub zip_url: String,
    pub zip_size: u64,
    pub sha256: String,
}

static FOUND: Mutex<Option<Release>> = Mutex::new(None);
static RESTART: AtomicBool = AtomicBool::new(false);
/// Held while files are being swapped: the app waits for it before exiting.
static SWAPPING: Mutex<()> = Mutex::new(());
static APP_EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The running program's path, taken before an update renames it.
pub fn app_exe() -> Option<PathBuf> {
    APP_EXE.get_or_init(|| std::env::current_exe().ok().and_then(|p| p.canonicalize().ok().or(Some(p)))).clone()
}

/// The newer release found by the last check.
pub fn found() -> Option<Release> {
    FOUND.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Asks GitHub for the latest release. `Ok(None)`: this version is the latest. Blocking.
pub fn check(scratch: &Path) -> Result<Option<Release>, String> {
    let release = check_against(scratch, env!("CARGO_PKG_VERSION"))?;
    release.clone_into(&mut FOUND.lock().unwrap_or_else(PoisonError::into_inner));
    Ok(release)
}

fn check_against(scratch: &Path, current: &str) -> Result<Option<Release>, String> {
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let json_path = scratch.join(format!("latest-release.{}.json", std::process::id()));
    let result = crate::image::tools::url_download(LATEST, &json_path)
        .and_then(|()| std::fs::read_to_string(&json_path).map_err(|e| e.to_string()))
        .and_then(|json| parse_release(&json, current));
    let _ = std::fs::remove_file(&json_path);
    result
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next().unwrap_or(Some(0))?, parts.next().unwrap_or(Some(0))?);
    parts.next().is_none().then_some(version)
}

/// Reads GitHub's "latest release" answer; `None` when it isn't newer than `current`.
fn parse_release(json: &str, current: &str) -> Result<Option<Release>, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|_| "GitHub's answer could not be read".to_string())?;
    let tag = value["tag_name"].as_str().ok_or("GitHub's answer has no version")?;
    let latest = parse_version(tag).ok_or_else(|| format!("unexpected version \"{tag}\""))?;
    let running = parse_version(current).ok_or_else(|| format!("unexpected version \"{current}\""))?;
    if latest <= running {
        return Ok(None);
    }
    let asset = value["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|a| a["name"].as_str().is_some_and(|n| n.ends_with("-win64.zip"))))
        .ok_or("the release has no Windows zip")?;
    let zip_url = asset["browser_download_url"].as_str().filter(|u| u.starts_with("https://")).ok_or("the release zip has no address")?;
    // GitHub publishes each file's SHA-256: a download that doesn't match is never installed.
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("the release zip has no checksum")?;
    Ok(Some(Release {
        version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        page: value["html_url"].as_str().unwrap_or_default().to_string(),
        zip_url: zip_url.to_string(),
        zip_size: asset["size"].as_u64().unwrap_or(0),
        sha256: sha256.to_ascii_lowercase(),
    }))
}

/// Downloads `release`, checks it and swaps the app's files. Blocking. On success the caller
/// quits the app with [`request_restart`] set; on failure nothing in the app's folder changed.
pub fn install(release: &Release) -> Result<(), String> {
    let exe = app_exe().ok_or("the app's location is unknown")?;
    install_into(release, exe.parent().ok_or("the app's location is unknown")?)
}

fn install_into(release: &Release, app_dir: &Path) -> Result<(), String> {
    let stage = app_dir.join(STAGE);
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| format!("cannot write to {}: {e}", app_dir.display()))?;
    let result = (|| {
        let zip = stage.join("download.zip");
        // About 90 MB: up to 30 minutes on a slow connection.
        crate::image::tools::url_download_within(&release.zip_url, &zip, 1800)?;
        verify_download(&zip, release)?;
        let files = stage.join("files");
        let list = unpack(&zip, &files)?;
        let _ = std::fs::remove_file(&zip);
        let _swapping = SWAPPING.lock().unwrap_or_else(PoisonError::into_inner);
        swap(&files, app_dir, &list)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn verify_download(zip: &Path, release: &Release) -> Result<(), String> {
    let mut file = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    if release.zip_size != 0 && size != release.zip_size {
        return Err(format!("the download is incomplete ({size} of {} bytes)", release.zip_size));
    }
    if hex::encode(hasher.finalize()) != release.sha256 {
        return Err("the download does not match the release's checksum".into());
    }
    Ok(())
}

/// A zip entry's name as a safe relative path (both separators accepted: PowerShell's
/// Compress-Archive may write backslashes). `None` for folders and anything leaving the folder.
fn entry_path(name: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = name.split(['/', '\\']).collect();
    let bad = |p: &&str| p.is_empty() || *p == "." || *p == ".." || p.contains(':') || *p == STAGE;
    if parts.iter().any(bad) || name.ends_with(OLD_SUFFIX) {
        return None;
    }
    let path: PathBuf = parts.iter().collect();
    path.components().all(|c| matches!(c, Component::Normal(_))).then_some(path)
}

/// Unpacks every file of the zip into `to`; returns their relative paths.
fn unpack(zip: &Path, to: &Path) -> Result<Vec<PathBuf>, String> {
    let file = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| "the download is not a zip file".to_string())?;
    let mut list = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        if entry.is_dir() || name.ends_with(['/', '\\']) {
            continue;
        }
        let rel = entry_path(&name).ok_or_else(|| format!("unexpected file name in the release: {name}"))?;
        let target = to.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("{}: {e}", target.display()))?;
        out.sync_all().map_err(|e| e.to_string())?;
        list.push(rel);
    }
    let exe = to.join(EXE);
    let program = std::fs::read(&exe).map_err(|_| format!("{EXE} is not in the release"))?;
    if !program.starts_with(b"MZ") {
        return Err(format!("{EXE} in the release is not a Windows program"));
    }
    Ok(list)
}

fn old_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(OLD_SUFFIX);
    PathBuf::from(name)
}

/// Moves each unpacked file into the app's folder, setting the one it replaces aside. All or
/// nothing: a failure puts back every file already moved.
fn swap(from: &Path, to: &Path, list: &[PathBuf]) -> Result<(), String> {
    // (target, whether an old copy was set aside) for each finished step.
    let mut done: Vec<(PathBuf, bool)> = Vec::new();
    let result = list.iter().try_for_each(|rel| {
        let (new, target) = (from.join(rel), to.join(rel));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let had_old = target.exists();
        if had_old {
            let old = old_name(&target);
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&target, &old).map_err(|e| format!("{} is in use ({e})", target.display()))?;
        }
        if let Err(e) = std::fs::rename(&new, &target) {
            if had_old {
                let _ = std::fs::rename(old_name(&target), &target);
            }
            return Err(format!("{}: {e}", target.display()));
        }
        done.push((target, had_old));
        Ok(())
    });
    if result.is_err() {
        for (target, had_old) in done.iter().rev() {
            let _ = std::fs::remove_file(target);
            if *had_old {
                let _ = std::fs::rename(old_name(target), target);
            }
        }
    }
    result
}

/// Removes what an update left in the app's folder: the replaced files (`*.mediaopti-old`,
/// exact suffix only) and an unfinished download.
pub fn remove_leftovers() {
    let Some(app_dir) = app_exe().and_then(|e| e.parent().map(Path::to_path_buf)) else { return };
    remove_leftovers_in(&app_dir);
}

fn remove_leftovers_in(app_dir: &Path) {
    let _ = std::fs::remove_dir_all(app_dir.join(STAGE));
    for entry in walkdir::WalkDir::new(app_dir).max_depth(4).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(OLD_SUFFIX) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Asks `main` to start the new version once the app has closed.
pub fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
}

/// Called by `main` after the window closed: waits for a swap in progress (closing the window
/// mid-swap must not leave half the files replaced), then starts the updated app if asked. It
/// is started outside the app's kill-on-exit job, or it would be ended with this process.
pub fn restart_if_requested() {
    drop(SWAPPING.lock().unwrap_or_else(PoisonError::into_inner));
    if !RESTART.load(Ordering::SeqCst) {
        return;
    }
    let Some(exe) = app_exe() else { return };
    let mut cmd = std::process::Command::new(&exe);
    cmd.args([UPDATED_FLAG, "--page=about"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        if cmd.creation_flags(CREATE_BREAKAWAY_FROM_JOB).spawn().is_ok() {
            return;
        }
        // Breakaway refused (an outer job forbids it): Explorer starts it on the app's behalf.
        let _ = std::process::Command::new("explorer").arg(&exe).spawn();
    }
    #[cfg(not(windows))]
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(tag: &str, digest: &str) -> String {
        serde_json::json!({
            "tag_name": tag,
            "html_url": "https://github.com/Guilherme-Weber/mediaopti/releases/tag/v9.0.0",
            "assets": [
                { "name": "notes.txt", "browser_download_url": "https://example.com/notes.txt", "size": 1 },
                { "name": "mediaopti-9.0.0-win64.zip", "browser_download_url": "https://example.com/m.zip",
                  "size": 1234, "digest": digest }
            ]
        })
        .to_string()
    }

    #[test]
    fn newer_release_is_found_and_older_ignored() {
        let digest = format!("sha256:{}", "AB".repeat(32));
        let found = parse_release(&answer("v3.1.0", &digest), "3.0.7").unwrap().unwrap();
        assert_eq!(found.version, "3.1.0");
        assert_eq!(found.zip_url, "https://example.com/m.zip");
        assert_eq!(found.zip_size, 1234);
        assert_eq!(found.sha256, "ab".repeat(32));
        assert_eq!(parse_release(&answer("v3.0.7", &digest), "3.0.7").unwrap(), None);
        assert_eq!(parse_release(&answer("v3.0.10", &digest), "3.0.9").unwrap().unwrap().version, "3.0.10");
        assert_eq!(parse_release(&answer("v2.9.9", &digest), "3.0.7").unwrap(), None);
        assert!(parse_release(&answer("v4.0.0", "md5:00"), "3.0.7").is_err(), "no SHA-256, no install");
        assert!(parse_release("not json", "3.0.7").is_err());
    }

    #[test]
    fn zip_names_stay_inside_the_folder() {
        assert_eq!(entry_path("mediaopti.exe"), Some(PathBuf::from("mediaopti.exe")));
        assert_eq!(entry_path("tools\\qpdf\\qpdf.exe"), Some(Path::new("tools").join("qpdf").join("qpdf.exe")));
        assert_eq!(entry_path("tools/flac/flac.exe"), Some(Path::new("tools").join("flac").join("flac.exe")));
        for bad in ["../evil.exe", "tools\\..\\..\\evil.exe", "/abs.exe", "\\abs.exe", "C:\\x.exe", "c:x.exe", "a.exe.mediaopti-old", "tools/"] {
            assert_eq!(entry_path(bad), None, "{bad}");
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mo-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn swap_replaces_files_and_leftovers_are_removed() {
        let dir = temp("swap");
        let (from, to) = (dir.join("new"), dir.join("app"));
        std::fs::create_dir_all(from.join("tools").join("x")).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join(EXE), "new exe").unwrap();
        std::fs::write(from.join("tools").join("x").join("x.exe"), "new tool").unwrap();
        std::fs::write(to.join(EXE), "old exe").unwrap();
        std::fs::write(to.join("keep.txt"), "user").unwrap();
        let list = [PathBuf::from(EXE), Path::new("tools").join("x").join("x.exe")];
        swap(&from, &to, &list).unwrap();
        assert_eq!(std::fs::read_to_string(to.join(EXE)).unwrap(), "new exe");
        assert_eq!(std::fs::read_to_string(to.join("tools").join("x").join("x.exe")).unwrap(), "new tool");
        assert_eq!(std::fs::read_to_string(old_name(&to.join(EXE))).unwrap(), "old exe");
        remove_leftovers_in(&to);
        assert!(!old_name(&to.join(EXE)).exists());
        assert!(to.join("keep.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_swap_puts_everything_back() {
        let dir = temp("rollback");
        let (from, to) = (dir.join("new"), dir.join("app"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join(EXE), "new exe").unwrap();
        std::fs::write(to.join(EXE), "old exe").unwrap();
        // The second file is missing from the unpacked folder: its move fails.
        let list = [PathBuf::from(EXE), PathBuf::from("missing.dll")];
        assert!(swap(&from, &to, &list).is_err());
        assert_eq!(std::fs::read_to_string(to.join(EXE)).unwrap(), "old exe");
        assert!(!old_name(&to.join(EXE)).exists());
        assert!(!to.join("missing.dll").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Downloads the latest release from GitHub (network, ~90 MB) and installs it over a
    /// running program named mediaopti.exe in a temp folder, as if this were version 0.0.1.
    #[test]
    #[ignore = "downloads the latest release from GitHub"]
    fn installs_the_latest_release_over_a_running_app() {
        let dir = std::env::var_os("MEDIAOPTI_UPDATE_TEST_DIR").map_or_else(|| temp("e2e"), PathBuf::from);
        let app = dir.join("app");
        let _ = std::fs::remove_dir_all(&app);
        std::fs::create_dir_all(&app).unwrap();
        let ping = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32").join("PING.EXE");
        std::fs::copy(&ping, app.join(EXE)).unwrap();
        let mut running = std::process::Command::new(app.join(EXE)).args(["-n", "60", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        let release = check_against(&dir, "0.0.1").unwrap().expect("a release");
        let result = install_into(&release, &app);
        let _ = running.kill();
        let _ = running.wait();
        result.unwrap();
        let installed = std::fs::read(app.join(EXE)).unwrap();
        assert!(installed.starts_with(b"MZ") && installed != std::fs::read(&ping).unwrap());
        assert!(old_name(&app.join(EXE)).exists(), "the running program was set aside");
        assert!(app.join("ffmpeg.exe").is_file() && app.join("tools").is_dir());
        assert!(!app.join(STAGE).exists());
        remove_leftovers_in(&app);
        assert!(!old_name(&app.join(EXE)).exists());
        println!("installed {} into {}", release.version, app.display());
    }

    #[test]
    fn unpack_checks_the_program() {
        use std::io::Write;
        let dir = temp("unpack");
        let zip_path = dir.join("r.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("mediaopti.exe", opts).unwrap();
        writer.write_all(b"MZ program").unwrap();
        writer.start_file("tools\\flac\\flac.exe", opts).unwrap();
        writer.write_all(b"MZ flac").unwrap();
        writer.finish().unwrap();
        let list = unpack(&zip_path, &dir.join("files")).unwrap();
        assert_eq!(list.len(), 2);
        assert!(dir.join("files").join("tools").join("flac").join("flac.exe").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
