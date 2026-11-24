//! JPEG structure: APPn/COM segments, EXIF orientation, metadata guards and the encoder
//! quality estimate.

use std::path::Path;

/// `(marker, payload)` of the APPn (0xE0-0xEF) and COM (0xFE) segments before the scan.
pub type Segments = Vec<(u8, Vec<u8>)>;

/// Safety cap on total APPn/COM header bytes read from a single JPEG (covers multi-MB
/// XMP/EXIF/ICC blocks without allowing a malformed file to exhaust memory).
const MAX_HEADER_BYTES: usize = 16 << 20;

/// Streams all APPn (0xE0-0xEF) and COM (0xFE) segments before the first scan (`SOS`),
/// seeking past other segments so large headers past 1 MB are never truncated.
pub fn segments(path: &Path) -> Option<Segments> {
    use std::io::{BufReader, Read, Seek, SeekFrom};
    let mut f = BufReader::new(std::fs::File::open(path).ok()?);
    let mut soi = [0u8; 2];
    f.read_exact(&mut soi).ok()?;
    if soi != [0xFF, 0xD8] {
        return None;
    }
    let mut segs = Vec::new();
    let mut total = 0usize;
    let mut byte = [0u8; 1];
    loop {
        if f.read_exact(&mut byte).is_err() {
            break;
        }
        if byte[0] != 0xFF {
            continue;
        }
        let mut m = 0xFFu8;
        while m == 0xFF {
            if f.read_exact(&mut byte).is_err() {
                return Some(segs);
            }
            m = byte[0];
        }
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            continue;
        }
        if m == 0xDA || m == 0xD9 {
            break;
        }
        let mut len_bytes = [0u8; 2];
        if f.read_exact(&mut len_bytes).is_err() {
            break;
        }
        let len = u16::from_be_bytes(len_bytes) as usize;
        if len < 2 {
            return None;
        }
        let payload_len = len - 2;
        if (0xE0..=0xEF).contains(&m) || m == 0xFE {
            total = total.saturating_add(payload_len);
            if total > MAX_HEADER_BYTES {
                return None;
            }
            let mut payload = vec![0u8; payload_len];
            f.read_exact(&mut payload).ok()?;
            segs.push((m, payload));
        } else if f.seek(SeekFrom::Current(payload_len as i64)).is_err() {
            break;
        }
    }
    Some(segs)
}

fn read_head(path: &Path, max: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    f.take(max as u64).read_to_end(&mut data).ok()?;
    Some(data)
}

#[cfg(test)]
fn parse_segments(data: &[u8]) -> Option<Segments> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut segs = Vec::new();
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = data[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        if m == 0xDA || m == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        // The length counts its own 2 bytes: below that the header is damaged.
        if len < 2 {
            return None;
        }
        let end = (i + 2 + len).min(data.len());
        if (0xE0..=0xEF).contains(&m) || m == 0xFE {
            segs.push((m, data[i + 4..end].to_vec()));
        }
        i = end;
    }
    Some(segs)
}

/// Identity of a segment: marker plus the first bytes up to a NUL (e.g. `Exif`, `ICC_PROFILE`).
fn seg_id(m: u8, d: &[u8]) -> (u8, Vec<u8>) {
    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len()).min(32);
    (m, d[..end].to_vec())
}

fn is_icc(m: u8, d: &[u8]) -> bool {
    m == 0xE2 && d.starts_with(b"ICC_PROFILE")
}

fn is_adobe(m: u8, d: &[u8]) -> bool {
    m == 0xEE && d.starts_with(b"Adobe")
}

pub fn exif_orientation(segs: &Segments) -> Option<u16> {
    segs.iter().find(|(m, d)| *m == 0xE1 && d.starts_with(b"Exif\0\0")).and_then(|(_, d)| tiff_orientation(&d[6..]))
}

/// The orientation tag of EXIF data (a TIFF header and its first IFD), as in JPEG's APP1
/// after "Exif\0\0" and in PNG's eXIf chunk.
pub fn tiff_orientation(t: &[u8]) -> Option<u16> {
    {
        let le = t.starts_with(b"II");
        let u16_at = |i: usize| -> Option<u16> {
            let b = t.get(i..i + 2)?;
            Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
        };
        let u32_at = |i: usize| -> Option<u32> {
            let b = t.get(i..i + 4)?;
            Some(if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) })
        };
        let ifd = u32_at(4)? as usize;
        let n = u16_at(ifd)? as usize;
        for k in 0..n {
            let e = ifd + 2 + 12 * k;
            if u16_at(e)? == 0x0112 {
                return u16_at(e + 8);
            }
        }
    }
    None
}

fn rotated(segs: &Segments) -> bool {
    exif_orientation(segs).unwrap_or(1) != 1
}

/// Whether `new` keeps what matters from `orig`: ICC profile and Adobe marker always,
/// EXIF when the photo is rotated (orientation must match), everything unless `strip`.
/// APP0/JFIF may change freely.
pub fn meta_ok(orig: &Segments, new: &Segments, strip: bool) -> bool {
    let ids = |s: &Segments| -> Vec<(u8, Vec<u8>)> {
        s.iter().filter(|(m, _)| *m != 0xE0).map(|(m, d)| seg_id(*m, d)).collect()
    };
    let (o, n) = (ids(orig), ids(new));
    if rotated(orig) && exif_orientation(new) != exif_orientation(orig) {
        return false;
    }
    let must: Vec<&(u8, Vec<u8>)> = if strip {
        o.iter()
            .filter(|(m, id)| {
                is_icc(*m, id) || is_adobe(*m, id) || (rotated(orig) && *m == 0xE1 && id.as_slice() == b"Exif")
            })
            .collect()
    } else {
        o.iter().collect()
    };
    must.iter().all(|s| n.contains(s))
}

/// Segments to carry over onto a re-encoded JPEG. The Adobe marker is never carried over:
/// the encoder writes its own header, and the old one could mislabel the colour transform.
pub fn keep_for_reencode(segs: &Segments, strip: bool) -> Segments {
    segs.iter()
        .filter(|(m, d)| *m != 0xE0 && !is_adobe(*m, d))
        .filter(|(m, d)| !strip || is_icc(*m, d) || (rotated(segs) && *m == 0xE1 && d.starts_with(b"Exif")))
        .cloned()
        .collect()
}

/// `segs` without the Adobe marker (to check re-encoded files, which drop it on purpose).
pub fn without_adobe(segs: &Segments) -> Segments {
    segs.iter().filter(|(m, d)| !is_adobe(*m, d)).cloned().collect()
}

/// The frame (SOF) marker of a JPEG: 0xC0 baseline, 0xC1 extended, 0xC2 progressive...
/// `None` if none comes before the first scan (damaged or not a JPEG).
pub fn frame_marker(data: &[u8]) -> Option<u8> {
    frame(data).map(|(m, _)| m)
}

/// Width and height from a JPEG's frame header.
pub fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let (_, at) = frame(data)?;
    // Length (2), precision (1), height (2), width (2).
    let h = u16::from_be_bytes([*data.get(at + 5)?, *data.get(at + 6)?]);
    let w = u16::from_be_bytes([*data.get(at + 7)?, *data.get(at + 8)?]);
    Some((u32::from(w), u32::from(h)))
}

/// Colour components in a JPEG's frame header: 1 grey, 3 YCbCr/RGB, 4 CMYK/YCCK.
pub fn components(data: &[u8]) -> Option<u8> {
    let (_, at) = frame(data)?;
    // Length (2), precision (1), height (2), width (2), components (1).
    data.get(at + 9).copied()
}

/// Whether a re-encode keeps the picture: the decoders turn CMYK/YCCK into RGB (without its
/// profile) and the encoders write YCbCr, so only grey and 3-component JPEGs are re-encoded.
pub fn reencodable(data: &[u8]) -> bool {
    matches!(components(data), Some(1 | 3))
}

/// The start of a JPEG file, enough to reach its frame header past big metadata segments.
pub fn head(path: &Path) -> Option<Vec<u8>> {
    read_head(path, 1 << 22)
}

/// The frame marker and where its segment starts (at its 0xFF).
fn frame(data: &[u8]) -> Option<(u8, usize)> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let m = data[i + 1];
        match m {
            0xFF => {
                i += 1;
                continue;
            }
            0xD8 | 0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            0xDA | 0xD9 => return None,
            // C4 (Huffman tables), C8 (reserved) and CC (arithmetic conditioning) aren't frames.
            0xC0..=0xCF if !matches!(m, 0xC4 | 0xC8 | 0xCC) => return Some((m, i)),
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// Replaces the APPn/COM segments of a JPEG with `segs`.
pub fn with_segments(jpg: &[u8], segs: &Segments) -> Vec<u8> {
    let mut i = 2;
    let mut body = Vec::new();
    while i + 4 <= jpg.len() && jpg[i] == 0xFF {
        let m = jpg[i + 1];
        if m == 0xDA {
            break;
        }
        let len = u16::from_be_bytes([jpg[i + 2], jpg[i + 3]]) as usize;
        let end = (i + 2 + len).min(jpg.len());
        if !((0xE0..=0xEF).contains(&m) || m == 0xFE) {
            body.extend_from_slice(&jpg[i..end]);
        }
        i = end;
    }
    let mut out = vec![0xFF, 0xD8];
    for (m, d) in segs {
        out.extend([0xFF, *m]);
        out.extend(((d.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(d);
    }
    out.extend(body);
    out.extend_from_slice(&jpg[i.min(jpg.len())..]);
    out
}

/// Where the first image of a JPEG ends (just after its EOI marker), following the segments
/// and the entropy-coded data. `None` if it can't be followed (damaged, truncated).
#[cfg(test)]
pub fn end_of_image(d: &[u8]) -> Option<usize> {
    if !d.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let be16 = |i: usize| -> Option<usize> { Some(usize::from(u16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]))) };
    let mut i = 2;
    loop {
        if *d.get(i)? != 0xFF {
            return None;
        }
        let m = *d.get(i + 1)?;
        match m {
            0xFF => i += 1, // fill byte
            0xD9 => return Some(i + 2),
            0x01 | 0xD0..=0xD7 => i += 2,
            0xDA => {
                // Scan header, then entropy-coded data up to the next marker (FF 00 is a
                // stuffed byte, FF D0-D7 a restart marker).
                i += 2 + be16(i + 2)?;
                loop {
                    i += d.get(i..)?.iter().position(|&b| b == 0xFF)?;
                    match *d.get(i + 1)? {
                        0x00 | 0xD0..=0xD7 => i += 2,
                        0xFF => i += 1,
                        _ => break,
                    }
                }
            }
            _ => i += 2 + be16(i + 2)?,
        }
    }
}

/// Streams a JPEG from disk (64 KiB buffer, never loads the full file into RAM) to check
/// whether any non-zero data follows its first `EOI` (`FF D9`).
/// Returns `None` if invalid/truncated before `EOI`, `Some(true)` if non-zero trailing data
/// exists (MPO, motion photo, Ultra HDR), and `Some(false)` if clean or zero-padded.
pub fn trailing_data(path: &Path) -> Option<bool> {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
    let mut f = BufReader::with_capacity(64 * 1024, std::fs::File::open(path).ok()?);
    let mut soi = [0u8; 2];
    f.read_exact(&mut soi).ok()?;
    if soi != [0xFF, 0xD8] {
        return None;
    }
    let read_u8 = |r: &mut BufReader<std::fs::File>| -> Option<u8> {
        let mut b = [0u8; 1];
        r.read_exact(&mut b).ok()?;
        Some(b[0])
    };
    let read_len = |r: &mut BufReader<std::fs::File>| -> Option<i64> {
        let mut b = [0u8; 2];
        r.read_exact(&mut b).ok()?;
        let len = u16::from_be_bytes(b);
        (len >= 2).then(|| i64::from(len - 2))
    };
    let mut pending_marker: Option<u8> = None;
    loop {
        let m = if let Some(m) = pending_marker.take() {
            m
        } else {
            if read_u8(&mut f)? != 0xFF {
                return None;
            }
            let mut m = read_u8(&mut f)?;
            while m == 0xFF {
                m = read_u8(&mut f)?;
            }
            m
        };
        match m {
            0xD9 => {
                // EOI reached: check if any non-zero byte remains.
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
            0x01 | 0xD0..=0xD7 => {}
            0xDA => {
                let skip = read_len(&mut f)?;
                f.seek(SeekFrom::Current(skip)).ok()?;
                // Scan entropy-coded bytes efficiently via BufRead::fill_buf.
                loop {
                    let buf = f.fill_buf().ok()?;
                    if buf.is_empty() {
                        return None;
                    }
                    if let Some(pos) = buf.iter().position(|&b| b == 0xFF) {
                        f.consume(pos + 1);
                        let mut next = read_u8(&mut f)?;
                        while next == 0xFF {
                            next = read_u8(&mut f)?;
                        }
                        if next == 0x00 || (0xD0..=0xD7).contains(&next) {
                            continue;
                        }
                        pending_marker = Some(next);
                        break;
                    }
                    let n = buf.len();
                    f.consume(n);
                }
            }
            _ => {
                let skip = read_len(&mut f)?;
                f.seek(SeekFrom::Current(skip)).ok()?;
            }
        }
    }
}

const STD_LUMA: [u32; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17,
    22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78,
    87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Estimated libjpeg-style quality (1-100) from the luminance quantization table.
pub fn quality(path: &Path) -> Option<f64> {
    let data = read_head(path, 1 << 16)?;
    let mut i = 2;
    while i + 4 <= data.len() && data[i] == 0xFF {
        let m = data[i + 1];
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if m == 0xDB {
            let mut j = i + 4;
            while j < (i + 2 + len).min(data.len()) {
                let (pq, tq) = (data[j] >> 4, data[j] & 15);
                let n = if pq == 1 { 128 } else { 64 };
                if tq == 0 {
                    let table = data.get(j + 1..j + 1 + n)?;
                    let sum: u32 = if pq == 1 {
                        table.chunks(2).map(|c| u32::from(u16::from_be_bytes([c[0], c[1]]))).sum()
                    } else {
                        table.iter().map(|&b| u32::from(b)).sum()
                    };
                    let scale = 100.0 * f64::from(sum) / f64::from(STD_LUMA.iter().sum::<u32>());
                    let q = if scale <= 100.0 { (200.0 - scale) / 2.0 } else { 5000.0 / scale };
                    return Some(q.clamp(1.0, 100.0));
                }
                j += 1 + n;
            }
        }
        if m == 0xDA {
            break;
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(m: u8, d: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, m];
        s.extend(((d.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(d);
        s
    }

    fn exif(orientation: u16) -> Vec<u8> {
        let mut t = b"Exif\0\0II*\0".to_vec();
        t.extend(8u32.to_le_bytes());
        t.extend(1u16.to_le_bytes());
        t.extend(0x0112u16.to_le_bytes());
        t.extend(3u16.to_le_bytes());
        t.extend(1u32.to_le_bytes());
        t.extend(orientation.to_le_bytes());
        t.extend([0, 0]);
        t.extend(0u32.to_le_bytes());
        t
    }

    fn jpeg(segs: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        for (m, d) in segs {
            j.extend(seg(*m, d));
        }
        j.extend(seg(0xDB, &[0; 65]));
        j.extend([0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]);
        j
    }

    #[test]
    fn segments_orientation_and_guards() {
        let orig = parse_segments(&jpeg(&[(0xE1, exif(6)), (0xE2, b"ICC_PROFILE\0x".to_vec()), (0xFE, b"hi".to_vec())])).unwrap();
        assert_eq!(orig.len(), 3);
        assert_eq!(exif_orientation(&orig), Some(6));
        let only_icc = parse_segments(&jpeg(&[(0xE2, b"ICC_PROFILE\0x".to_vec())])).unwrap();
        assert!(!meta_ok(&orig, &only_icc, true), "rotation must survive even when stripping");
        let kept = keep_for_reencode(&orig, true);
        assert_eq!(kept.len(), 2, "ICC and EXIF (rotated)");
        assert!(meta_ok(&orig, &kept, true));
        assert!(!meta_ok(&orig, &kept, false), "comment lost while keeping metadata");
        assert!(meta_ok(&orig, &orig, false));
    }

    /// A damaged header whose APPn/COM length is below 2 (the length counts its own 2 bytes)
    /// is not a JPEG we can read, and must not panic (a panic aborts the app in release).
    /// CMYK/YCCK JPEGs came back from a re-encode as RGB with the CMYK profile still on.
    #[test]
    fn only_grey_and_three_component_jpegs_are_reencoded() {
        let with_frame = |nf: u8| {
            let mut sof = vec![8, 0, 16, 0, 16, nf];
            for c in 1..=nf {
                sof.extend([c, 0x11, 0]);
            }
            let mut j = vec![0xFF, 0xD8];
            j.extend(seg(0xEE, b"Adobe\0\x64\0\0\0\0\x02"));
            j.extend(seg(0xC0, &sof));
            j.extend([0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]);
            j
        };
        assert_eq!(components(&with_frame(4)), Some(4));
        assert!(!reencodable(&with_frame(4)), "CMYK/YCCK");
        assert!(reencodable(&with_frame(3)) && reencodable(&with_frame(1)));
        assert!(!reencodable(b"not a jpeg"));
    }

    #[test]
    fn segment_length_below_two_is_rejected() {
        for len in [0u8, 1] {
            let mut j = vec![0xFF, 0xD8, 0xFF, 0xE1, 0, len, b'E', b'x', b'i', b'f'];
            j.extend(seg(0xDB, &[0; 65]));
            j.extend([0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]);
            assert_eq!(parse_segments(&j), None, "length {len}");
            let mut com = vec![0xFF, 0xD8, 0xFF, 0xFE, 0, len];
            com.extend([0xFF, 0xD9]);
            assert_eq!(parse_segments(&com), None, "COM length {len}");
        }
    }

    #[test]
    fn end_of_image_follows_scans() {
        // Entropy data with a stuffed FF 00 and a restart marker, then EOI and a second image.
        let mut j = vec![0xFF, 0xD8];
        j.extend(seg(0xDB, &[0; 65]));
        j.extend([0xFF, 0xDA, 0, 2, 1, 0xFF, 0x00, 2, 0xFF, 0xD3, 3, 0xFF, 0xD9]);
        let end = j.len();
        j.extend([0xFF, 0xD8, 0xFF, 0xD9]);
        assert_eq!(end_of_image(&j), Some(end));
        assert_eq!(end_of_image(&j[..end - 1]), None, "truncated");

        // Streaming `trailing_data` matches `end_of_image` without loading the file into RAM.
        let dir = std::env::temp_dir().join(format!("mo-jpeg-trail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        assert_eq!(trailing_data(&write("clean.jpg", &j[..end])), Some(false));
        let mut padded = j[..end].to_vec();
        padded.extend(std::iter::repeat_n(0u8, 64));
        assert_eq!(trailing_data(&write("padded.jpg", &padded)), Some(false));
        assert_eq!(trailing_data(&write("mpo.jpg", &j)), Some(true));
        assert_eq!(trailing_data(&write("cut.jpg", &j[..end - 1])), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_segments_replaces_headers_and_keeps_the_image() {
        let src = jpeg(&[(0xE0, b"JFIF\0".to_vec()), (0xEE, b"Adobe".to_vec())]);
        let out = with_segments(&src, &vec![(0xE1, exif(3))]);
        let segs = parse_segments(&out).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(exif_orientation(&segs), Some(3));
        assert!(out.ends_with(&[0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]));
    }

    /// Segments located past the first 1 MB of JPEG headers (e.g., after huge XMP/Photoshop blocks)
    /// are streamed without truncation.
    #[test]
    fn segments_stream_past_one_megabyte() {
        let dir = std::env::temp_dir().join(format!("mo-jpeg-big-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big-hdr.jpg");
        let mut raw = vec![0xFF, 0xD8];
        // 20 x 60 KB APP13 segments (~1.2 MB) before an ICC_PROFILE APP2 segment.
        let big = vec![b'X'; 60_000];
        for _ in 0..20 {
            raw.extend(seg(0xED, &big));
        }
        raw.extend(seg(0xE2, b"ICC_PROFILE\0tail-icc"));
        raw.extend(seg(0xDB, &[0; 65]));
        raw.extend([0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]);
        std::fs::write(&path, raw).unwrap();
        let segs = segments(&path).unwrap();
        assert_eq!(segs.len(), 21);
        assert!(segs.last().is_some_and(|(m, d)| *m == 0xE2 && d == b"ICC_PROFILE\0tail-icc"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
