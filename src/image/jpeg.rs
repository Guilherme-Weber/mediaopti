//! JPEG structure: APPn/COM segments, EXIF orientation, metadata guards and the encoder
//! quality estimate.

use std::path::Path;

/// `(marker, payload)` of the APPn (0xE0-0xEF) and COM (0xFE) segments before the scan.
pub type Segments = Vec<(u8, Vec<u8>)>;

pub fn segments(path: &Path) -> Option<Segments> {
    // The header segments are near the start; 1 MB covers even large EXIF/ICC blocks.
    let data = read_head(path, 1 << 20)?;
    parse_segments(&data)
}

fn read_head(path: &Path, max: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    f.take(max as u64).read_to_end(&mut data).ok()?;
    Some(data)
}

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
    for (m, d) in segs {
        if *m != 0xE1 || !d.starts_with(b"Exif\0\0") {
            continue;
        }
        let t = &d[6..];
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

    #[test]
    fn with_segments_replaces_headers_and_keeps_the_image() {
        let src = jpeg(&[(0xE0, b"JFIF\0".to_vec()), (0xEE, b"Adobe".to_vec())]);
        let out = with_segments(&src, &vec![(0xE1, exif(3))]);
        let segs = parse_segments(&out).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(exif_orientation(&segs), Some(3));
        assert!(out.ends_with(&[0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]));
    }
}
