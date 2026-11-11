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
    "beeim", "pingo", "oxipng", "ect", "jpegtran", "djpeg", "cjpeg", "cjpegli", "gifsicle", "ssimulacra2", "flac",
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
            dirs.push(d.join("tools"));
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
    /// SHA-256 of stdout, streamed (large decoder outputs).
    Hash,
}

#[derive(Debug)]
pub struct Ran {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub hash: Option<[u8; 32]>,
}

#[derive(Debug)]
pub enum RunError {
    Cancelled,
    Failed(String),
}

/// Runs a tool to completion, killing it on `cancel` or after `timeout`.
pub fn run(
    program: &Path,
    args: Vec<OsString>,
    cwd: Option<&Path>,
    capture: Capture,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Ran, RunError> {
    if cancel.is_cancelled() {
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
    let out_thread = std::thread::spawn(move || {
        let mut buf = vec![0u8; 1 << 16];
        let (mut bytes, mut hasher) = (Vec::new(), Sha256::new());
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => match capture {
                    Capture::None => {}
                    Capture::Bytes => bytes.extend_from_slice(&buf[..n]),
                    Capture::Hash => hasher.update(&buf[..n]),
                },
            }
        }
        let hash = matches!(capture, Capture::Hash).then(|| hasher.finalize().into());
        (bytes, hash)
    });
    // Drained so a chatty tool never blocks on a full pipe.
    let err_thread = std::thread::spawn(move || {
        let _ = std::io::copy(&mut err, &mut std::io::sink());
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if cancel.is_cancelled() || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            return if cancel.is_cancelled() {
                Err(RunError::Cancelled)
            } else {
                Err(RunError::Failed(format!("{} timed out", program.display())))
            };
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let (stdout, hash) = out_thread.join().unwrap_or_default();
    let _ = err_thread.join();
    Ok(Ran { success: status.is_some_and(|s| s.success()), stdout, hash })
}

/// `OsString` arguments from anything path- or string-like.
#[macro_export]
macro_rules! os_args {
    ($($a:expr),* $(,)?) => {
        vec![$(std::ffi::OsString::from($a)),*]
    };
}
