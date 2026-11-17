//! Audio (FLAC, MP3, WAV), ported from imageopti's `handle_flac`, `handle_mp3` and
//! `handle_wav`.
//!
//! - **Cover art**: embedded pictures (FLAC PICTURE blocks, ID3v2 APIC frames) are optimized
//!   by the image pipeline (pixel-identical, or SSIMULACRA2 >= 93 in lossy mode). Everything
//!   else (audio frames, tags, other blocks) is copied byte for byte and checked to be.
//! - **FLAC re-encoding** (`-8 -p -A subdivide_tukey(5)`, plus `-e` at max effort): only for
//!   files from encoders older than libFLAC 1.4 (every file at max effort), accepted only when
//!   the decoded audio, the tags and the CUESHEET, APPLICATION and PICTURE blocks are unchanged.
//! - **WAV -> FLAC** (a conversion: the extension changes): integer PCM only, accepted when
//!   the FLAC decodes to the same samples with the same channels, rate and bit depth.
//!
//! Audio data is streamed, never read into memory whole (hi-res albums can be huge).

use crate::image::tools::{self, Capture, RunError};
use crate::image::{self, Ctx, Kind, Optimized};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// An audio file is replaced when the new version is at least this much smaller (or 0.5%).
pub const MIN_GAIN: u64 = 64 * 1024;

/// Whether `after` bytes is enough smaller than `before` to replace the file: checked before
/// the slow verification (decoding the whole audio twice), so it isn't done for nothing.
pub fn worth_keeping(before: u64, after: u64) -> bool {
    before.saturating_sub(after) >= (before / 200).clamp(1, MIN_GAIN)
}

/// Tags or metadata bigger than this are left alone (they are read into memory).
const MAX_META: u64 = 64 * 1024 * 1024;
/// Pictures smaller than this aren't worth optimizing.
const MIN_ART: usize = 1024;

const PICTURE: u8 = 6;
const PADDING: u8 = 1;
const VORBIS_COMMENT: u8 = 4;

pub fn optimize(ctx: &Ctx, kind: Kind, src: &Path, work: &Path) -> Result<Optimized, RunError> {
    let r = match kind {
        Kind::Flac => flac(ctx, src, work),
        Kind::Mp3 => mp3(ctx, src, work),
        Kind::Wav => wav(ctx, src, work),
        Kind::Ogg => ogg(ctx, src, work),
        Kind::LosslessAudio => lossless_to_flac(ctx, src, work),
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
    Ok(Optimized::skipped(why))
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
    ctx.phase("cover art", &[]);
    let r = image::optimize(&Ctx { progress: None, ..ctx.clone() }, kind, &file, work)?;
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
pub(crate) fn audio_hash(ctx: &Ctx, path: &Path) -> Result<Option<[u8; 32]>, RunError> {
    let args = crate::os_args!["-v", "error", "-i", path, "-map", "0:a:0", "-f", "s32le", "-"];
    let ran = tools::run(&ctx.tools.ffmpeg, args, None, Capture::Hash, audio_timeout(ctx, path), &ctx.gate)?;
    Ok(ran.hash.filter(|_| ran.success))
}

/// Whether two audio files carry the same tags and as many cover pictures (their bytes may
/// differ: an optimized cover is the same cover). A file that isn't a native FLAC counts as
/// having none.
pub(crate) fn same_tags_and_art(ctx: &Ctx, a: &Path, b: &Path) -> Result<bool, RunError> {
    let pictures = |p: &Path| flac_read(p).ok().flatten().map_or(0, |f| f.blocks.iter().filter(|(t, _)| *t == 6).count());
    if pictures(a) != pictures(b) {
        return Ok(false);
    }
    let theirs = tags(ctx, b)?;
    Ok(theirs.is_some() && tags(ctx, a)? == theirs)
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

/// flac's encoding settings (imageopti's: `-e` at max effort).
fn flac_level(ctx: &Ctx) -> Vec<std::ffi::OsString> {
    let mut a = crate::os_args!["-8", "-p", "-A", "subdivide_tukey(5)"];
    if ctx.max.flac_e {
        a.push("-e".into());
    }
    a
}

/// Time allowed to encode or decode `path`: the usual limit, or more for big files (a
/// hi-res album as one file can take a long time at `-8 -p -e`).
fn audio_timeout(ctx: &Ctx, path: &Path) -> std::time::Duration {
    let bytes_per_sec: u64 = if ctx.max.flac_e { 256 * 1024 } else { 1024 * 1024 };
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    ctx.timeout.max(std::time::Duration::from_secs(size / bytes_per_sec))
}

/// Whether `out` decodes to the same audio as `src`. The original's decode (kept in `slot`) and
/// the candidate's run at the same time: each is one core, and a long album takes a while.
fn same_audio(ctx: &Ctx, src: &Path, slot: &mut Option<Option<[u8; 32]>>, out: &Path) -> Result<bool, RunError> {
    let (want, got) = if let Some(cached) = *slot {
        ctx.phase("verifying", &[out]);
        (cached, audio_hash(ctx, out)?)
    } else {
        ctx.phase("verifying", &[src, out]);
        let (a, b) = std::thread::scope(|s| {
            let reference = s.spawn(|| audio_hash(ctx, src));
            let candidate = audio_hash(ctx, out);
            (reference.join().unwrap_or(Ok(None)), candidate)
        });
        let want = a?;
        *slot = Some(want);
        (want, b?)
    };
    Ok(want.is_some() && got == want)
}

/// Written by libFLAC 1.4 or later: re-encoding gains almost nothing (about 0.1%).
fn is_modern(f: &Flac) -> bool {
    [b"reference libFLAC 1.4".as_slice(), b"reference libFLAC 1.5"].iter().any(|v| vendor(&f.blocks).starts_with(v))
}

/// A cheap look at the original's header: is there anything to try? Most music files have
/// no cover art (or a tiny one) and a modern encoder; they are not copied or decoded.
pub fn worth_trying(ctx: &Ctx, kind: Kind, path: &Path) -> bool {
    let has_art = |data: &[u8]| data.len() >= MIN_ART && sniff(data).is_some();
    match kind {
        Kind::Flac => flac_read(path).ok().flatten().is_some_and(|f| {
            ((ctx.max.flac_all || !is_modern(&f)) && ctx.tools.has("flac"))
                || f.blocks.iter().any(|(t, b)| *t == PICTURE && picture_parse(b).is_some_and(|p| has_art(p.data)))
        }),
        Kind::Mp3 => mp3_tag(path).ok().flatten().is_some_and(|tag| {
            id3_frames(&tag.body, tag.ver).iter().any(|(id, b)| *id == b"APIC" && apic_data_start(b).is_some_and(|i| has_art(&b[i..])))
        }),
        Kind::Wav => ctx.tools.has("flac") && wav_format(path).ok().flatten().is_some(),
        Kind::Ogg => ctx.tools.has("optivorbis") && ogg_info(path).ok().flatten().is_some(),
        _ => true,
    }
}

// ---------------------------------------------------------------- Ogg Vorbis

/// What a repacked Ogg Vorbis must keep.
#[derive(Debug, PartialEq, Eq)]
struct OggInfo {
    channels: u8,
    sample_rate: u32,
    /// Declared length: the last page's granule position (in samples).
    length: u64,
    /// The comment packet after its vendor string (tags, embedded pictures).
    comments: Vec<u8>,
}

/// Reads an Ogg file holding a single Vorbis stream (not Opus, FLAC, video or chained
/// streams): its format, declared length and tags. The audio pages are skipped, not read.
fn ogg_info(path: &Path) -> io::Result<Option<OggInfo>> {
    let mut f = BufReader::new(File::open(path)?);
    let (mut packets, mut current): (Vec<Vec<u8>>, Vec<u8>) = (Vec::new(), Vec::new());
    let (mut serial, mut length) = (None, 0u64);
    loop {
        let mut h = [0u8; 27];
        match read_full(&mut f, &mut h)? {
            0 => break,
            27 if &h[..4] == b"OggS" && h[4] == 0 => {}
            _ => return Ok(None),
        }
        let page_serial = u32::from_le_bytes(h[14..18].try_into().unwrap());
        if *serial.get_or_insert(page_serial) != page_serial {
            return Ok(None); // several streams
        }
        let granule = u64::from_le_bytes(h[6..14].try_into().unwrap());
        if granule != u64::MAX {
            length = granule;
        }
        let mut lacing = vec![0u8; usize::from(h[26])];
        if read_full(&mut f, &mut lacing)? != lacing.len() {
            return Ok(None);
        }
        if packets.len() >= 2 {
            let body: i64 = lacing.iter().map(|&l| i64::from(l)).sum();
            f.seek_relative(body)?;
            continue;
        }
        for l in lacing {
            let mut seg = vec![0u8; usize::from(l)];
            if read_full(&mut f, &mut seg)? != seg.len() {
                return Ok(None);
            }
            if packets.len() < 2 {
                current.extend_from_slice(&seg);
                if current.len() as u64 > MAX_META {
                    return Ok(None);
                }
                if l < 255 {
                    packets.push(std::mem::take(&mut current));
                }
            }
        }
    }
    let [ident, comment] = &packets[..] else { return Ok(None) };
    if !ident.starts_with(b"\x01vorbis") || ident.len() < 16 || !comment.starts_with(b"\x03vorbis") {
        return Ok(None);
    }
    let vendor_len = comment.get(7..11).map_or(usize::MAX, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let Some(comments) = comment.get(11usize.saturating_add(vendor_len)..) else { return Ok(None) };
    Ok(Some(OggInfo {
        channels: ident[11],
        sample_rate: u32::from_le_bytes(ident[12..16].try_into().unwrap()),
        length,
        comments: comments.to_vec(),
    }))
}

/// SHA-256 and byte count of the decoded audio (32-bit float), optionally cut after
/// `end_sample` samples.
fn decoded(ctx: &Ctx, path: &Path, end_sample: Option<u64>) -> Result<Option<([u8; 32], u64)>, RunError> {
    let mut args = crate::os_args!["-v", "error", "-i", path, "-map", "0:a:0"];
    if let Some(n) = end_sample {
        args.extend(crate::os_args!["-af", format!("atrim=end_sample={n}")]);
    }
    args.extend(crate::os_args!["-f", "f32le", "-"]);
    let ran = tools::run(&ctx.tools.ffmpeg, args, None, Capture::Hash, ctx.timeout, &ctx.gate)?;
    Ok(ran.hash.filter(|_| ran.success).map(|h| (h, ran.stdout_len)))
}

/// Lossless repacking with OptiVorbis (better codebooks, no wasted bits).
fn ogg(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, AudioError> {
    let Some(optivorbis) = ctx.tools.get("optivorbis") else { return skip("optivorbis is missing") };
    let Some(info) = ogg_info(src)? else { return skip("not a single Vorbis stream (Opus, FLAC, video or chained Ogg): skipped") };
    let out = work.join("out.ogg");
    ctx.phase("optimizing", &[src]);
    let ran = tools::run(optivorbis, crate::os_args!["-q", src, &out], Some(work), Capture::None, ctx.timeout, &ctx.gate)?;
    let before = std::fs::metadata(src).map_or(0, |s| s.len());
    let smaller = ran.success && std::fs::metadata(&out).is_ok_and(|m| worth_keeping(before, m.len()));
    if !smaller || ogg_info(&out)?.as_ref() != Some(&info) {
        return Ok(Optimized::nothing());
    }
    ctx.phase("verifying", &[src, &out]);
    let (Some((want, want_len)), Some((got, got_len))) = (decoded(ctx, src, None)?, decoded(ctx, &out, None)?) else {
        return Ok(Optimized::nothing());
    };
    // OptiVorbis drops samples past the declared end (the last granule position) that ffmpeg
    // still decodes: accept a short cut tail if everything before it is identical.
    const MAX_TAIL: u64 = 4 * 8192 * 8;
    let same = got == want
        || (got_len < want_len && want_len - got_len <= MAX_TAIL && {
            let frame = 4 * u64::from(info.channels.max(1));
            got_len % frame == 0 && decoded(ctx, src, Some(got_len / frame))?.is_some_and(|(h, n)| h == got && n == got_len)
        });
    Ok(if same { Optimized::found(Some(out), vec!["optivorbis".into()]) } else { Optimized::nothing() })
}

// ---------------------------------------------------------------- other lossless -> FLAC

/// What ffmpeg says about a file's streams.
#[derive(Debug, Default, PartialEq, Eq)]
struct StreamInfo {
    /// Codec of the (only) audio stream, e.g. "alac", "pcm_s24be".
    codec: String,
    /// "44100 Hz, stereo".
    rate_layout: String,
    /// Bits per sample (16, 24, 32), when stated.
    bits: Option<u32>,
    audio_streams: usize,
    /// Embedded pictures (attached cover art).
    pictures: usize,
    /// Video that isn't cover art.
    other_video: usize,
}

/// Codecs whose samples FLAC holds exactly.
const LOSSLESS_CODECS: &[&str] = &[
    "alac", "ape", "wavpack", "tta", "pcm_s16be", "pcm_s16le", "pcm_s24be", "pcm_s24le", "pcm_s32be", "pcm_s32le", "pcm_s8", "pcm_u8",
];

fn stream_info(ctx: &Ctx, path: &Path) -> Result<StreamInfo, RunError> {
    // Without an output ffmpeg exits with an error after describing the input: expected.
    let ran = tools::run(&ctx.tools.ffmpeg, crate::os_args!["-hide_banner", "-i", path], None, Capture::None, ctx.timeout, &ctx.gate)?;
    let text = String::from_utf8_lossy(&ran.stderr);
    let mut info = StreamInfo::default();
    for line in text.lines().filter(|l| l.trim_start().starts_with("Stream #")) {
        if let Some(rest) = line.split("Audio: ").nth(1) {
            info.audio_streams += 1;
            let parts: Vec<&str> = rest.split(", ").collect();
            info.codec = parts.first().and_then(|c| c.split_whitespace().next()).unwrap_or_default().to_string();
            info.rate_layout = parts.iter().skip(1).take(2).copied().collect::<Vec<_>>().join(", ");
            let fmt = parts.get(3).copied().unwrap_or_default();
            info.bits = if let Some(b) = fmt.split('(').nth(1).and_then(|b| b.split_whitespace().next()).and_then(|b| b.parse().ok()) {
                Some(b)
            } else if fmt.starts_with("s16") {
                Some(16)
            } else if fmt.starts_with("s32") {
                Some(32)
            } else if fmt.starts_with("u8") {
                Some(8)
            } else {
                None
            };
        } else if line.contains("Video: ") {
            if line.contains("(attached pic)") {
                info.pictures += 1;
            } else {
                info.other_video += 1;
            }
        }
    }
    Ok(info)
}

/// The tags of a file as ffmpeg reads them (lower-case keys), without the container's own
/// fields that a FLAC doesn't have.
fn tags(ctx: &Ctx, path: &Path) -> Result<Option<BTreeSet<(String, String)>>, RunError> {
    let ran = tools::run(&ctx.tools.ffmpeg, crate::os_args!["-v", "error", "-i", path, "-f", "ffmetadata", "-"], None, Capture::BytesUpTo(4 << 20), ctx.timeout, &ctx.gate)?;
    if !ran.success {
        return Ok(None);
    }
    const CONTAINER: &[&str] = &["major_brand", "minor_version", "compatible_brands", "encoder", "handler_name", "vendor_id", "creation_time"];
    let text = String::from_utf8_lossy(&ran.stdout);
    Ok(Some(
        text.lines()
            .filter(|l| !l.starts_with(';') && !l.starts_with('['))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .filter(|(k, _)| !CONTAINER.contains(&k.as_str()))
            .collect(),
    ))
}

/// ALAC, AIFF, APE, WavPack or TTA -> FLAC, kept only when the FLAC decodes to the same
/// samples (same rate, channels, bit depth) and keeps every tag and the cover art; the art is
/// then optimized like any FLAC's.
fn lossless_to_flac(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, AudioError> {
    let info = stream_info(ctx, src)?;
    if info.audio_streams != 1 || info.other_video > 0 {
        return skip("not a single audio stream (or has video): left alone");
    }
    if !LOSSLESS_CODECS.contains(&info.codec.as_str()) {
        return skip(&format!("{} is not lossless: left alone", if info.codec.is_empty() { "the audio" } else { &info.codec }));
    }
    let Some(bits) = info.bits.filter(|b| (8..=32).contains(b)) else { return skip("unknown bit depth: left alone") };
    let out = work.join("conv.flac");
    let mut args = crate::os_args!["-v", "error", "-y", "-i", src, "-map", "0:a:0", "-map", "0:v?", "-map_metadata", "0", "-c:a", "flac", "-compression_level", "12"];
    args.extend(crate::os_args!["-sample_fmt", if bits <= 16 { "s16" } else { "s32" }]);
    if bits > 16 && bits < 32 {
        args.extend(crate::os_args!["-bits_per_raw_sample", bits.to_string()]);
    }
    args.extend(crate::os_args!["-c:v", "copy", "-disposition:v", "attached_pic", &out]);
    ctx.phase("converting", &[src]);
    if !tools::run(&ctx.tools.ffmpeg, args, None, Capture::None, audio_timeout(ctx, src), &ctx.gate)?.success || !out.is_file() {
        return skip("could not convert it to FLAC");
    }
    let got = stream_info(ctx, &out)?;
    let same_format = got.codec == "flac" && got.rate_layout == info.rate_layout && got.pictures == info.pictures
        && flac_read(&out)?.as_ref().and_then(flac_format).is_some_and(|f| f.bits == bits);
    if !same_format {
        return skip("the FLAC would not have the same format or cover art: kept");
    }
    ctx.phase("verifying", &[src, &out]);
    let want = audio_hash(ctx, src)?;
    if want.is_none() || audio_hash(ctx, &out)? != want {
        return skip("the FLAC does not decode to the same audio: kept");
    }
    let (before, after) = (tags(ctx, src)?, tags(ctx, &out)?);
    if !before.zip(after).is_some_and(|(b, a)| b.is_subset(&a)) {
        return skip("not every tag could be kept in FLAC: kept");
    }
    // The cover art is optimized like any FLAC's (the audio frames are copied as they are).
    let art_work = work.join("art");
    std::fs::create_dir_all(&art_work)?;
    let art = flac(ctx, &out, &art_work)?;
    let (best, extra) = match art.best {
        Some(b) => (b, art.steps),
        None => (out, Vec::new()),
    };
    let steps = std::iter::once(format!("{}->flac", info.codec)).chain(extra).collect();
    Ok(Optimized { new_ext: Some("flac"), ..Optimized::found(Some(best), steps) })
}

// ---------------------------------------------------------------- WAV -> FLAC

/// The layout of a WAV's samples: what a FLAC of it must declare.
#[derive(Debug, PartialEq, Eq)]
struct PcmFormat {
    channels: u32,
    sample_rate: u32,
    bits: u32,
}

/// The format of a RIFF WAV holding integer PCM (plain or WAVE_FORMAT_EXTENSIBLE), which
/// FLAC stores losslessly; `None` for anything else (float, ADPCM, RF64...).
fn wav_format(path: &Path) -> io::Result<Option<PcmFormat>> {
    let mut f = BufReader::new(File::open(path)?);
    let mut head = [0u8; 12];
    if read_full(&mut f, &mut head)? != 12 || &head[..4] != b"RIFF" || &head[8..] != b"WAVE" {
        return Ok(None);
    }
    // Chunks before "fmt " are rare and small; give up after a few.
    for _ in 0..16 {
        let mut ch = [0u8; 8];
        if read_full(&mut f, &mut ch)? != 8 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(ch[4..].try_into().unwrap());
        if &ch[..4] != b"fmt " {
            f.seek_relative(i64::from(len) + i64::from(len & 1))?;
            continue;
        }
        if !(16..=1024).contains(&len) {
            return Ok(None);
        }
        let mut b = vec![0u8; len as usize];
        if read_full(&mut f, &mut b)? != b.len() {
            return Ok(None);
        }
        let le16 = |i: usize| u32::from(u16::from_le_bytes([b[i], b[i + 1]]));
        let tag = if le16(0) == 0xfffe && b.len() >= 26 { le16(24) } else { le16(0) };
        let fmt = PcmFormat {
            channels: le16(2),
            sample_rate: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            bits: le16(14),
        };
        let ok = tag == 1 && (1..=8).contains(&fmt.channels) && (4..=32).contains(&fmt.bits) && fmt.sample_rate > 0;
        return Ok(ok.then_some(fmt));
    }
    Ok(None)
}

/// The ids of a WAV's chunks besides the format, the audio and padding.
fn wav_extra_chunks(path: &Path) -> io::Result<Vec<[u8; 4]>> {
    let mut f = BufReader::new(File::open(path)?);
    let len = f.get_ref().metadata()?.len();
    f.seek(SeekFrom::Start(12))?;
    let (mut at, mut extra) = (12u64, Vec::new());
    while at + 8 <= len {
        let mut ch = [0u8; 8];
        if read_full(&mut f, &mut ch)? != 8 {
            break;
        }
        let id: [u8; 4] = ch[..4].try_into().unwrap();
        if !matches!(&id, b"fmt " | b"data" | b"fact" | b"JUNK" | b"junk" | b"PAD ") {
            extra.push(id);
        }
        let size = u64::from(u32::from_le_bytes(ch[4..].try_into().unwrap()));
        at += 8 + size + (size & 1);
        f.seek(SeekFrom::Start(at))?;
    }
    Ok(extra)
}

/// Channels, sample rate and bits per sample from a FLAC's STREAMINFO.
fn flac_format(f: &Flac) -> Option<PcmFormat> {
    let b = f.blocks.first().filter(|b| b.0 == 0)?.1.get(10..14)?;
    let v = u32::from_be_bytes(b.try_into().ok()?);
    Some(PcmFormat { sample_rate: v >> 12, channels: ((v >> 9) & 7) + 1, bits: ((v >> 4) & 31) + 1 })
}

/// Integer PCM WAV -> FLAC, keeping the other RIFF chunks (`--keep-foreign-metadata`) when
/// flac can. Accepted only when the FLAC decodes to the same samples, in the same format.
fn wav(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, AudioError> {
    let Some(flac) = ctx.tools.get("flac") else { return skip("flac is missing") };
    let Some(format) = wav_format(src)? else {
        return skip("not integer PCM (float, ADPCM or another codec): not convertible losslessly");
    };
    let out = work.join("out.flac");
    let mut encoded = false;
    // Without --keep-foreign-metadata, flac drops the other RIFF chunks (tags, cue points,
    // a game rip's loop points): that fallback is only for WAVs that have none.
    let plain_ok = wav_extra_chunks(src)?.is_empty();
    for keep in [true, false] {
        if !keep && !plain_ok {
            return skip("flac could not keep its extra RIFF chunks (tags, cue or loop points): kept as WAV");
        }
        let mut args = flac_level(ctx);
        if keep {
            args.push("--keep-foreign-metadata-if-present".into());
        }
        args.extend(crate::os_args!["--verify", "--silent", "-f", "-o", &out, src]);
        ctx.phase("converting", &[src]);
        if tools::run(flac, args, Some(work), Capture::None, audio_timeout(ctx, src), &ctx.gate)?.success && out.is_file() {
            encoded = true;
            break;
        }
    }
    if !encoded {
        return skip("flac could not encode it");
    }
    let same_format = flac_read(&out)?.as_ref().and_then(flac_format).is_some_and(|f| f == format);
    ctx.phase("verifying", &[src, &out]);
    let reference = audio_hash(ctx, src)?;
    if !same_format || reference.is_none() || audio_hash(ctx, &out)? != reference {
        return skip("the FLAC does not decode to the same audio: kept as WAV");
    }
    Ok(Optimized { new_ext: Some("flac"), ..Optimized::found(Some(out), vec!["wav->flac".into()]) })
}

fn flac(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, AudioError> {
    let Some(orig) = flac_read(src)? else {
        return skip("not a native FLAC (Ogg FLAC, an ID3 header or damaged): skipped");
    };
    // Decoding is the slow part: done only once a candidate exists.
    let mut orig_audio = None;
    let mut best = src.to_path_buf();
    let mut best_size = std::fs::metadata(src)?.len();
    let mut steps = Vec::new();

    // 1) cover art; the audio frames are copied byte for byte.
    let (mut new_blocks, mut n_art, mut art_steps) = (Vec::new(), 0, BTreeSet::new());
    for (i, (typ, body)) in orig.blocks.iter().enumerate() {
        if *typ == PADDING && ctx.strip {
            continue;
        }
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
    let unpadded = new_blocks.len() < orig.blocks.len();
    if n_art > 0 || unpadded {
        let out = work.join("art.flac");
        write_with_tail(&out, &flac_header(&new_blocks), src, orig.audio_at)?;
        let size = std::fs::metadata(&out)?.len();
        if worth_keeping(best_size, size) && flac_art_same_layout(&orig, src, &out, ctx.strip)? && same_audio(ctx, src, &mut orig_audio, &out)? {
            (best, best_size) = (out, size);
            steps.push(if n_art > 0 { art_label(n_art, &art_steps) } else { "no-padding".into() });
        }
    }

    // 2) re-encoding: worthwhile only for files from older or other encoders (any at max).
    // flac would drop an ID3v1 or APE tag appended after the audio (the art path copies it).
    if (ctx.max.flac_all || !is_modern(&orig)) && !tag_at_end(src)? {
        if let Some(flac) = ctx.tools.get("flac") {
            let out = work.join("enc.flac");
            let mut args = flac_level(ctx);
            if ctx.strip {
                args.push("--no-padding".into());
            }
            args.extend(crate::os_args!["--verify", "--silent", "-f", "-o", &out, &best]);
            ctx.phase("re-encoding", &[&best]);
            let ran =tools::run(flac, args, Some(work), Capture::None, audio_timeout(ctx, src), &ctx.gate)?;
            if ran.success {
                let size = std::fs::metadata(&out).map_or(u64::MAX, |m| m.len());
                let original = std::fs::metadata(src)?.len();
                if size < best_size && worth_keeping(original, size) && flac_encode_same_tags(&best, &out)? && same_audio(ctx, src, &mut orig_audio, &out)? {
                    best = out;
                    steps.push(if ctx.max.flac_e { "flac-8pe" } else { "flac-8p" }.to_string());
                }
            }
        }
    }
    let best = (best != src).then_some(best);
    Ok(Optimized::found(best, steps))
}

/// Whether an ID3v1 or APE tag follows the audio frames (some taggers append them to FLAC).
fn tag_at_end(path: &Path) -> io::Result<bool> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(160)))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    let id3v1 = tail.len() >= 128 && tail[tail.len() - 128..].starts_with(b"TAG");
    Ok(id3v1 || tail.windows(8).any(|w| w == b"APETAGEX"))
}

/// The new file has the same blocks except for picture data (and padding, when `strip`), and
/// the same audio bytes (the decode check follows separately).
fn flac_art_same_layout(orig: &Flac, src: &Path, out: &Path, strip: bool) -> Result<bool, AudioError> {
    let Some(new) = flac_read(out)? else { return Ok(false) };
    let kept: Vec<&(u8, Vec<u8>)> = orig.blocks.iter().filter(|b| !(strip && b.0 == PADDING)).collect();
    let same_blocks = new.blocks.len() == kept.len()
        && new.blocks.iter().zip(kept).all(|(n, o)| {
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
    Ok(same_blocks && same_tail(src, orig.audio_at, out, new.audio_at)?)
}

/// A re-encoded file must keep the tags and the CUESHEET, APPLICATION and PICTURE blocks
/// exactly (the decode check follows separately).
fn flac_encode_same_tags(before: &Path, out: &Path) -> Result<bool, AudioError> {
    let (Some(a), Some(b)) = (flac_read(before)?, flac_read(out)?) else { return Ok(false) };
    let kept = |f: &Flac| -> Vec<(u8, Vec<u8>)> { f.blocks.iter().filter(|b| matches!(b.0, 2 | 5 | 6)).cloned().collect() };
    let tags = |f: &Flac| -> Option<Vec<u8>> {
        f.blocks.iter().find(|b| b.0 == VORBIS_COMMENT).map(|b| vorbis_comments(&b.1).unwrap_or_default().to_vec())
    };
    Ok(kept(&a) == kept(&b) && tags(&a) == tags(&b))
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
struct Id3 {
    /// The 10-byte header.
    head: [u8; 10],
    ver: u8,
    body: Vec<u8>,
}

/// The ID3v2.3/2.4 tag at the start of a file, if it has a plain layout (no
/// unsynchronisation, extended header or footer: those are left alone).
fn mp3_tag(path: &Path) -> io::Result<Option<Id3>> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 10];
    if read_full(&mut f, &mut head)? != 10 || &head[..3] != b"ID3" || !matches!(head[3], 3 | 4) || head[5] & 0xd0 != 0 {
        return Ok(None);
    }
    let size = u64::from(syncsafe(&head[6..10]));
    if size > MAX_META {
        return Ok(None);
    }
    let mut body = vec![0u8; size as usize];
    if read_full(&mut f, &mut body)? != body.len() {
        return Ok(None);
    }
    Ok(Some(Id3 { head, ver: head[3], body }))
}

fn mp3(ctx: &Ctx, src: &Path, work: &Path) -> Result<Optimized, AudioError> {
    let Some(Id3 { head, ver, body: tag }) = mp3_tag(src)? else {
        return Ok(Optimized::nothing()); // no plain ID3v2.3/2.4 tag
    };
    let tag_end = 10 + tag.len() as u64;

    let mut frames: Vec<(&[u8], &[u8], Vec<u8>)> = Vec::new();
    let mut i = 0;
    while i + 10 <= tag.len() && tag[i..i + 4] != [0, 0, 0, 0] {
        let fs = if ver == 4 { syncsafe(&tag[i + 4..i + 8]) } else { u32::from_be_bytes(tag[i + 4..i + 8].try_into().unwrap()) } as usize;
        // Frame ids are A-Z/0-9; v2.4 sizes are syncsafe (some taggers write plain sizes,
        // which would be misread here). Anything unusual is left alone.
        let id_ok = tag[i..i + 4].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        let size_ok = ver != 4 || tag[i + 4..i + 8].iter().all(|b| b & 0x80 == 0);
        if fs == 0 || i + 10 + fs > tag.len() || !id_ok || !size_ok {
            return skip("unusual or damaged ID3 tag: skipped");
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
        return Ok(Optimized::nothing());
    }
    let mut new_tag = Vec::with_capacity(tag.len());
    for (fid, fflags, body) in &frames {
        new_tag.extend_from_slice(fid);
        let n = body.len() as u32;
        new_tag.extend_from_slice(&if ver == 4 { to_syncsafe(n) } else { n.to_be_bytes() });
        new_tag.extend_from_slice(fflags);
        new_tag.extend_from_slice(body);
    }
    new_tag.resize(new_tag.len() + if ctx.strip { 0 } else { padding }, 0);
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
    Ok(Optimized::found(best, steps))
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
            lossy_docs: false,
            parallel: false,
            timeout: Duration::from_secs(120),
            gate: crate::image::tools::Gate::default(),
            max: crate::image::Max::default(),
            slots: None,
            progress: None,
        })
    }

    fn ff(ctx: &Ctx, args: &[&str]) {
        let ok = std::process::Command::new(&ctx.tools.ffmpeg).args(["-v", "error", "-y"]).args(args).status().unwrap().success();
        assert!(ok, "ffmpeg {args:?}");
    }

    /// ALAC (with tags and cover art), 24-bit AIFF, WavPack and TTA become FLACs that decode to
    /// the same samples and keep the tags and the art; an .m4a holding AAC is left alone.
    #[test]
    fn lossless_audio_becomes_flac() {
        let Some(c) = ctx() else {
            skip_test!("tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-lossless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        ff(&c, &["-f", "lavfi", "-i", "testsrc=s=300x300", "-frames:v", "1", "-q:v", "2", &p("cover.jpg")]);
        let tone = ["-f", "lavfi", "-i", "sine=f=440:d=3"];
        ff(&c, &[&tone[..], &["-i", &p("cover.jpg"), "-map", "0:a", "-map", "1:v", "-c:a", "alac", "-c:v", "copy", "-disposition:v", "attached_pic",
            "-metadata", "title=Song", "-metadata", "artist=Band", "-metadata", "album=LP", "-metadata", "track=3", &p("a.m4a")]].concat());
        ff(&c, &[&tone[..], &["-c:a", "pcm_s24be", "-metadata", "title=T", &p("b.aiff")]].concat());
        ff(&c, &[&tone[..], &["-c:a", "wavpack", &p("c.wv")]].concat());
        ff(&c, &[&tone[..], &["-c:a", "tta", &p("d.tta")]].concat());
        ff(&c, &[&tone[..], &["-c:a", "aac", &p("e.m4a")]].concat());
        for (name, converted) in [("a.m4a", true), ("b.aiff", true), ("c.wv", true), ("d.tta", true), ("e.m4a", false)] {
            let src = dir.join(name);
            let work = dir.join(format!("w-{name}"));
            std::fs::create_dir_all(&work).unwrap();
            let r = optimize(&c, Kind::LosslessAudio, &src, &work).unwrap();
            if !converted {
                assert!(r.best.is_none() && r.note.as_deref().is_some_and(|n| n.contains("not lossless")), "{name}: {:?}", r.note);
                continue;
            }
            let out = r.best.unwrap_or_else(|| panic!("{name} not converted: {:?}", r.note));
            assert_eq!(r.new_ext, Some("flac"));
            assert_eq!(audio_hash(&c, &out).unwrap(), audio_hash(&c, &src).unwrap(), "{name}: samples");
            let (a, b) = (tags(&c, &src).unwrap().unwrap(), tags(&c, &out).unwrap().unwrap());
            assert!(a.is_subset(&b), "{name}: tags {a:?} vs {b:?}");
            let (si, so) = (stream_info(&c, &src).unwrap(), stream_info(&c, &out).unwrap());
            assert_eq!((si.bits, si.pictures, &si.rate_layout), (so.bits, so.pictures, &so.rate_layout), "{name}: format and art");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A game-rip WAV with loop points (`smpl` chunk) becomes a FLAC that keeps them: decoding
    /// it back with --keep-foreign-metadata gives the WAV byte for byte.
    #[test]
    fn wav_loop_points_survive_conversion() {
        let Some(c) = ctx() else {
            skip_test!("tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-wavsmpl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.wav");
        ff(&c, &["-f", "lavfi", "-i", "sine=f=440:d=2", "-c:a", "pcm_s16le", "-ar", "44100", plain.to_str().unwrap()]);
        // Append a smpl chunk (loop points) and fix the RIFF size.
        let mut wav = std::fs::read(&plain).unwrap();
        let mut smpl = b"smpl".to_vec();
        smpl.extend(60u32.to_le_bytes());
        smpl.extend([7u8; 60]);
        wav.extend(&smpl);
        let riff = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff.to_le_bytes());
        let src = dir.join("loop.wav");
        std::fs::write(&src, &wav).unwrap();
        assert!(wav_extra_chunks(&src).unwrap().contains(b"smpl"));
        assert!(wav_extra_chunks(&plain).unwrap().iter().all(|c| c == b"LIST"), "ffmpeg adds only LIST");

        let work = dir.join("w");
        std::fs::create_dir_all(&work).unwrap();
        let r = optimize(&c, Kind::Wav, &src, &work).unwrap();
        let flac = r.best.unwrap_or_else(|| panic!("not converted: {:?}", r.note));
        let back = dir.join("back.wav");
        let ok = std::process::Command::new(c.tools.get("flac").unwrap())
            .args(["-d", "--keep-foreign-metadata", "--silent", "-f", "-o"])
            .arg(&back)
            .arg(&flac)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_eq!(std::fs::read(&back).unwrap(), wav, "loop points and audio restored exactly");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A FLAC and an MP3 with a badly compressed PNG cover: the cover gets smaller, the
    /// audio stays byte-identical, and an old-encoder FLAC gets re-encoded.
    #[test]
    fn cover_art_and_reencode() {
        let Some(c) = ctx() else {
            skip_test!("audio tools not found");
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

    /// A modern FLAC with padding: left alone by default; with `strip`, the padding goes and
    /// the tags and samples stay.
    #[test]
    fn flac_padding_goes_only_when_stripping() {
        let Some(c) = ctx() else {
            skip_test!("audio tools not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-padding-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        ff(&c, &["-f", "lavfi", "-i", "sine=f=440:d=2", "-ac", "1", "-c:a", "pcm_s16le", wav.to_str().unwrap()]);
        let flac = dir.join("tone.flac");
        let ok = std::process::Command::new(c.tools.get("flac").unwrap())
            .args(["-8", "--padding=65536", "-T", "TITLE=Kept", "--silent", "-f", "-o"])
            .arg(&flac)
            .arg(&wav)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert!(flac_read(&flac).unwrap().unwrap().blocks.iter().any(|b| b.0 == PADDING));

        let work = dir.join("keep");
        std::fs::create_dir_all(&work).unwrap();
        let r = optimize(&c, Kind::Flac, &flac, &work).unwrap();
        assert!(r.best.is_none(), "nothing to do without strip: {:?}", r.steps);

        let c = Ctx { strip: true, ..c };
        let work = dir.join("strip");
        std::fs::create_dir_all(&work).unwrap();
        let r = optimize(&c, Kind::Flac, &flac, &work).unwrap();
        let best = r.best.unwrap_or_else(|| panic!("padding not removed: {:?}", r.note));
        assert_eq!(r.steps, ["no-padding"]);
        let out = flac_read(&best).unwrap().unwrap();
        assert!(out.blocks.iter().all(|b| b.0 != PADDING));
        assert_eq!(audio_hash(&c, &best).unwrap(), audio_hash(&c, &flac).unwrap());
        assert_eq!(tags(&c, &best).unwrap(), tags(&c, &flac).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// libvorbis output repacked by OptiVorbis: smaller, same format, tags and samples
    /// (or a verified cut tail); an Opus file in an .ogg is left alone.
    #[test]
    fn ogg_vorbis_repack() {
        let Some(c) = ctx().filter(|c| c.tools.has("optivorbis")) else {
            skip_test!("optivorbis not found");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mo-ogg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let ogg = dir.join("song.ogg");
        ff(&c, &["-f", "lavfi", "-i", "sine=frequency=440:duration=8", "-c:a", "libvorbis", "-q:a", "5",
            "-metadata", "title=Test", &s(&ogg)]);
        let opus = dir.join("opus.ogg");
        ff(&c, &["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "libopus", &s(&opus)]);
        assert!(ogg_info(&opus).unwrap().is_none() && !worth_trying(&c, Kind::Ogg, &opus), "Opus must be skipped");

        let info = ogg_info(&ogg).unwrap().expect("a Vorbis stream");
        assert_eq!((info.channels, info.sample_rate), (1, 44100));
        let work = dir.join("w");
        std::fs::create_dir_all(&work).unwrap();
        let r = optimize(&c, Kind::Ogg, &ogg, &work).unwrap();
        let best = r.best.unwrap_or_else(|| panic!("no gain ({:?})", r.note));
        println!("Ogg: {} -> {} {:?}", std::fs::metadata(&ogg).unwrap().len(), std::fs::metadata(&best).unwrap().len(), r.steps);
        assert_eq!(ogg_info(&best).unwrap(), Some(info), "format or tags changed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
