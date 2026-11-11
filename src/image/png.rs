//! PNG structure: chunk listing, metadata guards and restoring chunks that a tool dropped.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Chunks that change how the image looks: they must survive in every mode.
const DISPLAY: &[&str] = &["iCCP", "sRGB", "gAMA", "cHRM", "cICP", "mDCV", "cLLI", "eXIf", "acTL"];
/// Metadata chunks: they must survive unless metadata is stripped.
const META: &[&str] = &["tEXt", "iTXt", "zTXt", "pHYs", "tIME", "eXIf"];
/// Colour chunks, placed right after IHDR when restored.
const COLOUR: &[&str] = &["iCCP", "sRGB", "gAMA", "cHRM", "cICP", "mDCV", "cLLI", "sBIT"];

#[derive(Debug, Clone)]
pub struct PngInfo {
    pub bit_depth: u8,
    pub color_type: u8,
    pub chunks: HashSet<String>,
}

impl PngInfo {
    pub fn is_apng(&self) -> bool {
        self.chunks.contains("acTL")
    }

    pub fn has_alpha(&self) -> bool {
        matches!(self.color_type, 4 | 6) || self.chunks.contains("tRNS")
    }
}

/// Reads the chunk list without loading image data.
/// Width x height of a PNG in pixels, from its IHDR.
pub fn pixels(path: &Path) -> Option<u64> {
    let mut head = [0u8; 24];
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    (&head[..8] == SIGNATURE && &head[12..16] == b"IHDR").then(|| {
        u64::from(u32::from_be_bytes(head[16..20].try_into().unwrap())) * u64::from(u32::from_be_bytes(head[20..24].try_into().unwrap()))
    })
}

/// Where a PNG ends (just after its IEND chunk); `None` if damaged.
pub fn end_of_image(d: &[u8]) -> Option<usize> {
    if d.get(..8)? != SIGNATURE {
        return None;
    }
    let mut i = 8;
    loop {
        let len = u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?) as usize;
        let name = d.get(i + 4..i + 8)?;
        i = i.checked_add(12 + len)?;
        if name == b"IEND" {
            return (i <= d.len()).then_some(i);
        }
    }
}

pub fn info(path: &Path) -> Option<PngInfo> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut sig = [0u8; 8];
    f.read_exact(&mut sig).ok()?;
    if &sig != SIGNATURE {
        return None;
    }
    let mut info = PngInfo { bit_depth: 8, color_type: 6, chunks: HashSet::new() };
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
        if *c == "iCCP" && has("sRGB") {
            continue;
        }
        if (*c == "gAMA" || *c == "cHRM") && (has("sRGB") || has("iCCP")) {
            continue;
        }
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

/// Copies colour/metadata chunks that a tool dropped (beeim drops all of them) from the
/// original back into `out`: colour chunks right after IHDR, the rest before the first
/// IDAT. An RGB colour profile is not put on an image a tool turned grey (it would be
/// invalid); the metadata guard then rejects that step.
pub fn restore_chunks(orig: &Path, out: &Path, strip: bool) -> std::io::Result<()> {
    let o = std::fs::read(orig)?;
    let n = std::fs::read(out)?;
    if !o.starts_with(SIGNATURE) || !n.starts_with(SIGNATURE) || o.len() < 26 || n.len() < 26 {
        return Ok(());
    }
    let (oc, nc) = (chunks(&o), chunks(&n));
    let have: HashSet<&str> = nc.iter().map(|(t, _)| *t).collect();
    let profile_present = have.contains("iCCP") || have.contains("sRGB");
    let wanted = |t: &str| {
        let restorable = COLOUR.contains(&t) || t == "eXIf" || (!strip && META.contains(&t));
        restorable && !(profile_present && (t == "iCCP" || t == "sRGB"))
    };
    let missing: Vec<(&str, &[u8])> = oc.iter().copied().filter(|(t, _)| wanted(t) && !have.contains(t)).collect();
    if missing.is_empty() || nc.first().map(|c| c.0) != Some("IHDR") {
        return Ok(());
    }
    let (o_ct, n_ct) = (o[25], n[25]);
    if missing.iter().any(|(t, _)| *t == "iCCP") && matches!(o_ct, 2 | 3 | 6) && matches!(n_ct, 0 | 4) {
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
        std::fs::write(&a, png(6, &[("iCCP", b"prof\0\0x"), ("tEXt", b"Author\0me")])).unwrap();
        let i = info(&a).unwrap();
        assert_eq!((i.bit_depth, i.color_type), (8, 6));
        assert!(i.chunks.contains("iCCP") && i.has_alpha() && !i.is_apng());

        let bare = d.join("b.png");
        std::fs::write(&bare, png(6, &[])).unwrap();
        let b = info(&bare).unwrap();
        assert!(!meta_ok(&i, &b, false), "lost profile and text");
        assert!(!meta_ok(&i, &b, true), "profile is always required");
        let srgb = d.join("c.png");
        std::fs::write(&srgb, png(6, &[("sRGB", &[0])])).unwrap();
        assert!(meta_ok(&i, &info(&srgb).unwrap(), true), "sRGB replaces an sRGB profile");
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
}
