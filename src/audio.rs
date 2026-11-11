//! Audio (FLAC, MP3), ported from imageopti's `handle_flac` and `handle_mp3`.
//!
//! - **Cover art**: embedded pictures (FLAC PICTURE blocks, ID3v2 APIC frames) are optimized
//!   by the image pipeline (pixel-identical, or SSIMULACRA2 >= 93 in lossy mode). Everything
//!   else (audio frames, tags, other blocks) is copied byte for byte and checked to be.
//! - **FLAC re-encoding** (`-8 -p -A subdivide_tukey(5)`): only for files from encoders older
//!   than libFLAC 1.4, accepted only when the decoded audio, the tags and the CUESHEET,
//!   APPLICATION and PICTURE blocks are unchanged.
//!
//! Audio data is streamed, never read into memory whole (hi-res albums can be huge).

use crate::image::tools::{self, Capture, RunError};
use crate::image::{self, Ctx, Kind, Optimized};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Tags or metadata bigger than this are left alone (they are read into memory).
const MAX_META: u64 = 64 * 1024 * 1024;
/// Pictures smaller than this aren't worth optimizing.
const MIN_ART: usize = 1024;

const PICTURE: u8 = 6;
const VORBIS_COMMENT: u8 = 4;

pub fn optimize(ctx: &Ctx, kind: Kind, src: &Path, work: &Path) -> Result<Optimized, RunError> {
    let r = match kind {
        Kind::Flac => flac(ctx, src, work),
        Kind::Mp3 => mp3(ctx, src, work),
        _ => unreachable!("not audio"),
    };
    match r {
        Ok(o) => Ok(o),
        Err(AudioError::Run(e)) => Err(e),
        Err(AudioError::Io(e)) => Err(RunError::Failed(e.to_string())),
    }
}

enum AudioError {
    Run(RunError),
    Io(io::Error),
}

impl From<RunError> for AudioError {
    fn from(e: RunError) -> Self {
        Self::Run(e)
    }
}

impl From<io::Error> for AudioError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

fn skip(why: &str) -> Result<Optimized, AudioError> {
    Ok(Optimized { best: None, steps: Vec::new(), note: Some(why.to_string()) })
}

/// PNG or JPEG by signature.
fn sniff(data: &[u8]) -> Option<Kind> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Kind::Png)
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some(Kind::Jpg)
    } else {
        None
    }
}

/// A smaller picture and the steps that made it.
type Art = (Vec<u8>, Vec<String>);

/// Optimizes an embedded picture.
fn optimize_art(ctx: &Ctx, data: &[u8], work: &Path) -> Result<Option<Art>, RunError> {
    let Some(kind) = sniff(data).filter(|_| data.len() >= MIN_ART) else { return Ok(None) };
    let file = work.join(if kind == Kind::Png { "art.png" } else { "art.jpg" });
    if std::fs::create_dir_all(work).and_then(|()| std::fs::write(&file, data)).is_err() {
        return Ok(None);
    }
    let r = image::optimize(ctx, kind, &file, work)?;
    let Some(best) = r.best else { return Ok(None) };
    Ok(std::fs::read(best).ok().filter(|b| b.len() < data.len()).map(|b| (b, r.steps)))
}

fn art_label(n: usize, steps: &BTreeSet<String>) -> String {
    format!("cover-art[{n}: {}]", steps.iter().cloned().collect::<Vec<_>>().join("+"))
}

/// Whether `a` from `a_at` and `b` from `b_at` to their ends are the same bytes.
fn same_tail(a: &Path, a_at: u64, b: &Path, b_at: u64) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    if fa.metadata()?.len().checked_sub(a_at) != fb.metadata()?.len().checked_sub(b_at) {
        return Ok(false);
    }
    fa.seek(SeekFrom::Start(a_at))?;
    fb.seek(SeekFrom::Start(b_at))?;
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = read_full(&mut fa, &mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        if read_full(&mut fb, &mut bb[..n])? != n || ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Writes `head` then `src` from `at` to its end into `out`.
fn write_with_tail(out: &Path, head: &[u8], src: &Path, at: u64) -> io::Result<()> {
    let mut f = io::BufWriter::new(File::create(out)?);
    f.write_all(head)?;
    let mut s = File::open(src)?;
    s.seek(SeekFrom::Start(at))?;
    io::copy(&mut s, &mut f)?;
    f.flush()
}

/// SHA-256 of the decoded audio (first audio stream, 32-bit samples: exact for any FLAC).
fn audio_hash(ctx: &Ctx, path: &Path) -> Result<Option<[u8; 32]>, RunError> {
    let args = crate::os_args!["-v", "error", "-i", path, "-map", "0:a:0", "-f", "s32le", "-"];
    let ran = tools::run(&ctx.tools.ffmpeg, args, None, Capture::Hash, ctx.timeout, &ctx.cancel)?;
    Ok(ran.hash.filter(|_| ran.success))
}

// ---------------------------------------------------------------- FLAC

struct Flac {
    blocks: Vec<(u8, Vec<u8>)>,
    /// Where the audio frames start.
    audio_at: u64,
}

/// Reads a native FLAC's metadata blocks (not ones wrapped in Ogg or behind an ID3 tag).
fn flac_read(path: &Path) -> io::Result<Option<Flac>> {
    let mut f = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 4];
    if read_full(&mut f, &mut magic)? != 4 || &magic != b"fLaC" {
        return Ok(None);
    }
    let (mut blocks, mut at, mut total) = (Vec::new(), 4u64, 0u64);
    loop {
        let mut h = [0u8; 4];
        if read_full(&mut f, &mut h)? != 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        total += len as u64;
        if total > MAX_META {
            return Ok(None);
        }
        let mut body = vec![0u8; len];
        if read_full(&mut f, &mut body)? != len {
            return Ok(None);
        }
        blocks.push((h[0] & 0x7f, body));
        at += 4 + len as u64;
        if h[0] & 0x80 != 0 {
            return Ok(Some(Flac { blocks, audio_at: at }));
        }
    }
}

fn flac_header(blocks: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    for (n, (typ, body)) in blocks.iter().enumerate() {
        out.push(typ | if n + 1 == blocks.len() { 0x80 } else { 0 });
        out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(body);
    }
    out
}

struct Picture<'a> {
    ptype: u32,
    mime: &'a [u8],
    desc: &'a [u8],
    width: u32,
    height: u32,
    data: &'a [u8],
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn picture_parse(b: &[u8]) -> Option<Picture<'_>> {
    let ptype = be32(b, 0)?;
    let ml = be32(b, 4)? as usize;
    let mime = b.get(8..8 + ml)?;
    let mut i = 8 + ml;
    let dl = be32(b, i)? as usize;
    let desc = b.get(i + 4..i + 4 + dl)?;
    i += 4 + dl;
    let (width, height) = (be32(b, i)?, be32(b, i + 4)?);
    let n = be32(b, i + 16)? as usize;
    let data = b.get(i + 20..i + 20 + n)?;
    Some(Picture { ptype, mime, desc, width, height, data })
}

/// A PICTURE block with new image data. Colour depth and palette size are written as 0
/// (unknown): an optimized PNG may have a different depth or a palette.
fn picture_build(p: &Picture, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    for v in [p.ptype, p.mime.len() as u32] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(p.mime);
    out.extend_from_slice(&(p.desc.len() as u32).to_be_bytes());
    out.extend_from_slice(p.desc);
    for v in [p.width, p.height, 0, 0, data.len() as u32] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(data);
    out
}

/// The comments of a VORBIS_COMMENT block, without the vendor string (an encoder rewrites it).
fn vorbis_comments(body: &[u8]) -> Option<&[u8]> {
    let vl = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    body.get(4 + vl..)
}

fn vendor(blocks: &[(u8, Vec<u8>)]) -> &[u8] {
    blocks
        .iter()
        .find(|b| b.0 == VORBIS_COMMENT)
        .and_then(|(_, b)| {
            let vl = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
            b.get(4..4 + vl)
        })
        .unwrap_or_default()
}

fn flac(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, AudioError> {
    let Some(orig) = flac_read(src)? else {
        return skip("not a native FLAC (Ogg FLAC, an ID3 header or damaged): skipped");
    };
    let Some(orig_audio) = audio_hash(ctx, src)? else { return skip("its audio could not be decoded") };
    let mut best = src.to_path_buf();
    let mut best_size = std::fs::metadata(src)?.len();
    let mut steps = Vec::new();

    // 1) cover art; the audio frames are copied byte for byte.
    let (mut new_blocks, mut n_art, mut art_steps) = (Vec::new(), 0, BTreeSet::new());
    for (i, (typ, body)) in orig.blocks.iter().enumerate() {
        let mut body = body.clone();
        if *typ == PICTURE {
            if let Some(p) = picture_parse(&body) {
                if let Some((img, s)) = optimize_art(ctx, p.data, &work.join(format!("art{i}")))? {
                    body = picture_build(&p, &img);
                    n_art += 1;
                    art_steps.extend(s);
                }
            }
        }
        new_blocks.push((*typ, body));
    }
    if n_art > 0 {
        let out = work.join("art.flac");
        write_with_tail(&out, &flac_header(&new_blocks), src, orig.audio_at)?;
        let size = std::fs::metadata(&out)?.len();
        if size < best_size && flac_art_ok(ctx, &orig, &orig_audio, src, &out)? {
            (best, best_size) = (out, size);
            steps.push(art_label(n_art, &art_steps));
        }
    }

    // 2) re-encoding: worthwhile only for files from older or other encoders.
    let modern = [b"reference libFLAC 1.4".as_slice(), b"reference libFLAC 1.5"]
        .iter()
        .any(|v| vendor(&orig.blocks).starts_with(v));
    if !modern {
        if let Some(flac) = ctx.tools.get("flac") {
            let out = work.join("enc.flac");
            let args = crate::os_args![
                "-8", "-p", "-A", "subdivide_tukey(5)", "--verify", "--silent", "-f", "-o", &out, &best
            ];
            let ran = tools::run(flac, args, Some(work), Capture::None, ctx.timeout, &ctx.cancel)?;
            if ran.success {
                let size = std::fs::metadata(&out).map_or(u64::MAX, |m| m.len());
                if size < best_size && flac_encode_ok(ctx, &orig_audio, &best, &out)? {
                    best = out;
                    steps.push("flac-8p".to_string());
                }
            }
        }
    }
    let best = (best != src).then_some(best);
    Ok(Optimized { best, steps, note: None })
}

/// The new file has the same blocks except for picture data, the same audio bytes, and
/// still decodes to the same audio.
fn flac_art_ok(ctx: &Ctx, orig: &Flac, orig_audio: &[u8; 32], src: &Path, out: &Path) -> Result<bool, AudioError> {
    let Some(new) = flac_read(out)? else { return Ok(false) };
    let same_blocks = new.blocks.len() == orig.blocks.len()
        && new.blocks.iter().zip(&orig.blocks).all(|(n, o)| {
            n.0 == o.0
                && if n.0 == PICTURE {
                    match (picture_parse(&n.1), picture_parse(&o.1)) {
                        (Some(a), Some(b)) => a.ptype == b.ptype && a.mime == b.mime && a.desc == b.desc && a.width == b.width && a.height == b.height,
                        _ => n.1 == o.1,
                    }
                } else {
                    n.1 == o.1
                }
        });
    Ok(same_blocks && same_tail(src, orig.audio_at, out, new.audio_at)? && audio_hash(ctx, out)?.as_ref() == Some(orig_audio))
}

/// A re-encoded file must decode to the same audio and keep the tags and the CUESHEET,
/// APPLICATION and PICTURE blocks exactly.
fn flac_encode_ok(ctx: &Ctx, orig_audio: &[u8; 32], before: &Path, out: &Path) -> Result<bool, AudioError> {
    let (Some(a), Some(b)) = (flac_read(before)?, flac_read(out)?) else { return Ok(false) };
    let kept = |f: &Flac| -> Vec<(u8, Vec<u8>)> { f.blocks.iter().filter(|b| matches!(b.0, 2 | 5 | 6)).cloned().collect() };
    let tags = |f: &Flac| -> Option<Vec<u8>> {
        f.blocks.iter().find(|b| b.0 == VORBIS_COMMENT).map(|b| vorbis_comments(&b.1).unwrap_or_default().to_vec())
    };
    Ok(kept(&a) == kept(&b) && tags(&a) == tags(&b) && audio_hash(ctx, out)?.as_ref() == Some(orig_audio))
}

// ---------------------------------------------------------------- MP3

fn syncsafe(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 21) | (u32::from(b[1]) << 14) | (u32::from(b[2]) << 7) | u32::from(b[3])
}

fn to_syncsafe(n: u32) -> [u8; 4] {
    [(n >> 21) as u8 & 0x7f, (n >> 14) as u8 & 0x7f, (n >> 7) as u8 & 0x7f, n as u8 & 0x7f]
}

/// Cover art (APIC frames) in an ID3v2.3/2.4 tag. The MP3 audio stream and all other tag
/// frames are copied byte for byte.
fn mp3(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, AudioError> {
    let mut f = File::open(src)?;
    let mut head = [0u8; 10];
    if read_full(&mut f, &mut head)? != 10 || &head[..3] != b"ID3" || !matches!(head[3], 3 | 4) {
        return Ok(Optimized { best: None, steps: Vec::new(), note: None }); // no ID3v2.3/2.4 tag: no art
    }
    let (ver, flags) = (head[3], head[5]);
    if flags & 0xd0 != 0 {
        return skip("unusual ID3 layout (unsynchronisation, extended header or footer): skipped");
    }
    let size = u64::from(syncsafe(&head[6..10]));
    if size > MAX_META {
        return skip("ID3 tag too large: skipped");
    }
    let tag_end = 10 + size;
    let mut tag = vec![0u8; size as usize];
    if read_full(&mut f, &mut tag)? != tag.len() {
        return skip("damaged ID3 tag: skipped");
    }
    drop(f);

    let mut frames: Vec<(&[u8], &[u8], Vec<u8>)> = Vec::new();
    let mut i = 0;
    while i + 10 <= tag.len() && tag[i..i + 4] != [0, 0, 0, 0] {
        let fs = if ver == 4 { syncsafe(&tag[i + 4..i + 8]) } else { u32::from_be_bytes(tag[i + 4..i + 8].try_into().unwrap()) } as usize;
        if fs == 0 || i + 10 + fs > tag.len() {
            return skip("damaged ID3 tag: skipped");
        }
        frames.push((&tag[i..i + 4], &tag[i + 8..i + 10], tag[i + 10..i + 10 + fs].to_vec()));
        i += 10 + fs;
    }
    let padding = tag.len() - i;

    let (mut n_art, mut art_steps) = (0, BTreeSet::new());
    for (k, (fid, fflags, body)) in frames.iter_mut().enumerate() {
        if *fid != b"APIC" || *fflags != [0, 0] || body.len() <= MIN_ART {
            continue;
        }
        let Some(start) = apic_data_start(body) else { continue };
        if let Some((img, s)) = optimize_art(ctx, &body[start..], &work.join(format!("art{k}")))? {
            body.truncate(start);
            body.extend_from_slice(&img);
            n_art += 1;
            art_steps.extend(s);
        }
    }
    if n_art == 0 {
        return Ok(Optimized { best: None, steps: Vec::new(), note: None });
    }
    let mut new_tag = Vec::with_capacity(tag.len());
    for (fid, fflags, body) in &frames {
        new_tag.extend_from_slice(fid);
        let n = body.len() as u32;
        new_tag.extend_from_slice(&if ver == 4 { to_syncsafe(n) } else { n.to_be_bytes() });
        new_tag.extend_from_slice(fflags);
        new_tag.extend_from_slice(body);
    }
    new_tag.resize(new_tag.len() + padding, 0);
    let mut header = head[..6].to_vec();
    header.extend_from_slice(&to_syncsafe(new_tag.len() as u32));
    header.extend_from_slice(&new_tag);
    let out = work.join("out.mp3");
    write_with_tail(&out, &header, src, tag_end)?;

    // Same audio bytes after the tag, and the same frames apart from picture data.
    let ok = std::fs::metadata(&out)?.len() < std::fs::metadata(src)?.len()
        && same_tail(src, tag_end, &out, header.len() as u64)?
        && mp3_frames_match(&tag, &new_tag, ver);
    let best = ok.then_some(out);
    let steps = if ok { vec![art_label(n_art, &art_steps)] } else { Vec::new() };
    Ok(Optimized { best, steps, note: None })
}

/// Offset of the picture data in an APIC frame body:
/// encoding, MIME type `\0`, picture type, description (terminated by `\0` or, for UTF-16,
/// by `\0\0` on an even offset), data.
fn apic_data_start(body: &[u8]) -> Option<usize> {
    let enc = *body.first()?;
    let m_end = 1 + body.get(1..)?.iter().position(|&b| b == 0)?;
    let desc = m_end + 2;
    if desc > body.len() {
        return None;
    }
    let end = if matches!(enc, 1 | 2) {
        let mut j = desc;
        while j + 1 < body.len() && body[j..j + 2] != [0, 0] {
            j += 2;
        }
        j + 2
    } else {
        desc + body.get(desc..)?.iter().position(|&b| b == 0)? + 1
    };
    (end <= body.len()).then_some(end)
}

fn id3_frames(tag: &[u8], ver: u8) -> Vec<(&[u8], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 10 <= tag.len() && tag[i..i + 4] != [0, 0, 0, 0] {
        let fs = if ver == 4 { syncsafe(&tag[i + 4..i + 8]) } else { u32::from_be_bytes(tag[i + 4..i + 8].try_into().unwrap()) } as usize;
        if i + 10 + fs > tag.len() {
            break;
        }
        out.push((&tag[i..i + 4], &tag[i + 10..i + 10 + fs]));
        i += 10 + fs;
    }
    out
}

fn mp3_frames_match(old: &[u8], new: &[u8], ver: u8) -> bool {
    let (a, b) = (id3_frames(old, ver), id3_frames(new, ver));
    a.len() == b.len()
        && a.iter().zip(&b).all(|((fa, ba), (fb, bb))| {
            fa == fb
                && if *fa == b"APIC" {
                    match (apic_data_start(ba), apic_data_start(bb)) {
                        (Some(x), Some(y)) => ba[..x] == bb[..y] && sniff(&ba[x..]) == sniff(&bb[y..]),
                        _ => ba == bb,
                    }
                } else {
                    ba == bb
                }
        })
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
        (ffmpeg.exists() && ["oxipng", "flac"].iter().all(|n| t.has(n))).then(|| Ctx {
            tools: t,
            lossy: false,
            target: 93.0,
            lossy_min_gain: 0.03,
            strip: false,
            strict_alpha: false,
            parallel: false,
            timeout: Duration::from_secs(120),
            cancel: CancellationToken::new(),
        })
    }

    fn ff(ctx: &Ctx, args: &[&str]) {
        let ok = std::process::Command::new(&ctx.tools.ffmpeg).args(["-v", "error", "-y"]).args(args).status().unwrap().success();
        assert!(ok, "ffmpeg {args:?}");
    }

    /// A FLAC and an MP3 with a badly compressed PNG cover: the cover gets smaller, the
    /// audio stays byte-identical, and an old-encoder FLAC gets re-encoded.
    #[test]
    fn cover_art_and_reencode() {
        let Some(c) = ctx() else {
            eprintln!("skipping: audio tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-audio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let cover = dir.join("cover.png");
        ff(&c, &["-f", "lavfi", "-i", "mandelbrot=s=300x300", "-frames:v", "1", "-compression_level", "0", &s(&cover)]);
        let tone = ["-f", "lavfi", "-i", "sine=frequency=440:duration=5"];
        let flac = dir.join("song.flac");
        // ffmpeg's encoder (vendor "Lavf"), level 0: an "older encoder" the re-encode targets.
        ff(&c, &[&tone[..], &["-i", &s(&cover), "-map", "0:a", "-map", "1:v", "-c:a", "flac", "-compression_level", "0",
            "-c:v", "copy", "-disposition:v", "attached_pic", "-metadata", "title=Test", &s(&flac)]].concat());
        let mp3 = dir.join("song.mp3");
        ff(&c, &[&tone[..], &["-i", &s(&cover), "-map", "0:a", "-map", "1:v", "-c:a", "libmp3lame", "-c:v", "copy",
            "-id3v2_version", "3", "-metadata", "title=Test", &s(&mp3)]].concat());

        for (src, kind) in [(&flac, Kind::Flac), (&mp3, Kind::Mp3)] {
            let work = dir.join(format!("w-{kind:?}"));
            std::fs::create_dir_all(&work).unwrap();
            let before = std::fs::metadata(src).unwrap().len();
            let r = optimize(&c, kind, src, &work).unwrap();
            let best = r.best.unwrap_or_else(|| panic!("{kind:?}: no gain ({:?})", r.note));
            let after = std::fs::metadata(&best).unwrap().len();
            println!("{kind:?}: {before} -> {after} {:?}", r.steps);
            assert!(after < before);
            assert!(r.steps[0].starts_with("cover-art[1:"), "{:?}", r.steps);
            assert_eq!(audio_hash(&c, src).unwrap(), audio_hash(&c, &best).unwrap(), "{kind:?}: audio changed");
            if kind == Kind::Flac {
                assert!(r.steps.iter().any(|s| s == "flac-8p"), "{:?}", r.steps);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
