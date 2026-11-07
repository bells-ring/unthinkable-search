//! The in-memory index: column-oriented (SoA), sorted by folded name.
//!
//! Layout of the search blob (`folded`):
//! `\0 name0 \0 name1 \0 ... nameN-1 \0`
//! `folded_off[i]` is the byte offset of name i (right after its leading NUL),
//! `folded_off[n]` = `folded.len()` (name i spans `folded_off[i]..folded_off[i+1]-1`).

use crate::fold::fold_into;
use crate::scan::{NONE, VolumeMeta, VolumeScan, ATTR_DIRECTORY};
use rayon::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeInfo {
    pub root: String,
    pub meta: VolumeMeta,
    /// Entry index of the volume root in the index.
    pub root_entry: u32,
}

#[derive(Default)]
pub struct Index {
    pub volumes: Vec<VolumeInfo>,
    pub names: Vec<u8>,
    pub names_off: Vec<u32>,
    pub folded: Vec<u8>,
    pub folded_off: Vec<u32>,
    pub parent: Vec<u32>,
    pub size: Vec<u64>,
    pub mtime: Vec<u64>,
    /// Creation time (FILETIME, 0 = unknown).
    pub ctime: Vec<u64>,
    pub attrs: Vec<u32>,
    /// MFT record number of each entry (NONE for walk-scanned volumes).
    pub rec: Vec<u32>,
    /// Volume index (into `volumes`) of each entry.
    pub vol: Vec<u8>,
    /// Recycle Bin table, sorted by entry id. `names` always holds the on-disk
    /// name (paths must stay real), `folded` the name people search for. A row
    /// is either a `$R…` item (its original name) or the `$I…` file it came from
    /// (the source of truth that survives rebuilds).
    pub orig_ids: Vec<u32>,
    /// `orig_names` offsets, `orig_ids.len() + 1` of them.
    pub orig_off: Vec<u32>,
    pub orig_names: Vec<u8>,
    /// Lazily computed directory topology (only built on the first path query).
    pub dir_topo: std::sync::OnceLock<DirTopo>,
    /// Lazily computed recursive directory sizes (indexed by `DirTopo` slot).
    pub dir_sizes: std::sync::OnceLock<Vec<u64>>,
}

impl Index {
    #[inline]
    pub fn len(&self) -> usize {
        self.parent.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// On-disk name (`$R123456.txt` for an item in the Recycle Bin).
    #[inline]
    pub fn name(&self, i: u32) -> &str {
        let i = i as usize;
        let b = &self.names[self.names_off[i] as usize..self.names_off[i + 1] as usize];
        // Names are written only from &str and validated on load.
        unsafe { std::str::from_utf8_unchecked(b) }
    }

    fn orig_row(&self, i: u32) -> Option<&str> {
        let k = self.orig_ids.binary_search(&i).ok()?;
        let b = &self.orig_names[self.orig_off[k] as usize..self.orig_off[k + 1] as usize];
        // Validated on load.
        Some(unsafe { std::str::from_utf8_unchecked(b) })
    }

    /// Name to show and match: the original name for a Recycle Bin item, the
    /// on-disk name for all other entries.
    #[inline]
    pub fn display_name(&self, i: u32) -> &str {
        let real = self.name(i);
        if real.starts_with("$R") { self.orig_row(i).unwrap_or(real) } else { real }
    }

    /// Item in the Recycle Bin shown under its original name.
    #[inline]
    pub fn is_recycled(&self, i: u32) -> bool {
        self.name(i).starts_with("$R") && self.orig_row(i).is_some()
    }

    /// `$I…` rows of volume `v`: (entry id, original name).
    pub(crate) fn recycle_infos(&self, v: usize) -> impl Iterator<Item = (u32, &str)> {
        self.orig_ids.iter().filter(move |&&i| self.vol[i as usize] as usize == v && self.name(i).starts_with("$I")).filter_map(|&i| Some((i, self.orig_row(i)?)))
    }

    #[inline]
    pub fn folded_name(&self, i: u32) -> &[u8] {
        let i = i as usize;
        &self.folded[self.folded_off[i] as usize..self.folded_off[i + 1] as usize - 1]
    }

    #[inline]
    pub fn is_dir(&self, i: u32) -> bool {
        self.attrs[i as usize] & ATTR_DIRECTORY != 0
    }

    /// Collect the chain root..=i (root first) into `stack`.
    #[inline]
    fn chain(&self, i: u32, stack: &mut Vec<u32>) {
        stack.clear();
        let mut cur = i;
        // Depth guard against corrupted data (cycles).
        while cur != NONE && stack.len() < 4096 {
            stack.push(cur);
            cur = self.parent[cur as usize];
        }
        stack.reverse();
    }

    /// Full path of entry `i` (e.g. `C:\Windows\notepad.exe`; a root is `C:\`).
    pub fn full_path_into(&self, i: u32, out: &mut String) {
        let mut st = Vec::with_capacity(32);
        self.chain(i, &mut st);
        out.clear();
        for (k, &e) in st.iter().enumerate() {
            if k > 0 {
                out.push('\\');
            }
            out.push_str(self.name(e));
        }
        if st.len() == 1 {
            out.push('\\');
        }
    }

    /// Path of the directory containing `i` (empty for roots).
    pub fn dir_path_into(&self, i: u32, out: &mut String) {
        out.clear();
        let p = self.parent[i as usize];
        if p != NONE {
            self.full_path_into(p, out);
            if self.parent[p as usize] == NONE {
                out.pop(); // `C:` rather than `C:\` for the path column
            }
        }
    }

    /// Folded full path, used for path matching. `stack` is scratch space.
    pub fn folded_path_into(&self, i: u32, stack: &mut Vec<u32>, out: &mut Vec<u8>) {
        self.chain(i, stack);
        out.clear();
        for (k, &e) in stack.iter().enumerate() {
            if k > 0 {
                out.push(b'\\');
            }
            out.extend_from_slice(self.folded_name(e));
        }
    }

    /// Approximate heap usage in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.folded.capacity()
            + 4 * (self.names_off.capacity() + self.folded_off.capacity())
            + 4 * self.parent.capacity()
            + 8 * self.size.capacity()
            + 8 * self.mtime.capacity()
            + 8 * self.ctime.capacity()
            + self.dir_sizes.get().map_or(0, |v| 8 * v.capacity())
            + 4 * self.attrs.capacity()
            + 4 * self.rec.capacity()
            + self.vol.capacity()
            + 4 * (self.orig_ids.capacity() + self.orig_off.capacity())
            + self.orig_names.capacity()
            + self.dir_topo.get().map_or(0, |t| 4 * (t.order.capacity() + t.slot.capacity() + t.pslot.capacity() + t.dpar.capacity() + t.end.capacity() + t.dname_off.capacity()) + t.dnames.capacity())
    }

    /// Directories in topological order (every parent before its children).
    pub fn dir_topo(&self) -> &DirTopo {
        self.dir_topo.get_or_init(|| DirTopo::build(self))
    }

    /// Size of entry `i`: the file size, or for a directory the recursive sum of
    /// all files below it (computed once per index on first use, a few ms).
    pub fn total_size(&self, i: u32) -> u64 {
        if !self.is_dir(i) {
            return self.size[i as usize];
        }
        let t = self.dir_topo();
        let sizes = self.dir_sizes.get_or_init(|| {
            let mut acc = vec![0u64; t.order.len()];
            for (e, &ps) in t.pslot.iter().enumerate() {
                if ps != NONE && t.slot[e] == NONE {
                    acc[ps as usize] = acc[ps as usize].saturating_add(self.size[e]);
                }
            }
            // Pre-order (parents first): walking it backwards folds children into parents.
            for s in (0..t.order.len()).rev() {
                let ps = t.dpar[s];
                if ps != NONE {
                    acc[ps as usize] = acc[ps as usize].saturating_add(acc[s]);
                }
            }
            acc
        });
        sizes[t.slot[i as usize] as usize]
    }
}

/// Directory tree in DFS pre-order: every parent precedes its children and
/// every subtree is one contiguous slot range `[s, end[s])`. Path queries walk
/// it sequentially and in parallel over independent subtrees.
pub struct DirTopo {
    /// Directory entry ids in DFS pre-order (children in name order).
    pub order: Vec<u32>,
    /// entry id → position in `order` (NONE for files).
    pub slot: Vec<u32>,
    /// entry id → slot of its parent directory (NONE for roots). Sequential
    /// scans read this instead of chasing parent → slot.
    pub pslot: Vec<u32>,
    /// slot → parent slot (NONE for roots); always `< slot`.
    pub dpar: Vec<u32>,
    /// slot → end of its subtree (exclusive).
    pub end: Vec<u32>,
    /// Folded directory names in slot order: name of slot s is
    /// `dnames[dname_off[s]..dname_off[s + 1]]`.
    pub dname_off: Vec<u32>,
    pub dnames: Vec<u8>,
    /// Some name contains a backslash (possible in the NTFS POSIX namespace): path
    /// queries must then use full-path matching instead of the segment automaton.
    pub sep_in_names: bool,
}

impl DirTopo {
    fn build(ix: &Index) -> DirTopo {
        let n = ix.len();
        // Dense directory index k (in id = name order).
        let mut slot = vec![NONE; n];
        let mut dirs = Vec::new();
        for i in 0..n as u32 {
            if ix.is_dir(i) {
                slot[i as usize] = dirs.len() as u32;
                dirs.push(i);
            }
        }
        let nd = dirs.len();
        // Children lists (CSR) by dense index; parents that are missing, not a
        // directory or the node itself make a root.
        let par_k = |k: usize| {
            let p = ix.parent[dirs[k] as usize];
            if p == NONE || p == dirs[k] { NONE } else { slot[p as usize] }
        };
        let mut start = vec![0u32; nd + 1];
        for k in 0..nd {
            let p = par_k(k);
            if p != NONE {
                start[p as usize + 1] += 1;
            }
        }
        for k in 0..nd {
            start[k + 1] += start[k];
        }
        let mut fill = start.clone();
        let mut kids = vec![0u32; start[nd] as usize];
        for k in 0..nd {
            let p = par_k(k);
            if p != NONE {
                kids[fill[p as usize] as usize] = k as u32;
                fill[p as usize] += 1;
            }
        }
        // Iterative DFS: true roots first, then anything unreachable (cycles in
        // corrupted data) as extra roots. Children are pushed in reverse so they
        // come out in name order.
        let mut order = Vec::with_capacity(nd);
        let mut dpar = Vec::with_capacity(nd);
        let mut pos_of = vec![NONE; nd];
        let mut stack: Vec<(u32, u32)> = Vec::new();
        let roots = (0..nd as u32).filter(|&k| par_k(k as usize) == NONE).chain(0..nd as u32);
        for r in roots {
            if pos_of[r as usize] != NONE {
                continue;
            }
            stack.push((r, NONE));
            while let Some((k, pp)) = stack.pop() {
                if pos_of[k as usize] != NONE {
                    continue;
                }
                let pos = order.len() as u32;
                pos_of[k as usize] = pos;
                order.push(dirs[k as usize]);
                dpar.push(pp);
                for &c in kids[start[k as usize] as usize..start[k as usize + 1] as usize].iter().rev() {
                    if pos_of[c as usize] == NONE {
                        stack.push((c, pos));
                    }
                }
            }
        }
        // Subtree ends: sizes folded bottom-up (dpar < slot in pre-order).
        let mut size = vec![1u32; nd];
        for s in (0..nd).rev() {
            let p = dpar[s];
            if p != NONE {
                size[p as usize] += size[s];
            }
        }
        let end: Vec<u32> = (0..nd).map(|s| s as u32 + size[s]).collect();
        for (pos, &e) in order.iter().enumerate() {
            slot[e as usize] = pos as u32;
        }
        let pslot = ix.parent.iter().map(|&p| if p == NONE { NONE } else { slot[p as usize] }).collect();
        let sep_in_names = memchr::memchr(b'\\', &ix.folded).is_some();
        // Folded directory names copied in slot order: path queries walk them
        // sequentially instead of jumping around the 10+ MB name blob.
        let mut dname_off = Vec::with_capacity(nd + 1);
        let mut dnames = Vec::with_capacity(nd * 16);
        for &d in &order {
            dname_off.push(dnames.len() as u32);
            dnames.extend_from_slice(ix.folded_name(d));
        }
        dname_off.push(dnames.len() as u32);
        DirTopo { order, slot, pslot, dpar, end, dname_off, dnames, sep_in_names }
    }
}

/// Build a sorted index out of any number of volume scans.
pub fn build(scans: Vec<VolumeScan>) -> Index {
    // 1. Global temporary ids.
    let mut base = Vec::with_capacity(scans.len());
    let mut n = 0usize;
    for s in &scans {
        base.push(n as u32);
        n += s.entries.len();
    }
    assert!(n < NONE as usize, "too many entries");
    let links: Vec<_> = scans.iter().map(|s| s.recycle_links()).collect();

    // 2. Fold all names in parallel (chunked), producing a temp blob in temp-id order.
    const CH: usize = 1 << 15;
    let mut work: Vec<(usize, usize, usize)> = Vec::new(); // (scan, start, end)
    for (si, s) in scans.iter().enumerate() {
        let mut st = 0;
        while st < s.entries.len() {
            let en = (st + CH).min(s.entries.len());
            work.push((si, st, en));
            st = en;
        }
    }
    let parts: Vec<(Vec<u8>, Vec<u32>)> = work
        .par_iter()
        .map(|&(si, st, en)| {
            let s = &scans[si];
            let items = &links[si].items;
            let mut blob = Vec::with_capacity((en - st) * 24);
            let mut lens = Vec::with_capacity(en - st);
            for i in st..en {
                let b0 = blob.len();
                let mut name = s.name(i);
                if !items.is_empty() && name.starts_with("$R") {
                    name = items.get(&(i as u32)).copied().unwrap_or(name);
                }
                fold_into(name, &mut blob);
                lens.push((blob.len() - b0) as u32);
            }
            (blob, lens)
        })
        .collect();
    let total_folded: usize = parts.iter().map(|p| p.0.len()).sum();
    let mut tf = Vec::with_capacity(total_folded);
    let mut tf_off = Vec::with_capacity(n + 1);
    for (blob, lens) in &parts {
        let mut o = tf.len() as u32;
        for &l in lens {
            tf_off.push(o);
            o += l;
        }
        tf.extend_from_slice(blob);
    }
    tf_off.push(tf.len() as u32);
    drop(parts);
    let tname = |g: usize| &tf[tf_off[g] as usize..tf_off[g + 1] as usize];

    // 3. Sort by folded name: 8-byte big-endian prefix key, full compare on ties.
    let mut keys: Vec<(u64, u32)> = (0..n)
        .into_par_iter()
        .map(|g| {
            let s = tname(g);
            let mut k = [0u8; 8];
            let m = s.len().min(8);
            k[..m].copy_from_slice(&s[..m]);
            (u64::from_be_bytes(k), g as u32)
        })
        .collect();
    keys.par_sort_unstable_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| {
            let (x, y) = (tname(a.1 as usize), tname(b.1 as usize));
            let (x, y) = (&x[x.len().min(8)..], &y[y.len().min(8)..]);
            x.cmp(y).then(a.1.cmp(&b.1))
        })
    });
    let perm: Vec<u32> = keys.into_par_iter().map(|k| k.1).collect();
    let mut inv = vec![0u32; n];
    for (new, &old) in perm.iter().enumerate() {
        inv[old as usize] = new as u32;
    }

    // Map temp id -> (scan, local) quickly.
    let mut owner = Vec::with_capacity(n);
    for (si, s) in scans.iter().enumerate() {
        owner.extend(std::iter::repeat_n(si as u8, s.entries.len()));
    }
    assert!(scans.len() <= 255);
    let entry = |g: usize| {
        let si = owner[g] as usize;
        (si, &scans[si].entries[g - base[si] as usize])
    };

    // 4. Lay out the columns in sorted order.
    let parent: Vec<u32> = perm
        .par_iter()
        .map(|&g| {
            let (si, e) = entry(g as usize);
            if e.parent == NONE {
                NONE
            } else {
                inv[(base[si] + e.parent) as usize]
            }
        })
        .collect();
    let size: Vec<u64> = perm.par_iter().map(|&g| entry(g as usize).1.size).collect();
    let mtime: Vec<u64> = perm.par_iter().map(|&g| entry(g as usize).1.mtime).collect();
    let ctime: Vec<u64> = perm.par_iter().map(|&g| entry(g as usize).1.ctime).collect();
    let attrs: Vec<u32> = perm.par_iter().map(|&g| entry(g as usize).1.attrs).collect();
    let rec: Vec<u32> = perm.par_iter().map(|&g| entry(g as usize).1.rec).collect();
    let vol: Vec<u8> = perm.par_iter().map(|&g| owner[g as usize]).collect();

    let total_names: usize = scans.iter().map(|s| s.arena.len()).sum();
    let mut names = Vec::with_capacity(total_names);
    let mut names_off = Vec::with_capacity(n + 1);
    let mut folded = Vec::with_capacity(total_folded + n + 1);
    let mut folded_off = Vec::with_capacity(n + 1);
    folded.push(0);
    for &g in &perm {
        let (si, e) = entry(g as usize);
        names_off.push(names.len() as u32);
        let s = &scans[si];
        names.extend_from_slice(&s.arena[e.name_off as usize..(e.name_off + e.name_len) as usize]);
        folded_off.push(folded.len() as u32);
        folded.extend_from_slice(tname(g as usize));
        folded.push(0);
    }
    names_off.push(names.len() as u32);
    folded_off.push(folded.len() as u32);

    let mut orig: Vec<(u32, &str)> = Vec::new();
    for (si, l) in links.iter().enumerate() {
        let rows = l.items.iter().map(|(&id, &o)| (id, o)).chain(l.infos.iter().copied());
        orig.extend(rows.map(|(id, o)| (inv[(base[si] + id) as usize], o)));
    }
    orig.sort_unstable_by_key(|r| r.0);
    orig.dedup_by_key(|r| r.0);
    let mut orig_ids = Vec::with_capacity(orig.len());
    let mut orig_off = Vec::with_capacity(orig.len() + 1);
    let mut orig_names = Vec::new();
    for (id, o) in &orig {
        orig_ids.push(*id);
        orig_off.push(orig_names.len() as u32);
        orig_names.extend_from_slice(o.as_bytes());
    }
    orig_off.push(orig_names.len() as u32);

    let volumes = scans
        .iter()
        .enumerate()
        .map(|(si, s)| VolumeInfo {
            root: s.root.clone(),
            meta: s.meta.clone(),
            root_entry: inv[base[si] as usize],
        })
        .collect();

    Index {
        volumes,
        names,
        names_off,
        folded,
        folded_off,
        parent,
        size,
        mtime,
        ctime,
        attrs,
        rec,
        vol,
        orig_ids,
        orig_off,
        orig_names,
        dir_topo: Default::default(),
        dir_sizes: Default::default(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::scan::ATTR_DIRECTORY as D;

    pub fn sample() -> Index {
        let mut c = VolumeScan::new("C:");
        let win = c.push(0, "Windows", D, 0, 1);
        let sys = c.push(win, "System32", D, 0, 2);
        c.push(sys, "notepad.exe", 0, 100, 3);
        c.push(win, "Notepad.exe", 0, 200, 4);
        c.push(0, "readme.TXT", 0, 5, 5);
        let docs = c.push(0, "Документы", D, 0, 6);
        c.push(docs, "Отчёт 2026.pdf", 0, 7, 7);
        c.push(docs, "report.pdf", 0, 8, 8);
        let mut d = VolumeScan::new("D:");
        let p = d.push(0, "projects", D, 0, 9);
        c.push(0, "pdf", D, 0, 10);
        d.push(p, "notes.md", 0, 11, 11);
        build(vec![c, d])
    }

    #[test]
    fn sorted_and_paths() {
        let ix = sample();
        assert_eq!(ix.len(), 13);
        let win = (0..ix.len() as u32).find(|&i| ix.name(i) == "Windows").unwrap();
        assert_eq!(ix.total_size(win), 300);
        assert_eq!(ix.total_size(ix.volumes[0].root_entry), 300 + 5 + 7 + 8);
        assert_eq!(ix.total_size(ix.volumes[1].root_entry), 11);
        let names: Vec<&[u8]> = (0..ix.len() as u32).map(|i| ix.folded_name(i)).collect();
        let mut s = names.clone();
        s.sort();
        assert_eq!(names, s);
        let np = (0..ix.len() as u32).find(|&i| ix.name(i) == "notepad.exe").unwrap();
        let mut p = String::new();
        ix.full_path_into(np, &mut p);
        assert_eq!(p, "C:\\Windows\\System32\\notepad.exe");
        ix.dir_path_into(np, &mut p);
        assert_eq!(p, "C:\\Windows\\System32");
        let root = ix.volumes[1].root_entry;
        ix.full_path_into(root, &mut p);
        assert_eq!(p, "D:\\");
        let notes = (0..ix.len() as u32).find(|&i| ix.name(i) == "notes.md").unwrap();
        ix.dir_path_into(notes, &mut p);
        assert_eq!(p, "D:\\projects");
        let pr = ix.parent[notes as usize];
        ix.dir_path_into(pr, &mut p);
        assert_eq!(p, "D:");
    }

    /// `$I` contents in the v2 layout.
    pub fn info_v2(path: &str) -> Vec<u8> {
        let u: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut b = vec![0u8; 24];
        b[0] = 2;
        b.extend_from_slice(&(u.len() as u32).to_le_bytes());
        b.extend(u.iter().flat_map(|c| c.to_le_bytes()));
        b
    }

    #[test]
    fn recycle_info_layouts_and_bad_input() {
        use crate::scan::recycle_original_name as parse;
        assert_eq!(parse(&info_v2(r"D:\yt-dlp\Отчёт.mp4.part")).as_deref(), Some("Отчёт.mp4.part"));
        // v1: fixed buffer, NUL-terminated.
        let mut v1 = vec![0u8; 24 + 520];
        v1[0] = 1;
        for (k, c) in r"C:\a\report.txt".encode_utf16().enumerate() {
            v1[24 + 2 * k..26 + 2 * k].copy_from_slice(&c.to_le_bytes());
        }
        assert_eq!(parse(&v1).as_deref(), Some("report.txt"));
        // Never panics on truncated input, rejects what could disguise a name.
        let good = info_v2(r"C:\a\b.txt");
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]);
        }
        let mut huge = good.clone();
        huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&huge), None);
        for bad in [r"C:\a\", r"C:\a\b:c.txt", "C:\\a\\x\u{202E}fdp.exe", "C:\\a\\x\ty.txt", r"C:\a\.."] {
            assert_eq!(parse(&info_v2(bad)), None, "{bad}");
        }
        assert_eq!(parse(&[3u8; 64]), None);
    }

    pub fn recycle_scan() -> VolumeScan {
        let mut c = VolumeScan::new("C:");
        let bin = c.push(0, "$Recycle.Bin", D, 0, 1);
        let sid = c.push(bin, "S-1-5-21-1", D, 0, 1);
        let i = c.push(sid, "$IABC123.txt", 0, 100, 1);
        c.recycle.push((i, "Отчёт за год.txt".into()));
        c.push(sid, "$RABC123.txt", 0, 5000, 1);
        // A deleted folder: its children keep their names.
        let i = c.push(sid, "$IDIR456", 0, 100, 1);
        c.recycle.push((i, "Проект.old".into()));
        let d = c.push(sid, "$RDIR456", D, 0, 1);
        c.push(d, "main.rs", 0, 7, 1);
        // Extension spoofing: shown name must not hide an executable.
        let i = c.push(sid, "$IEVIL78.exe", 0, 100, 1);
        c.recycle.push((i, "invoice.pdf".into()));
        c.push(sid, "$REVIL78.exe", 0, 1, 1);
        // Metadata without an item (its `$R` was removed): nothing to show.
        let i = c.push(sid, "$IGONE90.txt", 0, 100, 1);
        c.recycle.push((i, "gone.txt".into()));
        // The same shape outside a Recycle Bin is an ordinary pair of files.
        let o = c.push(0, "other", D, 0, 1);
        let i = c.push(o, "$IOUT000.txt", 0, 1, 1);
        c.recycle.push((i, "fake.txt".into()));
        c.push(o, "$ROUT000.txt", 0, 1, 1);
        c
    }

    fn find(ix: &Index, folded: &str) -> Vec<String> {
        let mut p = String::new();
        (0..ix.len() as u32)
            .filter(|&i| ix.folded_name(i) == folded.as_bytes())
            .map(|i| {
                ix.full_path_into(i, &mut p);
                p.clone()
            })
            .collect()
    }

    #[test]
    fn recycled_items_are_found_by_original_name() {
        let ix = build(vec![recycle_scan()]);
        crate::store::validate(&ix).unwrap();
        let sid = r"C:\$Recycle.Bin\S-1-5-21-1";
        // Searchable and shown under the original name, opened by the real path.
        assert_eq!(find(&ix, "отчет за год.txt"), vec![format!(r"{sid}\$RABC123.txt")]);
        let i = (0..ix.len() as u32).find(|&i| ix.name(i) == "$RABC123.txt").unwrap();
        assert_eq!(ix.display_name(i), "Отчёт за год.txt");
        assert!(ix.is_recycled(i));
        // Folder: original name; children still resolve through the real one.
        assert_eq!(find(&ix, "проект.old"), vec![format!(r"{sid}\$RDIR456")]);
        assert_eq!(find(&ix, "main.rs"), vec![format!(r"{sid}\$RDIR456\main.rs")]);
        // Metadata files, spoofed extensions and look-alikes keep their names.
        assert_eq!(find(&ix, "$iabc123.txt").len(), 1);
        assert!(find(&ix, "invoice.pdf").is_empty());
        assert_eq!(find(&ix, "$revil78.exe").len(), 1);
        assert!(find(&ix, "gone.txt").is_empty());
        assert!(find(&ix, "fake.txt").is_empty());
        assert_eq!(find(&ix, "$rout000.txt"), vec![r"C:\other\$ROUT000.txt"]);
        let plain = (0..ix.len() as u32).find(|&i| ix.name(i) == "main.rs").unwrap();
        assert_eq!(ix.display_name(plain), "main.rs");
        assert!(!ix.is_recycled(plain));
    }
}
