//! Unified media format registry: every supported video, animated-picture, image, audio and
//! document format, plus its routing, tool requirements, quality-floor policy and disk budget
//! factor in one authoritative table.

use std::path::Path;

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mts", "m2ts", "flv", "m4v", "mkv", "mov", "webm", "avi", "wmv", "ts", "3gp", "mpg",
    "mpeg", "gif", "webp", "bik", "xmv", "ogv", "asf", "pss",
];

/// Legacy/non-MP4 containers that remux cleanly into `.mp4`.
pub const REMUX_TO_MP4_EXTS: &[&str] = &[
    "mts", "m2ts", "flv", "webm", "m4v", "mov", "avi", "wmv", "ts", "3gp", "mpg", "mpeg", "gif",
    "webp", "bik", "xmv", "ogv", "asf", "pss",
];

/// Formats processed by the CPU workers (audio, documents/archives, images).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Kind {
    Png = 0,
    Jpg = 1,
    Gif = 2,
    /// Audio: cover art and re-encoding, see [`crate::audio`].
    Flac = 3,
    Mp3 = 4,
    /// Converted to FLAC (integer PCM only), when conversions are on.
    Wav = 5,
    /// Ogg Vorbis, repacked losslessly by OptiVorbis.
    Ogg = 6,
    /// Ogg Opus: embedded cover art (`METADATA_BLOCK_PICTURE`) optimized; audio pages copied verbatim.
    Opus = 7,
    /// Other lossless audio (ALAC in .m4a, AIFF, APE, WavPack, TTA), converted to FLAC when
    /// conversions are on and the FLAC is smaller. (An .m4a holding AAC is left alone.)
    LosslessAudio = 8,
    /// Converted to PNG (pixel-identical), when conversions are on.
    Bmp = 9,
    Tga = 10,
    /// Office, OpenDocument and EPUB: zip repacked, pictures inside optimized losslessly.
    Doc = 11,
    /// Plain ZIP archive: recompressed losslessly at maximum Deflate effort (`ect -9`).
    Zip = 12,
    /// Comic book archive (zip): pictures inside follow the image setting.
    Cbz = 13,
    /// Comic book archive (RAR), converted to CBZ when conversions are on.
    Cbr = 14,
    /// PDF: qpdf rewrite, images optimized (see [`crate::pdf`]).
    Pdf = 15,
}

/// Which format-conversion toggle in `Options::convert` gates a format, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversionGate {
    Wav,
    Images,
    Cbr,
}

/// Which visually-lossless quality floor applies when lossy optimization runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityPolicy {
    /// Follows the user's Images selector (93 / 85 / 80).
    PictureFloor,
    /// Always keeps the default 93 floor (PDF pages stay readable) and requires `lossy_docs`.
    PdfDefault,
    /// No lossy pass.
    None,
}

/// Which lossy tool capability group a format needs to run its visually-lossless search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LossyTools {
    Png,
    Jpg,
    BothPictures,
    Pdf,
    None,
}

/// Whether `process_image` can read the source file directly without copying it to `WorkDir`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InPlace {
    /// Audio files under 248 chars (never modified in place; tools write into `work`).
    ShortPath,
    /// Standalone pictures whose extension matches content and path is short ASCII.
    MatchingAsciiShort,
    /// Always copy to an ASCII name inside `work`.
    Never,
}

/// Declarative specification of one CPU-worker media format.
#[derive(Clone, Copy, Debug)]
pub struct FormatSpec {
    pub label: &'static str,
    pub canonical_ext: &'static str,
    /// Queue stage: 0 = audio, 1 = documents/archives, 2 = images.
    pub stage: usize,
    pub is_archive: bool,
    pub conversion: Option<ConversionGate>,
    pub required_tools: &'static [&'static str],
    pub lossy_tools: LossyTools,
    pub quality_policy: QualityPolicy,
    /// Standalone picture that gets multi-thread tools (`threads: 0`) and pixel-based lossy estimates.
    pub is_standalone_picture: bool,
    /// Misnamed-extension fixing (`--fix-ext`) applies to this format.
    pub fix_ext_eligible: bool,
    /// Guard against data appended after the end of the image (motion photo, gain map, cards).
    pub has_trailing_guard: bool,
    pub in_place: InPlace,
    /// Temp-space multiplier for `WorkDir` reservation (when not reading in place as short audio).
    pub work_factor: u64,
}

impl Kind {
    pub const COUNT: usize = 16;

    #[cfg(test)]
    pub const ALL: [Self; Self::COUNT] = [
        Self::Png,
        Self::Jpg,
        Self::Gif,
        Self::Flac,
        Self::Mp3,
        Self::Wav,
        Self::Ogg,
        Self::Opus,
        Self::LosslessAudio,
        Self::Bmp,
        Self::Tga,
        Self::Doc,
        Self::Zip,
        Self::Cbz,
        Self::Cbr,
        Self::Pdf,
    ];

    pub const fn spec(self) -> &'static FormatSpec {
        match self {
            Self::Png => &FormatSpec {
                label: "PNG",
                canonical_ext: "png",
                stage: 2,
                is_archive: false,
                conversion: None,
                required_tools: &["oxipng"],
                lossy_tools: LossyTools::Png,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: true,
                fix_ext_eligible: true,
                has_trailing_guard: true,
                in_place: InPlace::MatchingAsciiShort,
                work_factor: 4,
            },
            Self::Jpg => &FormatSpec {
                label: "JPG",
                canonical_ext: "jpg",
                stage: 2,
                is_archive: false,
                conversion: None,
                required_tools: &["jpegtran"],
                lossy_tools: LossyTools::Jpg,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: true,
                fix_ext_eligible: true,
                has_trailing_guard: true,
                in_place: InPlace::MatchingAsciiShort,
                work_factor: 4,
            },
            Self::Gif => &FormatSpec {
                label: "GIF",
                canonical_ext: "gif",
                stage: 2,
                is_archive: false,
                conversion: None,
                required_tools: &["gifsicle"],
                lossy_tools: LossyTools::None,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: false,
                fix_ext_eligible: true,
                has_trailing_guard: true,
                in_place: InPlace::MatchingAsciiShort,
                work_factor: 4,
            },
            Self::Flac => &FormatSpec {
                label: "FLAC",
                canonical_ext: "flac",
                stage: 0,
                is_archive: false,
                conversion: None,
                required_tools: &["flac", "oxipng", "jpegtran"],
                lossy_tools: LossyTools::BothPictures,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::ShortPath,
                work_factor: 4,
            },
            Self::Mp3 => &FormatSpec {
                label: "MP3",
                canonical_ext: "mp3",
                stage: 0,
                is_archive: false,
                conversion: None,
                required_tools: &["oxipng", "jpegtran"],
                lossy_tools: LossyTools::BothPictures,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::ShortPath,
                work_factor: 4,
            },
            Self::Wav => &FormatSpec {
                label: "WAV",
                canonical_ext: "wav",
                stage: 0,
                is_archive: false,
                conversion: Some(ConversionGate::Wav),
                required_tools: &["flac"],
                lossy_tools: LossyTools::None,
                quality_policy: QualityPolicy::None,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Ogg => &FormatSpec {
                label: "OGG",
                canonical_ext: "ogg",
                stage: 0,
                is_archive: false,
                conversion: None,
                required_tools: &["optivorbis"],
                lossy_tools: LossyTools::None,
                quality_policy: QualityPolicy::None,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::ShortPath,
                work_factor: 4,
            },
            Self::Opus => &FormatSpec {
                label: "OPUS",
                canonical_ext: "opus",
                stage: 0,
                is_archive: false,
                conversion: None,
                required_tools: &["oxipng", "jpegtran"],
                lossy_tools: LossyTools::BothPictures,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::ShortPath,
                work_factor: 4,
            },
            Self::LosslessAudio => &FormatSpec {
                label: "ALAC/AIFF/APE/WavPack/TTA",
                canonical_ext: "bin",
                stage: 0,
                is_archive: false,
                conversion: Some(ConversionGate::Wav),
                required_tools: &["flac", "oxipng", "jpegtran"],
                lossy_tools: LossyTools::BothPictures,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Bmp => &FormatSpec {
                label: "BMP",
                canonical_ext: "bmp",
                stage: 2,
                is_archive: false,
                conversion: Some(ConversionGate::Images),
                required_tools: &["oxipng"],
                lossy_tools: LossyTools::Png,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: true,
                fix_ext_eligible: true,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Tga => &FormatSpec {
                label: "TGA",
                canonical_ext: "tga",
                stage: 2,
                is_archive: false,
                conversion: Some(ConversionGate::Images),
                required_tools: &["oxipng"],
                lossy_tools: LossyTools::Png,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: true,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Doc => &FormatSpec {
                label: "Office/EPUB",
                canonical_ext: "bin",
                stage: 1,
                is_archive: true,
                conversion: None,
                required_tools: &["oxipng", "jpegtran"],
                lossy_tools: LossyTools::None,
                quality_policy: QualityPolicy::None,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Zip => &FormatSpec {
                label: "ZIP",
                canonical_ext: "zip",
                stage: 1,
                is_archive: true,
                conversion: None,
                required_tools: &["ect"],
                lossy_tools: LossyTools::None,
                quality_policy: QualityPolicy::None,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Cbz => &FormatSpec {
                label: "CBZ",
                canonical_ext: "cbz",
                stage: 1,
                is_archive: true,
                conversion: None,
                required_tools: &["oxipng", "jpegtran"],
                lossy_tools: LossyTools::BothPictures,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Cbr => &FormatSpec {
                label: "CBR",
                canonical_ext: "cbr",
                stage: 1,
                is_archive: true,
                conversion: Some(ConversionGate::Cbr),
                required_tools: &["7z", "oxipng", "jpegtran"],
                lossy_tools: LossyTools::BothPictures,
                quality_policy: QualityPolicy::PictureFloor,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 4,
            },
            Self::Pdf => &FormatSpec {
                label: "PDF",
                canonical_ext: "pdf",
                stage: 1,
                is_archive: false,
                conversion: None,
                required_tools: &["qpdf", "jpegtran", "djpeg"],
                lossy_tools: LossyTools::Pdf,
                quality_policy: QualityPolicy::PdfDefault,
                is_standalone_picture: false,
                fix_ext_eligible: false,
                has_trailing_guard: false,
                in_place: InPlace::Never,
                work_factor: 16,
            },
        }
    }

    pub const fn is_audio(self) -> bool {
        self.spec().stage == 0
    }

    pub const fn is_archive(self) -> bool {
        self.spec().is_archive
    }

    /// Documents: PDF, Office/ODF/EPUB, comics, plain ZIPs.
    pub const fn is_document(self) -> bool {
        self.spec().stage == 1
    }

    /// The CPU queue stage: 0 audio, 1 documents, 2 images.
    pub const fn stage(self) -> usize {
        self.spec().stage
    }

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn label(self) -> &'static str {
        self.spec().label
    }

    /// CPU worker format by file extension (GIFs are routed separately when animated and large).
    pub fn of(path: &Path) -> Option<Self> {
        match ext_lower(path).as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" | "jpe" | "jfif" => Some(Self::Jpg),
            "gif" => Some(Self::Gif),
            "flac" => Some(Self::Flac),
            "mp3" => Some(Self::Mp3),
            "wav" => Some(Self::Wav),
            "m4a" | "aif" | "aiff" | "aifc" | "ape" | "wv" | "tta" => Some(Self::LosslessAudio),
            "ogg" | "oga" => Some(Self::Ogg),
            "opus" => Some(Self::Opus),
            "bmp" => Some(Self::Bmp),
            "tga" => Some(Self::Tga),
            "docx" | "xlsx" | "pptx" | "docm" | "xlsm" | "pptm" | "odt" | "ods" | "odp" | "odg" | "epub" => Some(Self::Doc),
            "zip" => Some(Self::Zip),
            "cbz" => Some(Self::Cbz),
            "cbr" => Some(Self::Cbr),
            "pdf" => Some(Self::Pdf),
            _ => None,
        }
    }
}

pub fn ext_lower(path: &Path) -> String {
    path.extension().map_or_default(|e| e.to_string_lossy().to_lowercase())
}

pub fn is_gif(path: &Path) -> bool {
    ext_lower(path) == "gif"
}

pub fn is_webp(path: &Path) -> bool {
    ext_lower(path) == "webp"
}

/// Animated picture formats converted to video when animated (GIF, WebP).
pub fn is_animated_picture(path: &Path) -> bool {
    is_gif(path) || is_webp(path)
}

pub fn is_video_file(path: &Path) -> bool {
    VIDEO_EXTENSIONS.contains(&ext_lower(path).as_str())
}

/// Whether `path` belongs to any format mediaopti scans (video, animated picture, or CPU format).
pub fn is_scanned_media(path: &Path) -> bool {
    is_video_file(path) || Kind::of(path).is_some()
}

/// Whether `file` could be the original that a temp file `temp` (video or CPU) was created from.
pub fn could_be_source(file: &Path, temp: &Path, video_temp: bool) -> bool {
    if video_temp {
        return is_video_file(file) || Kind::of(file) == Some(Kind::Gif);
    }
    let family = |p: &Path| Kind::of(p).map(Kind::stage);
    family(file).is_some() && family(file) == family(temp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_all_and_specs_are_consistent() {
        assert_eq!(Kind::ALL.len(), Kind::COUNT);
        for (idx, kind) in Kind::ALL.iter().copied().enumerate() {
            assert_eq!(kind.index(), idx, "{kind:?}");
            let s = kind.spec();
            assert!(!s.label.is_empty(), "{kind:?}");
            assert!(s.stage <= 2, "{kind:?}");
            assert!(!s.required_tools.is_empty(), "{kind:?}");
            assert!(s.work_factor >= 4, "{kind:?}");
        }
        assert!(is_scanned_media(Path::new("a.bik")));
        assert!(is_scanned_media(Path::new("a.opus")));
        assert!(is_scanned_media(Path::new("a.zip")));
        assert!(!is_scanned_media(Path::new("readme.txt")));
        assert!(could_be_source(Path::new("a.wav"), Path::new("a.mediaopti.i1.flac"), false));
        assert!(!could_be_source(Path::new("a.png"), Path::new("a.mediaopti.i1.flac"), false));
        assert!(could_be_source(Path::new("a.gif"), Path::new("a.mediaopti.w1.mp4"), true));
    }
}
