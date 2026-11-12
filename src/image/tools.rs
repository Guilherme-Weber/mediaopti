//! Finding the external optimizers and running them (low priority, killed on Stop or
//! timeout, tied to the app's kill-on-exit job).

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Image optimizers used when present. ffmpeg (bundled) is always used for verification.
pub const NAMES: &[&str] = &[
    "beeim", "pingo", "oxipng", "ect", "jpegtran", "djpeg", "cjpeg", "cjpegli", "gifsicle", "ssimulacra2", "flac", "optivorbis", "7z", "qpdf", "mutool",
];

#[derive(Clone, Debug, Default)]
pub struct Tools {
    paths: HashMap<&'static str, PathBuf>,
    pub ffmpeg: PathBuf,
}

impl Tools {
    /// Looks each tool up in: `<data_dir>\tools.json` (`{"pingo": "C:\\...\\pingo.exe"}`),
    /// a `tools` folder next to the executable, the project's `bin` folder (development),
    /// then PATH.
    pub fn discover(ffmpeg: PathBuf, data_dir: &Path) -> Self {
        let overrides: HashMap<String, PathBuf> = std::fs::read_to_string(data_dir.join("tools.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(d) = &exe_dir {
            // The release zip keeps each tool in its own folder (with its DLLs and licence).
            let tools = d.join("tools");
            let mut sub: Vec<PathBuf> = std::fs::read_dir(&tools)
                .map(|r| r.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()).collect())
                .unwrap_or_default();
            sub.sort();
            dirs.push(tools);
            dirs.extend(sub);
        }
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("bin"));
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
        let mut paths = HashMap::new();
        for name in NAMES {
            let file = if cfg!(windows) { format!("{name}.exe") } else { (*name).to_string() };
            let found = overrides
                .get(*name)
                .filter(|p| p.is_file())
                .cloned()
                .or_else(|| dirs.iter().map(|d| d.join(&file)).find(|p| p.is_file()));
            if let Some(p) = found {
                paths.insert(*name, p);
            }
        }
        Self { paths, ffmpeg }
    }

    pub fn get(&self, name: &str) -> Option<&Path> {
        self.paths.get(name).map(PathBuf::as_path)
    }

    pub fn has(&self, name: &str) -> bool {
        self.paths.contains_key(name)
    }

    /// e.g. "found: beeim, oxipng, ... | missing: pingo, ect"
    pub fn summary(&self) -> String {
        let (found, missing): (Vec<&str>, Vec<&str>) = NAMES.iter().partition(|n| self.has(n));
        let mut s = format!("Image tools found: {}", if found.is_empty() { "none".into() } else { found.join(", ") });
        if !missing.is_empty() {
            s.push_str(&format!(" | missing: {}", missing.join(", ")));
        }
        s
    }
}

pub enum Capture {
    /// Discard stdout.
    None,
    /// Keep stdout (small outputs).
    Bytes,
    /// Keep stdout up to this many bytes; a tool writing more is stopped and fails (big
    /// decodes must never be held in memory whole: running out of commit aborts the app).
    BytesUpTo(usize),
    /// SHA-256 of stdout, streamed (large decoder outputs).
    Hash,
}

#[derive(Debug)]
pub struct Ran {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub hash: Option<[u8; 32]>,
    /// Bytes written to stdout (whatever the capture).
    pub stdout_len: u64,
    /// The exit code (qpdf: 0 = fine, 3 = warnings, 2 = errors).
    pub code: Option<i32>,
}

/// Stop and Pause for the external tools: Stop kills a running tool, Pause suspends it
/// (time spent paused doesn't count toward its timeout).
#[derive(Clone, Debug, Default)]
pub struct Gate {
    pub cancel: CancellationToken,
    pub paused: Option<tokio::sync::watch::Receiver<bool>>,
}

impl Gate {
    pub fn new(cancel: CancellationToken, paused: Option<tokio::sync::watch::Receiver<bool>>) -> Self {
        Self { cancel, paused }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.as_ref().is_some_and(|r| *r.borrow())
    }

    /// Blocks while paused; false if Stop was pressed meanwhile.
    pub fn wait_while_paused(&self) -> bool {
        while self.is_paused() {
            if self.is_cancelled() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        !self.is_cancelled()
    }
}

/// Progress inside one job (a comic's pages, a PDF's images), shown next to its name.
#[derive(Debug, Default)]
pub struct Progress {
    pub done: std::sync::atomic::AtomicUsize,
    pub total: std::sync::atomic::AtomicUsize,
}

impl Progress {
    pub fn set_total(&self, n: usize) {
        self.total.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn step(&self) {
        self.done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// (done, total), or `None` before a total is known.
    pub fn get(&self) -> Option<(usize, usize)> {
        let total = self.total.load(std::sync::atomic::Ordering::Relaxed);
        (total > 0).then(|| (self.done.load(std::sync::atomic::Ordering::Relaxed).min(total), total))
    }
}

/// CPU slots shared by the image workers: each file being processed holds one, and a job
/// with many parts (a comic's pages) borrows the idle ones, so the parts run in parallel
/// without ever running more tools at once than there are workers.
#[derive(Debug)]
pub struct Slots {
    free: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
}

impl Slots {
    pub fn new(n: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { free: std::sync::Mutex::new(n), freed: std::sync::Condvar::new() })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, usize> {
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
            free = self.freed.wait_timeout(free, Duration::from_millis(200)).map_or_else(|e| e.into_inner().0, |r| r.0);
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

#[derive(Debug)]
pub enum RunError {
    Cancelled,
    Failed(String),
}

/// Runs a tool to completion, killing it on Stop or after `timeout`, suspending it while
/// paused.
pub fn run(
    program: &Path,
    args: Vec<OsString>,
    cwd: Option<&Path>,
    capture: Capture,
    timeout: Duration,
    gate: &Gate,
) -> Result<Ran, RunError> {
    if gate.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| RunError::Failed(format!("could not start {}: {e}", program.display())))?;
    crate::system::assign_std_child_to_job(&child);

    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let overflowed = std::sync::Arc::clone(&overflow);
    let out_thread = std::thread::spawn(move || {
        let mut buf = vec![0u8; 1 << 16];
        let (mut bytes, mut hasher, mut len) = (Vec::new(), Sha256::new(), 0u64);
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    len += n as u64;
                    match capture {
                        Capture::None => {}
                        Capture::Bytes => bytes.extend_from_slice(&buf[..n]),
                        Capture::BytesUpTo(limit) if bytes.len() + n > limit => {
                            overflowed.store(true, std::sync::atomic::Ordering::Relaxed);
                            bytes = Vec::new();
                            break; // the pipe closes: the tool stops writing
                        }
                        Capture::BytesUpTo(_) => bytes.extend_from_slice(&buf[..n]),
                        Capture::Hash => hasher.update(&buf[..n]),
                    }
                }
            }
        }
        let hash = matches!(capture, Capture::Hash).then(|| hasher.finalize().into());
        (bytes, hash, len)
    });
    // Drained so a chatty tool never blocks on a full pipe.
    let err_thread = std::thread::spawn(move || {
        let _ = std::io::copy(&mut err, &mut std::io::sink());
    });

    let started = Instant::now();
    let (mut suspended_at, mut paused_for) = (None::<Instant>, Duration::ZERO);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        let pause = gate.is_paused() && !gate.is_cancelled();
        if pause != suspended_at.is_some() && crate::system::set_std_process_suspended(&child, pause) {
            match suspended_at.take() {
                Some(t) => paused_for += t.elapsed(),
                None => suspended_at = Some(Instant::now()),
            }
        }
        let active = started.elapsed().saturating_sub(paused_for + suspended_at.map_or(Duration::ZERO, |t| t.elapsed()));
        if overflow.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            return Ok(Ran { success: false, stdout: Vec::new(), hash: None, stdout_len: 0, code: None });
        }
        if gate.is_cancelled() || active > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            return if gate.is_cancelled() {
                Err(RunError::Cancelled)
            } else {
                Err(RunError::Failed(format!("{} timed out", program.display())))
            };
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let (stdout, hash, stdout_len) = out_thread.join().unwrap_or_default();
    let _ = err_thread.join();
    let success = status.is_some_and(|s| s.success()) && !overflow.load(std::sync::atomic::Ordering::Relaxed);
    Ok(Ran { success, stdout, hash, stdout_len, code: status.and_then(|s| s.code()) })
}

/// `OsString` arguments from anything path- or string-like.
#[macro_export]
macro_rules! os_args {
    ($($a:expr),* $(,)?) => {
        vec![$(std::ffi::OsString::from($a)),*]
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A paused tool is suspended, and the time paused doesn't count toward its timeout.
    #[test]
    fn pause_suspends_a_tool_without_timing_it_out() {
        let ffmpeg = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
        if !ffmpeg.exists() {
            eprintln!("skipping: bundled ffmpeg not found");
            return;
        }
        let (tx, rx) = tokio::sync::watch::channel(true);
        let gate = Gate::new(CancellationToken::new(), Some(rx));
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(6));
            tx.send_replace(false);
        });
        let t = Instant::now();
        // 1 s of real-time work, paused for the first 6 s: it ends after the pause (it was
        // suspended) and isn't killed by the 5 s limit (paused time doesn't count; the rest is
        // margin for a busy machine).
        let args = crate::os_args!["-v", "error", "-re", "-f", "lavfi", "-i", "anullsrc=d=1", "-f", "null", "-"];
        let ran = run(&ffmpeg, args, None, Capture::None, Duration::from_secs(5), &gate);
        resume.join().unwrap();
        assert!(ran.as_ref().is_ok_and(|r| r.success), "{ran:?}");
        assert!(t.elapsed() >= Duration::from_millis(5900), "it ran while paused: {:?}", t.elapsed());
    }

    #[test]
    fn slots_are_borrowed_and_returned() {
        let s = Slots::new(3);
        let g = Gate::default();
        assert!(s.acquire(&g));
        assert_eq!(s.borrow_idle(5), 2);
        assert_eq!(s.borrow_idle(5), 0);
        s.release(2);
        assert!(s.acquire(&g) && s.acquire(&g));
        g.cancel.cancel();
        assert!(!s.acquire(&g), "Stop ends the wait");
    }
}
