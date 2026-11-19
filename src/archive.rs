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

/// Image pipeline settings for the pictures inside: documents never lossy and with strict
/// alpha, comics as set.
fn inner_ctx(ctx: &Ctx, kind: Kind) -> Ctx {
    Ctx {
        tools: ctx.tools.clone(),
        lossy: ctx.lossy && matches!(kind, Kind::Cbz | Kind::Cbr),
        target: ctx.target,
        lossy_min_gain: ctx.lossy_min_gain,
        strip: ctx.strip,
        // Documents keep every pixel, even fully transparent ones (invisible, but their
        // colours are content: an editor could change the alpha later).
        strict_alpha: ctx.strict_alpha || kind == Kind::Doc,
        lossy_docs: false,
        parallel: ctx.parallel,
        timeout: ctx.timeout,
        gate: ctx.gate.clone(),
        max: ctx.max,
        slots: ctx.slots.clone(),
        // Pages count on the archive's own progress (see `optimize_pages`).
        progress: None,
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

/// Pictures read ahead and optimized together: at most this many, or this many bytes.
const BATCH_IMAGES: usize = 32;
const BATCH_BYTES: u64 = 256 * 1024 * 1024;
/// Free commit needed per extra page optimized in parallel.
const PAGE_COMMIT: u64 = 1024 * 1024 * 1024;

/// One picture of a batch: its index in the archive, kind and bytes.
type Page = (usize, Kind, Vec<u8>);
/// A picture after optimizing: its index, bytes (smaller or the same) and steps.
type Done = (usize, Vec<u8>, Vec<String>);

/// Optimizes a batch of pictures: on this job's own thread plus the image workers' idle CPU
/// slots (a comic's pages then use the whole CPU near the end of a run). Returns, per page,
/// its index, bytes (optimized or not) and steps.
fn optimize_pages(ictx: &Ctx, pages: Vec<Page>, work: &Path, progress: Option<&image::tools::Progress>) -> Result<Vec<Done>, RunError> {
    /// Gives the borrowed slots back however the pages end, a panic included.
    struct Borrowed<'a>(Option<&'a image::tools::Slots>, usize);
    impl Drop for Borrowed<'_> {
        fn drop(&mut self) {
            if let Some(slots) = self.0 {
                slots.release(self.1);
            }
        }
    }
    let mut extra = 0;
    if let Some(slots) = &ictx.slots {
        let commit = crate::system::memory_status().map_or(u64::MAX, |m| m.available_commit);
        let affordable = usize::try_from(commit.saturating_sub(2 * PAGE_COMMIT) / PAGE_COMMIT).unwrap_or(usize::MAX);
        extra = slots.borrow_idle(pages.len().saturating_sub(1).min(affordable));
    }
    let _borrowed = Borrowed(ictx.slots.as_deref(), extra);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let pages: Vec<std::sync::Mutex<Option<Page>>> = pages.into_iter().map(|p| std::sync::Mutex::new(Some(p))).collect();
    let results = std::sync::Mutex::new(Vec::with_capacity(pages.len()));
    let failure = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..=extra {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(slot) = pages.get(i) else { break };
                let Some((idx, kind, data)) = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() else { continue };
                if failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() || !ictx.gate.wait_while_paused() {
                    failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert(RunError::Cancelled);
                    break;
                }
                crate::system::panic_here_in_tests(work);
                let r = optimize_entry(ictx, kind, &data, &work.join(format!("z{idx}")));
                if let Some(p) = progress {
                    p.step();
                }
                match r {
                    Ok(better) => {
                        let (bytes, steps) = better.unwrap_or((data, Vec::new()));
                        results.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((idx, bytes, steps));
                    }
                    Err(e) => {
                        failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert(e);
                        break;
                    }
                }
            });
        }
    });
    drop(_borrowed);
    if let Some(e) = failure.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
        return Err(e);
    }
    Ok(results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner))
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
    ctx.phase("pictures", &[]);
    if let Some(p) = &ctx.progress {
        let pictures = (0..zin.len()).filter(|&i| zin.name_for_index(i).is_some_and(|n| n.as_ref().is_ok_and(|n| image_kind(n).is_some()))).count();
        p.set_total(pictures);
    }
    let out = work.join(format!("repack.{}", crate::args::ext_lower(src)));
    let mut zout = ZipWriter::new(BufWriter::new(File::create(&out)?));
    zout.set_raw_comment(zin.comment().into())?;
    let (mut expected, mut n_img, mut steps) = (Vec::new(), 0, BTreeSet::new());
    let mut start = 0;
    while start < zin.len() {
        // Read a batch of pictures ahead and optimize them together...
        let (mut pages, mut bytes, mut end) = (Vec::new(), 0u64, start);
        while end < zin.len() && pages.len() < BATCH_IMAGES && bytes < BATCH_BYTES {
            let mut f = zin.by_index(end)?;
            let name = f.name().map(std::borrow::Cow::into_owned)?;
            if let Some(k) = image_kind(&name).filter(|_| !f.is_dir() && f.size() <= MAX_ENTRY_IMAGE) {
                let mut data = Vec::with_capacity(f.size() as usize);
                f.read_to_end(&mut data)?;
                bytes += data.len() as u64;
                pages.push((end, k, data));
            }
            end += 1;
        }
        let mut done: HashMap<usize, (Vec<u8>, Vec<String>)> =
            optimize_pages(&ictx, pages, work, ctx.progress.as_deref())?.into_iter().map(|(i, b, s)| (i, (b, s))).collect();
        // ...then write the entries of the batch in their order.
        for i in start..end {
            let mut f = zin.by_index(i)?;
            let name = f.name().map(std::borrow::Cow::into_owned)?;
            let stored = f.compression() == CompressionMethod::Stored;
            let mut options = f.options().into_full_options().with_file_comment(f.comment());
            if !stored {
                options = options.compression_method(CompressionMethod::Deflated).compression_level(Some(9));
            }
            let mut crc = f.crc32();
            if f.is_dir() {
                zout.add_directory(name, options)?;
            } else if let Some((data, s)) = done.remove(&i) {
                if !s.is_empty() {
                    crc = crc32(&data);
                    n_img += 1;
                    steps.extend(s);
                }
                zout.start_file(name, options)?;
                zout.write_all(&data)?;
            } else {
                zout.start_file(name, options)?;
                io::copy(&mut f, &mut zout)?;
            }
            // `add_directory` always writes a folder stored (it has no data either way).
            expected.push(Expected { name: f.name_raw().to_vec(), crc, stored: stored || f.is_dir(), dir: f.is_dir() });
        }
        start = end;
    }
    zout.finish()?.flush()?;
    let smaller = std::fs::metadata(&out)?.len() < std::fs::metadata(src)?.len();
    if !smaller || !verify(&out, &expected)? {
        return Ok(Optimized::nothing());
    }
    Ok(Optimized::found(Some(out), vec![label("zip-repack", n_img, &steps)]))
}

/// Whether a CBZ holds the same files as an extracted CBR folder: the same names, each file
/// identical or (pictures) the same picture. A CBZ made earlier from that CBR passes.
pub fn same_as_folder(ctx: &Ctx, dir: &Path, cbz: &Path, work: &Path) -> Result<bool, RunError> {
    let mut on_disk = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(dir) {
                on_disk.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let Ok(mut z) = File::open(cbz).map(BufReader::new).map_err(|_| ()).and_then(|f| ZipArchive::new(f).map_err(|_| ())) else { return Ok(false) };
    let mut names: Vec<String> = Vec::new();
    for i in 0..z.len() {
        let Ok(f) = z.by_index(i) else { return Ok(false) };
        if !f.is_dir() {
            names.push(f.name().unwrap_or_default().to_string());
        }
    }
    on_disk.sort();
    let mut sorted = names.clone();
    sorted.sort();
    if on_disk != sorted {
        return Ok(false);
    }
    let cmp = work.join("cmp");
    let _ = std::fs::create_dir_all(&cmp);
    for name in names {
        let Ok(mut f) = z.by_name(&name) else { return Ok(false) };
        let mut data = Vec::new();
        if f.read_to_end(&mut data).is_err() {
            return Ok(false);
        }
        let orig = dir.join(&name);
        if std::fs::read(&orig).is_ok_and(|o| crc32(&o) == crc32(&data)) {
            continue;
        }
        let Some(kind) = image_kind(&name) else { return Ok(false) };
        let copy = cmp.join(format!("page.{}", image::ext_of(kind)));
        let same = std::fs::write(&copy, &data).is_ok() && image::same_picture(ctx, &orig, &copy)?;
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
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
    let listing = tools::run(sz, crate::os_args!["l", "-slt", "-ba", "-sccUTF-8", src], None, Capture::Bytes, ctx.timeout, &ctx.gate)?;
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
    ctx.phase("extracting", &[src]);
    if !tools::run(sz, crate::os_args!["x", "-y", "-sccUTF-8", out_arg, src], None, Capture::None, ctx.timeout, &ctx.gate)?.success {
        return Ok(Optimized::skipped("7-Zip could not extract it"));
    }
    let ictx = inner_ctx(ctx, Kind::Cbr);
    ctx.phase("pictures", &[]);
    if let Some(p) = &ctx.progress {
        p.set_total(files.iter().filter(|e| image_kind(&e.path).is_some()).count());
    }
    let out = work.join("out.cbz");
    let mut zout = ZipWriter::new(BufWriter::new(File::create(&out)?));
    let (mut expected, mut n_img, mut steps) = (Vec::new(), 0, BTreeSet::new());
    let mut i = 0;
    while i < files.len() {
        // Read files (each checked against its CRC) until a batch of pictures is ready...
        let (mut batch, mut bytes, mut pictures) = (Vec::new(), 0u64, 0);
        while i < files.len() && pictures < BATCH_IMAGES && bytes < BATCH_BYTES {
            let e = &files[i];
            let data = std::fs::read(xdir.join(&e.path))?;
            if e.crc.is_some_and(|c| c != crc32(&data)) {
                return Ok(Optimized::skipped(&format!("an extracted file does not match its CRC: {}", e.path)));
            }
            pictures += usize::from(image_kind(&e.path).is_some());
            bytes += data.len() as u64;
            batch.push((i, data));
            i += 1;
        }
        // ...optimize its pictures together, then write the files in their order.
        let (pages, others): (Vec<_>, Vec<_>) = batch.into_iter().partition(|(j, _)| image_kind(&files[*j].path).is_some());
        let pages = pages.into_iter().filter_map(|(j, d)| Some((j, image_kind(&files[j].path)?, d))).collect();
        let mut ready = optimize_pages(&ictx, pages, work, ctx.progress.as_deref())?;
        ready.extend(others.into_iter().map(|(j, d)| (j, d, Vec::new())));
        ready.sort_by_key(|r| r.0);
        for (j, data, s) in ready {
            let e = &files[j];
            let name = e.path.replace('\\', "/");
            let kind = image_kind(&name);
            if !s.is_empty() {
                n_img += 1;
                steps.extend(s);
            }
            write_cbz_entry(&mut zout, e, &name, kind.is_some(), &data)?;
            let stored = kind.is_some() || name.to_ascii_lowercase().ends_with(".webp");
            expected.push(Expected { name: name.into_bytes(), crc: crc32(&data), stored, dir: false });
        }
    }
    zout.finish()?.flush()?;
    if !verify(&out, &expected)? {
        return Ok(Optimized::skipped("the CBZ did not verify: kept"));
    }
    let step = label("to-cbz", n_img, &steps);
    Ok(Optimized { new_ext: Some("cbz"), ..Optimized::found(Some(out), vec![step]) })
}

/// Adds one file of a CBR to the new CBZ.
fn write_cbz_entry(zout: &mut ZipWriter<BufWriter<File>>, e: &RarEntry, name: &str, picture: bool, data: &[u8]) -> Result<(), Error> {
    {
        // Already-compressed pictures are stored; everything else deflated.
        let stored = picture || name.to_ascii_lowercase().ends_with(".webp");
        let options = SimpleFileOptions::default()
            .compression_method(if stored { CompressionMethod::Stored } else { CompressionMethod::Deflated })
            .compression_level((!stored).then_some(9))
            .last_modified_time(e.modified.unwrap_or_else(DateTime::default_for_write))
            .large_file(data.len() as u64 >= u64::from(u32::MAX));
        zout.start_file(name, options)?;
        zout.write_all(data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::tools::Tools;
    use std::time::Duration;

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
            lossy_docs: false,
            parallel: false,
            timeout: Duration::from_secs(120),
            gate: crate::image::tools::Gate::default(),
            max: crate::image::Max::default(),
            slots: None,
            progress: None,
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
            skip_test!("tools not found");
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

    /// Java's ZipOutputStream and Python's zipfile write folder entries deflated; the zip
    /// crate writes them stored. The check wanted the original method, so every such EPUB,
    /// CBZ or Office file had its repack thrown away.
    #[test]
    fn deflated_folder_entries_do_not_spoil_the_repack() {
        let Some(c) = ctx() else {
            skip_test!("tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-zip-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pic = png_bytes(&c, &dir);
        let src = dir.join("book.cbz");
        let mut z = ZipWriter::new(File::create(&src).unwrap());
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        z.start_file("pages/", deflated).unwrap(); // a folder, written as Java does
        z.start_file("pages/01.png", SimpleFileOptions::default().compression_method(CompressionMethod::Stored)).unwrap();
        z.write_all(&pic).unwrap();
        z.finish().unwrap();
        assert!(!entries(&src)[0].1, "the folder entry is deflated in the original");
        let work = dir.join("w");
        std::fs::create_dir_all(&work).unwrap();
        let r = optimize(&c, Kind::Cbz, &src, &work).unwrap();
        assert!(r.best.is_some(), "repack thrown away: {:?}", r.note);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page that panics (a bug, a picture nobody foresaw) fails its comic, and the CPU
    /// slots the comic borrowed come back: the other image workers don't wait for them.
    #[test]
    fn borrowed_slots_come_back_after_a_panic() {
        let Some(mut c) = ctx() else {
            skip_test!("tools not found");
            return;
        };
        let slots = crate::image::tools::Slots::new(4);
        assert!(slots.acquire(&c.gate), "the job's own slot");
        c.slots = Some(slots.clone());
        let work = std::env::temp_dir().join(format!("mo-slot-panic-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        crate::system::PANIC_ON.lock().unwrap().push(work.file_name().unwrap().to_string_lossy().into_owned());
        let pages: Vec<Page> = (0..6).map(|i| (i, Kind::Png, vec![0u8; 16])).collect();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| optimize_pages(&c, pages, &work, None))).is_err();
        crate::system::PANIC_ON.lock().unwrap().retain(|n| !n.starts_with("mo-slot-panic-"));
        assert!(panicked);
        assert_eq!(slots.borrow_idle(usize::MAX), 3, "every borrowed slot is back (the job still holds its own)");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A comic with more pages than a batch, on borrowed CPU slots: every page optimized,
    /// the entries in their order, and the borrowed slots given back.
    #[test]
    fn comic_pages_run_in_parallel() {
        let Some(mut c) = ctx() else {
            skip_test!("tools not found");
            return;
        };
        c.lossy = false;
        let slots = crate::image::tools::Slots::new(4);
        assert!(slots.acquire(&c.gate), "the job's own slot");
        c.slots = Some(slots.clone());
        let progress = std::sync::Arc::new(crate::image::tools::Progress::default());
        c.progress = Some(progress.clone());
        let dir = std::env::temp_dir().join(format!("mo-comic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pic = png_bytes(&c, &dir);
        let src = dir.join("comic.cbz");
        let mut z = ZipWriter::new(File::create(&src).unwrap());
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let pages = BATCH_IMAGES + 5;
        for n in 0..pages {
            z.start_file(format!("p{n:03}.png"), stored).unwrap();
            z.write_all(&pic).unwrap();
            z.start_file(format!("p{n:03}.txt"), stored).unwrap();
            z.write_all(n.to_string().as_bytes()).unwrap();
        }
        z.finish().unwrap();
        let work = dir.join("w");
        std::fs::create_dir_all(&work).unwrap();
        let r = optimize(&c, Kind::Cbz, &src, &work).unwrap();
        let best = r.best.expect("smaller");
        let (a, b) = (entries(&src), entries(&best));
        assert_eq!(a.iter().map(|e| &e.0).collect::<Vec<_>>(), b.iter().map(|e| &e.0).collect::<Vec<_>>(), "order kept");
        for (x, y) in a.iter().zip(&b) {
            if x.0.ends_with(".png") {
                assert_ne!(x.2, y.2, "{} not optimized", x.0);
            } else {
                assert_eq!(x.2, y.2, "{} changed", x.0);
            }
        }
        assert!(r.steps[0].contains(&format!("[{pages} img")), "{:?}", r.steps);
        assert_eq!(slots.borrow_idle(10), 3, "borrowed slots returned");
        assert_eq!(progress.get(), Some((pages, pages)), "every page counted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
