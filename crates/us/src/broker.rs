//! Privilege separation: the elevated part only reads $MFT/USN, the UI runs without rights.
//!
//! GUI (medium IL) ──creates──▶ pipe `\\.\pipe\unthinkable-search-<pid>-<random>`
//!   DACL: only the current user's SID · FILE_FLAG_FIRST_PIPE_INSTANCE (no
//!   squatting) · PIPE_REJECT_REMOTE_CLIENTS · 1 instance.
//! GUI ──ShellExecuteEx("runas")──▶ `us.exe --broker <pipe> <gui pid>` (UAC prompt)
//!   GUI checks: connected client PID == PID of the process it launched.
//!   Broker checks: pipe server PID == <gui pid>; opens the pipe with
//!   SECURITY_IDENTIFICATION (the GUI can never impersonate the admin token).
//! Broker → GUI: only metadata (volume scans, changed records). Never a
//!   handle, never file contents. GUI → broker: one START message.
//! The broker exits as soon as the GUI process exits.
//!
//! Every message is length-prefixed and decoded with full bounds/UTF-8
//! validation (a compromised peer must not be able to corrupt the index).

use crate::engine::{letter, Updater};
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use us_core::live::RecState;
use us_core::scan::{ScanEntry, ScanKind, VolumeMeta, VolumeScan, NONE};
use us_core::Index;
use us_win::util::{last_err, wide, Handle};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;

const PROTO: u32 = 1;
const T_HELLO: u8 = 1;
const T_SCAN: u8 = 2;
const T_CHANGES: u8 = 3;
const T_LOG: u8 = 4;
const T_READY: u8 = 5;
const T_START: u8 = 10;
const MAX_FRAME: usize = 1 << 30;

// ---------------------------------------------------------------------------
// Encoding.

#[derive(Default)]
struct Enc(Vec<u8>);
impl Enc {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct Dec<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u16(&mut self) -> Option<u16> { Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?)) }
    fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn str(&mut self) -> Option<String> { String::from_utf8(self.bytes()?.to_vec()).ok() }
}

fn enc_scan(s: &VolumeScan) -> Vec<u8> {
    let mut e = Enc(Vec::with_capacity(s.entries.len() * 44 + s.arena.len() + 64));
    e.bytes(s.root.as_bytes());
    e.u8(s.meta.kind as u8);
    e.u32(s.meta.serial);
    e.u64(s.meta.journal_id);
    e.u64(s.meta.next_usn as u64);
    e.u32(s.entries.len() as u32);
    for x in &s.entries {
        e.u32(x.parent);
        e.u32(x.name_off);
        e.u32(x.name_len);
        e.u32(x.attrs);
        e.u64(x.size);
        e.u64(x.mtime);
        e.u64(x.ctime);
        e.u32(x.rec);
    }
    e.bytes(&s.arena);
    e.0
}

fn dec_scan(b: &[u8]) -> Option<VolumeScan> {
    let mut d = Dec { b, p: 0 };
    let root = String::from_utf8(d.bytes()?.to_vec()).ok()?;
    if root.len() > 64 {
        return None;
    }
    let kind = if d.u8()? == 1 { ScanKind::Mft } else { ScanKind::Walk };
    let meta = VolumeMeta { kind, serial: d.u32()?, journal_id: d.u64()?, next_usn: d.u64()? as i64 };
    let n = d.u32()? as usize;
    if n == 0 || n > b.len() / 44 {
        return None;
    }
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        entries.push(ScanEntry {
            parent: d.u32()?,
            name_off: d.u32()?,
            name_len: d.u32()?,
            attrs: d.u32()?,
            size: d.u64()?,
            mtime: d.u64()?,
            ctime: d.u64()?,
            rec: d.u32()?,
        });
    }
    let arena = d.bytes()?.to_vec();
    // Validate all invariants VolumeScan::name() and build() rely on.
    for (i, x) in entries.iter().enumerate() {
        let end = (x.name_off as usize).checked_add(x.name_len as usize)?;
        std::str::from_utf8(arena.get(x.name_off as usize..end)?).ok()?;
        if i == 0 { if x.parent != NONE { return None } } else if x.parent as usize >= n { return None }
    }
    Some(VolumeScan { root, meta, entries, arena })
}

fn enc_changes(vol: char, id: u64, usn: i64, items: &[(u32, Option<RecState>)]) -> Vec<u8> {
    let mut e = Enc::default();
    e.u8(vol as u8);
    e.u64(id);
    e.u64(usn as u64);
    e.u32(items.len() as u32);
    for (r, st) in items {
        e.u32(*r);
        match st {
            None => e.u8(0),
            Some(s) => {
                e.u8(1);
                e.u8(s.is_dir as u8);
                e.u32(s.attrs);
                e.u64(s.mtime);
                e.u64(s.ctime);
                e.u64(s.size);
                e.u16(s.names.len().min(u16::MAX as usize) as u16);
                for (p, n) in s.names.iter().take(u16::MAX as usize) {
                    e.u32(*p);
                    e.bytes(n.as_bytes());
                }
            }
        }
    }
    e.0
}

type ChangeMsg = (char, u64, i64, Vec<(u32, Option<RecState>)>);

fn dec_changes(b: &[u8]) -> Option<ChangeMsg> {
    let mut d = Dec { b, p: 0 };
    let vol = (d.u8()? as char).to_ascii_uppercase();
    if !vol.is_ascii_uppercase() {
        return None;
    }
    let id = d.u64()?;
    let usn = d.u64()? as i64;
    let n = d.u32()? as usize;
    if n > b.len() {
        return None;
    }
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let r = d.u32()?;
        let st = match d.u8()? {
            0 => None,
            _ => {
                let is_dir = d.u8()? != 0;
                let attrs = d.u32()?;
                let mtime = d.u64()?;
                let ctime = d.u64()?;
                let size = d.u64()?;
                let k = d.u16()? as usize;
                let mut names = Vec::with_capacity(k);
                for _ in 0..k {
                    let p = d.u32()?;
                    names.push((p, d.str()?));
                }
                Some(RecState { is_dir, attrs, mtime, ctime, size, names })
            }
        };
        items.push((r, st));
    }
    Some((vol, id, usn, items))
}

// ---------------------------------------------------------------------------
// Pipe I/O (works for both overlapped and synchronous handles).

pub(crate) fn io_all(h: &Handle, buf: &mut [u8], write: bool) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let chunk = (buf.len() - done).min(1 << 20) as u32;
        let ev = Handle::checked(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev.0;
        let mut n = 0u32;
        let ok = unsafe {
            if write {
                WriteFile(h.0, buf[done..].as_ptr(), chunk, &mut n, &mut ov)
            } else {
                ReadFile(h.0, buf[done..].as_mut_ptr(), chunk, &mut n, &mut ov)
            }
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING || unsafe { GetOverlappedResult(h.0, &ov, &mut n, 1) } == 0 {
                return Err(last_err());
            }
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed"));
        }
        done += n as usize;
    }
    Ok(())
}

fn send(h: &Handle, ty: u8, payload: &[u8]) -> io::Result<()> {
    let mut f = Vec::with_capacity(payload.len() + 5);
    f.extend_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
    f.push(ty);
    f.extend_from_slice(payload);
    io_all(h, &mut f, true)
}

fn recv(h: &Handle) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 4];
    io_all(h, &mut hdr, false)?;
    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame length"));
    }
    let mut b = vec![0u8; len];
    io_all(h, &mut b, false)?;
    let ty = b[0];
    b.remove(0);
    Ok((ty, b))
}

// ---------------------------------------------------------------------------
// GUI side.

fn user_sid_string() -> io::Result<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return Err(last_err());
        }
        let tok = Handle(tok);
        let mut buf = vec![0u64; 64];
        let mut len = 0u32;
        if GetTokenInformation(tok.0, TokenUser, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut len) == 0 {
            return Err(last_err());
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(tu.User.Sid, &mut s) == 0 {
            return Err(last_err());
        }
        let mut n = 0;
        while *s.add(n) != 0 {
            n += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s as *mut _);
        Ok(out)
    }
}

fn create_server(name: &str) -> io::Result<Handle> {
    create_pipe(name, true, 1)
}

/// Overlapped duplex byte pipe, local clients only, DACL = current user only.
/// `first`: FILE_FLAG_FIRST_PIPE_INSTANCE (fails if anyone already owns the name).
pub(crate) fn create_pipe(name: &str, first: bool, max_instances: u32) -> io::Result<Handle> {
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    // Protected DACL: generic-all for the current user only (never a NULL DACL).
    let sddl = wide(&format!("D:P(A;;GA;;;{})", user_sid_string()?));
    let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut psd, std::ptr::null_mut()) } == 0 {
        return Err(last_err());
    }
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd, bInheritHandle: 0 };
    let n = wide(name);
    let h = unsafe {
        CreateNamedPipeW(
            n.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            max_instances,
            1 << 20,
            1 << 20,
            0,
            &sa,
        )
    };
    unsafe { LocalFree(psd) };
    Handle::checked(h)
}

pub enum Msg {
    Scan(VolumeScan),
    Changes(ChangeMsg),
    Log(String),
    Ready,
}

pub struct Client {
    pub rx: mpsc::Receiver<Msg>,
    _broker: Handle,
}

impl Client {
    /// Launch the elevated broker (UAC prompt) and hand it our current volume
    /// cursors. Err = user declined UAC, or the handshake failed.
    pub fn start(ix: &Index) -> io::Result<Client> {
        // Every binary of ours accepts `--broker`, so a renamed exe still works.
        Self::start_with(ix, &std::env::current_exe()?)
    }

    /// Same as [`Client::start`] with an explicit `us.exe` (the DLL lives in a
    /// foreign host process, so "next to the current exe" is meaningless there).
    pub fn start_with(ix: &Index, exe: &std::path::Path) -> io::Result<Client> {
        use std::hash::BuildHasher;
        use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
        if !exe.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} не найден", exe.display())));
        }
        let rnd = std::collections::hash_map::RandomState::new().hash_one((std::process::id(), Instant::now()));
        let name = format!(r"\\.\pipe\unthinkable-search-{}-{rnd:016x}", std::process::id());
        let pipe = create_server(&name)?;

        let (verb, file) = (wide("runas"), wide(&exe.to_string_lossy()));
        let params = wide(&format!("--broker {name} {}", std::process::id()));
        let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ptr();
        sei.nShow = windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
        if unsafe { ShellExecuteExW(&mut sei) } == 0 || sei.hProcess.is_null() {
            return Err(last_err()); // ERROR_CANCELLED when UAC is declined
        }
        let broker = Handle(sei.hProcess);
        let broker_pid = unsafe { GetProcessId(broker.0) };

        // Overlapped connect; give up if the broker dies or never connects.
        let ev = Handle::checked(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev.0;
        if unsafe { ConnectNamedPipe(pipe.0, &mut ov) } == 0 {
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => {}
                ERROR_IO_PENDING => {
                    let hs = [ev.0, broker.0];
                    let w = unsafe { WaitForMultipleObjects(2, hs.as_ptr(), 0, 120_000) };
                    if w != WAIT_OBJECT_0 {
                        return Err(io::Error::other("broker did not connect"));
                    }
                }
                _ => return Err(last_err()),
            }
        }
        let mut cpid = 0u32;
        if unsafe { GetNamedPipeClientProcessId(pipe.0, &mut cpid) } == 0 || cpid != broker_pid {
            return Err(io::Error::other(format!("unexpected pipe client pid {cpid} (expected {broker_pid})")));
        }
        let (ty, hello) = recv(&pipe)?;
        let mut d = Dec { b: &hello, p: 0 };
        if ty != T_HELLO || d.u32() != Some(PROTO) {
            return Err(io::Error::other("broker protocol mismatch"));
        }
        let mut e = Enc::default();
        e.u32(ix.volumes.len() as u32);
        for v in &ix.volumes {
            e.u8(letter(&v.root) as u8);
            e.u8(v.meta.kind as u8);
            e.u64(v.meta.journal_id);
            e.u64(v.meta.next_usn as u64);
        }
        send(&pipe, T_START, &e.0)?;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let Ok((ty, b)) = recv(&pipe) else { break };
                let m = match ty {
                    T_SCAN => dec_scan(&b).map(Msg::Scan),
                    T_CHANGES => dec_changes(&b).map(Msg::Changes),
                    T_LOG => String::from_utf8(b).ok().map(Msg::Log),
                    T_READY => Some(Msg::Ready),
                    _ => None,
                };
                // Malformed message from the broker: stop trusting the channel.
                let Some(m) = m else { break };
                if tx.send(m).is_err() {
                    break;
                }
            }
        });
        Ok(Client { rx, _broker: broker })
    }
}

/// Feed broker messages into an updater. Returns false when the broker is gone.
pub fn pump(rx: &mpsc::Receiver<Msg>, up: &mut Updater, wait: Duration, log: &mut Vec<String>, ready: &mut bool) -> (bool, bool) {
    let mut changed_ix = false;
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Msg::Scan(s)) => {
                up.upsert(vec![s]);
                changed_ix = true;
            }
            Ok(Msg::Changes((vol, id, usn, items))) => {
                for (r, st) in items {
                    up.add(vol, r, st);
                }
                up.set_cursor(vol, id, usn);
            }
            Ok(Msg::Log(s)) => log.push(s),
            Ok(Msg::Ready) => *ready = true,
            Err(mpsc::RecvTimeoutError::Timeout) => return (true, changed_ix),
            Err(mpsc::RecvTimeoutError::Disconnected) => return (false, changed_ix),
        }
        if Instant::now() >= deadline {
            return (true, changed_ix);
        }
    }
}

// ---------------------------------------------------------------------------
// Broker side (`us.exe --broker <pipe> <gui pid>`, elevated).

pub fn broker_main(args: &[String]) -> i32 {
    match run_broker(args) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn run_broker(args: &[String]) -> io::Result<()> {
    use us_win::mft;
    use us_win::usn::{Journal, JournalError};
    us_win::util::harden_process();
    let name = args.first().ok_or_else(|| io::Error::other("pipe name"))?;
    let gui_pid: u32 = args.get(1).and_then(|s| s.parse().ok()).ok_or_else(|| io::Error::other("gui pid"))?;
    if !name.starts_with(r"\\.\pipe\unthinkable-search-") {
        return Err(io::Error::other("bad pipe name"));
    }
    const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
    const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
    let n = wide(name);
    let pipe = Handle::checked(unsafe {
        CreateFileW(n.as_ptr(), 0x8000_0000 | 0x4000_0000, 0, std::ptr::null(), OPEN_EXISTING, SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, std::ptr::null_mut())
    })?;
    let mut spid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut spid) } == 0 || spid != gui_pid {
        return Err(io::Error::other("pipe server is not our GUI"));
    }
    let gui = Handle::checked(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, gui_pid) })?;
    let mut e = Enc::default();
    e.u32(PROTO);
    send(&pipe, T_HELLO, &e.0)?;
    let (ty, b) = recv(&pipe)?;
    if ty != T_START {
        return Err(io::Error::other("expected START"));
    }
    let mut d = Dec { b: &b, p: 0 };
    let nv = d.u32().ok_or_else(|| io::Error::other("START"))? as usize;
    let mut known = Vec::new();
    for _ in 0..nv.min(26) {
        let (Some(l), Some(k), Some(id), Some(usn)) = (d.u8(), d.u8(), d.u64(), d.u64()) else { break };
        known.push(((l as char).to_ascii_uppercase(), k, id, usn as i64));
    }
    let log = |s: String| send(&pipe, T_LOG, s.as_bytes());

    let mut journals: Vec<Journal> = Vec::new();
    let scan_and_send = |l: char, journals: &mut Vec<Journal>| -> io::Result<()> {
        match mft::scan_mft(l, &mft::MftOptions::default()) {
            Ok((scan, _)) => {
                send(&pipe, T_SCAN, &enc_scan(&scan))?;
                if let Ok(j) = Journal::open(l, scan.meta.journal_id, scan.meta.next_usn) {
                    journals.push(j);
                }
            }
            Err(e) => log(format!("{l}: MFT scan failed: {e}"))?,
        }
        Ok(())
    };
    for v in us_win::volumes::list().into_iter().filter(|v| v.is_ntfs()) {
        let resumed = known
            .iter()
            .find(|k| k.0 == v.letter && k.1 == ScanKind::Mft as u8)
            .and_then(|k| Journal::open(v.letter, k.2, k.3).ok());
        match resumed {
            Some(j) => journals.push(j),
            None => scan_and_send(v.letter, &mut journals)?,
        }
    }
    send(&pipe, T_READY, &[])?;

    // Steady state: low background priority (CPU + I/O).
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    let mut touched = Vec::new();
    loop {
        if unsafe { WaitForSingleObject(gui.0, 100) } == WAIT_OBJECT_0 {
            return Ok(()); // GUI exited
        }
        let mut rescan = Vec::new();
        for j in journals.iter_mut() {
            touched.clear();
            match j.poll(&mut touched) {
                Ok(_) => {}
                Err(JournalError::Reset(_)) => {
                    rescan.push(j.letter);
                    continue;
                }
                Err(JournalError::Io(_)) => continue,
            }
            if touched.is_empty() {
                continue;
            }
            touched.sort_unstable();
            touched.dedup();
            let items: Vec<(u32, Option<RecState>)> = touched.iter().filter_map(|&r| j.fetch(r).ok().map(|st| (r, st))).collect();
            send(&pipe, T_CHANGES, &enc_changes(j.letter, j.journal_id, j.next_usn, &items))?;
        }
        for l in rescan {
            journals.retain(|j| j.letter != l);
            scan_and_send(l, &mut journals)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_roundtrip_and_rejects_garbage() {
        let mut s = VolumeScan::new("C:");
        s.meta = VolumeMeta { kind: ScanKind::Mft, serial: 7, journal_id: 9, next_usn: 11 };
        let d = s.push_rec(0, "dir", 0x10, 0, 1, 0, 30);
        s.push_rec(d, "файл.txt", 0, 5, 2, 0, 31);
        let b = enc_scan(&s);
        let r = dec_scan(&b).unwrap();
        assert_eq!(r.meta, s.meta);
        assert_eq!(r.name(2), "файл.txt");
        assert_eq!(r.entries[2].parent, d);
        // Truncations and bit flips must never panic and never yield a scan
        // whose names/parents are out of bounds.
        for cut in 0..b.len() {
            let _ = dec_scan(&b[..cut]);
        }
        for i in 0..b.len() {
            let mut x = b.clone();
            x[i] ^= 0xFF;
            if let Some(v) = dec_scan(&x) {
                for k in 0..v.entries.len() {
                    let _ = v.name(k);
                }
            }
        }
    }

    #[test]
    fn changes_roundtrip() {
        let items = vec![(5u32, None), (6, Some(RecState { is_dir: false, attrs: 1, mtime: 2, ctime: 4, size: 3, names: vec![(5, "ä.txt".into())] }))];
        let b = enc_changes('D', 1, 2, &items);
        let (v, id, usn, it) = dec_changes(&b).unwrap();
        assert_eq!((v, id, usn), ('D', 1, 2));
        assert_eq!(it, items);
        for cut in 0..b.len() {
            let _ = dec_changes(&b[..cut]);
        }
    }
}
