//! Case + diacritic folding used for both indexed names and queries.
//!
//! Diacritics are ignored (verified on a real index): é≡e, ü≡u, á≡a, and in
//! Cyrillic ё≡е, й≡и. Implementation: lowercase → canonical decomposition
//! (NFD) → drop combining marks.
//!
//! ASCII fast path for the overwhelming majority of names.

use unicode_normalization::char::{decompose_canonical, is_combining_mark};

/// Bump whenever folding output changes: stored indexes are re-folded on load.
/// 1 = lowercase only, 2 = lowercase + diacritics stripped.
pub const FOLD_VERSION: u32 = 2;

#[inline]
fn push_folded_char(ch: char, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4];
    for l in ch.to_lowercase() {
        if l.is_ascii() {
            out.push(l as u8);
            continue;
        }
        decompose_canonical(l, |d| {
            if !is_combining_mark(d) {
                out.extend_from_slice(d.encode_utf8(&mut buf).as_bytes());
            }
        });
    }
}

#[inline]
pub fn fold_into(s: &str, out: &mut Vec<u8>) {
    let b = s.as_bytes();
    if b.is_ascii() {
        out.reserve(b.len());
        for &c in b {
            // NUL can never be part of an NTFS name, but it is our separator,
            // so make sure it can never appear inside the blob.
            out.push(if c == 0 { b'?' } else { c.to_ascii_lowercase() });
        }
        return;
    }
    let start = out.len();
    for ch in s.chars() {
        if ch.is_ascii() {
            let c = ch as u8;
            out.push(if c == 0 { b'?' } else { c.to_ascii_lowercase() });
        } else {
            push_folded_char(ch, out);
        }
    }
    // A name made only of combining marks must not fold to "" (empty names
    // would break nothing, but keep them searchable by something).
    if out.len() == start {
        out.extend_from_slice(s.to_lowercase().as_bytes());
    }
}

#[inline]
pub fn fold(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len());
    fold_into(s, &mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ascii_and_unicode() {
        assert_eq!(fold("ReadMe.TXT"), b"readme.txt");
        assert_eq!(fold("ПРИВЕТ.doc"), "привет.doc".as_bytes());
        assert_eq!(fold("Straße"), "straße".as_bytes());
    }
    #[test]
    fn diacritics_are_folded() {
        assert_eq!(fold("Café Ünïcode Á"), b"cafe unicode a");
        assert_eq!(fold("Ёлка"), "елка".as_bytes());
        assert_eq!(fold("Йод"), "иод".as_bytes());
        assert_eq!(fold("ø"), "ø".as_bytes()); // no decomposition: unchanged
        assert_eq!(fold("\u{0301}"), "\u{0301}".as_bytes()); // marks-only name stays searchable
    }
}
