//! CLI (usc.exe) of unthinkable-search.
//!
//! Portable: every file it writes lives in `<exe dir>\data\` (or `--data DIR`).

use std::path::PathBuf;
use std::time::Instant;
use us_core::{store, Index, VolumeScan};
use us_win::{mft, volumes, walk};

use crate::{bench, selftest};

pub fn data_dir(args: &[String]) -> PathBuf {
    if let Some(i) = args.iter().position(|a| a == "--data") {
        if let Some(d) = args.get(i + 1) {
            return PathBuf::from(d);
        }
    }
    let exe = std::env::current_exe().expect("exe path");
    exe.parent().unwrap().join("data")
}

pub fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

fn usage() {
    eprintln!(
        "usc — unthinkable-search CLI (version {})\n\
         \n  usc index [--vol C,D] [--walk] [--threads N] [--block-kb K]  build index (MFT needs admin)\
         \n  usc search <query> [options]                                 search the saved index\
         \n      --format text|json|jsonl|tsv|csv|nul   (default text: one full path per line)\
         \n      --fields full,name,path,size,mtime,ctime,attrs,dir\
         \n      --sort name|path|size|mtime|ctime  --desc  --offset N  -n/--limit N (0 = all)\
         \n      --time iso|filetime|unix_ms  --count\
         \n  usc serve [--live auto|off] [--pipe [NAME]]                  JSON-lines server (stdin/stdout or named pipe)\
         \n  usc info [--json]                                            index statistics\
         \n  usc bench [--rounds R]                                       query latency benchmark\
         \n  usc bench-serve [--rounds R]                                 the same queries through `usc serve` (IPC)\
         \n  usc watch [--seconds N] [query]                              live USN updates (admin)\
         \n  usc selftest-live                                            create/rename/move/delete test (admin)\
         \n  usc menu [--invoke VERB] <path...>                           Explorer context menu items / run a verb\
         \n  common: --data DIR (default: <exe dir>\\data)\
         \n  docs: docs\\INTEGRATION.md",
        env!("CARGO_PKG_VERSION")
    );
}

pub fn main() -> i32 {
    us_win::util::harden_process();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        usage();
        return 2;
    };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "index" => cmd_index(rest),
        "search" => cmd_search(rest),
        "bench" => bench::cmd_bench(rest),
        "info" => cmd_info(rest),
        "watch" => selftest::cmd_watch(rest),
        "selftest-live" => selftest::cmd_selftest_live(rest),
        "gui" => crate::gui::run_with(rest),
        // Elevated metadata broker (see broker.rs); same as `us.exe --broker`.
        "--broker" => crate::broker::broker_main(rest),
        "serve" => crate::serve::cmd_serve(rest),
        "bench-serve" => bench::cmd_bench_serve(rest),
        "version" | "--version" | "-V" => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            0
        }
        "menu" => cmd_menu(rest),
        _ => {
            usage();
            2
        }
    };
    code
}

pub fn load_index(args: &[String]) -> Option<Index> {
    let p = data_dir(args).join("index.usdb");
    let t = Instant::now();
    match store::load(&p) {
        Ok(ix) => {
            eprintln!("loaded {} entries from {} in {:.1} ms", ix.len(), p.display(), t.elapsed().as_secs_f64() * 1e3);
            Some(ix)
        }
        Err(e) => {
            eprintln!("cannot load {}: {e} (run `us index` first)", p.display());
            None
        }
    }
}

fn cmd_index(args: &[String]) -> i32 {
    let force_walk = args.iter().any(|a| a == "--walk");
    let want: Option<Vec<char>> = opt(args, "--vol")
        .map(|v| v.split(',').filter_map(|s| s.trim().chars().next()).map(|c| c.to_ascii_uppercase()).collect());
    let mut mo = mft::MftOptions::default();
    if let Some(t) = opt(args, "--threads").and_then(|s| s.parse().ok()) {
        mo.threads = t;
    }
    if let Some(k) = opt(args, "--block-kb").and_then(|s| s.parse::<usize>().ok()) {
        mo.block_bytes = k * 1024;
    }
    let walk_threads: usize = opt(args, "--walk-threads").and_then(|s| s.parse().ok()).unwrap_or(16);
    let t0 = Instant::now();
    let scans = scan_volumes(want, force_walk, &mo, walk_threads, true);
    if scans.is_empty() {
        eprintln!("no volumes");
        return 1;
    }
    let t_scan = t0.elapsed();
    let t1 = Instant::now();
    let ix = us_core::build(scans);
    let t_build = t1.elapsed();
    let dir = data_dir(args);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        return 1;
    }
    let t2 = Instant::now();
    let p = dir.join("index.usdb");
    if let Err(e) = store::save(&ix, &p) {
        eprintln!("save failed: {e}");
        return 1;
    }
    let t_save = t2.elapsed();
    report_index(&ix, &p, t0, t_scan, t_build, t_save);
    0
}

/// Index every (selected) local volume in parallel: MFT for NTFS when
/// possible, directory walk otherwise.
pub fn scan_volumes(
    want: Option<Vec<char>>,
    force_walk: bool,
    mo: &mft::MftOptions,
    walk_threads: usize,
    verbose: bool,
) -> Vec<VolumeScan> {
    let vols: Vec<_> = volumes::list()
        .into_iter()
        .filter(|v| want.as_ref().is_none_or(|w| w.contains(&v.letter)))
        .collect();
    // All volumes in parallel (different disks → independent I/O queues).
    std::thread::scope(|s| {
        let hs: Vec<_> = vols
            .iter()
            .map(|v| {
                let mo = &mo;
                s.spawn(move || {
                    let t = Instant::now();
                    if v.is_ntfs() && !force_walk {
                        match mft::scan_mft(v.letter, mo) {
                            Ok((scan, st)) => {
                                if verbose { eprintln!(
                                    "{}: MFT {} entries in {:.0} ms  (records {}, MFT {:.1} MiB, read {:.1} MiB in {} blocks, skipped {}, {} threads, read+parse {:.0} ms [Σread {:.0} ms, Σparse {:.0} ms], assemble {:.0} ms)",
                                    v.root(), scan.entries.len(), t.elapsed().as_secs_f64() * 1e3,
                                    st.records, st.mft_bytes as f64 / 1048576.0, st.bytes_read as f64 / 1048576.0,
                                    st.blocks, st.skipped_blocks, st.threads, st.read_parse_ms, st.read_ms_sum, st.parse_ms_sum, st.assemble_ms
                                ); }
                                return scan;
                            }
                            Err(e) => {
                                if verbose {
                                    eprintln!("{}: MFT read failed ({e}); falling back to directory walk", v.root())
                                }
                            }
                        }
                    }
                    let mut scan = walk::walk_volume(v.letter, walk_threads);
                    scan.meta.serial = v.serial;
                    if verbose {
                        eprintln!("{}: walk {} entries in {:.0} ms ({})", v.root(), scan.entries.len(), t.elapsed().as_secs_f64() * 1e3, v.fs);
                    }
                    scan
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Index all local volumes with default settings (used by the GUI on first start).
pub fn build_index_all(force_walk: bool) -> Index {
    us_core::build(scan_volumes(None, force_walk, &mft::MftOptions::default(), 16, false))
}

fn report_index(ix: &Index, p: &std::path::Path, t0: Instant, t_scan: std::time::Duration, t_build: std::time::Duration, t_save: std::time::Duration) {
    eprintln!(
        "TOTAL {} entries | scan {:.0} ms | build {:.0} ms | save {:.0} ms | all {:.0} ms | RAM {:.1} MiB ({:.1} B/entry) | file {:.1} MiB",
        ix.len(),
        t_scan.as_secs_f64() * 1e3,
        t_build.as_secs_f64() * 1e3,
        t_save.as_secs_f64() * 1e3,
        t0.elapsed().as_secs_f64() * 1e3,
        ix.heap_bytes() as f64 / 1048576.0,
        ix.heap_bytes() as f64 / ix.len().max(1) as f64,
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as f64 / 1048576.0
    );
}

fn cmd_search(args: &[String]) -> i32 {
    use crate::output::{self as out, Field, TimeFormat};
    use std::io::Write;
    use us_core::sort::SortKey;
    let mut limit: Option<usize> = None;
    let (mut offset, mut desc, mut count_only) = (0usize, false, false);
    let (mut format, mut fields, mut sort, mut tf) = ("text".to_string(), None::<Vec<Field>>, SortKey::Name, TimeFormat::Iso);
    let mut words = Vec::new();
    let mut i = 0;
    let bad = |m: String| {
        eprintln!("error: {m}");
        2
    };
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "-n" | "--limit" => {
                limit = Some(match val.parse() {
                    Ok(v) => v,
                    Err(_) => return bad(format!("bad limit '{val}'")),
                });
                i += 1;
            }
            "--offset" => {
                offset = val.parse().unwrap_or(0);
                i += 1;
            }
            "--format" => {
                format = val.to_ascii_lowercase();
                i += 1;
            }
            "--fields" => {
                fields = Some(match out::parse_fields(&val) {
                    Ok(f) if !f.is_empty() => f,
                    Ok(_) => return bad("empty --fields".into()),
                    Err(e) => return bad(e),
                });
                i += 1;
            }
            "--sort" => {
                sort = match SortKey::parse(&val) {
                    Some(k) => k,
                    None => return bad(format!("bad sort key '{val}'")),
                };
                i += 1;
            }
            "--time" => {
                tf = match TimeFormat::parse(&val) {
                    Some(t) => t,
                    None => return bad(format!("bad time format '{val}'")),
                };
                i += 1;
            }
            "--desc" => desc = true,
            "--count" => count_only = true,
            "--data" => i += 1,
            a => words.push(a.to_string()),
        }
        i += 1;
    }
    if !["text", "json", "jsonl", "tsv", "csv", "nul"].contains(&format.as_str()) {
        return bad(format!("bad format '{format}'"));
    }
    let Some(ix) = load_index_quiet(args, format != "text" || count_only) else { return 1 };
    let (ids, ms) = out::query_sorted(&ix, &words.join(" "), sort, desc);
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::with_capacity(1 << 16, stdout.lock());
    if count_only {
        let _ = writeln!(w, "{}", ids.len());
        return 0;
    }
    // Text keeps the old default of 30 rows; machine formats return all rows.
    let limit = match limit {
        Some(0) => usize::MAX,
        Some(n) => n,
        None if format == "text" => 30,
        None => usize::MAX,
    };
    let page = ids.iter().skip(offset).take(limit);
    let fields = fields.unwrap_or_else(|| if format == "text" { vec![Field::Full] } else { out::DEFAULT_FIELDS.to_vec() });
    let (mut line, mut scratch) = (String::with_capacity(512), String::with_capacity(260));
    let res = (|| -> std::io::Result<()> {
        match format.as_str() {
            "json" => {
                write!(w, "{{\"total\":{},\"offset\":{offset},\"ms\":{ms:.3},\"items\":[", ids.len())?;
                for (k, &id) in page.enumerate() {
                    line.clear();
                    if k > 0 {
                        line.push(',');
                    }
                    out::json_item(&mut line, &ix, id, &fields, tf, &mut scratch);
                    w.write_all(line.as_bytes())?;
                }
                writeln!(w, "]}}")?;
            }
            "jsonl" => {
                for &id in page {
                    line.clear();
                    out::json_item(&mut line, &ix, id, &fields, tf, &mut scratch);
                    line.push('\n');
                    w.write_all(line.as_bytes())?;
                }
            }
            "tsv" | "csv" | "text" => {
                let csv = format == "csv";
                let sep = if csv { ',' } else { '\t' };
                if format != "text" {
                    let head: Vec<&str> = fields.iter().map(|f| f.key()).collect();
                    writeln!(w, "{}", head.join(&sep.to_string()))?;
                }
                for &id in page {
                    line.clear();
                    for (k, &f) in fields.iter().enumerate() {
                        if k > 0 {
                            line.push(sep);
                        }
                        let v = out::cell(&ix, id, f, tf, &mut scratch);
                        if csv { out::csv_quote(&mut line, &v) } else { out::tsv_clean(&mut line, &v) }
                    }
                    line.push_str(if csv { "\r\n" } else { "\n" });
                    w.write_all(line.as_bytes())?;
                }
            }
            _ => {
                // nul: full paths, each terminated by \0 (xargs -0 style)
                for &id in page {
                    ix.full_path_into(id, &mut scratch);
                    w.write_all(scratch.as_bytes())?;
                    w.write_all(b"\0")?;
                }
            }
        }
        w.flush()
    })();
    if format == "text" {
        eprintln!("{} results in {ms:.3} ms", ids.len());
    }
    // A closed pipe (`| head`) is not an error for a search tool.
    match res {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
            eprintln!("write error: {e}");
            1
        }
        _ => 0,
    }
}

/// Like `load_index`, but silent on success (machine formats keep stderr clean).
fn load_index_quiet(args: &[String], quiet: bool) -> Option<Index> {
    if !quiet {
        return load_index(args);
    }
    let p = data_dir(args).join("index.usdb");
    match store::load(&p) {
        Ok(ix) => Some(ix),
        Err(e) => {
            eprintln!("cannot load {}: {e} (run `usc index` first)", p.display());
            None
        }
    }
}

fn cmd_info(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--json") {
        let Some(ix) = load_index_quiet(args, true) else { return 1 };
        let mut s = String::new();
        crate::serve::info_json(&mut s, Some(&ix), None);
        println!("{s}");
        return 0;
    }
    let Some(ix) = load_index(args) else { return 1 };
    let dirs = (0..ix.len() as u32).filter(|&i| ix.is_dir(i)).count();
    println!("entries {} (dirs {dirs}, files {})", ix.len(), ix.len() - dirs);
    println!("RAM {:.1} MiB, names {:.1} MiB, folded blob {:.1} MiB", ix.heap_bytes() as f64 / 1048576.0, ix.names.len() as f64 / 1048576.0, ix.folded.len() as f64 / 1048576.0);
    for v in &ix.volumes {
        println!("{}  kind {:?}  serial {:08X}  journal {:016X}  next_usn {}", v.root, v.meta.kind, v.meta.serial, v.meta.journal_id, v.meta.next_usn);
    }
    0
}

/// `usc menu [--invoke VERB] <path...>`: build the Explorer context menu for
/// the given files and print its items (no UI), or run one verb (e.g. `copy`).
/// Diagnostics for the GUI's right-click menu.
fn cmd_menu(args: &[String]) -> i32 {
    use windows_sys::Win32::System::Ole::{OleFlushClipboard, OleInitialize, OleUninitialize};
    let (verb, paths) = match args {
        [flag, v, rest @ ..] if flag == "--invoke" => (Some(v.as_str()), rest),
        _ => (None, args),
    };
    unsafe { OleInitialize(std::ptr::null()) };
    let code = match crate::shellmenu::Items::new(std::ptr::null_mut(), paths) {
        Ok(it) => match verb {
            Some(v) => {
                let ok = it.invoke_verb(std::ptr::null_mut(), v, false);
                println!("{v}: {}", if ok { "ok" } else { "failed" });
                unsafe { OleFlushClipboard() };
                if ok { 0 } else { 1 }
            }
            None => {
                for l in it.labels() {
                    println!("{l}");
                }
                0
            }
        },
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    };
    unsafe { OleUninitialize() };
    code
}
