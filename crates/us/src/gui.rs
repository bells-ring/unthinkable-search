//! Native Win32 GUI: search box + virtual (owner-data) ListView.
//!
//! Threads: UI (this), search worker (latest query wins, cancels the previous
//! one), index worker (load/build + live USN updates). Results arrive via
//! PostMessage; the ListView only asks for the rows it actually draws.
//!
//! Keys: type to search · ↓/↑ move · Enter open · Ctrl+Enter open folder ·
//! Esc hide · Ctrl+Alt+F global show/hide · Ctrl+Q / close button exit.
//! In text fields (search box, inline rename): Ctrl+A select all ·
//! Ctrl+Backspace delete word (a bare EDIT control has neither).
//! In the list: right click / Shift+F10 / menu key = Explorer context menu
//! (Shift = extended verbs) · Del / Shift+Del delete · Ctrl+C / Ctrl+X copy /
//! cut files · Ctrl+Shift+C copy full path · F2 rename · Alt+Enter properties ·
//! Ctrl+A select all.
//!
//! Re-entrancy rule: anything that can run a modal loop (menus, shell verbs,
//! message boxes, ShellExecute, LVM_EDITLABEL) is called OUTSIDE `with_app`.

use crate::is_elevated;
use crate::service::{self, Service};
use crate::shellmenu::{self, Pick};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Instant;
use us_core::sort::{sort_ids, SortKey};
use us_core::{query, search, Index, Query};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};

const SS_LEFTNOWORDWRAP: u32 = 0xC;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_RESULT: u32 = WM_APP + 1;
const WM_APP_INDEX: u32 = WM_APP + 2;
const WM_APP_STATUS: u32 = WM_APP + 3;
const WM_APP_RENAME_SEL: u32 = WM_APP + 4;
const WM_APP_SELINFO: u32 = WM_APP + 5;
/// Own context menu items (shell items start at `shellmenu::SHELL_FIRST`).
const CMD_OPEN_PATH: u32 = 1;
const CMD_COPY_PATH: u32 = 2;
/// Larger selections are refused for shell operations (each item = one PIDL).
const MAX_SELECTION: usize = 20_000;
const COL_NAME: usize = 0;
const COL_PATH: usize = 1;
const COL_SIZE: usize = 2;
const COL_MTIME: usize = 3;
const COL_CTIME: usize = 4;
const ID_EDIT: usize = 100;
const ID_LIST: usize = 101;
const HOTKEY_ID: i32 = 1;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Shared state between threads.

struct Shared {
    ix: RwLock<Option<Arc<Index>>>,
    req: Mutex<Option<(u64, String)>>,
    req_cv: Condvar,
    seq_no: AtomicU64,
    cancel: Mutex<Arc<AtomicBool>>,
    status: Mutex<String>,
    quit: AtomicBool,
}

struct SearchResult {
    seq_no: u64,
    q: String,
    ix: Arc<Index>,
    ids: Vec<u32>,
    ms: f64,
}

struct App {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    status: HWND,
    shared: Arc<Shared>,
    res: Option<SearchResult>,
    /// Sort: column, descending.
    sort: (usize, bool),
    text: Vec<u16>,
    /// Index load/build + live updates (background thread).
    svc: Option<Service>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// Label editing is allowed only when started by F2 / "Rename" (not by
    /// a slow double click on a selected row).
    static RENAME_ARMED: Cell<bool> = const { Cell::new(false) };
    /// Full path of the item being renamed inline.
    static RENAME_PATH: RefCell<Option<String>> = const { RefCell::new(None) };
    /// A selection-changed status refresh is already queued.
    static SELINFO_PENDING: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with the app state. Re-entrant calls (a modal loop dispatching
/// back into the window procedure) get `None` instead of a panic.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok()?.as_mut().map(f))
}

// ---------------------------------------------------------------------------
// Workers.

fn search_worker(shared: Arc<Shared>, hwnd: HWND) {
    let hwnd = hwnd as usize;
    let mut prev: Option<(Arc<Index>, String, Vec<u32>)> = None;
    loop {
        let (seq_no, q) = {
            let mut g = shared.req.lock().unwrap();
            while g.is_none() && !shared.quit.load(Ordering::Relaxed) {
                g = shared.req_cv.wait(g).unwrap();
            }
            if shared.quit.load(Ordering::Relaxed) {
                return;
            }
            g.take().unwrap()
        };
        let Some(ix) = shared.ix.read().unwrap().clone() else { continue };
        let cancel = shared.cancel.lock().unwrap().clone();
        let t = Instant::now();
        let pq = Query::parse(&q);
        let ids = match &prev {
            // Narrowing while typing: filter the previous result instead of rescanning.
            Some((pix, ps, pr)) if Arc::ptr_eq(pix, &ix) && query::is_refinement(ps, &q) => {
                search::search_within(&ix, &pq, pr, &cancel)
            }
            _ => search::search(&ix, &pq, &cancel),
        };
        if cancel.load(Ordering::Relaxed) || shared.seq_no.load(Ordering::Relaxed) != seq_no {
            continue; // superseded
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        prev = Some((ix.clone(), q.clone(), ids.clone()));
        let r = Box::new(SearchResult { seq_no, q, ix, ids, ms });
        unsafe { PostMessageW(hwnd as HWND, WM_APP_RESULT, 0, Box::into_raw(r) as isize) };
    }
}

// ---------------------------------------------------------------------------
// UI helpers.

fn submit_query(app: &App) {
    let len = unsafe { GetWindowTextLengthW(app.edit) } as usize;
    let mut buf = vec![0u16; len + 1];
    unsafe { GetWindowTextW(app.edit, buf.as_mut_ptr(), buf.len() as i32) };
    let q = String::from_utf16_lossy(&buf[..len]);
    let seq_no = app.shared.seq_no.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut c = app.shared.cancel.lock().unwrap();
        c.store(true, Ordering::Relaxed);
        *c = Arc::new(AtomicBool::new(false));
    }
    *app.shared.req.lock().unwrap() = Some((seq_no, q));
    app.shared.req_cv.notify_one();
}

fn group3(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 * 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{2009}');
        }
        out.push(ch);
    }
    out
}

/// Explorer-style size column: KiB rounded up ("1 КБ" for 1..1024 bytes).
fn fmt_size(n: u64) -> String {
    format!("{} КБ", group3(n.div_ceil(1024)))
}

/// Human-readable size with exact bytes, for the status line.
fn fmt_size_long(n: u64) -> String {
    const U: [&str; 5] = ["байт", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = n as f64;
    let mut k = 0;
    while v >= 1024.0 && k + 1 < U.len() {
        v /= 1024.0;
        k += 1;
    }
    if k == 0 { format!("{} байт", group3(n)) } else { format!("{v:.1} {} ({} байт)", U[k], group3(n)) }
}

fn fmt_time(ft: u64) -> String {
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if ft == 0 {
        return String::new();
    }
    unsafe {
        let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let mut u: SYSTEMTIME = std::mem::zeroed();
        let mut l: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut u) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &u, &mut l) == 0 {
            return String::new();
        }
        format!("{:02}.{:02}.{:04} {:02}:{:02}", l.wDay, l.wMonth, l.wYear, l.wHour, l.wMinute)
    }
}

fn cell_text(app: &App, row: usize, col: i32) -> String {
    let Some(r) = &app.res else { return String::new() };
    let Some(&id) = r.ids.get(row) else { return String::new() };
    let ix = &r.ix;
    match col as usize {
        COL_NAME => ix.display_name(id).to_string(),
        COL_PATH => {
            let mut s = String::new();
            ix.dir_path_into(id, &mut s);
            s
        }
        COL_SIZE => fmt_size(ix.total_size(id)),
        COL_MTIME => fmt_time(ix.mtime[id as usize]),
        COL_CTIME => fmt_time(ix.ctime[id as usize]),
        _ => String::new(),
    }
}

fn selected_row(app: &App) -> Option<usize> {
    let i = unsafe { SendMessageW(app.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
    if i >= 0 { Some(i as usize) } else if app.res.as_ref().is_some_and(|r| !r.ids.is_empty()) { Some(0) } else { None }
}

fn focused_row(app: &App) -> Option<usize> {
    let i = unsafe { SendMessageW(app.list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize) };
    if i >= 0 && unsafe { SendMessageW(app.list, LVM_GETITEMSTATE, i as usize, LVIS_SELECTED as isize) } != 0 {
        Some(i as usize)
    } else {
        let i = unsafe { SendMessageW(app.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
        (i >= 0).then_some(i as usize)
    }
}

fn row_path(app: &App, row: usize) -> Option<String> {
    let r = app.res.as_ref()?;
    let &id = r.ids.get(row)?;
    let mut p = String::new();
    r.ix.full_path_into(id, &mut p);
    Some(p)
}

/// Full paths of the selected rows (focused row first).
fn selection_paths(app: &App) -> Result<Vec<String>, String> {
    let n = unsafe { SendMessageW(app.list, LVM_GETSELECTEDCOUNT, 0, 0) } as usize;
    if n == 0 {
        return Err(String::new());
    }
    if n > MAX_SELECTION {
        return Err(format!("Выбрано {n} объектов — для операций оболочки не больше {MAX_SELECTION}"));
    }
    let focus = focused_row(app);
    let mut out = Vec::with_capacity(n);
    if let Some(p) = focus.and_then(|f| row_path(app, f)) {
        out.push(p);
    }
    let mut i = usize::MAX; // -1: start from the beginning
    loop {
        let next = unsafe { SendMessageW(app.list, LVM_GETNEXTITEM, i, LVNI_SELECTED as isize) };
        if next < 0 {
            break;
        }
        i = next as usize;
        if Some(i) != focus {
            out.extend(row_path(app, i));
        }
    }
    Ok(out)
}

fn open_path(p: &str, folder: bool) {
    unsafe {
        if folder {
            let args = w(&format!("/select,\"{p}\""));
            ShellExecuteW(std::ptr::null_mut(), w("open").as_ptr(), w("explorer.exe").as_ptr(), args.as_ptr(), std::ptr::null(), SW_SHOWNORMAL);
        } else {
            ShellExecuteW(std::ptr::null_mut(), std::ptr::null(), w(p).as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
        }
    }
}

/// Temporary message in the status line (replaced by the next status update).
fn flash_status(msg: &str) {
    with_app(|a| unsafe { SetWindowTextW(a.status, w(msg).as_ptr()) });
}

/// Status line text for the current selection: one item → its details.
fn selection_info(app: &App) -> Option<String> {
    let n = unsafe { SendMessageW(app.list, LVM_GETSELECTEDCOUNT, 0, 0) } as usize;
    let r = app.res.as_ref()?;
    if n == 1 {
        let &id = r.ids.get(focused_row(app)?)?;
        let ix = &r.ix;
        let kind = if ix.is_dir(id) { "папка" } else { "файл" };
        Some(format!(
            "{} · {kind} · {} · изменён {} · создан {}",
            ix.display_name(id),
            fmt_size_long(ix.total_size(id)),
            fmt_time(ix.mtime[id as usize]),
            fmt_time(ix.ctime[id as usize]),
        ))
    } else if n > 1 && n <= 100_000 {
        let mut total = 0u64;
        let mut i = usize::MAX;
        loop {
            let next = unsafe { SendMessageW(app.list, LVM_GETNEXTITEM, i, LVNI_SELECTED as isize) };
            if next < 0 {
                break;
            }
            i = next as usize;
            if let Some(&id) = r.ids.get(i) {
                total = total.saturating_add(r.ix.total_size(id));
            }
        }
        Some(format!("Выбрано: {n} · {}", fmt_size_long(total)))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Shell actions (run outside `with_app`, see the re-entrancy rule).

fn shell_verb(hwnd: HWND, verb: &str, shift: bool) {
    let Some(sel) = with_app(|a| selection_paths(a)) else { return };
    match sel.and_then(|paths| shellmenu::Items::new(hwnd, &paths)) {
        Ok(items) => {
            items.invoke_verb(hwnd, verb, shift);
        }
        Err(e) if !e.is_empty() => flash_status(&e),
        Err(_) => {}
    }
}

fn copy_paths(hwnd: HWND) {
    let Some(Ok(paths)) = with_app(|a| selection_paths(a)) else { return };
    if shellmenu::set_clipboard_text(hwnd, &paths.join("\r\n")) {
        flash_status(&format!("Скопировано путей: {}", paths.len()));
    }
}

/// Start inline renaming of `row`. The path is captured now: results may be
/// replaced by a live update while the user is typing.
fn begin_rename(row: usize) {
    // The row shows the original name, the file is `$R…`: renaming it would
    // break the link with `$I…`, and the Recycle Bin could no longer restore it.
    let recycled = with_app(|a| a.res.as_ref().and_then(|r| r.ids.get(row).map(|&id| r.ix.is_recycled(id)))).flatten();
    if recycled == Some(true) {
        flash_status("Объект в Корзине нельзя переименовать: сначала восстановите его");
        return;
    }
    let Some(Some((list, path))) = with_app(|a| row_path(a, row).map(|p| (a.list, p))) else { return };
    RENAME_PATH.with(|p| *p.borrow_mut() = Some(path));
    RENAME_ARMED.set(true);
    unsafe {
        SetFocus(list);
        SendMessageW(list, LVM_EDITLABELW, row, 0);
    }
    RENAME_ARMED.set(false);
}

/// `lp` = WM_CONTEXTMENU lParam (screen point, or -1 from the keyboard).
fn context_menu(hwnd: HWND, lp: LPARAM) {
    let keyboard = lp == -1;
    let got = with_app(|a| -> Option<(Result<Vec<String>, String>, POINT, Option<usize>)> {
        let mut pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
        unsafe {
            if keyboard {
                let row = focused_row(a)?;
                let mut rc = RECT { left: LVIR_LABEL as i32, top: 0, right: 0, bottom: 0 };
                SendMessageW(a.list, LVM_GETITEMRECT, row, &mut rc as *mut _ as isize);
                pt = POINT { x: rc.left, y: rc.bottom };
                ClientToScreen(a.list, &mut pt);
            } else {
                let mut ht: LVHITTESTINFO = std::mem::zeroed();
                ht.pt = pt;
                ScreenToClient(a.list, &mut ht.pt);
                if SendMessageW(a.list, LVM_HITTEST, 0, &mut ht as *mut _ as isize) < 0 {
                    return None; // empty area
                }
            }
        }
        let single = (unsafe { SendMessageW(a.list, LVM_GETSELECTEDCOUNT, 0, 0) } == 1).then(|| focused_row(a)).flatten();
        Some((selection_paths(a), pt, single))
    })
    .flatten();
    let Some((sel, pt, single)) = got else { return };
    let paths = match sel {
        Ok(p) => p,
        Err(e) => {
            if !e.is_empty() {
                flash_status(&e);
            }
            return;
        }
    };
    let items = match shellmenu::Items::new(hwnd, &paths) {
        Ok(i) => i,
        Err(e) => {
            flash_status(&format!("Контекстное меню недоступно: {e}"));
            return;
        }
    };
    let own = [(CMD_OPEN_PATH, "Открыть п&уть"), (CMD_COPY_PATH, "Скопировать полное &имя в буфер обмена"), (0, "")];
    let extended = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;
    match items.show(hwnd, pt, &own, extended) {
        Pick::Own(CMD_OPEN_PATH) => open_path(&paths[0], true),
        Pick::Own(CMD_COPY_PATH) => copy_paths(hwnd),
        Pick::Rename => match single {
            Some(row) => begin_rename(row),
            None => flash_status("Переименовать можно только один объект"),
        },
        _ => {}
    }
}

/// Characters Windows forbids in file names.
fn bad_name(s: &str) -> bool {
    s.is_empty() || s == "." || s == ".." || s.chars().any(|c| (c as u32) < 32 || "\\/:*?\"<>|".contains(c)) || s.ends_with(' ') || s.ends_with('.')
}

fn finish_rename(hwnd: HWND, new_name: Option<String>) {
    let Some(old) = RENAME_PATH.with(|p| p.borrow_mut().take()) else { return };
    let Some(new_name) = new_name else { return }; // cancelled
    let Some(k) = old.rfind('\\') else { return };
    if old[k + 1..] == new_name {
        return;
    }
    let err = if bad_name(&new_name) {
        Some("Имя файла не должно содержать символов \\ / : * ? \" < > | и не может заканчиваться точкой или пробелом.".to_string())
    } else {
        let new = format!("{}\\{}", &old[..k], new_name);
        use windows_sys::Win32::Storage::FileSystem::MoveFileW;
        if unsafe { MoveFileW(w(&old).as_ptr(), w(&new).as_ptr()) } == 0 {
            Some(format!("{}\n\n{}", std::io::Error::last_os_error(), old))
        } else {
            flash_status(&format!("Переименовано: {new_name}"));
            None
        }
    };
    if let Some(e) = err {
        unsafe { MessageBoxW(hwnd, w(&e).as_ptr(), w("Переименование").as_ptr(), MB_ICONERROR | MB_OK) };
    }
}

fn update_status(app: &App) {
    let base = app.shared.status.lock().unwrap().clone();
    let s = match (&app.res, selection_info(app)) {
        (Some(r), Some(sel)) => format!("{} объектов · {:.2} мс · {sel}", r.ids.len(), r.ms),
        (Some(r), None) => format!("{} объектов · {:.2} мс · {base}", r.ids.len(), r.ms),
        (None, _) => base,
    };
    unsafe { SetWindowTextW(app.status, w(&s).as_ptr()) };
}

fn apply_sort(app: &mut App) {
    let (col, desc) = app.sort;
    let Some(r) = app.res.as_mut() else { return };
    let key = match col {
        COL_PATH => SortKey::Path,
        COL_SIZE => SortKey::Size,
        COL_MTIME => SortKey::Modified,
        COL_CTIME => SortKey::Created,
        _ => SortKey::Name,
    };
    let ix = r.ix.clone();
    sort_ids(&ix, &mut r.ids, key, desc);
}

fn layout(app: &App) {
    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(app.hwnd, &mut rc);
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
        let dpi = GetDpiForWindow(app.hwnd).max(96) as i32;
        let s = |v: i32| v * dpi / 96;
        let eh = s(28);
        let sh = s(22);
        let pad = s(6);
        MoveWindow(app.edit, pad, pad, cw - 2 * pad, eh, 1);
        MoveWindow(app.list, 0, eh + 2 * pad, cw, ch - eh - 2 * pad - sh, 1);
        MoveWindow(app.status, pad, ch - sh + s(3), cw - 2 * pad, sh - s(3), 1);
    }
}

fn toggle_visible(hwnd: HWND, edit: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) != 0 && GetForegroundWindow() == hwnd {
            ShowWindow(hwnd, SW_HIDE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
            SetFocus(edit);
            SendMessageW(edit, EM_SETSEL as u32, 0, -1);
        }
    }
}

// ---------------------------------------------------------------------------
// Window procedure.

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(r) = shellmenu::forward(msg, wp, lp) {
        return r; // owner-drawn shell submenus ("Send to", "Open with")
    }
    match msg {
        WM_SIZE => {
            with_app(|a| layout(a));
            0
        }
        WM_COMMAND => {
            let id = wp & 0xFFFF;
            let code = (wp >> 16) as u32;
            if id == ID_EDIT && code == EN_CHANGE {
                with_app(|a| submit_query(a));
            }
            0
        }
        WM_APP_RESULT => {
            let r = unsafe { Box::from_raw(lp as *mut SearchResult) };
            with_app(|a| {
                if r.seq_no == a.shared.seq_no.load(Ordering::SeqCst) {
                    // New query → old row selection is meaningless. Same query
                    // re-run after a live update → keep it.
                    let new_query = a.res.as_ref().is_none_or(|old| old.q != r.q);
                    a.res = Some(*r);
                    if a.sort != (0, false) {
                        apply_sort(a);
                    }
                    let n = a.res.as_ref().map_or(0, |r| r.ids.len());
                    unsafe {
                        if new_query {
                            let mut it: LVITEMW = std::mem::zeroed();
                            it.stateMask = LVIS_SELECTED;
                            SendMessageW(a.list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
                        }
                        SendMessageW(a.list, LVM_SETITEMCOUNT, n, (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as isize);
                        InvalidateRect(a.list, std::ptr::null(), 0);
                    }
                    update_status(a);
                }
            });
            0
        }
        WM_CONTEXTMENU => {
            let is_list = with_app(|a| wp as HWND == a.list).unwrap_or(false);
            if is_list {
                context_menu(hwnd, lp);
                0
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
            }
        }
        WM_APP_RENAME_SEL => {
            // Explorer-style: select the name without its extension.
            with_app(|a| unsafe {
                let ed = SendMessageW(a.list, LVM_GETEDITCONTROL, 0, 0) as HWND;
                if !ed.is_null() {
                    let len = GetWindowTextLengthW(ed) as usize;
                    let mut buf = vec![0u16; len + 1];
                    GetWindowTextW(ed, buf.as_mut_ptr(), buf.len() as i32);
                    let name = &buf[..len];
                    let end = match name.iter().rposition(|&c| c == b'.' as u16) {
                        Some(k) if k > 0 && wp == 0 => k,
                        _ => len,
                    };
                    SendMessageW(ed, EM_SETSEL as u32, 0, end as isize);
                }
            });
            0
        }
        WM_APP_SELINFO => {
            SELINFO_PENDING.set(false);
            with_app(|a| update_status(a));
            0
        }
        WM_APP_INDEX => {
            with_app(|a| {
                if let Some(ix) = a.svc.as_ref().and_then(|s| s.index()) {
                    *a.shared.ix.write().unwrap() = Some(ix);
                }
                submit_query(a); // re-run the current query on the new index
            });
            0
        }
        WM_APP_STATUS => {
            with_app(|a| {
                if let Some(s) = &a.svc {
                    *a.shared.status.lock().unwrap() = s.status().2;
                }
                update_status(a);
            });
            0
        }
        WM_NOTIFY => {
            let hdr = unsafe { &*(lp as *const NMHDR) };
            if hdr.idFrom == ID_LIST {
                match hdr.code {
                    LVN_GETDISPINFOW => {
                        let di = unsafe { &mut *(lp as *mut NMLVDISPINFOW) };
                        if di.item.mask & LVIF_TEXT != 0 && !di.item.pszText.is_null() && di.item.cchTextMax > 0 {
                            with_app(|a| {
                                let t = cell_text(a, di.item.iItem as usize, di.item.iSubItem);
                                a.text.clear();
                                a.text.extend(t.encode_utf16().take(di.item.cchTextMax as usize - 1));
                                a.text.push(0);
                                unsafe { std::ptr::copy_nonoverlapping(a.text.as_ptr(), di.item.pszText, a.text.len()) };
                            });
                        }
                    }
                    NM_DBLCLK => {
                        let ia = unsafe { &*(lp as *const NMITEMACTIVATE) };
                        if ia.iItem >= 0
                            && let Some(Some(p)) = with_app(|a| row_path(a, ia.iItem as usize))
                        {
                            open_path(&p, false);
                        }
                    }
                    LVN_COLUMNCLICK => {
                        let nm = unsafe { &*(lp as *const NMLISTVIEW) };
                        with_app(|a| {
                            let col = nm.iSubItem as usize;
                            a.sort = if a.sort.0 == col { (col, !a.sort.1) } else { (col, col >= COL_SIZE) };
                            apply_sort(a);
                            unsafe { InvalidateRect(a.list, std::ptr::null(), 0) };
                        });
                    }
                    LVN_ITEMCHANGED | LVN_ODSTATECHANGED => {
                        // Coalesced: a range selection fires many notifications.
                        if !SELINFO_PENDING.replace(true) {
                            unsafe { PostMessageW(hwnd, WM_APP_SELINFO, 0, 0) };
                        }
                    }
                    LVN_BEGINLABELEDITW => {
                        if !RENAME_ARMED.get() {
                            return 1; // no rename by slow double click
                        }
                        let is_dir = with_app(|a| {
                            let di = unsafe { &*(lp as *const NMLVDISPINFOW) };
                            a.res.as_ref().and_then(|r| r.ids.get(di.item.iItem as usize)).is_some_and(|&id| a.res.as_ref().unwrap().ix.is_dir(id))
                        });
                        unsafe { PostMessageW(hwnd, WM_APP_RENAME_SEL, is_dir.unwrap_or(false) as usize, 0) };
                        return 0;
                    }
                    LVN_ENDLABELEDITW => {
                        let di = unsafe { &*(lp as *const NMLVDISPINFOW) };
                        let new_name = (!di.item.pszText.is_null()).then(|| unsafe {
                            let p = di.item.pszText;
                            let n = (0..).take_while(|&k| *p.add(k) != 0).count();
                            String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
                        });
                        finish_rename(hwnd, new_name);
                        return 0; // owner-data: the index (live updates) supplies the new name
                    }
                    _ => {}
                }
            }
            0
        }
        WM_HOTKEY => {
            with_app(|a| toggle_visible(a.hwnd, a.edit));
            0
        }
        WM_DPICHANGED => {
            let rc = unsafe { &*(lp as *const RECT) };
            unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Start of the word to the left of `caret` (UTF-16 units, like an EDIT control):
/// spaces, then one run of word characters or of punctuation.
fn word_left(text: &[u16], caret: usize) -> usize {
    #[derive(PartialEq)]
    enum Class {
        Space,
        Word,
        Other,
    }
    let class = |u: u16| match char::from_u32(u as u32) {
        Some(c) if c.is_whitespace() => Class::Space,
        Some(c) if !c.is_alphanumeric() && c != '_' => Class::Other,
        _ => Class::Word, // letters, digits and surrogates (astral characters)
    };
    let mut k = caret.min(text.len());
    while k > 0 && class(text[k - 1]) == Class::Space {
        k -= 1;
    }
    if k > 0 {
        let c = class(text[k - 1]);
        while k > 0 && class(text[k - 1]) == c {
            k -= 1;
        }
    }
    k
}

/// Ctrl+Backspace. Unhandled, it reaches the control as the DEL character (0x7F)
/// and silently becomes part of the query.
fn delete_word_left(edit: HWND) {
    unsafe {
        let (mut start, mut end) = (0u32, 0u32);
        SendMessageW(edit, EM_GETSEL as u32, &mut start as *mut u32 as usize, &mut end as *mut u32 as isize);
        if start == end {
            let len = GetWindowTextLengthW(edit) as usize;
            let mut buf = vec![0u16; len + 1];
            let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            start = word_left(&buf[..n], end as usize) as u32;
            if start == end {
                return;
            }
            SendMessageW(edit, EM_SETSEL as u32, start as usize, end as isize);
        }
        // Undoable; notifies EN_CHANGE, so the search box re-runs the query.
        SendMessageW(edit, EM_REPLACESEL as u32, 1, w("").as_ptr() as isize);
    }
}

/// Keyboard shortcuts handled before dispatch. Returns true if consumed.
fn pre_translate(msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN {
        return false;
    }
    // AltGr arrives as Ctrl+Alt: it types characters (`@` is AltGr+Q on some
    // layouts) and must not trigger Ctrl shortcuts.
    let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) < 0 && GetKeyState(VK_MENU as i32) >= 0 };
    let shift = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;
    let alt = msg.message == WM_SYSKEYDOWN;
    let vk = msg.wParam as u16;

    /// Actions that may run modal loops: performed after `with_app` returns.
    enum Act {
        Open(String, bool),
        Verb(&'static str, bool),
        CopyPaths,
        Rename(usize),
        SelectAll(HWND),
    }
    let mut act = None;
    let consumed = with_app(|a| {
        let in_edit = msg.hwnd == a.edit;
        let in_list = msg.hwnd == a.list;
        // Search box or the inline rename control.
        let in_text = in_edit || (!in_list && !msg.hwnd.is_null() && msg.hwnd == unsafe { SendMessageW(a.list, LVM_GETEDITCONTROL, 0, 0) } as HWND);
        if in_text && ctrl && !alt {
            match vk {
                0x41 if !shift => {
                    unsafe { SendMessageW(msg.hwnd, EM_SETSEL as u32, 0, -1) };
                    return true;
                }
                VK_BACK => {
                    delete_word_left(msg.hwnd);
                    return true;
                }
                _ => {}
            }
        }
        if !(in_edit || in_list) {
            return false;
        }
        if alt {
            if vk == VK_RETURN && in_list {
                act = Some(Act::Verb("properties", false));
                return true;
            }
            return false;
        }
        match vk {
            VK_ESCAPE => {
                unsafe { ShowWindow(a.hwnd, SW_HIDE) };
                true
            }
            VK_RETURN => {
                if let Some(p) = selected_row(a).and_then(|r| row_path(a, r)) {
                    act = Some(Act::Open(p, ctrl));
                }
                true
            }
            VK_DELETE if in_list => {
                act = Some(Act::Verb("delete", shift));
                true
            }
            VK_F2 if in_list => {
                act = focused_row(a).map(Act::Rename);
                true
            }
            0x43 if ctrl && in_list => {
                // Ctrl+C files · Ctrl+Shift+C full paths as text
                act = Some(if shift { Act::CopyPaths } else { Act::Verb("copy", false) });
                true
            }
            0x58 if ctrl && in_list => {
                act = Some(Act::Verb("cut", false)); // Ctrl+X
                true
            }
            0x41 if ctrl && in_list => {
                // Ctrl+A (owner-data list views don't do it themselves)
                act = Some(Act::SelectAll(a.list));
                true
            }
            VK_DOWN | VK_NEXT if in_edit => {
                unsafe {
                    SetFocus(a.list);
                    if SendMessageW(a.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) < 0 {
                        let mut it: LVITEMW = std::mem::zeroed();
                        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                        it.state = LVIS_SELECTED | LVIS_FOCUSED;
                        SendMessageW(a.list, LVM_SETITEMSTATE, 0, &it as *const _ as isize);
                    }
                }
                true
            }
            VK_UP if in_list => {
                let first = unsafe { SendMessageW(a.list, LVM_GETNEXTITEM, usize::MAX, (LVNI_SELECTED | LVNI_FOCUSED) as isize) } <= 0;
                if first {
                    unsafe { SetFocus(a.edit) };
                }
                first
            }
            0x51 if ctrl => {
                // Ctrl+Q
                unsafe { DestroyWindow(a.hwnd) };
                true
            }
            _ => false,
        }
    })
    .unwrap_or(false);
    let hwnd = unsafe { GetAncestor(msg.hwnd, GA_ROOT) };
    match act {
        Some(Act::Open(p, folder)) => open_path(&p, folder),
        Some(Act::Verb(v, shift)) => shell_verb(hwnd, v, shift),
        Some(Act::CopyPaths) => copy_paths(hwnd),
        Some(Act::Rename(row)) => begin_rename(row),
        Some(Act::SelectAll(list)) => unsafe {
            let mut it: LVITEMW = std::mem::zeroed();
            it.stateMask = LVIS_SELECTED;
            it.state = LVIS_SELECTED;
            SendMessageW(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
        },
        None => {}
    }
    consumed
}

// ---------------------------------------------------------------------------

/// Visual styles (Common Controls v6) without shipping a manifest file:
/// activate the manifest embedded in shell32.dll (resource 124).
fn enable_visual_styles() {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::{ActivateActCtx, CreateActCtxW, ACTCTXW};
    const ACTCTX_FLAG_RESOURCE_NAME_VALID: u32 = 0x8;
    unsafe {
        let src = w("shell32.dll");
        let mut ctx: ACTCTXW = std::mem::zeroed();
        ctx.cbSize = std::mem::size_of::<ACTCTXW>() as u32;
        ctx.dwFlags = ACTCTX_FLAG_RESOURCE_NAME_VALID;
        ctx.lpSource = src.as_ptr();
        ctx.lpResourceName = 124 as *const u16;
        let h = CreateActCtxW(&ctx);
        if h != INVALID_HANDLE_VALUE {
            let mut cookie = 0usize;
            ActivateActCtx(h, &mut cookie);
        }
    }
}

fn ui_font(dpi: u32) -> HFONT {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, &mut ncm as *mut _ as *mut _, 0, dpi);
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

/// GUI entry. Options: `--live off` (no UAC prompt, no live updates), `--data DIR`.
pub fn run_with(args: &[String]) -> i32 {
    use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    us_win::util::harden_process();
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    enable_visual_styles();
    // STA + OLE: the shell context menu and its clipboard verbs need it.
    unsafe { windows_sys::Win32::System::Ole::OleInitialize(std::ptr::null()) };
    unsafe {
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES };
        InitCommonControlsEx(&icc);
    }
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let cls = w("unthinkable_search_main");
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszClassName: cls.as_ptr(),
            hIcon: LoadIconW(std::ptr::null_mut(), IDI_APPLICATION),
            ..std::mem::zeroed()
        };
        if RegisterClassExW(&wc) == 0 {
            eprintln!("RegisterClassExW failed: {}", std::io::Error::last_os_error());
        }
    }
    let title = w(if is_elevated() { "unthinkable-search (администратор)" } else { "unthinkable-search" });
    let hwnd = unsafe {
        CreateWindowExW(0, cls.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, 1100, 700, std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null())
    };
    if hwnd.is_null() {
        eprintln!("CreateWindowExW failed: {}", std::io::Error::last_os_error());
        return 1;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let font = ui_font(dpi);
    let edit = unsafe {
        CreateWindowExW(WS_EX_CLIENTEDGE, w("EDIT").as_ptr(), std::ptr::null(), WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32, 0, 0, 0, 0, hwnd, ID_EDIT as HMENU, hinst, std::ptr::null())
    };
    let list = unsafe {
        CreateWindowExW(
            0,
            WC_LISTVIEWW,
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | (LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_EDITLABELS) as u32,
            0, 0, 0, 0, hwnd, ID_LIST as HMENU, hinst, std::ptr::null(),
        )
    };
    let status = unsafe {
        CreateWindowExW(0, w("STATIC").as_ptr(), std::ptr::null(), WS_CHILD | WS_VISIBLE | SS_LEFTNOWORDWRAP as u32, 0, 0, 0, 0, hwnd, std::ptr::null_mut(), hinst, std::ptr::null())
    };
    unsafe {
        for h in [edit, list, status] {
            SendMessageW(h, WM_SETFONT, font as usize, 1);
        }
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);
        let s = |v: i32| v * dpi.max(96) as i32 / 96;
        for (i, (name, width, right)) in [("Имя", s(280), false), ("Путь", s(420), false), ("Размер", s(100), true), ("Изменён", s(125), false), ("Создан", s(125), false)].iter().enumerate() {
            let t = w(name);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                fmt: if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT },
                cx: *width,
                pszText: t.as_ptr() as *mut u16,
                ..std::mem::zeroed()
            };
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }
        RegisterHotKey(hwnd, HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 0x46); // Ctrl+Alt+F
    }
    let shared = Arc::new(Shared {
        ix: RwLock::new(None),
        req: Mutex::new(None),
        req_cv: Condvar::new(),
        seq_no: AtomicU64::new(0),
        cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
        status: Mutex::new("Загрузка индекса…".into()),
        quit: AtomicBool::new(false),
    });
    APP.with(|a| {
        *a.borrow_mut() = Some(App { hwnd, edit, list, status, shared: shared.clone(), res: None, sort: (0, false), text: Vec::new(), svc: None })
    });
    with_app(|a| {
        layout(a);
        update_status(a);
    });
    let hw = hwnd as usize;
    let s1 = shared.clone();
    let searcher = std::thread::spawn(move || search_worker(s1, hw as HWND));
    // Events are only posted; the UI thread reads the service in the handlers.
    let mut opts = service::Options::next_to_exe();
    if crate::cli::opt(args, "--live") == Some("off") {
        opts.live = service::LiveMode::Off;
    }
    if crate::cli::opt(args, "--data").is_some() {
        opts.data_dir = crate::cli::data_dir(args);
    }
    let svc = Service::start(opts, move |ev| {
        let m = match ev {
            service::Event::Index => WM_APP_INDEX,
            service::Event::Status => WM_APP_STATUS,
        };
        unsafe { PostMessageW(hw as HWND, m, 0, 0) };
    });
    with_app(|a| a.svc = Some(svc));
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(edit);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if pre_translate(&msg) {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
        // Files copied/cut via the menu must stay pasteable after we exit.
        windows_sys::Win32::System::Ole::OleFlushClipboard();
        windows_sys::Win32::System::Ole::OleUninitialize();
    }
    shared.quit.store(true, Ordering::SeqCst);
    shared.req_cv.notify_all();
    let _ = searcher.join();
    if let Some(svc) = with_app(|a| a.svc.take()).flatten() {
        svc.stop(); // saves the index with fresh USN cursors
    }
    0
}

pub fn run() -> i32 {
    run_with(&[])
}

#[cfg(test)]
mod tests {
    use super::word_left;

    #[test]
    fn word_left_stops_at_word_boundaries() {
        let at = |s: &str, caret: usize| word_left(&s.encode_utf16().collect::<Vec<_>>(), caret);
        assert_eq!(at("", 0), 0);
        assert_eq!(at("foo", 0), 0);
        assert_eq!(at("foo", 3), 0);
        assert_eq!(at("foo bar", 7), 4);
        assert_eq!(at("foo bar  ", 9), 4);
        assert_eq!(at("foo bar", 5), 4); // from the middle of a word
        assert_eq!(at("отчёт год", 9), 6);
        assert_eq!(at(r"windows\system32", 16), 8);
        assert_eq!(at(r"windows\", 8), 7);
        assert_eq!(at("ext:pdf", 4), 3);
        assert_eq!(at("a_b", 3), 0);
        assert_eq!(at("a 😀", 4), 2); // never splits a surrogate pair
        assert_eq!(at("foo", 99), 0); // caret past the end
    }
}
