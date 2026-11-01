//! `us watch` and `us selftest-live` (both need admin: they read the USN journal).

use crate::engine::Live;
use crate::{data_dir, load_index, opt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use us_core::{search, store, Query};

fn count(live: &Live, q: &str) -> usize {
    search::search(live.ix(), &Query::parse(q), &AtomicBool::new(false)).len()
}

fn flush_log(live: &mut Live) {
    for l in live.log.drain(..) {
        println!("  [log] {l}");
    }
}

pub fn cmd_watch(args: &[String]) -> i32 {
    let secs: u64 = opt(args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(60);
    let q: String = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !a.starts_with("--") && !(*i > 0 && args[i - 1].starts_with("--")))
        .map(|(_, a)| a.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let Some(ix) = load_index(args) else { return 1 };
    let t0 = Instant::now();
    let mut live = Live::new(ix);
    println!("watching {} journal(s), startup {:.0} ms (incl. catch-up), {} entries", live.watching(), t0.elapsed().as_secs_f64() * 1e3, live.ix().len());
    flush_log(&mut live);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        live.poll();
        if let Some(st) = live.maybe_apply(Duration::from_millis(200)) {
            let c = if q.is_empty() { String::new() } else { format!(", '{q}' → {}", count(&live, &q)) };
            println!("applied {} records in {:.1} ms → {} entries{c}", st.records, st.ms, st.entries);
        }
        flush_log(&mut live);
        std::thread::sleep(Duration::from_millis(250));
    }
    let p = data_dir(args).join("index.usdb");
    match store::save(live.ix(), &p) {
        Ok(()) => println!("saved {}", p.display()),
        Err(e) => println!("save failed: {e}"),
    }
    0
}

struct Ctx {
    live: Live,
    fails: usize,
    lat: Vec<f64>,
}

impl Ctx {
    /// Poll + apply until `pred` holds; returns latency in ms.
    fn wait(&mut self, step: &str, pred: impl Fn(&Live) -> bool) {
        let t = Instant::now();
        loop {
            self.live.poll();
            self.live.maybe_apply(Duration::ZERO);
            if pred(&self.live) {
                let ms = t.elapsed().as_secs_f64() * 1e3;
                self.lat.push(ms);
                println!("  PASS {step:<48} visible after {ms:>7.1} ms (last rebuild {:.1} ms)", self.live.last_apply().as_secs_f64() * 1e3);
                break;
            }
            if t.elapsed() > Duration::from_secs(10) {
                println!("  FAIL {step:<48} not visible after 10 s");
                self.fails += 1;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        flush_log(&mut self.live);
    }
}

fn path_of(live: &Live, q: &str) -> Vec<String> {
    let mut p = String::new();
    search::search(live.ix(), &Query::parse(q), &AtomicBool::new(false))
        .into_iter()
        .map(|i| {
            live.ix().full_path_into(i, &mut p);
            p.clone()
        })
        .collect()
}

fn ends(live: &Live, q: &str, suffix: &Path) -> bool {
    let s = suffix.to_string_lossy().to_lowercase();
    let v = path_of(live, q);
    v.len() == 1 && v[0].to_lowercase().ends_with(&s)
}

pub fn cmd_selftest_live(args: &[String]) -> i32 {
    let Some(ix) = load_index(args) else { return 1 };
    let mut ctx = Ctx { live: Live::new(ix), fails: 0, lat: Vec::new() };
    flush_log(&mut ctx.live);
    if ctx.live.watching() == 0 {
        println!("no USN journal could be opened (not elevated, or index built by directory walk)");
        return 1;
    }
    let tok = format!("zq{}x", std::process::id());
    let base: PathBuf = data_dir(args).join(format!("livetest-{tok}"));
    let rel = |p: &Path| p.strip_prefix(data_dir(args)).unwrap_or(p).to_path_buf();
    println!("live self-test in {} (token {tok})", base.display());

    let dir_a = base.join(format!("dirA_{tok}"));
    std::fs::create_dir_all(&dir_a).unwrap();
    let f1 = dir_a.join(format!("f1_{tok}.txt"));
    std::fs::write(&f1, b"0123456789").unwrap();
    let r = rel(&f1);
    ctx.wait("create dir + file", |l| ends(l, &format!("f1_{tok}"), &r));

    std::fs::write(&f1, vec![b'x'; 5000]).unwrap();
    let q1 = format!("f1_{tok}");
    ctx.wait("size update (10 → 5000 bytes)", |l| {
        let res = search::search(l.ix(), &Query::parse(&q1), &AtomicBool::new(false));
        res.len() == 1 && l.ix().size[res[0] as usize] == 5000
    });

    let f2 = dir_a.join(format!("f2_{tok}.txt"));
    std::fs::rename(&f1, &f2).unwrap();
    ctx.wait("rename file", |l| count(l, &format!("f1_{tok}")) == 0 && count(l, &format!("f2_{tok}")) == 1);

    let dir_b = base.join(format!("dirB_{tok}"));
    std::fs::rename(&dir_a, &dir_b).unwrap();
    let r = rel(&dir_b.join(format!("f2_{tok}.txt")));
    ctx.wait("rename parent dir (child path follows)", |l| ends(l, &format!("f2_{tok}"), &r));

    let dir_c = dir_b.join(format!("dirC_{tok}"));
    std::fs::create_dir(&dir_c).unwrap();
    let f3 = dir_c.join(format!("f2_{tok}.txt"));
    std::fs::rename(dir_b.join(format!("f2_{tok}.txt")), &f3).unwrap();
    let pq = format!("dirc_{tok}\\f2_{tok}");
    ctx.wait("mkdir + move file into it (path query)", |l| count(l, &pq) == 1);

    let link = base.join(format!("link_{tok}.txt"));
    std::fs::hard_link(&f3, &link).unwrap();
    ctx.wait("hard link (both names present)", |l| count(l, &format!("link_{tok}")) == 1 && count(l, &format!("f2_{tok}")) == 1);

    let many = base.join(format!("many_{tok}"));
    std::fs::create_dir(&many).unwrap();
    for i in 0..1000 {
        std::fs::write(many.join(format!("m{i}_{tok}.dat")), b"").unwrap();
    }
    ctx.wait("1000 files burst", |l| count(l, &format!("_{tok}.dat")) == 1000);

    std::fs::remove_dir_all(&base).unwrap();
    ctx.wait("delete whole tree", |l| count(l, &tok) == 0);

    let mut s = ctx.lat.clone();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{} — {} steps, {} failed; latency median {:.1} ms, max {:.1} ms",
        if ctx.fails == 0 { "LIVE SELF-TEST OK" } else { "LIVE SELF-TEST FAILED" },
        ctx.lat.len() + ctx.fails,
        ctx.fails,
        s.get(s.len() / 2).copied().unwrap_or(0.0),
        s.last().copied().unwrap_or(0.0)
    );
    if ctx.fails == 0 { 0 } else { 1 }
}
