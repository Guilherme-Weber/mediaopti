//! Image optimization (PNG, JPEG, GIF), ported from imageopti.
//!
//! Every lossless step must decode to the same pixels and keep the colour description and
//! metadata ([`verify`]). Lossy steps (optional) are accepted only at the lowest quality that
//! keeps a SSIMULACRA2 score of at least the target against the original, and only when
//! they beat the lossless result by a margin. Chains were picked by benchmark:
//!
//! - PNG: beeim -sl=5 (pingo -s4 if beeim is missing) -> oxipng -o4. beeim drops colour
//!   and text chunks; they are copied back from the original.
//! - JPEG: jpegtran (mozjpeg) -> ECT. Lossy: jpegli, mozjpeg, pingo (only JPEGs saved at
//!   quality >= 90 can get smaller at the default target).
//! - GIF: gifsicle -O3.
//!
//! The "max" effort (imageopti's `--level max`) adds oxipng `-o max` and ECT `-9` for PNG,
//! ECT `-9` and pingo for JPEG, pingo as a lossy PNG method, and searches lossy JPEG
//! re-encodes of every file (not only those saved at quality >= 90) from quality 50.

pub mod jpeg;
pub mod png;
pub mod tools;
pub mod verify;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tools::{Capture, RunError, Tools};
use verify::Verifier;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Png,
    Jpg,
    Gif,
    /// Audio: cover art and re-encoding, see [`crate::audio`].
    Flac,
    Mp3,
    /// Converted to FLAC (integer PCM only), when conversions are on.
    Wav,
    /// Ogg Vorbis, repacked losslessly by OptiVorbis.
    Ogg,
    /// Other lossless audio (ALAC in .m4a, AIFF, APE, WavPack, TTA), converted to FLAC when
    /// conversions are on and the FLAC is smaller. (An .m4a holding AAC is left alone.)
    LosslessAudio,
    /// Converted to PNG (pixel-identical), when conversions are on.
    Bmp,
    Tga,
    /// Office, OpenDocument and EPUB: zip repacked, pictures inside optimized losslessly.
    Doc,
    /// Comic book archive (zip): pictures inside follow the image setting.
    Cbz,
    /// Comic book archive (RAR), converted to CBZ when conversions are on.
    Cbr,
    /// PDF: qpdf rewrite, images optimized (see [`crate::pdf`]).
    Pdf,
}

impl Kind {
    pub fn is_audio(self) -> bool {
        matches!(self, Kind::Flac | Kind::Mp3 | Kind::Wav | Kind::Ogg | Kind::LosslessAudio)
    }

    pub fn is_archive(self) -> bool {
        matches!(self, Kind::Doc | Kind::Cbz | Kind::Cbr)
    }

    /// Documents: PDF, Office/ODF/EPUB, comics.
    pub fn is_document(self) -> bool {
        matches!(self, Kind::Pdf | Kind::Doc | Kind::Cbz | Kind::Cbr)
    }

    /// The CPU queue stage this kind runs in: 0 audio (disk-heavy, few at a time), 1 documents
    /// (few at a time; their pages borrow idle CPU slots), 2 images (every CPU slot).
    pub fn stage(self) -> usize {
        if self.is_audio() {
            0
        } else if self.is_document() {
            1
        } else {
            2
        }
    }

}

/// Files for the CPU workers, by extension (GIFs are routed separately: big animated ones
/// become video).
pub fn kind_of(path: &Path) -> Option<Kind> {
    match crate::args::ext_lower(path).as_str() {
        "png" => Some(Kind::Png),
        "jpg" | "jpeg" | "jpe" | "jfif" => Some(Kind::Jpg),
        "gif" => Some(Kind::Gif),
        "flac" => Some(Kind::Flac),
        "mp3" => Some(Kind::Mp3),
        "wav" => Some(Kind::Wav),
        "m4a" | "aif" | "aiff" | "aifc" | "ape" | "wv" | "tta" => Some(Kind::LosslessAudio),
        "ogg" | "oga" => Some(Kind::Ogg),
        "bmp" => Some(Kind::Bmp),
        "tga" => Some(Kind::Tga),
        "docx" | "xlsx" | "pptx" | "docm" | "xlsm" | "pptm" | "odt" | "ods" | "odp" | "odg" | "epub" => Some(Kind::Doc),
        "cbz" => Some(Kind::Cbz),
        "cbr" => Some(Kind::Cbr),
        "pdf" => Some(Kind::Pdf),
        _ => None,
    }
}

/// PNG, JPEG or GIF by content, whatever the extension says (misnamed files are common:
/// a ".png" saved as JPEG by a website).
pub fn sniff_picture(path: &Path) -> Option<Kind> {
    let mut head = [0u8; 8];
    std::io::Read::read_exact(&mut std::fs::File::open(path).ok()?, &mut head).ok()?;
    match head {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] => Some(Kind::Png),
        [0xff, 0xd8, 0xff, ..] => Some(Kind::Jpg),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some(Kind::Gif),
        _ => None,
    }
}

/// Where a GIF ends (just after its trailer), following its blocks; `None` if damaged.
fn gif_end(d: &[u8]) -> Option<usize> {
    if d.len() < 13 || !d.starts_with(b"GIF8") {
        return None;
    }
    let table = |flags: u8| if flags & 0x80 != 0 { 3usize << ((flags & 7) + 1) } else { 0 };
    let mut i = 13 + table(d[10]);
    let skip_blocks = |mut i: usize| -> Option<usize> {
        loop {
            let n = usize::from(*d.get(i)?);
            i += 1 + n;
            if n == 0 {
                return Some(i);
            }
        }
    };
    loop {
        match *d.get(i)? {
            0x3B => return Some(i + 1),
            0x21 => i = skip_blocks(i + 2)?,
            0x2C => i = skip_blocks(i + 10 + table(*d.get(i + 9)?) + 1)?,
            _ => return None,
        }
    }
}

/// Whether a picture carries data after its end: a motion photo's video, an Ultra HDR gain
/// map, an MPO's second picture, an appended archive. Optimizers drop it and the pixel check
/// can't see it, so such files are left alone. (Zero padding doesn't count.)
pub fn has_data_after_end(kind: Kind, path: &Path) -> bool {
    let Ok(d) = std::fs::read(path) else { return false };
    let end = match kind {
        Kind::Jpg => jpeg::end_of_image(&d),
        Kind::Gif => gif_end(&d),
        Kind::Png => png::end_of_image(&d),
        _ => None,
    };
    end.is_some_and(|e| d.get(e..).is_some_and(|rest| rest.iter().any(|&b| b != 0)))
}

/// Whether a JPEG, PNG or GIF looks cut off: its structure can't be followed to its end
/// marker (a download that stopped, a damaged copy). Some valid files lack the marker too,
/// so this only explains why nothing smaller was made; it never stops a file.
pub fn looks_cut_off(kind: Kind, path: &Path) -> bool {
    let Ok(d) = std::fs::read(path) else { return false };
    let end = match kind {
        Kind::Jpg => jpeg::end_of_image(&d),
        Kind::Gif => gif_end(&d),
        Kind::Png => png::end_of_image(&d),
        _ => return false,
    };
    end.is_none()
}

/// The real extension of a picture by its content ("png", "jpg", "gif", "webp", "bmp"), for
/// renaming misnamed files. `None` for anything else.
pub fn real_picture_ext(path: &Path) -> Option<&'static str> {
    let mut head = [0u8; 12];
    std::io::Read::read_exact(&mut std::fs::File::open(path).ok()?, &mut head).ok()?;
    match sniff_picture(path) {
        Some(k) => Some(ext_of(k)),
        None if &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" => Some("webp"),
        None if &head[..2] == b"BM" && u32::from_le_bytes([head[2], head[3], head[4], head[5]]) > 26 => Some("bmp"),
        None => None,
    }
}

/// Whether extension `ext` (lower case) is a name for the format `real` gives.
pub fn ext_is(real: &str, ext: &str) -> bool {
    ext == real || (real == "jpg" && matches!(ext, "jpeg" | "jpe" | "jfif"))
}

/// Whether two pictures show the same thing: identical pixels and frame timing, or (single
/// frames of the same size) SSIMULACRA2 at or above the target. Used before removing one of
/// two files that would get the same name.
/// Width × height of a PNG, JPEG or GIF, from its header (whatever its extension says).
/// `None` for other files, or a JPEG whose frame header is past its first megabyte.
pub fn pixel_count(path: &Path) -> Option<u64> {
    let mut head = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(std::fs::File::open(path).ok()?, 1 << 20), &mut head).ok()?;
    let be32 = |at: usize| Some(u64::from(u32::from_be_bytes(head.get(at..at + 4)?.try_into().ok()?)));
    let le16 = |at: usize| Some(u64::from(u16::from_le_bytes(head.get(at..at + 2)?.try_into().ok()?)));
    match sniff_picture(path)? {
        Kind::Png => Some(be32(16)? * be32(20)?),
        Kind::Jpg => jpeg::dimensions(&head).map(|(w, h)| u64::from(w) * u64::from(h)),
        Kind::Gif => Some(le16(6)? * le16(8)?),
        _ => None,
    }
}

/// Whether two pictures carry the same metadata either way (whichever stays must not lose
/// anything the other has): JPEG APPn/COM segments, PNG text and colour chunks. Pictures of
/// different formats (a BMP and its PNG) have nothing comparable and count as the same.
pub fn same_metadata(ctx: &Ctx, a: &Path, b: &Path) -> bool {
    match (sniff_picture(a), sniff_picture(b)) {
        (Some(Kind::Jpg), Some(Kind::Jpg)) => match (jpeg::segments(a), jpeg::segments(b)) {
            (Some(x), Some(y)) => jpeg::meta_ok(&x, &y, ctx.strip) && jpeg::meta_ok(&y, &x, ctx.strip),
            _ => false,
        },
        (Some(Kind::Png), Some(Kind::Png)) => match (png::info(a), png::info(b)) {
            (Some(x), Some(y)) => png::meta_ok(&x, &y, ctx.strip) && png::meta_ok(&y, &x, ctx.strip),
            _ => false,
        },
        _ => true,
    }
}

pub fn same_picture(ctx: &Ctx, a: &Path, b: &Path) -> Result<bool, RunError> {
    let decode = |p: &Path| -> Result<Option<(Vec<u8>, usize)>, RunError> {
        let args = crate::os_args!["-v", "error", "-i", p, "-map", "0:v:0", "-fps_mode", "passthrough", "-enc_time_base", "demux", "-pix_fmt", "rgba", "-f", "framemd5", "-"];
        let ran = tools::run(&ctx.tools.ffmpeg, args, None, Capture::BytesUpTo(64 << 20), ctx.timeout, &ctx.gate)?;
        if !ran.success {
            return Ok(None);
        }
        let lines: Vec<u8> = String::from_utf8_lossy(&ran.stdout).lines().filter(|l| !l.starts_with('#')).flat_map(|l| l.bytes().chain(*b"\n")).collect();
        let frames = lines.iter().filter(|&&c| c == b'\n').count();
        Ok(Some((lines, frames)))
    };
    let (Some((da, fa)), Some((db, fb))) = (decode(a)?, decode(b)?) else { return Ok(false) };
    if da == db {
        return Ok(true);
    }
    Ok(fa == 1 && fb == 1 && ssim_score(ctx, a, b)?.is_some_and(|s| s >= ctx.target))
}

pub fn ext_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Png => "png",
        Kind::Jpg => "jpg",
        Kind::Gif => "gif",
        _ => "bin",
    }
}

/// The parts of the "maximum" effort, each one on or off (measured separately, see
/// [`Max::chosen`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Max {
    /// PNG: oxipng `-o max` instead of `-o 4`.
    pub oxipng_max: bool,
    /// PNG: ECT `-6` after oxipng.
    pub png_ect: bool,
    /// PNG lossy: pingo `-quality` as a third method.
    pub png_pingo: bool,
    /// JPEG: ECT `-9` instead of `-4`.
    pub jpg_ect9: bool,
    /// JPEG: pingo `-s4` lossless after ECT.
    pub jpg_pingo: bool,
    /// JPEG lossy: also JPEGs saved below quality 90.
    pub jpg_low_q: bool,
    /// JPEG lossy: search from quality 50 instead of 84.
    pub jpg_wide: bool,
    /// FLAC: re-encode files from libFLAC 1.4+ too.
    pub flac_all: bool,
    /// FLAC: `-e` (exhaustive model search).
    pub flac_e: bool,
    /// PDF: lossy JPEG re-encoding whatever the JPEG's quality.
    pub pdf_any_q: bool,
}

impl Max {
    /// The parts used by the Maximum setting: those that paid off when each was measured
    /// alone on random samples of the author's library (2026-11-11, lossy images):
    /// - ECT on PNG (-9 under 2 MP, -6 above): 80 real PNGs -2.57% -> -2.87%, Kodak 36.85% ->
    ///   37.05%, for about a quarter more time. Since 2026-11-14 always -6 (speed).
    ///
    /// Left out: oxipng `-o max` (+0.03 points, +77% time), pingo as a lossy PNG method (0),
    /// every JPEG part (0 on 80 real JPEGs; the q50 search +65% time), PDF JPEGs below q85 (0
    /// on 21 PDFs), re-encoding libFLAC 1.4+ files (-0.11%, under the replace threshold for
    /// almost every file) and flac `-e` (+0.02 points for 8x the time).
    pub fn chosen() -> Self {
        Self { png_ect: true, ..Self::default() }
    }

    /// Every part: its name (saved settings, `MEDIAOPTI_MAX_PARTS`), a label, and what it was
    /// measured to give (see [`Max::chosen`]). Every part keeps the same verification.
    pub const PARTS: &'static [(&'static str, &'static str, &'static str)] = &[
        ("png_ect", "PNG: ECT after oxipng", "Default. 80 real PNGs: -2.57% -> -2.87%, for about a quarter more time."),
        ("oxipng_max", "PNG: oxipng -o max instead of -o 4", "+0.03 points for +77% time."),
        ("png_pingo", "PNG (lossy): pingo -quality as a third method", "No gain measured."),
        ("jpg_ect9", "JPEG: ECT -9 instead of -4", "No gain measured on 80 real JPEGs."),
        ("jpg_pingo", "JPEG: pingo -s4 after ECT", "No gain measured on 80 real JPEGs."),
        ("jpg_low_q", "JPEG (lossy): also JPEGs saved below quality 90", "No gain measured on 80 real JPEGs."),
        ("jpg_wide", "JPEG (lossy): search from quality 50 instead of 84", "No gain measured; +65% time."),
        ("flac_all", "FLAC: re-encode files from libFLAC 1.4+ too", "-0.11%: under the replace threshold for almost every file."),
        ("flac_e", "FLAC: exhaustive model search (-e)", "+0.02 points for 8x the time."),
        ("pdf_any_q", "PDF (lossy): re-encode JPEGs whatever their quality", "No gain measured on 21 PDFs."),
    ];

    /// Whether the part called `name` is on.
    pub fn has(self, name: &str) -> bool {
        match name {
            "oxipng_max" => self.oxipng_max,
            "png_ect" => self.png_ect,
            "png_pingo" => self.png_pingo,
            "jpg_ect9" => self.jpg_ect9,
            "jpg_pingo" => self.jpg_pingo,
            "jpg_low_q" => self.jpg_low_q,
            "jpg_wide" => self.jpg_wide,
            "flac_all" => self.flac_all,
            "flac_e" => self.flac_e,
            "pdf_any_q" => self.pdf_any_q,
            _ => false,
        }
    }

    /// The names of the parts that are on.
    pub fn names(self) -> Vec<&'static str> {
        Self::PARTS.iter().map(|p| p.0).filter(|n| self.has(n)).collect()
    }

    pub fn from_names(names: &[&str]) -> Self {
        let on = |n: &str| names.contains(&n);
        Self {
            oxipng_max: on("oxipng_max"),
            png_ect: on("png_ect"),
            png_pingo: on("png_pingo"),
            jpg_ect9: on("jpg_ect9"),
            jpg_pingo: on("jpg_pingo"),
            jpg_low_q: on("jpg_low_q"),
            jpg_wide: on("jpg_wide"),
            flac_all: on("flac_all"),
            flac_e: on("flac_e"),
            pdf_any_q: on("pdf_any_q"),
        }
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub tools: Tools,
    pub lossy: bool,
    /// Minimum SSIMULACRA2 score for lossy results (93 ~ pingo -quality=100).
    pub target: f64,
    /// A lossy result must be this much smaller than the lossless one (0.03 = 3%).
    pub lossy_min_gain: f64,
    pub strip: bool,
    pub strict_alpha: bool,
    /// PDFs may get lossy images (at the target, every rendered page checked).
    pub lossy_docs: bool,
    /// Several files run at once: keep each tool single-threaded.
    pub parallel: bool,
    pub timeout: Duration,
    /// Stop and Pause.
    pub gate: tools::Gate,
    /// The "maximum" effort parts in use (all off at normal effort).
    pub max: Max,
    /// The image workers' CPU slots (borrowed by jobs with many parts), if shared.
    pub slots: Option<std::sync::Arc<tools::Slots>>,
    /// This job's progress (pages of a comic, images of a PDF), if someone shows it.
    pub progress: Option<std::sync::Arc<tools::Progress>>,
}

impl Ctx {
    /// Shows what the job does now; `reads` are the files its tools will read whole (their
    /// sizes give the percentage), empty if unknown.
    pub fn phase(&self, name: &'static str, reads: &[&Path]) {
        if let Some(p) = &self.progress {
            p.begin(name, reads.iter().filter_map(|f| size(f)).sum());
        }
    }
}

pub struct Optimized {
    /// The smallest verified result, or `None` if nothing was smaller.
    pub best: Option<PathBuf>,
    pub steps: Vec<String>,
    /// Why the file was not processed, if it wasn't.
    pub note: Option<String>,
    /// The result is another format, saved with this extension (e.g. WAV -> "flac").
    pub new_ext: Option<&'static str>,
}

impl Optimized {
    pub fn found(best: Option<PathBuf>, steps: Vec<String>) -> Self {
        Self { best, steps, note: None, new_ext: None }
    }

    pub fn nothing() -> Self {
        Self::found(None, Vec::new())
    }

    pub fn skipped(why: &str) -> Self {
        Self { note: Some(why.to_string()), ..Self::nothing() }
    }
}

/// Content-history mode for images, compatible with imageopti's history.
/// The history mode of a pass (imageopti's names; `-strip` when metadata was removed).
pub fn mode(lossy: bool, strip: bool) -> &'static str {
    match (lossy, strip) {
        (true, false) => "lossy-normal-t93",
        (true, true) => "lossy-normal-t93-strip",
        (false, false) => "lossless-normal",
        (false, true) => "lossless-normal-strip",
    }
}

pub(crate) fn run(ctx: &Ctx, tool: &str, args: Vec<OsString>, cwd: &Path) -> Result<bool, RunError> {
    let Some(program) = ctx.tools.get(tool) else { return Ok(false) };
    Ok(tools::run(program, args, Some(cwd), Capture::None, ctx.timeout, &ctx.gate)?.success)
}

pub(crate) fn size(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok().map(|m| m.len()).filter(|&n| n > 0)
}

/// Feeds the current best file to each step and keeps only verified gains.
struct Chain<'a> {
    ctx: &'a Ctx,
    work: PathBuf,
    best: PathBuf,
    best_size: u64,
    steps: Vec<String>,
    n: usize,
    ext: String,
}

impl<'a> Chain<'a> {
    fn new(ctx: &'a Ctx, work: &Path, src: &Path) -> Self {
        Self {
            ctx,
            work: work.to_path_buf(),
            best: src.to_path_buf(),
            best_size: size(src).unwrap_or(u64::MAX),
            steps: Vec::new(),
            n: 0,
            ext: crate::args::ext_lower(src),
        }
    }

    fn next_out(&mut self) -> PathBuf {
        self.n += 1;
        self.work.join(format!("s{}.{}", self.n, self.ext))
    }

    /// Runs a tool that edits its file in place on a copy of the current best.
    fn inplace(
        &mut self,
        label: &str,
        tool: &str,
        args: impl FnOnce(&Path) -> Vec<OsString>,
        check: &mut dyn FnMut(&Path) -> Result<bool, RunError>,
        repair: Option<&dyn Fn(&Path)>,
    ) -> Result<(), RunError> {
        if !self.ctx.tools.has(tool) {
            return Ok(());
        }
        let out = self.next_out();
        if std::fs::copy(&self.best, &out).is_err() {
            return Ok(());
        }
        let ok = run(self.ctx, tool, args(&out), &self.work)?;
        self.offer(label, &out, ok, check, repair)
    }

    /// Runs a tool that reads the current best and writes `out`.
    fn write_to(
        &mut self,
        label: &str,
        tool: &str,
        args: impl FnOnce(&Path, &Path) -> Vec<OsString>,
        check: &mut dyn FnMut(&Path) -> Result<bool, RunError>,
        repair: Option<&dyn Fn(&Path)>,
    ) -> Result<(), RunError> {
        if !self.ctx.tools.has(tool) {
            return Ok(());
        }
        let out = self.next_out();
        let ok = run(self.ctx, tool, args(&self.best.clone(), &out), &self.work)?;
        self.offer(label, &out, ok, check, repair)
    }

    fn offer(
        &mut self,
        label: &str,
        out: &Path,
        ran_ok: bool,
        check: &mut dyn FnMut(&Path) -> Result<bool, RunError>,
        repair: Option<&dyn Fn(&Path)>,
    ) -> Result<(), RunError> {
        let Some(mut s) = size(out).filter(|_| ran_ok) else { return Ok(()) };
        if s < self.best_size {
            if let Some(r) = repair {
                r(out);
                s = size(out).unwrap_or(u64::MAX);
            }
        }
        if s < self.best_size && check(out)? {
            self.best = out.to_path_buf();
            self.best_size = s;
            self.steps.push(label.to_string());
        }
        Ok(())
    }
}

/// Optimizes `src` (a private copy inside `work`). Returns the best verified result.
pub fn optimize(ctx: &Ctx, kind: Kind, src: &Path, work: &Path) -> Result<Optimized, RunError> {
    if kind.is_audio() {
        return crate::audio::optimize(ctx, kind, src, work);
    }
    if kind == Kind::Pdf {
        return crate::pdf::optimize(ctx, src, work);
    }
    if kind.is_archive() {
        return crate::archive::optimize(ctx, kind, src, work);
    }
    if matches!(kind, Kind::Bmp | Kind::Tga) {
        return to_png(ctx, kind, src, work);
    }
    if has_data_after_end(kind, src) {
        return Ok(Optimized::skipped("it has data after the end of the picture (a motion photo, second picture or gain map): kept as it is"));
    }
    let ver = Verifier::new(ctx, kind, src);
    let skip = |why: &str| Ok(Optimized::skipped(why));
    match kind {
        Kind::Png if ver.png.is_none() => return skip("not a PNG"),
        Kind::Jpg if ver.segs.is_none() => return skip("not a JPEG"),
        _ => {}
    }
    ctx.phase("lossless", &[]);
    let mut chain = match kind {
        Kind::Png => png_chain(ctx, work, src, src, &ver)?,
        Kind::Jpg => jpg_chain(ctx, work, src, &ver)?,
        Kind::Gif => {
            let mut c = Chain::new(ctx, work, src);
            let mut check = |p: &Path| ver.ok(p);
            let strip: &[&str] = if ctx.strip { &["--no-comments", "--no-names"] } else { &[] };
            c.write_to("gifsicle", "gifsicle", |i, o| {
                let mut a = crate::os_args!["-O3", "--no-warnings"];
                a.extend(strip.iter().map(Into::into));
                a.extend(crate::os_args!["-o", o, i]);
                a
            }, &mut check, None)?;
            c
        }
        _ => unreachable!("handled above"),
    };
    if ctx.lossy && ctx.tools.has("ssimulacra2") && matches!(kind, Kind::Png | Kind::Jpg) {
        ctx.phase("lossy search", &[]);
        match kind {
            Kind::Png => lossy_png(ctx, work, src, &mut chain, &ver)?,
            Kind::Jpg => lossy_jpeg(ctx, work, src, &mut chain, &ver)?,
            _ => {}
        }
    }
    let best = (chain.best != src).then_some(chain.best);
    Ok(Optimized::found(best, chain.steps))
}

/// BMP and TGA files bigger than this are left alone (their alpha plane is read into memory).
const TO_PNG_MAX: u64 = 256 * 1024 * 1024;

/// Whether a BMP/TGA stores more than 8 bits per channel (BMP bit count or TGA pixel depth
/// above 32, e.g. 64-bit BMP). `None` if the header isn't one.
fn more_than_8_bits(kind: Kind, path: &Path) -> Option<bool> {
    let mut head = [0u8; 30];
    std::io::Read::read_exact(&mut std::fs::File::open(path).ok()?, &mut head).ok()?;
    match kind {
        Kind::Bmp if &head[..2] == b"BM" => Some(u16::from_le_bytes([head[28], head[29]]) > 32),
        Kind::Tga => Some(head[16] > 32),
        _ => None,
    }
}

/// BMP/TGA -> PNG (pixel-identical), then the normal PNG pipeline. An alpha channel that is
/// all 0 (junk that viewers ignore) or all 255 (opaque) is dropped.
fn to_png(ctx: &Ctx, kind: Kind, src: &Path, work: &Path) -> Result<Optimized, RunError> {
    if std::fs::metadata(src).map_or(true, |m| m.len() > TO_PNG_MAX) {
        return Ok(Optimized::skipped("too large to convert"));
    }
    let Some(deep) = more_than_8_bits(kind, src) else { return Ok(Optimized::skipped("not a BMP/TGA image")) };
    ctx.phase("converting", &[]);
    let ffmpeg = &ctx.tools.ffmpeg;
    let decode = |args: Vec<OsString>, capture: Capture| -> Result<Option<tools::Ran>, RunError> {
        let mut a = crate::os_args!["-v", "error", "-i", src, "-frames:v", "1"];
        a.extend(args);
        let ran = tools::run(ffmpeg, a, None, capture, ctx.timeout, &ctx.gate)?;
        Ok(ran.success.then_some(ran))
    };
    // alphaextract fails on images without an alpha channel.
    let alpha_plane = decode(crate::os_args!["-vf", "alphaextract", "-f", "rawvideo", "-pix_fmt", "gray", "-"], Capture::Bytes)?;
    let alpha = alpha_plane.is_some_and(|a| !a.stdout.is_empty() && !a.stdout.iter().all(|&v| v == a.stdout[0] && (v == 0 || v == 255)));
    let png = work.join("conv.png");
    // Without alpha, ffmpeg picks the PNG layout (palette, grey, RGB).
    let layout = if alpha { "rgba|rgba64be" } else { "pal8|gray|gray16be|rgb24|rgb48be" };
    let args = crate::os_args!["-vf", format!("format=pix_fmts={layout}"), "-y", &png];
    if decode(args, Capture::None)?.is_none() || !png.is_file() {
        return Ok(Optimized::skipped("could not convert it to PNG"));
    }
    // Compared at the source's depth: at 16 bits per channel for deep sources (so lost
    // precision is caught), else at 8 (expanding 5-bit RGB555 straight to 16 bits rounds
    // differently from going through 8, a false mismatch).
    let fmt = match (alpha, deep) {
        (true, true) => "rgba64le",
        (true, false) => "rgba",
        (false, true) => "rgb48le",
        (false, false) => "rgb24",
    };
    let hash = |p: &Path| -> Result<Option<[u8; 32]>, RunError> {
        let a = crate::os_args!["-v", "error", "-i", p, "-frames:v", "1", "-pix_fmt", fmt, "-f", "rawvideo", "-"];
        let ran = tools::run(ffmpeg, a, None, Capture::Hash, ctx.timeout, &ctx.gate)?;
        Ok(ran.hash.filter(|_| ran.success))
    };
    let reference = hash(src)?;
    if reference.is_none() || hash(&png)? != reference {
        return Ok(Optimized::skipped("the PNG is not pixel-identical: kept"));
    }
    let r = optimize(ctx, Kind::Png, &png, work)?;
    let steps = std::iter::once("to-png".to_string()).chain(r.steps).collect();
    Ok(Optimized { new_ext: Some("png"), ..Optimized::found(Some(r.best.unwrap_or(png)), steps) })
}

/// Lossless PNG chain; `restore_from` is where dropped chunks are copied back from.
fn png_chain<'a>(ctx: &'a Ctx, work: &Path, src: &Path, restore_from: &Path, ver: &Verifier) -> Result<Chain<'a>, RunError> {
    let mut c = Chain::new(ctx, work, src);
    let apng = ver.png.as_ref().is_some_and(png::PngInfo::is_apng);
    let strip = ctx.strip;
    let repair = |p: &Path| {
        let _ = png::restore_chunks(restore_from, p, strip);
    };
    let mut check = |p: &Path| ver.ok(p);
    let use_beeim = ctx.tools.has("beeim") && !apng;
    if use_beeim {
        c.inplace("beeim", "beeim", |o| crate::os_args!["-sl=5", "-force", o], &mut check, Some(&repair))?;
    }
    // beeim recolours fully transparent pixels, which strict alpha rejects: pingo -noalpha
    // keeps them, so it is tried too on images with alpha.
    let alpha_img = ver.png.as_ref().is_some_and(png::PngInfo::has_alpha);
    if !use_beeim || (ctx.strict_alpha && alpha_img) {
        let alpha = if ctx.strict_alpha { "-noalpha" } else { "-quiet" };
        c.inplace("pingo", "pingo", |o| crate::os_args!["-s4", "-lossless", "-quiet", "-nostrip", alpha, o], &mut check, Some(&repair))?;
    }
    let mut args = crate::os_args!["-o", if ctx.max.oxipng_max { "max" } else { "4" }, "-q"];
    if ctx.parallel {
        args.extend(crate::os_args!["-t", "1"]);
    }
    if !ctx.strict_alpha {
        args.push("--alpha".into());
    }
    if strip {
        args.extend(crate::os_args!["--strip", "safe"]);
    }
    c.write_to(
        "oxipng",
        "oxipng",
        |i, o| {
            let mut a = args;
            a.extend(crate::os_args!["--out", o, i]);
            a
        },
        &mut check,
        Some(&repair),
    )?;
    if ctx.max.png_ect && !apng {
        let strict = ctx.strict_alpha;
        // -6, not -9: -9 costs ~5x the time (Pictures sample: -0.50% vs -0.26% under 2 MP,
        // -0.39% vs -0.29% above), and ECT was ~38% of all tool time on a lossy image run.
        let args = |o: &Path| {
            let mut a = crate::os_args!["-6", "-quiet"];
            if strict {
                a.push("--strict".into()); // keeps fully transparent pixels
            }
            a.push(o.into());
            a
        };
        c.inplace("ect", "ect", args, &mut check, Some(&repair))?;
    }
    Ok(c)
}

fn jpg_chain<'a>(ctx: &'a Ctx, work: &Path, src: &Path, ver: &Verifier) -> Result<Chain<'a>, RunError> {
    let mut c = Chain::new(ctx, work, src);
    let mut check = |p: &Path| ver.ok(p);
    let copy = if ctx.strip && ver.segs.as_ref().and_then(jpeg::exif_orientation).unwrap_or(1) == 1 { "icc" } else { "all" };
    c.write_to(
        "jpegtran",
        "jpegtran",
        |i, o| crate::os_args!["-copy", copy, "-optimize", "-progressive", "-outfile", o, i],
        &mut check,
        None,
    )?;
    let level = if ctx.max.jpg_ect9 { "-9" } else { "-4" };
    c.inplace("ect", "ect", |o| crate::os_args![level, "-progressive", "-quiet", o], &mut check, None)?;
    if ctx.max.jpg_pingo {
        c.inplace("pingo", "pingo", |o| crate::os_args!["-s4", "-lossless", "-quiet", "-nostrip", o], &mut check, None)?;
    }
    Ok(c)
}

// ---------------------------------------------------------------- lossy

/// SSIMULACRA2 of `b` against `a`. Retries on plain RGBA copies when ssimulacra2 can't
/// read a file (it rejects some valid PNGs).
pub(crate) fn ssim_score(ctx: &Ctx, a: &Path, b: &Path) -> Result<Option<f64>, RunError> {
    let Some(ssim) = ctx.tools.get("ssimulacra2") else { return Ok(None) };
    let score = |x: &Path, y: &Path| -> Result<Option<f64>, RunError> {
        let ran = tools::run(ssim, crate::os_args![x, y], None, Capture::Bytes, ctx.timeout, &ctx.gate)?;
        Ok(ran
            .success
            .then(|| String::from_utf8_lossy(&ran.stdout).split_whitespace().last().and_then(|t| t.parse().ok()))
            .flatten())
    };
    if let Some(s) = score(a, b)? {
        return Ok(Some(s));
    }
    let (na, nb) = (b.with_extension("ref.png"), b.with_extension("cmp.png"));
    for (from, to) in [(a, &na), (b, &nb)] {
        let args = crate::os_args!["-v", "error", "-y", "-i", from, "-frames:v", "1", "-pix_fmt", "rgba", to];
        if !tools::run(&ctx.tools.ffmpeg, args, None, Capture::None, ctx.timeout, &ctx.gate)?.success {
            return Ok(None);
        }
    }
    let s = score(&na, &nb);
    let _ = std::fs::remove_file(&nb);
    s
}

pub(crate) type Encoder<'e> = Box<dyn FnMut(u32) -> Result<Option<PathBuf>, RunError> + 'e>;

/// Binary-searches quality `n` in `lo..=hi` (higher = better) for the smallest output that
/// still scores >= target and is below `limit` bytes. One probe at a fairly strong setting
/// first: already-optimized images don't get smaller there, and are skipped cheaply.
pub(crate) fn quality_search(ctx: &Ctx, reference: &Path, lo: u32, hi: u32, encode: &mut Encoder, limit: u64) -> Result<Option<PathBuf>, RunError> {
    let probe = hi - (hi - lo) / 4;
    let first = encode(probe)?;
    if first.as_deref().and_then(size).is_none_or(|s| s >= limit) {
        return Ok(None);
    }
    let (mut lo, mut hi) = (i64::from(lo), i64::from(hi));
    let mut best: Option<(PathBuf, u64)> = None;
    let mut cached = Some((probe, first));
    while lo <= hi {
        let n = ((lo + hi) / 2) as u32;
        let out = match cached.take() {
            Some((p, out)) if p == n => out,
            other => {
                cached = other;
                encode(n)?
            }
        };
        let Some((out, s)) = out.and_then(|o| size(&o).map(|s| (o, s))) else {
            lo = i64::from(n) + 1;
            continue;
        };
        if s >= limit {
            hi = i64::from(n) - 1; // too big to be useful: go lower, no need to score it
            continue;
        }
        if ssim_score(ctx, reference, &out)?.is_some_and(|q| q >= ctx.target) {
            if best.as_ref().is_none_or(|(_, b)| s < *b) {
                best = Some((out, s));
            }
            hi = i64::from(n) - 1;
        } else {
            lo = i64::from(n) + 1;
        }
    }
    Ok(best.map(|(p, _)| p))
}

/// Tries each method, each limited by the best result so far.
pub(crate) fn search_methods(ctx: &Ctx, reference: &Path, methods: Vec<(&str, u32, u32, Encoder)>, limit: u64) -> Result<Option<(PathBuf, String)>, RunError> {
    let mut best: Option<(PathBuf, String, u64)> = None;
    for (name, lo, hi, mut enc) in methods {
        let cap = best.as_ref().map_or(limit, |b| b.2);
        if let Some(c) = quality_search(ctx, reference, lo, hi, &mut enc, cap)? {
            let s = size(&c).unwrap_or(u64::MAX);
            let q: String = c.file_stem().map(|f| f.to_string_lossy().chars().filter(char::is_ascii_digit).collect()).unwrap_or_default();
            best = Some((c, format!("{name}={q}"), s));
        }
    }
    Ok(best.map(|(p, l, _)| (p, l)))
}

fn lossy_png(ctx: &Ctx, work: &Path, src: &Path, chain: &mut Chain, ver: &Verifier) -> Result<(), RunError> {
    if ver.png.as_ref().is_some_and(png::PngInfo::is_apng) {
        return Ok(());
    }
    let lw = work.join("lossy");
    let _ = std::fs::create_dir_all(&lw);
    let strip = ctx.strip;
    // beeim -quantizer won the benchmark (36% at >= 93 on photos, vs 27% for pingo).
    // -predicter is tried after it and wins on some photos (Kodak: 5 of 24, up to 5.6
    // points more); the other beeim/pingo lossy modes never beat both.
    let inplace = |tag: &'static str, tool: &'static str, args: fn(u32, &Path) -> Vec<OsString>| -> Encoder {
        let lw = lw.clone();
        Box::new(move |n| {
            let out = lw.join(format!("{tag}{n}.png"));
            if std::fs::copy(src, &out).is_err() || !run(ctx, tool, args(n, &out), &lw)? {
                return Ok(None);
            }
            let _ = png::restore_chunks(src, &out, strip);
            Ok(Some(out))
        })
    };
    let mut methods: Vec<(&str, u32, u32, Encoder)> = Vec::new();
    if ctx.tools.has("beeim") {
        methods.push(("beeim-quantizer", 1, 100, inplace("bq", "beeim", |n, o| crate::os_args![format!("-quantizer={n}"), "-force", o])));
        methods.push(("beeim-predicter", 1, 100, inplace("bp", "beeim", |n, o| crate::os_args![format!("-predicter={n}"), "-force", o])));
    }
    if ctx.tools.has("pingo") && (ctx.max.png_pingo || !ctx.tools.has("beeim")) {
        methods.push(("pingo-quality", 50, 100, inplace("pq", "pingo", |n, o| crate::os_args![format!("-quality={n}"), "-nostrip", "-quiet", o])));
    }
    let limit = (chain.best_size as f64 * (1.0 - ctx.lossy_min_gain)) as u64;
    let Some((cand, label)) = search_methods(ctx, src, methods, limit)? else { return Ok(()) };
    if !ver.meta_ok(&cand) {
        return Ok(());
    }
    // Squeeze the lossy result losslessly too (verified against itself).
    let post_dir = lw.join("post");
    let _ = std::fs::create_dir_all(&post_dir);
    let cand_ver = Verifier::new(ctx, Kind::Png, &cand);
    let post = png_chain(ctx, &post_dir, &cand, src, &cand_ver)?;
    if post.best_size <= limit {
        chain.best = post.best;
        chain.best_size = post.best_size;
        chain.steps = std::iter::once(label).chain(post.steps).collect();
    }
    Ok(())
}

fn lossy_jpeg(ctx: &Ctx, work: &Path, src: &Path, chain: &mut Chain, ver: &Verifier) -> Result<(), RunError> {
    let Some(segs) = ver.segs.as_ref() else { return Ok(()) };
    // Only JPEGs saved at high quality get smaller at >= 93: on a 200-JPEG sample, q >= 90
    // kept 24 of 25 wins and skipped a third of the files.
    if !ctx.max.jpg_low_q && jpeg::quality(src).is_some_and(|q| q < 90.0) {
        return Ok(());
    }
    // Winners were q90-98 on the Pictures sample; max searches from 50.
    let lo_q = if ctx.max.jpg_wide { 50 } else { 84 };
    let lw = work.join("lossy");
    let _ = std::fs::create_dir_all(&lw);
    let pnm = lw.join("dec.pnm");
    let decoded = run(ctx, "djpeg", crate::os_args!["-outfile", &pnm, src], &lw)?;
    let keep = jpeg::keep_for_reencode(segs, ctx.strip);
    let reencoder = |tag: &'static str, tool: &'static str, args: fn(u32, &Path, &Path) -> Vec<OsString>| -> Encoder {
        let (lw, pnm, keep) = (lw.clone(), pnm.clone(), keep.clone());
        Box::new(move |n| {
            let out = lw.join(format!("{tag}{n}.jpg"));
            if !run(ctx, tool, args(n, &pnm, &out), &lw)? {
                return Ok(None);
            }
            if let Ok(data) = std::fs::read(&out) {
                let _ = std::fs::write(&out, jpeg::with_segments(&data, &keep));
            }
            Ok(Some(out))
        })
    };
    let mut methods: Vec<(&str, u32, u32, Encoder)> = Vec::new();
    if decoded {
        // jpegli first: 2.7% vs mozjpeg's 1.9% at >= 93, best on 26 of 37 images.
        if ctx.tools.has("cjpegli") {
            methods.push(("jpegli-q", lo_q, 100, reencoder("jl", "cjpegli", |n, i, o| crate::os_args![i, o, "-q", n.to_string()])));
        }
        if ctx.tools.has("cjpeg") {
            methods.push(("mozjpeg-q", lo_q, 98, reencoder("mz", "cjpeg", |n, i, o| crate::os_args!["-quality", n.to_string(), "-outfile", o, i])));
        }
    }
    if ctx.tools.has("pingo") {
        let lw2 = lw.clone();
        methods.push((
            "pingo-quality",
            if ctx.max.jpg_wide { 50 } else { 90 },
            100,
            Box::new(move |n| {
                let out = lw2.join(format!("pg{n}.jpg"));
                if std::fs::copy(src, &out).is_err() || !run(ctx, "pingo", crate::os_args![format!("-quality={n}"), "-nostrip", "-quiet", &out], &lw2)? {
                    return Ok(None);
                }
                Ok(Some(out))
            }),
        ));
    }
    let limit = (chain.best_size as f64 * (1.0 - ctx.lossy_min_gain)) as u64;
    let Some((cand, label)) = search_methods(ctx, src, methods, limit)? else { return Ok(()) };
    // Re-encoders drop the Adobe marker on purpose.
    let ok = jpeg::segments(&cand).is_some_and(|n| jpeg::meta_ok(&jpeg::without_adobe(segs), &n, ctx.strip));
    if ok {
        if let Some(s) = size(&cand).filter(|&s| s <= limit) {
            chain.best = cand;
            chain.best_size = s;
            chain.steps = vec![label];
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Option<Tools> {
        let ffmpeg = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
        let t = Tools::discover(ffmpeg.clone(), &std::env::temp_dir());
        (ffmpeg.exists() && ["oxipng", "jpegtran", "djpeg", "ssimulacra2"].iter().all(|n| t.has(n))).then_some(t)
    }

    fn ctx(tools: Tools, lossy: bool) -> Ctx {
        Ctx {
            tools,
            lossy,
            target: 93.0,
            lossy_min_gain: 0.03,
            strip: false,
            strict_alpha: false,
            lossy_docs: false,
            parallel: false,
            timeout: Duration::from_secs(120),
            gate: tools::Gate::default(),
            max: crate::image::Max::default(),
            slots: None,
            progress: None,
        }
    }

    fn make(ff: &Path, args: &[&str], out: &Path) {
        let ok = std::process::Command::new(ff).args(["-v", "error", "-y"]).args(args).arg(out).status().unwrap().success();
        assert!(ok, "could not make {}", out.display());
    }

    #[test]
    fn png_and_jpeg_lossless_and_lossy() {
        let Some(t) = tools() else {
            eprintln!("skipping: image tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ff = t.ffmpeg.clone();
        let png = dir.join("photo.png");
        make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=640x480", "-frames:v", "1", "-compression_level", "1"], &png);
        let jpg = dir.join("photo.jpg");
        make(&ff, &["-f", "lavfi", "-i", "testsrc2=s=800x600", "-frames:v", "1", "-q:v", "1"], &jpg);

        for (src, kind) in [(&png, Kind::Png), (&jpg, Kind::Jpg)] {
            let before = size(src).unwrap();
            // lossless: smaller and pixel-identical
            let c = ctx(t.clone(), false);
            let work = dir.join(format!("w-{kind:?}"));
            std::fs::create_dir_all(&work).unwrap();
            let r = optimize(&c, kind, src, &work).unwrap();
            let best = r.best.unwrap_or_else(|| panic!("{kind:?}: no lossless gain"));
            assert!(size(&best).unwrap() < before);
            assert!(Verifier::new(&c, kind, src).ok(&best).unwrap(), "{kind:?}: pixels changed");
            println!("{kind:?} lossless {before} -> {} {:?}", size(&best).unwrap(), r.steps);

            // lossy: smaller still and SSIMULACRA2 >= 93
            let c = ctx(t.clone(), true);
            let work = dir.join(format!("l-{kind:?}"));
            std::fs::create_dir_all(&work).unwrap();
            let r = optimize(&c, kind, src, &work).unwrap();
            let best = r.best.unwrap();
            let score = ssim_score(&c, src, &best).unwrap().unwrap();
            println!("{kind:?} lossy {before} -> {} {:?} ssim {score:.1}", size(&best).unwrap(), r.steps);
            assert!(score >= 93.0, "{kind:?}: quality {score}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Metadata survives by default and is removed only with `strip`: a PNG text chunk, a
    /// JPEG's EXIF and a GIF comment. Pixels stay identical either way.
    #[test]
    fn metadata_is_removed_only_when_asked() {
        let Some(t) = tools().filter(|t| t.has("gifsicle")) else {
            eprintln!("skipping: image tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-strip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ff = t.ffmpeg.clone();
        let marker = b"mediaopti-marker";

        let png = dir.join("text.png");
        make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=320x240", "-frames:v", "1", "-compression_level", "1"], &png);
        let mut data = std::fs::read(&png).unwrap();
        let mut body = b"Comment\0".to_vec();
        body.extend_from_slice(marker);
        let mut chunk = (body.len() as u32).to_be_bytes().to_vec();
        let typed: Vec<u8> = [&b"tEXt"[..], &body].concat();
        chunk.extend_from_slice(&typed);
        chunk.extend_from_slice(&crc32fast::hash(&typed).to_be_bytes());
        let iend = data.len() - 12;
        data.splice(iend..iend, chunk);
        std::fs::write(&png, &data).unwrap();

        let jpg = dir.join("exif.jpg");
        make(&ff, &["-f", "lavfi", "-i", "testsrc2=s=400x300", "-frames:v", "1", "-q:v", "1"], &jpg);
        let mut data = std::fs::read(&jpg).unwrap();
        let mut exif = b"Exif\0\0II*\0\x08\0\0\0\0\0".to_vec();
        exif.extend_from_slice(marker);
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);
        data.splice(2..2, app1);
        std::fs::write(&jpg, &data).unwrap();

        let plain_gif = dir.join("plain.gif");
        make(&ff, &["-f", "lavfi", "-i", "testsrc=s=200x150:d=0.5:r=10"], &plain_gif);
        let gif = dir.join("comment.gif");
        let ok = std::process::Command::new(t.get("gifsicle").unwrap())
            .args(["--comment", std::str::from_utf8(marker).unwrap()])
            .arg(&plain_gif)
            .arg("-o")
            .arg(&gif)
            .status()
            .unwrap()
            .success();
        assert!(ok);

        // PNG eXIf: removed when the picture isn't turned, kept when it is.
        let with_exif = |name: &str, orientation: u8| {
            let p = dir.join(name);
            make(&ff, &["-f", "lavfi", "-i", "mandelbrot=s=320x240", "-frames:v", "1", "-compression_level", "1"], &p);
            let mut data = std::fs::read(&p).unwrap();
            // TIFF header, one IFD entry: Orientation (0x0112), SHORT, 1 value.
            let mut exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            exif.extend([orientation, 0, 0, 0, 0, 0, 0, 0]);
            exif.extend_from_slice(marker);
            let typed: Vec<u8> = [&b"eXIf"[..], &exif].concat();
            let mut chunk = (exif.len() as u32).to_be_bytes().to_vec();
            chunk.extend_from_slice(&typed);
            chunk.extend_from_slice(&crc32fast::hash(&typed).to_be_bytes());
            data.splice(33..33, chunk); // right after IHDR
            std::fs::write(&p, &data).unwrap();
            p
        };
        let upright = with_exif("upright.png", 1);
        let turned = with_exif("turned.png", 6);
        assert!(png::info(&turned).unwrap().rotated && !png::info(&upright).unwrap().rotated);

        let has_marker = |p: &Path| std::fs::read(p).unwrap().windows(marker.len()).any(|w| w == marker);
        for (src, keep_when_stripping) in [(&upright, false), (&turned, true)] {
            let c = Ctx { strip: true, ..ctx(t.clone(), false) };
            let work = dir.join(format!("w-exif-{keep_when_stripping}"));
            std::fs::create_dir_all(&work).unwrap();
            let r = optimize(&c, Kind::Png, src, &work).unwrap();
            let best = r.best.unwrap_or_else(|| panic!("no gain ({:?})", r.note));
            assert_eq!(has_marker(&best), keep_when_stripping, "{}", src.display());
        }
        for (src, kind) in [(&png, Kind::Png), (&jpg, Kind::Jpg), (&gif, Kind::Gif)] {
            assert!(has_marker(src), "{kind:?}: test file lacks the metadata");
            for strip in [false, true] {
                let c = Ctx { strip, ..ctx(t.clone(), false) };
                let work = dir.join(format!("w-{kind:?}-{strip}"));
                std::fs::create_dir_all(&work).unwrap();
                let r = optimize(&c, kind, src, &work).unwrap();
                let best = r.best.unwrap_or_else(|| panic!("{kind:?} strip={strip}: no gain ({:?})", r.note));
                assert_eq!(has_marker(&best), !strip, "{kind:?} strip={strip}");
                assert!(Verifier::new(&c, kind, src).ok(&best).unwrap(), "{kind:?}: pixels changed");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real photos from a folder (`MEDIAOPTI_TEST_IMAGES`): the lossy search must find
    /// what imageopti finds (Kodak PNG ~26%, a quality-97 JPEG ~28%).
    #[test]
    #[ignore = "needs MEDIAOPTI_TEST_IMAGES pointing at real photos"]
    fn lossy_matches_imageopti_on_real_photos() {
        let (Some(t), Ok(dir)) = (tools(), std::env::var("MEDIAOPTI_TEST_IMAGES")) else { return };
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let src = entry.path();
            let Some(kind) = kind_of(&src).filter(|k| *k != Kind::Gif) else { continue };
            let work = std::env::temp_dir().join(format!("mo-real-{}-{}", std::process::id(), entry.file_name().to_string_lossy()));
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work).unwrap();
            let c = ctx(t.clone(), true);
            let r = optimize(&c, kind, &src, &work).unwrap();
            let before = size(&src).unwrap();
            match r.best {
                Some(b) => println!("{}: {:.1}% {:?} ssim {:.1}", src.display(), 100.0 * (before - size(&b).unwrap()) as f64 / before as f64,
                    r.steps, ssim_score(&c, &src, &b).unwrap().unwrap_or(0.0)),
                None => println!("{}: no gain", src.display()),
            }
            let _ = std::fs::remove_dir_all(&work);
        }
    }

    /// BMP/TGA -> PNG: 24-bit, opaque 32-bit (alpha dropped), palette, and real alpha all
    /// convert pixel-identically (checked here independently, at 16 bits per channel).
    /// GIF frames must keep their timing: a GIF with the same pixels but other delays fails
    /// verification, also on the path that ignores invisible pixels; a misnamed JPEG is
    /// recognised by content.
    #[test]
    fn gif_timing_is_verified_and_pictures_are_sniffed() {
        let Some(t) = tools() else {
            eprintln!("skipping: image tools not found");
            return;
        };
        let Some(gifsicle) = t.get("gifsicle").map(Path::to_path_buf) else {
            eprintln!("skipping: gifsicle not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-giftime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gif = dir.join("a.gif");
        make(&t.ffmpeg, &["-f", "lavfi", "-i", "testsrc=s=64x48:r=5:d=1", "-vf", "split[a][b];[a]palettegen=reserve_transparent=1[p];[b][p]paletteuse", "-loop", "0"], &gif);
        let c = ctx(t, false);
        let ver = Verifier::new(&c, Kind::Gif, &gif);
        let slow = dir.join("slow.gif");
        let ok = std::process::Command::new(&gifsicle).arg("-d").arg("50").arg(&gif).arg("-o").arg(&slow).status().unwrap().success();
        assert!(ok);
        assert!(!ver.ok(&slow).unwrap(), "changed frame delays must fail");
        let opt = dir.join("opt.gif");
        let ok = std::process::Command::new(&gifsicle).arg("-O3").arg(&gif).arg("-o").arg(&opt).status().unwrap().success();
        assert!(ok);
        assert!(ver.ok(&opt).unwrap(), "gifsicle -O3 keeps pixels and timing");

        let jpg_as_png = dir.join("photo.png");
        make(&c.tools.ffmpeg, &["-f", "lavfi", "-i", "testsrc=s=64x48:d=1", "-frames:v", "1", "-f", "mjpeg"], &jpg_as_png);
        assert_eq!(sniff_picture(&jpg_as_png), Some(Kind::Jpg));
        assert_eq!(sniff_picture(&gif), Some(Kind::Gif));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Motion photos, MPO and Ultra HDR keep data after the picture's end: such files are
    /// left alone (optimizers would drop it). Zero padding is not data.
    #[test]
    fn pictures_with_data_after_the_end_are_kept() {
        let Some(t) = tools() else {
            eprintln!("skipping: image tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-trailing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = ctx(t, false);
        for (name, extra, kept) in [
            ("mpo.jpg", b"\xff\xd8\xff\xe1second picture\xff\xd9".to_vec(), true),
            ("zip.png", b"PK\x03\x04appended archive".to_vec(), true),
            ("anim.gif", b"trailing".to_vec(), true),
            ("padded.png", vec![0u8; 300], false),
        ] {
            let f = dir.join(name);
            let fmt: &[&str] = match &name[name.len() - 3..] {
                "jpg" => &["-q:v", "2"],
                "png" => &["-compression_level", "0"],
                _ => &[],
            };
            let mut a = vec!["-f", "lavfi", "-i", "testsrc=s=160x120:d=1", "-frames:v", "1"];
            a.extend(fmt);
            make(&c.tools.ffmpeg, &a, &f);
            let mut d = std::fs::read(&f).unwrap();
            d.extend(&extra);
            std::fs::write(&f, d).unwrap();
            let kind = kind_of(&f).unwrap();
            assert_eq!(has_data_after_end(kind, &f), kept, "{name}");
            let w = dir.join(format!("w-{name}"));
            std::fs::create_dir_all(&w).unwrap();
            let r = optimize(&c, kind, &f, &w).unwrap();
            if kept {
                assert!(r.best.is_none() && r.note.as_deref().is_some_and(|n| n.contains("after the end")), "{name}: {:?}", r.note);
            } else {
                assert!(r.best.is_some(), "{name}: zero padding must not block optimizing");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn big_decodes_are_not_held_in_memory() {
        let Some(t) = tools() else {
            eprintln!("skipping: image tools not found");
            return;
        };
        // 64 frames of 1000x1000 RGBA = 256 MB, over a 16 MB cap: stopped, reported as failed.
        let args = crate::os_args!["-v", "error", "-f", "lavfi", "-i", "color=s=1000x1000:d=64:r=1", "-pix_fmt", "rgba", "-f", "rawvideo", "-"];
        let ran = tools::run(&t.ffmpeg, args, None, Capture::BytesUpTo(16 << 20), Duration::from_secs(60), &tools::Gate::default()).unwrap();
        assert!(!ran.success && ran.stdout.is_empty());
    }

    /// Picture sizes come from the headers, by content (a JPEG named .png is still a JPEG).
    #[test]
    fn pixel_counts_from_headers() {
        let dir = std::env::temp_dir().join(format!("mo-pixels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(13_481u32.to_be_bytes());
        png.extend(12_136u32.to_be_bytes());
        png.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE1, 0, 4, 0, 0];
        jpg.extend([0xFF, 0xC2, 0, 11, 8]);
        jpg.extend(3000u16.to_be_bytes()); // height
        jpg.extend(4000u16.to_be_bytes()); // width
        jpg.extend([1, 1, 0x11, 0, 0xFF, 0xDA, 0, 2]);
        let mut gif = b"GIF89a".to_vec();
        gif.extend(320u16.to_le_bytes());
        gif.extend(240u16.to_le_bytes());
        gif.extend([0, 0, 0]);
        for (name, data, want) in [("a.png", png, 13_481 * 12_136), ("a.png.jpg", jpg.clone(), 12_000_000), ("jpeg.png", jpg, 12_000_000), ("a.gif", gif, 76_800)] {
            std::fs::write(dir.join(name), data).unwrap();
            assert_eq!(pixel_count(&dir.join(name)), Some(want), "{name}");
        }
        std::fs::write(dir.join("a.txt"), b"not a picture").unwrap();
        assert_eq!(pixel_count(&dir.join("a.txt")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bmp_and_tga_become_identical_pngs() {
        let Some(t) = tools() else {
            eprintln!("skipping: image tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-topng-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ff = t.ffmpeg.clone();
        let src = "mandelbrot=s=320x240,trim=end_frame=1";
        let cases: [(&str, &[&str], &str); 4] = [
            ("rgb.bmp", &["-pix_fmt", "bgr24"], "rgb48le"),
            ("opaque.bmp", &["-pix_fmt", "bgra"], "rgb48le"),
            ("pal.bmp", &["-vf", "split[a][b];[a]palettegen[p];[b][p]paletteuse", "-pix_fmt", "pal8"], "rgb48le"),
            ("alpha.tga", &["-vf", "format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='X'", "-pix_fmt", "bgra"], "rgba64le"),
        ];
        for (name, args, fmt) in cases {
            let file = dir.join(name);
            let mut a = vec!["-f", "lavfi", "-i", src, "-frames:v", "1"];
            a.extend_from_slice(args);
            make(&ff, &a, &file);
            let work = dir.join(format!("w-{name}"));
            std::fs::create_dir_all(&work).unwrap();
            let kind = kind_of(&file).unwrap();
            // Strict alpha: by default the PNG chain may recolour fully transparent pixels
            // (invisible), which the exact comparison below would flag.
            let c = Ctx { strict_alpha: true, ..ctx(t.clone(), false) };
            let r = optimize(&c, kind, &file, &work).unwrap();
            assert_eq!(r.new_ext, Some("png"), "{name}: {:?}", r.note);
            let best = r.best.unwrap();
            let raw = |p: &Path| {
                std::process::Command::new(&ff).args(["-v", "error", "-i"]).arg(p).args(["-pix_fmt", fmt, "-f", "rawvideo", "-"]).output().unwrap().stdout
            };
            assert_eq!(raw(&file), raw(&best), "{name}: pixels changed");
            println!("{name}: {} -> {} {:?}", size(&file).unwrap(), size(&best).unwrap(), r.steps);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
