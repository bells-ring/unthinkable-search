//! Result ordering. Search results come out in name order for free (the index
//! is sorted by folded name); other keys sort the result set in parallel.

use crate::Index;
use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    /// Full path (case-/diacritics-insensitive, like names).
    Path,
    /// File size; folders by recursive size.
    Size,
    Modified,
    Created,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s.to_ascii_lowercase().as_str() {
            "name" => SortKey::Name,
            "path" | "full" => SortKey::Path,
            "size" => SortKey::Size,
            "mtime" | "modified" | "date_modified" => SortKey::Modified,
            "ctime" | "created" | "date_created" => SortKey::Created,
            _ => return None,
        })
    }

    pub fn from_index(i: u32) -> Option<SortKey> {
        [SortKey::Name, SortKey::Path, SortKey::Size, SortKey::Modified, SortKey::Created].get(i as usize).copied()
    }
}

/// Sort result ids. Ties are broken by name order, so the output is stable.
pub fn sort_ids(ix: &Index, ids: &mut Vec<u32>, key: SortKey, desc: bool) {
    match key {
        SortKey::Name => ids.par_sort_unstable(), // id order == name order
        SortKey::Path => {
            let mut keyed: Vec<(Vec<u8>, u32)> = ids
                .par_iter()
                .map_init(
                    || (Vec::new(), Vec::new()),
                    |(st, buf), &i| {
                        ix.folded_path_into(i, st, buf);
                        (buf.clone(), i)
                    },
                )
                .collect();
            keyed.par_sort_unstable();
            *ids = keyed.into_iter().map(|k| k.1).collect();
        }
        SortKey::Size => ids.par_sort_unstable_by_key(|&i| (ix.total_size(i), i)),
        SortKey::Modified => ids.par_sort_unstable_by_key(|&i| (ix.mtime[i as usize], i)),
        SortKey::Created => ids.par_sort_unstable_by_key(|&i| (ix.ctime[i as usize], i)),
    }
    if desc {
        ids.reverse();
    }
}
