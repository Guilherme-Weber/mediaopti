//! Parsing of ffmpeg's stderr: progress (`frame= ... time= ... speed=`) and failure diagnostics.

use regex::Regex;
use std::sync::LazyLock;
use std::time::Instant;

static DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Duration:\s*(\d{2}:\d{2}:\d{2}\.\d+)").unwrap());
static FPS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"fps=\s*([\d.]+)").unwrap());
static SPEED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"speed=\s*([\d.]+)x").unwrap());
static TIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"time=\s*(\d{2}:\d{2}:\d{2}\.\d+)").unwrap());
static NO_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Failed to set value '0:[vV]:0' for option 'map'|Stream map '0:[vV]:0' matches no streams|Output file does not contain any stream").unwrap()
});
static NVENC_UNSUPPORTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Frame dimensions are less than the minimum supported value|InitializeEncoder failed|Error while opening encoder|Error reinitializing filters").unwrap()
});
/// GPU memory (NVENC buffers, CUDA frames) or system memory ran out: the file is fine; it
/// is tried again once another encoder has finished. A decoder that can't get a frame
/// buffer says "get_buffer() failed", then "Invalid data found" (as for a corrupt file),
/// drops the frame and goes on: ffmpeg may still exit 0 with frames missing.
static OUT_OF_MEMORY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)out of memory|Cannot allocate memory|get_buffer\(\) failed|Memory allocation error|Failed to allocate").unwrap()
});
/// The frame size or format changed mid-stream and ffmpeg rebuilt the filters (only
/// allowed on the GPU path; the CPU path runs with `-reinit_filter 0`).
static RECONFIGURED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Reconfiguring filter graph").unwrap());
static CORRUPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Invalid data found when processing input|Error opening input files: End of file|moov atom not found").unwrap()
});
static ERROR_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)error|invalid|failed|corrupt|cannot|unable|could not write header").unwrap()
});

/// ffmpeg's ` @ 000001d9544f8a40` after a component name: different on every run.
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" @ (0x)?[0-9a-fA-F]+\]").unwrap());

#[derive(Debug, Default)]
pub struct Diagnostics {
    pub no_video_stream: bool,
    pub nvenc_unsupported: bool,
    pub corrupt_input: bool,
    /// Ran out of GPU or system memory (also matches `nvenc_unsupported`; this one wins).
    pub out_of_memory: bool,
    /// The filters were rebuilt mid-stream (frame size changed).
    pub reconfigured: bool,
    pub error_lines: Vec<String>,
    /// The same error lines in ffmpeg's own words, without the worker prefix and memory
    /// addresses, each once (see [`Diagnostics::reason`]).
    messages: Vec<String>,
    /// Input duration in seconds as reported by ffmpeg (0 if unknown).
    pub duration_sec: f64,
    /// Trailing partial line carried over to the next stderr chunk.
    carry: String,
}

#[derive(Debug, Clone)]
pub struct WorkerProgress {
    pub file: String,
    pub fps: f64,
    pub speed: f64,
    pub duration_sec: f64,
    pub percent: f64,
    pub last_encoded_sec: f64,
    pub last_progress_at: Instant,
    /// What the worker does now ("encoding", "measuring quality at +4"...).
    pub step: String,
    /// When the worker started this file.
    pub started: Instant,
}

impl WorkerProgress {
    pub fn new(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            fps: 0.0,
            speed: 0.0,
            duration_sec: 0.0,
            percent: 0.0,
            last_encoded_sec: 0.0,
            last_progress_at: Instant::now(),
            step: "encoding".to_string(),
            started: Instant::now(),
        }
    }
}

impl Diagnostics {
    /// What ffmpeg said went wrong, for a log line: its first two different error lines,
    /// e.g. "[indeo5] Corrupted tile data encountered!; [indeo5] Error while decoding band:
    /// 0, plane: 0". `None` when it printed none.
    pub fn reason(&self) -> Option<String> {
        let text = self.messages.iter().take(2).cloned().collect::<Vec<_>>().join("; ");
        (!text.is_empty()).then(|| text.chars().take(300).collect())
    }
}

fn hms_to_seconds(hms: &str) -> f64 {
    let parts: Vec<&str> = hms.trim().split(':').collect();
    if parts.len() != 3 {
        return 0.0;
    }
    let p = |s: &str| s.parse::<f64>().unwrap_or(0.0);
    p(parts[0]) * 3600.0 + p(parts[1]) * 60.0 + p(parts[2])
}

/// Feeds one stderr chunk into the worker's progress and diagnostics.
/// Returns true when a progress line was seen (so the UI should refresh).
pub fn parse_chunk(
    progress: &mut WorkerProgress,
    diag: &mut Diagnostics,
    worker_id: u32,
    display_name: &str,
    chunk: &str,
) -> bool {
    let mut text = std::mem::take(&mut diag.carry);
    text.push_str(chunk);

    // Keep an unterminated last line for the next chunk so lines split across reads still match.
    let complete_len = text.rfind(['\r', '\n']).map_or(0, |i| i + 1);
    diag.carry = text[complete_len..].to_string();
    if diag.carry.len() > 4096 {
        diag.carry.clear();
    }
    let text = &text[..complete_len];

    if progress.duration_sec == 0.0 {
        if let Some(m) = DURATION.captures(text) {
            progress.duration_sec = hms_to_seconds(&m[1]);
        }
    }
    if NO_VIDEO.is_match(text) {
        diag.no_video_stream = true;
    }
    if NVENC_UNSUPPORTED.is_match(text) {
        diag.nvenc_unsupported = true;
    }
    if OUT_OF_MEMORY.is_match(text) {
        diag.out_of_memory = true;
    }
    if RECONFIGURED.is_match(text) {
        diag.reconfigured = true;
    }
    if CORRUPT.is_match(text) {
        diag.corrupt_input = true;
    }

    let mut saw_progress = false;
    for line in text.split(['\r', '\n']).filter(|l| !l.is_empty()) {
        if line.contains("frame=") && line.contains("time=") {
            if let Some(m) = FPS.captures(line) {
                progress.fps = m[1].parse().unwrap_or(0.0);
            }
            if let Some(m) = SPEED.captures(line) {
                progress.speed = m[1].parse().unwrap_or(0.0);
            }
            if let Some(m) = TIME.captures(line) {
                let encoded = hms_to_seconds(&m[1]);
                // Only reset the stall watchdog if the video timestamp actually moved forward.
                if encoded > progress.last_encoded_sec {
                    progress.last_encoded_sec = encoded;
                    progress.last_progress_at = Instant::now();
                }
                if progress.duration_sec > 0.0 {
                    progress.percent = (encoded / progress.duration_sec * 100.0).clamp(0.0, 99.0);
                }
            }
            saw_progress = true;
        } else if ERROR_LINE.is_match(line)
            && !line.contains("Input #0")
            && !line.contains("Output #0")
            && !diag.no_video_stream
        {
            let trimmed = line.trim();
            if !trimmed.is_empty() && diag.error_lines.len() < 15 {
                diag.error_lines
                    .push(format!("[W{worker_id}: {display_name}] {trimmed}"));
            }
            let message = ADDRESS.replace_all(trimmed, "]").into_owned();
            if !message.is_empty() && diag.messages.len() < 5 && !diag.messages.contains(&message) {
                diag.messages.push(message);
            }
        }
    }
    saw_progress
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_across_split_chunks() {
        let mut p = WorkerProgress::new("a.mp4");
        let mut d = Diagnostics::default();
        parse_chunk(&mut p, &mut d, 1, "a.mp4", "  Duration: 00:00:10.00, start: 0\n");
        assert_eq!(p.duration_sec, 10.0);
        assert!(!parse_chunk(&mut p, &mut d, 1, "a.mp4", "frame=  120 fps= 60 q=30.0 size=  1kB time=00:00:0"));
        assert!(parse_chunk(&mut p, &mut d, 1, "a.mp4", "5.00 bitrate=1k speed=2.5x\r"));
        assert_eq!(p.fps, 60.0);
        assert_eq!(p.speed, 2.5);
        assert_eq!(p.percent, 50.0);
    }

    #[test]
    fn repeated_timestamp_does_not_reset_stall_watchdog() {
        // FreeYourself.wmv froze at 5% while ffmpeg kept printing the same time=.
        let mut p = WorkerProgress::new("a.wmv");
        let mut d = Diagnostics::default();
        parse_chunk(&mut p, &mut d, 1, "a.wmv", "frame=  140 fps= 12 time=00:00:05.80 speed=0.5x\r");
        let first = p.last_progress_at;
        std::thread::sleep(std::time::Duration::from_millis(20));
        parse_chunk(&mut p, &mut d, 1, "a.wmv", "frame=  140 fps= 12 time=00:00:05.80 speed=0.5x\r");
        assert_eq!(p.last_progress_at, first, "a frozen timestamp must not count as progress");
        parse_chunk(&mut p, &mut d, 1, "a.wmv", "frame=  150 fps= 12 time=00:00:06.20 speed=0.5x\r");
        assert!(p.last_progress_at > first);
    }

    #[test]
    fn detects_diagnostics() {
        let mut p = WorkerProgress::new("a.mp4");
        let mut d = Diagnostics::default();
        parse_chunk(&mut p, &mut d, 2, "a.mp4", "[mov] moov atom not found\nError opening input file\n");
        assert!(d.corrupt_input);
        assert_eq!(d.error_lines.len(), 1);
        assert!(d.error_lines[0].starts_with("[W2: a.mp4] Error opening"));

        // The reason in ffmpeg's words, each line once, without addresses (Latest1.avi).
        let mut d = Diagnostics::default();
        let indeo = "[indeo5 @ 000001d9544f8a40] Corrupted tile data encountered!\n[indeo5 @ 000001d9544f8a40] Error while decoding band: 0, plane: 0\n";
        parse_chunk(&mut p, &mut d, 1, "Latest1.avi", &format!("{indeo}{indeo}[vist#0:0/indeo5 @ 0x1] Error submitting packet to decoder: Invalid data found when processing input\n"));
        assert!(d.corrupt_input);
        assert_eq!(d.reason().as_deref(), Some("[indeo5] Corrupted tile data encountered!; [indeo5] Error while decoding band: 0, plane: 0"));
        assert_eq!(Diagnostics::default().reason(), None);

        // Seen with 6-8 parallel 4K encodes: not a file the GPU can't encode.
        for text in [
            "[av1_nvenc @ 0] CreateInputBuffer failed: out of memory (10): \n[vost#0:0/av1_nvenc] Error while opening encoder\n",
            "[CUDA @ 0] cu->cuMemAlloc(&data, size) failed -> CUDA_ERROR_OUT_OF_MEMORY: out of memory\n",
            "[vf#0:0 @ 0] Error while filtering: Cannot allocate memory\n",
            "[libaom-av1 @ 0] Failed to decode frame: Memory allocation error\n[dec:libaom-av1 @ 0] Error submitting packet to decoder: Invalid data found when processing input\n",
            "[h264 @ 0] get_buffer() failed\n[h264 @ 0] thread_get_buffer() failed\n[dec:h264 @ 0] Decoding error: Invalid data found when processing input\n",
        ] {
            let mut d = Diagnostics::default();
            parse_chunk(&mut p, &mut d, 1, "a", text);
            assert!(d.out_of_memory, "{text}");
        }

        let mut d = Diagnostics::default();
        parse_chunk(&mut p, &mut d, 1, "a", "Stream map '0:V:0' matches no streams.\n");
        assert!(d.no_video_stream);

        let mut d = Diagnostics::default();
        parse_chunk(&mut p, &mut d, 1, "a", "[av1_nvenc] InitializeEncoder failed: invalid param (8)\n");
        assert!(d.nvenc_unsupported);
    }
}
