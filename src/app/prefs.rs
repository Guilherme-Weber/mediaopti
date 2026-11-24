//! Saved settings (`ui-settings.json`) and the lists the window's options choose from.

use super::*;

pub(super) const ENCODERS: &[(&str, &str)] = &[
    ("av1_nvenc", "AV1 NVENC (10-bit GPU)"),
    ("hevc_nvenc", "HEVC NVENC (10-bit GPU)"),
    ("h264_nvenc", "H.264 NVENC (GPU)"),
    ("libx264", "x264 (CPU)"),
];
pub(super) const JOBS: &[(u32, &str)] = &[
    (1, "1 (single stream)"),
    (2, "2 streams"),
    (3, "3 streams"),
    (4, "4 streams (fastest)"),
];
pub(super) const QUALITY: &[(u32, &str)] = &[
    (20, "High quality (camera / raw)"),
    (23, "Balanced (web / re-encode)"),
    (26, "Compact (max space saver)"),
];
pub(super) const SPEEDS: &[(&str, &str)] = &[
    ("slow", "Slow (NVENC p7, best)"),
    ("medium", "Medium (NVENC p7)"),
    ("veryfast", "Very fast (NVENC p5)"),
];
// Keep "Move to a backup folder" at index 1: the window shows the folder line for it.
pub(super) const ORIGINALS: &[(u32, &str)] = &[
    (1, "Move to the Recycle Bin"),
    (2, "Move to a backup folder"),
    (0, "Delete permanently"),
];
pub(super) const GIFS: &[(u32, &str)] = &[
    (1, "Over 1 MB to video"),
    (5, "Over 5 MB to video"),
    (0, "All animated GIFs to video"),
    (NO_GIFS, "Keep all GIFs as GIF"),
];
pub(super) const IMAGES: &[(u32, &str)] = &[
    (2, "Visually lossless (SSIMULACRA2 ≥ 93)"),
    (3, "High quality, smaller (≥ 85)"),
    (4, "Smallest, slight banding up close (≥ 80, like Pinga)"),
    (1, "Lossless only (pixel-identical)"),
];
pub(super) const PDFS: &[(u32, &str)] = &[
    (1, "Visually lossless images (pages checked)"),
    (0, "Lossless only"),
];
pub(super) const AUDIO: &[(u32, &str)] = &[
    (2, "Lossy, at the Images quality level"),
    (1, "Lossless only"),
];
pub(super) const FIX_EXT: &[(u32, &str)] = &[
    (0, "Keep file names"),
    (1, "Rename to the real format"),
];
pub(super) const QUALITY_CHECK: &[(u32, &str)] = &[
    (93, "VMAF 93 (recommended)"),
    (95, "VMAF 95 (stricter)"),
    (0, "Off (preset only)"),
];

// ------------------------------------------------------------------- settings

/// Saved settings. `serde(default)`: settings from batchviproc or older versions lack the
/// newer fields, which then take their defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Prefs {
    /// The file's format version; a file without one (0) is from before 3.1 and goes through
    /// the migrations in `load`.
    #[serde(default)]
    pub(super) version: u32,
    pub(super) encoder: String,
    pub(super) concurrency: u32,
    pub(super) crf: u32,
    pub(super) preset: String,
    /// 0 = delete, 1 = Recycle Bin, 2 = backup folder.
    pub(super) originals: u32,
    /// Chosen backup folder; empty = next to the scanned folder.
    pub(super) backup_dir: String,
    pub(super) gif_min_mb: u32,
    pub(super) vmaf_floor: u32,
    /// 1 = lossless, 2 = visually lossless (93), 3 = high quality (85), 4 = smallest (80).
    pub(super) images: u32,
    /// Each type on or off.
    pub(super) videos: bool,
    pub(super) images_on: bool,
    pub(super) audio_on: bool,
    pub(super) docs: bool,
    /// Each lossless conversion on or off.
    pub(super) conv_wav: bool,
    pub(super) conv_img: bool,
    pub(super) conv_cbr: bool,
    /// 1 = visually lossless PDF images, 0 = lossless only.
    pub(super) lossy_docs: u32,
    /// Cover art: 1 = lossless, 2 = lossy at the picture quality floor (`images`).
    pub(super) audio: u32,
    /// Folder for work files; empty = the system temp folder.
    pub(super) temp_dir: String,
    /// 1 = rename misnamed pictures (a JPEG called .png) to their real format.
    pub(super) fix_ext: u32,
    /// The side navigation shows only icons.
    pub(super) nav_compact: bool,
    /// Advanced: GPU decoding allowed.
    pub(super) gpu_decoding: bool,
    /// Advanced: the maximum-effort parts in use (`image::Max` field names). Saved only when
    /// they differ from the default, so a better default reaches everyone who kept it.
    #[serde(skip_serializing_if = "is_default_parts")]
    pub(super) max_parts: Vec<String>,
    /// Advanced: leave out data tracks no container can carry over.
    pub(super) drop_data: bool,
    /// Advanced: remove metadata (pictures) and tag padding (audio).
    pub(super) strip: bool,
    /// Advanced: with `strip`, redo files processed without it.
    pub(super) redo_stripped: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            encoder: "av1_nvenc".into(),
            concurrency: 4,
            crf: 23,
            preset: "slow".into(),
            originals: 1,
            backup_dir: String::new(),
            gif_min_mb: 1,
            vmaf_floor: 93,
            images: 2,
            videos: true,
            images_on: true,
            audio_on: true,
            docs: true,
            conv_wav: true,
            conv_img: true,
            conv_cbr: true,
            lossy_docs: 1,
            audio: 2,
            temp_dir: String::new(),
            fix_ext: 0,
            nav_compact: false,
            gpu_decoding: true,
            version: PREFS_VERSION,
            max_parts: crate::image::Max::chosen().names().into_iter().map(String::from).collect(),
            drop_data: false,
            strip: false,
            redo_stripped: false,
        }
    }
}

/// Position of `value` in `choices`, falling back to the position of `default`.
pub(super) fn position<K: PartialEq>(choices: &[(K, &str)], value: &K, default: &K) -> i32 {
    let find = |v: &K| choices.iter().position(|(k, _)| k == v);
    find(value).or_else(|| find(default)).unwrap_or(0) as i32
}

/// The settings file's format version (see `Prefs::version`).
pub(super) const PREFS_VERSION: u32 = 1;

/// Whether `parts` are the default maximum-effort parts (in any order).
pub(super) fn is_default_parts(parts: &[String]) -> bool {
    let sorted = |v: &[String]| {
        let mut v = v.to_vec();
        v.sort();
        v
    };
    sorted(parts) == sorted(&Prefs::default().max_parts)
}

impl Prefs {
    /// The saved settings, and what went wrong reading them, if anything (for the log). A
    /// field of an unexpected type costs that field only; an unreadable file gives defaults.
    pub(super) fn load(path: &Path) -> (Self, Option<String>) {
        let Some(text) = std::fs::read_to_string(path).ok() else { return (Self::default(), None) };
        let (mut p, problem) = match serde_json::from_str::<Prefs>(&text) {
            Ok(p) => (p, None),
            Err(err) => match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text) {
                Ok(fields) => Self::field_by_field(fields),
                Err(_) => {
                    return (Self::default(), Some(format!("The settings file {} could not be read ({err}); default settings are used", path.display())));
                }
            },
        };
        // Older settings: "Don't process" was a quality choice, conversions one switch.
        let old: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if p.images == 0 {
            (p.images_on, p.images) = (false, 2);
        }
        if p.audio == 0 {
            (p.audio_on, p.audio) = (false, 2);
        }
        if old.get("conv_wav").is_none() && old.get("convert").and_then(serde_json::Value::as_u64) == Some(0) {
            (p.conv_wav, p.conv_img, p.conv_cbr) = (false, false, false);
        }
        p.version = PREFS_VERSION;
        (p, problem)
    }

    /// The defaults with each saved field that reads as its type: one bad field doesn't
    /// throw away the others (the backup folder among them).
    pub(super) fn field_by_field(fields: serde_json::Map<String, serde_json::Value>) -> (Self, Option<String>) {
        let mut good = match serde_json::to_value(Self::default()) {
            Ok(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        let mut bad = Vec::new();
        for (key, value) in fields {
            let mut trial = good.clone();
            trial.insert(key.clone(), value.clone());
            if serde_json::from_value::<Prefs>(serde_json::Value::Object(trial)).is_ok() {
                good.insert(key, value);
            } else {
                bad.push(key);
            }
        }
        let p = serde_json::from_value(serde_json::Value::Object(good)).unwrap_or_default();
        (p, Some(format!("Some saved settings had unexpected values and are back to their defaults: {}", bad.join(", "))))
    }

    /// Written whole or not at all (a crash mid-write must not leave a cut-off file).
    pub(super) fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = crate::history::write_atomically(path, json.as_bytes());
        }
    }

    /// Reads the current selection from the window.
    pub(super) fn from_window(w: &AppWindow) -> Self {
        let pick = |i: i32, len: usize| (i.max(0) as usize).min(len - 1);
        Self {
            version: PREFS_VERSION,
            encoder: ENCODERS[pick(w.get_encoder_index(), ENCODERS.len())].0.into(),
            concurrency: JOBS[pick(w.get_jobs_index(), JOBS.len())].0,
            crf: QUALITY[pick(w.get_quality_index(), QUALITY.len())].0,
            preset: SPEEDS[pick(w.get_speed_index(), SPEEDS.len())].0.into(),
            originals: ORIGINALS[pick(w.get_originals_index(), ORIGINALS.len())].0,
            backup_dir: w.get_backup_dir().into(),
            gif_min_mb: GIFS[pick(w.get_gifs_index(), GIFS.len())].0,
            vmaf_floor: QUALITY_CHECK[pick(w.get_check_index(), QUALITY_CHECK.len())].0,
            images: IMAGES[pick(w.get_images_index(), IMAGES.len())].0,
            videos: w.get_videos_on(),
            images_on: w.get_images_on(),
            audio_on: w.get_audio_on(),
            docs: w.get_docs_on(),
            conv_wav: w.get_conv_wav(),
            conv_img: w.get_conv_img(),
            conv_cbr: w.get_conv_cbr(),
            lossy_docs: PDFS[pick(w.get_pdf_index(), PDFS.len())].0,
            audio: AUDIO[pick(w.get_audio_index(), AUDIO.len())].0,
            temp_dir: w.get_temp_dir().into(),
            fix_ext: FIX_EXT[pick(w.get_fix_ext_index(), FIX_EXT.len())].0,
            nav_compact: w.get_nav_compact(),
            gpu_decoding: w.get_gpu_decoding(),
            max_parts: w.get_adv_parts().iter().filter(|p| p.on).map(|p| p.name.to_string()).collect(),
            drop_data: w.get_drop_data(),
            strip: w.get_strip(),
            redo_stripped: w.get_redo_stripped(),
        }
    }

    /// Shows the selection in the window; unknown saved values (e.g. the removed OpenCL
    /// encoder) fall back to the defaults.
    pub(super) fn apply_to_window(&self, w: &AppWindow) {
        let d = Prefs::default();
        w.set_encoder_index(position(ENCODERS, &self.encoder.as_str(), &d.encoder.as_str()));
        w.set_jobs_index(position(JOBS, &self.concurrency, &d.concurrency));
        w.set_quality_index(position(QUALITY, &self.crf, &d.crf));
        w.set_speed_index(position(SPEEDS, &self.preset.as_str(), &d.preset.as_str()));
        w.set_originals_index(position(ORIGINALS, &self.originals, &d.originals));
        w.set_backup_dir(self.backup_dir.as_str().into());
        w.set_gifs_index(position(GIFS, &self.gif_min_mb, &d.gif_min_mb));
        w.set_check_index(position(QUALITY_CHECK, &self.vmaf_floor, &d.vmaf_floor));
        w.set_images_index(position(IMAGES, &self.images, &d.images));
        w.set_videos_on(self.videos);
        w.set_images_on(self.images_on);
        w.set_audio_on(self.audio_on);
        w.set_docs_on(self.docs);
        w.set_conv_wav(self.conv_wav);
        w.set_conv_img(self.conv_img);
        w.set_conv_cbr(self.conv_cbr);
        w.set_pdf_index(position(PDFS, &self.lossy_docs, &d.lossy_docs));
        w.set_audio_index(position(AUDIO, &self.audio, &d.audio));
        w.set_temp_dir(self.temp_dir.as_str().into());
        w.set_fix_ext_index(position(FIX_EXT, &self.fix_ext, &d.fix_ext));
        w.set_nav_compact(self.nav_compact);
        w.set_gpu_decoding(self.gpu_decoding);
        w.set_drop_data(self.drop_data);
        w.set_strip(self.strip);
        w.set_redo_stripped(self.redo_stripped);
        let parts: Vec<AdvPart> = crate::image::Max::PARTS
            .iter()
            .map(|(name, label, detail)| AdvPart {
                name: (*name).into(),
                label: (*label).into(),
                detail: (*detail).into(),
                on: self.max_parts.iter().any(|p| p == name),
            })
            .collect();
        w.set_adv_parts(ModelRc::new(VecModel::from(parts)));
        w.set_adv_default(self.advanced_are_defaults());
    }

    pub(super) fn advanced_are_defaults(&self) -> bool {
        let d = Prefs::default();
        self.gpu_decoding == d.gpu_decoding && self.drop_data == d.drop_data && self.strip == d.strip && self.redo_stripped == d.redo_stripped && self.max_parts_sorted() == d.max_parts_sorted()
    }

    pub(super) fn max_parts_sorted(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.max_parts.iter().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    pub(super) fn to_options(&self) -> Options {
        let d = Options::default();
        let non_empty = |s: &str| (!s.trim().is_empty()).then(|| PathBuf::from(s));
        let gif_min_bytes = match self.gif_min_mb {
            NO_GIFS => u64::MAX,
            mb if ALLOWED_GIF_MIN_MB.contains(&mb) => u64::from(mb) * 1024 * 1024,
            _ => d.gif_min_bytes,
        };
        let vmaf_floor = match self.vmaf_floor {
            0 => None,
            v if ALLOWED_VMAF_FLOOR.contains(&v) => Some(f64::from(v)),
            _ => d.vmaf_floor,
        };
        Options {
            crf: self.crf,
            preset: self.preset.clone(),
            encoder: self.encoder.clone(),
            concurrency: self.concurrency,
            originals: match self.originals {
                0 => Originals::Delete,
                2 => Originals::BackupFolder(non_empty(&self.backup_dir)),
                1 => Originals::RecycleBin,
                _ => d.originals,
            },
            gif_min_bytes,
            vmaf_floor,
            images: if self.images_on {
                match self.images {
                    1 => ImageMode::Lossless,
                    2..=4 => ImageMode::Lossy,
                    _ => d.images,
                }
            } else {
                ImageMode::Off
            },
            image_target: match self.images {
                3 => 85,
                4 => 80,
                _ => d.image_target,
            },
            convert: Conversions { wav: self.conv_wav, images: self.conv_img, cbr: self.conv_cbr },
            videos: self.videos,
            docs: self.docs,
            lossy_docs: self.lossy_docs != 0,
            audio: if self.audio_on {
                match self.audio {
                    1 => ImageMode::Lossless,
                    2 => ImageMode::Lossy,
                    _ => d.audio,
                }
            } else {
                ImageMode::Off
            },
            temp_dir: non_empty(&self.temp_dir),
            fix_ext: self.fix_ext != 0,
            gpu_decoding: self.gpu_decoding,
            max: crate::image::Max::from_names(&self.max_parts.iter().map(String::as_str).collect::<Vec<_>>()),
            drop_data: self.drop_data,
            strip: self.strip,
            redo_stripped: self.redo_stripped,
        }
        .validated()
    }
}

pub(super) fn labels<K>(choices: &[(K, &str)]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        choices.iter().map(|(_, l)| SharedString::from(*l)).collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One field of an unexpected type (an older or hand-edited file) costs that field only:
    /// the others, the backup folder above all, are kept, and the problem is reported.
    #[test]
    fn a_bad_field_doesnt_reset_the_other_settings() {
        let dir = std::env::temp_dir().join(format!("mo-prefs-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-settings.json");
        std::fs::write(&path, r#"{"version":1,"concurrency":"four","originals":2,"backup_dir":"F:\\backup"}"#).unwrap();
        let (p, problem) = Prefs::load(&path);
        assert_eq!((p.originals, p.backup_dir.as_str()), (2, r"F:\backup"));
        assert_eq!(p.concurrency, Prefs::default().concurrency);
        assert!(problem.is_some_and(|w| w.contains("concurrency")));
        // An unreadable file: defaults, reported.
        std::fs::write(&path, "{ not json").unwrap();
        let (p, problem) = Prefs::load(&path);
        assert!(p == Prefs::default() && problem.is_some());
        // No file yet: defaults, nothing to report.
        let (_, problem) = Prefs::load(&dir.join("none.json"));
        assert!(problem.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Saved settings name their format version, and the parts of maximum effort only when
    /// they differ from the default (so a better default reaches everyone who kept it).
    #[test]
    fn saved_settings_have_a_version_and_only_changed_parts() {
        let dir = std::env::temp_dir().join(format!("mo-prefs-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-settings.json");
        Prefs::default().save(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""version": 1"#) && !text.contains("max_parts"), "{text}");
        let custom = Prefs { max_parts: vec!["flac_e".into()], ..Prefs::default() };
        custom.save(&path);
        assert_eq!(Prefs::load(&path).0, custom);
        // A file from before versions: read like before, version 0.
        std::fs::write(&path, r#"{"encoder":"av1_nvenc","images":0}"#).unwrap();
        let (old, problem) = Prefs::load(&path);
        assert!(problem.is_none() && !old.images_on && old.version == PREFS_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_saved_values_fall_back_to_defaults() {
        let d = Prefs::default();
        // e.g. the removed OpenCL encoder or an out-of-range job count
        assert_eq!(position(ENCODERS, &"libx264_opencl", &d.encoder.as_str()), 0);
        assert_eq!(position(JOBS, &9, &d.concurrency), 3);
        assert_eq!(position(SPEEDS, &"medium", &d.preset.as_str()), 1);
    }

    #[test]
    fn advanced_options_default_to_the_measured_choice() {
        // Settings saved before Advanced options existed take the defaults.
        let old: Prefs = serde_json::from_str(r#"{"encoder":"av1_nvenc","concurrency":2}"#).unwrap();
        assert!(old.gpu_decoding);
        assert_eq!(old.max_parts, ["png_ect"]);
        let o = old.to_options();
        assert!(o.gpu_decoding);
        assert!(!old.drop_data && !o.drop_data && !old.strip && !o.strip);
        assert_eq!(o.max, crate::image::Max::chosen());
        // Chosen parts reach the engine; unknown names are ignored.
        let p = Prefs { gpu_decoding: false, drop_data: true, strip: true, max_parts: vec!["flac_e".into(), "bogus".into()], ..Prefs::default() };
        let o = p.to_options();
        assert!(!o.gpu_decoding && o.drop_data && o.strip);
        assert_eq!(o.max, crate::image::Max { flac_e: true, ..crate::image::Max::default() });
        // Quality floors and off-states map straight to Options.
        for (v, t) in [(2, 93), (3, 85), (4, 80)] {
            let o = Prefs { images: v, ..Prefs::default() }.to_options();
            assert_eq!((o.images, o.image_target), (ImageMode::Lossy, t));
        }
        for (level, t) in [(4, 80), (1, 93)] {
            let o = Prefs { images_on: false, images: level, ..Prefs::default() }.to_options();
            assert_eq!((o.images, o.image_target), (ImageMode::Off, t));
        }
        let o = Prefs { originals: 2, backup_dir: r"D:\old".into(), gif_min_mb: NO_GIFS, vmaf_floor: 0, ..Prefs::default() }.to_options();
        assert_eq!(o.originals, Originals::BackupFolder(Some(PathBuf::from(r"D:\old"))));
        assert_eq!(o.gif_min_bytes, u64::MAX);
        assert_eq!(o.vmaf_floor, None);
        // Every part has a name that round-trips.
        for (name, _, _) in crate::image::Max::PARTS {
            assert_eq!(crate::image::Max::from_names(&[name]).names(), [*name]);
        }
        assert_eq!(crate::image::Max::PARTS.len(), 10);
    }
}
