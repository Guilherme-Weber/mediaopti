//! Persistent diagnostics in %APPDATA%\mediaopti\logs:
//! - `mediaopti.log`: the activity log, rotated to `mediaopti.1.log` at 5 MB;
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
    state: Mutex<Option<(File, u64)>>,
}

impl FileLog {
    pub fn open(dir: &Path) -> Self {
        let _ = fs::create_dir_all(dir);
        let path = dir.join("mediaopti.log");
        let rotated = dir.join("mediaopti.1.log");
        let state = Self::open_file(&path);
        Self {
            path,
            rotated,
            state: Mutex::new(state),
        }
    }

    fn open_file(path: &Path) -> Option<(File, u64)> {
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        let size = file.metadata().map_or(0, |m| m.len());
        Some((file, size))
    }

    /// Appends one line, prefixed with the local date and time. Never fails loudly:
    /// logging must not break processing.
    pub fn write_line(&self, message: &str) {
        let line = format!("{} {message}\r\n", system::local_datetime());
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
    #[cfg(not(windows))]
    {
        (0, Vec::new())
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
        let _ = fs::remove_dir_all(&dir);
    }
}
