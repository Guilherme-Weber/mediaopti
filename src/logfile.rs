//! Persistent diagnostics in %APPDATA%\mediaopti\logs:
//! - `mediaopti.log`: the activity log, rotated to `mediaopti.1.log` at 5 MB;
//! - `failures.log`: durable append-only record of every failure line (never rotated away,
//!   so long runs that rotate `mediaopti.log` never lose their failure causes);
//! - `crash-<timestamp>.txt`: written by the panic hook for any panic (release builds have no
//!   console, so this is the only trace of one). A panic in a file's job fails that file only.

use crate::system;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub struct FileLog {
    path: PathBuf,
    rotated: PathBuf,
    failures_path: PathBuf,
    state: Mutex<Option<(File, u64)>>,
    failures: Mutex<Option<File>>,
}

/// Whether `message` is a failure line that must survive log rotation.
pub(crate) fn is_failure_message(message: &str) -> bool {
    let body = match message.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((_, body)) => body,
        None => message,
    };
    body.starts_with("Failed")
        || body.starts_with("Stopped after an ffmpeg startup error")
        || body.contains("stopped unexpectedly:")
}

impl FileLog {
    pub fn open(dir: &Path) -> Self {
        let _ = fs::create_dir_all(dir);
        let path = dir.join("mediaopti.log");
        let rotated = dir.join("mediaopti.1.log");
        let failures_path = dir.join("failures.log");
        let state = Self::open_file(&path);
        Self {
            path,
            rotated,
            failures_path,
            state: Mutex::new(state),
            failures: Mutex::new(None),
        }
    }

    fn open_file(path: &Path) -> Option<(File, u64)> {
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        let size = file.metadata().map_or(0, |m| m.len());
        Some((file, size))
    }

    /// Appends one line, prefixed with the local date and time. Never fails loudly:
    /// logging must not break processing. Failure lines are also persisted to `failures.log`
    /// without rotation.
    pub fn write_line(&self, message: &str) {
        let line = format!("{} {message}\r\n", system::local_datetime());
        {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.as_ref().is_some_and(|(_, size)| *size + line.len() as u64 > MAX_LOG_BYTES) {
                *state = None; // close before renaming
                let _ = fs::rename(&self.path, &self.rotated);
                *state = Self::open_file(&self.path);
            }
            if let Some((file, size)) = state.as_mut() {
                if file.write_all(line.as_bytes()).is_ok() {
                    *size += line.len() as u64;
                }
            }
        }
        if is_failure_message(message) {
            let mut failures = self.failures.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if failures.is_none() {
                *failures = OpenOptions::new().create(true).append(true).open(&self.failures_path).ok();
            }
            if let Some(file) = failures.as_mut() {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }
}

/// Writes a report for any panic, then lets the default hook run. A panic in a file's job
/// only fails that file (the app goes on); the report is there to find the bug.
pub fn install_panic_hook(dir: PathBuf) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("crash-{}.txt", system::local_timestamp_compact()));
        let thread = std::thread::current();
        let (base, frames) = return_addresses();
        let report = format!(
            "mediaopti {}: internal error (panic) at {}\r\nthread: {}\r\n{info}\r\n\r\nbacktrace:\r\n{}\r\n\
             addresses (mediaopti.exe at {base:#x}; with the release's saved .pdb, address - base = offset):\r\n{}\r\n",
            env!("CARGO_PKG_VERSION"),
            system::local_datetime(),
            thread.name().unwrap_or("unnamed"),
            std::backtrace::Backtrace::force_capture(),
            frames.iter().map(|a| format!("{a:#x}")).collect::<Vec<_>>().join(" "),
        );
        let _ = fs::write(&path, report);
        default_hook(info);
    }));
}

/// The executable's load address and the current thread's return addresses. Releases ship
/// without their .pdb (it names the build machine's paths), so the report's function names
/// stop at the standard library; these let the release's saved .pdb place every frame.
fn return_addresses() -> (usize, Vec<usize>) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        let mut frames = [std::ptr::null_mut(); 62];
        // SAFETY: the buffer holds 62 pointers, as passed; no hash is asked for.
        let n = unsafe { RtlCaptureStackBackTrace(0, 62, frames.as_mut_ptr(), std::ptr::null_mut()) };
        // SAFETY: a null name asks for the executable's own module, which is always loaded.
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        (base, frames[..usize::from(n)].iter().map(|&f| f as usize).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report's raw addresses include frames of the executable itself (after its base).
    #[cfg(windows)]
    #[test]
    fn return_addresses_point_into_the_executable() {
        let (base, frames) = return_addresses();
        assert!(base != 0 && frames.len() > 2);
        assert!(frames.iter().any(|&f| f > base && f - base < 256 << 20), "{base:#x} {frames:x?}");
    }

    #[test]
    fn appends_and_rotates() {
        let dir = std::env::temp_dir().join(format!("bvp-logs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = FileLog::open(&dir);
        log.write_line("first line");
        assert!(fs::read_to_string(dir.join("mediaopti.log")).unwrap().contains("first line"));

        // Force a rotation by pretending the file is already full.
        log.state.lock().unwrap().as_mut().unwrap().1 = MAX_LOG_BYTES;
        log.write_line("after rotation");
        assert!(fs::read_to_string(dir.join("mediaopti.1.log")).unwrap().contains("first line"));
        let current = fs::read_to_string(dir.join("mediaopti.log")).unwrap();
        assert!(current.contains("after rotation") && !current.contains("first line"));
        assert!(!dir.join("failures.log").exists(), "normal lines do not create failures.log");

        // Failure lines are persisted to failures.log and survive multiple rotations.
        log.write_line("[Worker 1] Failed on clip.mp4: corrupt");
        log.state.lock().unwrap().as_mut().unwrap().1 = MAX_LOG_BYTES;
        log.write_line("rotate 1");
        log.state.lock().unwrap().as_mut().unwrap().1 = MAX_LOG_BYTES;
        log.write_line("[Images] Failed on broken.png: damaged");
        let fails = fs::read_to_string(dir.join("failures.log")).unwrap();
        assert!(fails.contains("Failed on clip.mp4") && fails.contains("Failed on broken.png") && !fails.contains("rotate 1"));
        let _ = fs::remove_dir_all(&dir);
    }
}
