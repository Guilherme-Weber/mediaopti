//! PNG structure: chunk listing, metadata guards and restoring chunks that a tool dropped.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Chunks that change how the image looks: they must survive in every mode.
const DISPLAY: &[&str] = &["iCCP", "sRGB", "gAMA", "cHRM", "cICP", "mDCV", "cLLI", "acTL"];
/// Metadata chunks: they must survive unless metadata is stripped.
const META: &[&str] = &["tEXt", "iTXt", "zTXt", "pHYs", "tIME", "eXIf"];
/// Colour chunks, placed right after IHDR when restored.
const COLOUR: &[&str] = &["iCCP", "sRGB", "gAMA", "cHRM", "cICP", "mDCV", "cLLI", "sBIT"];

#[derive(Debug, Clone)]
pub struct PngInfo {
    pub bit_depth: u8,
    pub color_type: u8,
    pub chunks: HashSet<String>,
    /// An eXIf chunk with an orientation other than "normal": it changes how the picture is
    /// shown, so it survives even when metadata is stripped.
    pub rotated: bool,
    /// Whether the `iCCP` chunk describes a standard sRGB profile (only then may a tool
    /// replace `iCCP` with an `sRGB` chunk without shifting wide-gamut colours).
    pub srgb_iccp: bool,
}

/// Whether an `iCCP` chunk payload (`<name>\0<method><zlib-icc>`) identifies an sRGB profile.
/// Wide-gamut profiles (Display P3, Adobe RGB, ProPhoto, etc.) return `false` so replacing
/// them with an `sRGB` chunk is rejected by [`meta_ok`].
fn is_srgb_iccp(data: &[u8]) -> bool {
    let nul = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let name = String::from_utf8_lossy(&data[..nul]).to_ascii_lowercase();
    let looks_srgb = |s: &str| {
        (s.contains("srgb") || s.contains("iec61966-2.1") || s.contains("iec 61966-2.1"))
            && !s.contains("p3")
            && !s.contains("adobe")
            && !s.contains("prophoto")
            && !s.contains("2020")
    };
    if looks_srgb(&name) {
        return true;
    }
    // Some tools label the chunk "ICC Profile" or "Photoshop ICC profile": inspect the
    // decompressed ICC profile's `desc` tag bytes when small enough.
    if data.get(nul + 1) != Some(&0) {
        return false;
    }
    let mut decoder = flate2::read::ZlibDecoder::new(&data[nul + 2..]);
    let mut icc = Vec::new();
    if std::io::Read::take(&mut decoder, 128 * 1024).read_to_end(&mut icc).is_err() || icc.len() < 132 {
        return false;
    }
    let tag_count = u32::from_be_bytes([icc[128], icc[129], icc[130], icc[131]]) as usize;
    for t in 0..tag_count.min(64) {
        let entry = 132 + t * 12;
        let Some(rec) = icc.get(entry..entry + 12) else { break };
        if &rec[..4] == b"desc" {
            let off = u32::from_be_bytes([rec[4], rec[5], rec[6], rec[7]]) as usize;
            let len = u32::from_be_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize;
            if let Some(desc) = icc.get(off..off.saturating_add(len).min(icc.len())) {
                let text: String = desc.iter().copied().filter(|&b| (0x20..0x7f).contains(&b)).map(char::from).collect();
                return looks_srgb(&text.to_ascii_lowercase());
            }
        }
    }
    false
}

/// Whether eXIf data turns the picture.
fn exif_rotates(data: &[u8]) -> bool {
    super::jpeg::tiff_orientation(data).unwrap_or(1) != 1
}

impl PngInfo {
    pub fn is_apng(&self) -> bool {
        self.chunks.contains("acTL")
    }

    pub fn has_alpha(&self) -> bool {
        matches!(self.color_type, 4 | 6) || self.chunks.contains("tRNS")
    }
}

/// Follows a PNG's chunks by seeking past their data (never loads IDAT into memory).
/// Returns `None` if not a valid PNG or cut off before `IEND`, `Some(true)` if any
/// non-zero byte follows `IEND`, and `Some(false)` if clean or zero-padded.
pub fn trailing_data(path: &Path) -> Option<bool> {
    let mut f = std::fs::File::open(path).ok()?;
    let total = f.metadata().ok()?.len();
    let mut sig = [0u8; 8];
    f.read_exact(&mut sig).ok()?;
    if &sig != SIGNATURE {
        return None;
    }
    let mut pos = 8u64;
    loop {
        let mut hdr = [0u8; 8];
        f.read_exact(&mut hdr).ok()?;
        let len = u64::from(u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]));
        pos = pos.checked_add(12)?.checked_add(len)?;
        if pos > total {
            return None;
        }
        if &hdr[4..8] == b"IEND" {
            if pos == total {
                return Some(false);
            }
            f.seek(SeekFrom::Start(pos)).ok()?;
            let mut buf = [0u8; 8192];
            loop {
                let n = f.read(&mut buf).ok()?;
                if n == 0 {
                    return Some(false);
                }
                if buf[..n].iter().any(|&b| b != 0) {
                    return Some(true);
                }
            }
        }
        f.seek(SeekFrom::Start(pos)).ok()?;
    }
}

/// Reads the chunk list without loading image data.
pub fn info(path: &Path) -> Option<PngInfo> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut sig = [0u8; 8];
    f.read_exact(&mut sig).ok()?;
    if &sig != SIGNATURE {
        return None;
    }
    let mut info = PngInfo { bit_depth: 8, color_type: 6, chunks: HashSet::new(), rotated: false, srgb_iccp: false };
    loop {
        let mut hdr = [0u8; 8];
        if f.read_exact(&mut hdr).is_err() {
            break;
        }
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as i64;
        let name = String::from_utf8_lossy(&hdr[4..8]).into_owned();
        if name == "IHDR" {
            let mut d = [0u8; 13];
            f.read_exact(&mut d).ok()?;
            info.bit_depth = d[8];
            info.color_type = d[9];
            f.seek(SeekFrom::Current(len - 13 + 4)).ok()?;
        } else if name == "eXIf" && (0..=1 << 20).contains(&len) {
            let mut d = vec![0u8; len as usize];
            f.read_exact(&mut d).ok()?;
            info.rotated = exif_rotates(&d);
            f.seek(SeekFrom::Current(4)).ok()?;
        } else if name == "iCCP" && (0..=1 << 20).contains(&len) {
            let mut d = vec![0u8; len as usize];
            f.read_exact(&mut d).ok()?;
            info.srgb_iccp = is_srgb_iccp(&d);
            f.seek(SeekFrom::Current(4)).ok()?;
        } else {
            f.seek(SeekFrom::Current(len + 4)).ok()?;
        }
        let end = name == "IEND";
        info.chunks.insert(name);
        if end {
            break;
        }
    }
    Some(info)
}

/// Whether `new` keeps what matters from `orig`: colour description always, metadata
/// unless `strip`. Equivalent colour descriptions are accepted (an sRGB iCCP may become
/// an sRGB chunk; gAMA/cHRM are ignored by decoders when sRGB or iCCP is present).
pub fn meta_ok(orig: &PngInfo, new: &PngInfo, strip: bool) -> bool {
    for c in DISPLAY.iter().filter(|c| orig.chunks.contains(**c)) {
        if new.chunks.contains(*c) {
            continue;
        }
        let has = |n: &str| new.chunks.contains(n);
        if *c == "iCCP" && orig.srgb_iccp && has("sRGB") {
            continue;
        }
        if (*c == "gAMA" || *c == "cHRM") && (has("sRGB") || has("iCCP")) {
            continue;
        }
        return false;
    }
    if orig.rotated && !new.chunks.contains("eXIf") {
        return false;
    }
    strip || META.iter().all(|c| !orig.chunks.contains(*c) || new.chunks.contains(*c))
}

/// Raw chunks `(type, length+type+data+crc)` of a PNG held in memory.
fn chunks(data: &[u8]) -> Vec<(&str, &[u8])> {
    let mut out = Vec::new();
    let mut i = 8;
    while i + 12 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        let end = (i + 12 + len).min(data.len());
        let name = std::str::from_utf8(&data[i + 4..i + 8]).unwrap_or("????");
        out.push((name, &data[i..end]));
        i = end;
        if name == "IEND" {
            break;
        }
    }
    out
}

/// Colour and metadata chunks extracted once from an original PNG (seeking past IDAT),
/// so restoring them across every lossless and lossy step never re-reads the original
/// image data. When `chunks` is empty, `apply` does no disk I/O at all.
pub struct Restorable {
    orig_color_type: u8,
    chunks: Vec<(String, Vec<u8>)>,
}

impl Restorable {
    /// Reads only the small colour/metadata chunks from `orig` (skipping IDAT).
    pub fn extract(orig: &Path, strip: bool) -> Self {
        let empty = Self { orig_color_type: 0, chunks: Vec::new() };
        let Ok(mut f) = std::fs::File::open(orig) else { return empty };
        let mut sig = [0u8; 8];
        if f.read_exact(&mut sig).is_err() || &sig != SIGNATURE {
            return empty;
        }
        let mut orig_color_type = 0u8;
        let mut chunks = Vec::new();
        loop {
            let mut hdr = [0u8; 8];
            if f.read_exact(&mut hdr).is_err() {
                break;
            }
            let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
            let name = String::from_utf8_lossy(&hdr[4..8]).into_owned();
            if name == "IHDR" && len >= 13 {
                let mut d = [0u8; 13];
                if f.read_exact(&mut d).is_err() {
                    break;
                }
                orig_color_type = d[9];
                if f.seek(SeekFrom::Current(len as i64 - 13 + 4)).is_err() {
                    break;
                }
            } else {
                let maybe_keep = COLOUR.contains(&name.as_str())
                    || name == "eXIf"
                    || (!strip && META.contains(&name.as_str()));
                if maybe_keep && len <= 64 << 20 {
                    let mut raw = vec![0u8; 8 + len + 4];
                    raw[..8].copy_from_slice(&hdr);
                    if f.read_exact(&mut raw[8..]).is_err() {
                        break;
                    }
                    let keep = if name == "eXIf" && strip {
                        exif_rotates(&raw[8..8 + len])
                    } else {
                        true
                    };
                    if keep {
                        chunks.push((name.clone(), raw));
                    }
                } else if f.seek(SeekFrom::Current(len as i64 + 4)).is_err() {
                    break;
                }
            }
            if name == "IEND" {
                break;
            }
        }
        Self { orig_color_type, chunks }
    }

    /// Copies whichever extracted chunks `out` is missing back into `out`.
    pub fn apply(&self, out: &Path) -> std::io::Result<()> {
        if self.chunks.is_empty() {
            return Ok(());
        }
        let n = std::fs::read(out)?;
        if !n.starts_with(SIGNATURE) || n.len() < 26 {
            return Ok(());
        }
        let nc = chunks(&n);
        let have: HashSet<&str> = nc.iter().map(|(t, _)| *t).collect();
        let profile_present = have.contains("iCCP") || have.contains("sRGB");
        let missing: Vec<(&str, &[u8])> = self
            .chunks
            .iter()
            .filter(|(t, _)| !(profile_present && (t == "iCCP" || t == "sRGB")) && !have.contains(t.as_str()))
            .map(|(t, raw)| (t.as_str(), raw.as_slice()))
            .collect();
        if missing.is_empty() || nc.first().map(|c| c.0) != Some("IHDR") {
            return Ok(());
        }
        let n_ct = n[25];
        if missing.iter().any(|(t, _)| *t == "iCCP") && matches!(self.orig_color_type, 2 | 3 | 6) && matches!(n_ct, 0 | 4) {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(n.len() + missing.iter().map(|c| c.1.len()).sum::<usize>());
        buf.extend_from_slice(&n[..8]);
        buf.extend_from_slice(nc[0].1);
        for (t, raw) in &missing {
            if COLOUR.contains(t) {
                buf.extend_from_slice(raw);
            }
        }
        let mut placed = false;
        for (t, raw) in &nc[1..] {
            if *t == "IDAT" && !placed {
                for (mt, mraw) in &missing {
                    if !COLOUR.contains(mt) {
                        buf.extend_from_slice(mraw);
                    }
                }
                placed = true;
            }
            buf.extend_from_slice(raw);
        }
        std::fs::write(out, buf)
    }
}

/// Copies colour/metadata chunks that a tool dropped (beeim drops all of them) from the
/// original back into `out`: colour chunks right after IHDR, the rest before the first
/// IDAT. An RGB colour profile is not put on an image a tool turned grey (it would be
/// invalid); the metadata guard then rejects that step.
#[cfg(test)]
pub fn restore_chunks(orig: &Path, out: &Path, strip: bool) -> std::io::Result<()> {
    Restorable::extract(orig, strip).apply(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for b in data {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    pub fn chunk(name: &str, data: &[u8]) -> Vec<u8> {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        let mut td = name.as_bytes().to_vec();
        td.extend_from_slice(data);
        c.extend_from_slice(&td);
        c.extend_from_slice(&crc32(&td).to_be_bytes());
        c
    }

    /// A minimal PNG made of the given chunks (IHDR first, IEND last).
    pub fn png(color_type: u8, extra_before_idat: &[(&str, &[u8])]) -> Vec<u8> {
        let mut ihdr = 1u32.to_be_bytes().to_vec();
        ihdr.extend(1u32.to_be_bytes());
        ihdr.extend([8, color_type, 0, 0, 0]);
        let mut p = SIGNATURE.to_vec();
        p.extend(chunk("IHDR", &ihdr));
        for (n, d) in extra_before_idat {
            p.extend(chunk(n, d));
        }
        p.extend(chunk("IDAT", &[0x78, 0x9C, 0x63, 0x60, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]));
        p.extend(chunk("IEND", &[]));
        p
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mo-png-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_chunks_and_guards_metadata() {
        let d = dir("guard");
        let a = d.join("a.png");
        std::fs::write(&a, png(6, &[("iCCP", b"sRGB IEC61966-2.1\0\0x"), ("tEXt", b"Author\0me")])).unwrap();
        let i = info(&a).unwrap();
        assert_eq!((i.bit_depth, i.color_type), (8, 6));
        assert!(i.chunks.contains("iCCP") && i.srgb_iccp && i.has_alpha() && !i.is_apng());

        let bare = d.join("b.png");
        std::fs::write(&bare, png(6, &[])).unwrap();
        let b = info(&bare).unwrap();
        assert!(!meta_ok(&i, &b, false), "lost profile and text");
        assert!(!meta_ok(&i, &b, true), "profile is always required");
        let srgb = d.join("c.png");
        std::fs::write(&srgb, png(6, &[("sRGB", &[0])])).unwrap();
        let srgb_info = info(&srgb).unwrap();
        assert!(meta_ok(&i, &srgb_info, true), "sRGB replaces an sRGB profile");

        // Wide-gamut iCCP (Display P3) must NOT be accepted when replaced by sRGB.
        let p3 = d.join("p3.png");
        std::fs::write(&p3, png(6, &[("iCCP", b"Display P3\0\0x")])).unwrap();
        let p3_info = info(&p3).unwrap();
        assert!(!p3_info.srgb_iccp);
        assert!(!meta_ok(&p3_info, &srgb_info, true), "Display P3 must not become sRGB");
        assert!(info(&d.join("missing.png")).is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn restores_dropped_chunks_in_the_right_places() {
        let d = dir("restore");
        let (o, n) = (d.join("o.png"), d.join("n.png"));
        std::fs::write(&o, png(2, &[("gAMA", &[0, 0, 177, 143]), ("pHYs", &[0; 9]), ("tEXt", b"k\0v")])).unwrap();
        std::fs::write(&n, png(2, &[])).unwrap();
        restore_chunks(&o, &n, false).unwrap();
        let data = std::fs::read(&n).unwrap();
        let names: Vec<&str> = chunks(&data).iter().map(|c| c.0).collect();
        assert_eq!(names, ["IHDR", "gAMA", "pHYs", "tEXt", "IDAT", "IEND"]);
        assert!(meta_ok(&info(&o).unwrap(), &info(&n).unwrap(), false));

        // strip: only colour comes back
        std::fs::write(&n, png(2, &[])).unwrap();
        restore_chunks(&o, &n, true).unwrap();
        let data = std::fs::read(&n).unwrap();
        let names: Vec<&str> = chunks(&data).iter().map(|c| c.0).collect();
        assert_eq!(names, ["IHDR", "gAMA", "IDAT", "IEND"]);

        // an RGB profile is not put on an image that became grey
        std::fs::write(&o, png(2, &[("iCCP", b"rgb\0\0x")])).unwrap();
        std::fs::write(&n, png(0, &[])).unwrap();
        restore_chunks(&o, &n, false).unwrap();
        assert!(!info(&n).unwrap().chunks.contains("iCCP"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn trailing_data_checks_png_without_reading_idat() {
        let d = dir("trailing");
        let base = png(2, &[]);
        let clean = d.join("clean.png");
        std::fs::write(&clean, &base).unwrap();
        assert_eq!(trailing_data(&clean), Some(false));

        let padded = d.join("padded.png");
        let mut p = base.clone();
        p.extend(std::iter::repeat_n(0u8, 64));
        std::fs::write(&padded, &p).unwrap();
        assert_eq!(trailing_data(&padded), Some(false));

        let extra = d.join("extra.png");
        let mut e = base.clone();
        e.extend_from_slice(b"PK\x03\x04");
        std::fs::write(&extra, &e).unwrap();
        assert_eq!(trailing_data(&extra), Some(true));

        let cut = d.join("cut.png");
        std::fs::write(&cut, &base[..base.len() - 4]).unwrap();
        assert_eq!(trailing_data(&cut), None);

        let other = d.join("not.png");
        std::fs::write(&other, b"hello").unwrap();
        assert_eq!(trailing_data(&other), None);
        let _ = std::fs::remove_dir_all(d);
    }
}
