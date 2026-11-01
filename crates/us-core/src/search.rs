//! Search execution.
//!
//! Fast path: SIMD substring search over the contiguous NUL-separated blob,
//! split into chunks on entry boundaries and scanned in parallel. The blob is
//! sorted by name, so concatenated chunk results are already sorted. A driver
//! term with several alternatives (`a|b`, `ext:jpg;png`) runs one scan per key
//! and merges. Per key the strategy is picked by sampled density (`KeyScan`):
//! memmem jumps between sparse hits, an AVX2 pass (`dense`) maps dense hits to
//! entries with popcounts. `!foo` is the complement of a scan; other queries
//! without a literal driver use a parallel per-entry predicate.
//!
//! Path terms (`windows\sys`) use a segment automaton over the directory tree
//! (DFS pre-order, parallel over subtrees; see `PathPrep`): afterwards any
//! entry is decided by one state byte of its parent — no full path is ever
//! materialized.

use crate::index::{DirTopo, Index};
use crate::query::{glob_match, Atom, Driver, Pat, Query};
use crate::scan::NONE;
use memchr::memmem::Finder;
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Most path segments (`\`-separated parts minus one) handled by the segment
/// automaton: its per-directory state is one byte (hit + 7 segment bits).
const MAX_PATH_SEPS: usize = 7;

/// Per-query directory state for one path-substring atom `P`.
///
/// A path is `seg0\seg1\…\segN` (seg0 = volume, e.g. `c:`); names never
/// contain `\`. Split `P = p0\p1\…\pm`. `P` occurs in a path iff for some j:
/// seg_j ends with p0, seg_{j+1..j+m-1} == p1..p(m-1), seg_{j+m} starts with pm.
/// One pass over the directory tree (DFS pre-order, parallel over subtrees)
/// computes one state byte per directory: bit0 = "path contains P", bit t+1 =
/// "path ends with p0\…\pt" (t < m). Afterwards any entry (file or folder) is
/// decided by its parent's byte plus, rarely, a prefix compare of its own name
/// — no memmem, no path materialization. 73 KB of state per 73k directories
/// stays in L2.
struct PathPrep {
    /// p0 (suffix of a segment), p1..p(m-1) (whole segments), pm (prefix).
    parts: Vec<Vec<u8>>,
    /// By slot: bit0 = HIT, bits 1..=m = E_0..E_(m-1).
    st: Vec<u8>,
    /// Bit of E_(m-1) ("the next segment completes P if it starts with pm").
    ready: u8,
}

impl PathPrep {
    /// None → the caller falls back to full-path matching (pattern without a
    /// separator, more than `MAX_PATH_SEPS` separators, or names containing `\`,
    /// e.g. POSIX-namespace NTFS names, which break the segment model).
    fn build(ix: &Index, pat: &[u8]) -> Option<PathPrep> {
        let topo = ix.dir_topo();
        let parts: Vec<Vec<u8>> = pat.split(|&b| b == b'\\').map(<[u8]>::to_vec).collect();
        let m = parts.len() - 1;
        if m == 0 || m > MAX_PATH_SEPS || topo.sep_in_names {
            return None;
        }
        let nd = topo.order.len();
        let mut st = vec![0u8; nd];
        let ready = 1u8 << m;
        let e_mask = ((1u16 << m) - 1) as u8; // E_0..E_(m-1) after >> 1
        let parts_ref = &parts;
        // State byte of slot `s` from its parent's state byte.
        let step = |s: usize, pst: u8| -> u8 {
            let name = &topo.dnames[topo.dname_off[s] as usize..topo.dname_off[s + 1] as usize];
            let pe = pst >> 1;
            // E_0: this segment ends with p0; E_t: whole segment == p_t after E_(t-1).
            let mut e = name.ends_with(&parts_ref[0]) as u8;
            let mut cand = (pe << 1) & e_mask & !1;
            while cand != 0 {
                let t = cand.trailing_zeros() as usize;
                cand &= cand - 1;
                if name == parts_ref[t].as_slice() {
                    e |= 1 << t;
                }
            }
            let hit = pst & 1 != 0 || (pst & ready != 0 && name.starts_with(&parts_ref[m]));
            (e << 1) | hit as u8
        };
        // Big subtrees are walked sequentially until their children are small
        // enough; small subtrees (contiguous in pre-order) are coalesced into
        // windows of ~grain slots processed in parallel.
        let grain = (nd / (rayon::current_num_threads() * 8)).max(512);
        let mut windows_ab: Vec<(usize, usize)> = Vec::new();
        let mut s = 0;
        while s < nd {
            let e = topo.end[s] as usize;
            if e - s <= grain {
                match windows_ab.last_mut() {
                    Some(w) if w.1 == s && w.1 - w.0 < grain => w.1 = e,
                    _ => windows_ab.push((s, e)),
                }
                s = e;
            } else {
                let p = topo.dpar[s];
                st[s] = step(s, if p == NONE { 0 } else { st[p as usize] });
                s += 1;
            }
        }
        // A window node whose parent lies outside the window reads the final,
        // sequentially computed state from a snapshot (73 KB copy).
        let base = st.clone();
        let mut windows: Vec<(usize, &mut [u8])> = Vec::with_capacity(windows_ab.len());
        let (mut rest, mut off) = (&mut st[..], 0usize);
        for &(a, b) in &windows_ab {
            let (_, r1) = std::mem::take(&mut rest).split_at_mut(a - off);
            let (w, r2) = r1.split_at_mut(b - a);
            windows.push((a, w));
            (rest, off) = (r2, b);
        }
        windows.into_par_iter().for_each(|(a, w)| {
            for j in 0..w.len() {
                let s = a + j;
                let pst = match topo.dpar[s] {
                    NONE => 0,
                    p if (p as usize) >= a => w[p as usize - a],
                    p => base[p as usize],
                };
                w[j] = step(s, pst);
            }
        });
        Some(PathPrep { parts, st, ready })
    }

    /// Does the full path of entry `i` contain P?
    #[inline]
    fn matches(&self, ix: &Index, i: u32) -> bool {
        self.matches_in(ix, ix.dir_topo(), i)
    }

    #[inline(always)]
    fn matches_in(&self, ix: &Index, topo: &DirTopo, i: u32) -> bool {
        let ps = topo.pslot[i as usize];
        if ps == NONE {
            return false; // a volume root is a single segment; P has a separator
        }
        let s = self.st[ps as usize];
        s & 1 != 0 || (s & self.ready != 0 && ix.folded_name(i).starts_with(&self.parts[self.parts.len() - 1]))
    }
}

/// A query prepared for execution (prebuilt SIMD finders, path state).
pub struct Prepared<'q> {
    q: &'q Query,
    /// Per term, per alt: finder for substring atoms.
    finders: Vec<Vec<Option<Finder<'q>>>>,
    paths: Vec<Vec<Option<PathPrep>>>,
    driver: Option<Driver>,
}

impl<'q> Prepared<'q> {
    pub fn new(q: &'q Query, ix: &Index) -> Self {
        let finders: Vec<Vec<Option<Finder<'q>>>> = q
            .terms
            .iter()
            .map(|t| {
                t.alts
                    .iter()
                    .map(|a| match a {
                        Atom::Name(Pat::Substr(s)) | Atom::Path(Pat::Substr(s)) => Some(Finder::new(s.as_slice())),
                        _ => None,
                    })
                    .collect()
            })
            .collect();
        let paths = q
            .terms
            .iter()
            .map(|t| {
                t.alts
                    .iter()
                    .map(|a| match a {
                        Atom::Path(Pat::Substr(s)) if !s.is_empty() => PathPrep::build(ix, s),
                        _ => None,
                    })
                    .collect()
            })
            .collect();
        Prepared { q, finders, paths, driver: q.driver() }
    }

    /// The query is exactly one term with one alternative: (atom, term index).
    fn single(&self) -> Option<(&'q Atom, usize)> {
        match self.q.terms.as_slice() {
            [t] if t.alts.len() == 1 => Some((&t.alts[0], 0)),
            _ => None,
        }
    }

    /// Evaluate the whole query against entry `i`, optionally skipping one
    /// term already proven by the driver.
    #[inline]
    fn matches(&self, ix: &Index, i: u32, skip: Option<usize>, sc: &mut Scratch) -> bool {
        // Name slice is computed only if an atom needs it (`file:`/`folder:`/paths don't).
        let mut name_cache: Option<&[u8]> = None;
        let mut name = || *name_cache.get_or_insert_with(|| ix.folded_name(i));
        let mut path_ready = false;
        for (ti, t) in self.q.terms.iter().enumerate() {
            if Some(ti) == skip {
                continue;
            }
            let mut any = false;
            for (ai, a) in t.alts.iter().enumerate() {
                let ok = match a {
                    Atom::Name(Pat::Substr(_)) => self.finders[ti][ai].as_ref().unwrap().find(name()).is_some(),
                    Atom::Name(Pat::Glob(g)) => glob_match(g, name()),
                    Atom::Ext(list) => ext_match(name(), list),
                    Atom::IsFile => !ix.is_dir(i),
                    Atom::IsDir => ix.is_dir(i),
                    Atom::Path(p) => {
                        if let Some(pp) = &self.paths[ti][ai] {
                            pp.matches(ix, i)
                        } else {
                            if !path_ready {
                                ix.folded_path_into(i, &mut sc.stack, &mut sc.full);
                                path_ready = true;
                            }
                            match p {
                                Pat::Substr(_) => self.finders[ti][ai].as_ref().unwrap().find(&sc.full).is_some(),
                                Pat::Glob(g) => glob_match(g, &sc.full),
                            }
                        }
                    }
                };
                if ok {
                    any = true;
                    break;
                }
            }
            if any == t.neg {
                return false;
            }
        }
        true
    }
}

#[derive(Default)]
struct Scratch {
    stack: Vec<u32>,
    full: Vec<u8>,
}

#[inline]
fn ext_match(name: &[u8], list: &[Vec<u8>]) -> bool {
    let Some(dot) = memchr::memrchr(b'.', name) else {
        return false;
    };
    let e = &name[dot + 1..];
    list.iter().any(|x| x.as_slice() == e)
}

/// Number of chunks per thread (load balancing for skewed hit density).
const CHUNKS_PER_THREAD: usize = 8;
const MIN_CHUNK_ENTRIES: usize = 16 * 1024;

fn chunk_bounds(n: usize) -> Vec<(usize, usize)> {
    let k = (rayon::current_num_threads() * CHUNKS_PER_THREAD)
        .min(n.div_ceil(MIN_CHUNK_ENTRIES))
        .max(1);
    let step = n.div_ceil(k);
    (0..k)
        .map(|c| (c * step, ((c + 1) * step).min(n)))
        .filter(|(a, b)| a < b)
        .collect()
}

/// Merge sorted, deduplicated id lists into one.
fn union_sorted(mut lists: Vec<Vec<u32>>) -> Vec<u32> {
    lists.retain(|l| !l.is_empty());
    match lists.len() {
        0 => Vec::new(),
        1 => lists.pop().unwrap(),
        _ => {
            // Pairwise linear merges of sorted, duplicate-free lists.
            while lists.len() > 1 {
                let b = lists.pop().unwrap();
                let a = lists.pop().unwrap();
                lists.push(merge_sorted(&a, &b));
            }
            lists.pop().unwrap()
        }
    }
}

fn merge_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out.push(x.min(y));
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Search the whole index. Results are entry ids in name order.
/// `cancel` is polled between chunks; a cancelled search returns partial garbage
/// the caller must discard.
pub fn search(ix: &Index, q: &Query, cancel: &AtomicBool) -> Vec<u32> {
    let n = ix.len();
    if q.is_empty() {
        return (0..n as u32).collect();
    }
    let p = Prepared::new(q, ix);
    let bounds = chunk_bounds(n);
    let parts: Vec<Vec<u32>> = match &p.driver {
        Some(d) => {
            let keys: Vec<KeyScan> = d.keys.iter().map(|k| KeyScan::new(ix, &k.bytes, k.lead)).collect();
            let skip = d.exact.then_some(d.term);
            let trivial = d.exact && q.terms.len() == 1;
            bounds
                .par_iter()
                .map_init(Scratch::default, |sc, &(a, b)| {
                    if cancel.load(Ordering::Relaxed) {
                        return Vec::new();
                    }
                    let cands: Vec<Vec<u32>> = keys.iter().map(|k| k.scan(ix, a, b)).collect();
                    let mut c = union_sorted(cands);
                    if !trivial {
                        c.retain(|&i| p.matches(ix, i, skip, sc));
                    }
                    c
                })
                .collect()
        }
        None => match p.single() {
            // `windows\system32`: one path atom → tight loop, no generic term walk.
            Some((Atom::Path(_), ti)) if !q.terms[ti].neg && p.paths[ti][0].is_some() => {
                let pp = p.paths[ti][0].as_ref().unwrap();
                let topo = ix.dir_topo();
                bounds
                    .par_iter()
                    .map(|&(a, b)| {
                        if cancel.load(Ordering::Relaxed) {
                            return Vec::new();
                        }
                        (a as u32..b as u32).filter(|&i| pp.matches_in(ix, topo, i)).collect()
                    })
                    .collect()
            }
            // `!foo`: complement of a blob scan (one SIMD pass instead of a memmem per name).
            Some((Atom::Name(Pat::Substr(s)), ti)) if q.terms[ti].neg && !s.is_empty() => {
                let key = KeyScan::new(ix, s, 0);
                bounds
                    .par_iter()
                    .map(|&(a, b)| {
                        if cancel.load(Ordering::Relaxed) {
                            return Vec::new();
                        }
                        let hits = key.scan(ix, a, b);
                        let mut out = Vec::with_capacity(b - a - hits.len());
                        let mut next = a as u32;
                        for h in hits {
                            out.extend(next..h);
                            next = h + 1;
                        }
                        out.extend(next..b as u32);
                        out
                    })
                    .collect()
            }
            _ => bounds
                .par_iter()
                .map_init(Scratch::default, |sc, &(a, b)| {
                    if cancel.load(Ordering::Relaxed) {
                        return Vec::new();
                    }
                    (a as u32..b as u32).filter(|&i| p.matches(ix, i, None, sc)).collect()
                })
                .collect(),
        },
    };
    concat(parts)
}

/// Re-filter a previous result set (incremental search while typing).
pub fn search_within(ix: &Index, q: &Query, prev: &[u32], cancel: &AtomicBool) -> Vec<u32> {
    if q.is_empty() {
        return prev.to_vec();
    }
    let p = Prepared::new(q, ix);
    let step = (prev.len() / (rayon::current_num_threads() * CHUNKS_PER_THREAD)).max(4096);
    let parts: Vec<Vec<u32>> = prev
        .par_chunks(step)
        .map_init(Scratch::default, |sc, ch| {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            ch.iter().copied().filter(|&i| p.matches(ix, i, None, sc)).collect()
        })
        .collect();
    concat(parts)
}

fn concat(parts: Vec<Vec<u32>>) -> Vec<u32> {
    if parts.len() == 1 {
        return parts.into_iter().next().unwrap();
    }
    let total: usize = parts.iter().map(Vec::len).sum();
    let mut out: Vec<u32> = Vec::with_capacity(total);
    if total < 64 * 1024 {
        for p in parts {
            out.extend_from_slice(&p);
        }
        return out;
    }
    // Big result: the fresh allocation's pages are first touched (page faults)
    // and filled by all threads at once instead of one.
    {
        let mut dst: Vec<&mut [std::mem::MaybeUninit<u32>]> = Vec::with_capacity(parts.len());
        let mut rest = &mut out.spare_capacity_mut()[..total];
        for p in &parts {
            let (d, r) = std::mem::take(&mut rest).split_at_mut(p.len());
            dst.push(d);
            rest = r;
        }
        dst.into_par_iter().zip(parts.par_iter()).for_each(|(d, s)| {
            for (x, &v) in d.iter_mut().zip(s.iter()) {
                x.write(v);
            }
        });
    }
    // SAFETY: every slot in 0..total was written above (the parts tile it exactly).
    unsafe { out.set_len(total) };
    out
}

/// One literal key of a scan, with the strategy picked per query:
/// * sparse keys → `memmem` jumps between hits (skips most of the blob);
/// * dense keys (`a`, `e`, `dll`: tens of thousands of hits) → one linear
///   AVX2 pass (`dense::scan`) that maps hits to entries with popcounts, so
///   the cost no longer grows with the number of hits.
struct KeyScan<'k> {
    bytes: &'k [u8],
    finder: Finder<'k>,
    lead: usize,
    dense: bool,
}

/// Occurrences per MiB of names above which the linear pass beats memmem
/// (measured on 478k entries: `a`/`e` ≈ 25k/MiB win by 20%, `re` ≈ 6k/MiB
/// loses by 15%; break-even near `o` ≈ 20k/MiB).
const DENSE_PER_MIB: usize = 16_000;

impl<'k> KeyScan<'k> {
    fn new(ix: &Index, bytes: &'k [u8], lead: usize) -> KeyScan<'k> {
        let finder = Finder::new(bytes);
        let dense = !bytes.is_empty() && !bytes.contains(&0) && dense::available() && Self::is_dense(ix, &finder);
        KeyScan { bytes, finder, lead, dense }
    }

    /// Sample 8 evenly spread 8 KiB windows (the blob is sorted by name, so
    /// one window would be biased); stop counting as soon as the density
    /// threshold is reached.
    fn is_dense(ix: &Index, finder: &Finder) -> bool {
        const W: usize = 8 * 1024;
        let blob = &ix.folded;
        if blob.len() < 256 * W {
            return false; // small index: memmem is always fast enough
        }
        let need = DENSE_PER_MIB * 8 * W / (1 << 20);
        let step = blob.len() / 8;
        let mut seen = 0;
        for k in 0..8 {
            seen += finder.find_iter(&blob[k * step..k * step + W]).take(need - seen).count();
            if seen >= need {
                return true;
            }
        }
        false
    }

    /// Entries in [a, b) whose name contains the key, ascending, distinct.
    #[inline]
    fn scan(&self, ix: &Index, a: usize, b: usize) -> Vec<u32> {
        if self.dense {
            let mut out = Vec::with_capacity((b - a) / 2);
            // SAFETY: `dense` is only set when `dense::available()` (AVX2, POPCNT, BMI1).
            unsafe { dense::scan(&ix.folded, &ix.folded_off, self.bytes, a, b, &mut out) };
            out
        } else {
            scan_chunk(ix, &self.finder, self.lead, a, b, |_| true)
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod dense {
    use std::arch::x86_64::*;

    pub fn available() -> bool {
        use std::sync::OnceLock;
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| is_x86_feature_detected!("avx2") && is_x86_feature_detected!("popcnt") && is_x86_feature_detected!("bmi1"))
    }

    #[inline(always)]
    unsafe fn mask64(lo: __m256i, hi: __m256i) -> u64 {
        unsafe { (_mm256_movemask_epi8(lo) as u32 as u64) | ((_mm256_movemask_epi8(hi) as u32 as u64) << 32) }
    }

    /// Append the entries in [a, b) whose name contains `needle` (no NUL, non-empty).
    ///
    /// Blob layout: `\0 name_a \0 name_a+1 \0 …`; `off[e]` = first byte of entry e.
    /// Per 64-byte block: mask of bytes equal to the needle's first byte AND of
    /// bytes equal to its last byte shifted by len-1 (candidates), plus a mask
    /// of NULs. A candidate at bit `k` belongs to entry `base + popcnt(nul
    /// below k)`; a needle without NUL never spans two entries.
    #[target_feature(enable = "avx2,popcnt,bmi1")]
    pub unsafe fn scan(blob: &[u8], off: &[u32], needle: &[u8], a: usize, b: usize, out: &mut Vec<u32>) {
        let l = needle.len();
        let start = off[a] as usize - 1; // the NUL before entry a
        let end = off[b] as usize; // exclusive; end-1 is the NUL after entry b-1
        let ptr = blob.as_ptr();
        // SAFETY (all loads): every 32-byte load below lies inside [p, p + 63 + l) ⊆ [start, end).
        let (vf, vl, vz) = (_mm256_set1_epi8(needle[0] as i8), _mm256_set1_epi8(needle[l - 1] as i8), _mm256_setzero_si256());
        let mid = &needle[1..l.saturating_sub(1).max(1)];
        // Entry of a non-NUL byte q = base + #NULs in [p, q); the NUL at `start` makes it `a`.
        let mut base = a as i64 - 1;
        let mut last = u32::MAX;
        let mut p = start;
        while p + 63 + l <= end {
            let (b0, b1) = unsafe { (_mm256_loadu_si256(ptr.add(p) as *const __m256i), _mm256_loadu_si256(ptr.add(p + 32) as *const __m256i)) };
            let nul = unsafe { mask64(_mm256_cmpeq_epi8(b0, vz), _mm256_cmpeq_epi8(b1, vz)) };
            let mut cand = unsafe { mask64(_mm256_cmpeq_epi8(b0, vf), _mm256_cmpeq_epi8(b1, vf)) };
            if l > 1 && cand != 0 {
                let (e0, e1) = unsafe {
                    (_mm256_loadu_si256(ptr.add(p + l - 1) as *const __m256i), _mm256_loadu_si256(ptr.add(p + l - 1 + 32) as *const __m256i))
                };
                cand &= unsafe { mask64(_mm256_cmpeq_epi8(e0, vl), _mm256_cmpeq_epi8(e1, vl)) };
            }
            while cand != 0 {
                let k = cand.trailing_zeros();
                let q = p + k as usize;
                if l <= 2 || &blob[q + 1..q + l - 1] == mid {
                    let e = (base + (nul & ((1u64 << k) - 1)).count_ones() as i64) as u32;
                    if e != last {
                        out.push(e);
                        last = e;
                    }
                    // The entry is decided: drop its other candidates (up to the next NUL).
                    let above = nul & !((2u64 << k).wrapping_sub(1));
                    if above == 0 {
                        break; // the rest of the block belongs to this entry
                    }
                    cand &= !((1u64 << above.trailing_zeros()) - 1);
                } else {
                    cand &= cand - 1;
                }
            }
            base += nul.count_ones() as i64;
            p += 64;
        }
        // Tail (< 64 + l bytes): scalar.
        while p < end {
            if blob[p] == 0 {
                base += 1;
            } else if p + l <= end && blob[p..p + l] == *needle {
                let e = base as u32;
                if e != last {
                    out.push(e);
                    last = e;
                }
            }
            p += 1;
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod dense {
    pub fn available() -> bool {
        false
    }
    pub unsafe fn scan(_: &[u8], _: &[u32], _: &[u8], _: usize, _: usize, _: &mut Vec<u32>) {
        unreachable!()
    }
}

/// Scan entries [a, b) of the blob with `finder`, calling `accept` once per
/// distinct entry that contains the key.
#[inline]
fn scan_chunk(
    ix: &Index,
    finder: &Finder,
    lead: usize,
    a: usize,
    b: usize,
    mut accept: impl FnMut(u32) -> bool,
) -> Vec<u32> {
    let off = &ix.folded_off;
    let blob = &ix.folded;
    // Byte range: the NUL before entry a .. the NUL after entry b-1 (inclusive).
    let start = off[a] as usize - 1;
    let end = off[b] as usize; // exclusive; off[b]-1 is the trailing NUL of b-1
    let hay = &blob[..end];
    let mut out = Vec::new();
    let mut pos = start;
    let mut cur = a; // entries before `cur` are already consumed
    while pos < end {
        let Some(h) = finder.find(&hay[pos..]) else { break };
        let q = (pos + h + lead) as u32;
        // Entry containing byte q: last e in [cur, b) with off[e] <= q (off[cur] <= q
        // holds). Dense keys hit `cur` or a neighbour, so gallop forward from
        // `cur` (1–3 probes) instead of bisecting the whole chunk (~14 probes).
        let mut lo = cur;
        let mut step = 1;
        let mut hi = cur + 1;
        while hi < b && off[hi] <= q {
            lo = hi;
            step *= 2;
            hi = lo + step;
        }
        let hi = hi.min(b);
        let e = lo + off[lo..hi].partition_point(|&o| o <= q) - 1;
        if accept(e as u32) {
            out.push(e as u32);
        }
        cur = e + 1;
        if cur >= b {
            break;
        }
        pos = off[cur] as usize - 1; // the NUL that starts entry `cur`
    }
    out
}

/// Total count without materializing (used by benchmarks).
pub fn count(ix: &Index, q: &Query) -> usize {
    search(ix, q, &AtomicBool::new(false)).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    fn names(ix: &Index, q: &str) -> Vec<String> {
        search(ix, &Query::parse(q), &AtomicBool::new(false))
            .into_iter()
            .map(|i| ix.name(i).to_string())
            .collect()
    }

    #[test]
    fn substring_and_order() {
        let ix = sample();
        // Equal folded names keep scan order (stable tie-break by temp id).
        assert_eq!(names(&ix, "NOTEPAD"), vec!["notepad.exe", "Notepad.exe"]);
        assert_eq!(names(&ix, "pdf"), vec!["pdf", "report.pdf", "Отчёт 2026.pdf"]);
        assert_eq!(names(&ix, "ОТЧЁТ"), vec!["Отчёт 2026.pdf"]);
        assert_eq!(names(&ix, "\"т 20\""), vec!["Отчёт 2026.pdf"]);
    }

    #[test]
    fn anchors_globs_ext() {
        let ix = sample();
        assert_eq!(names(&ix, "*.pdf"), vec!["report.pdf", "Отчёт 2026.pdf"]);
        assert_eq!(names(&ix, "re*"), vec!["readme.TXT", "report.pdf"]);
        assert_eq!(names(&ix, "pdf*"), vec!["pdf"]);
        assert_eq!(names(&ix, "ext:pdf"), vec!["report.pdf", "Отчёт 2026.pdf"]);
        assert_eq!(names(&ix, "ext:txt;md"), vec!["notes.md", "readme.TXT"]);
        assert_eq!(names(&ix, "*"), names(&ix, ""));
    }

    #[test]
    fn boolean_and_types() {
        let ix = sample();
        assert_eq!(names(&ix, "pdf !report"), vec!["pdf", "Отчёт 2026.pdf"]);
        assert_eq!(names(&ix, "pdf file:"), vec!["report.pdf", "Отчёт 2026.pdf"]);
        assert_eq!(names(&ix, "folder:pdf"), vec!["pdf"]);
        assert_eq!(names(&ix, "notes|readme"), vec!["notes.md", "readme.TXT"]);
        assert_eq!(names(&ix, "e r"), vec!["projects", "readme.TXT", "report.pdf"]);
    }

    #[test]
    fn paths() {
        let ix = sample();
        assert_eq!(names(&ix, r"system32\note"), vec!["notepad.exe"]);
        assert_eq!(names(&ix, "windows\\ exe"), vec!["notepad.exe", "Notepad.exe"]);
        assert_eq!(names(&ix, "d:/"), vec!["notes.md", "projects"]);
    }

    #[test]
    fn path_accel_equals_full_path() {
        use crate::scan::{VolumeScan, ATTR_DIRECTORY};
        let mut s = VolumeScan::new("C:");
        let mut x: u64 = 12345;
        let mut dirs = vec![0u32];
        let words = ["ab", "b", "abc", "Ca", "x", "ab\u{0430}", "bc"];
        for i in 0..3000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let parent = dirs[(x as usize) % dirs.len()];
            let w = words[(x >> 20) as usize % words.len()];
            if i % 3 == 0 {
                dirs.push(s.push(parent, w, ATTR_DIRECTORY, 0, 0));
            } else {
                s.push(parent, &format!("{w}{i}"), 0, 0, 0);
            }
        }
        let ix = crate::index::build(vec![s]);
        assert!(!ix.dir_topo().sep_in_names);
        check_paths(&ix);
    }

    /// Path search vs. naive "folded full path contains P", for the segment
    /// automaton (≤ 7 separators), its fallback (more separators) and edge
    /// shapes (leading/trailing/double separators, mixed with other terms).
    fn check_paths(ix: &Index) {
        let c = AtomicBool::new(false);
        let pats = [
            "ab\\b", "b\\ab", "c:\\ab", "\\", "a\\ab\\b", "\u{0430}\\b", "c\\ca\\x\\ab", "zz\\q", "ab\\", "\\ab",
            "\\\\", "ab\\\\b", "c:\\", "b\\b\\b\\b\\b\\b\\b\\b\\b", "x\\x\\x\\x\\x\\x\\x\\x", "b\\ab\\ab\\ab\\ab\\ab\\ab\\a",
        ];
        for pat in pats {
            let needle = crate::fold::fold(pat);
            let (mut st, mut buf) = (Vec::new(), Vec::new());
            let naive: Vec<u32> = (0..ix.len() as u32)
                .filter(|&i| {
                    ix.folded_path_into(i, &mut st, &mut buf);
                    memchr::memmem::find(&buf, &needle).is_some()
                })
                .collect();
            assert_eq!(search(ix, &Query::parse(pat), &c), naive, "pattern {pat}");
            // with another term (generic per-entry path) and negated
            let with_b: Vec<u32> = naive.iter().copied().filter(|&i| ix.folded_name(i).contains(&b'b')).collect();
            assert_eq!(search(ix, &Query::parse(&format!("\"{pat}\" b")), &c), with_b, "pattern {pat} + b");
            let not: Vec<u32> = (0..ix.len() as u32).filter(|i| naive.binary_search(i).is_err()).collect();
            assert_eq!(search(ix, &Query::parse(&format!("!\"{pat}\"")), &c), not, "pattern !{pat}");
        }
    }

    #[test]
    fn path_fallback_when_names_contain_backslash() {
        use crate::scan::{VolumeScan, ATTR_DIRECTORY};
        // POSIX-namespace NTFS names may contain '\': the segment model must not be used.
        let mut s = VolumeScan::new("C:");
        let d = s.push(0, "ab", ATTR_DIRECTORY, 0, 0);
        let e = s.push(d, "x\\ab", ATTR_DIRECTORY, 0, 0);
        s.push(e, "b1", 0, 0, 0);
        s.push(0, "b\\ab2", 0, 0, 0);
        s.push(d, "bc", 0, 0, 0);
        let ix = crate::index::build(vec![s]);
        assert!(ix.dir_topo().sep_in_names);
        check_paths(&ix);
    }

    #[test]
    fn dense_kernel_equals_memmem() {
        use crate::scan::{VolumeScan, ATTR_DIRECTORY};
        if !dense::available() {
            return;
        }
        let mut s = VolumeScan::new("X:");
        let d = s.push(0, "dir", ATTR_DIRECTORY, 0, 0);
        let mut x: u64 = 99;
        for i in 0..60_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // varied lengths (1..90) so matches straddle 64-byte blocks and chunk ends
            let len = 1 + (x % 90) as usize;
            let name: String = (0..len).map(|k| (b'a' + ((x >> (k % 40)) % 6) as u8) as char).collect();
            s.push(d, &format!("{name}{i}"), 0, 0, 0);
        }
        let ix = crate::index::build(vec![s]);
        let n = ix.len();
        for key in ["a", "ab", "abc", "ffff", "a1", "e5", "abcdefabcdef", "9"] {
            let f = Finder::new(key.as_bytes());
            for (a, b) in [(0, n), (1, n / 3), (n / 3, n), (n - 1, n), (5, 6)] {
                let mut fast = Vec::new();
                unsafe { dense::scan(&ix.folded, &ix.folded_off, key.as_bytes(), a, b, &mut fast) };
                let slow = scan_chunk(&ix, &f, 0, a, b, |_| true);
                assert_eq!(fast, slow, "key {key} range {a}..{b}");
            }
        }
    }

    #[test]
    fn within() {
        let ix = sample();
        let c = AtomicBool::new(false);
        let prev = search(&ix, &Query::parse("p"), &c);
        let r = search_within(&ix, &Query::parse("pd"), &prev, &c);
        assert_eq!(r, search(&ix, &Query::parse("pd"), &c));
    }

    #[test]
    fn chunked_scan_matches_naive() {
        // Many entries to force multiple chunks.
        use crate::scan::{VolumeScan, ATTR_DIRECTORY};
        let mut s = VolumeScan::new("X:");
        let d = s.push(0, "dir", ATTR_DIRECTORY, 0, 0);
        for i in 0..200_000u32 {
            s.push(d, &format!("file_{i:06}_{}.dat", i % 97), 0, i as u64, 0);
        }
        let ix = crate::index::build(vec![s]);
        let c = AtomicBool::new(false);
        for q in ["_42", "\"1.dat\"", "file_00", "*_7.dat", "ext:dat", "99", "zzz"] {
            let pq = Query::parse(q);
            let fast = search(&ix, &pq, &c);
            let p = Prepared::new(&pq, &ix);
            let mut sc = Scratch::default();
            let naive: Vec<u32> = (0..ix.len() as u32).filter(|&i| p.matches(&ix, i, None, &mut sc)).collect();
            assert_eq!(fast, naive, "query {q}");
        }
    }
}
