//! Lossless verification: a candidate must decode to the same pixels as the reference.
//!
//! - JPEG: SHA-256 of djpeg's decoded output.
//! - PNG/GIF: ffmpeg `framemd5` at the image's native bit depth (also checks frame timing
//!   of GIF/APNG). Converting 8-bit palette images to 16-bit in ffmpeg is not exact, so the
//!   native depth matters.
//! - Optimizers like pingo/beeim may recolour pixels that are fully transparent (invisible);
//!   unless strict alpha is asked for, such differences are accepted.

use super::png::{self, PngInfo};
use super::tools::{self, Capture, RunError};
use super::{jpeg, Ctx, Kind};
use sha2::{Digest, Sha256};
use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Masked compares hold two decoded rasters in memory: one at a time, each at most this big
/// (decoding stops beyond it, and only pixel-identical results are then accepted).
static MASK_LOCK: Mutex<()> = Mutex::new(());
const MASK_LIMIT: usize = 256 * 1024 * 1024;

pub struct Verifier<'a> {
    ctx: &'a Ctx,
    kind: Kind,
    reference: PathBuf,
    pub png: Option<PngInfo>,
    pub segs: Option<jpeg::Segments>,
    signature: OnceCell<Option<Signature>>,
    /// Fast streaming Rust PNG signature (avoids spawning `ffmpeg.exe` on non-interlaced static PNGs).
    png_sig: OnceCell<Option<[u8; 32]>>,
}

/// What a decode must match: the pixels (and frame timing) as one hash, and the frame
/// timing alone (for compares that ignore invisible pixels).
#[derive(Clone, PartialEq, Eq)]
struct Signature {
    all: Vec<u8>,
    timing: Vec<u8>,
}

impl<'a> Verifier<'a> {
    pub fn new(ctx: &'a Ctx, kind: Kind, reference: &Path) -> Self {
        Self {
            ctx,
            kind,
            reference: reference.to_path_buf(),
            png: if kind == Kind::Png { png::info(reference) } else { None },
            segs: if kind == Kind::Jpg { jpeg::segments(reference) } else { None },
            signature: OnceCell::new(),
            png_sig: OnceCell::new(),
        }
    }

    fn is_16bit_png(&self) -> bool {
        self.png.as_ref().is_some_and(|p| p.bit_depth == 16)
    }

    fn pix_fmt(&self) -> &'static str {
        if self.is_16bit_png() { "rgba64le" } else { "rgba" }
    }

    fn ffmpeg_decode(&self, path: &Path, format: &str, capture: Capture) -> Result<Option<tools::Ran>, RunError> {
        let mut args = crate::os_args!["-v", "error"];
        if self.png.as_ref().is_some_and(PngInfo::is_apng) {
            args.extend(crate::os_args!["-f", "apng"]);
        }
        // Every frame once, stamped in the file's own time base: by default ffmpeg renumbers
        // frames 0, 1, 2... and a GIF with other frame delays would look the same.
        args.extend(crate::os_args!["-i", path, "-map", "0:v:0", "-fps_mode", "passthrough", "-enc_time_base", "demux"]);
        args.extend(crate::os_args!["-pix_fmt", self.pix_fmt(), "-f", format, "-"]);
        let ran = tools::run(&self.ctx.tools.ffmpeg, args, None, capture, self.ctx.timeout, &self.ctx.gate)?;
        Ok(ran.success.then_some(ran))
    }

    fn signature_of(&self, path: &Path) -> Result<Option<Signature>, RunError> {
        if self.kind == Kind::Jpg {
            if let Some(djpeg) = self.ctx.tools.get("djpeg") {
                let ran = tools::run(djpeg, crate::os_args![path], None, Capture::Hash, self.ctx.timeout, &self.ctx.gate)?;
                if ran.success {
                    let mut all = b"djpeg:".to_vec();
                    all.extend(ran.hash.unwrap_or_default());
                    return Ok(Some(Signature { all, timing: Vec::new() }));
                }
            }
        }
        let Some(ran) = self.ffmpeg_decode(path, "framemd5", Capture::Bytes)? else {
            return Ok(None);
        };
        let text = String::from_utf8_lossy(&ran.stdout);
        let (mut all, mut timing) = (Sha256::new(), Sha256::new());
        // Lines: stream, dts, pts, duration, size, md5.
        for line in text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
            all.update(line.as_bytes());
            all.update(b"\n");
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            timing.update(fields.get(..4).unwrap_or(&fields[..]).join(",").as_bytes());
            timing.update(b"\n");
        }
        Ok(Some(Signature { all: all.finalize().to_vec(), timing: timing.finalize().to_vec() }))
    }

    fn masked_equal(&self, path: &Path) -> Result<bool, RunError> {
        let _guard = MASK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (Some(a), Some(b)) = (
            self.ffmpeg_decode(&self.reference, "rawvideo", Capture::BytesUpTo(MASK_LIMIT))?,
            self.ffmpeg_decode(path, "rawvideo", Capture::BytesUpTo(MASK_LIMIT))?,
        ) else {
            return Ok(false);
        };
        Ok(masked_rgba_equal(&a.stdout, &b.stdout, if self.pix_fmt() == "rgba64le" { 2 } else { 1 }))
    }

    pub fn same_pixels(&self, path: &Path) -> Result<bool, RunError> {
        // Fast streaming Rust PNG path for static non-interlaced PNGs: uses ~64 KiB of RAM
        // (1 scanline at a time) and zero `ffmpeg.exe` spawns, including row-by-row masked
        // alpha comparison without `MASK_LOCK` or 512 MB raw buffers. Falls back cleanly to
        // ffmpeg for APNG, interlaced PNGs, or any decoding edge case.
        if self.kind == Kind::Png && !self.png.as_ref().is_some_and(PngInfo::is_apng) {
            let target_16 = self.is_16bit_png();
            let ref_sig = *self.png_sig.get_or_init(|| png_stream_signature(&self.reference, target_16));
            if let (Some(r), Some(c)) = (ref_sig, png_stream_signature(path, target_16)) {
                if r == c {
                    return Ok(true);
                }
                let alpha = self.png.as_ref().is_some_and(PngInfo::has_alpha);
                if alpha && !self.ctx.strict_alpha {
                    if let Some(eq) = png_masked_equal(&self.reference, path, target_16) {
                        return Ok(eq);
                    }
                } else {
                    return Ok(false);
                }
            }
        }
        let reference = if let Some(s) = self.signature.get() {
            s.clone()
        } else {
            let s = self.signature_of(&self.reference)?;
            let _ = self.signature.set(s.clone());
            s
        };
        let Some(reference) = reference else { return Ok(false) };
        let Some(candidate) = self.signature_of(path)? else { return Ok(false) };
        if candidate == reference {
            return Ok(true);
        }
        let alpha = self.kind == Kind::Gif || self.png.as_ref().is_some_and(PngInfo::has_alpha);
        // Same frames at the same times; only the colour of invisible pixels may differ.
        if alpha && !self.ctx.strict_alpha && candidate.timing == reference.timing {
            return self.masked_equal(path);
        }
        Ok(false)
    }

    pub fn meta_ok(&self, path: &Path) -> bool {
        match self.kind {
            Kind::Png => match (&self.png, png::info(path)) {
                (Some(o), Some(n)) => png::meta_ok(o, &n, self.ctx.strip),
                _ => false,
            },
            Kind::Jpg => match (&self.segs, jpeg::segments(path)) {
                (Some(o), Some(n)) => jpeg::meta_ok(o, &n, self.ctx.strip),
                _ => false,
            },
            Kind::Gif => true,
            _ => false, // audio has its own checks
        }
    }

    /// Lossless check: metadata kept and identical pixels.
    pub fn ok(&self, path: &Path) -> Result<bool, RunError> {
        Ok(self.meta_ok(path) && self.same_pixels(path)?)
    }
}

struct PngStream {
    reader: ::png::Reader<std::io::BufReader<std::fs::File>>,
    width: u32,
    height: u32,
    color: ::png::ColorType,
    depth: ::png::BitDepth,
}

/// Opens a non-interlaced static PNG for row-by-row streaming decoding (`Transformations::EXPAND`
/// expands palette, <8-bit grayscale, and `tRNS` into 8- or 16-bit channels). Returns `None`
/// for interlaced PNGs, APNGs, or any error (caller falls back to ffmpeg).
fn open_png_stream(path: &Path, target_16bit: bool) -> Option<PngStream> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = ::png::Decoder::new(std::io::BufReader::with_capacity(64 * 1024, file));
    decoder.set_transformations(::png::Transformations::EXPAND);
    let reader = decoder.read_info().ok()?;
    let info = reader.info();
    if info.interlaced || info.animation_control.is_some() {
        return None;
    }
    let (width, height) = (info.width, info.height);
    let (color, depth) = reader.output_color_type();
    if !matches!(depth, ::png::BitDepth::Eight | ::png::BitDepth::Sixteen) {
        return None;
    }
    if !target_16bit && depth == ::png::BitDepth::Sixteen {
        return None;
    }
    Some(PngStream { reader, width, height, color, depth })
}

/// Normalizes one decoded PNG scanline into `out` as native-depth RGBA (`rgba8` when
/// `!target_16bit`, `rgba16be` when `target_16bit`).
fn normalize_png_row(src: &[u8], color: ::png::ColorType, depth: ::png::BitDepth, target_16bit: bool, out: &mut Vec<u8>) -> Option<()> {
    out.clear();
    match (depth, target_16bit) {
        (::png::BitDepth::Eight, false) => match color {
            ::png::ColorType::Rgba => out.extend_from_slice(src),
            ::png::ColorType::Rgb => {
                for &[r, g, b] in src.as_chunks::<3>().0 {
                    out.extend_from_slice(&[r, g, b, 255]);
                }
            }
            ::png::ColorType::GrayscaleAlpha => {
                for &[g, a] in src.as_chunks::<2>().0 {
                    out.extend_from_slice(&[g, g, g, a]);
                }
            }
            ::png::ColorType::Grayscale => {
                for &g in src {
                    out.extend_from_slice(&[g, g, g, 255]);
                }
            }
            _ => return None,
        },
        (::png::BitDepth::Sixteen, true) => match color {
            ::png::ColorType::Rgba => out.extend_from_slice(src),
            ::png::ColorType::Rgb => {
                for px in src.as_chunks::<6>().0 {
                    out.extend_from_slice(px);
                    out.extend_from_slice(&[0xFF, 0xFF]);
                }
            }
            ::png::ColorType::GrayscaleAlpha => {
                for &[g0, g1, a0, a1] in src.as_chunks::<4>().0 {
                    out.extend_from_slice(&[g0, g1, g0, g1, g0, g1, a0, a1]);
                }
            }
            ::png::ColorType::Grayscale => {
                for &[g0, g1] in src.as_chunks::<2>().0 {
                    out.extend_from_slice(&[g0, g1, g0, g1, g0, g1, 0xFF, 0xFF]);
                }
            }
            _ => return None,
        },
        (::png::BitDepth::Eight, true) => {
            // Exact PNG 8->16 bit scaling (`v * 257`, i.e. `[v, v]`) so a lossless 16->8
            // reduction of pixels whose high and low bytes match verifies as identical.
            match color {
                ::png::ColorType::Rgba => {
                    for &v in src {
                        out.extend_from_slice(&[v, v]);
                    }
                }
                ::png::ColorType::Rgb => {
                    for &[r, g, b] in src.as_chunks::<3>().0 {
                        out.extend_from_slice(&[r, r, g, g, b, b, 0xFF, 0xFF]);
                    }
                }
                ::png::ColorType::GrayscaleAlpha => {
                    for &[g, a] in src.as_chunks::<2>().0 {
                        out.extend_from_slice(&[g, g, g, g, g, g, a, a]);
                    }
                }
                ::png::ColorType::Grayscale => {
                    for &g in src {
                        out.extend_from_slice(&[g, g, g, g, g, g, 0xFF, 0xFF]);
                    }
                }
                _ => return None,
            }
        }
        _ => return None,
    }
    Some(())
}

/// SHA-256 of `(width, height, normalized RGBA scanlines)` streamed one row at a time.
fn png_stream_signature(path: &Path, target_16bit: bool) -> Option<[u8; 32]> {
    let mut s = open_png_stream(path, target_16bit)?;
    let bps = if target_16bit { 2 } else { 1 };
    let mut row_buf = Vec::with_capacity(s.width as usize * 4 * bps);
    let mut hasher = Sha256::new();
    hasher.update(s.width.to_le_bytes());
    hasher.update(s.height.to_le_bytes());
    let mut rows = 0u32;
    while let Some(row) = s.reader.next_row().ok()? {
        normalize_png_row(row.data(), s.color, s.depth, target_16bit, &mut row_buf)?;
        hasher.update(&row_buf);
        rows += 1;
    }
    (rows == s.height).then(|| hasher.finalize().into())
}

/// Row-by-row masked RGBA comparison of two static non-interlaced PNGs (ignores RGB of
/// fully transparent pixels). Holds only two single scanlines in memory (~64 KiB).
fn png_masked_equal(a: &Path, b: &Path, target_16bit: bool) -> Option<bool> {
    let mut sa = open_png_stream(a, target_16bit)?;
    let mut sb = open_png_stream(b, target_16bit)?;
    if sa.width != sb.width || sa.height != sb.height {
        return Some(false);
    }
    let bps = if target_16bit { 2 } else { 1 };
    let (mut ba, mut bb) = (Vec::with_capacity(sa.width as usize * 4 * bps), Vec::with_capacity(sb.width as usize * 4 * bps));
    let mut rows = 0u32;
    loop {
        match (sa.reader.next_row().ok()?, sb.reader.next_row().ok()?) {
            (Some(xa), Some(xb)) => {
                normalize_png_row(xa.data(), sa.color, sa.depth, target_16bit, &mut ba)?;
                normalize_png_row(xb.data(), sb.color, sb.depth, target_16bit, &mut bb)?;
                if !masked_rgba_equal(&ba, &bb, bps) {
                    return Some(false);
                }
                rows += 1;
            }
            (None, None) => return Some(rows == sa.height),
            _ => return Some(false),
        }
    }
}

/// RGBA rasters equal except for the colour of fully transparent pixels.
/// `bytes_per_sample`: 1 for rgba, 2 for rgba64le.
pub fn masked_rgba_equal(a: &[u8], b: &[u8], bytes_per_sample: usize) -> bool {
    if a.len() != b.len() || a.len() > MASK_LIMIT {
        return false;
    }
    let px = 4 * bytes_per_sample;
    a.chunks_exact(px).zip(b.chunks_exact(px)).all(|(pa, pb)| {
        let alpha = 3 * bytes_per_sample;
        if pa[alpha..] != pb[alpha..] {
            return false;
        }
        pa[alpha..].iter().all(|&x| x == 0) || pa[..alpha] == pb[..alpha]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_compare_ignores_only_invisible_colour() {
        let a = [10, 20, 30, 0, 1, 2, 3, 255];
        assert!(masked_rgba_equal(&a, &[99, 99, 99, 0, 1, 2, 3, 255], 1), "invisible pixel recoloured");
        assert!(!masked_rgba_equal(&a, &[10, 20, 30, 0, 1, 2, 4, 255], 1), "visible pixel changed");
        assert!(!masked_rgba_equal(&a, &[10, 20, 30, 1, 1, 2, 3, 255], 1), "alpha changed");
        assert!(!masked_rgba_equal(&a, &a[..4], 1), "different sizes");
    }

    #[test]
    fn streaming_png_verifier_matches_formats_and_masked_alpha() {
        let dir = std::env::temp_dir().join(format!("mo-png-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write_png = |name: &str, ct: ::png::ColorType, depth: ::png::BitDepth, data: &[u8]| {
            let p = dir.join(name);
            let mut enc = ::png::Encoder::new(std::fs::File::create(&p).unwrap(), 2, 1);
            enc.set_color(ct);
            enc.set_depth(depth);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(data).unwrap();
            p
        };
        // Grayscale [42, 200] equals RGB [(42,42,42), (200,200,200)] equals opaque RGBA.
        let gray = write_png("gray.png", ::png::ColorType::Grayscale, ::png::BitDepth::Eight, &[42, 200]);
        let rgb = write_png("rgb.png", ::png::ColorType::Rgb, ::png::BitDepth::Eight, &[42, 42, 42, 200, 200, 200]);
        let rgba = write_png("rgba.png", ::png::ColorType::Rgba, ::png::BitDepth::Eight, &[42, 42, 42, 255, 200, 200, 200, 255]);
        assert_eq!(png_stream_signature(&gray, false), png_stream_signature(&rgb, false));
        assert_eq!(png_stream_signature(&rgb, false), png_stream_signature(&rgba, false));

        // 16-bit with matching high/low bytes equals 8-bit when verified at 16-bit depth.
        let g16 = write_png("g16.png", ::png::ColorType::Grayscale, ::png::BitDepth::Sixteen, &[42, 42, 200, 200]);
        assert_eq!(png_stream_signature(&g16, true), png_stream_signature(&rgb, true));

        // Masked alpha: invisible pixel recoloured passes; visible pixel changed fails.
        let a1 = write_png("a1.png", ::png::ColorType::Rgba, ::png::BitDepth::Eight, &[10, 20, 30, 0, 1, 2, 3, 255]);
        let a2 = write_png("a2.png", ::png::ColorType::Rgba, ::png::BitDepth::Eight, &[99, 88, 77, 0, 1, 2, 3, 255]);
        let a3 = write_png("a3.png", ::png::ColorType::Rgba, ::png::BitDepth::Eight, &[10, 20, 30, 0, 1, 2, 4, 255]);
        assert_ne!(png_stream_signature(&a1, false), png_stream_signature(&a2, false));
        assert_eq!(png_masked_equal(&a1, &a2, false), Some(true));
        assert_eq!(png_masked_equal(&a1, &a3, false), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
