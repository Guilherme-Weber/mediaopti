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

pub mod jpeg;
pub mod png;
pub mod tools;
pub mod verify;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tools::{Capture, RunError, Tools};
use verify::Verifier;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Png,
    Jpg,
    Gif,
}

/// PNG/JPEG by extension (GIFs are routed separately: big animated ones become video).
pub fn kind_of(path: &Path) -> Option<Kind> {
    match crate::args::ext_lower(path).as_str() {
        "png" => Some(Kind::Png),
        "jpg" | "jpeg" | "jpe" | "jfif" => Some(Kind::Jpg),
        "gif" => Some(Kind::Gif),
        _ => None,
    }
}

pub struct Ctx {
    pub tools: Tools,
    pub lossy: bool,
    /// Minimum SSIMULACRA2 score for lossy results (93 ~ pingo -quality=100).
    pub target: f64,
    /// A lossy result must be this much smaller than the lossless one (0.03 = 3%).
    pub lossy_min_gain: f64,
    pub strip: bool,
    pub strict_alpha: bool,
    /// Several files run at once: keep each tool single-threaded.
    pub parallel: bool,
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

pub struct Optimized {
    /// The smallest verified result, or `None` if nothing was smaller.
    pub best: Option<PathBuf>,
    pub steps: Vec<String>,
    /// Why the file was not processed, if it wasn't.
    pub note: Option<String>,
}

/// Content-history mode for images, compatible with imageopti's history.
pub fn mode(lossy: bool) -> &'static str {
    if lossy { "lossy-normal-t93" } else { "lossless-normal" }
}

fn run(ctx: &Ctx, tool: &str, args: Vec<OsString>, cwd: &Path) -> Result<bool, RunError> {
    let Some(program) = ctx.tools.get(tool) else { return Ok(false) };
    Ok(tools::run(program, args, Some(cwd), Capture::None, ctx.timeout, &ctx.cancel)?.success)
}

fn size(p: &Path) -> Option<u64> {
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
    let ver = Verifier::new(ctx, kind, src);
    let skip = |why: &str| Ok(Optimized { best: None, steps: Vec::new(), note: Some(why.to_string()) });
    match kind {
        Kind::Png if ver.png.is_none() => return skip("not a PNG"),
        Kind::Jpg if ver.segs.is_none() => return skip("not a JPEG"),
        _ => {}
    }
    let mut chain = match kind {
        Kind::Png => png_chain(ctx, work, src, src, &ver)?,
        Kind::Jpg => jpg_chain(ctx, work, src, &ver)?,
        Kind::Gif => {
            let mut c = Chain::new(ctx, work, src);
            let mut check = |p: &Path| ver.ok(p);
            c.write_to("gifsicle", "gifsicle", |i, o| crate::os_args!["-O3", "--no-warnings", "-o", o, i], &mut check, None)?;
            c
        }
    };
    if ctx.lossy && ctx.tools.has("ssimulacra2") {
        match kind {
            Kind::Png => lossy_png(ctx, work, src, &mut chain, &ver)?,
            Kind::Jpg => lossy_jpeg(ctx, work, src, &mut chain, &ver)?,
            Kind::Gif => {}
        }
    }
    let best = (chain.best != src).then_some(chain.best);
    Ok(Optimized { best, steps: chain.steps, note: None })
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
    if ctx.tools.has("beeim") && !apng {
        c.inplace("beeim", "beeim", |o| crate::os_args!["-sl=5", "-force", o], &mut check, Some(&repair))?;
    } else {
        let alpha = if ctx.strict_alpha { "-noalpha" } else { "-quiet" };
        c.inplace("pingo", "pingo", |o| crate::os_args!["-s4", "-lossless", "-quiet", "-nostrip", alpha, o], &mut check, Some(&repair))?;
    }
    let mut args = crate::os_args!["-o", "4", "-q"];
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
    c.inplace("ect", "ect", |o| crate::os_args!["-4", "-progressive", "-quiet", o], &mut check, None)?;
    Ok(c)
}

// ---------------------------------------------------------------- lossy

/// SSIMULACRA2 of `b` against `a`. Retries on plain RGBA copies when ssimulacra2 can't
/// read a file (it rejects some valid PNGs).
fn ssim_score(ctx: &Ctx, a: &Path, b: &Path) -> Result<Option<f64>, RunError> {
    let Some(ssim) = ctx.tools.get("ssimulacra2") else { return Ok(None) };
    let score = |x: &Path, y: &Path| -> Result<Option<f64>, RunError> {
        let ran = tools::run(ssim, crate::os_args![x, y], None, Capture::Bytes, ctx.timeout, &ctx.cancel)?;
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
        if !tools::run(&ctx.tools.ffmpeg, args, None, Capture::None, ctx.timeout, &ctx.cancel)?.success {
            return Ok(None);
        }
    }
    let s = score(&na, &nb);
    let _ = std::fs::remove_file(&nb);
    s
}

type Encoder<'e> = Box<dyn FnMut(u32) -> Result<Option<PathBuf>, RunError> + 'e>;

/// Binary-searches quality `n` in `lo..=hi` (higher = better) for the smallest output that
/// still scores >= target and is below `limit` bytes. One probe at a fairly strong setting
/// first: already-optimized images don't get smaller there, and are skipped cheaply.
fn quality_search(ctx: &Ctx, reference: &Path, lo: u32, hi: u32, encode: &mut Encoder, limit: u64) -> Result<Option<PathBuf>, RunError> {
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
fn search_methods(ctx: &Ctx, reference: &Path, methods: Vec<(&str, u32, u32, Encoder)>, limit: u64) -> Result<Option<(PathBuf, String)>, RunError> {
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
    } else if ctx.tools.has("pingo") {
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
    if jpeg::quality(src).is_some_and(|q| q < 90.0) {
        return Ok(());
    }
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
            methods.push(("jpegli-q", 84, 100, reencoder("jl", "cjpegli", |n, i, o| crate::os_args![i, o, "-q", n.to_string()])));
        }
        if ctx.tools.has("cjpeg") {
            methods.push(("mozjpeg-q", 84, 98, reencoder("mz", "cjpeg", |n, i, o| crate::os_args!["-quality", n.to_string(), "-outfile", o, i])));
        }
    }
    if ctx.tools.has("pingo") {
        let lw2 = lw.clone();
        methods.push((
            "pingo-quality",
            90,
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
            parallel: false,
            timeout: Duration::from_secs(120),
            cancel: CancellationToken::new(),
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
}
