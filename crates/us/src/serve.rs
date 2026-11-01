//! `usc serve` — long-running JSON-lines server for integrations
//! (protocol: docs/INTEGRATION.md). The index stays in RAM with live updates;
//! a search costs ~0.1–3 ms instead of re-loading the index per call.
//!
//! Transports:
//! * stdin/stdout (default) — the host program spawns `usc serve` and talks
//!   through the child's pipes. Exits on EOF or `{"cmd":"quit"}`.
//! * named pipe (`--pipe [NAME]`) — many local clients share one server and
//!   one UAC prompt. DACL: current user only, local clients only, first
//!   instance must be ours (no squatting). Stops on `{"cmd":"shutdown"}`.
//!
//! Every request is one JSON object per line; every response is one line.
//! Requests on one connection are answered in order; `id` is echoed back.

use crate::broker::{create_pipe, io_all};
use crate::output::{self as out, Field, TimeFormat};
use crate::service::{self, Event, LiveMode, Service};
use serde_json::Value;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use us_core::scan::ScanKind;
use us_core::sort::SortKey;
use us_core::Index;
use us_win::util::{last_err, Handle};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, PIPE_UNLIMITED_INSTANCES};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

pub const PROTOCOL: u32 = 1;
pub const DEFAULT_PIPE: &str = r"\\.\pipe\unthinkable-search";
const DEFAULT_LIMIT: usize = 100;
const MAX_LINE: usize = 1 << 20;

/// Writes one line to a client; false = the client is gone.
type Sink = Arc<dyn Fn(&str) -> bool + Send + Sync>;

enum Flow {
    Continue,
    Close,
    Shutdown,
}

struct Server {
    // RwLock: searches may wait for the first index while the service thread
    // broadcasts events (both are readers). Written only once, at shutdown.
    svc: RwLock<Option<Service>>,
    subs: Mutex<Vec<(u64, Sink)>>,
    next_conn: AtomicU64,
    shutdown: AtomicBool,
}

impl Server {
    fn with_svc<R>(&self, f: impl FnOnce(&Service) -> R) -> Option<R> {
        self.svc.read().unwrap().as_ref().map(f)
    }

    fn broadcast(&self, ev: Event) {
        let mut line = String::new();
        match ev {
            Event::Index => {
                let Some((g, n)) = self.with_svc(|s| (s.generation(), s.index().map_or(0, |i| i.len()))) else { return };
                line.push_str(&format!("{{\"event\":\"index\",\"generation\":{g},\"entries\":{n}}}"));
            }
            Event::Status => {
                let Some((st, live, msg)) = self.with_svc(|s| s.status()) else { return };
                line.push_str(&format!("{{\"event\":\"status\",\"state\":\"{}\",\"live\":{live},\"status\":", st.as_str()));
                out::json_str(&mut line, &msg);
                line.push('}');
            }
        }
        let subs: Vec<(u64, Sink)> = self.subs.lock().unwrap().clone();
        let dead: Vec<u64> = subs.into_iter().filter(|(_, s)| !s(&line)).map(|(c, _)| c).collect();
        if !dead.is_empty() {
            self.subs.lock().unwrap().retain(|(c, _)| !dead.contains(c));
        }
    }

    fn handle(&self, line: &str, conn: u64, sink: &Sink) -> Flow {
        let v: Value = match serde_json::from_str(line) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => {
                sink(&error(None, "bad_request", "request must be a JSON object"));
                return Flow::Continue;
            }
            Err(e) => {
                sink(&error(None, "bad_json", &e.to_string()));
                return Flow::Continue;
            }
        };
        let id = v.get("id");
        let reply = |body: &str| sink(&ok(id, body));
        match v.get("cmd").and_then(Value::as_str).unwrap_or("") {
            "ping" | "info" | "search" => {
                let r = self.with_svc(|svc| stateless(svc, &v)).unwrap_or_else(|| error(id, "not_ready", "shutting down"));
                sink(&r);
            }
            "subscribe" => {
                let mut subs = self.subs.lock().unwrap();
                if !subs.iter().any(|(c, _)| *c == conn) {
                    subs.push((conn, sink.clone()));
                }
                drop(subs);
                reply("");
            }
            "unsubscribe" => {
                self.subs.lock().unwrap().retain(|(c, _)| *c != conn);
                reply("");
            }
            "quit" => {
                reply("");
                return Flow::Close;
            }
            "shutdown" => {
                reply("");
                return Flow::Shutdown;
            }
            "" => {
                sink(&error(id, "bad_request", "missing \"cmd\""));
            }
            other => {
                sink(&error(id, "unknown_cmd", &format!("unknown cmd '{other}'")));
            }
        }
        Flow::Continue
    }
}

/// Commands that need no connection state (also used by the DLL's `us_request`).
pub fn stateless(svc: &Service, v: &Value) -> String {
    let id = v.get("id");
    match v.get("cmd").and_then(Value::as_str).unwrap_or("") {
        "ping" => ok(id, ""),
        "info" => {
            let mut s = String::new();
            info_json(&mut s, svc.index().as_deref(), Some(svc));
            ok(id, &s[1..s.len() - 1]) // splice the object's fields into the reply
        }
        "search" => search_response(svc, v, id),
        "" => error(id, "bad_request", "missing \"cmd\""),
        other => error(id, "unknown_cmd", &format!("unknown cmd '{other}'")),
    }
}

/// Parse one request line and answer it (DLL entry point).
pub fn request(svc: &Service, line: &str) -> String {
    match serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}')) {
        Ok(v @ Value::Object(_)) => stateless(svc, &v),
        Ok(_) => error(None, "bad_request", "request must be a JSON object"),
        Err(e) => error(None, "bad_json", &e.to_string()),
    }
}

fn search_response(svc: &Service, v: &Value, id: Option<&Value>) -> String {
    let Some(q) = v.get("query").and_then(Value::as_str) else {
        return error(id, "bad_request", "\"query\" (string) is required");
    };
    let num = |k: &str| v.get(k).and_then(Value::as_u64);
    let offset = num("offset").unwrap_or(0) as usize;
    let limit = match v.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(x) => match x.as_u64() {
            Some(0) => usize::MAX,
            Some(n) => n as usize,
            None => return error(id, "bad_request", "\"limit\" must be a non-negative integer"),
        },
    };
    let sort = match v.get("sort").and_then(Value::as_str) {
        None => SortKey::Name,
        Some(s) => match SortKey::parse(s) {
            Some(k) => k,
            None => return error(id, "bad_request", &format!("bad sort '{s}'")),
        },
    };
    let desc = v.get("desc").and_then(Value::as_bool).unwrap_or(false);
    let tf = match v.get("time").and_then(Value::as_str) {
        None => TimeFormat::Iso,
        Some(s) => match TimeFormat::parse(s) {
            Some(t) => t,
            None => return error(id, "bad_request", &format!("bad time format '{s}'")),
        },
    };
    let fields: Vec<Field> = match v.get("fields") {
        None | Some(Value::Null) => out::DEFAULT_FIELDS.to_vec(),
        Some(Value::String(s)) => match out::parse_fields(s) {
            Ok(f) => f,
            Err(e) => return error(id, "bad_request", &e),
        },
        Some(Value::Array(a)) => {
            let mut f = Vec::with_capacity(a.len());
            for x in a {
                match x.as_str().and_then(Field::parse) {
                    Some(k) => f.push(k),
                    None => return error(id, "bad_request", &format!("unknown field {x}")),
                }
            }
            f
        }
        Some(_) => return error(id, "bad_request", "\"fields\" must be an array or a comma-separated string"),
    };
    let wait = Duration::from_millis(num("wait_ms").unwrap_or(30_000));
    let (got, generation) = (svc.wait_index(wait), svc.generation());
    let Some(ix) = got else {
        return error(id, "not_ready", "index is not ready yet (first indexing / UAC prompt pending)");
    };
    let (ids, ms) = out::query_sorted(&ix, q, sort, desc);
    let page: Vec<u32> = ids.iter().skip(offset).take(limit).copied().collect();
    let mut body = format!("\"total\":{},\"offset\":{offset},\"count\":{},\"ms\":{ms:.3},\"generation\":{generation},\"items\":[", ids.len(), page.len());
    let mut scratch = String::new();
    for (k, &e) in page.iter().enumerate() {
        if k > 0 {
            body.push(',');
        }
        out::json_item(&mut body, &ix, e, &fields, tf, &mut scratch);
    }
    body.push(']');
    ok(id, &body)
}

fn id_prefix(id: Option<&Value>) -> String {
    match id {
        Some(v) => format!("\"id\":{},", serde_json::to_string(v).unwrap_or_else(|_| "null".into())),
        None => String::new(),
    }
}

fn ok(id: Option<&Value>, body: &str) -> String {
    format!("{{{}\"ok\":true{}{body}}}", id_prefix(id), if body.is_empty() { "" } else { "," })
}

fn error(id: Option<&Value>, code: &str, msg: &str) -> String {
    let mut s = format!("{{{}\"ok\":false,\"error\":\"{code}\",\"message\":", id_prefix(id));
    out::json_str(&mut s, msg);
    s.push('}');
    s
}

/// `{"version":..,"protocol":..,"entries":..,...}` — shared by `info` and `usc info --json`.
pub fn info_json(s: &mut String, ix: Option<&Index>, svc: Option<&Service>) {
    s.push_str(&format!("{{\"version\":\"{}\",\"protocol\":{PROTOCOL},\"entries\":{}", env!("CARGO_PKG_VERSION"), ix.map_or(0, |i| i.len())));
    if let Some(svc) = svc {
        let (st, live, msg) = svc.status();
        s.push_str(&format!(",\"generation\":{},\"state\":\"{}\",\"live\":{live},\"status\":", svc.generation(), st.as_str()));
        out::json_str(s, &msg);
    }
    s.push_str(",\"volumes\":[");
    if let Some(ix) = ix {
        for (k, v) in ix.volumes.iter().enumerate() {
            if k > 0 {
                s.push(',');
            }
            s.push_str("{\"root\":");
            out::json_str(s, &v.root);
            s.push_str(if v.meta.kind == ScanKind::Mft { ",\"kind\":\"mft\"}" } else { ",\"kind\":\"walk\"}" });
        }
    }
    s.push_str("]}");
}

fn hello() -> String {
    format!("{{\"event\":\"hello\",\"version\":\"{}\",\"protocol\":{PROTOCOL},\"pid\":{}}}", env!("CARGO_PKG_VERSION"), std::process::id())
}

/// Split a byte stream into lines and dispatch them. Returns false on shutdown.
fn pump_lines(server: &Server, conn: u64, sink: &Sink, mut read: impl FnMut(&mut Vec<u8>) -> io::Result<bool>) -> bool {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // `read` appends bytes; Ok(false) = EOF.
        let more = read(&mut buf).unwrap_or_default();
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line);
            // .NET / PowerShell writers often prepend a UTF-8 BOM to the stream.
            let text = text.trim_start_matches('\u{feff}').trim();
            if text.is_empty() {
                continue;
            }
            match server.handle(text, conn, sink) {
                Flow::Continue => {}
                Flow::Close => return true,
                Flow::Shutdown => return false,
            }
        }
        if buf.len() > MAX_LINE {
            sink(&error(None, "bad_request", "line too long"));
            return true;
        }
        if !more {
            return true;
        }
    }
}

pub fn cmd_serve(args: &[String]) -> i32 {
    let mut opts = service::Options::next_to_exe();
    opts.data_dir = crate::cli::data_dir(args);
    match crate::cli::opt(args, "--live") {
        None | Some("auto") => {}
        Some("off") => opts.live = LiveMode::Off,
        Some(x) => {
            eprintln!("error: --live auto|off (got '{x}')");
            return 2;
        }
    }
    let pipe = args.iter().position(|a| a == "--pipe").map(|i| match args.get(i + 1) {
        Some(n) if !n.starts_with("--") => {
            if n.starts_with(r"\\.\pipe\") { n.clone() } else { format!(r"\\.\pipe\{n}") }
        }
        _ => DEFAULT_PIPE.to_string(),
    });

    let server = Arc::new_cyclic(|weak: &Weak<Server>| {
        let w = weak.clone();
        let svc = Service::start(opts, move |ev| {
            if let Some(s) = w.upgrade() {
                s.broadcast(ev);
            }
        });
        Server { svc: RwLock::new(Some(svc)), subs: Mutex::new(Vec::new()), next_conn: AtomicU64::new(1), shutdown: AtomicBool::new(false) }
    });

    let code = match pipe {
        None => serve_stdio(&server),
        Some(name) => serve_pipe(&server, &name),
    };
    // Stop (and save) on this thread: never let the service thread drop the last Arc.
    server.subs.lock().unwrap().clear();
    let svc = server.svc.write().unwrap().take(); // guard dropped here, before join
    if let Some(svc) = svc {
        svc.stop();
    }
    code
}

fn serve_stdio(server: &Arc<Server>) -> i32 {
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let sink: Sink = Arc::new(move |line: &str| {
        let mut o = stdout.lock().unwrap();
        o.write_all(line.as_bytes()).and_then(|_| o.write_all(b"\n")).and_then(|_| o.flush()).is_ok()
    });
    sink(&hello());
    let conn = server.next_conn.fetch_add(1, Ordering::Relaxed);
    let stdin = io::stdin();
    let mut lock = stdin.lock();
    pump_lines(server, conn, &sink, |buf| Ok(lock.read_until(b'\n', buf)? > 0));
    0
}

fn serve_pipe(server: &Arc<Server>, name: &str) -> i32 {
    let mut next = match create_pipe(name, true, PIPE_UNLIMITED_INSTANCES) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("error: cannot create {name}: {e} (is another server running?)");
            return 1;
        }
    };
    eprintln!("listening on {name}");
    while !server.shutdown.load(Ordering::SeqCst) {
        match connect(&next, &server.shutdown) {
            Ok(true) => {}
            Ok(false) => break, // shutdown requested
            Err(e) => {
                eprintln!("connect: {e}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
        let conn_handle = Arc::new(next);
        next = match create_pipe(name, false, PIPE_UNLIMITED_INSTANCES) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("error: cannot create next pipe instance: {e}");
                return 1;
            }
        };
        let srv = server.clone();
        std::thread::spawn(move || pipe_client(srv, conn_handle));
    }
    0
}

/// Wait for a client on an overlapped pipe instance; Ok(false) = shutdown.
fn connect(h: &Handle, stop: &AtomicBool) -> io::Result<bool> {
    let ev = Handle::checked(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.0;
    if unsafe { ConnectNamedPipe(h.0, &mut ov) } != 0 {
        return Ok(true);
    }
    match unsafe { GetLastError() } {
        ERROR_PIPE_CONNECTED => return Ok(true),
        ERROR_IO_PENDING => {}
        _ => return Err(last_err()),
    }
    let mut n = 0u32;
    loop {
        if unsafe { WaitForSingleObject(ev.0, 250) } == WAIT_OBJECT_0 {
            return if unsafe { GetOverlappedResult(h.0, &ov, &mut n, 0) } != 0 { Ok(true) } else { Err(last_err()) };
        }
        if stop.load(Ordering::SeqCst) {
            unsafe {
                CancelIoEx(h.0, &ov);
                GetOverlappedResult(h.0, &ov, &mut n, 1); // `ov` must outlive the I/O
            }
            return Ok(false);
        }
    }
}

/// Read whatever is available (blocking); Ok(0) = client disconnected.
fn read_some(h: &Handle, buf: &mut [u8]) -> io::Result<usize> {
    let ev = Handle::checked(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.0;
    let mut n = 0u32;
    if unsafe { ReadFile(h.0, buf.as_mut_ptr(), buf.len() as u32, &mut n, &mut ov) } == 0 {
        match unsafe { GetLastError() } {
            ERROR_IO_PENDING => {
                if unsafe { GetOverlappedResult(h.0, &ov, &mut n, 1) } == 0 {
                    let e = unsafe { GetLastError() };
                    return if e == ERROR_BROKEN_PIPE { Ok(0) } else { Err(io::Error::from_raw_os_error(e as i32)) };
                }
            }
            ERROR_BROKEN_PIPE => return Ok(0),
            e => return Err(io::Error::from_raw_os_error(e as i32)),
        }
    }
    Ok(n as usize)
}

fn pipe_client(server: Arc<Server>, h: Arc<Handle>) {
    let conn = server.next_conn.fetch_add(1, Ordering::Relaxed);
    let hw = h.clone();
    let wlock = Arc::new(Mutex::new(()));
    let sink: Sink = Arc::new(move |line: &str| {
        let _g = wlock.lock().unwrap();
        let mut b = Vec::with_capacity(line.len() + 1);
        b.extend_from_slice(line.as_bytes());
        b.push(b'\n');
        io_all(&hw, &mut b, true).is_ok()
    });
    sink(&hello());
    let mut chunk = vec![0u8; 64 * 1024];
    let keep_running = pump_lines(&server, conn, &sink, |buf| {
        let n = read_some(&h, &mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
        Ok(n > 0)
    });
    server.subs.lock().unwrap().retain(|(c, _)| *c != conn);
    unsafe { windows_sys::Win32::System::Pipes::DisconnectNamedPipe(h.0) };
    if !keep_running {
        server.shutdown.store(true, Ordering::SeqCst);
    }
}
