//! Live index engine.
//!
//! * `Updater` — owns the current index snapshot, accumulates changed records
//!   per volume (keyed by drive letter), applies them with an adaptive
//!   debounce. Source-agnostic: fed either by local journals (`Live`, when we
//!   are elevated) or by the elevated broker over a pipe (`broker`).
//! * `Live` — `Updater` + local USN journals (CLI `watch`/`selftest-live`,
//!   elevated GUI).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use us_core::live::{self, Changes, RecState};
use us_core::scan::ScanKind;
use us_core::{Index, VolumeScan};
use us_win::mft;
use us_win::usn::{Journal, JournalError};

pub struct ApplyStats {
    pub records: usize,
    pub ms: f64,
    pub entries: usize,
}

pub fn letter(root: &str) -> char {
    root.chars().next().unwrap_or('?').to_ascii_uppercase()
}

pub struct Updater {
    pub ix: Arc<Index>,
    pending: HashMap<char, Changes>,
    cursors: HashMap<char, (u64, i64)>,
    pending_since: Option<Instant>,
    pub last_apply: Duration,
}

impl Updater {
    pub fn new(ix: Index) -> Updater {
        Updater { ix: Arc::new(ix), pending: HashMap::new(), cursors: HashMap::new(), pending_since: None, last_apply: Duration::ZERO }
    }

    pub fn add(&mut self, vol: char, rec: u32, st: Option<RecState>) {
        self.pending.entry(vol).or_default().insert(rec, st);
        self.pending_since.get_or_insert_with(Instant::now);
    }

    pub fn set_cursor(&mut self, vol: char, journal_id: u64, next_usn: i64) {
        self.cursors.insert(vol, (journal_id, next_usn));
    }

    pub fn has_pending(&self) -> bool {
        self.pending_since.is_some()
    }

    /// Replace/append whole volumes (fresh MFT scans). Pending changes for
    /// those volumes are dropped: the scan already reflects them.
    pub fn upsert(&mut self, scans: Vec<VolumeScan>) {
        for s in &scans {
            let l = letter(&s.root);
            self.pending.remove(&l);
            self.cursors.remove(&l);
        }
        self.ix = Arc::new(live::upsert_volumes(&self.ix, scans));
    }

    /// Apply pending changes once they are at least `debounce` old (and at
    /// least 4× the cost of the last rebuild, so rebuilds never dominate CPU).
    pub fn maybe_apply(&mut self, debounce: Duration) -> Option<ApplyStats> {
        let since = self.pending_since?;
        if since.elapsed() < debounce.max(self.last_apply * 4) {
            return None;
        }
        let t = Instant::now();
        let mut records = 0;
        let mut changes = Vec::with_capacity(self.ix.volumes.len());
        let mut metas = Vec::with_capacity(self.ix.volumes.len());
        for vi in &self.ix.volumes {
            let l = letter(&vi.root);
            let c = self.pending.remove(&l).unwrap_or_default();
            records += c.len();
            changes.push(c);
            let mut m = vi.meta.clone();
            if let Some(&(id, usn)) = self.cursors.get(&l) {
                if m.kind == ScanKind::Mft && id == m.journal_id {
                    m.next_usn = usn;
                }
            }
            metas.push(m);
        }
        self.pending.clear(); // changes for volumes not in the index are meaningless
        self.ix = Arc::new(live::apply(&self.ix, &changes, &metas));
        self.pending_since = None;
        self.last_apply = t.elapsed();
        Some(ApplyStats { records, ms: self.last_apply.as_secs_f64() * 1e3, entries: self.ix.len() })
    }
}

/// Local journals (we are elevated) feeding an `Updater`.
pub struct Live {
    pub up: Updater,
    journals: Vec<Journal>,
    pub log: Vec<String>,
    touched: Vec<u32>,
}

impl Live {
    pub fn new(ix: Index) -> Live {
        let mut s = Live { up: Updater::new(ix), journals: Vec::new(), log: Vec::new(), touched: Vec::new() };
        let roots: Vec<(String, u64, i64, ScanKind)> = s
            .up
            .ix
            .volumes
            .iter()
            .map(|v| (v.root.clone(), v.meta.journal_id, v.meta.next_usn, v.meta.kind))
            .collect();
        let mut rescan = Vec::new();
        for (root, id, usn, kind) in roots {
            if kind != ScanKind::Mft {
                continue;
            }
            match Journal::open(letter(&root), id, usn) {
                Ok(j) => s.journals.push(j),
                Err(JournalError::Reset(why)) => {
                    s.log.push(format!("{root}: {why} → rescan"));
                    rescan.push(letter(&root));
                }
                Err(JournalError::Io(e)) => s.log.push(format!("{root}: journal unavailable: {e}")),
            }
        }
        s.rescan(&rescan);
        s
    }

    pub fn ix(&self) -> &Arc<Index> {
        &self.up.ix
    }

    pub fn watching(&self) -> usize {
        self.journals.len()
    }

    fn rescan(&mut self, vols: &[char]) {
        if vols.is_empty() {
            return;
        }
        let mut scans = Vec::new();
        for &l in vols {
            self.journals.retain(|j| j.letter != l);
            match mft::scan_mft(l, &mft::MftOptions::default()) {
                Ok((scan, _)) => scans.push(scan),
                Err(e) => self.log.push(format!("{l}: rescan failed: {e}")),
            }
        }
        let metas: Vec<(char, u64, i64)> = scans.iter().map(|s| (letter(&s.root), s.meta.journal_id, s.meta.next_usn)).collect();
        self.up.upsert(scans);
        for (l, id, usn) in metas {
            if let Ok(j) = Journal::open(l, id, usn) {
                self.journals.push(j);
            }
        }
    }

    /// Read all journals; fetch the new state of every touched record.
    pub fn poll(&mut self) -> usize {
        let mut fetched = 0;
        let mut rescan = Vec::new();
        for j in &mut self.journals {
            self.touched.clear();
            match j.poll(&mut self.touched) {
                Ok(_) => {}
                Err(JournalError::Reset(why)) => {
                    self.log.push(format!("{}: {why} → rescan", j.letter));
                    rescan.push(j.letter);
                    continue;
                }
                Err(JournalError::Io(e)) => {
                    self.log.push(format!("{}: journal read error: {e}", j.letter));
                    continue;
                }
            }
            self.touched.sort_unstable();
            self.touched.dedup();
            for &r in &self.touched {
                match j.fetch(r) {
                    Ok(st) => {
                        self.up.add(j.letter, r, st);
                        fetched += 1;
                    }
                    Err(e) => self.log.push(format!("{}: record {r}: {e}", j.letter)),
                }
            }
            self.up.set_cursor(j.letter, j.journal_id, j.next_usn);
        }
        self.rescan(&rescan);
        fetched
    }

    pub fn maybe_apply(&mut self, debounce: Duration) -> Option<ApplyStats> {
        self.up.maybe_apply(debounce)
    }

    pub fn last_apply(&self) -> Duration {
        self.up.last_apply
    }
}
