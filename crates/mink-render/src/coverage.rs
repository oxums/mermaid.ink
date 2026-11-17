//! Which characters each FONT_DIR face maps (its cmap), for the per-run font fallback in svg.rs:
//! merman's export picks one fallback font per line, Chrome (fontconfig) one per character.

use std::path::Path;

pub(crate) struct FontCoverage {
    /// (family, sorted disjoint inclusive codepoint ranges), one per family in FONT_DIR load
    /// order (file name order, as merman loads them). The first is the primary font.
    faces: Vec<(String, Vec<(u32, u32)>)>,
}

impl FontCoverage {
    pub(crate) fn load(dir: &Path) -> Result<Self, String> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    matches!(e.to_ascii_lowercase().as_str(), "ttf" | "ttc" | "otf")
                })
            })
            .collect();
        files.sort();
        let mut faces: Vec<(String, Vec<(u32, u32)>)> = Vec::new();
        for path in files {
            let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            for face in sfnt_faces(&data) {
                let Some((family, ranges)) = parse_face(&data, face) else {
                    continue;
                };
                if !faces.iter().any(|(f, _)| *f == family) {
                    faces.push((family, ranges));
                }
            }
        }
        Ok(Self { faces })
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self { faces: Vec::new() }
    }

    /// The first family after the primary that maps `c`; None when the primary maps it or no
    /// face does (usvg's own fallback then decides).
    pub(crate) fn fallback(&self, c: char) -> Option<&str> {
        let (primary, rest) = self.faces.split_first()?;
        if covers(&primary.1, c) {
            return None;
        }
        rest.iter().find(|f| covers(&f.1, c)).map(|f| f.0.as_str())
    }
}

fn covers(ranges: &[(u32, u32)], c: char) -> bool {
    let c = c as u32;
    let i = ranges.partition_point(|r| r.1 < c);
    ranges.get(i).is_some_and(|r| r.0 <= c)
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// Offsets of the table directories in a font file or collection.
fn sfnt_faces(d: &[u8]) -> Vec<usize> {
    if d.get(..4) != Some(b"ttcf") {
        return vec![0];
    }
    let count = u32_at(d, 8).unwrap_or(0).min(256) as usize;
    (0..count)
        .filter_map(|i| Some(u32_at(d, 12 + 4 * i)? as usize))
        .collect()
}

fn table<'a>(d: &'a [u8], face: usize, tag: &[u8; 4]) -> Option<&'a [u8]> {
    let n = u16_at(d, face + 4)? as usize;
    (0..n).find_map(|i| {
        let rec = face + 12 + 16 * i;
        if d.get(rec..rec + 4)? != tag {
            return None;
        }
        let (off, len) = (u32_at(d, rec + 8)? as usize, u32_at(d, rec + 12)? as usize);
        d.get(off..off.checked_add(len)?)
    })
}

fn parse_face(d: &[u8], face: usize) -> Option<(String, Vec<(u32, u32)>)> {
    let family = family_name(table(d, face, b"name")?)?;
    let mut ranges = cmap_ranges(table(d, face, b"cmap")?);
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1.saturating_add(1) => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Some((family, merged))
}

/// Typographic family (name ID 16) or family (ID 1), Windows/Unicode UTF-16BE, English first,
/// as fontdb reads it.
fn family_name(t: &[u8]) -> Option<String> {
    let count = u16_at(t, 2)? as usize;
    let strings = u16_at(t, 4)? as usize;
    let mut best: Option<(u8, String)> = None;
    for i in 0..count {
        let rec = 6 + 12 * i;
        let (platform, id) = (u16_at(t, rec)?, u16_at(t, rec + 6)?);
        let (len, off) = (u16_at(t, rec + 8)? as usize, u16_at(t, rec + 10)? as usize);
        if !matches!(platform, 0 | 3) || !matches!(id, 1 | 16) {
            continue;
        }
        let english = platform == 0 || u16_at(t, rec + 4)? == 0x0409;
        let rank = 1 + u8::from(english) + 2 * u8::from(id == 16);
        if best.as_ref().is_some_and(|(r, _)| *r >= rank) {
            continue;
        }
        let raw = t.get(strings + off..strings + off + len)?;
        let utf16: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        best = Some((rank, String::from_utf16_lossy(&utf16)));
    }
    best.map(|(_, name)| name)
}

/// Codepoint ranges of the Unicode format 4 and 12 subtables.
/// shortcut: a format 4 segment counts as fully mapped even where its glyph array holds 0;
/// upgrade to per-codepoint lookups if a shipped font relies on that.
fn cmap_ranges(t: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let n = u16_at(t, 2).unwrap_or(0) as usize;
    for i in 0..n {
        let rec = 4 + 8 * i;
        let (Some(platform), Some(encoding), Some(off)) =
            (u16_at(t, rec), u16_at(t, rec + 2), u32_at(t, rec + 4))
        else {
            break;
        };
        if !(platform == 0 || (platform == 3 && matches!(encoding, 1 | 10))) {
            continue;
        }
        let off = off as usize;
        match u16_at(t, off) {
            Some(4) => {
                let seg = u16_at(t, off + 6).unwrap_or(0) as usize / 2;
                for s in 0..seg {
                    let end = u16_at(t, off + 14 + 2 * s);
                    let start = u16_at(t, off + 16 + 2 * seg + 2 * s);
                    if let (Some(start), Some(end)) = (start, end)
                        && start <= end
                        && start != 0xFFFF
                    {
                        out.push((u32::from(start), u32::from(end)));
                    }
                }
            }
            Some(12) => {
                let groups = u32_at(t, off + 12).unwrap_or(0) as usize;
                for g in 0..groups.min(1 << 20) {
                    let at = off + 16 + 12 * g;
                    match (u32_at(t, at), u32_at(t, at + 4)) {
                        (Some(s), Some(e)) if s <= e => out.push((s, e)),
                        _ => break,
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_lookup() {
        let r = [(0x20, 0x7E), (0xA0, 0xFF)];
        assert!(covers(&r, 'a') && covers(&r, 'é') && covers(&r, ' '));
        assert!(!covers(&r, '\u{7F}') && !covers(&r, 'ก') && !covers(&r, '\u{1F}'));
    }

    /// Needs ./fonts (`bash tools/fonts/fetch.sh`); skipped without it.
    #[test]
    fn font_dir_fallback_order() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts");
        let Ok(c) = FontCoverage::load(&dir) else {
            return;
        };
        if c.faces.is_empty() {
            return;
        }
        assert_eq!(c.faces[0].0, "Liberation Sans");
        assert_eq!((c.fallback('a'), c.fallback('é')), (None, None));
        assert_eq!(c.fallback('अ'), Some("FreeSans")); // Devanagari: FreeSans before FreeSerif
        assert_eq!(c.fallback('中'), Some("WenQuanYi Zen Hei"));
        assert_eq!(c.fallback('ก'), Some("Loma")); // Thai, as Chrome picked
        assert_eq!(c.fallback('ש'), Some("DejaVu Sans")); // Hebrew, as Chrome picked
    }
}
