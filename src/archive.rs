//! ZIP-based files (Office, OpenDocument, EPUB, CBZ) and CBR -> CBZ, ported from imageopti's
//! `handle_zip` and `handle_cbr`.
//!
//! Images inside are optimized by the image pipeline; documents (Office, EPUB) stay lossless,
//! comics follow the image setting. The archive is rewritten with every entry in the same
//! order, stored entries stay stored (ODF and EPUB need their `mimetype` entry stored and
//! first), the rest is deflated at level 9. The result is reopened and every entry read back
//! through its CRC, and must match the expected names, order, storage and CRCs. Encrypted and
//! digitally signed files are left alone.

use crate::image::tools::{self, Capture, RunError};
use crate::image::{self, Ctx, Kind, Optimized};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

/// Images inside an archive bigger than this are copied as they are (read into memory).
const MAX_ENTRY_IMAGE: u64 = 256 * 1024 * 1024;
/// Entries smaller than this aren't worth optimizing.
const MIN_ENTRY_IMAGE: usize = 256;

/// Entries that mark a digitally signed document: rewriting would break the signature.
const SIGNATURE_MARKERS: &[&str] = &["_xmlsignatures/", "META-INF/documentsignatures.xml", "META-INF/signatures.xml"];

pub fn optimize(ctx: &Ctx, kind: Kind, src: &Path, work: &Path) -> Result<Optimized, RunError> {
    let r = match kind {
        Kind::Doc | Kind::Cbz => repack(ctx, kind, src, work),
        Kind::Cbr => cbr(ctx, src, work),
        _ => unreachable!("not an archive"),
    };
    r.map_err(|e| match e {
        Error::Run(r) => r,
        Error::Other(m) => RunError::Failed(m),
    })
}

enum Error {
    Run(RunError),
    Other(String),
}

impl From<RunError> for Error {
    fn from(e: RunError) -> Self {
        Self::Run(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Other(e.to_string())
    }
}

/// Image pipeline settings for the pictures inside: documents never lossy, comics as set.
fn inner_ctx(ctx: &Ctx, kind: Kind) -> Ctx {
    Ctx {
        tools: ctx.tools.clone(),
        lossy: ctx.lossy && matches!(kind, Kind::Cbz | Kind::Cbr),
        target: ctx.target,
        lossy_min_gain: ctx.lossy_min_gain,
        strip: ctx.strip,
        strict_alpha: ctx.strict_alpha,
        parallel: ctx.parallel,
        timeout: ctx.timeout,
        cancel: ctx.cancel.clone(),
    }
}

fn image_kind(name: &str) -> Option<Kind> {
    image::kind_of(Path::new(name)).filter(|k| matches!(k, Kind::Png | Kind::Jpg | Kind::Gif))
}

/// A smaller verified version of an image entry, and the steps that made it.
type Better = (Vec<u8>, Vec<String>);

/// Optimizes one image entry in its own temp folder.
fn optimize_entry(ictx: &Ctx, kind: Kind, data: &[u8], work: &Path) -> Result<Option<Better>, RunError> {
    if data.len() <= MIN_ENTRY_IMAGE {
        return Ok(None);
    }
    let ext = match kind {
        Kind::Png => "png",
        Kind::Jpg => "jpg",
        _ => "gif",
    };
    let file = work.join(format!("in.{ext}"));
    if std::fs::create_dir_all(work).and_then(|()| std::fs::write(&file, data)).is_err() {
        return Ok(None);
    }
    let r = image::optimize(ictx, kind, &file, work)?;
    let result = r.best.and_then(|b| std::fs::read(b).ok()).filter(|b| b.len() < data.len()).map(|b| (b, r.steps));
    let _ = std::fs::remove_dir_all(work);
    Ok(result)
}

fn label(base: &str, n: usize, steps: &BTreeSet<String>) -> String {
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}[{n} img: {}]", steps.iter().cloned().collect::<Vec<_>>().join("+"))
    }
}

/// What an entry of the new archive must be.
struct Expected {
    name: Vec<u8>,
    crc: u32,
    stored: bool,
    dir: bool,
}

/// Reopens `path` and reads every entry through its CRC check; names, order, storage and
/// CRCs must be as expected.
fn verify(path: &Path, expected: &[Expected]) -> Result<bool, Error> {
    let Ok(mut z) = ZipArchive::new(BufReader::new(File::open(path)?)) else { return Ok(false) };
    if z.len() != expected.len() {
        return Ok(false);
    }
    for (i, want) in expected.iter().enumerate() {
        let Ok(mut f) = z.by_index(i) else { return Ok(false) };
        let stored = f.compression() == CompressionMethod::Stored;
        if f.name_raw() != want.name.as_slice() || f.crc32() != want.crc || stored != want.stored || f.is_dir() != want.dir {
            return Ok(false);
        }
        // Reading to the end checks the CRC of the decompressed data.
        if io::copy(&mut f, &mut io::sink()).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Office, OpenDocument, EPUB and CBZ: images optimized, the archive rewritten.
fn repack(ctx: &Ctx, kind: Kind, src: &Path, work: &Path) -> Result<Optimized, Error> {
    let Ok(mut zin) = ZipArchive::new(BufReader::new(File::open(src)?)) else {
        return Ok(Optimized::skipped("not a valid zip"));
    };
    for i in 0..zin.len() {
        let Ok(f) = zin.by_index_raw(i) else { return Ok(Optimized::skipped("damaged zip")) };
        if f.encrypted() {
            return Ok(Optimized::skipped("encrypted: skipped"));
        }
        let name = String::from_utf8_lossy(f.name_raw()).into_owned();
        if SIGNATURE_MARKERS.iter().any(|m| name.starts_with(m)) {
            return Ok(Optimized::skipped("digitally signed: skipped"));
        }
    }
    let ictx = inner_ctx(ctx, kind);
    let out = work.join(format!("repack.{}", crate::args::ext_lower(src)));
    let mut zout = ZipWriter::new(BufWriter::new(File::create(&out)?));
    zout.set_raw_comment(zin.comment().into())?;
    let (mut expected, mut n_img, mut steps) = (Vec::new(), 0, BTreeSet::new());
    for i in 0..zin.len() {
        let mut f = zin.by_index(i)?;
        let name = f.name().map(std::borrow::Cow::into_owned)?;
        let stored = f.compression() == CompressionMethod::Stored;
        let mut options = f.options().into_full_options().with_file_comment(f.comment());
        if !stored {
            options = options.compression_method(CompressionMethod::Deflated).compression_level(Some(9));
        }
        let mut crc = f.crc32();
        if f.is_dir() {
            zout.add_directory(name.clone(), options)?;
        } else if let Some(k) = image_kind(&name).filter(|_| f.size() <= MAX_ENTRY_IMAGE) {
            let mut data = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut data)?;
            if let Some((better, s)) = optimize_entry(&ictx, k, &data, &work.join(format!("z{i}")))? {
                data = better;
                crc = crc32(&data);
                n_img += 1;
                steps.extend(s);
            }
            zout.start_file(name.clone(), options)?;
            zout.write_all(&data)?;
        } else {
            zout.start_file(name.clone(), options)?;
            io::copy(&mut f, &mut zout)?;
        }
        expected.push(Expected { name: f.name_raw().to_vec(), crc, stored, dir: f.is_dir() });
    }
    zout.finish()?.flush()?;
    let smaller = std::fs::metadata(&out)?.len() < std::fs::metadata(src)?.len();
    if !smaller || !verify(&out, &expected)? {
        return Ok(Optimized::nothing());
    }
    Ok(Optimized::found(Some(out), vec![label("zip-repack", n_img, &steps)]))
}

fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

// ---------------------------------------------------------------- CBR -> CBZ

/// One file listed by `7z l -slt`.
struct RarEntry {
    path: String,
    crc: Option<u32>,
    modified: Option<DateTime>,
}

/// "2019-03-04 12:34:56.1234567" (local time, as 7-Zip lists it).
fn parse_7z_time(s: &str) -> Option<DateTime> {
    let (date, time) = s.split_once(' ')?;
    let d: Vec<u16> = date.split('-').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let t: Vec<u8> = time.split(['.', ':']).take(3).map(|p| p.parse().ok()).collect::<Option<_>>()?;
    match (d.as_slice(), t.as_slice()) {
        ([y, mo, da], [h, mi, se]) => DateTime::from_date_and_time((*y).max(1980), *mo as u8, *da as u8, *h, *mi, *se).ok(),
        _ => None,
    }
}

/// A relative path with no `..`: anything else could point outside the extraction folder.
fn safe_relative(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// CBR (RAR) -> CBZ (ZIP): the same files in the same order, images optimized (as comics:
/// lossy follows the image setting), each extracted file checked against its CRC.
fn cbr(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, Error> {
    let Some(sz) = ctx.tools.get("7z") else { return Ok(Optimized::skipped("7-Zip is missing")) };
    let listing = tools::run(sz, crate::os_args!["l", "-slt", "-ba", "-sccUTF-8", src], None, Capture::Bytes, ctx.timeout, &ctx.cancel)?;
    if !listing.success {
        return Ok(Optimized::skipped("7-Zip cannot read it"));
    }
    let text = String::from_utf8_lossy(&listing.stdout).replace('\r', "");
    let mut files = Vec::new();
    for block in text.split("\n\n") {
        let d: HashMap<&str, &str> = block.lines().filter_map(|l| l.split_once(" = ")).collect();
        let Some(path) = d.get("Path").filter(|p| !p.is_empty()) else { continue };
        if d.get("Encrypted") == Some(&"+") {
            return Ok(Optimized::skipped("encrypted: skipped"));
        }
        if d.get("Folder") == Some(&"+") || d.get("Attributes").is_some_and(|a| a.contains('D')) {
            continue;
        }
        if !safe_relative(path) {
            return Ok(Optimized::skipped("unsafe path inside the archive: skipped"));
        }
        files.push(RarEntry {
            path: (*path).to_string(),
            crc: d.get("CRC").and_then(|c| u32::from_str_radix(c, 16).ok()),
            modified: d.get("Modified").and_then(|m| parse_7z_time(m)),
        });
    }
    if files.is_empty() {
        return Ok(Optimized::skipped("empty archive (or not one 7-Zip lists)"));
    }
    let xdir = work.join("x");
    let mut out_arg = std::ffi::OsString::from("-o");
    out_arg.push(&xdir);
    if !tools::run(sz, crate::os_args!["x", "-y", "-sccUTF-8", out_arg, src], None, Capture::None, ctx.timeout, &ctx.cancel)?.success {
        return Ok(Optimized::skipped("7-Zip could not extract it"));
    }
    let ictx = inner_ctx(ctx, Kind::Cbr);
    let out = work.join("out.cbz");
    let mut zout = ZipWriter::new(BufWriter::new(File::create(&out)?));
    let (mut expected, mut n_img, mut steps) = (Vec::new(), 0, BTreeSet::new());
    for (i, e) in files.iter().enumerate() {
        let mut data = std::fs::read(xdir.join(&e.path))?;
        if e.crc.is_some_and(|c| c != crc32(&data)) {
            return Ok(Optimized::skipped(&format!("an extracted file does not match its CRC: {}", e.path)));
        }
        let name = e.path.replace('\\', "/");
        let kind = image_kind(&name);
        if let Some(k) = kind {
            if let Some((better, s)) = optimize_entry(&ictx, k, &data, &work.join(format!("z{i}")))? {
                data = better;
                n_img += 1;
                steps.extend(s);
            }
        }
        // Already-compressed pictures are stored; everything else deflated.
        let stored = kind.is_some() || name.to_ascii_lowercase().ends_with(".webp");
        let options = SimpleFileOptions::default()
            .compression_method(if stored { CompressionMethod::Stored } else { CompressionMethod::Deflated })
            .compression_level((!stored).then_some(9))
            .last_modified_time(e.modified.unwrap_or_else(DateTime::default_for_write))
            .large_file(data.len() as u64 >= u64::from(u32::MAX));
        zout.start_file(name.clone(), options)?;
        zout.write_all(&data)?;
        expected.push(Expected { name: name.into_bytes(), crc: crc32(&data), stored, dir: false });
    }
    zout.finish()?.flush()?;
    if !verify(&out, &expected)? {
        return Ok(Optimized::skipped("the CBZ did not verify: kept"));
    }
    let step = label("to-cbz", n_img, &steps);
    Ok(Optimized { new_ext: Some("cbz"), ..Optimized::found(Some(out), vec![step]) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::tools::Tools;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> Option<Ctx> {
        let ffmpeg = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join("ffmpeg.exe");
        let t = Tools::discover(ffmpeg.clone(), &std::env::temp_dir());
        (ffmpeg.exists() && ["oxipng", "7z"].iter().all(|n| t.has(n))).then(|| Ctx {
            tools: t,
            lossy: true,
            target: 93.0,
            lossy_min_gain: 0.03,
            strip: false,
            strict_alpha: false,
            parallel: false,
            timeout: Duration::from_secs(120),
            cancel: CancellationToken::new(),
        })
    }

    fn png_bytes(c: &Ctx, dir: &Path) -> Vec<u8> {
        let p = dir.join("pic.png");
        let ok = std::process::Command::new(&c.tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "mandelbrot=s=320x240,trim=end_frame=1", "-frames:v", "1", "-compression_level", "0"])
            .arg(&p)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        std::fs::read(p).unwrap()
    }

    fn entries(p: &Path) -> Vec<(String, bool, u32)> {
        let mut z = ZipArchive::new(File::open(p).unwrap()).unwrap();
        (0..z.len())
            .map(|i| {
                let f = z.by_index(i).unwrap();
                (f.name().unwrap().into_owned(), f.compression() == CompressionMethod::Stored, f.crc32())
            })
            .collect()
    }

    /// An ODF-like document and a comic: same entries in the same order, `mimetype` still
    /// stored and first, text untouched, the picture smaller (lossless in the document).
    #[test]
    fn documents_and_comics_repack() {
        let Some(c) = ctx() else {
            eprintln!("skipping: tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pic = png_bytes(&c, &dir);
        let text = "<doc>".repeat(2000);
        for (name, kind) in [("doc.odt", Kind::Doc), ("comic.cbz", Kind::Cbz)] {
            let src = dir.join(name);
            let mut z = ZipWriter::new(File::create(&src).unwrap());
            let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            let fast = SimpleFileOptions::default().compression_level(Some(1));
            z.start_file("mimetype", stored).unwrap();
            z.write_all(b"application/vnd.oasis.opendocument.text").unwrap();
            z.start_file("content.xml", fast).unwrap();
            z.write_all(text.as_bytes()).unwrap();
            z.add_directory("Pictures/", fast).unwrap();
            z.start_file("Pictures/a.png", stored).unwrap();
            z.write_all(&pic).unwrap();
            z.finish().unwrap();

            let work = dir.join(format!("w-{name}"));
            std::fs::create_dir_all(&work).unwrap();
            let r = optimize(&c, kind, &src, &work).unwrap();
            let best = r.best.unwrap_or_else(|| panic!("{name}: no gain {:?}", r.note));
            println!("{name}: {} -> {} {:?}", std::fs::metadata(&src).unwrap().len(), std::fs::metadata(&best).unwrap().len(), r.steps);
            let (a, b) = (entries(&src), entries(&best));
            assert_eq!(a.iter().map(|e| (&e.0, e.1)).collect::<Vec<_>>(), b.iter().map(|e| (&e.0, e.1)).collect::<Vec<_>>(), "{name}: layout changed");
            assert_eq!(a[1].2, b[1].2, "{name}: text changed");
            assert_ne!(a[3].2, b[3].2, "{name}: picture not optimized");
            if kind == Kind::Doc {
                assert!(!r.steps[0].contains('='), "documents must stay lossless: {:?}", r.steps);
            }
        }

        // An encrypted zip is left alone.
        let enc = dir.join("secret.cbz");
        let sz = c.tools.get("7z").unwrap();
        let ok = std::process::Command::new(sz).arg("a").arg("-tzip").arg("-pxyz").arg(&enc).arg(dir.join("pic.png")).output().unwrap().status.success();
        assert!(ok);
        let r = optimize(&c, Kind::Cbz, &enc, &dir).unwrap();
        assert!(r.best.is_none() && r.note.as_deref().is_some_and(|n| n.contains("encrypted")), "{:?}", r.note);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
