//! Explorer context menu (`IContextMenu`) for indexed files â€” the same menu
//! Explorer shows: Open, Send to, Cut/Copy, Delete, Properties, shell
//! extensions (7-Zip, Defender, â€¦). No `windows` crate: the four COM vtables
//! we need are declared by hand below.
//!
//! Must run on an STA/OLE thread (`OleInitialize`) â€” clipboard verbs (copy,
//! cut) hand an `IDataObject` to OLE.
//!
//! Re-entrancy: `TrackPopupMenuEx` and `InvokeCommand` (delete confirmation,
//! "Open with" dialog) run modal loops that dispatch messages to the window
//! procedure. Callers must not hold any `RefCell` borrow across these calls.

// HWND/HMENU are opaque handles, not dereferenced pointers.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::c_void;
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const IID_ISHELLFOLDER: GUID = GUID::from_u128(0x000214E6_0000_0000_C000_000000000046);
const IID_ICONTEXTMENU: GUID = GUID::from_u128(0x000214E4_0000_0000_C000_000000000046);
const IID_ICONTEXTMENU2: GUID = GUID::from_u128(0x000214F4_0000_0000_C000_000000000046);
const IID_ICONTEXTMENU3: GUID = GUID::from_u128(0xBCFCE0A0_EC17_11D0_8D10_00A0C90F2719);

/// First command id handed to the shell; ids below are ours.
pub const SHELL_FIRST: u32 = 100;
const SHELL_LAST: u32 = 0x7FFF;
const CMIC_MASK_UNICODE: u32 = 0x4000;

type Raw = *mut c_void;

#[repr(C)]
struct UnkVtbl {
    query_interface: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT,
    add_ref: unsafe extern "system" fn(Raw) -> u32,
    release: unsafe extern "system" fn(Raw) -> u32,
}

#[repr(C)]
struct ShellFolderVtbl {
    unk: UnkVtbl,
    // ParseDisplayName, EnumObjects, BindToObject, BindToStorage, CompareIDs,
    // CreateViewObject, GetAttributesOf
    _skip: [usize; 7],
    get_ui_object_of: unsafe extern "system" fn(Raw, HWND, u32, *const *const ITEMIDLIST, *const GUID, *mut u32, *mut Raw) -> HRESULT,
}

#[repr(C)]
struct ContextMenuVtbl {
    unk: UnkVtbl,
    query_context_menu: unsafe extern "system" fn(Raw, HMENU, u32, u32, u32, u32) -> HRESULT,
    invoke_command: unsafe extern "system" fn(Raw, *const CMINVOKECOMMANDINFOEX) -> HRESULT,
    get_command_string: unsafe extern "system" fn(Raw, usize, u32, *const u32, *mut u8, u32) -> HRESULT,
}

#[repr(C)]
struct ContextMenu2Vtbl {
    base: ContextMenuVtbl,
    handle_menu_msg: unsafe extern "system" fn(Raw, u32, WPARAM, LPARAM) -> HRESULT,
}

#[repr(C)]
struct ContextMenu3Vtbl {
    base: ContextMenu2Vtbl,
    handle_menu_msg2: unsafe extern "system" fn(Raw, u32, WPARAM, LPARAM, *mut LRESULT) -> HRESULT,
}

#[inline]
unsafe fn vt<T>(p: Raw) -> *const T {
    unsafe { *(p as *const *const T) }
}

unsafe fn release(p: Raw) {
    if !p.is_null() {
        unsafe { ((*vt::<UnkVtbl>(p)).release)(p) };
    }
}

unsafe fn query(p: Raw, iid: &GUID) -> Raw {
    let mut out: Raw = std::ptr::null_mut();
    if unsafe { ((*vt::<UnkVtbl>(p)).query_interface)(p, iid, &mut out) } < 0 {
        return std::ptr::null_mut();
    }
    out
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

thread_local! {
    /// IContextMenu3/2 of the menu currently being tracked (owner-drawn
    /// submenus like "Send to" / "Open with" need the menu messages).
    static ACTIVE: Cell<(Raw, Raw)> = const { Cell::new((std::ptr::null_mut(), std::ptr::null_mut())) };
}

/// Forward menu messages to the active shell menu. Call first thing in the
/// window procedure; `Some` = handled.
pub fn forward(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if !matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        return None;
    }
    let (cm3, cm2) = ACTIVE.with(|a| a.get());
    unsafe {
        if !cm3.is_null() {
            let mut r: LRESULT = 0;
            if ((*vt::<ContextMenu3Vtbl>(cm3)).handle_menu_msg2)(cm3, msg, wp, lp, &mut r) >= 0 {
                return Some(r);
            }
        } else if !cm2.is_null() && ((*vt::<ContextMenu2Vtbl>(cm2)).handle_menu_msg)(cm2, msg, wp, lp) >= 0 {
            return Some(if msg == WM_INITMENUPOPUP { 0 } else { 1 });
        }
    }
    None
}

/// Shell items + their context menu handler.
pub struct Items {
    pidls: Vec<*mut ITEMIDLIST>,
    cm: Raw,
    cm2: Raw,
    cm3: Raw,
}

impl Drop for Items {
    fn drop(&mut self) {
        unsafe {
            release(self.cm3);
            release(self.cm2);
            release(self.cm);
            for &p in &self.pidls {
                ILFree(p);
            }
        }
    }
}

pub enum Pick {
    None,
    /// One of the caller's own items.
    Own(u32),
    /// The shell's "Rename" verb â€” the host has to rename itself.
    Rename,
    /// A shell command was executed.
    Invoked,
}

impl Items {
    /// Bind `paths` (full paths). Items in one folder go through that folder
    /// (full fidelity, all shell extensions); a selection spanning folders goes
    /// through the desktop folder with absolute item ids.
    pub fn new(hwnd: HWND, paths: &[String]) -> Result<Items, String> {
        let mut it = Items { pidls: Vec::with_capacity(paths.len()), cm: std::ptr::null_mut(), cm2: std::ptr::null_mut(), cm3: std::ptr::null_mut() };
        if paths.is_empty() {
            return Err("Ð½Ð¸Ñ‡ÐµÐ³Ð¾ Ð½Ðµ Ð²Ñ‹Ð±Ñ€Ð°Ð½Ð¾".into());
        }
        unsafe {
            for p in paths {
                let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
                let hr = SHParseDisplayName(w(p).as_ptr(), std::ptr::null_mut(), &mut pidl, 0, std::ptr::null_mut());
                if hr < 0 || pidl.is_null() {
                    return Err(format!("Ð½Ðµ Ð½Ð°Ð¹Ð´ÐµÐ½: {p}"));
                }
                it.pidls.push(pidl);
            }
            let parent_of = |p: &str| p.rfind('\\').map(|k| p[..k].to_ascii_lowercase());
            let same_parent = paths.iter().all(|p| parent_of(p) == parent_of(&paths[0]));
            let mut folder: Raw = std::ptr::null_mut();
            let mut children: Vec<*const ITEMIDLIST> = Vec::with_capacity(paths.len());
            if same_parent {
                for (k, &p) in it.pidls.iter().enumerate() {
                    let mut f: Raw = std::ptr::null_mut();
                    let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
                    if SHBindToParent(p, &IID_ISHELLFOLDER, &mut f, &mut last) < 0 || f.is_null() {
                        release(folder);
                        return Err("SHBindToParent".into());
                    }
                    if k == 0 { folder = f } else { release(f) }
                    children.push(last);
                }
            } else {
                if SHGetDesktopFolder(&mut folder) < 0 || folder.is_null() {
                    return Err("SHGetDesktopFolder".into());
                }
                children.extend(it.pidls.iter().map(|&p| p as *const ITEMIDLIST));
            }
            let hr = ((*vt::<ShellFolderVtbl>(folder)).get_ui_object_of)(
                folder,
                hwnd,
                children.len() as u32,
                children.as_ptr(),
                &IID_ICONTEXTMENU,
                std::ptr::null_mut(),
                &mut it.cm,
            );
            release(folder);
            if hr < 0 || it.cm.is_null() {
                return Err(format!("GetUIObjectOf: 0x{:08X}", hr as u32));
            }
            it.cm3 = query(it.cm, &IID_ICONTEXTMENU3);
            if it.cm3.is_null() {
                it.cm2 = query(it.cm, &IID_ICONTEXTMENU2);
            }
        }
        Ok(it)
    }

    fn query_menu(&self, hmenu: HMENU, flags: u32) -> bool {
        unsafe { ((*vt::<ContextMenuVtbl>(self.cm)).query_context_menu)(self.cm, hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) >= 0 }
    }

    /// Language-independent verb of shell command `id` ("open", "delete", "rename", â€¦).
    fn verb(&self, id: u32) -> String {
        let mut buf = [0u16; 128];
        let hr = unsafe {
            ((*vt::<ContextMenuVtbl>(self.cm)).get_command_string)(self.cm, (id - SHELL_FIRST) as usize, GCS_VERBW, std::ptr::null(), buf.as_mut_ptr() as *mut u8, buf.len() as u32)
        };
        if hr < 0 {
            return String::new();
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    fn invoke(&self, hwnd: HWND, verb_id: Option<u32>, verb: &str, pt: POINT, shift: bool, ctrl: bool) -> bool {
        let verb_a: Vec<u8> = verb.bytes().chain(std::iter::once(0)).collect();
        let verb_w = w(verb);
        let mut ci: CMINVOKECOMMANDINFOEX = unsafe { std::mem::zeroed() };
        ci.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32;
        ci.fMask = CMIC_MASK_UNICODE
            | CMIC_MASK_PTINVOKE
            | if shift { CMIC_MASK_SHIFT_DOWN } else { 0 }
            | if ctrl { CMIC_MASK_CONTROL_DOWN } else { 0 };
        ci.hwnd = hwnd;
        match verb_id {
            // MAKEINTRESOURCE(offset)
            Some(id) => {
                ci.lpVerb = (id - SHELL_FIRST) as usize as *const u8;
                ci.lpVerbW = (id - SHELL_FIRST) as usize as *const u16;
            }
            None => {
                ci.lpVerb = verb_a.as_ptr();
                ci.lpVerbW = verb_w.as_ptr();
            }
        }
        ci.nShow = SW_SHOWNORMAL;
        ci.ptInvoke = pt;
        unsafe { ((*vt::<ContextMenuVtbl>(self.cm)).invoke_command)(self.cm, &ci) >= 0 }
    }

    /// Run a verb without showing the menu ("delete", "copy", "cut", "properties").
    pub fn invoke_verb(&self, hwnd: HWND, verb: &str, shift: bool) -> bool {
        unsafe {
            let hmenu = CreatePopupMenu();
            // Default menu implementations resolve string verbs only after QueryContextMenu.
            let ok = self.query_menu(hmenu, CMF_NORMAL | CMF_ITEMMENU) && {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                self.invoke(hwnd, None, verb, pt, shift, false)
            };
            DestroyMenu(hmenu);
            ok
        }
    }

    /// Show the menu at screen point `pt`. `own` items (id < SHELL_FIRST, label;
    /// id 0 = separator) are inserted right after the shell's default item.
    pub fn show(&self, hwnd: HWND, pt: POINT, own: &[(u32, &str)], extended: bool) -> Pick {
        unsafe {
            let hmenu = CreatePopupMenu();
            if hmenu.is_null() {
                return Pick::None;
            }
            let flags = CMF_NORMAL | CMF_ITEMMENU | CMF_CANRENAME | if extended { CMF_EXTENDEDVERBS } else { 0 };
            if !self.query_menu(hmenu, flags) {
                DestroyMenu(hmenu);
                return Pick::None;
            }
            let def = GetMenuDefaultItem(hmenu, 1, 0);
            let mut pos = if def == u32::MAX { 0 } else { def + 1 };
            for &(id, label) in own {
                let t = w(label);
                if id == 0 {
                    InsertMenuW(hmenu, pos, MF_BYPOSITION | MF_SEPARATOR, 0, std::ptr::null());
                } else {
                    InsertMenuW(hmenu, pos, MF_BYPOSITION | MF_STRING, id as usize, t.as_ptr());
                }
                pos += 1;
            }
            ACTIVE.with(|a| a.set((self.cm3, self.cm2)));
            let cmd = TrackPopupMenuEx(hmenu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, hwnd, std::ptr::null()) as u32;
            ACTIVE.with(|a| a.set((std::ptr::null_mut(), std::ptr::null_mut())));
            let pick = if cmd == 0 {
                Pick::None
            } else if cmd < SHELL_FIRST {
                Pick::Own(cmd)
            } else if self.verb(cmd).eq_ignore_ascii_case("rename") {
                Pick::Rename
            } else {
                let shift = key_down(0x10);
                let ctrl = key_down(0x11);
                self.invoke(hwnd, Some(cmd), "", pt, shift, ctrl);
                Pick::Invoked
            };
            DestroyMenu(hmenu);
            pick
        }
    }

    /// Menu labels (for tests / diagnostics), without showing anything.
    pub fn labels(&self) -> Vec<String> {
        let mut out = Vec::new();
        unsafe {
            let hmenu = CreatePopupMenu();
            if self.query_menu(hmenu, CMF_NORMAL | CMF_ITEMMENU | CMF_CANRENAME) {
                let n = GetMenuItemCount(hmenu);
                for i in 0..n.max(0) as u32 {
                    let mut buf = [0u16; 256];
                    let len = GetMenuStringW(hmenu, i, buf.as_mut_ptr(), buf.len() as i32, MF_BYPOSITION);
                    let id = GetMenuItemID(hmenu, i as i32);
                    let label = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
                    let verb = if id != u32::MAX && id >= SHELL_FIRST { self.verb(id) } else { String::new() };
                    out.push(if label.is_empty() { "â€”".into() } else if verb.is_empty() { label } else { format!("{label}  [{verb}]") });
                }
            }
            DestroyMenu(hmenu);
        }
        out
    }
}


fn key_down(vk: i32) -> bool {
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState(vk) < 0 }
}

/// Put text on the clipboard (CF_UNICODETEXT).
pub fn set_clipboard_text(hwnd: HWND, s: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    const CF_UNICODETEXT: u32 = 13;
    let u = w(s);
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, u.len() * 2);
        let mut ok = false;
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(u.as_ptr(), p, u.len());
                GlobalUnlock(h);
                ok = !SetClipboardData(CF_UNICODETEXT, h).is_null();
            }
            if !ok {
                GlobalFree(h);
            }
        }
        CloseClipboard();
        ok
    }
}
