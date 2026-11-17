//! Tool versions for the About page, and new versions of pingo and beeim.
//!
//! The bundled tools change only with a mediaopti release (each is tested with it). pingo and
//! beeim are downloaded from their author's site, which publishes no version number or
//! checksum: once a week the app downloads them into a staging folder, reads the version
//! their banner prints and offers the newer ones. Nothing is replaced without the user.

use crate::image::tools::{Tools, DOWNLOADED, NAMES};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often css-ig.net is asked for new pingo and beeim versions.
const CHECK_EVERY: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    pub name: &'static str,
    /// "unknown" when the tool prints none and no record has it.
    pub version: String,
    /// Where it comes from: "included", "downloaded" or "on this PC".
    pub source: &'static str,
}

/// A newer pingo or beeim, downloaded and waiting in the staging folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newer {
    pub name: &'static str,
    pub have: Option<String>,
    pub available: String,
}

/// Arguments that make a tool print its version; `None`: it prints none.
fn version_args(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "oxipng" | "gifsicle" | "flac" | "qpdf" | "optivorbis" => Some(&["--version"]),
        "jpegtran" | "djpeg" | "cjpeg" | "ffmpeg" => Some(&["-version"]),
        "mutool" => Some(&["-v"]),
        // Their banner (printed with no arguments) holds the version.
        "ect" | "7z" | "pingo" | "beeim" => Some(&[]),
        _ => None,
    }
}

/// The version in a tool's output: the first "1.2" or "1.2.3", with pingo's "[rc3]" kept.
pub fn parse_version(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"\bv?(\d+\.\d+(?:\.\d+)?)(?:\s*\[(\w+)\])?").ok()?;
    let c = re.captures(text)?;
    Some(match c.get(2) {
        Some(tag) => format!("{} {}", &c[1], tag.as_str()),
        None => c[1].to_string(),
    })
}

/// Orders versions: numbers first, then a final release after its "rc" ones.
fn version_key(v: &str) -> (Vec<u32>, u32) {
    let mut parts = v.split(' ');
    let nums = parts.next().unwrap_or_default().split('.').filter_map(|n| n.parse().ok()).collect();
    let stage = parts.next().map_or(u32::MAX, |tag| tag.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0));
    (nums, stage)
}

pub fn is_newer(candidate: &str, installed: &str) -> bool {
    version_key(candidate) > version_key(installed)
}

/// Runs `program args` and returns what it printed (both streams), killed after `limit`.
fn output_of(program: &Path, args: &[&str], limit: Duration) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // no console window
    }
    let mut child = cmd.spawn().ok()?;
    let (mut out, mut err) = (child.stdout.take()?, child.stderr.take()?);
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut a = Vec::new();
        let _ = out.read_to_end(&mut a);
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        a.extend(b);
        a
    });
    let start = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if start.elapsed() > limit {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.wait();
    reader.join().ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// The version `program` reports about itself.
pub fn version_of(name: &str, program: &Path) -> Option<String> {
    let args = version_args(name)?;
    parse_version(&output_of(program, args, Duration::from_secs(5))?)
}

/// The version recorded in a `versions.json` next to the tool or one folder up (the release
/// zip has `tools\versions.json`, written by scripts/package.ps1).
fn recorded_version(name: &str, program: &Path) -> Option<String> {
    program.ancestors().skip(1).take(2).find_map(|d| {
        let text = std::fs::read_to_string(d.join("versions.json")).ok()?;
        let map: HashMap<String, String> = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        map.get(name).cloned()
    })
}

/// The version Scoop installed the tool as (`scoop-manifest.json` up to three folders up).
fn scoop_version(program: &Path) -> Option<String> {
    program.ancestors().skip(1).take(3).find_map(|d| {
        let text = std::fs::read_to_string(d.join("scoop-manifest.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v.get("version")?.as_str().map(str::to_string)
    })
}

/// Every tool found, with its version and where it comes from. Blocking (runs the tools,
/// all at once; about a second).
pub fn list(tools: &Tools, data_dir: &Path) -> Vec<ToolVersion> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let included_dir = exe_dir.as_ref().map(|d| d.join("tools"));
    let downloaded_dir = data_dir.join("tools");
    let mut found: Vec<(&'static str, PathBuf)> = vec![("ffmpeg", tools.ffmpeg.clone())];
    found.extend(NAMES.iter().filter_map(|n| tools.get(n).map(|p| (*n, p.to_path_buf()))));
    std::thread::scope(|s| {
        let handles: Vec<_> = found
            .iter()
            .map(|(name, path)| {
                let (included_dir, downloaded_dir, exe_dir) = (&included_dir, &downloaded_dir, &exe_dir);
                s.spawn(move || {
                    let version = version_of(name, path)
                        .or_else(|| recorded_version(name, path))
                        .or_else(|| scoop_version(path))
                        .unwrap_or_else(|| "unknown".into());
                    let source = if included_dir.as_ref().is_some_and(|d| path.starts_with(d))
                        || (*name == "ffmpeg" && exe_dir.as_ref().is_some_and(|d| path.parent() == Some(d.as_path())))
                    {
                        "included"
                    } else if path.starts_with(downloaded_dir) {
                        "downloaded"
                    } else {
                        "on this PC"
                    };
                    ToolVersion { name, version, source }
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

fn staging(data_dir: &Path) -> PathBuf {
    data_dir.join("tools").join("new")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Looks for newer pingo and beeim versions: downloads them into the staging folder when
/// `force` or a week has passed since the last look, then compares the staged ones with
/// the installed ones (older or equal staged copies are removed). Blocking.
pub fn check_downloaded(tools: &Tools, data_dir: &Path, force: bool) -> Result<Vec<Newer>, String> {
    let stage = staging(data_dir);
    let stamp = stage.join("checked.txt");
    let last = std::fs::read_to_string(&stamp).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    let mut errors = Vec::new();
    if force || now_secs().saturating_sub(last) >= CHECK_EVERY.as_secs() {
        std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        for (name, url) in DOWNLOADED {
            if let Err(why) = crate::image::tools::download_tool(name, url, &stage) {
                errors.push(format!("{name}: {why}"));
            }
        }
        if errors.len() < DOWNLOADED.len() {
            let _ = std::fs::write(&stamp, now_secs().to_string());
        }
    }
    let mut newer = Vec::new();
    for (name, _) in DOWNLOADED {
        let staged = stage.join(format!("{name}.exe"));
        if !staged.is_file() {
            continue;
        }
        let Some(available) = version_of(name, &staged) else {
            let _ = std::fs::remove_file(&staged);
            continue;
        };
        let have = tools.get(name).and_then(|p| version_of(name, p));
        if have.as_deref().is_none_or(|h| is_newer(&available, h)) {
            newer.push(Newer { name, have, available });
        } else {
            let _ = std::fs::remove_file(&staged);
        }
    }
    if newer.is_empty() && errors.len() == DOWNLOADED.len() {
        return Err(errors.join("; "));
    }
    Ok(newer)
}

/// Puts the staged newer versions in place (the app's own tools folder, which is searched
/// before the project and PATH folders). Returns what was installed.
pub fn install_newer(data_dir: &Path) -> Result<Vec<&'static str>, String> {
    let (stage, dir) = (staging(data_dir), data_dir.join("tools"));
    let mut done = Vec::new();
    for (name, _) in DOWNLOADED {
        let staged = stage.join(format!("{name}.exe"));
        if staged.is_file() {
            std::fs::rename(&staged, dir.join(format!("{name}.exe"))).map_err(|e| format!("{name}: {e}"))?;
            done.push(*name);
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_and_ordered() {
        let cases = [
            ("oxipng 10.2.1", "10.2.1"),
            ("LCDF Gifsicle 1.97\nCopyright (C) 1997-2025 Eddie Kohler", "1.97"),
            ("7-Zip 26.06 (x64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-11-06", "26.06"),
            ("Efficient Compression Tool\n(c) 2014-2024 Felix Hanau.\nVersion 0.9.5 compiled on Jan  7 2024", "0.9.5"),
            ("  ----\n  pingo v1.29 [rc3] - experimental web image optimizer", "1.29 rc3"),
            ("ffmpeg version 9.0.2-essentials_build-www.gyan.dev Copyright (c) 2000-2026", "9.0.2"),
            ("OptiVorbis v0.3.2 (2026-08-09)", "0.3.2"),
            ("mozjpeg version 4.1.5 (build 20231110)", "4.1.5"),
        ];
        for (text, want) in cases {
            assert_eq!(parse_version(text).as_deref(), Some(want), "{text}");
        }
        assert!(is_newer("1.30", "1.29 rc3") && is_newer("1.29", "1.29 rc3") && is_newer("1.29 rc4", "1.29 rc3"));
        assert!(!is_newer("1.29 rc3", "1.29") && !is_newer("0.29", "0.29") && !is_newer("1.9", "1.10"));
    }

    /// Downloads pingo and beeim from css-ig.net into a temporary data folder and compares
    /// them with the installed ones (network).
    #[test]
    #[ignore = "downloads from css-ig.net"]
    fn checks_css_ig_for_new_versions() {
        let ffmpeg = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
        let dir = std::env::temp_dir().join(format!("mo-newtools-{}", std::process::id()));
        let tools = Tools::discover(ffmpeg, &dir);
        let newer = check_downloaded(&tools, &dir, true).unwrap();
        println!("installed: pingo {:?}, beeim {:?}", tools.get("pingo").and_then(|p| version_of("pingo", p)), tools.get("beeim").and_then(|p| version_of("beeim", p)));
        println!("newer: {newer:?}");
        for (name, _) in DOWNLOADED {
            let staged = staging(&dir).join(format!("{name}.exe"));
            assert_eq!(staged.is_file(), newer.iter().any(|n| n.name == *name), "{name}: staged only when newer");
        }
        // A week hasn't passed: nothing is downloaded again.
        let before = std::fs::read_to_string(staging(&dir).join("checked.txt")).unwrap();
        let _ = check_downloaded(&tools, &dir, false).unwrap();
        assert_eq!(std::fs::read_to_string(staging(&dir).join("checked.txt")).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_tools_report_their_versions() {
        let ffmpeg = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
        if !ffmpeg.exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("mo-toolver-{}", std::process::id()));
        let tools = Tools::discover(ffmpeg, &dir);
        let list = list(&tools, &dir);
        let ff = list.iter().find(|t| t.name == "ffmpeg").unwrap();
        assert!(ff.version.chars().next().is_some_and(|c| c.is_ascii_digit()), "{ff:?}");
        for t in &list {
            println!("{} {} ({})", t.name, t.version, t.source);
        }
    }
}
