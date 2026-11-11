//! OS integration: keeping the PC awake, tying ffmpeg children to the app's lifetime,
//! memory checks, single-instance locking and Windows-friendly file operations.
//!
//! Everything in here that touches the disk is blocking; async callers run it through
//! `spawn_blocking` so slow disks or network shares never stall the worker threads.

use std::fs::{self, File, FileTimes};
use std::io::Read;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

/// Prevents system sleep while alive (like Electron's 'prevent-app-suspension').
/// SetThreadExecutionState is per-thread, so a dedicated thread holds the request.
pub struct PowerGuard {
    release: Option<mpsc::Sender<()>>,
}

impl PowerGuard {
    pub fn acquire() -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::System::Power::{
                    SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
                };
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
                let _ = rx.recv();
                SetThreadExecutionState(ES_CONTINUOUS);
            }
            #[cfg(not(windows))]
            let _ = rx.recv();
        });
        Self { release: Some(tx) }
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(());
        }
    }
}

/// A Windows job object with KILL_ON_JOB_CLOSE: every ffmpeg process assigned to it is
/// terminated automatically when the app exits, even if it crashes.
#[cfg(windows)]
mod job {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    static JOB: OnceLock<usize> = OnceLock::new();

    fn handle() -> usize {
        *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            job as usize
        })
    }

    pub fn assign(process: std::os::windows::io::RawHandle) {
        let job = handle();
        if job != 0 && !process.is_null() {
            unsafe {
                AssignProcessToJobObject(job as _, process as _);
            }
        }
    }
}

pub fn assign_to_kill_on_exit_job(child: &tokio::process::Child) {
    #[cfg(windows)]
    if let Some(h) = child.raw_handle() {
        job::assign(h);
    }
    #[cfg(not(windows))]
    let _ = child;
}

/// Same as [`assign_to_kill_on_exit_job`] for a `std::process::Child` (image tools).
pub fn assign_std_child_to_job(child: &std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        job::assign(child.as_raw_handle());
    }
    #[cfg(not(windows))]
    let _ = child;
}

#[cfg(windows)]
#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtSuspendProcess(process: *mut core::ffi::c_void) -> i32;
    fn NtResumeProcess(process: *mut core::ffi::c_void) -> i32;
}

/// Freezes (or thaws) every thread of a child process, which is how Pause works: ffmpeg
/// keeps its state and output file and simply continues where it was. Returns success.
pub fn set_process_suspended(child: &tokio::process::Child, suspended: bool) -> bool {
    #[cfg(windows)]
    if let Some(h) = child.raw_handle() {
        // SAFETY: `h` is the live process handle owned by `child` (full access rights).
        let status = unsafe {
            if suspended {
                NtSuspendProcess(h as _)
            } else {
                NtResumeProcess(h as _)
            }
        };
        return status >= 0;
    }
    let _ = (child, suspended);
    false
}

/// [`set_process_suspended`] for a `std::process::Child` (image and audio tools).
pub fn set_std_process_suspended(child: &std::process::Child, suspended: bool) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let h = child.as_raw_handle();
        // SAFETY: `h` is the live process handle owned by `child` (full access rights).
        let status = unsafe {
            if suspended {
                NtSuspendProcess(h as _)
            } else {
                NtResumeProcess(h as _)
            }
        };
        status >= 0
    }
    #[cfg(not(windows))]
    {
        let _ = (child, suspended);
        false
    }
}

/// Free space (for this user) on the drive holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut free = 0u64;
        // SAFETY: `wide` is NUL-terminated; the out pointer is valid for the call.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// Gives a file or folder the hidden attribute (best effort).
pub fn set_hidden(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let Ok(meta) = fs::metadata(path) else { return };
        let current = std::os::windows::fs::MetadataExt::file_attributes(&meta);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and lives through the call.
        unsafe { windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(wide.as_ptr(), current | 0x2) };
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// Whether a process with this id is running.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
            CloseHandle(h);
            alive
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        true
    }
}

/// Removes work folders and files (`mediaopti-img-<pid>-<n>`, `mediaopti-vmaf-<pid>-<n>.json`)
/// left in `dir` by runs that were killed or crashed. Only names of that exact shape whose
/// process is gone are touched. Returns how many were removed.
pub fn clean_stale_work(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let me = std::process::id();
    let mut removed = 0;
    for e in entries.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        let pid = ["mediaopti-img-", "mediaopti-vmaf-"].iter().find_map(|p| {
            let rest = name.strip_prefix(p)?;
            let (pid, tail) = rest.split_once('-')?;
            let tail = tail.strip_suffix(".json").unwrap_or(tail);
            (!tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())).then_some(pid.parse::<u32>().ok()?)
        });
        let Some(pid) = pid else { continue };
        if pid == me || process_alive(pid) {
            continue;
        }
        let path = e.path();
        let gone = if path.is_dir() { fs::remove_dir_all(&path) } else { remove_file_if_exists(&path) };
        removed += usize::from(gone.is_ok());
    }
    removed
}

/// Hides the console window. Encoders run at below-normal priority: the desktop stays
/// responsive, but unlike idle priority the encode doesn't stall completely while
/// something else (a game, a build) keeps the CPU busy.
pub fn configure_background_command(cmd: &mut tokio::process::Command, low_priority: bool) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let flags = if low_priority {
            CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
        } else {
            CREATE_NO_WINDOW
        };
        cmd.creation_flags(flags);
    }
    #[cfg(not(windows))]
    let _ = (cmd, low_priority);
}

/// Free physical memory and free commit (RAM + page file) in bytes.
#[derive(Clone, Copy, Debug)]
pub struct MemoryStatus {
    pub available_physical: u64,
    pub available_commit: u64,
}

pub fn memory_status() -> Option<MemoryStatus> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return None;
        }
        Some(MemoryStatus {
            available_physical: status.ullAvailPhys,
            available_commit: status.ullAvailPageFile,
        })
    }
    #[cfg(not(windows))]
    None
}

/// Holds a named mutex so only one mediaopti runs at a time (two instances would
/// append to the same history file and fight over the same folders).
pub struct SingleInstance {
    #[cfg(windows)]
    _handle: usize,
}

impl SingleInstance {
    /// Returns None when another instance already holds the lock.
    pub fn acquire(name: &str) -> Option<Self> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
            use windows_sys::Win32::System::Threading::CreateMutexW;
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let handle = CreateMutexW(std::ptr::null(), 0, wide.as_ptr());
            if handle.is_null() {
                // Can't tell; don't block the user.
                return Some(Self { _handle: 0 });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return None;
            }
            // Intentionally kept open for the life of the process.
            Some(Self { _handle: handle as usize })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Some(Self {})
        }
    }
}

fn clear_readonly(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
}

/// Deletes a file if present, clearing the read-only attribute first.
pub fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    clear_readonly(path);
    fs::remove_file(path)
}

/// Moves a file to the Recycle Bin (restorable from there). Succeeds only if the file is
/// really gone from its folder. If Windows cannot recycle it (Recycle Bin disabled or full,
/// file too big for it), it asks the user instead of silently deleting it for good; if the
/// user declines, the error kind is `Interrupted`.
pub fn recycle(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::sync::Mutex;
        use windows_sys::Win32::UI::Shell::{
            SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
            FOF_WANTNUKEWARNING, FO_DELETE, SHFILEOPSTRUCTW,
        };
        // The shell API is not meant to be driven from several threads at once.
        static LOCK: Mutex<()> = Mutex::new(());
        let full = std::path::absolute(path)?;
        // A list with a single entry: the path, then two NULs.
        let from: Vec<u16> = full.as_os_str().encode_wide().chain([0, 0]).collect();
        let mut op = SHFILEOPSTRUCTW {
            hwnd: std::ptr::null_mut(),
            wFunc: FO_DELETE,
            pFrom: from.as_ptr(),
            pTo: std::ptr::null(),
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING) as u16,
            fAnyOperationsAborted: 0,
            hNameMappings: std::ptr::null_mut(),
            lpszProgressTitle: std::ptr::null(),
        };
        let rc = {
            let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: `op` and the buffers it points to are valid for the whole call.
            unsafe { SHFileOperationW(&mut op) }
        };
        if op.fAnyOperationsAborted != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("moving {} to the Recycle Bin was cancelled", path.display()),
            ));
        }
        if rc != 0 || path.exists() {
            return Err(std::io::Error::other(format!(
                "could not move {} to the Recycle Bin (code {rc})",
                path.display()
            )));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(std::io::Error::other("the Recycle Bin is only available on Windows"))
    }
}

/// Number of hard links to a file (1 for a normal file). Replacing a hard-linked file
/// would silently split it from its other names, so such files are left alone.
pub fn hard_link_count(path: &Path) -> std::io::Result<u32> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let file = File::open(path)?;
        // SAFETY: all-zero is a valid value for this plain C struct.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle stays open for the call; `info` is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.nNumberOfLinks)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(path)?.nlink() as u32)
    }
}

/// What to do with an original that is being replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposal {
    Delete,
    RecycleBin,
    /// Move it here (parent folders are created; an existing file is never overwritten).
    MoveTo(std::path::PathBuf),
}

/// Claims a new file name by creating it empty, only if no file has it (atomic). Two jobs
/// converting to the same name (`a.bmp` and `a.tga` both to `a.png`) can't both win: the new
/// version then replaces only its own placeholder, never someone else's file.
pub fn claim_new_name(path: &Path) -> std::io::Result<()> {
    fs::OpenOptions::new().write(true).create_new(true).open(path).map(drop)
}

/// Removes a placeholder made by [`claim_new_name`] if it is still empty.
pub fn release_claim(path: &Path) {
    if fs::metadata(path).is_ok_and(|m| m.len() == 0) {
        let _ = fs::remove_file(path);
    }
}

impl Disposal {
    /// The same disposal for another file in the same folder (a backup goes next to the
    /// original's backup).
    pub fn for_sibling(&self, other: &Path) -> Disposal {
        match self {
            Disposal::MoveTo(dest) => Disposal::MoveTo(dest.with_file_name(other.file_name().unwrap_or_default())),
            d => d.clone(),
        }
    }
}

/// Recycles, moves or deletes a file as `disposal` says (a duplicate being removed).
pub fn dispose(path: &Path, disposal: &Disposal) -> std::io::Result<()> {
    match disposal {
        Disposal::RecycleBin => {
            clear_readonly(path);
            recycle(path)
        }
        Disposal::MoveTo(dest) => move_file(path, dest).map(drop),
        Disposal::Delete => remove_file_if_exists(path),
    }
}

/// `path`, or `path` with " (2)", " (3)"... before the extension if it exists.
pub fn unused_name(path: &Path) -> std::path::PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// Moves a file, also across drives (copy, keep its dates, then delete). Returns where it went.
pub fn move_file(from: &Path, to: &Path) -> std::io::Result<std::path::PathBuf> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let to = unused_name(to);
    if fs::rename(from, &to).is_ok() {
        return Ok(to);
    }
    let attrs = OriginalAttributes::from_metadata(&fs::metadata(from)?);
    fs::copy(from, &to)?;
    restore_attributes(&to, &attrs)?;
    clear_readonly(from);
    if let Err(err) = fs::remove_file(from) {
        let _ = remove_file_if_exists(&to);
        return Err(err);
    }
    Ok(to)
}

/// Puts `tmp` in place of `original` (named `final_target`, which differs when the container
/// changes), coping with read-only originals (common on phone backups) and Explorer or
/// antivirus briefly holding a handle. The original is recycled, moved or deleted as
/// `disposal` says; recycling and moving happen first, and if they fail nothing is changed.
/// Blocking: sleeps between retries.
pub fn replace_file_with_retry(
    tmp: &Path,
    final_target: &Path,
    original: &Path,
    disposal: &Disposal,
) -> std::io::Result<()> {
    const MAX_ATTEMPTS: u64 = 6;
    let mut attempt = 1;
    let mut set_aside = false;
    loop {
        let result = (|| -> std::io::Result<()> {
            if !set_aside {
                match disposal {
                    Disposal::RecycleBin => {
                        clear_readonly(original);
                        recycle(original)?;
                    }
                    // Moved with its read-only flag and dates intact.
                    Disposal::MoveTo(dest) => {
                        move_file(original, dest)?;
                    }
                    Disposal::Delete => {}
                }
                set_aside = true;
            }
            clear_readonly(original);
            if final_target != original {
                clear_readonly(final_target);
            }
            if let Err(err) = fs::rename(tmp, final_target) {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    fs::copy(tmp, final_target)?;
                    fs::remove_file(tmp)?;
                } else {
                    return Err(err);
                }
            }
            if final_target != original && *disposal == Disposal::Delete {
                let _ = remove_file_if_exists(original);
            }
            Ok(())
        })();

        match result {
            Ok(()) => return Ok(()),
            // The user declined deleting a file that could not be recycled: don't ask again.
            Err(err) if attempt >= MAX_ATTEMPTS || err.kind() == std::io::ErrorKind::Interrupted => {
                return Err(err)
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(250 * attempt));
                attempt += 1;
            }
        }
    }
}

/// Dates and attributes (read-only, hidden, system, not indexed) of the original file,
/// restored on the recompressed copy.
pub struct OriginalAttributes {
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub readonly: bool,
    /// Hidden, system and not-content-indexed flags (Windows file attributes).
    pub flags: u32,
}

/// FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED.
const KEPT_FLAGS: u32 = 0x2 | 0x4 | 0x2000;

impl OriginalAttributes {
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            created: meta.created().ok(),
            accessed: meta.accessed().ok(),
            modified: meta.modified().ok(),
            readonly: meta.permissions().readonly(),
            #[cfg(windows)]
            flags: std::os::windows::fs::MetadataExt::file_attributes(meta) & KEPT_FLAGS,
            #[cfg(not(windows))]
            flags: 0,
        }
    }
}

/// Restores creation, access and modification dates, then the read-only flag.
pub fn restore_attributes(path: &Path, attrs: &OriginalAttributes) -> std::io::Result<()> {
    clear_readonly(path);
    {
        let file = File::options().write(true).open(path)?;
        let mut ft = FileTimes::new();
        if let Some(a) = attrs.accessed {
            ft = ft.set_accessed(a);
        }
        if let Some(m) = attrs.modified {
            ft = ft.set_modified(m);
        }
        #[cfg(windows)]
        if let Some(c) = attrs.created {
            use std::os::windows::fs::FileTimesExt;
            ft = ft.set_created(c);
        }
        file.set_times(ft)?;
    }
    if attrs.readonly {
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(windows)]
    if attrs.flags != 0 {
        use std::os::windows::ffi::OsStrExt;
        let current = std::os::windows::fs::MetadataExt::file_attributes(&fs::metadata(path)?);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and lives through the call.
        if unsafe { windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(wide.as_ptr(), current | attrs.flags) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn local_time() -> (u16, u16, u16, u16, u16, u16) {
    #[cfg(windows)]
    unsafe {
        let mut t = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        let t: windows_sys::Win32::Foundation::SYSTEMTIME = t;
        (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
    }
    #[cfg(not(windows))]
    {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (1970, 1, 1, (secs / 3600 % 24) as u16, (secs / 60 % 60) as u16, (secs % 60) as u16)
    }
}

/// Local wall-clock time as HH:MM:SS, for log lines.
pub fn local_time_hms() -> String {
    let (_, _, _, h, m, s) = local_time();
    format!("{h:02}:{m:02}:{s:02}")
}

/// Local date and time as YYYY-MM-DD HH:MM:SS, for log files.
pub fn local_datetime() -> String {
    let (y, mo, d, h, mi, s) = local_time();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Compact local timestamp for file names: YYYYMMDD-HHMMSS.
pub fn local_timestamp_compact() -> String {
    let (y, mo, d, h, mi, s) = local_time();
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Returns true only for real animated GIFs: at least 16x16 and more than one frame.
/// Static 1-frame GIFs and spacer GIFs (DOT.GIF, spacer.gif) return false.
/// Reads in chunks and stops at the second frame marker, so large GIFs aren't loaded whole.
/// Graphic Control Extension block header in a GIF stream.
const GIF_GCE_MARKER: [u8; 3] = [0x21, 0xF9, 0x04];

pub fn is_animated_and_valid_gif(mut reader: impl Read) -> bool {
    let mut header = [0u8; 13];
    if reader.read_exact(&mut header).is_err()
        || !(header.starts_with(b"GIF89a") || header.starts_with(b"GIF87a"))
    {
        return false;
    }
    let width = u16::from_le_bytes([header[6], header[7]]);
    let height = u16::from_le_bytes([header[8], header[9]]);
    if width < 16 || height < 16 {
        return false;
    }

    // Count Graphic Control Extension blocks: more than one means animated.
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry = 0usize; // bytes kept from the previous chunk so markers can span chunks
    let mut found = 0;
    loop {
        let n = match reader.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let filled = carry + n;
        let window = &buf[..filled];
        let mut i = 0;
        while i + 3 <= filled {
            if window[i..i + 3] == GIF_GCE_MARKER {
                found += 1;
                if found >= 2 {
                    return true;
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        // Keep the last two bytes (a marker can't start later than that).
        carry = filled.min(2);
        buf.copy_within(filled - carry..filled, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn gif(w: u16, h: u16, frames: usize, padding_between: usize) -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend(w.to_le_bytes());
        g.extend(h.to_le_bytes());
        g.extend([0u8; 3]);
        g.extend(vec![0u8; 20]);
        for _ in 0..frames {
            g.extend(vec![0u8; padding_between]);
            g.extend([0x21, 0xF9, 0x04]);
        }
        g
    }

    #[test]
    fn gif_detection() {
        assert!(!is_animated_and_valid_gif(Cursor::new(gif(32, 32, 1, 10))));
        assert!(is_animated_and_valid_gif(Cursor::new(gif(32, 32, 2, 10))));
        assert!(!is_animated_and_valid_gif(Cursor::new(gif(1, 1, 5, 10)))); // spacer
        assert!(!is_animated_and_valid_gif(Cursor::new(b"not a gif at all".to_vec())));
    }

    #[test]
    fn gif_markers_split_across_chunks_are_found() {
        // Second marker straddles the 64 KiB chunk boundary.
        let mut g = gif(64, 64, 1, 0);
        let pad = 64 * 1024 - (g.len() - 13) - 1;
        g.extend(vec![0u8; pad]);
        g.extend([0x21, 0xF9, 0x04]);
        assert!(is_animated_and_valid_gif(Cursor::new(g)));
    }

    #[test]
    fn restores_dates_and_readonly_flag() {
        let path = std::env::temp_dir().join(format!("bvp-attrs-{}.bin", std::process::id()));
        std::fs::write(&path, b"data").unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
        let attrs = OriginalAttributes {
            created: Some(old),
            accessed: Some(old),
            modified: Some(old),
            readonly: true,
            flags: KEPT_FLAGS & !0x4, // hidden + not indexed
        };
        restore_attributes(&path, &attrs).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.modified().unwrap(), old);
        assert!(meta.permissions().readonly());
        #[cfg(windows)]
        assert_eq!(std::os::windows::fs::MetadataExt::file_attributes(&meta) & KEPT_FLAGS, 0x2 | 0x2000, "hidden and not-indexed kept");
        remove_file_if_exists(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    #[ignore = "moves a temp file into the user's Recycle Bin"]
    fn recycles_and_replaces() {
        let dir = std::env::temp_dir().join(format!("mo-recycle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (orig, tmp) = (dir.join("clip.mp4"), dir.join("clip.mediaopti.w1.mp4"));
        fs::write(&orig, b"original").unwrap();
        fs::write(&tmp, b"new").unwrap();
        replace_file_with_retry(&tmp, &orig, &orig, &Disposal::RecycleBin).unwrap();
        assert_eq!(fs::read(&orig).unwrap(), b"new");
        assert!(!tmp.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn moves_originals_to_a_backup_folder_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("mo-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let backup = dir.join("backup").join("sub").join("clip.mp4");
        for round in 1..=2 {
            let (orig, tmp) = (dir.join("clip.mp4"), dir.join("clip.mediaopti.w1.mp4"));
            fs::write(&orig, format!("original {round}")).unwrap();
            fs::write(&tmp, b"new").unwrap();
            replace_file_with_retry(&tmp, &orig, &orig, &Disposal::MoveTo(backup.clone())).unwrap();
            assert_eq!(fs::read(&orig).unwrap(), b"new");
        }
        assert_eq!(fs::read_to_string(&backup).unwrap(), "original 1");
        assert_eq!(fs::read_to_string(backup.with_file_name("clip (2).mp4")).unwrap(), "original 2");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_work_folders_of_dead_runs_are_removed() {
        let dir = std::env::temp_dir().join(format!("mo-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        // 4294967 is far above any real process id on Windows.
        let dead = ["mediaopti-img-4294967-3", "mediaopti-vmaf-4294967-0.json"];
        let kept = [format!("mediaopti-img-{me}-1"), "mediaopti-img-4294967-x".into(), "mediaopti-img-notes".into(), "my-mediaopti-img-4294967-3".into()];
        for d in dead.iter().map(|s| (*s).to_string()).chain(kept.iter().cloned()) {
            if d.ends_with(".json") {
                fs::write(dir.join(&d), b"{}").unwrap();
            } else {
                fs::create_dir_all(dir.join(&d).join("sub")).unwrap();
            }
        }
        assert_eq!(clean_stale_work(&dir), 2);
        assert!(dead.iter().all(|d| !dir.join(d).exists()));
        assert!(kept.iter().all(|d| dir.join(d).exists()), "only dead runs' exact names are removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_name_is_claimed_once() {
        let dir = std::env::temp_dir().join(format!("mo-claim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("a.png");
        let (a, b) = std::thread::scope(|s| {
            let a = s.spawn(|| claim_new_name(&target).is_ok());
            let b = s.spawn(|| claim_new_name(&target).is_ok());
            (a.join().unwrap(), b.join().unwrap())
        });
        assert!(a ^ b, "exactly one job gets the name");
        fs::write(dir.join("b.png"), b"user file").unwrap();
        assert!(claim_new_name(&dir.join("b.png")).is_err());
        release_claim(&dir.join("b.png"));
        assert_eq!(fs::read(dir.join("b.png")).unwrap(), b"user file", "never removes a real file");
        release_claim(&target);
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_instance_lock_is_exclusive() {
        let name = format!("Local\\mediaopti-test-{}", std::process::id());
        let first = SingleInstance::acquire(&name);
        assert!(first.is_some());
        assert!(SingleInstance::acquire(&name).is_none());
    }
}
