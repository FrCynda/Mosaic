//! Windows shell helpers: Explorer thumbnails, the file clipboard, open / reveal / properties.
use std::ffi::c_void;
use std::iter::once;
use std::mem::{size_of, zeroed};
use std::os::windows::process::CommandExt;
use std::process::Command;

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF};

use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, SW_SHOW};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

thread_local!(static HELPER: std::cell::RefCell<Option<std::process::Child>> = const { std::cell::RefCell::new(None) });

/// PNG of what Explorer would show for `path` at about `size` px (image/video/PDF preview, else the icon).
/// `icon_only` skips previews, which is cheaper and right for exe/lnk. Call from a [`shell_spawn`] job.
///
/// Runs in a helper process (this exe with `--shell-helper`), one per worker thread: binding a real path makes
/// the shell load third-party folder extensions into the caller, and ExplorerPatcher's (registered under
/// Drive\ShellEx\FolderExtensions) crashes in our process. A crash now costs one thumbnail; the next call
/// starts a fresh helper.
/// ponytail: no per-request timeout; a hung thumbnailer stalls its worker (1 of 4). Kill+respawn on timeout if seen.
pub fn thumbnail(path: &str, size: i32, icon_only: bool) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    HELPER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let exe = crate::native::thumbs_exe().or_else(|_| std::env::current_exe()).ok()?;
            *slot = Command::new(exe)
                .arg("--shell-helper")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .spawn()
                .ok();
        }
        let child = slot.as_mut()?;
        let ask = |c: &mut std::process::Child| -> std::io::Result<Vec<u8>> {
            let (stdin, stdout) = (c.stdin.as_mut().unwrap(), c.stdout.as_mut().unwrap());
            writeln!(stdin, "{size}\t{}\t{path}", icon_only as u8)?;
            stdin.flush()?;
            let mut len = [0u8; 4];
            stdout.read_exact(&mut len)?;
            let mut png = vec![0u8; u32::from_le_bytes(len) as usize];
            stdout.read_exact(&mut png)?;
            Ok(png)
        };
        match ask(child) {
            Ok(png) => (!png.is_empty()).then_some(png),
            Err(_) => {
                let _ = child.kill(); // died (or wedged) on this file: drop it, the next request starts a new one
                *slot = None;
                None
            }
        }
    })
}

/// `--shell-helper`: answer "size\ticon_only\tpath" lines on stdin with length-prefixed PNGs (0 = none) on stdout.
pub fn helper_main() {
    use std::io::{BufRead, Write};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let mut f = line.splitn(3, '\t');
        let (size, icon_only, path) = (f.next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(128), f.next() == Some("1"), f.next().unwrap_or(""));
        let png = thumbnail_here(path, size, icon_only).unwrap_or_default();
        if out.write_all(&(png.len() as u32).to_le_bytes()).and_then(|_| out.write_all(&png)).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn thumbnail_here(path: &str, size: i32, icon_only: bool) -> Option<Vec<u8>> {
    unsafe {
        let path = path.strip_prefix("shell:").filter(|p| p.starts_with("::")).unwrap_or(path); // "shell:::{CLSID}" is parsed as "::{CLSID}"
        let w = wide(path);
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let flags = SIIGBF(if icon_only { 0x1 | 0x4 } else { 0x1 }); // BIGGERSIZEOK [| ICONONLY]
        let hbm = f.GetImage(SIZE { cx: size, cy: size }, flags).ok()?;
        let png = bitmap_png(hbm);
        let _ = DeleteObject(hbm);
        png
    }
}

/// A shell job; `true` means it was dropped unrun (answer it cheaply).
pub type Job = Box<dyn FnOnce(bool) + Send>;
static JOBS: (std::sync::Mutex<Vec<Job>>, std::sync::Condvar) = (std::sync::Mutex::new(Vec::new()), std::sync::Condvar::new());
const MAX_JOBS: usize = 512; // a few screens of grid tiles

/// Queue icon/thumbnail work. Newest first: while scrolling, the items on screen *now* are the ones asked last,
/// so they jump ahead of items already scrolled past, and the oldest backlog is dropped.
/// Four threads each enter COM once and never leave: tearing COM down after every call (on many threads at
/// once) crashed inside shell hooks such as ExplorerPatcher's while scrolling fast.
pub fn shell_spawn(job: Job) {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        for _ in 0..4 {
            std::thread::spawn(|| {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                }
                loop {
                    let job = {
                        let mut q = JOBS.0.lock().unwrap();
                        while q.is_empty() {
                            let (g, t) = JOBS.1.wait_timeout(q, std::time::Duration::from_secs(30)).unwrap();
                            q = g;
                            if t.timed_out() && q.is_empty() {
                                // idle: end this thread's helper process (tens of MB); the next thumbnail starts a new one
                                let _ = HELPER.with(|c| c.borrow_mut().take().map(|mut h| h.kill()));
                            }
                        }
                        q.pop().unwrap()
                    };
                    job(false);
                }
            });
        }
    });
    let dropped = {
        let mut q = JOBS.0.lock().unwrap();
        q.push(job);
        (q.len() > MAX_JOBS).then(|| q.remove(0))
    };
    JOBS.1.notify_one();
    if let Some(old) = dropped {
        old(true);
    }
}

unsafe fn bitmap_png(hbm: HBITMAP) -> Option<Vec<u8>> {
    let (w, h, mut px) = bitmap_bgra(hbm)?;
    for p in px.chunks_exact_mut(4) {
        let a = p[3];
        // shell bitmaps are premultiplied BGRA; PNG wants straight RGBA
        let un = |c: u8| if a == 0 || a == 255 { c } else { ((c as u32 * 255 / a as u32).min(255)) as u8 };
        let (b, g, r) = (un(p[0]), un(p[1]), un(p[2]));
        p[0] = r;
        p[1] = g;
        p[2] = b;
    }
    crate::infrastructure::icons::encode_png(&px, w as u32, h as u32)
}

/// A bitmap's pixels as premultiplied BGRA (opaque when it has no alpha at all), with its width and height.
unsafe fn bitmap_bgra(hbm: HBITMAP) -> Option<(usize, usize, Vec<u8>)> {
    let mut bm: BITMAP = zeroed();
    if GetObjectW(hbm, size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth as usize, bm.bmHeight as usize);
    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader.biSize = size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w as i32;
    bmi.bmiHeader.biHeight = -(h as i32); // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    let mut px = vec![0u8; w * h * 4];
    let hdc = GetDC(None);
    let got = GetDIBits(hdc, hbm, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
    ReleaseDC(None, hdc);
    if got == 0 {
        return None;
    }
    if !px.chunks_exact(4).any(|p| p[3] != 0) {
        px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    Some((w, h, px))
}

/// Put real files on the clipboard: Ctrl+V in Explorer copies them, or moves them for a cut.
pub fn clipboard_files(paths: &[String], cut: bool) -> Result<(), String> {
    // DROPFILES header (offset 20, POINT, fNC, fWide=1) then "a\0b\0\0" as UTF-16
    let mut drop: Vec<u8> = Vec::new();
    for v in [20u32, 0, 0, 0, 1] {
        drop.extend(v.to_le_bytes());
    }
    for p in paths {
        for c in p.encode_utf16().chain(once(0)) {
            drop.extend(c.to_le_bytes());
        }
    }
    drop.extend([0, 0]);
    let effect = (if cut { 2u32 } else { 1u32 }).to_le_bytes().to_vec(); // DROPEFFECT_MOVE / COPY
    unsafe {
        let fmt = RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr());
        // a clipboard opened without an owner window can't keep its data after EmptyClipboard
        let hwnd = CreateWindowExW(0, wide("STATIC").as_ptr(), std::ptr::null(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(hwnd) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let res = if opened {
            EmptyClipboard();
            let (h1, h2) = (global(&drop), global(&effect));
            let r = if SetClipboardData(15, h1 as _).is_null() {
                GlobalFree(h1);
                Err("SetClipboardData failed".to_string())
            } else {
                if SetClipboardData(fmt, h2 as _).is_null() {
                    GlobalFree(h2);
                }
                Ok(())
            };
            CloseClipboard();
            r
        } else {
            Err("clipboard busy".to_string())
        };
        DestroyWindow(hwnd);
        res
    }
}

/// The files on the clipboard and whether they were cut (else copied), as a paste will take them.
pub fn clipboard_get() -> Option<(Vec<String>, bool)> {
    use windows_sys::Win32::System::DataExchange::GetClipboardData;
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let drop = GetClipboardData(15) as HDROP; // CF_HDROP
        let files = (!drop.is_null()).then(|| {
            (0..DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0)).map(|i| {
                let n = DragQueryFileW(drop, i, std::ptr::null_mut(), 0);
                let mut b = vec![0u16; n as usize + 1];
                DragQueryFileW(drop, i, b.as_mut_ptr(), n + 1);
                String::from_utf16_lossy(&b[..n as usize])
            }).collect::<Vec<_>>()
        });
        let effect = GetClipboardData(RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr()));
        let cut = !effect.is_null() && {
            let p = GlobalLock(effect) as *const u32;
            let v = if p.is_null() { 0 } else { p.read_unaligned() };
            GlobalUnlock(effect);
            v & 2 != 0 // DROPEFFECT_MOVE
        };
        CloseClipboard();
        Some((files?, cut))
    }
}

pub unsafe fn global(data: &[u8]) -> *mut c_void {
    let h = GlobalAlloc(GMEM_MOVEABLE, data.len());
    std::ptr::copy_nonoverlapping(data.as_ptr(), GlobalLock(h) as *mut u8, data.len());
    GlobalUnlock(h);
    h
}

pub fn open(path: &str) {
    let _ = Command::new("cmd").args(["/c", "start", "", path]).creation_flags(0x0800_0000).spawn();
}

/// Where a .lnk points (None for a broken link or one into a shell namespace with no path).
pub fn link_target(lnk: &str) -> Option<String> {
    use windows::core::Interface;
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>().ok()?.Load(PCWSTR(wide(lnk).as_ptr()), STGM_READ).ok()?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut::<WIN32_FIND_DATAW>(), 0).ok()?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
    }
}

/// Where a .url file points when that is a local path (`URL=file:///C:/dir`); web addresses are None.
pub fn url_target(url: &str) -> Option<String> {
    let text = std::fs::read_to_string(url).ok()?;
    let v = text.lines().find_map(|l| l.trim().strip_prefix("URL="))?;
    let p = v.strip_prefix("file:///")?.replace('/', "\\");
    Some(p.replace("%20", " "))
}

/// A shell location with no path (This PC, Network, …) by its `shell:::{CLSID}` name.
pub fn open_special(target: &str) {
    let _ = Command::new("explorer").arg(target).spawn();
}

/// A console in `dir`: PowerShell or cmd, optionally elevated (the UAC prompt; the shell is told the folder too,
/// since an elevated process doesn't always inherit the starting directory).
pub fn terminal(powershell: bool, admin: bool, dir: &str) {
    let exe = if powershell { "powershell.exe" } else { "cmd.exe" };
    if !admin {
        let _ = Command::new(exe).current_dir(dir).creation_flags(0x0000_0010).spawn(); // CREATE_NEW_CONSOLE
        return;
    }
    let args = if powershell { format!("-NoExit -Command \"Set-Location -LiteralPath '{}'\"", dir.replace('\'', "''")) } else { format!("/K cd /d \"{dir}\"") };
    std::thread::spawn({
        let dir = dir.to_string();
        move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let (verb, file, params, d) = (wide("runas"), wide(exe), wide(&args), wide(&dir));
            let mut sei: SHELLEXECUTEINFOW = zeroed();
            sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            sei.fMask = SEE_MASK_NOASYNC;
            sei.lpVerb = verb.as_ptr();
            sei.lpFile = file.as_ptr();
            sei.lpParameters = params.as_ptr();
            sei.lpDirectory = d.as_ptr();
            sei.nShow = SW_SHOW;
            ShellExecuteExW(&mut sei);
        }
    });
}

/// One entry of Explorer's right-click menu, as the window draws it. A separator has no label; a submenu has
/// `sub` (and id 0). `px` is a `w`×`h` icon, premultiplied BGRA.
#[derive(serde::Serialize, serde::Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct MenuItem {
    pub id: u32,
    pub label: String,
    pub keys: String,
    pub verb: String,
    pub off: bool,
    pub on: bool,
    pub bold: bool,
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
    pub sub: Vec<MenuItem>,
}

/// The `--shell-menu` process, kept running: the shell's menu loads every shell extension on the machine, so a
/// process per right-click took seconds (and showed the busy cursor while it started). One that crashes
/// (ExplorerPatcher's does, see `thumbnail`) or hangs must not take the window with it, so they load there.
struct MenuHelper {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<String>,
}
static MENU_HELPER: std::sync::Mutex<Option<MenuHelper>> = std::sync::Mutex::new(None);

fn menu_helper(g: &mut Option<MenuHelper>) -> Option<&mut MenuHelper> {
    use std::io::BufRead;
    if g.as_mut().is_some_and(|h| !matches!(h.child.try_wait(), Ok(None))) {
        *g = None; // ended (idle for a while, or crashed): start another
    }
    if g.is_none() {
        let exe = std::env::current_exe().ok()?;
        let mut child = Command::new(exe).arg("--shell-menu").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
        let (stdin, out) = (child.stdin.take()?, child.stdout.take()?);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                if line.ok().and_then(|l| tx.send(l).ok()).is_none() {
                    break;
                }
            }
        });
        *g = Some(MenuHelper { child, stdin, rx });
    }
    g.as_mut()
}

/// Start the menu process ahead of the first right-click, so that one is quick too.
pub fn menu_warm() {
    if let Ok(mut g) = MENU_HELPER.lock() {
        menu_helper(&mut g);
    }
}

/// Send one request; with `reply`, wait for its answer line. A helper that doesn't answer in time is ended.
fn menu_ask(req: serde_json::Value, reply: bool) -> Option<String> {
    use std::io::Write;
    use std::sync::mpsc::RecvTimeoutError;
    let mut g = MENU_HELPER.lock().ok()?;
    for _ in 0..2 {
        let h = menu_helper(&mut g)?;
        while h.rx.try_recv().is_ok() {} // a late answer to a request that timed out
        if writeln!(h.stdin, "{req}").and_then(|_| h.stdin.flush()).is_err() {
            *g = None; // it ended just now: once more with a new one
            continue;
        }
        // what it opens (Properties, Open with…, a copy's questions) must be able to come to the front
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(h.child.id()) };
        if !reply {
            return Some(String::new());
        }
        match h.rx.recv_timeout(std::time::Duration::from_secs(4)) {
            Ok(l) => return Some(l),
            Err(RecvTimeoutError::Disconnected) => *g = None,
            Err(RecvTimeoutError::Timeout) => {
                let _ = h.child.kill();
                *g = None;
                return None;
            }
        }
    }
    None
}

/// Explorer's menu for `paths`, or with `bg` for the empty space of the folder `paths[0]`. Shift held shows the
/// extended verbs, as in Explorer. Then [`menu_pick`] runs what was picked, or lets it go.
pub fn menu_items(paths: &[String], bg: bool) -> Option<Vec<MenuItem>> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
    let ext = unsafe { GetAsyncKeyState(VK_SHIFT as i32) } < 0;
    serde_json::from_str(&menu_ask(serde_json::json!({ "cmd": "menu", "paths": paths, "bg": bg, "ext": ext }), true)?).ok()
}

/// Run the item `id` of the last [`menu_items`] (at screen point x, y), or with None just let that menu go.
pub fn menu_pick(id: Option<u32>, x: i32, y: i32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
    let (shift, ctrl) = unsafe { (GetKeyState(VK_SHIFT as i32) < 0, GetKeyState(VK_CONTROL as i32) < 0) };
    menu_ask(serde_json::json!({ "cmd": "pick", "id": id, "x": x, "y": y, "shift": shift, "ctrl": ctrl }), false);
}

/// A verb of the folder `dir`'s empty space run without its menu ("paste").
pub fn menu_verb(dir: &str, verb: &str, x: i32, y: i32) {
    menu_ask(serde_json::json!({ "cmd": "verb", "paths": [dir], "verb": verb, "x": x, "y": y }), false);
}

/// A verb of item `path`'s own menu run without the menu ("pintohome": pin to Quick access).
pub fn item_verb(path: &str, verb: &str) {
    menu_ask(serde_json::json!({ "cmd": "verb", "paths": [path], "verb": verb, "bg": false, "x": 0, "y": 0 }), false);
}

/// Windows Terminal in `dir` (wt.exe, the app execution alias).
pub fn wt(dir: &str) {
    let _ = Command::new("wt.exe").args(["-d", dir]).spawn();
}

/// A shortcut `lnk` to `target`.
pub fn make_link(target: &str, lnk: &str) -> bool {
    use windows::core::Interface;
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else { return false };
        if link.SetPath(PCWSTR(wide(target).as_ptr())).is_err() {
            return false;
        }
        link.cast::<IPersistFile>().is_ok_and(|f| f.Save(PCWSTR(wide(lnk).as_ptr()), true).is_ok())
    }
}

/// The process reference shell extensions hold while they work on after the menu closes (a copy's progress, a
/// Properties dialog): `SHSetInstanceExplorer`'s object, a bare IUnknown that only counts.
static REFS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
static BASE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1); // the shell's own, held from the start
#[repr(C)]
struct RefVt {
    qi: unsafe extern "system" fn(*mut c_void, *const windows::core::GUID, *mut *mut c_void) -> windows::core::HRESULT,
    add: unsafe extern "system" fn(*mut c_void) -> u32,
    rel: unsafe extern "system" fn(*mut c_void) -> u32,
}
#[allow(dead_code)] // read by COM, through the object's pointer
struct RefObj(*const RefVt);
unsafe impl Sync for RefObj {}
unsafe extern "system" fn ref_qi(this: *mut c_void, iid: *const windows::core::GUID, out: *mut *mut c_void) -> windows::core::HRESULT {
    use windows::core::Interface;
    if *iid == windows::core::IUnknown::IID {
        *out = this;
        ref_add(this);
        windows::core::HRESULT(0)
    } else {
        *out = std::ptr::null_mut();
        windows::core::HRESULT(0x8000_4002u32 as i32) // E_NOINTERFACE
    }
}
unsafe extern "system" fn ref_add(_: *mut c_void) -> u32 {
    REFS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}
unsafe extern "system" fn ref_rel(_: *mut c_void) -> u32 {
    REFS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1
}
static REF_VT: RefVt = RefVt { qi: ref_qi, add: ref_add, rel: ref_rel };
static REF_OBJ: RefObj = RefObj(&REF_VT);

/// Requests read from stdin, for the window thread; `None` once stdin closed (the window is gone).
static INBOX: std::sync::Mutex<Option<std::collections::VecDeque<String>>> = std::sync::Mutex::new(None);
const WM_INBOX: u32 = 0x8000 + 1; // WM_APP + 1
thread_local! {
    /// The menu last asked for, until something on it is picked.
    static HELD: std::cell::RefCell<Option<(windows::Win32::UI::Shell::IContextMenu, isize)>> = const { std::cell::RefCell::new(None) };
    static LAST: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// What a handler that draws its own items (icons) or fills a submenu when it opens (Send to, Open with) does
/// with a menu message.
unsafe fn menu_msg(cm: &windows::Win32::UI::Shell::IContextMenu, m: u32, wp: usize, l: isize) -> Option<isize> {
    use windows::core::Interface;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{IContextMenu2, IContextMenu3};
    if let Ok(c3) = cm.cast::<IContextMenu3>() {
        let mut r = LRESULT(0);
        return c3.HandleMenuMsg2(m, WPARAM(wp), LPARAM(l), Some(&mut r)).is_ok().then_some(r.0);
    }
    cm.cast::<IContextMenu2>().ok()?.HandleMenuMsg(m, WPARAM(wp), LPARAM(l)).is_ok().then_some(0)
}

unsafe extern "system" fn host_proc(h: windows_sys::Win32::Foundation::HWND, m: u32, wp: usize, l: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, KillTimer, PostQuitMessage, WM_TIMER};
    match m {
        WM_INBOX => {
            while let Some(line) = INBOX.lock().ok().and_then(|mut q| q.as_mut()?.pop_front()) {
                LAST.set(Some(std::time::Instant::now()));
                serve(h, &line); // may show a dialog, whose loop brings the next request here before it returns
            }
            0
        }
        WM_TIMER if wp == 1 => {
            KillTimer(h, 1);
            prewarm(h);
            0
        }
        WM_TIMER => {
            // done once the window is gone, or nothing was asked for 10 minutes; not while something picked
            // still works (a copy) or shows a window (Properties)
            let gone = INBOX.lock().map_or(true, |q| q.is_none());
            let quiet = LAST.get().is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(600));
            if (gone || quiet) && REFS.load(std::sync::atomic::Ordering::SeqCst) <= BASE.load(std::sync::atomic::Ordering::SeqCst) && !own_window_shown() {
                PostQuitMessage(0);
            }
            0
        }
        _ => DefWindowProcW(h, m, wp, l),
    }
}

/// `--shell-menu`: answer [`menu_items`], [`menu_pick`] and [`menu_verb`] requests, a JSON line each on stdin.
pub fn menu_main() {
    use windows::core::Interface;
    use windows_sys::Win32::System::Ole::{OleFlushClipboard, OleInitialize};
    use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        OleInitialize(std::ptr::null_mut()); // COM, plus the OLE clipboard that Copy / Cut / Paste use
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let class = wide("MosaicShellMenu");
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(host_proc);
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
        // the owner of what verbs show, hidden
        let host = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), std::ptr::null(), WS_POPUP, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
        let unk = std::mem::ManuallyDrop::new(windows::core::IUnknown::from_raw(&REF_OBJ as *const RefObj as *mut c_void));
        windows::Win32::UI::Shell::SHSetInstanceExplorer(&*unk);
        BASE.store(REFS.load(std::sync::atomic::Ordering::SeqCst), std::sync::atomic::Ordering::SeqCst);
        LAST.set(Some(std::time::Instant::now()));
        *INBOX.lock().unwrap() = Some(Default::default());
        let to = host as isize;
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                INBOX.lock().unwrap().get_or_insert_with(Default::default).push_back(line);
                PostMessageW(to as _, WM_INBOX, 0, 0);
            }
            *INBOX.lock().unwrap() = None;
        });
        // waiting for messages right away ends the busy cursor Windows shows while a program starts
        SetTimer(host, 1, 50, None);
        SetTimer(host, 2, 5000, None);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        OleFlushClipboard(); // what Copy / Cut put on the clipboard outlives this process
    }
}

/// Load the extensions the first menus need before they're asked for: a folder's, and a file's.
unsafe fn prewarm(host: windows_sys::Win32::Foundation::HWND) {
    let Ok(me) = std::env::current_exe() else { return };
    let dir = me.parent().map_or(String::new(), |d| d.to_string_lossy().into_owned());
    for (p, bg) in [(dir.as_str(), true), (&*me.to_string_lossy(), false)] {
        let _ = build_menu(&[p], bg, false, host);
    }
}

/// Explorer's menu for `paths` (see [`context_menu`]): the handler, its HMENU, and what's on it.
unsafe fn build_menu(paths: &[&str], bg: bool, ext: bool, host: windows_sys::Win32::Foundation::HWND) -> Option<(windows::Win32::UI::Shell::IContextMenu, isize, Vec<MenuItem>)> {
    use windows::Win32::UI::WindowsAndMessaging::HMENU;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu, WM_INITMENUPOPUP};
    let cm: windows::Win32::UI::Shell::IContextMenu = context_menu(paths, bg, windows::Win32::Foundation::HWND(host as _))?;
    let menu = CreatePopupMenu();
    let flags = if bg { 0 } else { 0x10 } | if ext { 0x100 } else { 0 }; // CMF_CANRENAME, CMF_EXTENDEDVERBS
    if cm.QueryContextMenu(HMENU(menu as _), 0, 1, 0x7fff, flags).is_err() {
        DestroyMenu(menu);
        return None;
    }
    menu_msg(&cm, WM_INITMENUPOPUP, menu as usize, 0);
    let items = read_menu(&cm, menu, 0);
    Some((cm, menu as isize, items))
}

unsafe fn serve(host: windows_sys::Win32::Foundation::HWND, line: &str) {
    use std::io::Write;
    use windows_sys::Win32::System::Ole::OleFlushClipboard;
    use windows_sys::Win32::UI::WindowsAndMessaging::DestroyMenu;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim_start_matches('\u{feff}')) else { return };
    let paths: Vec<&str> = v["paths"].as_array().map_or(Vec::new(), |a| a.iter().filter_map(|p| p.as_str()).collect());
    let (x, y) = (v["x"].as_i64().unwrap_or(0) as i32, v["y"].as_i64().unwrap_or(0) as i32);
    windows_sys::Win32::UI::WindowsAndMessaging::KillTimer(host, 1); // asked already: no need to prewarm
    let held = HELD.take(); // a new request lets go of the last menu, after running its pick
    match v["cmd"].as_str() {
        Some("menu") => {
            let mut items = Vec::new();
            if let Some((cm, menu, it)) = build_menu(&paths, v["bg"] == true, v["ext"] == true, host) {
                items = it;
                HELD.set(Some((cm, menu)));
            }
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{}", serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())).and_then(|_| out.flush());
        }
        Some("pick") => {
            if let (Some((cm, _)), Some(id)) = (&held, v["id"].as_u64().filter(|&id| id >= 1)) {
                invoke(cm, host, Ok(id as usize - 1), x, y, v["shift"] == true, v["ctrl"] == true);
                OleFlushClipboard();
            }
        }
        Some("verb") => {
            if let Some((cm, menu, _)) = build_menu(&paths, v["bg"] != false, false, host) {
                invoke(&cm, host, Err(v["verb"].as_str().unwrap_or("")), x, y, false, false);
                DestroyMenu(menu as _);
            }
            OleFlushClipboard();
        }
        _ => {}
    }
    if let Some((_, menu)) = held {
        DestroyMenu(menu as _);
    }
}

unsafe fn invoke(cm: &windows::Win32::UI::Shell::IContextMenu, host: windows_sys::Win32::Foundation::HWND, verb: Result<usize, &str>, x: i32, y: i32, shift: bool, ctrl: bool) {
    use windows::core::PCSTR;
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::UI::Shell::{CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX};
    let mut ci: CMINVOKECOMMANDINFOEX = zeroed();
    ci.cbSize = size_of::<CMINVOKECOMMANDINFOEX>() as u32;
    ci.fMask = 0x4000 | 0x2000_0000 | if shift { 0x1000_0000 } else { 0 } | if ctrl { 0x4000_0000 } else { 0 }; // CMIC_MASK_UNICODE | _PTINVOKE | _SHIFT_DOWN | _CONTROL_DOWN
    ci.hwnd = HWND(host as _);
    ci.nShow = windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    ci.ptInvoke = POINT { x, y };
    let (a, w) = match verb {
        Ok(_) => (Vec::new(), Vec::new()),
        Err(v) => (format!("{v}\0").into_bytes(), wide(v)),
    };
    (ci.lpVerb, ci.lpVerbW) = match verb {
        Ok(id) => (PCSTR(id as *const u8), PCWSTR(id as *const u16)), // MAKEINTRESOURCE(id)
        Err(_) => (PCSTR(a.as_ptr()), PCWSTR(w.as_ptr())),
    };
    let _ = cm.InvokeCommand(&ci as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO);
}

/// "Open &with\tCtrl+O" → ("Open with", "Ctrl+O"): Explorer's menu text without its access-key marks.
fn menu_label(s: &str) -> (String, String) {
    let (label, keys) = s.split_once('\t').unwrap_or((s, ""));
    let mut out = String::new();
    let mut cs = label.chars();
    while let Some(c) = cs.next() {
        match c {
            '&' => out.extend(cs.next()), // "&&" is a literal "&"
            c => out.push(c),
        }
    }
    (out.trim().to_string(), keys.trim().to_string())
}

/// What's on `menu`, submenus filled in (their handlers fill them when told they open).
unsafe fn read_menu(cm: &windows::Win32::UI::Shell::IContextMenu, menu: windows_sys::Win32::UI::WindowsAndMessaging::HMENU, depth: u32) -> Vec<MenuItem> {
    use windows::core::PSTR;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    let mut out = Vec::new();
    for i in 0..GetMenuItemCount(menu).max(0) as u32 {
        let mut mi: MENUITEMINFOW = zeroed();
        mi.cbSize = size_of::<MENUITEMINFOW>() as u32;
        mi.fMask = MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_BITMAP | MIIM_DATA;
        if GetMenuItemInfoW(menu, i, 1, &mut mi) == 0 {
            continue;
        }
        if mi.fType & MFT_SEPARATOR != 0 {
            out.push(MenuItem::default());
            continue;
        }
        // ponytail: an item that draws its own text too (rare, old extensions) is left out
        let text = if mi.fType & MFT_OWNERDRAW != 0 {
            String::new()
        } else {
            let n = GetMenuStringW(menu, i, std::ptr::null_mut(), 0, MF_BYPOSITION).max(0);
            let mut b = vec![0u16; n as usize + 1];
            GetMenuStringW(menu, i, b.as_mut_ptr(), n + 1, MF_BYPOSITION);
            String::from_utf16_lossy(&b[..n as usize])
        };
        let (label, keys) = menu_label(&text);
        if label.is_empty() {
            continue;
        }
        let mut it = MenuItem { id: mi.wID, label, keys, off: mi.fState & MFS_DISABLED != 0, on: mi.fState & MFS_CHECKED != 0, bold: mi.fState & MFS_DEFAULT != 0, ..Default::default() };
        if !mi.hSubMenu.is_null() {
            it.id = 0;
            if depth < 4 {
                menu_msg(cm, WM_INITMENUPOPUP, mi.hSubMenu as usize, i as isize);
                it.sub = read_menu(cm, mi.hSubMenu, depth + 1);
            }
            it.off |= it.sub.iter().all(|s| s.label.is_empty());
        } else if depth == 0 && (1..0x8000).contains(&it.id) {
            // the verb's name: "rename" is the window's own (in its list, not Explorer's view)
            let mut name = [0u16; 64];
            if cm.GetCommandString(it.id as usize - 1, 4, None, PSTR(name.as_mut_ptr() as *mut u8), name.len() as u32).is_ok() {
                it.verb = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(0)]);
            }
        }
        let icon = match mi.hbmpItem as isize {
            -1 => drawn_icon(cm, menu, &mi), // HBMMENU_CALLBACK: the handler draws it
            0..=11 => None,                    // none, or a system glyph (HBMMENU_*)
            _ => bitmap_bgra(HBITMAP(mi.hbmpItem as _)),
        };
        if let Some((w, h, px)) = icon.filter(|i| i.0 <= 64 && i.1 <= 64) {
            (it.w, it.h, it.px) = (w as i32, h as i32, px);
        }
        out.push(it);
    }
    out
}

/// The icon of an item whose handler draws it (Open with's apps, Send to's places): drawn on black and on white
/// into bitmaps of the size it asks for. On black the result is the premultiplied color; the alpha is how much
/// the white shows through.
unsafe fn drawn_icon(cm: &windows::Win32::UI::Shell::IContextMenu, menu: windows_sys::Win32::UI::WindowsAndMessaging::HMENU, mi: &windows_sys::Win32::UI::WindowsAndMessaging::MENUITEMINFOW) -> Option<(usize, usize, Vec<u8>)> {
    use windows_sys::Win32::Graphics::Gdi as g;
    use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODA_DRAWENTIRE, ODT_MENU};
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_DRAWITEM, WM_MEASUREITEM};
    let mut ms: MEASUREITEMSTRUCT = zeroed();
    ms.CtlType = ODT_MENU;
    ms.itemID = mi.wID;
    ms.itemData = mi.dwItemData;
    menu_msg(cm, WM_MEASUREITEM, 0, &mut ms as *mut _ as isize)?;
    let (w, h) = (ms.itemWidth as usize, ms.itemHeight as usize);
    if w == 0 || h == 0 || w > 64 || h > 64 {
        return None;
    }
    let shot = |bg: u8| -> Option<Vec<u8>> {
        let mut bmi: g::BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<g::BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w as i32;
        bmi.bmiHeader.biHeight = -(h as i32);
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        let dc = g::CreateCompatibleDC(std::ptr::null_mut());
        let mut bits = std::ptr::null_mut();
        let bmp = g::CreateDIBSection(dc, &bmi, g::DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bmp.is_null() {
            g::DeleteDC(dc);
            return None;
        }
        let old = g::SelectObject(dc, bmp);
        std::ptr::write_bytes(bits as *mut u8, bg, w * h * 4);
        let mut ds: DRAWITEMSTRUCT = zeroed();
        ds.CtlType = ODT_MENU;
        ds.itemID = mi.wID;
        ds.itemAction = ODA_DRAWENTIRE;
        ds.hwndItem = menu as _;
        ds.hDC = dc;
        ds.rcItem = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        ds.itemData = mi.dwItemData;
        let drew = menu_msg(cm, WM_DRAWITEM, 0, &mut ds as *mut _ as isize).is_some();
        g::GdiFlush();
        let px = drew.then(|| std::slice::from_raw_parts(bits as *const u8, w * h * 4).to_vec());
        g::SelectObject(dc, old);
        g::DeleteObject(bmp);
        g::DeleteDC(dc);
        px
    };
    let (mut b, wt) = (shot(0)?, shot(255)?);
    for (p, q) in b.chunks_exact_mut(4).zip(wt.chunks_exact(4)) {
        let shows = (0..3).map(|k| q[k].saturating_sub(p[k]) as u32).sum::<u32>() / 3;
        p[3] = (255 - shows) as u8;
        (0..3).for_each(|k| p[k] = p[k].min(p[3]));
    }
    b.chunks_exact(4).any(|p| p[3] != 0).then_some((w, h, b))
}

unsafe fn own_window_shown() -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};
    unsafe extern "system" fn each(h: windows_sys::Win32::Foundation::HWND, found: isize) -> i32 {
        let mut pid = 0;
        GetWindowThreadProcessId(h, &mut pid);
        if pid == GetCurrentProcessId() && IsWindowVisible(h) != 0 {
            *(found as *mut bool) = true;
            return 0;
        }
        1
    }
    let mut found = false;
    EnumWindows(Some(each), &mut found as *mut bool as isize);
    found
}

/// Drag `paths` out as Explorer does, under `image` (premultiplied BGRA, its width and height: the first item's
/// thumbnail or icon), moved, copied or linked as the target says (a move on the same drive). Returns once
/// dropped or cancelled.
pub fn drag_out(paths: &[&str], host: isize, image: Option<(Vec<u8>, i32, i32)>) {
    use windows::Win32::Foundation::{COLORREF, POINT};
    use windows::Win32::Graphics::Gdi::{CreateDIBSection, BITMAPINFOHEADER};
    use windows::Win32::System::Com::{CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::Ole::{IDropSource, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
    use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDragSourceHelper2, SHDoDragDrop, DSH_ALLOWDROPDESCRIPTIONTEXT, SHDRAGIMAGE};
    let host = windows::Win32::Foundation::HWND(host as _);
    unsafe {
        let Some(data) = context_menu::<IDataObject>(paths, false, host) else { return };
        if let Ok(h) = CoCreateInstance::<_, IDragSourceHelper2>(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) {
            let _ = h.SetFlags(DSH_ALLOWDROPDESCRIPTIONTEXT.0 as u32); // room for "Move to …" under it
            let mut set = false;
            if let Some((px, w, ht)) = image {
                let mut bi: BITMAPINFO = zeroed();
                bi.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -ht, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..zeroed() };
                let mut bits = std::ptr::null_mut();
                if let Ok(bm) = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
                    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len().min((w * ht * 4) as usize));
                    // held by the pointer a little below its middle, as Explorer's
                    let di = SHDRAGIMAGE { sizeDragImage: SIZE { cx: w, cy: ht }, ptOffset: POINT { x: w / 2, y: ht * 3 / 4 }, hbmpDragImage: bm, crColorKey: COLORREF(0xFFFF_FFFF) };
                    set = h.InitializeFromBitmap(&di, &data).is_ok(); // the helper owns the bitmap now
                    if !set {
                        let _ = DeleteObject(bm);
                    }
                }
            }
            if !set {
                let _ = h.InitializeFromWindow(None, None, &data);
            }
        }
        let _ = SHDoDragDrop(host, &data, None::<&IDropSource>, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
    }
}

/// The shell's menu (or other UI object: a drag's data) for `paths` (all in one folder: from that folder, as
/// Explorer asks; from several: from the desktop, by full path), or with `bg` for the empty space of the folder `paths[0]`.
unsafe fn context_menu<T: windows::core::Interface>(paths: &[&str], bg: bool, host: windows::Win32::Foundation::HWND) -> Option<T> {
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{ILFindLastID, ILFree, IShellFolder, SHBindToParent, SHGetDesktopFolder, SHParseDisplayName};
    let pidls: Vec<*mut ITEMIDLIST> = paths
        .iter()
        .filter_map(|p| {
            let mut pidl = std::ptr::null_mut();
            SHParseDisplayName(PCWSTR(wide(p).as_ptr()), None, &mut pidl, 0, None).ok().map(|_| pidl)
        })
        .collect();
    let parent = |p: &str| std::path::Path::new(p).parent().map(|d| d.to_string_lossy().to_lowercase());
    let one_folder = paths.iter().all(|p| parent(p) == parent(paths[0]));
    let cm = (|| {
        let first = *pidls.first()?;
        if bg {
            let desktop = SHGetDesktopFolder().ok()?;
            let folder: IShellFolder = desktop.BindToObject(first, None).ok()?;
            return folder.CreateViewObject(host).ok();
        }
        if one_folder {
            let folder: IShellFolder = SHBindToParent(first, None).ok()?;
            let kids: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| ILFindLastID(*p) as *const ITEMIDLIST).collect();
            folder.GetUIObjectOf(host, &kids, None).ok()
        } else {
            let all: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
            SHGetDesktopFolder().ok()?.GetUIObjectOf(host, &all, None).ok()
        }
    })();
    for p in pidls {
        ILFree(Some(p));
    }
    cm
}

pub fn reveal(path: &str) {
    let _ = Command::new("explorer").raw_arg(format!("/select,\"{path}\"")).spawn();
}

/// Explorer's Properties dialog. Runs on its own thread with a message pump, since the dialog lives on it.
pub fn properties(path: String) {
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let (verb, file) = (wide("properties"), wide(&path));
        let mut sei: SHELLEXECUTEINFOW = zeroed();
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_INVOKEIDLIST;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.nShow = SW_SHOW;
        ShellExecuteExW(&mut sei);
        use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};
        let mut msg: MSG = zeroed();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while std::time::Instant::now() < end {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn menu_labels_lose_their_access_keys() {
        assert_eq!(super::menu_label("Open &with\tCtrl+O"), ("Open with".into(), "Ctrl+O".into()));
        assert_eq!(super::menu_label("Tom && Jerry"), ("Tom & Jerry".into(), String::new()));
    }
}
