//! Option normalization and ffmpeg command-line construction.

use std::path::{Path, PathBuf};

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mts", "m2ts", "flv", "m4v", "mkv", "mov", "webm", "avi", "wmv", "ts", "3gp", "mpg",
    "mpeg", "gif",
];

/// Legacy/non-MP4 containers that remux cleanly into .mp4
const REMUX_TO_MP4_EXTS: &[&str] = &[
    "mts", "m2ts", "flv", "webm", "m4v", "mov", "avi", "wmv", "ts", "3gp", "mpg", "mpeg", "gif",
];


const ALLOWED_CRF: &[u32] = &[20, 23, 26];
const ALLOWED_PRESETS: &[&str] = &[
    "ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow",
];
const ALLOWED_ENCODERS: &[&str] = &[
    "av1_nvenc",
    "hevc_nvenc",
    "h264_nvenc",
    "libx264",
];
const ALLOWED_CONCURRENCY: &[u32] = &[1, 2, 3, 4];
/// GIF size limits in MB: GIFs below the limit stay GIFs (the image engine optimizes them).
/// `NO_GIFS` means GIFs are never converted to video.
pub const NO_GIFS: u32 = u32::MAX;
const ALLOWED_GIF_MIN_MB: &[u32] = &[0, 1, 5, NO_GIFS];
/// VMAF floors for the quality-checked fallback; 0 turns it off.
const ALLOWED_VMAF_FLOOR: &[u32] = &[0, 93, 95];

/// What happens to an original once a smaller version replaces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Originals {
    Delete,
    RecycleBin,
    /// Moved into a backup folder that mirrors the scanned folder's structure.
    /// `None`: next to the scanned folder, as `<folder> (originals)`.
    BackupFolder(Option<PathBuf>),
}

/// How images (PNG, JPEG, small GIFs) are optimized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageMode {
    Off,
    /// Pixel-identical only.
    Lossless,
    /// Also lossy, at the lowest quality that keeps SSIMULACRA2 >= 93.
    Lossy,
}

/// Which lossless format conversions are on (each changes the file extension).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conversions {
    /// WAV -> FLAC.
    pub wav: bool,
    /// BMP/TGA -> PNG.
    pub images: bool,
    /// CBR -> CBZ.
    pub cbr: bool,
}

impl Conversions {
    pub const ALL: Self = Self { wav: true, images: true, cbr: true };
    pub const NONE: Self = Self { wav: false, images: false, cbr: false };

    /// From a bit mask: 1 = WAV, 2 = BMP/TGA, 4 = CBR.
    pub fn from_mask(m: u32) -> Self {
        Self { wav: m & 1 != 0, images: m & 2 != 0, cbr: m & 4 != 0 }
    }
}

/// What a video file holds besides its main picture, as `ffmpeg -i` lists it: carried into
/// the new version and checked there (nothing may be lost on the way).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Streams {
    pub audio: usize,
    /// Each audio track's codec and channel count, in order.
    pub audio_codecs: Vec<String>,
    pub audio_channels: Vec<u32>,
    /// Subtitle codecs (`subrip`, `ass`, `mov_text`, `hdmv_pgs_subtitle`...).
    pub subtitles: Vec<String>,
    /// Attachments (fonts and pictures in MKV).
    pub attachments: usize,
    /// Input indexes of cover pictures (`attached pic` video streams).
    pub covers: Vec<usize>,
    pub chapters: usize,
    /// Container duration in seconds.
    pub duration: Option<f64>,
    /// Data tracks by tag (`tmcd` timecode, `qtvr`, `gpmd` telemetry...).
    pub data: Vec<String>,
    /// HDR transfer (PQ or HLG) on the main picture.
    pub hdr: bool,
    /// HDR10 static metadata on the first frame (filled by the engine for HDR files only).
    pub mastering: bool,
    pub light_level: bool,
    /// HDR10+ dynamic metadata on the first frame (filled by the engine, HDR files only).
    pub hdr10_plus: bool,
    pub dolby_vision: bool,
    /// The container's own tags (recording date, place, camera, title...), keys in lower
    /// case; the format's bookkeeping (`major_brand`, `encoder`...) is left out.
    pub tags: Vec<(String, String)>,
}

/// Tags the muxer writes for itself: not the file's content.
const OWN_TAGS: &[&str] = &["major_brand", "minor_version", "compatible_brands", "encoder"];

impl Streams {
    /// The recording date as one value: QuickTime files may list it twice (`a;b`).
    pub fn creation_time(&self) -> Option<&str> {
        self.tags.iter().find(|(k, _)| k == "creation_time").map(|(_, v)| v.split(';').next().unwrap_or_default().trim())
    }
}

/// Audio codecs an MP4 holds as they are (copied, never re-encoded).
const MP4_AUDIO: &[&str] = &["aac", "mp3", "mp2", "opus", "ac3", "eac3", "alac"];

/// Integer PCM: re-encoded to FLAC (lossless, about half the size; checked by decoding).
pub fn is_integer_pcm(codec: &str) -> bool {
    ["pcm_s16", "pcm_s24", "pcm_s32", "pcm_u8", "pcm_s8"].iter().any(|p| codec.starts_with(p))
}

fn channel_count(layout: &str) -> u32 {
    let l = layout.trim();
    if let Some(n) = l.strip_suffix(" channels").and_then(|n| n.trim().parse().ok()) {
        return n;
    }
    match l.split('(').next().unwrap_or_default() {
        "mono" => 1,
        "stereo" | "downmix" => 2,
        "2.1" | "3.0" => 3,
        "quad" | "4.0" | "3.1" => 4,
        "4.1" | "5.0" => 5,
        "5.1" | "6.0" | "hexagonal" => 6,
        "6.1" | "7.0" => 7,
        "7.1" | "octagonal" => 8,
        _ => 2,
    }
}

impl Streams {
    /// Reads the listing ffmpeg prints for `ffmpeg -i <file>` (one input).
    pub fn parse(listing: &str) -> Self {
        let mut s = Streams::default();
        let mut main_video = false;
        // The input's own tags: between "Input #0" and "Duration:".
        let mut in_tags = false;
        for raw in listing.lines() {
            let line = raw.trim();
            if line.starts_with("Input #0") {
                in_tags = true;
                continue;
            }
            if in_tags && !line.starts_with("Duration: ") {
                if let Some((k, v)) = line.split_once(':').filter(|_| line != "Metadata:") {
                    let key = k.trim().to_ascii_lowercase();
                    if key.is_empty() {
                        if let Some(last) = s.tags.last_mut() {
                            last.1.push('\n');
                            last.1.push_str(v.trim());
                        }
                    } else if !OWN_TAGS.contains(&key.as_str()) {
                        s.tags.push((key, v.trim().to_string()));
                    }
                }
                continue;
            }
            in_tags = false;
            if line.contains("DOVI configuration record") {
                s.dolby_vision = true;
            }
            if line.starts_with("Chapter #0:") {
                s.chapters += 1;
            } else if let Some(rest) = line.strip_prefix("Duration: ") {
                let t = rest.split(',').next().unwrap_or_default();
                let p: Vec<f64> = t.split(':').filter_map(|x| x.trim().parse().ok()).collect();
                if p.len() == 3 {
                    s.duration = Some(p[0] * 3600.0 + p[1] * 60.0 + p[2]);
                }
            } else if let Some(rest) = line.strip_prefix("Stream #0:") {
                let index = rest.split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse::<usize>().ok());
                if let Some((_, a)) = rest.split_once(": Audio: ") {
                    s.audio += 1;
                    s.audio_codecs.push(a.split([' ', ',', '(']).next().unwrap_or_default().to_string());
                    let layout = a.split_once(" Hz, ").map_or("", |(_, r)| r.split(',').next().unwrap_or_default());
                    s.audio_channels.push(channel_count(layout));
                } else if let Some((_, d)) = rest.split_once(": Data: ") {
                    // "none (qtvr / 0x72767471)", "bin_data (gpmd / ...)", "none, 0 kb/s"
                    let codec = d.split([' ', ',']).next().unwrap_or_default();
                    let tag = d[codec.len()..].strip_prefix(" (").and_then(|t| t.split([' ', '/', ')']).next()).filter(|t| !t.is_empty());
                    s.data.push(tag.unwrap_or(codec).to_string());
                } else if let Some((_, codec)) = rest.split_once(": Subtitle: ") {
                    s.subtitles.push(codec.split([' ', ',', '(']).next().unwrap_or_default().to_string());
                } else if rest.contains(": Attachment:") {
                    s.attachments += 1;
                } else if rest.contains(": Video:") && rest.contains("(attached pic)") {
                    s.covers.extend(index);
                } else if rest.contains(": Video:") && !main_video {
                    main_video = true;
                    s.hdr = rest.contains("smpte2084") || rest.contains("arib-std-b67");
                }
            }
        }
        s
    }

    /// Whether an MP4 can hold all of it as it is: no attachments, subtitles only in
    /// mov_text (converting others would lose styling or can't be done at all), and audio
    /// it takes without re-encoding (else MKV holds it; integer PCM becomes FLAC there).
    pub fn fits_mp4(&self) -> bool {
        self.attachments == 0
            && self.subtitles.iter().all(|c| c == "mov_text")
            && self.audio_codecs.iter().all(|c| MP4_AUDIO.contains(&c.as_str()))
    }

    /// What makes the file unfit for re-encoding without losing something, if anything:
    /// Dolby Vision (its dynamic metadata can't be re-encoded) or a data track no container
    /// ffmpeg writes can carry (only `tmcd` timecode is rebuilt, in MP4/MOV).
    pub fn blocker(&self, drop_data: bool) -> Option<Blocked> {
        if self.dolby_vision {
            return Some(Blocked { why: "Dolby Vision (its dynamic metadata can't be re-encoded)".into(), permanent: true });
        }
        if self.hdr10_plus {
            return Some(Blocked { why: "HDR10+ (its dynamic metadata can't be re-encoded)".into(), permanent: true });
        }
        if self.data.iter().any(|t| t == "qtvr") {
            // The picture is the panorama's tiles: re-encoded, it is no longer a panorama.
            return Some(Blocked { why: "a QuickTime VR panorama".into(), permanent: true });
        }
        let other = self.dropped_data();
        (!other.is_empty() && !drop_data).then(|| Blocked {
            why: format!("a data track that can't be carried over ({})", other.join(", ")),
            permanent: false,
        })
    }

    /// The data tracks the new version can't hold (everything but the timecode).
    pub fn dropped_data(&self) -> Vec<&str> {
        self.data.iter().map(String::as_str).filter(|t| *t != "tmcd").collect()
    }
}

/// Why a video is left as it is (`Streams::blocker`).
#[derive(Debug, PartialEq, Eq)]
pub struct Blocked {
    pub why: String,
    /// Nothing can change it, so it goes into history; otherwise the "Remove data tracks"
    /// option may let it through in a later run.
    pub permanent: bool,
}

/// The extension of the new version: MP4 for the old containers (and GIFs), else the same as
/// the original; MKV instead of MP4 when the file holds what MP4 can't (`Streams::fits_mp4`).
pub fn output_ext(input: &Path, streams: &Streams) -> String {
    let ext = ext_lower(input);
    let target = if REMUX_TO_MP4_EXTS.contains(&ext.as_str()) { "mp4".to_string() } else { ext };
    if matches!(target.as_str(), "mp4" | "m4v") && !is_gif(input) && !streams.fits_mp4() {
        "mkv".into()
    } else {
        target
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub crf: u32,
    pub preset: String,
    pub encoder: String,
    pub concurrency: u32,
    pub originals: Originals,
    /// Animated GIFs smaller than this stay GIFs (`u64::MAX`: no GIF is converted).
    pub gif_min_bytes: u64,
    /// When the preset output is not smaller, retry with stronger settings and accept the
    /// result only if its VMAF stays at or above this floor. `None`: no retry.
    pub vmaf_floor: Option<f64>,
    /// Added to the CQ/CRF for the quality-checked retry (0 for the normal encode).
    pub cq_boost: u32,
    /// Decode on the GPU (NVDEC) and scale there to this fixed size (set per attempt, only
    /// for inputs whose decoded frames were checked to match the CPU's: see
    /// `Engine::gpu_decodable`). Fixed, because NVENC crops silently when the frame size
    /// changes mid-stream; scaling every frame to the first one's size is what the CPU
    /// path does too.
    pub gpu_decode: Option<(u32, u32)>,
    pub images: ImageMode,
    /// Lossless format conversions (WAV -> FLAC, BMP/TGA -> PNG, CBR -> CBZ), each on or off.
    pub convert: Conversions,
    /// Videos (and animated GIFs over the limit) are processed at all.
    pub videos: bool,
    /// Documents (PDF, Office, EPUB, comics) are processed at all.
    pub docs: bool,
    /// PDF images may be re-encoded lossily (SSIMULACRA2 >= 93, every page rendered and
    /// checked), when images are visually lossless too. Text and vectors are never touched.
    pub lossy_docs: bool,
    /// Audio files (cover art, FLAC/Ogg repacking, WAV -> FLAC): off, lossless, or cover art
    /// visually lossless (the audio itself is always identical).
    pub audio: ImageMode,
    /// Where CPU jobs keep their work files (`None`: the system temp folder).
    pub temp_dir: Option<PathBuf>,
    /// Rename pictures whose extension doesn't match their format (a JPEG named .png).
    pub fix_ext: bool,
    /// Advanced: GPU decoding allowed (see `Engine::gpu_decodable`).
    pub gpu_decoding: bool,
    /// Advanced: the maximum-effort parts in use.
    pub max: crate::image::Max,
    /// Advanced: data tracks no container can carry over (GoPro telemetry, iPhone metadata)
    /// are left out of the new version instead of keeping the video as it is.
    pub drop_data: bool,
    /// Advanced: remove metadata from pictures (EXIF, comments, text chunks; colour profiles
    /// and rotation stay) and padding from FLAC/MP3 tags. Audio tags are never removed.
    pub strip: bool,
    /// Advanced: with `strip`, files processed without it are done again.
    pub redo_stripped: bool,
    /// What the file being encoded holds besides its picture (set per job).
    pub streams: Streams,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            crf: 23,
            preset: "slow".into(),
            encoder: "av1_nvenc".into(),
            concurrency: 4,
            originals: Originals::RecycleBin,
            gif_min_bytes: 1024 * 1024,
            vmaf_floor: Some(93.0),
            cq_boost: 0,
            gpu_decode: None,
            images: ImageMode::Lossy,
            convert: Conversions::ALL,
            videos: true,
            docs: true,
            lossy_docs: true,
            audio: ImageMode::Lossy,
            temp_dir: None,
            fix_ext: false,
            gpu_decoding: true,
            max: crate::image::Max::chosen(),
            drop_data: false,
            strip: false,
            redo_stripped: false,
            streams: Streams::default(),
        }
    }
}

/// Options as sent by the UI; anything missing or invalid falls back to the defaults.
#[derive(Debug, Default)]
pub struct RawOptions {
    pub crf: Option<f64>,
    pub preset: Option<String>,
    pub encoder: Option<String>,
    pub concurrency: Option<f64>,
    /// 0 = delete, 1 = Recycle Bin, 2 = backup folder (`backup_dir`, empty = default).
    pub originals: Option<u32>,
    pub backup_dir: Option<String>,
    pub gif_min_mb: Option<u32>,
    pub vmaf_floor: Option<u32>,
    /// 0 = off, 1 = lossless, 2 = lossy (visually lossless).
    pub images: Option<u32>,
    /// 0 = keep file formats, anything else = all conversions (older settings); see
    /// `conversions` for each one.
    pub convert: Option<u32>,
    /// Conversions as a bit mask (1 = WAV -> FLAC, 2 = BMP/TGA -> PNG, 4 = CBR -> CBZ); wins
    /// over `convert`.
    pub conversions: Option<u32>,
    /// 0 = skip videos.
    pub videos: Option<u32>,
    /// 0 = skip documents.
    pub docs: Option<u32>,
    /// 0 = PDFs lossless only, 1 = visually lossless PDF images.
    pub lossy_docs: Option<u32>,
    /// 0 = off, 1 = lossless, 2 = cover art visually lossless.
    pub audio: Option<u32>,
    /// Folder for work files; empty = the system temp folder.
    pub temp_dir: Option<String>,
    /// 1 = rename misnamed pictures to their real format.
    pub fix_ext: Option<u32>,
    /// Advanced: 0 = never decode on the GPU.
    pub gpu_decoding: Option<u32>,
    /// Advanced: the maximum-effort parts in use (`image::Max` field names); `None` = the
    /// measured choice.
    pub max_parts: Option<Vec<String>>,
    /// Advanced: 1 = leave out data tracks that can't be carried over.
    pub drop_data: Option<u32>,
    /// Advanced: 1 = remove metadata.
    pub strip: Option<u32>,
    /// Advanced: 1 = redo files processed without removing metadata.
    pub redo_stripped: Option<u32>,
}

impl Options {
    pub fn normalize(raw: RawOptions) -> Self {
        let d = Options::default();
        let crf = raw.crf.map(|v| v as u32).filter(|v| ALLOWED_CRF.contains(v));
        let concurrency = raw
            .concurrency
            .map(|v| v as u32)
            .filter(|v| ALLOWED_CONCURRENCY.contains(v));
        let preset = raw.preset.filter(|p| ALLOWED_PRESETS.contains(&p.as_str()));
        let encoder = raw.encoder.filter(|e| ALLOWED_ENCODERS.contains(&e.as_str()));
        let gif_min_bytes = raw
            .gif_min_mb
            .filter(|v| ALLOWED_GIF_MIN_MB.contains(v))
            .map_or(d.gif_min_bytes, |mb| {
                if mb == NO_GIFS { u64::MAX } else { u64::from(mb) * 1024 * 1024 }
            });
        let vmaf_floor = match raw.vmaf_floor.filter(|v| ALLOWED_VMAF_FLOOR.contains(v)) {
            Some(0) => None,
            Some(v) => Some(f64::from(v)),
            None => d.vmaf_floor,
        };
        Self {
            crf: crf.unwrap_or(d.crf),
            preset: preset.unwrap_or(d.preset),
            encoder: encoder.unwrap_or(d.encoder),
            concurrency: concurrency.unwrap_or(d.concurrency),
            originals: match raw.originals {
                Some(0) => Originals::Delete,
                Some(2) => Originals::BackupFolder(
                    raw.backup_dir.filter(|d| !d.trim().is_empty()).map(PathBuf::from),
                ),
                Some(1) => Originals::RecycleBin,
                _ => d.originals,
            },
            gif_min_bytes,
            vmaf_floor,
            cq_boost: 0,
            gpu_decode: None,
            images: match raw.images {
                Some(0) => ImageMode::Off,
                Some(1) => ImageMode::Lossless,
                Some(2) => ImageMode::Lossy,
                _ => d.images,
            },
            convert: match (raw.conversions, raw.convert) {
                (Some(mask), _) => Conversions::from_mask(mask),
                (None, Some(0)) => Conversions::NONE,
                (None, Some(_)) => Conversions::ALL,
                (None, None) => d.convert,
            },
            videos: raw.videos.map_or(d.videos, |v| v != 0),
            docs: raw.docs.map_or(d.docs, |v| v != 0),
            lossy_docs: raw.lossy_docs.map_or(d.lossy_docs, |c| c != 0),
            audio: match raw.audio {
                Some(0) => ImageMode::Off,
                Some(1) => ImageMode::Lossless,
                Some(2) => ImageMode::Lossy,
                _ => d.audio,
            },
            temp_dir: raw.temp_dir.filter(|t| !t.trim().is_empty()).map(PathBuf::from),
            fix_ext: raw.fix_ext.map_or(d.fix_ext, |f| f != 0),
            gpu_decoding: raw.gpu_decoding.map_or(d.gpu_decoding, |g| g != 0),
            max: raw.max_parts.map_or(d.max, |names| crate::image::Max::from_names(&names.iter().map(String::as_str).collect::<Vec<_>>())),
            drop_data: raw.drop_data.map_or(d.drop_data, |v| v != 0),
            strip: raw.strip.map_or(d.strip, |v| v != 0),
            redo_stripped: raw.redo_stripped.map_or(d.redo_stripped, |v| v != 0),
            streams: Streams::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Primary,
    TranscodeAudio,
    CpuFallback,
}

pub fn ext_lower(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

pub fn is_gif(path: &Path) -> bool {
    ext_lower(path) == "gif"
}


pub fn is_video_file(path: &Path) -> bool {
    VIDEO_EXTENSIONS.contains(&ext_lower(path).as_str())
}

/// `<dir>/<stem>.mediaopti.w<id><ext>`, switching legacy containers to `.mp4`.
#[cfg(test)]
pub fn temp_name(input: &Path, worker_id: u32) -> std::path::PathBuf {
    let out_ext = if REMUX_TO_MP4_EXTS.contains(&ext_lower(input).as_str()) {
        "mp4".to_string()
    } else {
        input.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()
    };
    temp_name_with(input, worker_id, &out_ext)
}

/// `<dir>/<stem>.mediaopti.w<id>.<ext>` (see [`output_ext`]).
pub fn temp_name_with(input: &Path, worker_id: u32, ext: &str) -> std::path::PathBuf {
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dot = if ext.is_empty() { "" } else { "." };
    input.with_file_name(format!("{stem}.mediaopti.w{worker_id}{dot}{ext}"))
}

fn map_nvenc_preset(ui_preset: &str) -> &'static str {
    match ui_preset {
        "ultrafast" => "p4",
        "superfast" | "veryfast" => "p5",
        "faster" | "fast" => "p6",
        _ => "p7",
    }
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

pub fn build_ffmpeg_args(input: &Path, tmp: &Path, options: &Options, mode: Mode) -> Vec<String> {
    let input = input.to_string_lossy().into_owned();
    let tmp = tmp.to_string_lossy().into_owned();
    // x264 CRF and NVENC CQ both top out at 51 (AV1 NVENC at 63); the boost only applies to
    // the quality-checked retry.
    let boost = options.cq_boost;
    let crf = options.crf;
    let x264_crf = (crf + boost).min(51);
    let preset = options.preset.as_str();
    let nvenc_preset = map_nvenc_preset(preset);
    let gif = is_gif(Path::new(&input));

    // -reinit_filter 0 prevents mid-stream resolution changes from crashing the filtergraph
    let base = s(&[
        "-hide_banner", "-loglevel", "info", "-stats", "-fflags", "+genpts", "-reinit_filter", "0",
    ]);
    // Ensures dimensions are even and at least 2x2 (never 0x0 on tiny inputs)
    let even_scale = s(&["-vf", "scale=max(2\\,trunc(iw/2)*2):max(2\\,trunc(ih/2)*2)"]);
    let x264_audio = if gif {
        s(&["-an"])
    } else {
        copy_audio(options)
    };

    // Everything else the file holds is carried over: subtitles, attachments (MKV only: the
    // output is MKV whenever there are some, see `output_ext`) and cover pictures, copied.
    let mut keep = Vec::new();
    if !gif {
        keep.extend(s(&["-map", "0:s?", "-c:s", "copy"]));
        if ext_lower(Path::new(&tmp)) == "mkv" {
            keep.extend(s(&["-map", "0:t?"]));
        }
        for (k, index) in options.streams.covers.iter().enumerate() {
            let n = k + 1; // output video 0 is the encoded picture
            keep.extend(["-map".into(), format!("0:{index}"), format!("-c:v:{n}"), "copy".into(), format!("-disposition:v:{n}"), "attached_pic".into()]);
        }
    }

    let mut args = base;

    // Software CPU fallback for tiny GIFs/stickers or streams rejected by NVENC.
    if mode == Mode::CpuFallback {
        args.extend(s(&["-i", &input, "-map", "0:V:0"]));
        args.extend(tag_flags(options));
        args.extend(even_scale);
        args.extend(s(&["-c:v", "libx264", "-preset", "slow", "-crf", &x264_crf.to_string(), "-pix_fmt", "yuv420p"]));
        if gif {
            args.extend(s(&["-an"]));
        } else {
            // Old VFR WMV/AVI files that reach this fallback could stall the muxer
            // (FreeYourself.wmv): constant frame rate and a deep muxing queue. The audio is
            // carried over as in the normal encode (this is a fallback for the picture).
            args.extend(s(&["-fps_mode", "cfr", "-max_muxing_queue_size", "4096"]));
            args.extend(copy_audio(options));
        }
        args.extend(keep.clone());
        args.extend(s(&["-movflags", "+faststart+use_metadata_tags", "-y", &tmp]));
        return streams_specific(args, options);
    }

    let audio_flags = if gif {
        s(&["-an"])
    } else if mode == Mode::TranscodeAudio {
        // The muxer refused the audio as it was: every track re-encoded to AAC, keeping its
        // channels, with timing repaired. 96 kb/s per channel (192 for stereo, 576 for 5.1) is
        // above transparency for ffmpeg's AAC; the source here is old lossy audio (WMA, MP3)
        // at or below that, so the second generation is not audibly worse.
        let mut a = s(&["-fps_mode", "cfr", "-max_muxing_queue_size", "4096", "-map", "0:a?", "-af", "aresample=async=1:first_pts=0", "-c:a", "aac"]);
        for (i, ch) in options.streams.audio_channels.iter().enumerate() {
            a.extend([format!("-b:a:{i}"), format!("{}k", 96 * (*ch).max(1))]);
        }
        if options.streams.audio_channels.is_empty() {
            a.extend(s(&["-b:a", "192k"]));
        }
        a
    } else {
        copy_audio(options)
    };

    let nvenc = |codec: &str, cq: u32, aq: &str, bf: &str, ten_bit: bool| {
        // GPU decode only feeds the 10-bit encoders: scale_cuda converts to P010 there, and
        // for 4:2:0 input it hands the encoder the same frames as the CPU path (checked).
        let gpu = options.gpu_decode.filter(|_| ten_bit);
        let mut a = if gpu.is_some() { s(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]) } else { Vec::new() };
        a.extend(s(&["-i", &input, "-map", "0:V:0"]));
        a.extend(tag_flags(options));
        if let Some((w, h)) = gpu {
            a.extend(s(&["-vf", &format!("scale_cuda=w={w}:h={h}:format=p010le")]));
        } else {
            a.extend(even_scale.clone());
        }
        a.extend(s(&[
            "-c:v", codec, "-preset", nvenc_preset, "-tune", "hq", "-rc", "vbr", "-cq",
            &cq.to_string(), "-b:v", "0", "-multipass", "fullres", "-rc-lookahead", "32",
            "-spatial-aq", "1", "-temporal-aq", "1", "-aq-strength", aq, "-bf", bf,
            "-b_ref_mode", "middle",
        ]));
        if ten_bit && gpu.is_none() {
            a.extend(s(&["-pix_fmt", "p010le"]));
        }
        a.extend(audio_flags.clone());
        a.extend(keep.clone());
        a.extend(s(&["-movflags", "+faststart+use_metadata_tags", "-y", &tmp]));
        a
    };

    match options.encoder.as_str() {
        "av1_nvenc" => {
            let cq = (match crf { 20 => 32, 26 => 40, _ => 36 } + boost).min(63);
            args.extend(nvenc("av1_nvenc", cq, "5", "4", true));
        }
        "hevc_nvenc" => {
            let cq = (match crf { 20 => 26, 26 => 34, _ => 30 } + boost).min(51);
            args.extend(nvenc("hevc_nvenc", cq, "6", "4", true));
        }
        "h264_nvenc" => {
            let cq = (match crf { 20 => 22, 26 => 28, _ => 25 } + boost).min(51);
            args.extend(nvenc("h264_nvenc", cq, "7", "3", false));
        }
        _ => {
            args.extend(s(&["-i", &input, "-map", "0:V:0"]));
            args.extend(even_scale);
            args.extend(s(&["-vcodec", "libx264", "-preset", preset, "-crf", &x264_crf.to_string()]));
            args.extend(tag_flags(options));
            args.extend(x264_audio);
            args.extend(keep);
            args.extend(s(&["-movflags", "+use_metadata_tags", "-y", &tmp]));
        }
    }

    // On the GPU path the filter must be rebuilt when the frame size changes: scale_cuda
    // then scales the new size to the fixed output. Kept as it was, it passes the larger
    // frames on and NVENC encodes only their top-left corner, without any error.
    if args.iter().any(|a| a == "-hwaccel") {
        if let Some(i) = args.iter().position(|a| a == "-reinit_filter") {
            args.drain(i..i + 2);
        }
    }
    streams_specific(args, options)
}

/// The input's tags carried over (with `use_metadata_tags`, MP4/MOV keep every key, e.g.
/// the iPhone's place, make and model), minus the old format's bookkeeping, and the
/// recording date given as one value (else MKV drops a doubled QuickTime date).
fn tag_flags(options: &Options) -> Vec<String> {
    let mut a = s(&["-map_metadata", "0"]);
    for own in &OWN_TAGS[..3] {
        a.extend(["-metadata".into(), format!("{own}=")]);
    }
    if let Some(t) = options.streams.creation_time() {
        a.extend(["-metadata".into(), format!("creation_time={t}")]);
    }
    a
}

/// Every audio track copied as it is; integer PCM becomes FLAC (lossless, about half the
/// size, and checked by decoding before the original is replaced).
fn copy_audio(options: &Options) -> Vec<String> {
    let mut a = s(&["-map", "0:a?", "-c:a", "copy"]);
    for (i, codec) in options.streams.audio_codecs.iter().enumerate() {
        if is_integer_pcm(codec) {
            a.extend([format!("-c:a:{i}"), "flac".into()]);
        }
    }
    a
}

/// With cover pictures copied as extra video streams, the filter and pixel format must name
/// the encoded stream (a filter on a copied stream is an error).
fn streams_specific(mut args: Vec<String>, options: &Options) -> Vec<String> {
    if !options.streams.covers.is_empty() {
        for a in &mut args {
            if a == "-vf" {
                "-filter:v:0".clone_into(a);
            } else if a == "-pix_fmt" {
                "-pix_fmt:v:0".clone_into(a);
            }
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cq_boost_raises_cq_and_crf_with_caps() {
        let p = Path::new(r"C:.mp4");
        let tmp = temp_name(p, 1);
        let mut o = Options { crf: 20, ..Options::default() };
        o.cq_boost = 8;
        assert!(build_ffmpeg_args(p, &tmp, &o, Mode::Primary).join(" ").contains("-cq 40"));
        o.cq_boost = 60;
        assert!(build_ffmpeg_args(p, &tmp, &o, Mode::Primary).join(" ").contains("-cq 63"));
        o.encoder = "libx264".into();
        o.cq_boost = 8;
        assert!(build_ffmpeg_args(p, &tmp, &o, Mode::Primary).join(" ").contains("-crf 28"));
        assert!(build_ffmpeg_args(p, &tmp, &o, Mode::CpuFallback).join(" ").contains("-crf 28"));
    }

    #[test]
    fn normalizes_invalid_options_to_defaults() {
        let o = Options::normalize(RawOptions {
            crf: Some(99.0),
            preset: Some("bogus".into()),
            encoder: Some("rm -rf".into()),
            concurrency: Some(9.0),
            originals: Some(9),
            backup_dir: None,
            gif_min_mb: Some(3),
            vmaf_floor: Some(50),
            images: Some(7),
            convert: None,
            lossy_docs: None,
            audio: Some(9),
            temp_dir: Some("  ".into()),
            fix_ext: None,
            conversions: None,
            videos: None,
            docs: None,
            gpu_decoding: None,
            drop_data: None,
            strip: None,
            redo_stripped: None,
            max_parts: None,
        });
        assert_eq!(o.audio, ImageMode::Lossy, "audio: cover art visually lossless by default");
        assert!(o.temp_dir.is_none() && !o.fix_ext);
        assert_eq!(o.convert, Conversions::ALL, "conversions on by default");
        assert!(o.videos && o.docs);
        assert_eq!(o.images, ImageMode::Lossy, "images: visually lossless by default");
        assert_eq!(o.crf, 23);
        assert_eq!(o.preset, "slow");
        assert_eq!(o.encoder, "av1_nvenc");
        assert_eq!(o.concurrency, 4);
        assert_eq!(o.originals, Originals::RecycleBin, "originals go to the Recycle Bin by default");
        assert_eq!(o.gif_min_bytes, 1024 * 1024);
        assert_eq!(o.vmaf_floor, Some(93.0));

        let o = Options::normalize(RawOptions {
            originals: Some(2),
            backup_dir: Some(r"D:\old".into()),
            gif_min_mb: Some(NO_GIFS),
            vmaf_floor: Some(0),
            ..RawOptions::default()
        });
        assert_eq!(o.originals, Originals::BackupFolder(Some(PathBuf::from(r"D:\old"))));
        assert_eq!(o.gif_min_bytes, u64::MAX);
        assert_eq!(o.vmaf_floor, None);
    }

    #[test]
    fn temp_name_remuxes_legacy_containers() {
        let p = PathBuf::from(r"C:\v\clip.MOV");
        assert_eq!(temp_name(&p, 2), PathBuf::from(r"C:\v\clip.mediaopti.w2.mp4"));
        let p = PathBuf::from(r"C:\v\clip.mkv");
        assert_eq!(temp_name(&p, 1), PathBuf::from(r"C:\v\clip.mediaopti.w1.mkv"));
    }

    #[test]
    fn av1_args_match_electron_version() {
        let o = Options::default();
        let a = build_ffmpeg_args(Path::new("in.mp4"), Path::new("out.mp4"), &o, Mode::Primary);
        let joined = a.join(" ");
        assert!(joined.contains("-c:v av1_nvenc -preset p7 -tune hq -rc vbr -cq 36"));
        assert!(joined.contains("-aq-strength 5 -bf 4 -b_ref_mode middle -pix_fmt p010le"));
        assert!(joined.ends_with("-map 0:a? -c:a copy -map 0:s? -c:s copy -movflags +faststart+use_metadata_tags -y out.mp4"));
    }

    #[test]
    fn gpu_decode_scales_on_the_gpu_for_10_bit_encoders_only() {
        let o = Options { gpu_decode: Some((1920, 1080)), ..Options::default() };
        let a = build_ffmpeg_args(Path::new("in.mp4"), Path::new("out.mp4"), &o, Mode::Primary).join(" ");
        assert!(a.contains("-hwaccel cuda -hwaccel_output_format cuda -i in.mp4"), "{a}");
        assert!(a.contains("-vf scale_cuda=w=1920:h=1080:format=p010le"), "{a}");
        // The filter may be rebuilt when the size changes (it scales to the fixed size).
        assert!(!a.contains("-reinit_filter 0"), "{a}");
        assert!(!a.contains("-pix_fmt") && !a.contains("-vf scale="), "{a}");
        // h264_nvenc (8-bit) and the CPU fallback keep decoding on the CPU.
        let o = Options { gpu_decode: Some((1920, 1080)), encoder: "h264_nvenc".into(), ..Options::default() };
        let a = build_ffmpeg_args(Path::new("in.mp4"), Path::new("out.mp4"), &o, Mode::Primary).join(" ");
        assert!(!a.contains("hwaccel"), "{a}");
        let o = Options { gpu_decode: Some((1920, 1080)), ..Options::default() };
        let a = build_ffmpeg_args(Path::new("in.mp4"), Path::new("out.mp4"), &o, Mode::CpuFallback).join(" ");
        assert!(!a.contains("hwaccel"), "{a}");
    }

    /// Writes args for every option combination to $ARGS_DUMP (used for parity checks
    /// against the old Electron implementation). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "writes a parity dump; needs ARGS_DUMP"]
    fn dump_all_args() {
        let Ok(out) = std::env::var("ARGS_DUMP") else { return };
        let mut rows = Vec::new();
        for encoder in ALLOWED_ENCODERS {
            for crf in ALLOWED_CRF {
                for preset in ["slow", "medium", "veryfast"] {
                    for (mode_name, mode) in [
                        ("primary", Mode::Primary),
                        ("transcode-audio", Mode::TranscodeAudio),
                        ("cpu-fallback", Mode::CpuFallback),
                    ] {
                        for input in [r"C:\v\a.mp4", r"C:\v\b.gif", r"C:\v\c.wmv"] {
                            let o = Options {
                                crf: *crf,
                                preset: preset.into(),
                                encoder: encoder.to_string(),
                                concurrency: 2,
                                ..Options::default()
                            };
                            let tmp = temp_name(Path::new(input), 1);
                            rows.push(serde_json::json!({
                                "encoder": encoder, "crf": crf, "preset": preset, "mode": mode_name,
                                "input": input, "tmp": tmp.to_string_lossy(),
                                "args": build_ffmpeg_args(Path::new(input), &tmp, &o, mode),
                            }));
                        }
                    }
                }
            }
        }
        std::fs::write(out, serde_json::to_string(&rows).unwrap()).unwrap();
    }

    #[test]
    fn gif_drops_audio_and_cpu_fallback_uses_x264() {
        let o = Options::default();
        let a = build_ffmpeg_args(Path::new("a.gif"), Path::new("a.mp4"), &o, Mode::CpuFallback);
        let joined = a.join(" ");
        assert!(joined.contains("-c:v libx264 -preset slow -crf 23 -pix_fmt yuv420p -an"));
        assert!(!joined.contains("-fps_mode"), "GIFs keep their own frame timing");

        let v = build_ffmpeg_args(Path::new("a.wmv"), Path::new("a.mp4"), &o, Mode::CpuFallback).join(" ");
        assert!(v.contains("-fps_mode cfr -max_muxing_queue_size 4096 -map 0:a? -c:a copy"), "{v}");
        assert!(!v.contains("aac") && !v.contains("-ac "), "audio is carried, not downmixed: {v}");
    }

    const LISTING: &str = "Input #0, matroska,webm, from 'in.mkv':\n  Metadata:\n    ENCODER         : Lavf61\n\
        \x20 Duration: 00:01:02.50, start: 0.000000, bitrate: 1000 kb/s\n\
        \x20 Chapters:\n    Chapter #0:0: start 0.000000, end 1.500000\n      Metadata:\n        title           : One\n\
        \x20   Chapter #0:1: start 1.500000, end 3.000000\n\
        \x20 Stream #0:0: Video: hevc (Main 10), yuv420p10le(tv, bt2020nc/bt2020/arib-std-b67), 640x360, 30 fps\n\
        \x20 Stream #0:1(por): Audio: aac (LC), 44100 Hz, mono, fltp (default)\n\
        \x20 Stream #0:2(eng): Audio: aac (LC), 44100 Hz, mono, fltp\n\
        \x20 Stream #0:3: Subtitle: subrip (srt)\n\
        \x20 Stream #0:4[0x5](und): Subtitle: hdmv_pgs_subtitle (pgssub), 1920x1080\n\
        \x20 Stream #0:5: Attachment: ttf\n\
        \x20 Stream #0:6: Video: mjpeg (Baseline), yuvj420p, 600x600, 90k tbn (attached pic)\n";

    #[test]
    fn streams_are_read_from_the_listing() {
        let s = Streams::parse(LISTING);
        assert_eq!(s.audio, 2);
        assert_eq!(s.subtitles, ["subrip", "hdmv_pgs_subtitle"]);
        assert_eq!((s.attachments, s.covers.clone(), s.chapters), (1, vec![6], 2));
        assert!(s.duration.is_some_and(|d| (d - 62.5).abs() < 1e-9));
        assert!(!s.fits_mp4());
        let mp4 = Streams { subtitles: vec!["mov_text".into()], audio: 3, ..Streams::default() };
        assert!(mp4.fits_mp4());
    }

    #[test]
    fn container_tags_are_read_and_carried_over() {
        let listing = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'IMG_0001.MOV':\n  Metadata:\n\
            \x20   major_brand     : qt  \n    minor_version   : 0\n    compatible_brands: qt  \n\
            \x20   creation_time   : 2024-05-01T10:20:30.000000Z;2024-05-01T10:20:30Z\n\
            \x20   com.apple.quicktime.location.ISO6709: +12.3456-045.6789+010.000/\n\
            \x20   com.apple.quicktime.make: Apple\n    comment         : line one\n                    : line two\n\
            \x20   encoder         : Lavf61\n\
            \x20 Duration: 00:00:02.00, start: 0.000000, bitrate: 1 kb/s\n\
            \x20 Stream #0:0[0x1](und): Video: h264 (High) (avc1), yuv420p, 640x360, 30 fps\n\
            \x20   Metadata:\n      handler_name    : Core Media Video\n";
        let s = Streams::parse(listing);
        let keys: Vec<&str> = s.tags.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["creation_time", "com.apple.quicktime.location.iso6709", "com.apple.quicktime.make", "comment"], "stream tags and bookkeeping left out");
        assert_eq!(s.tags[3].1, "line one\nline two");
        assert_eq!(s.creation_time(), Some("2024-05-01T10:20:30.000000Z"));
        let o = Options { streams: s, ..Options::default() };
        let a = build_ffmpeg_args(Path::new("IMG_0001.MOV"), Path::new("out.mp4"), &o, Mode::Primary).join(" ");
        assert!(a.contains("-map_metadata 0 -metadata major_brand= -metadata minor_version= -metadata compatible_brands= -metadata creation_time=2024-05-01T10:20:30.000000Z"), "{a}");
        assert!(a.contains("-movflags +faststart+use_metadata_tags"), "{a}");
        let x = build_ffmpeg_args(Path::new("IMG_0001.MOV"), Path::new("out.mp4"), &Options { encoder: "libx264".into(), ..o }, Mode::Primary).join(" ");
        assert!(x.contains("-map_metadata 0") && x.contains("+use_metadata_tags"), "{x}");
        let hdr10_plus = Streams { hdr10_plus: true, ..Streams::default() };
        assert!(hdr10_plus.blocker(true).is_some_and(|b| b.why.contains("HDR10+") && b.permanent));
    }

    #[test]
    fn audio_data_hdr_and_dolby_vision_are_read() {
        let listing = "  Duration: 00:00:10.00, start: 0.000000, bitrate: 1 kb/s\n\
            \x20 Stream #0:0[0x1](und): Video: hevc (Main 10) (hvc1), yuv420p10le(tv, bt2020nc/bt2020/smpte2084), 3840x2160, 30 fps\n\
            \x20     DOVI configuration record: version: 1.0, profile: 8, level: 6\n\
            \x20 Stream #0:1[0x2](eng): Audio: ac3 (ac-3 / 0x332D6361), 48000 Hz, 5.1(side), fltp, 448 kb/s\n\
            \x20 Stream #0:2[0x3](eng): Audio: pcm_s24le (in24), 48000 Hz, stereo, s32 (24 bit), 2304 kb/s\n\
            \x20 Stream #0:3[0x4](eng): Audio: wmav2, 44100 Hz, 7 channels, fltp\n\
            \x20 Stream #0:4[0x5](eng): Data: none (tmcd / 0x64636D74), 0 kb/s\n\
            \x20 Stream #0:5[0x6](eng): Data: bin_data (gpmd / 0x646D7067), 61 kb/s\n\
            \x20 Stream #0:6[0x7](eng): Data: none, 0 kb/s (default)\n";
        let s = Streams::parse(listing);
        assert_eq!(s.audio_codecs, ["ac3", "pcm_s24le", "wmav2"]);
        assert_eq!(s.audio_channels, [6, 2, 7]);
        assert_eq!(s.data, ["tmcd", "gpmd", "none"]);
        assert!(s.hdr && s.dolby_vision && !s.fits_mp4());
        assert!(s.blocker(true).is_some_and(|b| b.why.contains("Dolby Vision") && b.permanent));
        let no_dv = Streams { dolby_vision: false, ..s };
        let b = no_dv.blocker(false).unwrap();
        assert!(b.why.contains("gpmd, none") && !b.why.contains("tmcd") && !b.permanent, "{b:?}");
        assert_eq!(no_dv.blocker(true), None, "left out when the option is on");
        let timecode_only = Streams { data: vec!["tmcd".into()], ..Streams::default() };
        assert_eq!(timecode_only.blocker(false), None, "timecode is rebuilt in MP4/MOV");
        let qtvr = Streams { data: vec!["qtvr".into(), "none".into()], ..Streams::default() };
        assert!(qtvr.blocker(true).is_some_and(|b| b.why.contains("QuickTime VR") && b.permanent), "never re-encoded");
        let opus = Streams { audio_codecs: vec!["opus".into(), "mp3".into(), "ac3".into()], ..Streams::default() };
        assert!(opus.fits_mp4(), "copied into MP4 as they are");
    }

    #[test]
    fn audio_is_copied_pcm_becomes_flac_and_aac_keeps_channels() {
        let streams = Streams { audio_codecs: vec!["ac3".into(), "pcm_s16le".into()], audio_channels: vec![6, 2], ..Streams::default() };
        let o = Options { streams, ..Options::default() };
        let p = build_ffmpeg_args(Path::new("in.mov"), Path::new("out.mkv"), &o, Mode::Primary).join(" ");
        assert!(p.contains("-map 0:a? -c:a copy -c:a:1 flac") && !p.contains("-c:a:0"), "{p}");
        let t = build_ffmpeg_args(Path::new("in.mov"), Path::new("out.mkv"), &o, Mode::TranscodeAudio).join(" ");
        assert!(t.contains("-c:a aac -b:a:0 576k -b:a:1 192k") && !t.contains("-ac "), "{t}");
        let x = build_ffmpeg_args(Path::new("in.mov"), Path::new("out.mkv"), &Options { encoder: "libx264".into(), ..o }, Mode::Primary).join(" ");
        assert!(x.contains("-map 0:a? -c:a copy -c:a:1 flac") && !x.contains("aac"), "{x}");
    }

    #[test]
    fn mkv_when_mp4_cannot_hold_everything() {
        let plain = Streams { audio: 1, ..Streams::default() };
        let subs = Streams { subtitles: vec!["webvtt".into()], ..Streams::default() };
        let fonts = Streams { attachments: 1, ..Streams::default() };
        assert_eq!(output_ext(Path::new("a.webm"), &plain), "mp4");
        assert_eq!(output_ext(Path::new("a.webm"), &subs), "mkv");
        assert_eq!(output_ext(Path::new("a.mov"), &fonts), "mkv");
        assert_eq!(output_ext(Path::new("a.mp4"), &subs), "mkv");
        assert_eq!(output_ext(Path::new("a.mkv"), &subs), "mkv");
        assert_eq!(output_ext(Path::new("a.gif"), &subs), "mp4");
        assert_eq!(temp_name_with(Path::new(r"C:\v\a.mov"), 2, "mkv"), Path::new(r"C:\v\a.mediaopti.w2.mkv"));
    }

    #[test]
    fn everything_else_is_carried_over() {
        let mut o = Options { streams: Streams { covers: vec![3], ..Streams::default() }, ..Options::default() };
        let mkv = build_ffmpeg_args(Path::new("in.mkv"), Path::new("out.mkv"), &o, Mode::Primary).join(" ");
        assert!(mkv.contains("-map 0:a? -c:a copy -map 0:s? -c:s copy -map 0:t? -map 0:3 -c:v:1 copy -disposition:v:1 attached_pic"), "{mkv}");
        assert!(mkv.contains("-filter:v:0 scale=") && mkv.contains("-pix_fmt:v:0 p010le") && !mkv.contains("-vf "), "{mkv}");
        o.streams = Streams::default();
        for mode in [Mode::TranscodeAudio, Mode::CpuFallback] {
            let a = build_ffmpeg_args(Path::new("in.avi"), Path::new("out.mp4"), &o, mode).join(" ");
            assert!(a.contains("-map 0:a? ") && !a.contains("0:a:0") && !a.contains("-map_chapters"), "{a}");
            assert!(a.contains("-map 0:s? -c:s copy") && !a.contains("0:t?"), "no attachments in MP4: {a}");
        }
        let gif = build_ffmpeg_args(Path::new("a.gif"), Path::new("a.mp4"), &o, Mode::Primary).join(" ");
        assert!(!gif.contains("0:s?"), "{gif}");
    }
}
