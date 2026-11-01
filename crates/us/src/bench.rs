//! Benchmarks on a fixed query set: in-process (`usc bench`) and through the
//! `usc serve` IPC (`usc bench-serve`).

use crate::{load_index, opt};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use us_core::{query, search, Query};

pub const QUERIES: &[&str] = &[
    "a",
    "e",
    "exe",
    "dll",
    "windows",
    "system32",
    "notepad.exe",
    "readme",
    "config",
    "microsoft",
    "zzqqxx",
    "*.dll",
    "*.txt",
    "ext:jpg;png",
    "win*",
    "set*.exe",
    "micro soft",
    "windows\\system32",
    "system32\\ dll",
    "ows\\sys",
    "!e",
    "dll|exe",
];

fn stats(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[0], v[v.len() / 2])
}

pub fn cmd_bench(args: &[String]) -> i32 {
    let rounds: usize = opt(args, "--rounds").and_then(|s| s.parse().ok()).unwrap_or(15);
    let Some(ix) = load_index(args) else { return 1 };
    let cancel = AtomicBool::new(false);
    println!("entries: {}, threads: {}, rounds: {rounds}", ix.len(), rayon::current_num_threads());
    println!("{:<14} {:>10} {:>10} {:>10}", "query", "results", "min ms", "median ms");
    let parsed: Vec<Query> = QUERIES.iter().map(|q| Query::parse(q)).collect();
    let mut times = vec![Vec::new(); QUERIES.len()];
    let mut counts = vec![0usize; QUERIES.len()];
    for r in 0..rounds + 2 {
        for (qi, q) in parsed.iter().enumerate() {
            let t = Instant::now();
            let res = search::search(&ix, q, &cancel);
            let dt = t.elapsed().as_secs_f64() * 1e3;
            counts[qi] = res.len();
            if r >= 2 {
                times[qi].push(dt);
            }
        }
    }
    let mut total_med = 0.0;
    for (qi, q) in QUERIES.iter().enumerate() {
        let (mn, md) = stats(times[qi].clone());
        total_med += md;
        println!("{:<14} {:>10} {:>10.3} {:>10.3}", q, counts[qi], mn, md);
    }
    println!("sum of medians: {total_med:.2} ms");

    // Typing simulation: "notepad" keystroke by keystroke with result narrowing.
    for word in ["notepad", "microsoft"] {
        let t = Instant::now();
        let mut prev: Option<(String, Vec<u32>)> = None;
        for k in 1..=word.len() {
            let s = &word[..k];
            let q = Query::parse(s);
            let res = match &prev {
                Some((ps, pr)) if query::is_refinement(ps, s) => search::search_within(&ix, &q, pr, &cancel),
                _ => search::search(&ix, &q, &cancel),
            };
            prev = Some((s.to_string(), res));
        }
        println!(
            "typing '{word}' ({} keystrokes, incremental): {:.3} ms total, {} final results",
            word.len(),
            t.elapsed().as_secs_f64() * 1e3,
            prev.unwrap().1.len()
        );
    }
    0
}

/// `usc bench-serve`: the same query set through `usc serve` (child process,
/// JSON lines over pipes, 100 rows with full paths per answer) — what an
/// integrating program actually sees, including IPC and serialization.
pub fn cmd_bench_serve(args: &[String]) -> i32 {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let rounds: usize = opt(args, "--rounds").and_then(|s| s.parse().ok()).unwrap_or(15);
    let data = crate::cli::data_dir(args);
    let exe = std::env::current_exe().expect("exe path");
    let mut child = match Command::new(exe)
        .args(["serve", "--live", "off", "--data"])
        .arg(&data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot start usc serve: {e}");
            return 1;
        }
    };
    let mut tx = child.stdin.take().unwrap();
    let mut rx = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    rx.read_line(&mut line).expect("hello");
    let mut roundtrip = |req: &str, line: &mut String| -> f64 {
        let t = Instant::now();
        tx.write_all(req.as_bytes()).and_then(|_| tx.write_all(b"\n")).and_then(|_| tx.flush()).expect("write");
        line.clear();
        rx.read_line(line).expect("read");
        t.elapsed().as_secs_f64() * 1e3
    };
    // Wait for the index (first search blocks until it is loaded).
    let t_ready = roundtrip(r#"{"cmd":"search","query":"zzqqxx","limit":1,"wait_ms":120000}"#, &mut line);
    println!("usc serve ready in {t_ready:.0} ms (load inside the child)");
    println!("usc serve IPC (stdin/stdout JSON, 100 rows: full path), rounds: {rounds}");
    println!("{:<14} {:>10} {:>10} {:>10}", "query", "results", "min ms", "median ms");
    let reqs: Vec<String> = QUERIES
        .iter()
        .map(|q| {
            let mut s = String::from(r#"{"cmd":"search","limit":100,"fields":["full"],"query":"#);
            crate::output::json_str(&mut s, q);
            s.push('}');
            s
        })
        .collect();
    let mut times = vec![Vec::new(); QUERIES.len()];
    let mut totals = vec![0u64; QUERIES.len()];
    for r in 0..rounds + 2 {
        for (qi, req) in reqs.iter().enumerate() {
            let dt = roundtrip(req, &mut line);
            if r >= 2 {
                times[qi].push(dt);
            }
            if let Some(p) = line.find("\"total\":") {
                totals[qi] = line[p + 8..].split(|c: char| !c.is_ascii_digit()).next().and_then(|s| s.parse().ok()).unwrap_or(0);
            }
        }
    }
    let mut total_med = 0.0;
    for (qi, q) in QUERIES.iter().enumerate() {
        let (mn, md) = stats(times[qi].clone());
        total_med += md;
        println!("{:<14} {:>10} {:>10.3} {:>10.3}", q, totals[qi], mn, md);
    }
    println!("sum of medians: {total_med:.2} ms");
    let _ = roundtrip(r#"{"cmd":"quit"}"#, &mut line);
    let _ = child.wait();
    0
}
