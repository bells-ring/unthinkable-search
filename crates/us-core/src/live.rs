//! Live updates: apply changed MFT records (from the USN journal) to an index.
//!
//! Strategy (v1): rebuild a fresh sorted snapshot from "current snapshot minus
//! changed records plus their new state". Parents are resolved through MFT
//! record numbers, so directory renames/moves are handled exactly (all
//! descendants get the new path). The same routine doubles as compaction.
//! Cost ≈ build() (~50–80 ms per 500k entries); the caller debounces.
//! TODO(v2, >10M entries): delta overlay searched alongside the snapshot.

use crate::index::{build, Index};
use crate::ntfs::{FIRST_USER_RECORD, ROOT_RECORD};
use crate::scan::{NONE, ScanEntry, VolumeMeta, VolumeScan, ATTR_DIRECTORY};
use rayon::prelude::*;
use std::collections::HashMap;

/// New state of one MFT base record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecState {
    pub is_dir: bool,
    pub attrs: u32,
    pub mtime: u64,
    pub ctime: u64,
    pub size: u64,
    /// (parent record, name). Directories: exactly one name. Files: one per hard link.
    pub names: Vec<(u32, String)>,
}

/// Latest known state per changed record of ONE volume (`None` = deleted / not in use).
pub type Changes = HashMap<u32, Option<RecState>>;

#[derive(Clone, Copy)]
struct Item {
    rec: u32,
    parent_rec: u32,
    off: u32,
    len: u32,
    attrs: u32,
    size: u64,
    mtime: u64,
    ctime: u64,
}

/// Apply per-volume changes (indexed like `ix.volumes`) and new volume metas.
pub fn apply(ix: &Index, changes: &[Changes], metas: &[VolumeMeta]) -> Index {
    let n = ix.len();
    let nv = ix.volumes.len();
    let mut by_vol: Vec<Vec<u32>> = vec![Vec::new(); nv];
    for i in 0..n as u32 {
        by_vol[ix.vol[i as usize] as usize].push(i);
    }
    let scans: Vec<VolumeScan> = (0..nv)
        .into_par_iter()
        .map(|v| {
            let meta = metas.get(v).cloned().unwrap_or_else(|| ix.volumes[v].meta.clone());
            let ch = changes.get(v).filter(|c| !c.is_empty());
            let is_mft = ix.rec[ix.volumes[v].root_entry as usize] != NONE;
            match ch {
                Some(ch) if is_mft => rebuild_volume(ix, v, &by_vol[v], ch, meta),
                _ => copy_volume(ix, v, &by_vol[v], meta),
            }
        })
        .collect();
    build(scans)
}

/// Replace volumes with the same root (case-insensitive), append new ones.
/// Works on an empty `Index::default()` too (first start).
pub fn upsert_volumes(ix: &Index, scans: Vec<VolumeScan>) -> Index {
    let mut repl = Vec::new();
    let mut extra = Vec::new();
    for s in scans {
        match ix.volumes.iter().position(|v| v.root.eq_ignore_ascii_case(&s.root)) {
            Some(v) => repl.push((v, s)),
            None => extra.push(s),
        }
    }
    rebuild_with(ix, repl, extra)
}

/// Replace whole volumes with fresh scans (journal reset → MFT rescan).
pub fn replace_volumes(ix: &Index, repl: Vec<(usize, VolumeScan)>) -> Index {
    rebuild_with(ix, repl, Vec::new())
}

fn rebuild_with(ix: &Index, mut repl: Vec<(usize, VolumeScan)>, extra: Vec<VolumeScan>) -> Index {
    let nv = ix.volumes.len();
    let mut by_vol: Vec<Vec<u32>> = vec![Vec::new(); nv];
    for i in 0..ix.len() as u32 {
        by_vol[ix.vol[i as usize] as usize].push(i);
    }
    let mut slots: Vec<Option<VolumeScan>> = (0..nv).map(|_| None).collect();
    for (v, s) in repl.drain(..) {
        if v < nv {
            slots[v] = Some(s);
        }
    }
    let mut scans: Vec<VolumeScan> = slots
        .into_par_iter()
        .enumerate()
        .map(|(v, s)| s.unwrap_or_else(|| copy_volume(ix, v, &by_vol[v], ix.volumes[v].meta.clone())))
        .collect();
    scans.extend(extra);
    build(scans)
}

/// Unchanged volume: convert snapshot entries back to a scan, preserving the tree.
fn copy_volume(ix: &Index, v: usize, ents: &[u32], meta: VolumeMeta) -> VolumeScan {
    let root = ix.volumes[v].root_entry;
    let mut local: HashMap<u32, u32> = HashMap::with_capacity(ents.len());
    let mut s = VolumeScan::new(&ix.volumes[v].root);
    s.meta = meta;
    s.entries[0].attrs = ix.attrs[root as usize];
    s.entries[0].mtime = ix.mtime[root as usize];
    s.entries[0].ctime = ix.ctime[root as usize];
    s.entries[0].rec = ix.rec[root as usize];
    local.insert(root, 0);
    s.entries.reserve(ents.len());
    for &i in ents {
        if i == root {
            continue;
        }
        let iu = i as usize;
        // parent fixed below (holds the snapshot id for now)
        let id = s.push_rec(ix.parent[iu], ix.name(i), ix.attrs[iu], ix.size[iu], ix.mtime[iu], ix.ctime[iu], ix.rec[iu]);
        local.insert(i, id);
    }
    let mut keep = vec![true; s.entries.len()];
    for (k, e) in s.entries.iter_mut().enumerate().skip(1) {
        match local.get(&e.parent) {
            Some(&p) => e.parent = p,
            None => keep[k] = false,
        }
    }
    if keep.iter().all(|&k| k) { s } else { compact_orphans(s, &keep) }
}

/// Drop entries flagged in `keep` (and anything below them).
fn compact_orphans(s: VolumeScan, keep: &[bool]) -> VolumeScan {
    let mut out = VolumeScan::new(&s.root);
    out.meta = s.meta.clone();
    out.entries[0] = ScanEntry { name_off: 0, name_len: s.root.len() as u32, ..s.entries[0] };
    let mut map = vec![NONE; s.entries.len()];
    map[0] = 0;
    // Parents may come after children in a scan; iterate to a fixed point.
    let mut changed = true;
    while changed {
        changed = false;
        for k in 1..s.entries.len() {
            if map[k] != NONE || !keep[k] {
                continue;
            }
            let p = s.entries[k].parent as usize;
            if p < map.len() && map[p] != NONE {
                let e = s.entries[k];
                map[k] = out.push_rec(map[p], s.name(k), e.attrs, e.size, e.mtime, e.ctime, e.rec);
                changed = true;
            }
        }
    }
    out
}

/// Changed MFT volume: rebuild its tree from records.
fn rebuild_volume(ix: &Index, v: usize, ents: &[u32], ch: &Changes, meta: VolumeMeta) -> VolumeScan {
    let root = ix.volumes[v].root_entry;
    let mut arena: Vec<u8> = Vec::with_capacity(ents.len() * 16);
    let mut items: Vec<Item> = Vec::with_capacity(ents.len() + ch.len());
    let mut max_rec = ROOT_RECORD + 1;
    for &i in ents {
        let iu = i as usize;
        let r = ix.rec[iu];
        if i == root || r == NONE || ch.contains_key(&r) {
            continue;
        }
        let p = ix.parent[iu];
        if p == NONE {
            continue;
        }
        let off = arena.len() as u32;
        arena.extend_from_slice(ix.name(i).as_bytes());
        items.push(Item {
            rec: r,
            parent_rec: ix.rec[p as usize],
            off,
            len: arena.len() as u32 - off,
            attrs: ix.attrs[iu],
            size: ix.size[iu],
            mtime: ix.mtime[iu],
            ctime: ix.ctime[iu],
        });
        max_rec = max_rec.max(r as usize + 1);
    }
    for (&r, st) in ch {
        let Some(st) = st else { continue };
        if (r as usize) < FIRST_USER_RECORD {
            continue;
        }
        for (pr, name) in &st.names {
            let off = arena.len() as u32;
            arena.extend_from_slice(name.as_bytes());
            items.push(Item {
                rec: r,
                parent_rec: *pr,
                off,
                len: arena.len() as u32 - off,
                attrs: st.attrs | if st.is_dir { ATTR_DIRECTORY } else { 0 },
                size: if st.is_dir { 0 } else { st.size },
                mtime: st.mtime,
                ctime: st.ctime,
            });
            if st.is_dir {
                break; // a directory has exactly one name
            }
        }
        max_rec = max_rec.max(r as usize + 1);
    }
    for it in &items {
        max_rec = max_rec.max(it.parent_rec as usize + 1);
    }

    // Directory record → its item.
    let mut dir_item = vec![NONE; max_rec];
    for (k, it) in items.iter().enumerate() {
        if it.attrs & ATTR_DIRECTORY != 0 && dir_item[it.rec as usize] == NONE {
            dir_item[it.rec as usize] = k as u32;
        }
    }
    // Reachability from the root (cycle-safe), same scheme as ntfs::assemble.
    const UNK: u8 = 0;
    const OK: u8 = 1;
    const BAD: u8 = 2;
    const VISIT: u8 = 3;
    let mut state = vec![UNK; max_rec];
    state[ROOT_RECORD] = OK;
    let mut stack = Vec::new();
    for r0 in 0..max_rec {
        if state[r0] != UNK || dir_item[r0] == NONE {
            continue;
        }
        stack.clear();
        let mut cur = r0;
        let verdict = loop {
            match state[cur] {
                OK => break OK,
                BAD | VISIT => break BAD,
                _ => {}
            }
            let di = dir_item[cur];
            if di == NONE {
                state[cur] = BAD;
                break BAD;
            }
            state[cur] = VISIT;
            stack.push(cur);
            cur = items[di as usize].parent_rec as usize;
        };
        for &x in &stack {
            state[x] = verdict;
        }
    }

    let mut s = VolumeScan::new(&ix.volumes[v].root);
    s.meta = meta;
    s.entries[0].attrs = ix.attrs[root as usize];
    s.entries[0].mtime = ix.mtime[root as usize];
    s.entries[0].ctime = ix.ctime[root as usize];
    s.entries[0].rec = ROOT_RECORD as u32;
    let mut dir_local = vec![NONE; max_rec];
    dir_local[ROOT_RECORD] = 0;
    let name = |it: &Item| unsafe { std::str::from_utf8_unchecked(&arena[it.off as usize..(it.off + it.len) as usize]) };
    for r in 0..max_rec {
        if r != ROOT_RECORD && state[r] == OK {
            let it = items[dir_item[r] as usize];
            dir_local[r] = s.push_rec(it.parent_rec, name(&it), it.attrs, it.size, it.mtime, it.ctime, it.rec);
        }
    }
    for (k, it) in items.iter().enumerate() {
        let is_dir = it.attrs & ATTR_DIRECTORY != 0;
        if is_dir && dir_item[it.rec as usize] == k as u32 {
            continue; // already emitted
        }
        if is_dir || state[it.parent_rec as usize] != OK {
            continue;
        }
        s.push_rec(it.parent_rec, name(it), it.attrs, it.size, it.mtime, it.ctime, it.rec);
    }
    for e in s.entries.iter_mut().skip(1) {
        e.parent = dir_local[e.parent as usize];
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::search;
    use crate::Query;
    use std::sync::atomic::AtomicBool;

    fn mft_index() -> Index {
        // Synthetic MFT-like volume: records 30.. are user records.
        let mut s = VolumeScan::new("T:");
        s.entries[0].rec = 5;
        let docs = s.push_rec(0, "docs", ATTR_DIRECTORY, 0, 0, 0, 30);
        let sub = s.push_rec(docs, "sub", ATTR_DIRECTORY, 0, 0, 0, 31);
        s.push_rec(sub, "deep.txt", 0, 1, 0, 0, 32);
        s.push_rec(docs, "a.txt", 0, 2, 0, 0, 33);
        s.push_rec(0, "top.bin", 0, 3, 0, 0, 34);
        let mut w = VolumeScan::new("W:"); // a walk volume, untouched
        w.push(0, "walked.txt", 0, 0, 0);
        build(vec![s, w])
    }

    fn paths(ix: &Index, q: &str) -> Vec<String> {
        let mut p = String::new();
        search(ix, &Query::parse(q), &AtomicBool::new(false))
            .into_iter()
            .map(|i| {
                ix.full_path_into(i, &mut p);
                p.clone()
            })
            .collect()
    }

    #[test]
    fn create_rename_move_delete() {
        let ix = mft_index();
        let st = |is_dir, names: Vec<(u32, &str)>| {
            Some(RecState { is_dir, attrs: 0, mtime: 9, ctime: 8, size: 7, names: names.into_iter().map(|(p, n)| (p, n.to_string())).collect() })
        };
        let mut ch = Changes::new();
        ch.insert(31, st(true, vec![(5, "moved")])); // move+rename dir: docs\sub -> \moved
        ch.insert(40, st(false, vec![(31, "new.txt")])); // created inside it
        ch.insert(33, None); // deleted
        ch.insert(34, st(false, vec![(30, "top.bin"), (5, "link.bin")])); // moved + hard link
        let nx = apply(&ix, &[ch, Changes::new()], &[]);
        assert_eq!(paths(&nx, "deep"), vec!["T:\\moved\\deep.txt"]);
        assert_eq!(paths(&nx, "new.txt"), vec!["T:\\moved\\new.txt"]);
        assert!(paths(&nx, "a.txt").is_empty());
        assert_eq!(paths(&nx, "bin"), vec!["T:\\link.bin", "T:\\docs\\top.bin"]);
        assert_eq!(paths(&nx, "walked"), vec!["W:\\walked.txt"]);
        assert_eq!(paths(&nx, "moved\\"), vec!["T:\\moved\\deep.txt", "T:\\moved\\new.txt"]);
        assert_eq!(nx.len(), ix.len() + 1); // +new.txt +link.bin -a.txt
        crate::store::validate(&nx).unwrap();
        // Deleting a directory orphans anything left below it.
        let mut ch = Changes::new();
        ch.insert(30, None);
        let nx2 = apply(&nx, &[ch], &[]);
        assert!(paths(&nx2, "top.bin").is_empty());
        assert_eq!(paths(&nx2, "link"), vec!["T:\\link.bin"]);
    }

    #[test]
    fn noop_apply_is_identity() {
        let ix = mft_index();
        let nx = apply(&ix, &[], &[]);
        assert_eq!(nx.names, ix.names);
        assert_eq!(nx.parent, ix.parent);
        assert_eq!(nx.rec, ix.rec);
    }
}
