//! Hand-written COM glue, kept small and in one place:
//!
//! * an `IDropTarget` so files dragged from Explorer can be dropped on the notch
//!   (and the notch can open while they hover over it),
//! * an `IDropSource` + shell data object to drag shelf items back out,
//! * "what folder is under this point?" — UI Automation for the item under the
//!   cursor plus `IShellWindows` for the Explorer window's location,
//! * clipboard text / file-list helpers.
//!
//! `windows-sys` has no COM interface wrappers, so the few vtable slots we use
//! are called directly. Slot numbers are from the Windows SDK headers.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
    System::{Com::*, DataExchange::*, Memory::*, Ole::*, SystemServices::MODIFIERKEYS_FLAGS},
    UI::{Controls::ILD_TRANSPARENT, Shell::*, WindowsAndMessaging::*},
};

use crate::wide;

/// Posted to the notch window by the drop target. Handlers read [`PENDING_DROP`].
pub const WM_DRAG_ENTER: u32 = WM_APP + 10;
pub const WM_DRAG_LEAVE: u32 = WM_APP + 11;
pub const WM_DROP_FILES: u32 = WM_APP + 12;

thread_local! {
    /// Files from the last OLE drop, handed to the host via `WM_DROP_FILES` so
    /// the drop callback never touches host state re-entrantly.
    pub static PENDING_DROP: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);
const IID_IDROPTARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);
const IID_IDROPSOURCE: GUID = GUID::from_u128(0x00000121_0000_0000_c000_000000000046);
const CLSID_CUIAUTOMATION: GUID = GUID::from_u128(0xff48dba4_60ef_4201_aa87_54103eef594e);
const IID_IUIAUTOMATION: GUID = GUID::from_u128(0x30cbe57d_d9d0_452a_ab13_7ac5ac4825ee);
const CLSID_SHELLWINDOWS: GUID = GUID::from_u128(0x9ba05972_f6a8_11cf_a442_00a0c90a8f39);
const IID_ISHELLWINDOWS: GUID = GUID::from_u128(0x85cb6900_4d95_11cf_960c_0080c7f4ee85);
const IID_IWEBBROWSERAPP: GUID = GUID::from_u128(0x0002df05_0000_0000_c000_000000000046);
const IID_ISERVICEPROVIDER: GUID = GUID::from_u128(0x6d5140c1_7436_11ce_8034_00aa006009fa);
const SID_STOPLEVELBROWSER: GUID = GUID::from_u128(0x4c96be40_915c_11cf_99d3_00aa004ae837);
const IID_ISHELLBROWSER: GUID = GUID::from_u128(0x000214e2_0000_0000_c000_000000000046);
const IID_IIMAGELIST: GUID = GUID::from_u128(0x46eb5926_582e_4017_9fdf_e8998daa0950);

const UIA_LISTITEM: i32 = 50007;
const UIA_TREEITEM: i32 = 50024;

type Raw = *mut c_void;

/// Fetch vtable slot `n` of a COM object as function type `F`.
unsafe fn slot<F: Copy>(obj: Raw, n: usize) -> F {
    let vtbl = *(obj as *const *const usize);
    std::mem::transmute_copy(&*vtbl.add(n))
}

unsafe fn release(obj: Raw) {
    if !obj.is_null() {
        slot::<unsafe extern "system" fn(Raw) -> u32>(obj, 2)(obj);
    }
}

unsafe fn query(obj: Raw, iid: &GUID) -> Raw {
    let mut out = null_mut();
    slot::<unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT>(obj, 0)(
        obj, iid, &mut out,
    );
    out
}

unsafe fn take_bstr(b: *mut u16) -> String {
    if b.is_null() {
        return String::new();
    }
    let len = windows_sys::Win32::Foundation::SysStringLen(b) as usize;
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(b, len));
    SysFreeString(b);
    s
}

// --- IDropTarget ----------------------------------------------------------

#[repr(C)]
struct DropTarget {
    vtbl: *const DropTargetVtbl,
    refs: AtomicU32,
    hwnd: HWND,
}

#[repr(C)]
struct DropTargetVtbl {
    query_interface: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT,
    add_ref: unsafe extern "system" fn(Raw) -> u32,
    release: unsafe extern "system" fn(Raw) -> u32,
    drag_enter:
        unsafe extern "system" fn(Raw, Raw, MODIFIERKEYS_FLAGS, POINTL, *mut u32) -> HRESULT,
    drag_over: unsafe extern "system" fn(Raw, MODIFIERKEYS_FLAGS, POINTL, *mut u32) -> HRESULT,
    drag_leave: unsafe extern "system" fn(Raw) -> HRESULT,
    drop: unsafe extern "system" fn(Raw, Raw, MODIFIERKEYS_FLAGS, POINTL, *mut u32) -> HRESULT,
}

static DROP_TARGET_VTBL: DropTargetVtbl = DropTargetVtbl {
    query_interface: dt_query,
    add_ref: dt_add_ref,
    release: dt_release,
    drag_enter: dt_enter,
    drag_over: dt_over,
    drag_leave: dt_leave,
    drop: dt_drop,
};

unsafe extern "system" fn dt_query(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    if guid_eq(&*iid, &IID_IUNKNOWN) || guid_eq(&*iid, &IID_IDROPTARGET) {
        dt_add_ref(this);
        *out = this;
        S_OK
    } else {
        *out = null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn dt_add_ref(this: Raw) -> u32 {
    (*(this as *mut DropTarget))
        .refs
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

// The target lives for the whole process; it is never freed.
unsafe extern "system" fn dt_release(this: Raw) -> u32 {
    (*(this as *mut DropTarget))
        .refs
        .fetch_sub(1, Ordering::Relaxed)
        .saturating_sub(1)
}

unsafe fn has_files(data: Raw) -> bool {
    let fmt = hdrop_format();
    slot::<unsafe extern "system" fn(Raw, *const FORMATETC) -> HRESULT>(data, 5)(data, &fmt) == S_OK
}

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP,
        ptd: null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    }
}

unsafe extern "system" fn dt_enter(
    this: Raw,
    data: Raw,
    _: MODIFIERKEYS_FLAGS,
    _: POINTL,
    effect: *mut u32,
) -> HRESULT {
    let ok = has_files(data);
    *effect = if ok { DROPEFFECT_LINK } else { DROPEFFECT_NONE };
    if ok {
        PostMessageW((*(this as *mut DropTarget)).hwnd, WM_DRAG_ENTER, 0, 0);
    }
    S_OK
}

unsafe extern "system" fn dt_over(
    _: Raw,
    _: MODIFIERKEYS_FLAGS,
    _: POINTL,
    effect: *mut u32,
) -> HRESULT {
    *effect &= DROPEFFECT_LINK | DROPEFFECT_COPY;
    if *effect == 0 {
        *effect = DROPEFFECT_LINK;
    }
    S_OK
}

unsafe extern "system" fn dt_leave(this: Raw) -> HRESULT {
    PostMessageW((*(this as *mut DropTarget)).hwnd, WM_DRAG_LEAVE, 0, 0);
    S_OK
}

unsafe extern "system" fn dt_drop(
    this: Raw,
    data: Raw,
    _: MODIFIERKEYS_FLAGS,
    _: POINTL,
    effect: *mut u32,
) -> HRESULT {
    let fmt = hdrop_format();
    let mut medium: STGMEDIUM = std::mem::zeroed();
    let hr = slot::<unsafe extern "system" fn(Raw, *const FORMATETC, *mut STGMEDIUM) -> HRESULT>(
        data, 3,
    )(data, &fmt, &mut medium);
    if hr == S_OK {
        let files = hdrop_paths(medium.u.hGlobal as HDROP);
        ReleaseStgMedium(&mut medium);
        PENDING_DROP.with(|p| p.borrow_mut().extend(files));
        *effect = DROPEFFECT_LINK;
        PostMessageW((*(this as *mut DropTarget)).hwnd, WM_DROP_FILES, 0, 0);
    } else {
        *effect = DROPEFFECT_NONE;
        PostMessageW((*(this as *mut DropTarget)).hwnd, WM_DRAG_LEAVE, 0, 0);
    }
    S_OK
}

unsafe fn hdrop_paths(drop: HDROP) -> Vec<PathBuf> {
    let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
    (0..count)
        .filter_map(|i| {
            let len = DragQueryFileW(drop, i, null_mut(), 0);
            let mut buf = vec![0u16; len as usize + 1];
            let got = DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
            (got > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..got as usize])))
        })
        .collect()
}

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// Register the notch window as a file drop target. Needs `OleInitialize`.
pub unsafe fn register_drop_target(hwnd: HWND) {
    let target = Box::into_raw(Box::new(DropTarget {
        vtbl: &DROP_TARGET_VTBL,
        refs: AtomicU32::new(1),
        hwnd,
    }));
    RegisterDragDrop(hwnd, target as Raw);
}

// --- dragging shelf items out ----------------------------------------------

#[repr(C)]
struct DropSource {
    vtbl: *const DropSourceVtbl,
}

#[repr(C)]
struct DropSourceVtbl {
    query_interface: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT,
    add_ref: unsafe extern "system" fn(Raw) -> u32,
    release: unsafe extern "system" fn(Raw) -> u32,
    query_continue_drag: unsafe extern "system" fn(Raw, BOOL, MODIFIERKEYS_FLAGS) -> HRESULT,
    give_feedback: unsafe extern "system" fn(Raw, u32) -> HRESULT,
}

static DROP_SOURCE_VTBL: DropSourceVtbl = DropSourceVtbl {
    query_interface: ds_query,
    add_ref: ds_ref,
    release: ds_ref,
    query_continue_drag: ds_continue,
    give_feedback: ds_feedback,
};

unsafe extern "system" fn ds_query(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    if guid_eq(&*iid, &IID_IUNKNOWN) || guid_eq(&*iid, &IID_IDROPSOURCE) {
        *out = this;
        S_OK
    } else {
        *out = null_mut();
        E_NOINTERFACE
    }
}

// Stack-allocated for the duration of `DoDragDrop`: reference counting is moot.
unsafe extern "system" fn ds_ref(_: Raw) -> u32 {
    1
}

unsafe extern "system" fn ds_continue(_: Raw, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
    if escape != 0 {
        DRAGDROP_S_CANCEL
    } else if keys & windows_sys::Win32::System::SystemServices::MK_LBUTTON == 0 {
        DRAGDROP_S_DROP
    } else {
        S_OK
    }
}

unsafe extern "system" fn ds_feedback(_: Raw, _: u32) -> HRESULT {
    DRAGDROP_S_USEDEFAULTCURSORS
}

/// Start a modal shell drag of `path` (copy or link). Must be called while the
/// left button is down and *without* holding any host borrow: `DoDragDrop`
/// pumps messages.
pub unsafe fn drag_out(path: &Path) {
    let mut full = null_mut();
    let name = wide(&path.to_string_lossy());
    if SHParseDisplayName(name.as_ptr(), null_mut(), &mut full, 0, null_mut()) != S_OK {
        return;
    }
    let child = ILClone(ILFindLastID(full));
    let parent = ILClone(full);
    ILRemoveLastID(parent);
    let mut data = null_mut();
    let children = [child as *const _];
    if SHCreateDataObject(
        parent,
        1,
        children.as_ptr(),
        null_mut(),
        &IID_IDATAOBJECT,
        &mut data,
    ) == S_OK
    {
        let mut source = DropSource {
            vtbl: &DROP_SOURCE_VTBL,
        };
        let mut effect = 0u32;
        DoDragDrop(
            data,
            &mut source as *mut _ as Raw,
            DROPEFFECT_COPY | DROPEFFECT_LINK,
            &mut effect,
        );
        release(data);
    }
    ILFree(child);
    ILFree(parent);
    ILFree(full);
}

// --- what is under the cursor? ---------------------------------------------

pub enum Target {
    Folder(PathBuf),
    File(PathBuf),
}

/// Resolve the folder or file under screen point `pt` (Explorer windows and
/// the desktop). The caller makes its own window click-through first.
pub unsafe fn target_at(pt: POINT) -> Option<Target> {
    let hwnd = WindowFromPoint(pt);
    if hwnd.is_null() {
        return None;
    }
    let root = GetAncestor(hwnd, GA_ROOT);
    let class = class_name(root);
    let (name, kind) = uia_item_at(pt).unwrap_or_default();

    let bases: Vec<PathBuf> = match class.as_str() {
        "CabinetWClass" | "ExploreWClass" => explorer_folder(root).into_iter().collect(),
        "Progman" | "WorkerW" => desktop_folders(),
        _ => return None,
    };

    if !name.is_empty() {
        if kind == UIA_TREEITEM {
            if let Some(p) = known_folder_by_name(&name) {
                return Some(Target::Folder(p));
            }
        }
        for base in &bases {
            if let Some(p) = find_child(base, &name) {
                return Some(if p.is_dir() {
                    Target::Folder(p)
                } else {
                    Target::File(p)
                });
            }
        }
    }
    // Dropped on an Explorer window's background: use the folder it shows.
    if matches!(class.as_str(), "CabinetWClass" | "ExploreWClass") {
        return bases.into_iter().next().map(Target::Folder);
    }
    None
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Name and control type of the list/tree item under `pt`, walking up from
/// the deepest element (e.g. a details-view cell) to its item.
unsafe fn uia_item_at(pt: POINT) -> Option<(String, i32)> {
    let mut uia = null_mut();
    if CoCreateInstance(
        &CLSID_CUIAUTOMATION,
        null_mut(),
        CLSCTX_INPROC_SERVER,
        &IID_IUIAUTOMATION,
        &mut uia,
    ) != S_OK
    {
        return None;
    }
    let mut el = null_mut();
    slot::<unsafe extern "system" fn(Raw, POINT, *mut Raw) -> HRESULT>(uia, 7)(uia, pt, &mut el);
    let mut walker = null_mut();
    slot::<unsafe extern "system" fn(Raw, *mut Raw) -> HRESULT>(uia, 14)(uia, &mut walker);
    let mut found = None;
    for _ in 0..6 {
        if el.is_null() {
            break;
        }
        let mut kind = 0i32;
        slot::<unsafe extern "system" fn(Raw, *mut i32) -> HRESULT>(el, 21)(el, &mut kind);
        if kind == UIA_LISTITEM || kind == UIA_TREEITEM {
            let mut name = null_mut();
            slot::<unsafe extern "system" fn(Raw, *mut *mut u16) -> HRESULT>(el, 23)(el, &mut name);
            found = Some((take_bstr(name), kind));
            break;
        }
        let mut parent = null_mut();
        if !walker.is_null() {
            slot::<unsafe extern "system" fn(Raw, Raw, *mut Raw) -> HRESULT>(walker, 3)(
                walker,
                el,
                &mut parent,
            );
        }
        release(el);
        el = parent;
    }
    release(el);
    release(walker);
    release(uia);
    found
}

#[repr(C)]
struct Variant {
    vt: u16,
    reserved: [u16; 3],
    value: i64,
    extra: i64,
}

/// The folder shown by Explorer window `root` (its active tab on Windows 11).
unsafe fn explorer_folder(root: HWND) -> Option<PathBuf> {
    let mut windows = null_mut();
    if CoCreateInstance(
        &CLSID_SHELLWINDOWS,
        null_mut(),
        CLSCTX_ALL,
        &IID_ISHELLWINDOWS,
        &mut windows,
    ) != S_OK
    {
        return None;
    }
    let active_tab = FindWindowExW(
        root,
        null_mut(),
        wide("ShellTabWindowClass").as_ptr(),
        null(),
    );
    let mut count = 0i32;
    slot::<unsafe extern "system" fn(Raw, *mut i32) -> HRESULT>(windows, 7)(windows, &mut count);
    let mut result = None;
    for i in 0..count {
        let index = Variant {
            vt: 3, // VT_I4
            reserved: [0; 3],
            value: i as i64,
            extra: 0,
        };
        let mut disp = null_mut();
        slot::<unsafe extern "system" fn(Raw, Variant, *mut Raw) -> HRESULT>(windows, 8)(
            windows, index, &mut disp,
        );
        if disp.is_null() {
            continue;
        }
        let app = query(disp, &IID_IWEBBROWSERAPP);
        release(disp);
        if app.is_null() {
            continue;
        }
        let mut hwnd: isize = 0;
        slot::<unsafe extern "system" fn(Raw, *mut isize) -> HRESULT>(app, 37)(app, &mut hwnd);
        let mut matches = hwnd as HWND == root;
        if matches && !active_tab.is_null() {
            matches = tab_window(app).map_or(true, |t| t == active_tab);
        }
        if matches {
            let mut url = null_mut();
            slot::<unsafe extern "system" fn(Raw, *mut *mut u16) -> HRESULT>(app, 30)(
                app, &mut url,
            );
            result = file_url_to_path(&take_bstr(url));
        }
        release(app);
        if result.is_some() {
            break;
        }
    }
    release(windows);
    result
}

unsafe fn tab_window(app: Raw) -> Option<HWND> {
    let sp = query(app, &IID_ISERVICEPROVIDER);
    if sp.is_null() {
        return None;
    }
    let mut browser = null_mut();
    slot::<unsafe extern "system" fn(Raw, *const GUID, *const GUID, *mut Raw) -> HRESULT>(sp, 3)(
        sp,
        &SID_STOPLEVELBROWSER,
        &IID_ISHELLBROWSER,
        &mut browser,
    );
    release(sp);
    if browser.is_null() {
        return None;
    }
    let mut hwnd = null_mut();
    slot::<unsafe extern "system" fn(Raw, *mut HWND) -> HRESULT>(browser, 3)(browser, &mut hwnd);
    release(browser);
    (!hwnd.is_null()).then_some(hwnd)
}

/// `file:///C:/My%20Docs` → `C:\My Docs` (UNC `file://server/share` too).
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url
        .strip_prefix("file:///")
        .map(str::to_owned)
        .or_else(|| url.strip_prefix("file://").map(|r| format!("//{r}")))?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&out).replace('/', "\\");
    let p = PathBuf::from(s);
    p.is_dir().then_some(p)
}

fn desktop_folders() -> Vec<PathBuf> {
    [FOLDERID_Desktop, FOLDERID_PublicDesktop]
        .iter()
        .filter_map(|id| unsafe { known_folder(id) })
        .collect()
}

pub unsafe fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut p = null_mut();
    if SHGetKnownFolderPath(id, 0, null_mut(), &mut p) != S_OK {
        return None;
    }
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
    CoTaskMemFree(p as *const c_void);
    Some(PathBuf::from(s))
}

/// Navigation-pane items: well-known folders and drives ("Local Disk (C:)").
fn known_folder_by_name(name: &str) -> Option<PathBuf> {
    if let Some(i) = name.rfind(":)") {
        let letter = name[..i].chars().last()?;
        return Some(PathBuf::from(format!("{letter}:\\")));
    }
    let ids = [
        FOLDERID_Desktop,
        FOLDERID_Downloads,
        FOLDERID_Documents,
        FOLDERID_Pictures,
        FOLDERID_Music,
        FOLDERID_Videos,
    ];
    ids.iter()
        .filter_map(|id| unsafe { known_folder(id) })
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
        })
}

/// Find `name` in `dir`, also matching without the extension (Explorer hides
/// known extensions by default). Folders win over files.
fn find_child(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.exists() {
        return Some(exact);
    }
    let mut file_match = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string());
        if stem.as_deref() == Some(name) {
            if p.is_dir() {
                return Some(p);
            }
            file_match.get_or_insert(p);
        }
    }
    file_match
}

// --- file icons ---------------------------------------------------------------

/// A 48 px shell icon for `path` (falls back to the 32 px one). Caller owns it.
/// `generic` asks for the icon of the file *type* only (no disk access per
/// file), which is what long lists use.
pub unsafe fn file_icon(path: &Path, generic: bool) -> HICON {
    let w = wide(&path.to_string_lossy());
    let (attrs, extra) = if generic {
        let attrs = if path.is_dir() {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        (attrs, SHGFI_USEFILEATTRIBUTES)
    } else {
        (0, 0)
    };
    let mut info: SHFILEINFOW = std::mem::zeroed();
    let ok = SHGetFileInfoW(
        w.as_ptr(),
        attrs,
        &mut info,
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_SYSICONINDEX | extra,
    );
    if ok != 0 {
        let mut list = null_mut();
        if SHGetImageList(SHIL_EXTRALARGE as i32, &IID_IIMAGELIST, &mut list) == S_OK
            && !list.is_null()
        {
            let mut icon = null_mut();
            slot::<unsafe extern "system" fn(Raw, i32, u32, *mut HICON) -> HRESULT>(list, 10)(
                list,
                info.iIcon,
                ILD_TRANSPARENT,
                &mut icon,
            );
            release(list);
            if !icon.is_null() {
                return icon;
            }
        }
    }
    let mut info: SHFILEINFOW = std::mem::zeroed();
    SHGetFileInfoW(
        w.as_ptr(),
        attrs,
        &mut info,
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_LARGEICON | extra,
    );
    info.hIcon
}

// --- clipboard ------------------------------------------------------------------

pub enum Clip {
    Text(String),
    Files(Vec<PathBuf>),
}

/// Read text or a file list from the clipboard. `None` when it holds neither
/// or another app has it open (the caller retries shortly).
pub unsafe fn read_clipboard(hwnd: HWND) -> Option<Clip> {
    if OpenClipboard(hwnd) == 0 {
        return None;
    }
    let mut out = None;
    if IsClipboardFormatAvailable(CF_HDROP as u32) != 0 {
        let h = GetClipboardData(CF_HDROP as u32);
        if !h.is_null() {
            let files = hdrop_paths(h as HDROP);
            if !files.is_empty() {
                out = Some(Clip::Files(files));
            }
        }
    }
    if out.is_none() && IsClipboardFormatAvailable(CF_UNICODETEXT as u32) != 0 {
        let h = GetClipboardData(CF_UNICODETEXT as u32);
        let p = GlobalLock(h) as *const u16;
        if !p.is_null() {
            let max = GlobalSize(h) / 2;
            let mut len = 0;
            while len < max && *p.add(len) != 0 {
                len += 1;
            }
            out = Some(Clip::Text(String::from_utf16_lossy(
                std::slice::from_raw_parts(p, len),
            )));
            GlobalUnlock(h);
        }
    }
    CloseClipboard();
    out
}

/// Put text on the clipboard. Returns the new clipboard sequence number so the
/// host can ignore its own change in `WM_CLIPBOARDUPDATE`.
pub unsafe fn write_clipboard_text(hwnd: HWND, text: &str) -> u32 {
    if OpenClipboard(hwnd) == 0 {
        return 0;
    }
    EmptyClipboard();
    let w = wide(text);
    let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
    if !h.is_null() {
        let p = GlobalLock(h) as *mut u16;
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        GlobalUnlock(h);
        SetClipboardData(CF_UNICODETEXT as u32, h);
    }
    CloseClipboard();
    GetClipboardSequenceNumber()
}

/// Put a file list on the clipboard (as Explorer's copy does).
pub unsafe fn write_clipboard_files(hwnd: HWND, files: &[PathBuf]) -> u32 {
    if OpenClipboard(hwnd) == 0 {
        return 0;
    }
    EmptyClipboard();
    // DROPFILES header followed by a double-NUL-terminated wide path list.
    let mut list: Vec<u16> = Vec::new();
    for f in files {
        list.extend(f.to_string_lossy().encode_utf16());
        list.push(0);
    }
    list.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let h = GlobalAlloc(GMEM_MOVEABLE, header + list.len() * 2);
    if !h.is_null() {
        let p = GlobalLock(h) as *mut u8;
        let df = DROPFILES {
            pFiles: header as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: 0,
            fWide: 1,
        };
        std::ptr::copy_nonoverlapping(&df as *const _ as *const u8, p, header);
        std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, p.add(header), list.len() * 2);
        GlobalUnlock(h);
        SetClipboardData(CF_HDROP as u32, h);
    }
    CloseClipboard();
    GetClipboardSequenceNumber()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_urls() {
        let tmp = std::env::temp_dir();
        let url = format!(
            "file:///{}",
            tmp.to_string_lossy().replace('\\', "/").replace(' ', "%20")
        );
        assert_eq!(file_url_to_path(&url), Some(tmp));
        assert_eq!(file_url_to_path("shell:::{20D04FE0}"), None);
    }
}
