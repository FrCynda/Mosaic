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
    let has_alpha = px.chunks_exact(4).any(|p| p[3] != 0);
    for p in px.chunks_exact_mut(4) {
        let a = if has_alpha { p[3] } else { 255 };
        // shell bitmaps are premultiplied BGRA; PNG wants straight RGBA
        let un = |c: u8| if a == 0 || a == 255 { c } else { ((c as u32 * 255 / a as u32).min(255)) as u8 };
        let (b, g, r) = (un(p[0]), un(p[1]), un(p[2]));
        p[0] = r;
        p[1] = g;
        p[2] = b;
        p[3] = a;
    }
    crate::infrastructure::icons::encode_png(&px, w as u32, h as u32)
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
