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

/// Masked compares hold two decoded rasters in memory: one at a time.
static MASK_LOCK: Mutex<()> = Mutex::new(());
const MASK_LIMIT: usize = 600 * 1024 * 1024;

pub struct Verifier<'a> {
    ctx: &'a Ctx,
    kind: Kind,
    reference: PathBuf,
    pub png: Option<PngInfo>,
    pub segs: Option<jpeg::Segments>,
    signature: OnceCell<Option<Vec<u8>>>,
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
        }
    }

    fn pix_fmt(&self) -> &'static str {
        if self.png.as_ref().is_some_and(|p| p.bit_depth == 16) { "rgba64le" } else { "rgba" }
    }

    fn ffmpeg_decode(&self, path: &Path, format: &str, capture: Capture) -> Result<Option<tools::Ran>, RunError> {
        let mut args = crate::os_args!["-v", "error"];
        if self.png.as_ref().is_some_and(PngInfo::is_apng) {
            args.extend(crate::os_args!["-f", "apng"]);
        }
        args.extend(crate::os_args!["-i", path, "-map", "0:v:0", "-pix_fmt", self.pix_fmt(), "-f", format, "-"]);
        let ran = tools::run(&self.ctx.tools.ffmpeg, args, None, capture, self.ctx.timeout, &self.ctx.cancel)?;
        Ok(ran.success.then_some(ran))
    }

    fn signature_of(&self, path: &Path) -> Result<Option<Vec<u8>>, RunError> {
        if self.kind == Kind::Jpg {
            if let Some(djpeg) = self.ctx.tools.get("djpeg") {
                let ran = tools::run(djpeg, crate::os_args![path], None, Capture::Hash, self.ctx.timeout, &self.ctx.cancel)?;
                if ran.success {
                    let mut sig = b"djpeg:".to_vec();
                    sig.extend(ran.hash.unwrap_or_default());
                    return Ok(Some(sig));
                }
            }
        }
        let Some(ran) = self.ffmpeg_decode(path, "framemd5", Capture::Bytes)? else {
            return Ok(None);
        };
        let text = String::from_utf8_lossy(&ran.stdout);
        let mut h = Sha256::new();
        for line in text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
            h.update(line.as_bytes());
            h.update(b"\n");
        }
        Ok(Some(h.finalize().to_vec()))
    }

    fn masked_equal(&self, path: &Path) -> Result<bool, RunError> {
        let _guard = MASK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (Some(a), Some(b)) = (
            self.ffmpeg_decode(&self.reference, "rawvideo", Capture::Bytes)?,
            self.ffmpeg_decode(path, "rawvideo", Capture::Bytes)?,
        ) else {
            return Ok(false);
        };
        Ok(masked_rgba_equal(&a.stdout, &b.stdout, if self.pix_fmt() == "rgba64le" { 2 } else { 1 }))
    }

    pub fn same_pixels(&self, path: &Path) -> Result<bool, RunError> {
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
        if alpha && !self.ctx.strict_alpha {
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
            Kind::Flac | Kind::Mp3 => false, // audio has its own checks
        }
    }

    /// Lossless check: metadata kept and identical pixels.
    pub fn ok(&self, path: &Path) -> Result<bool, RunError> {
        Ok(self.meta_ok(path) && self.same_pixels(path)?)
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
}
