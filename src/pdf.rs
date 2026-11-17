//! PDF, ported from imageopti's `handle_pdf`, `pdf_optimize_images` and `pdf_pages_look_same`.
//!
//! - qpdf rewrites the file: object streams, every Flate stream recompressed at level 9. Text,
//!   fonts and drawings are only recompressed, never changed.
//! - JPEG images are re-optimized losslessly through qpdf's JSON stream export/import
//!   (jpegtran, sequential; djpeg must decode the same pixels).
//! - Optional lossy pass (images visually lossless and "PDF: visually lossless images"): JPEG
//!   and plain 8-bit RGB/grey images may be re-encoded as sequential JPEG at SSIMULACRA2 >= 93,
//!   and then **every page** of both PDFs is rendered (mutool, 96 dpi) and must score >= 93.
//!   Soft masks, colour-key masks, /Decode arrays, indexed colour, image masks and JPEG 2000
//!   are never touched.
//! - Every result must pass `qpdf --check` (as cleanly as the original) with the same page
//!   count. Encrypted and digitally signed PDFs are left alone.

use crate::image::tools::{self, Capture, RunError};
use crate::image::{self, jpeg, Ctx, Encoder, Optimized};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The lossy pass renders every page twice: PDFs longer than this stay lossless.
const MAX_LOSSY_PAGES: u64 = 600;
/// Image streams smaller than this aren't worth optimizing.
const MIN_IMAGE: u64 = 2048;
/// JPEGs smaller than this, or saved below this quality, are not re-encoded lossily.
const MIN_LOSSY_JPEG: u64 = 8192;
const MIN_LOSSY_QUALITY: f64 = 85.0;

fn qpdf(ctx: &Ctx, args: Vec<OsString>, capture: Capture) -> Result<tools::Ran, RunError> {
    let program = ctx.tools.get("qpdf").ok_or_else(|| RunError::Unavailable("qpdf is missing".into()))?;
    tools::run(program, args, None, capture, ctx.timeout, &ctx.gate)
}

fn size(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(u64::MAX, |m| m.len())
}

/// Whether the file contains `needle` (streamed).
fn file_contains(path: &Path, needle: &[u8]) -> std::io::Result<bool> {
    let mut f = std::fs::File::open(path)?;
    let (mut buf, mut tail) = (vec![0u8; 1 << 22], Vec::new());
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        let mut window = std::mem::take(&mut tail);
        window.extend_from_slice(&buf[..n]);
        if window.windows(needle.len()).any(|w| w == needle) {
            return Ok(true);
        }
        tail = window[window.len().saturating_sub(needle.len())..].to_vec();
    }
}

/// What every candidate must keep: the page count, and qpdf's verdict on the structure.
struct Reference {
    pages: String,
    check_code: Option<i32>,
}

impl Reference {
    fn read(ctx: &Ctx, p: &Path) -> Result<Option<Self>, RunError> {
        let Some(pages) = pages(ctx, p)? else { return Ok(None) };
        let check_code = qpdf(ctx, crate::os_args!["--check", p], Capture::None)?.code;
        Ok(Some(Self { pages, check_code }))
    }

    /// qpdf --check passes (with warnings only if the original had some) and the page count
    /// is the same.
    fn accepts(&self, ctx: &Ctx, p: &Path) -> Result<bool, RunError> {
        let code = qpdf(ctx, crate::os_args!["--check", p], Capture::None)?.code;
        let structure_ok = matches!(code, Some(0 | 3)) && (self.check_code != Some(0) || code == Some(0));
        Ok(structure_ok && pages(ctx, p)?.as_deref() == Some(self.pages.as_str()))
    }
}

fn pages(ctx: &Ctx, p: &Path) -> Result<Option<String>, RunError> {
    let ran = qpdf(ctx, crate::os_args!["--show-npages", p], Capture::Bytes)?;
    Ok(matches!(ran.code, Some(0 | 3)).then(|| String::from_utf8_lossy(&ran.stdout).trim().to_string()).filter(|s| !s.is_empty()))
}

pub fn optimize(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, RunError> {
    if !ctx.tools.has("qpdf") {
        return Ok(Optimized::skipped("qpdf is missing"));
    }
    if qpdf(ctx, crate::os_args!["--is-encrypted", src], Capture::None)?.code == Some(0) {
        return Ok(Optimized::skipped("encrypted: skipped"));
    }
    if file_contains(src, b"/ByteRange").map_err(|e| RunError::Failed(e.to_string()))? {
        return Ok(Optimized::skipped("digitally signed: skipped"));
    }
    ctx.phase("reading", &[]);
    let Some(reference) = Reference::read(ctx, src)? else { return Ok(Optimized::skipped("qpdf cannot read it")) };
    let rewrite = crate::os_args![
        "--object-streams=generate", "--compress-streams=y", "--recompress-flate", "--compression-level=9",
        "--decode-level=generalized", "--warning-exit-0"
    ];
    let src_size = size(src);
    let can_render = ctx.tools.has("mutool") && ctx.tools.has("ssimulacra2") && reference.pages.parse::<u64>().is_ok_and(|n| n <= MAX_LOSSY_PAGES);
    let lossy_ok = ctx.lossy && ctx.lossy_docs && can_render;
    // A damaged original (qpdf --check reports problems) is rewritten from qpdf's repair,
    // which passes qpdf's own check whatever the repair lost: every page must also render
    // the same, or the file is left as it is.
    let damaged = reference.check_code != Some(0);
    if damaged && !can_render {
        return Ok(Optimized::skipped("damaged (qpdf reports problems) and its pages can't be checked: left as it is"));
    }

    if ctx.tools.has("jpegtran") && ctx.tools.has("djpeg") {
        for lossy in if lossy_ok { vec![true, false] } else { vec![false] } {
            let dir = work.join(if lossy { "lossy" } else { "lossless" });
            let Some(update) = optimize_images(ctx, src, &dir, lossy)? else { continue };
            let out = dir.join("out.pdf");
            let mut args = rewrite.clone();
            let mut u = OsString::from("--update-from-json=");
            u.push(&update.json);
            args.extend([u, src.into(), out.clone().into()]);
            ctx.phase("rewriting", &[]);
            let ran = qpdf(ctx, args, Capture::None)?;
            let ok = ran.success
                && size(&out) < src_size
                && reference.accepts(ctx, &out)?
                && ((update.lossy == 0 && !damaged) || {
                    ctx.phase("checking pages", &[]);
                    pages_look_same(ctx, src, &out, &dir.join("render"))?
                });
            if ok {
                let mut label = if damaged { "qpdf (damaged original, pages checked)" } else { "qpdf" }.to_string();
                if update.lossless > 0 {
                    label += &format!("+{} jpeg", update.lossless);
                }
                if update.lossy > 0 {
                    label += &format!("+{} img lossy=ssim{}", update.lossy, ctx.target);
                }
                return Ok(Optimized::found(Some(out), vec![label]));
            }
        }
    }
    // A plain rewrite (or the image update did not verify).
    let out = work.join("rewrite.pdf");
    let mut args = rewrite;
    args.extend([src.into(), out.clone().into()]);
    ctx.phase("rewriting", &[]);
    let ok = qpdf(ctx, args, Capture::None)?.success
        && size(&out) < src_size
        && reference.accepts(ctx, &out)?
        && (!damaged || {
            ctx.phase("checking pages", &[]);
            pages_look_same(ctx, src, &out, &work.join("render"))?
        });
    let label = if damaged { "qpdf (damaged original, pages checked)" } else { "qpdf" };
    Ok(if ok { Optimized::found(Some(out), vec![label.into()]) } else { Optimized::nothing() })
}

/// Renders every page of both PDFs at 96 dpi (screen size); each page of `b` must be
/// identical or score >= target against `a`: the document must read and look the same,
/// not just its images.
fn pages_look_same(ctx: &Ctx, a: &Path, b: &Path, work: &Path) -> Result<bool, RunError> {
    let Some(mutool) = ctx.tools.get("mutool") else { return Ok(false) };
    let mut rendered = Vec::new();
    for (name, p) in [("a", a), ("b", b)] {
        let dir = work.join(name);
        if std::fs::create_dir_all(&dir).is_err() {
            return Ok(false);
        }
        let args = crate::os_args!["draw", "-q", "-r", "96", "-o", dir.join("p%05d.png"), p];
        if !tools::run(mutool, args, None, Capture::None, ctx.timeout * 2, &ctx.gate)?.success {
            return Ok(false);
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).map_or_else(|_| Vec::new(), |d| d.filter_map(|e| e.ok().map(|e| e.path())).collect());
        files.sort();
        rendered.push(files);
    }
    if rendered[0].is_empty() || rendered[0].len() != rendered[1].len() {
        return Ok(false);
    }
    for (x, y) in rendered[0].iter().zip(&rendered[1]) {
        if std::fs::read(x).ok() == std::fs::read(y).ok() {
            continue;
        }
        if !image::ssim_score(ctx, x, y)?.is_some_and(|s| s >= ctx.target) {
            return Ok(false);
        }
    }
    let _ = std::fs::remove_dir_all(work);
    Ok(true)
}

struct Update {
    json: PathBuf,
    lossless: usize,
    lossy: usize,
}

/// A JPEG written as a sequential (baseline) file: progressive JPEGs are refused in PDFs, for
/// maximum reader compatibility.
fn sequential(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|d| jpeg::frame_marker(&d))
        .is_some_and(|m| !matches!(m, 0xC2 | 0xC6 | 0xCA | 0xCE))
}

/// JPEG re-encoders for the quality search, from a decoded `pnm`; ICC profiles in `keep` are
/// carried over.
fn reencoders<'a>(ctx: &'a Ctx, lw: &Path, pnm: &Path, keep: &jpeg::Segments) -> Vec<(&'static str, u32, u32, Encoder<'a>)> {
    let wrap = |tag: &'static str, tool: &'static str, args: fn(u32, &Path, &Path) -> Vec<OsString>| -> Encoder<'a> {
        let (lw, pnm, keep) = (lw.to_path_buf(), pnm.to_path_buf(), keep.clone());
        Box::new(move |n| {
            let out = lw.join(format!("{tag}{n}.jpg"));
            if !image::run(ctx, tool, args(n, &pnm, &out), &lw)? {
                return Ok(None);
            }
            if !keep.is_empty() {
                if let Ok(data) = std::fs::read(&out) {
                    let _ = std::fs::write(&out, jpeg::with_segments(&data, &keep));
                }
            }
            Ok(sequential(&out).then_some(out))
        })
    };
    let mut methods = Vec::new();
    if ctx.tools.has("cjpegli") {
        methods.push(("jpegli-q", 84, 100, wrap("jl", "cjpegli", |n, i, o| crate::os_args![i, o, "-q", n.to_string(), "-p", "0"])));
    }
    if ctx.tools.has("cjpeg") {
        methods.push(("mozjpeg-q", 84, 98, wrap("mz", "cjpeg", |n, i, o| crate::os_args!["-quality", n.to_string(), "-baseline", "-outfile", o, i])));
    }
    methods
}

fn djpeg_hash(ctx: &Ctx, p: &Path) -> Result<Option<[u8; 32]>, RunError> {
    let Some(djpeg) = ctx.tools.get("djpeg") else { return Ok(None) };
    let ran = tools::run(djpeg, crate::os_args![p], None, Capture::Hash, ctx.timeout, &ctx.gate)?;
    Ok(ran.hash.filter(|_| ran.success))
}

/// Exports the PDF's streams with qpdf, optimizes the images, and writes the qpdf JSON
/// update. `None` when no image got smaller.
fn optimize_images(ctx: &Ctx, src: &Path, work: &Path, lossy: bool) -> Result<Option<Update>, RunError> {
    let streams = work.join("streams");
    if std::fs::create_dir_all(&streams).is_err() {
        return Ok(None);
    }
    let js = work.join("pdf.json");
    let mut prefix = OsString::from("--json-stream-prefix=");
    prefix.push(streams.join("s"));
    let decode = if lossy { "--decode-level=generalized" } else { "--decode-level=none" };
    let args = crate::os_args!["--json-output=2", "--json-key=qpdf", "--json-stream-data=file", prefix, decode, src, &js];
    let ran = qpdf(ctx, args, Capture::None)?;
    if !matches!(ran.code, Some(0 | 3)) {
        return Ok(None);
    }
    let Some(doc) = std::fs::read(&js).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return Ok(None) };
    let Some([header, Value::Object(objects)]) = doc.get("qpdf").and_then(Value::as_array).map(Vec::as_slice) else {
        return Ok(None);
    };
    let deref = |v: &Value| -> Value {
        match v.as_str().filter(|s| s.ends_with(" R")) {
            Some(r) => objects
                .get(&format!("obj:{r}"))
                .and_then(|o| o.get("value").or_else(|| o.pointer("/stream/dict")))
                .cloned()
                .unwrap_or(Value::Null),
            None => v.clone(),
        }
    };
    let soft_masks: HashSet<&str> = objects
        .values()
        .filter_map(|o| o.pointer("/stream/dict/~1SMask").and_then(Value::as_str))
        .collect();
    let keep_ratio = 1.0 - ctx.lossy_min_gain;
    let (mut update, mut n_lossless, mut n_lossy) = (Map::new(), 0, 0);
    ctx.phase("pictures", &[]);
    if let Some(p) = &ctx.progress {
        let images = objects.values().filter(|o| o.pointer("/stream/dict/~1Subtype").and_then(Value::as_str) == Some("/Image")).count();
        p.set_total(images);
    }
    for (n, (key, obj)) in objects.iter().enumerate() {
        if let Some(p) = &ctx.progress {
            if obj.pointer("/stream/dict/~1Subtype").and_then(Value::as_str) == Some("/Image") {
                p.step();
            }
        }
        let (Some(dict), Some(file)) = (
            obj.pointer("/stream/dict").and_then(Value::as_object),
            obj.pointer("/stream/datafile").and_then(Value::as_str).map(PathBuf::from),
        ) else {
            continue;
        };
        if dict.get("/Subtype").and_then(Value::as_str) != Some("/Image") || soft_masks.contains(key.trim_start_matches("obj:")) {
            continue;
        }
        let file_size = size(&file);
        if file_size == u64::MAX || file_size < MIN_IMAGE {
            continue;
        }
        let filter = match dict.get("/Filter") {
            Some(Value::Array(a)) if a.len() == 1 => a[0].as_str(),
            Some(f) => f.as_str(),
            None => None,
        };
        let lw = work.join(format!("i{n}"));
        let _ = std::fs::create_dir_all(&lw);
        let mut new_dict = dict.clone();
        let mut best: Option<PathBuf> = None;
        let mut lossy_hit = false;
        if filter == Some("/DCTDecode") {
            // Lossless: Huffman tables optimized, written sequential, same decoded pixels.
            let out = lw.join("lossless.jpg");
            let args = crate::os_args!["-copy", "all", "-revert", "-optimize", "-outfile", &out, &file];
            if image::run(ctx, "jpegtran", args, &lw)? && size(&out) < file_size {
                let want = djpeg_hash(ctx, &file)?;
                if want.is_some() && djpeg_hash(ctx, &out)? == want {
                    best = Some(out);
                }
            }
            let ref_jpg = lw.join("ref.jpg"); // extracted streams have no extension; scorers want one
            let quality_ok = ctx.max.pdf_any_q || jpeg::quality(&file).is_none_or(|q| q >= MIN_LOSSY_QUALITY);
            if lossy && file_size >= MIN_LOSSY_JPEG && quality_ok && std::fs::copy(&file, &ref_jpg).is_ok() {
                let pnm = lw.join("dec.pnm");
                if image::run(ctx, "djpeg", crate::os_args!["-outfile", &pnm, &ref_jpg], &lw)? {
                    let keep: jpeg::Segments = jpeg::segments(&ref_jpg)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|(m, d)| *m == 0xE2 && d.starts_with(b"ICC_PROFILE"))
                        .collect();
                    let limit = (best.as_deref().map_or(file_size, size) as f64 * keep_ratio) as u64;
                    if let Some((cand, _)) = image::search_methods(ctx, &ref_jpg, reencoders(ctx, &lw, &pnm, &keep), limit)? {
                        best = Some(cand);
                        lossy_hit = true;
                    }
                }
            }
        } else if lossy && filter.is_none() {
            // Plain 8-bit RGB/grey pixels (decoded from Flate by qpdf) -> JPEG.
            let num = |k: &str| dict.get(k).map(deref).and_then(|v| v.as_u64());
            let (Some(w), Some(h), Some(8)) = (num("/Width"), num("/Height"), num("/BitsPerComponent")) else { continue };
            let cs = dict.get("/ColorSpace").map_or(Value::Null, deref);
            let comps = match &cs {
                Value::String(s) if s == "/DeviceRGB" => Some(3),
                Value::String(s) if s == "/DeviceGray" => Some(1),
                Value::Array(a) if a.len() == 2 && a[0] == "/ICCBased" => deref(&a[1]).get("/N").and_then(Value::as_u64),
                _ => None,
            };
            let Some(comps) = comps.filter(|c| matches!(c, 1 | 3)) else { continue };
            let masked = ["/ImageMask", "/Decode", "/Mask", "/SMask"].iter().any(|k| dict.contains_key(*k));
            if masked || w * h < 64 * 64 || file_size != w * h * comps {
                continue;
            }
            let Ok(data) = std::fs::read(&file) else { continue };
            let pnm = lw.join("img.pnm");
            let mut f = Vec::with_capacity(data.len() + 32);
            f.extend_from_slice(format!("P{}\n{w} {h}\n255\n", if comps == 3 { 6 } else { 5 }).as_bytes());
            f.extend_from_slice(&data);
            if std::fs::write(&pnm, &f).is_err() {
                continue;
            }
            // Its size in the PDF now: /Length if direct, else what Flate level 9 gives.
            let encoded = dict.get("/Length").map(deref).and_then(|v| v.as_u64()).unwrap_or_else(|| {
                let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
                let _ = z.write_all(&data);
                z.finish().map_or(u64::MAX, |v| v.len() as u64)
            });
            let limit = (encoded as f64 * keep_ratio) as u64;
            let Some((cand, _)) = image::search_methods(ctx, &pnm, reencoders(ctx, &lw, &pnm, &Vec::new()), limit)? else { continue };
            best = Some(cand);
            lossy_hit = true;
            new_dict.remove("/DecodeParms");
            new_dict.insert("/Filter".into(), json!("/DCTDecode"));
        } else {
            continue;
        }
        let Some(best) = best else { continue };
        new_dict.remove("/Length");
        update.insert(key.clone(), json!({"stream": {"dict": new_dict, "datafile": best.to_string_lossy()}}));
        if lossy_hit {
            n_lossy += 1;
        } else {
            n_lossless += 1;
        }
    }
    if update.is_empty() {
        return Ok(None);
    }
    let json_path = work.join("pdf-update.json");
    let body = json!({"qpdf": [header, update]});
    if std::fs::write(&json_path, body.to_string()).is_err() {
        return Ok(None);
    }
    Ok(Some(Update { json: json_path, lossless: n_lossless, lossy: n_lossy }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::tools::Tools;
    use std::time::Duration;

    /// A damaged original (qpdf repairs it while reading) is rewritten only when every page
    /// renders the same: the repaired copy passes qpdf's own check whatever it lost.
    #[test]
    fn damaged_pdfs_are_checked_page_by_page() {
        let Some(c) = ctx(false) else {
            skip_test!("PDF tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-pdfdamaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ff, mutool) = (c.tools.ffmpeg.clone(), c.tools.get("mutool").unwrap().to_path_buf());
        let png = dir.join("page.png");
        run_ok(&ff, &["-v".as_ref(), "error".as_ref(), "-f".as_ref(), "lavfi".as_ref(), "-i".as_ref(), "testsrc2=s=400x300".as_ref(), "-frames:v".as_ref(), "1".as_ref(), png.as_os_str()]);
        let pdf = dir.join("doc.pdf");
        run_ok(&mutool, &["convert".as_ref(), "-o".as_ref(), pdf.as_os_str(), png.as_os_str()]);
        // A wrong cross-reference offset: readers (and qpdf) rebuild it, with a warning.
        let text = std::fs::read(&pdf).unwrap();
        let at = text.windows(9).rposition(|w| w == b"startxref").unwrap() + 10;
        let end = at + text[at..].iter().position(|b| !b.is_ascii_digit()).unwrap();
        let damaged = [&text[..at], b"1", &text[end..]].concat();
        std::fs::write(&pdf, damaged).unwrap();
        assert_ne!(Reference::read(&c, &pdf).unwrap().unwrap().check_code, Some(0), "damaged");
        let work = dir.join("w");
        std::fs::create_dir_all(&work).unwrap();
        let r = optimize(&c, &pdf, &work).unwrap();
        assert!(r.best.is_some(), "the repaired rewrite renders the same and is smaller: {:?}", r.note);
        assert!(r.steps[0].contains("pages checked"), "{:?}", r.steps);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-encoded JPEGs go into PDFs only when sequential: the check must read the frame
    /// marker (it looked for it among the APPn/COM segments, where it never is).
    #[test]
    fn progressive_jpegs_are_not_sequential() {
        let dir = std::env::temp_dir().join(format!("mo-seq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpeg = |sof: u8| {
            let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, b'h', b'i'];
            j.extend([0xFF, 0xDB, 0, 3, 0]);
            j.extend([0xFF, sof, 0, 11, 8, 0, 16, 0, 16, 1, 1, 0x11, 0]);
            j.extend([0xFF, 0xDA, 0, 2, 1, 2, 0xFF, 0xD9]);
            j
        };
        let (base, prog) = (dir.join("base.jpg"), dir.join("prog.jpg"));
        std::fs::write(&base, jpeg(0xC0)).unwrap();
        std::fs::write(&prog, jpeg(0xC2)).unwrap();
        assert!(sequential(&base));
        assert!(!sequential(&prog), "progressive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ctx(lossy: bool) -> Option<Ctx> {
        let ffmpeg = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
        let t = Tools::discover(ffmpeg.clone(), &std::env::temp_dir());
        let need = ["qpdf", "mutool", "jpegtran", "djpeg", "ssimulacra2"];
        (ffmpeg.exists() && need.iter().all(|n| t.has(n))).then(|| Ctx {
            tools: t,
            lossy,
            target: 93.0,
            lossy_min_gain: 0.03,
            strip: false,
            strict_alpha: false,
            lossy_docs: lossy,
            parallel: false,
            timeout: Duration::from_secs(300),
            gate: crate::image::tools::Gate::default(),
            max: crate::image::Max::default(),
            slots: None,
            progress: None,
        })
    }

    fn run_ok(program: &Path, args: &[&std::ffi::OsStr]) {
        let ok = std::process::Command::new(program).args(args).output().unwrap().status.success();
        assert!(ok, "{} {args:?}", program.display());
    }

    /// A PDF holding a quality-98 JPEG and one holding a PNG (Flate): the lossless pass
    /// optimizes the JPEG with identical pixels, the lossy pass re-encodes both and every
    /// rendered page still scores >= 93.
    #[test]
    fn pdf_images_lossless_and_lossy() {
        let (Some(lossless), Some(lossy)) = (ctx(false), ctx(true)) else {
            skip_test!("PDF tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-pdf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ff, mutool) = (lossless.tools.ffmpeg.clone(), lossless.tools.get("mutool").unwrap().to_path_buf());
        let src = "testsrc2=s=800x600,trim=end_frame=1";
        let (jpg, png) = (dir.join("photo.jpg"), dir.join("photo.png"));
        for (out, q) in [(&jpg, ["-q:v", "1"]), (&png, ["-compression_level", "1"])] {
            run_ok(&ff, &["-v".as_ref(), "error".as_ref(), "-y".as_ref(), "-f".as_ref(), "lavfi".as_ref(), "-i".as_ref(), src.as_ref(), q[0].as_ref(), q[1].as_ref(), out.as_os_str()]);
        }
        for img in [&jpg, &png] {
            let pdf = img.with_extension(format!("{}.pdf", crate::args::ext_lower(img)));
            run_ok(&mutool, &["convert".as_ref(), "-o".as_ref(), pdf.as_os_str(), img.as_os_str()]);
            for (name, c) in [("lossless", &lossless), ("lossy", &lossy)] {
                let work = dir.join(format!("w-{}-{name}", pdf.file_name().unwrap().to_string_lossy()));
                std::fs::create_dir_all(&work).unwrap();
                let r = optimize(c, &pdf, &work).unwrap();
                let Some(best) = r.best else {
                    println!("{} {name}: no gain ({:?})", pdf.display(), r.note);
                    continue;
                };
                println!("{} {name}: {} -> {} {:?}", pdf.file_name().unwrap().to_string_lossy(), size(&pdf), size(&best), r.steps);
                let reference = Reference::read(c, &pdf).unwrap().unwrap();
                assert!(reference.accepts(c, &best).unwrap(), "{name}: qpdf check failed");
                if name == "lossless" {
                    assert!(!r.steps[0].contains("lossy"), "{:?}", r.steps);
                }
                if r.steps[0].contains("lossy") {
                    assert!(pages_look_same(c, &pdf, &best, &work.join("again")).unwrap(), "{name}: pages differ");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every PDF in `MEDIAOPTI_TEST_PDFS` (copies), lossy: prints what each one gets.
    #[test]
    #[ignore = "needs MEDIAOPTI_TEST_PDFS pointing at real PDFs"]
    fn pdf_folder() {
        let (Some(c), Ok(dir)) = (ctx(true), std::env::var("MEDIAOPTI_TEST_PDFS")) else { return };
        for e in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let src = e.path();
            if crate::args::ext_lower(&src) != "pdf" {
                continue;
            }
            let work = std::env::temp_dir().join(format!("mo-pdfs-{}-{}", std::process::id(), e.file_name().to_string_lossy()));
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work).unwrap();
            let t = std::time::Instant::now();
            let r = optimize(&c, &src, &work).unwrap();
            let after = r.best.as_deref().map_or(size(&src), size);
            println!("{:>10} -> {:>10} ({:5.1}%) {:4.0}s {:?} {:?} {}", size(&src), after,
                100.0 * (size(&src) - after) as f64 / size(&src) as f64, t.elapsed().as_secs_f64(), r.steps, r.note, e.file_name().to_string_lossy());
            let _ = std::fs::remove_dir_all(&work);
        }
    }
}
