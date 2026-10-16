//! The preview pane at the right of the list (Alt+P): the one selected file, shown or played.
//! - video and audio play with Media Foundation (MFPlay), under our own play / seek / time bar
//! - documents (PDF, Office, …) use the preview handler Explorer's preview pane would, which Windows runs in its
//!   own process (prevhost), so a misbehaving handler can't take the app down and its memory isn't ours
//! - text and code are drawn here
//! - pictures, and anything else, come from the shell's thumbnail helper at the pane's size (else the file's icon)

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows::core::{Interface, BSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND as WinHwnd, RECT as WinRect};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFMediaEngine, IMFMediaEngineClassFactory, IMFMediaEngineEx, IMFMediaEngineNotify, MFCreateAttributes, MFCreateDXGIDeviceManager, MFStartup,
    CLSID_MFMediaEngineClassFactory, MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_DXGI_MANAGER, MF_MEDIA_ENGINE_PLAYBACK_HWND,
};
use windows::Win32::System::Com::{CLSIDFromString, CoCreateInstance, IStream, CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::Win32::UI::Shell::{
    AssocQueryStringW, IInitializeWithItem, IPreviewHandler, IPreviewHandlerVisuals, IShellItem, SHCreateItemFromParsingName, SHCreateStreamOnFileEx,
    ASSOCF, ASSOCSTR_SHELLEXTENSION,
};
use windows61::Win32::Graphics::DirectWrite::*;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, GetStockObject, InvalidateRect, BLACK_BRUSH, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{self, Bitmap, Canvas, Gfx};
use super::{gdi, inside, pixels, png_pixels, put, put_mid, shell, theme, w, wz, xy};

const VIDEO: &[&str] = &["mp4", "m4v", "mkv", "webm", "avi", "wmv", "asf", "mov", "mpg", "mpeg", "m2ts", "mts", "3gp", "3g2"];
const AUDIO: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "wma", "ogg", "oga", "opus", "aif", "aiff", "amr", "ac3"];
const IMAGES: &[&str] = &["jpg", "jpeg", "jfif", "png", "bmp", "webp", "tif", "tiff", "ico", "heic", "heif", "avif", "jxr", "dds", "tga", "psd"];
/// Shown as text even when Windows has a (white, plain) preview handler for them.
use crate::extract::{looks_text, OFFICE, TEXTS};
/// Text past this is cut, so opening a huge log stays instant.
const TEXT_MAX: usize = 256 * 1024;
const T_MEDIA: usize = 1;
const T_GIF: usize = 2;
/// A decoded GIF frame: premultiplied BGRA, width, height, delay (ms).
type GifFrame = (Vec<u8>, i32, i32, u32);
const T_AUTO: usize = 3;
const WM_ICON: u32 = WM_APP + 11;
/// Folder and archive listings are cut after this many entries.
const LIST_MAX: usize = 2000;
const ARCHIVES: &[&str] = &["zip", "jar", "war", "apk", "vsix", "nupkg", "whl", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "cab"];
/// Width (CSS px) of the divider strip at the pane's left.
pub const GAP: i32 = 8;

pub enum Target {
    Note(&'static str),
    File { path: String, name: String, detail: String },
}

enum Body {
    Note(&'static str),
    Picture,
    Text,
    Dir, // a folder's contents as clickable rows with icons
    Media,
    Handler,
    Anim, // animated GIF, decoded and played frame-by-frame ourselves
}

pub struct Preview {
    pub hwnd: HWND,
    video: HWND, // MFPlay draws the video into this child
    gfx: Gfx,
    cv: Canvas,
    mono: Option<gfx::Format>,
    scale: f64,
    key: String, // the path shown (or the note), so re-selecting the same file changes nothing
    title: String,
    detail: String,
    body: Body,
    note: &'static str, // under a fallback picture
    png: Option<Vec<u8>>,
    pic: Option<(Bitmap, i32, i32, u32, i32)>, // decoded for canvas `gen` at box size
    text: Vec<u16>,
    layout: Option<gfx::Text>,
    text_h: f32,
    scroll: f32,
    cut: bool,
    player: Option<IMFMediaEngine>, // the media engine (Edge's player): MFPlay can't open AV1, this can
    audio: bool,
    started: Instant,
    drag: Option<Bar>, // dragging the seek or volume bar
    vol: f32,          // 0–1, kept from file to file
    muted: bool,
    icons: Option<gfx::Format>, // Segoe MDL2 Assets: play, pause, speaker
    live: bool, // its divider is being dragged: stretch the picture, keep the text's wrap, until it's let go
    grip: f32,  // how lit the divider's grip is
    handler: Option<IPreviewHandler>,
    gif: Option<GifFrame>,                                  // the frame on screen
    gif_rx: Option<std::sync::mpsc::Receiver<GifFrame>>,    // the next few, from the decoding thread
    gif_bmp: Option<(Bitmap, u32)>, // current frame's bitmap, and the canvas `gen` it was made for
    grab: i32,          // dragging the scrollbar thumb: where it was grabbed, from its top
    auto: Option<Auto>, // middle-click autoscroll
    rows: Vec<Entry>,   // Body::Dir: the folder's entries
    ent_png: HashMap<String, Vec<u8>>, // shell icons by `Entry::key`
    ent_bmp: HashMap<String, (Bitmap, u32)>,
    ent_asked: HashSet<String>,
    hot: Option<usize>,
    press: Option<usize>,
    tracking: bool,
}

struct Entry {
    name: String,
    path: String,
    dir: bool,
    size: u64,
}

impl Entry {
    /// What the icon is cached under: one per folder/extension, except types whose icon is the file's own.
    fn key(&self) -> String {
        let ext = std::path::Path::new(&self.name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if self.dir {
            "\\dir".into()
        } else if matches!(ext.as_str(), "" | "exe" | "lnk" | "ico" | "url" | "scr" | "msi" | "cur" | "appx") {
            self.path.clone()
        } else {
            format!(".{ext}")
        }
    }
}

struct Auto {
    x: i32,
    y: i32,
    t0: Instant,
    moved: bool,
    last: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum Bar {
    Seek,
    Volume,
    Thumb,
}

/// The media engine's event sink, by hand: an object (one a window, never freed) that only notes that the file
/// failed to open (`MF_MEDIA_ENGINE_EVENT_ERROR`, from any thread); `tick` looks at the flag.
#[repr(C)]
struct EngineNotify(*const NotifyVtbl, std::sync::atomic::AtomicBool);
unsafe impl Sync for EngineNotify {}
#[repr(C)]
struct NotifyVtbl {
    query: unsafe extern "system" fn(*mut std::ffi::c_void, *const windows::core::GUID, *mut *mut std::ffi::c_void) -> windows::core::HRESULT,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    event: unsafe extern "system" fn(*mut std::ffi::c_void, u32, usize, u32) -> windows::core::HRESULT,
}
/// This window's (each window has a thread of its own, and its own video playing).
fn notify() -> &'static EngineNotify {
    // ponytail: 16 bytes kept per window ever opened, for an object the engine may still call after it is shut down
    thread_local!(static N: &'static EngineNotify = Box::leak(Box::new(EngineNotify(&NOTIFY_VTBL, std::sync::atomic::AtomicBool::new(false)))));
    N.with(|n| *n)
}
unsafe extern "system" fn notify_query(this: *mut std::ffi::c_void, _: *const windows::core::GUID, out: *mut *mut std::ffi::c_void) -> windows::core::HRESULT {
    *out = this;
    windows::core::HRESULT(0)
}
unsafe extern "system" fn notify_ref(_: *mut std::ffi::c_void) -> u32 {
    2 // a static: never freed
}
unsafe extern "system" fn notify_event(this: *mut std::ffi::c_void, event: u32, _: usize, _: u32) -> windows::core::HRESULT {
    if event == 5 {
        (*(this as *const EngineNotify)).1.store(true, std::sync::atomic::Ordering::Relaxed); // MF_MEDIA_ENGINE_EVENT_ERROR
    }
    windows::core::HRESULT(0)
}
static NOTIFY_VTBL: NotifyVtbl = NotifyVtbl { query: notify_query, add_ref: notify_ref, release: notify_ref, event: notify_event };

thread_local!(static PV: RefCell<Option<Preview>> = const { RefCell::new(None) });

impl Drop for Preview {
    /// The window closed with a video in it: one engine fewer (its own release does the rest).
    fn drop(&mut self) {
        if self.player.is_some() {
            ENGINES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// One D3D11 device for every media engine, in every window of the process. Left to itself, each engine makes its
/// own and the GPU driver never lets go of it (measured: ~13 driver threads and ~30 MB kept per video previewed).
fn dxgi() -> Option<IMFDXGIDeviceManager> {
    shared().map(|s| s.0.clone())
}
struct Shared(IMFDXGIDeviceManager, windows::Win32::Graphics::Direct3D11::ID3D11Device);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {} // the manager is free-threaded, its device multithread-protected (below)
fn shared() -> Option<&'static Shared> {
    static M: std::sync::OnceLock<Option<Shared>> = std::sync::OnceLock::new();
    M.get_or_init(|| unsafe { new_dxgi() }).as_ref()
}
/// Media engines alive in the process, and whether the device was trimmed since the last one went.
static ENGINES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static TRIMMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// No video left playing anywhere: the driver hands back what it holds for the device, which stays (made again,
/// it would leak).
pub fn trim_video() {
    use std::sync::atomic::Ordering::*;
    if ENGINES.load(Relaxed) != 0 || TRIMMED.swap(true, Relaxed) {
        return;
    }
    if let Some(Shared(_, dev)) = shared() {
        unsafe {
            if let Ok(cx) = dev.GetImmediateContext() {
                cx.ClearState();
                cx.Flush();
            }
            if let Ok(d) = dev.cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice3>() {
                d.Trim();
            }
        }
    }
}
unsafe fn new_dxgi() -> Option<Shared> {
    let mut dev = None;
    let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, None, flags, None, D3D11_SDK_VERSION, Some(&mut dev), None, None).ok()?;
    let dev = dev?;
    // the engines' threads share it
    if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let (mut token, mut mgr) = (0, None);
    MFCreateDXGIDeviceManager(&mut token, &mut mgr).ok()?;
    let mgr = mgr?;
    mgr.ResetDevice(&dev, token).ok()?;
    Some(Shared(mgr, dev))
}

/// Run `f` on the pane. `None` while it is busy further up the stack: a preview handler's cross-process call
/// pumps messages, so our window procedures can run inside it.
fn with<R>(f: impl FnOnce(&mut Preview) -> R) -> Option<R> {
    PV.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Make the (hidden) pane as a child of `parent`. Returns its window.
pub unsafe fn create(parent: HWND, scale: f64) -> HWND {
    static CLASS: std::sync::Once = std::sync::Once::new();
    let (pc, vc) = (wz("MosaicPreview"), wz("MosaicVideo"));
    let inst = GetModuleHandleW(null());
    CLASS.call_once(|| {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = inst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.lpfnWndProc = Some(preview_proc);
        wc.lpszClassName = pc.as_ptr();
        RegisterClassExW(&wc);
        wc.lpfnWndProc = Some(video_proc);
        wc.hbrBackground = GetStockObject(BLACK_BRUSH);
        wc.lpszClassName = vc.as_ptr();
        RegisterClassExW(&wc);
    });
    let hwnd = CreateWindowExW(0, pc.as_ptr(), null(), WS_CHILD | WS_CLIPCHILDREN, 0, 0, 10, 10, parent, null_mut(), inst, null());
    let video = CreateWindowExW(0, vc.as_ptr(), null(), WS_CHILD, 0, 0, 10, 10, hwnd, null_mut(), inst, null());
    if let Ok(gfx) = Gfx::new(scale, crate::i18n::rtl()) {
        let p = Preview {
            hwnd,
            video,
            cv: Canvas::new(hwnd, &gfx),
            mono: mono(&gfx, scale),
            icons: icons(&gfx, scale),
            gfx,
            scale,
            key: String::new(),
            title: String::new(),
            detail: String::new(),
            body: Body::Note(""),
            note: "",
            png: None,
            pic: None,
            text: Vec::new(),
            layout: None,
            text_h: 0.0,
            scroll: 0.0,
            cut: false,
            player: None,
            audio: false,
            started: Instant::now(),
            drag: None,
            vol: 1.0,
            muted: false,
            live: false,
            grip: 0.0,
            handler: None,
            gif: None,
            gif_rx: None,
            gif_bmp: None,
            grab: 0,
            auto: None,
            rows: Vec::new(),
            ent_png: HashMap::new(),
            ent_bmp: HashMap::new(),
            ent_asked: HashSet::new(),
            hot: None,
            press: None,
            tracking: false,
        };
        PV.with(|c| *c.borrow_mut() = Some(p));
    }
    hwnd
}

/// Show `t`. Returns (path, px) when the pane wants the shell's picture of a file at that size: the caller fetches
/// it off this thread and hands it to [`got_pic`].
pub fn show(t: Target) -> Option<(String, i32)> {
    with(|p| p.show(t)).flatten()
}

pub fn got_pic(path: &str, png: Option<Vec<u8>>) {
    with(|p| {
        if p.key == path && matches!(p.body, Body::Picture | Body::Media) {
            if png.is_none() && matches!(p.body, Body::Picture) {
                p.body = Body::Note("No preview");
            }
            p.png = png;
            p.pic = None;
            unsafe { InvalidateRect(p.hwnd, null(), 0) };
        }
    });
}

/// How lit the divider's grip is (0–1).
pub fn grip(a: f32) {
    with(|p| {
        if p.grip != a {
            p.grip = a;
            let r = RECT { left: 0, top: 0, right: p.px(GAP), bottom: 1 << 15 };
            unsafe { InvalidateRect(p.hwnd, &r, 0) };
        }
    });
}

/// The pane's divider is being dragged (`true`) or was let go.
pub fn resizing(on: bool) {
    with(|p| {
        p.live = on;
        if !on {
            p.relayout();
        }
    });
}

/// Stop playback and let go of the file (the pane was hidden).
pub fn clear() {
    with(|p| p.clear());
}

/// About to move, delete or rename `paths`: if the pane shows one of them (or something inside one), let go of it,
/// else a playing video holds the file and the shell waits on it ("File In Use"). True if it did.
pub fn release(paths: &[String]) -> bool {
    with(|p| {
        let k = p.key.to_lowercase();
        let hit = !k.is_empty() && paths.iter().any(|f| {
            let f = f.trim_end_matches('\\').to_lowercase();
            k == f || k.starts_with(&format!("{f}\\"))
        });
        if hit {
            p.clear();
        }
        hit
    })
    .unwrap_or(false)
}

/// Redraw now: `theme()` is read fresh on every paint, but the pane is its own window, so the main
/// window's `InvalidateRect` never reaches it — a theme change needs this to show up without delay.
pub fn invalidate() {
    with(|p| unsafe { InvalidateRect(p.hwnd, null(), 0) });
}

/// Its pixels go (the window minimized), made again when it's next drawn.
pub fn release_pixels() {
    with(|p| p.cv.release());
}

pub fn rescale(scale: f64) {
    with(|p| {
        if let Ok(g) = Gfx::new(scale, crate::i18n::rtl()) {
            p.scale = scale;
            p.cv = Canvas::new(p.hwnd, &g);
            p.mono = mono(&g, scale);
            p.icons = icons(&g, scale);
            p.gfx = g;
            p.pic = None;
            p.ent_png.clear();
            p.ent_bmp.clear();
            p.ent_asked.clear();
            p.relayout();
        }
    });
}

/// Code font: Consolas 12px, wrapping at spaces (and inside words too long for a line), 4-space tabs.
fn mono(g: &Gfx, scale: f64) -> Option<gfx::Format> {
    unsafe {
        let f = g
            .dw
            .CreateTextFormat(windows61::core::w!("Consolas"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, (12.0 * scale) as f32, windows61::core::w!(""))
            .ok()?;
        let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK);
        let _ = f.SetIncrementalTabStop((4.0 * 12.0 * 0.55 * scale) as f32); // Consolas is 0.55 em wide
        Some(f)
    }
}

/// The icon font Windows' own media controls use.
fn icons(g: &Gfx, scale: f64) -> Option<gfx::Format> {
    unsafe {
        g.dw.CreateTextFormat(windows61::core::w!("Segoe MDL2 Assets"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, (14.0 * scale) as f32, windows61::core::w!(""))
            .ok()
    }
}

fn wr(r: &RECT) -> WinRect {
    WinRect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

/// Bytes as UTF-16 for on-screen display: `extract::decode_str` does the actual decoding.
fn decode(b: &[u8], cut: bool) -> Vec<u16> {
    crate::extract::decode_str(b, cut).encode_utf16().collect()
}

fn clock(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Folders first, then files, each sorted by name (the first `LIST_MAX`).
fn folder_listing(path: &str) -> Option<Vec<Entry>> {
    let mut v: Vec<Entry> = std::fs::read_dir(path)
        .ok()?
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (dir, size) = e.metadata().map_or((false, 0), |m| (m.is_dir(), m.len()));
            Entry { path: e.path().to_string_lossy().into_owned(), name, dir, size }
        })
        .collect();
    v.sort_by_cached_key(|e| (!e.dir, e.name.to_lowercase()));
    v.truncate(LIST_MAX);
    Some(v)
}

/// The first `LIST_MAX` lines, then how many were left out ("(empty)" for none).
fn capped(lines: impl Iterator<Item = String>, total: usize, exact: bool) -> String {
    if total == 0 {
        return "(empty)".into();
    }
    let mut out: String = lines.take(LIST_MAX).map(|l| l + "\n").collect();
    if total > LIST_MAX {
        out.push_str(&if exact { format!("… and {} more\n", total - LIST_MAX) } else { "… and more\n".into() });
    }
    out
}

/// An archive's entries: zip files are read in-process, everything else (7z, rar, tar.*, iso, cab…) by the `tar`
/// that ships with Windows 10+, which reads them all. Names only, as stored.
fn archive_listing(path: &str, ext: &str) -> Option<String> {
    use std::io::BufRead;
    let mut names: Vec<String> = Vec::new();
    let (mut total, mut exact) = (0, true);
    if matches!(ext, "zip" | "jar" | "war" | "apk" | "vsix" | "nupkg" | "whl") {
        if let Ok(mut z) = std::fs::File::open(path).map_err(|_| ()).and_then(|f| zip::ZipArchive::new(f).map_err(|_| ())) {
            total = z.len();
            names = (0..z.len().min(LIST_MAX)).filter_map(|i| Some(z.by_index_raw(i).ok()?.name().to_string())).collect();
        }
    }
    if names.is_empty() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut child = Command::new("tar").args(["-tf", path]).creation_flags(0x0800_0000).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        for line in std::io::BufReader::new(child.stdout.take()?).lines().map_while(Result::ok) {
            total += 1;
            if names.len() < LIST_MAX {
                names.push(line);
            } else {
                exact = false; // ponytail: past the cap the count is only "more than"; reading a huge archive to the end isn't worth it
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    if names.is_empty() {
        return None;
    }
    let count = if exact { total.to_string() } else { format!("{}+", LIST_MAX) };
    Some(format!("{count} {}\n\n{}", if count == "1" { "entry" } else { "entries" }, capped(names.into_iter(), total, exact)))
}

impl Preview {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }

    fn show(&mut self, t: Target) -> Option<(String, i32)> {
        let key = match &t {
            Target::Note(m) => m.to_string(),
            Target::File { path, .. } => path.clone(),
        };
        if key == self.key {
            return None;
        }
        self.clear();
        self.key = key;
        let fetch = match t {
            Target::Note(m) => {
                self.body = Body::Note(m);
                None
            }
            Target::File { path, name, detail } => {
                self.title = name;
                self.detail = detail;
                self.open(&path)
            }
        };
        self.relayout();
        fetch
    }

    fn clear(&mut self) {
        unsafe {
            if let Some(p) = self.player.take() {
                let _ = p.Shutdown();
                ENGINES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(h) = self.handler.take() {
                let _ = h.Unload();
            }
            ShowWindow(self.video, SW_HIDE);
            KillTimer(self.hwnd, T_MEDIA);
            KillTimer(self.hwnd, T_GIF);
            KillTimer(self.hwnd, T_AUTO);
            InvalidateRect(self.hwnd, null(), 0);
        }
        self.auto = None;
        self.key.clear();
        self.title.clear();
        self.detail.clear();
        self.body = Body::Note("");
        self.note = "";
        self.png = None;
        self.pic = None;
        self.text.clear();
        self.rows.clear();
        self.hot = None;
        self.layout = None;
        self.scroll = 0.0;
        self.cut = false;
        self.audio = false;
        self.gif = None;
        self.gif_rx = None; // the decoding thread stops at its next send
        self.gif_bmp = None;
    }

    /// Pick how to show `path`; returns a picture to fetch, if one is wanted.
    fn open(&mut self, path: &str) -> Option<(String, i32)> {
        let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let (body, _) = self.rects();
        let fetch = Some((path.to_string(), (body.right - body.left).max(body.bottom - body.top).clamp(64, 1600)));
        let e = ext.as_str();
        if std::path::Path::new(path).is_dir() {
            match folder_listing(path) {
                Some(v) => {
                    self.rows = v;
                    self.hot = None;
                    self.body = Body::Dir;
                    self.text_h = self.rows.len() as f32 * self.row_h() as f32;
                }
                None => {
                    self.note = "Can't read this folder";
                    self.body = Body::Picture;
                    return fetch;
                }
            }
            return None;
        }
        if e == "gif" {
            // Media Foundation has no real GIF source, so this is decoded and animated by hand
            if self.load_gif(path) {
                return None;
            }
            self.body = Body::Picture;
            return fetch;
        }
        if VIDEO.contains(&e) || AUDIO.contains(&e) {
            self.audio = AUDIO.contains(&e);
            if self.play(path) {
                return if self.audio { fetch } else { None }; // audio: the album art (or the file's icon)
            }
            self.note = "This file can't be played";
            self.body = Body::Picture;
            return fetch;
        }
        if IMAGES.contains(&e) {
            self.body = Body::Picture;
            return fetch;
        }
        if ARCHIVES.contains(&e) {
            if let Some(t) = archive_listing(path, e) {
                self.list(t);
                return None;
            }
        }
        if (TEXTS.contains(&e) && self.load_text(path, true)) || self.open_handler(path, e) || (OFFICE.contains(&e) && self.load_office_text(path, e)) || self.load_text(path, false) {
            return None;
        }
        self.note = "No preview for this type of file";
        self.body = Body::Picture;
        fetch
    }

    /// Show generated text (a folder's or an archive's contents) as a scrollable page.
    fn list(&mut self, text: String) {
        self.text = text.encode_utf16().collect();
        self.cut = false;
        self.body = Body::Text;
    }

    fn load_text(&mut self, path: &str, force: bool) -> bool {
        use std::io::Read;
        let mut b = Vec::new();
        let Ok(f) = std::fs::File::open(path) else { return false };
        if f.take(TEXT_MAX as u64 + 1).read_to_end(&mut b).is_err() || (!force && !looks_text(&b)) {
            return false;
        }
        self.cut = b.len() > TEXT_MAX;
        b.truncate(TEXT_MAX);
        self.text = decode(&b, self.cut);
        self.body = Body::Text;
        true
    }

    /// Falls back to plain text pulled out of an Office/OpenDocument zip when Windows has no real preview
    /// handler for it registered (no Office or LibreOffice installed) — a rough read beats "no preview".
    fn load_office_text(&mut self, path: &str, ext: &str) -> bool {
        let Some(text) = crate::extract::office_text(path, ext) else { return false };
        self.text = text.encode_utf16().collect();
        self.cut = false;
        self.body = Body::Text;
        true
    }

    /// Plays a GIF from a decoding thread that stays a few frames ahead (a bounded channel), looping by decoding the
    /// file again: memory is a handful of frames whatever the length. Decoding every frame up front held 1.5 GB for
    /// a 1800-frame 480x432 GIF (twice that while converting) and froze the window until it was done.
    /// `image` composites each frame per its disposal method, so each is a full frame at the GIF's own size.
    fn load_gif(&mut self, path: &str) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel::<GifFrame>(3);
        let path = path.to_string();
        std::thread::spawn(move || loop {
            let Ok(f) = std::fs::File::open(&path) else { return };
            let Ok(dec) = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(f)) else { return };
            let mut n = 0;
            for fr in image::AnimationDecoder::into_frames(dec) {
                let Ok(fr) = fr else { break };
                let (num, den) = fr.delay().numer_denom_ms();
                // GIF delay is stored in centiseconds; 0 (and, by the convention every major browser follows, any
                // value under ~2 cs) means "as fast as the format allows" and is shown at a 10 fps default instead
                let ms = if den == 0 { 0 } else { num / den };
                let buf = fr.buffer();
                let (w, h) = (buf.width() as usize, buf.height() as usize);
                let (px, dw, dh) = pixels(buf.as_raw(), w, h, w.max(h)); // premultiplies to BGRA; size = no downscale
                if tx.send((px, dw, dh, if ms < 20 { 100 } else { ms })).is_err() {
                    return; // preview moved on
                }
                n += 1;
            }
            if n <= 1 {
                return; // a still (or broken) GIF: nothing to loop
            }
        });
        let Ok(first) = rx.recv_timeout(Duration::from_secs(3)) else { return false };
        unsafe { SetTimer(self.hwnd, T_GIF, first.3, None) };
        self.gif = Some(first);
        self.gif_rx = Some(rx);
        self.gif_bmp = None;
        self.body = Body::Anim;
        true
    }

    /// Every frame's delay, once it's shown: take the next one from the decoder and repaint.
    fn gif_tick(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let next = self.gif_rx.as_ref().map(|rx| rx.try_recv());
        unsafe {
            match next {
                Some(Ok(f)) => {
                    SetTimer(self.hwnd, T_GIF, f.3, None);
                    self.gif = Some(f);
                    self.gif_bmp = None;
                    InvalidateRect(self.hwnd, null(), 0);
                }
                Some(Err(TryRecvError::Empty)) => {
                    SetTimer(self.hwnd, T_GIF, 10, None); // decoder a little behind: try again shortly
                }
                _ => {
                    KillTimer(self.hwnd, T_GIF); // ended (a still image) or gone: keep the last frame
                }
            }
        }
    }

    /// The preview handler registered for `.ext`, run by Windows in its preview host process.
    fn open_handler(&mut self, path: &str, ext: &str) -> bool {
        unsafe {
            let (dot, iid) = (wz(&format!(".{ext}")), wz("{8895b1c6-b41f-4c1c-a562-0d564250836f}"));
            let mut buf = [0u16; 64];
            let mut n = buf.len() as u32;
            if AssocQueryStringW(ASSOCF(0), ASSOCSTR_SHELLEXTENSION, PCWSTR(dot.as_ptr()), PCWSTR(iid.as_ptr()), PWSTR(buf.as_mut_ptr()), &mut n).is_err() {
                return false;
            }
            let Ok(clsid) = CLSIDFromString(PCWSTR(buf.as_ptr())) else { return false };
            // out of process only: an in-process handler that crashes would take the app with it
            let Ok(h) = CoCreateInstance::<_, IPreviewHandler>(&clsid, None, CLSCTX_LOCAL_SERVER) else { return false };
            let p = wz(path);
            let file = PCWSTR(p.as_ptr());
            let init = h.cast::<IInitializeWithItem>().is_ok_and(|i| SHCreateItemFromParsingName::<_, _, IShellItem>(file, None).and_then(|it| i.Initialize(&it, 0)).is_ok())
                || h.cast::<IInitializeWithFile>().is_ok_and(|i| i.Initialize(file, 0).is_ok())
                || h.cast::<IInitializeWithStream>().is_ok_and(|i| {
                    // STGM_READ | STGM_SHARE_DENY_NONE: the file stays usable while it is previewed
                    SHCreateStreamOnFileEx(file, 0x40, 0, BOOL(0), None::<&IStream>).and_then(|s| i.Initialize(&s, 0)).is_ok()
                });
            if !init {
                return false;
            }
            if let Ok(v) = h.cast::<IPreviewHandlerVisuals>() {
                let _ = v.SetBackgroundColor(COLORREF(gdi(theme().card)));
                let _ = v.SetTextColor(COLORREF(gdi(theme().text)));
            }
            self.body = Body::Handler;
            let (body, _) = self.rects();
            if h.SetWindow(WinHwnd(self.hwnd as _), &wr(&body)).is_err() || h.DoPreview().is_err() {
                let _ = h.Unload();
                return false;
            }
            self.handler = Some(h);
            true
        }
    }

    fn play(&mut self, path: &str) -> bool {
        static MF: std::sync::Once = std::sync::Once::new();
        unsafe {
            MF.call_once(|| {
                let _ = MFStartup(0x0002_0070, 0); // MF_VERSION, MFSTARTUP_FULL
            });
            notify().1.store(false, std::sync::atomic::Ordering::Relaxed);
            let make = || -> windows::core::Result<IMFMediaEngine> {
                let mut at = None;
                MFCreateAttributes(&mut at, 3)?;
                let at = at.ok_or_else(windows::core::Error::empty)?;
                let notify = IMFMediaEngineNotify::from_raw(notify() as *const EngineNotify as *mut std::ffi::c_void);
                at.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
                at.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, self.video as usize as u64)?;
                if let Some(m) = dxgi() {
                    at.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &m)?;
                }
                let factory: IMFMediaEngineClassFactory = CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
                let engine = factory.CreateInstance(0, &at)?;
                engine.SetSource(&BSTR::from(path))?;
                let _ = engine.SetLoop(BOOL(!self.audio as i32)); // a video repeats, audio just ends
                let _ = engine.SetVolume(self.vol as f64);
                let _ = engine.SetMuted(BOOL(self.muted as i32));
                engine.Play()?;
                Ok(engine)
            };
            let Ok(engine) = make() else { return false };
            self.player = Some(engine);
            ENGINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            TRIMMED.store(false, std::sync::atomic::Ordering::Relaxed);
            self.body = Body::Media;
            self.started = Instant::now();
            SetTimer(self.hwnd, T_MEDIA, 250, None);
        }
        true
    }

    /// Seconds played and in total, once the file is open.
    fn times(&self) -> Option<(f64, f64)> {
        let pl = self.player.as_ref()?;
        unsafe {
            let d = pl.GetDuration();
            (d.is_finite() && d > 0.0).then(|| (pl.GetCurrentTime().min(d), d)) // NaN until the metadata is read
        }
    }

    fn playing(&self) -> bool {
        self.player.as_ref().is_some_and(|p| unsafe { !p.IsPaused().as_bool() && !p.IsEnded().as_bool() })
    }

    /// Distinct from a user's pause: played to the end (audio only: a video loops).
    fn stopped(&self) -> bool {
        self.player.as_ref().is_some_and(|p| unsafe { p.IsEnded().as_bool() })
    }

    fn toggle(&mut self) {
        if let Some(p) = &self.player {
            unsafe {
                if p.IsEnded().as_bool() {
                    let _ = p.SetCurrentTime(0.0);
                }
                let _ = if self.playing() { p.Pause() } else { p.Play() };
                InvalidateRect(self.hwnd, null(), 0);
            }
        }
    }

    fn seek(&mut self, x: i32) {
        let (Some(p), Some((_, d))) = (&self.player, self.times()) else { return };
        let t = self.track();
        let f = ((x - t.left) as f64 / (t.right - t.left).max(1) as f64).clamp(0.0, 1.0);
        unsafe {
            let _ = p.SetCurrentTime(f * d);
            InvalidateRect(self.hwnd, null(), 0);
        }
    }

    fn volume(&mut self, x: i32) {
        let t = self.vol_track();
        self.vol = ((x - t.left) as f32 / (t.right - t.left).max(1) as f32).clamp(0.0, 1.0);
        self.muted = false;
        self.apply_volume();
    }
    fn mute(&mut self) {
        self.muted = !self.muted;
        self.apply_volume();
    }
    fn apply_volume(&self) {
        if let Some(p) = &self.player {
            unsafe {
                let _ = p.SetVolume(self.vol as f64);
                let _ = p.SetMuted(BOOL(self.muted as i32));
            }
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// Every 250 ms while media is open: move the time along; give up on files Media Foundation can't open.
    fn tick(&mut self) {
        if self.player.is_none() {
            unsafe { KillTimer(self.hwnd, T_MEDIA) };
            return;
        }
        match self.times() {
            None if notify().1.load(std::sync::atomic::Ordering::Relaxed) || self.started.elapsed() > Duration::from_secs(8) => {
                if let Some(p) = self.player.take() {
                    unsafe {
                        let _ = p.Shutdown();
                        ENGINES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        ShowWindow(self.video, SW_HIDE);
                    }
                }
                self.body = Body::Note("This file can't be played");
            }
            // a video that ended despite looping (some sources): restart it; audio just stops
            Some((_, dur)) if !self.audio && dur > 0.0 && self.stopped() => {
                if let Some(p) = &self.player {
                    unsafe {
                        let _ = p.SetCurrentTime(0.0);
                        let _ = p.Play();
                    }
                }
            }
            _ => {}
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// The pane's card: the window less the divider strip at its left (part of this window, so the grip moves in
    /// the same frame as the pane; its mouse input goes through to the main window).
    fn card(&self) -> RECT {
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut c) };
        RECT { left: self.px(GAP), ..c }
    }

    /// The content box and (for media) the control bar under it.
    fn rects(&self) -> (RECT, RECT) {
        let c = self.card();
        let (pad, lh) = (self.px(12), self.gfx.f.lh);
        let top = pad + 2 * lh + self.px(10);
        let mut body = RECT { left: c.left + pad, top, right: (c.right - pad).max(c.left + pad + 1), bottom: (c.bottom - pad).max(top + 1) };
        let mut ctl = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if matches!(self.body, Body::Media) {
            ctl = RECT { top: (body.bottom - lh - self.px(12)).max(body.top), ..body };
            body.bottom = (ctl.top - self.px(8)).max(body.top + 1);
        }
        (body, ctl)
    }
    fn play_btn(&self) -> RECT {
        let (_, c) = self.rects();
        RECT { right: c.left + (c.bottom - c.top), ..c }
    }
    /// Speaker (mute) button and volume bar at the bar's right end.
    fn mute_btn(&self) -> RECT {
        let (_, c) = self.rects();
        let x = self.vol_track().left - self.px(6);
        RECT { left: x - (c.bottom - c.top), right: x, ..c }
    }
    fn vol_track(&self) -> RECT {
        let (_, c) = self.rects();
        RECT { left: (c.right - self.px(64)).max(c.left), ..c }
    }
    /// The seek bar's clickable box: between ▶ and the time.
    fn track(&self) -> RECT {
        let (_, c) = self.rects();
        let tw = self.gfx.width("0:00:00 / 0:00:00", &self.gfx.f.reg);
        RECT { left: c.left + (c.bottom - c.top) + self.px(12), right: (self.mute_btn().left - tw - self.px(24)).max(c.left), ..c }
    }

    /// The pane changed size: move the video / handler, re-wrap text.
    fn relayout(&mut self) {
        let (body, _) = self.rects();
        unsafe {
            if self.player.is_some() && !self.audio {
                MoveWindow(self.video, body.left, body.top, body.right - body.left, body.bottom - body.top, 1);
                ShowWindow(self.video, SW_SHOWNA);
                if let Some(ex) = self.player.as_ref().and_then(|p| p.cast::<IMFMediaEngineEx>().ok()) {
                    let dst = WinRect { left: 0, top: 0, right: body.right - body.left, bottom: body.bottom - body.top };
                    let _ = ex.UpdateVideoStream(None, Some(&dst), None);
                }
            } else {
                ShowWindow(self.video, SW_HIDE);
            }
            if let Some(h) = &self.handler {
                let _ = h.SetRect(&wr(&body));
            }
            InvalidateRect(self.hwnd, null(), 0);
        }
        if !self.live {
            self.layout = None; // re-wrapped at the new width when painted (a long file takes a moment: not per drag step)
        }
    }

    fn text_layout(&mut self, width: i32) -> Option<gfx::Text> {
        if self.layout.is_none() && !self.text.is_empty() {
            let l = self.gfx.layout(&self.text, self.mono.as_ref()?, width as f32, 1e7)?;
            self.text_h = gfx::metrics(&l).height;
            self.layout = Some(l);
        }
        self.layout.clone()
    }

    fn line(&self) -> f32 {
        if matches!(self.body, Body::Dir) {
            return self.row_h() as f32;
        }
        (12.0 * 1.17 * self.scale) as f32 // Consolas' line
    }

    fn row_h(&self) -> i32 {
        self.gfx.f.lh + self.px(8)
    }

    fn max_scroll(&self) -> f32 {
        let (body, _) = self.rects();
        let slack = if matches!(self.body, Body::Dir) { 0.0 } else { self.line() };
        (self.text_h - (body.bottom - body.top) as f32 + slack).max(0.0)
    }

    /// The row under a client point, for `Body::Dir`.
    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        let (body, _) = self.rects();
        if !matches!(self.body, Body::Dir) || x < body.left || x >= body.right || y < body.top || y >= body.bottom {
            return None;
        }
        let i = ((y - body.top) as f32 + self.scroll) / self.row_h() as f32;
        (i >= 0.0).then_some(i as usize).filter(|&i| i < self.rows.len())
    }

    /// Ask the shell helper for the icons of the rows on screen (once per kind), decode what has arrived.
    fn dir_icons(&mut self, body: &RECT) {
        let rh = self.row_h() as f32;
        let a = ((self.scroll / rh) as usize).min(self.rows.len());
        let b = (((self.scroll + (body.bottom - body.top) as f32) / rh).ceil() as usize + 1).min(self.rows.len());
        let (size, hw, gen) = (self.px(16), self.hwnd as isize, self.cv.gen);
        for i in a..b.max(a) {
            let key = self.rows[i].key();
            if !self.ent_png.contains_key(&key) && self.ent_asked.insert(key.clone()) {
                let (path, k) = (self.rows[i].path.clone(), key.clone());
                shell::shell_spawn(Box::new(move |dropped| {
                    let png = if dropped { None } else { shell::thumbnail(&path, size, true) };
                    let m: Box<(String, Option<Vec<u8>>)> = Box::new((k, png));
                    unsafe { PostMessageW(hw as HWND, WM_ICON, 0, Box::into_raw(m) as isize) };
                }));
            }
            if self.ent_bmp.get(&key).is_none_or(|x| x.1 != gen) {
                if let Some((px, w, h)) = self.ent_png.get(&key).and_then(|p| png_pixels(p, size)) {
                    if let Some(bm) = self.cv.make_bitmap(&px, w, h) {
                        self.ent_bmp.insert(key, (bm, gen));
                    }
                }
            }
        }
    }

    fn got_icon(&mut self, (key, png): (String, Option<Vec<u8>>)) {
        if let Some(p) = png {
            self.ent_png.insert(key.clone(), p);
            self.ent_bmp.remove(&key);
        }
        self.ent_asked.remove(&key);
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn thumb_len(&self, bh: f32) -> f32 {
        (bh * bh / self.text_h).max(self.px(24) as f32)
    }

    /// The scrollbar's thumb: only for text that doesn't fit, wider while it's dragged.
    fn thumb(&self) -> Option<RECT> {
        let (body, _) = self.rects();
        let bh = (body.bottom - body.top) as f32;
        if !matches!(self.body, Body::Text | Body::Dir) || self.text_h <= bh {
            return None;
        }
        let len = self.thumb_len(bh);
        let top = body.top as f32 + (bh - len) * self.scroll / self.max_scroll().max(1.0);
        let right = self.card().right - self.px(3);
        let w = self.px(if self.drag == Some(Bar::Thumb) { 7 } else { 4 });
        Some(RECT { left: right - w, top: top as i32, right, bottom: (top + len) as i32 })
    }

    /// A press on the scrollbar's strip (wider than the thumb): grab the thumb, or page towards the pointer.
    fn sb_down(&mut self, x: i32, y: i32) -> bool {
        let Some(t) = self.thumb() else { return false };
        let (body, _) = self.rects();
        if x < self.card().right - self.px(16) || y < body.top || y >= body.bottom {
            return false;
        }
        if y >= t.top && y < t.bottom {
            self.grab = y - t.top;
            self.drag = Some(Bar::Thumb);
            unsafe { SetCapture(self.hwnd) };
        } else {
            let page = (body.bottom - body.top) as f32 - self.line();
            self.scroll = (self.scroll + if y < t.top { -page } else { page }).clamp(0.0, self.max_scroll());
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
        true
    }

    fn drag_thumb(&mut self, y: i32) {
        let (body, _) = self.rects();
        let bh = (body.bottom - body.top) as f32;
        let free = (bh - self.thumb_len(bh)).max(1.0);
        let t = ((y - self.grab - body.top) as f32 / free).clamp(0.0, 1.0);
        self.scroll = t * self.max_scroll();
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn wheel(&mut self, delta: i32) {
        self.stop_auto();
        if !matches!(self.body, Body::Text | Body::Dir) {
            return;
        }
        self.scroll = (self.scroll - delta as f32 / 120.0 * 3.0 * self.line()).clamp(0.0, self.max_scroll());
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    // -- middle-click autoscroll, as in the list: press to start, the pointer's distance from the anchor is the speed
    fn stop_auto(&mut self) {
        if self.auto.take().is_some() {
            unsafe {
                KillTimer(self.hwnd, T_AUTO);
                ReleaseCapture();
                InvalidateRect(self.hwnd, null(), 0);
            }
        }
    }

    fn mdown(&mut self, x: i32, y: i32) {
        if self.auto.is_some() {
            return self.stop_auto();
        }
        if self.thumb().is_some() {
            let now = Instant::now();
            self.auto = Some(Auto { x, y, t0: now, moved: false, last: now });
            unsafe {
                SetCapture(self.hwnd);
                SetTimer(self.hwnd, T_AUTO, 15, None);
                InvalidateRect(self.hwnd, null(), 0);
            }
        }
    }

    /// Held and dragged: stop on release. A quick click keeps scrolling until the next click.
    fn mup(&mut self) {
        if self.auto.as_ref().is_some_and(|a| a.moved || a.t0.elapsed() > Duration::from_millis(400)) {
            self.stop_auto();
        }
    }

    fn auto_tick(&mut self) {
        let Some(a) = &mut self.auto else { return };
        let now = Instant::now();
        let dt = (now - a.last).as_secs_f32().min(0.05);
        a.last = now;
        let mut pt = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut pt);
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut pt);
        }
        let d = (pt.y - a.y) as f32 / self.scale as f32;
        let e = if d.abs() < 12.0 { 0.0 } else { d - 12.0 * d.signum() };
        a.moved |= e != 0.0;
        let v = (e * e.abs() * 0.05 + e * 4.0) * self.scale as f32 * dt;
        self.scroll = (self.scroll + v).clamp(0.0, self.max_scroll());
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn paint(&mut self) {
        let (body, ctl) = self.rects();
        // decode the picture for this canvas and box (after a resize or a GPU reset, again)
        if let Some(png) = &self.png {
            let size = (body.right - body.left).max(body.bottom - body.top);
            if self.pic.as_ref().is_none_or(|p| p.3 != self.cv.gen || (p.4 != size && !self.live)) {
                self.pic = png_pixels(png, size).and_then(|(px, w, h)| self.cv.make_bitmap(&px, w, h).map(|b| (b, w, h, self.cv.gen, size)));
            }
        }
        if matches!(self.body, Body::Anim) && self.gif_bmp.as_ref().is_none_or(|b| b.1 != self.cv.gen) {
            if let Some((px, w, h, _)) = &self.gif {
                self.gif_bmp = self.cv.make_bitmap(px, *w, *h).map(|b| (b, self.cv.gen));
            }
        }
        if matches!(self.body, Body::Dir) {
            self.dir_icons(&body);
        }
        let layout = if matches!(self.body, Body::Text) { self.text_layout(body.right - body.left) } else { None };
        if !self.cv.begin() {
            return;
        }
        let (g, cv, lh, s) = (&self.gfx, &self.cv, self.gfx.f.lh, self.scale);
        let px = |v: i32| (v as f64 * s).round() as i32;
        let rad = |v: f64| (v * s) as f32;
        let c = self.card();
        // flush against the window's right edge: a panel, a hairline along its left (the divider strip is its own)
        cv.clear(theme().card);
        cv.fill(&RECT { left: 0, top: c.top, right: 1, bottom: c.bottom }, theme().line, 1.0);
        if self.grip > 0.0 {
            // the divider's grip: the middle quarter of the strip, lit by the main window (hover, drag)
            let (x, y, half) = (c.left / 2, (c.top + c.bottom) / 2, (c.bottom - c.top) / 8);
            cv.rrect_a(&RECT { left: x - px(2), top: y - half, right: x + px(2), bottom: y + half }, rad(2.0), Some(theme().accent), None, self.grip);
        }
        let bw = (body.right - body.left) as f32;
        if let Some(t) = g.layout(&w(&self.title), &g.f.semi, bw, lh as f32) {
            cv.text(&t, body.left, px(12), theme().text);
        }
        let detail = if self.cut { format!("{} · first {} KB shown", self.detail, TEXT_MAX / 1024) } else { self.detail.clone() };
        if let Some(t) = g.layout(&w(&detail), &g.f.reg, bw, lh as f32) {
            cv.text(&t, body.left, px(12) + lh, theme().muted);
        }
        let mid = |r: &RECT| (r.top + r.bottom - lh) / 2;
        match self.body {
            Body::Note(m) => put_mid(g, cv, m, &g.f.reg, &body, mid(&body), theme().muted),
            Body::Text => {
                if let Some(l) = &layout {
                    cv.clip(&body);
                    cv.text(l, body.left, body.top - self.scroll.round() as i32, theme().text);
                    cv.unclip();
                }
                if let Some(r) = self.thumb() {
                    let a = if self.drag == Some(Bar::Thumb) { 0.9 } else { 0.45 };
                    cv.rrect_a(&r, (r.right - r.left) as f32 / 2.0, Some(theme().muted), None, a);
                }
            }
            Body::Dir => {
                cv.clip(&body);
                if self.rows.is_empty() {
                    put_mid(g, cv, "(empty)", &g.f.reg, &body, body.top + px(8), theme().muted);
                }
                let (rh, off) = (self.row_h(), self.scroll.round() as i32);
                let first = (off / rh).max(0) as usize;
                for (i, e) in self.rows.iter().enumerate().skip(first) {
                    let top = body.top + i as i32 * rh - off;
                    if top >= body.bottom {
                        break;
                    }
                    let r = RECT { left: body.left, top, right: body.right - px(10), bottom: top + rh };
                    if self.hot == Some(i) {
                        cv.rrect(&r, rad(5.0), Some(theme().hl), None);
                    }
                    let ty = top + (rh - lh) / 2;
                    if let Some((bm, _)) = self.ent_bmp.get(&e.key()) {
                        let iy = top + (rh - px(16)) / 2;
                        cv.bitmap(bm, &RECT { left: r.left + px(6), top: iy, right: r.left + px(22), bottom: iy + px(16) }, 1.0);
                    }
                    let size = if e.dir { String::new() } else { super::fmt_size(e.size as i64) };
                    let sw = if size.is_empty() { 0 } else { g.layout(&w(&size), &g.f.reg, 1e4, 1e4).map_or(0, |t| gfx::metrics(&t).width.ceil() as i32) };
                    if sw > 0 {
                        put(g, cv, &size, &g.f.reg, r.right - px(8) - sw, ty, theme().muted);
                    }
                    cv.clip(&RECT { left: r.left + px(28), right: r.right - px(16) - sw, ..r });
                    put(g, cv, &e.name, &g.f.reg, r.left + px(30), ty, theme().text);
                    cv.unclip();
                }
                cv.unclip();
                if let Some(r) = self.thumb() {
                    let a = if self.drag == Some(Bar::Thumb) { 0.9 } else { 0.45 };
                    cv.rrect_a(&r, (r.right - r.left) as f32 / 2.0, Some(theme().muted), None, a);
                }
            }
            Body::Picture | Body::Media => {
                // the picture, contained and centered (for audio: the album art, else the file's icon)
                let bx = if self.note.is_empty() { body } else { RECT { bottom: body.bottom - 2 * lh, ..body } };
                if let Some((b, pw, ph, _, _)) = &self.pic {
                    if matches!(self.body, Body::Picture) || self.audio {
                        let k = ((bx.right - bx.left) as f64 / *pw as f64).min((bx.bottom - bx.top) as f64 / *ph as f64).min(1.0);
                        let (dw, dh) = ((*pw as f64 * k).round() as i32, (*ph as f64 * k).round() as i32);
                        let (x, y) = (bx.left + (bx.right - bx.left - dw) / 2, bx.top + (bx.bottom - bx.top - dh) / 2);
                        cv.bitmap(b, &RECT { left: x, top: y, right: x + dw, bottom: y + dh }, 1.0);
                    }
                }
                if !self.note.is_empty() {
                    put_mid(g, cv, self.note, &g.f.reg, &body, body.bottom - lh - px(4), theme().muted);
                }
            }
            Body::Handler => {}
            Body::Anim => {
                if let (Some((b, _)), Some((_, pw, ph, _))) = (&self.gif_bmp, &self.gif) {
                    let k = ((body.right - body.left) as f64 / *pw as f64).min((body.bottom - body.top) as f64 / *ph as f64).min(1.0);
                    let (dw, dh) = ((*pw as f64 * k).round() as i32, (*ph as f64 * k).round() as i32);
                    let (x, y) = (body.left + (body.right - body.left - dw) / 2, body.top + (body.bottom - body.top - dh) / 2);
                    cv.bitmap(b, &RECT { left: x, top: y, right: x + dw, bottom: y + dh }, 1.0);
                }
            }
        }
        if let Some(a) = &self.auto {
            let (x, y) = (a.x as f32, a.y as f32);
            cv.circle(x, y, rad(13.0), theme().card, 0.95);
            cv.circle_line(x, y, rad(13.0), theme().muted, 1.0);
            cv.chevron(x, y - rad(5.0), rad(4.0), false, theme().text, rad(1.5));
            cv.chevron(x, y + rad(5.0), rad(4.0), true, theme().text, rad(1.5));
        }
        if matches!(self.body, Body::Media) {
            // ▶ / ❚❚, the seek bar, the time, then the speaker (mute) and the volume
            let glyph = |s: &str, fallback: &str, r: &RECT, c: u32| match &self.icons {
                Some(f) => {
                    if let Some(t) = g.layout(&w(s), f, 1e4, 1e4) {
                        let m = gfx::metrics(&t);
                        cv.text(&t, r.left + (r.right - r.left - m.width.round() as i32) / 2, r.top + (r.bottom - r.top - m.height.round() as i32) / 2, c);
                    }
                }
                None => put_mid(g, cv, fallback, &g.f.reg, r, mid(r), c),
            };
            let bar = |t: &RECT, f: f64| {
                let y = (t.top + t.bottom) / 2;
                let x = t.left + ((t.right - t.left) as f64 * f.clamp(0.0, 1.0)) as i32;
                cv.rrect_a(&RECT { left: t.left, top: y - px(2), right: t.right, bottom: y + px(2) }, rad(2.0), Some(theme().muted), None, 0.35);
                cv.rrect(&RECT { left: t.left, top: y - px(2), right: x.max(t.left + px(4)), bottom: y + px(2) }, rad(2.0), Some(theme().accent), None);
                cv.circle(x as f32, y as f32, rad(6.0), theme().accent, 1.0);
            };
            let b = self.play_btn();
            cv.rrect(&b, rad(6.0), Some(theme().bg), Some(theme().line));
            let playing = self.playing();
            glyph(if playing { "\u{E769}" } else { "\u{E768}" }, if playing { "❚❚" } else { "▶" }, &b, theme().text);
            let t = self.track();
            let (pos, dur) = self.times().unwrap_or((0.0, 0.0));
            bar(&t, if dur > 0.0 { pos / dur } else { 0.0 });
            let time = if dur > 0.0 { format!("{} / {}", clock(pos), clock(dur)) } else { "…".into() };
            super::put(g, cv, &time, &g.f.reg, t.right + px(12), mid(&ctl), theme().muted);
            let v = if self.muted { 0.0 } else { self.vol };
            let speaker = if v <= 0.0 { "\u{E74F}" } else if v < 0.34 { "\u{E993}" } else if v < 0.67 { "\u{E994}" } else { "\u{E995}" }; // mute, volume 1–3
            glyph(speaker, if v == 0.0 { "×" } else { "♪" }, &self.mute_btn(), if self.muted { theme().muted } else { theme().text });
            bar(&self.vol_track(), v as f64);
        }
        self.cv.end();
    }
}

unsafe extern "system" fn preview_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_NCHITTEST => {
            // the divider strip: the main window handles it (hover, drag)
            let mut pt = windows_sys::Win32::Foundation::POINT { x: (l & 0xffff) as i16 as i32, y: ((l >> 16) & 0xffff) as i16 as i32 };
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(h, &mut pt);
            let gap = (GAP as f64 * windows_sys::Win32::UI::HiDpi::GetDpiForWindow(h) as f64 / 96.0).round() as i32;
            return if pt.x < gap { HTTRANSPARENT as LRESULT } else { HTCLIENT as LRESULT };
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(h, &mut ps);
            with(|p| p.paint());
            EndPaint(h, &ps);
            return 0;
        }
        WM_SIZE => {
            with(|p| p.relayout());
            return 0;
        }
        WM_TIMER if wp == T_MEDIA => {
            with(|p| p.tick());
            return 0;
        }
        WM_TIMER if wp == T_GIF => {
            with(|p| p.gif_tick());
            return 0;
        }
        WM_TIMER if wp == T_AUTO => {
            with(|p| p.auto_tick());
            return 0;
        }
        WM_ICON => {
            let p = l as *mut (String, Option<Vec<u8>>);
            if with(|p2| p2.got_icon(*Box::from_raw(p))).is_none() {
                PostMessageW(h, m, wp, l); // busy: try again in a moment
            }
            return 0;
        }
        WM_MOUSELEAVE => {
            with(|p| {
                p.tracking = false;
                if p.hot.take().is_some() {
                    InvalidateRect(p.hwnd, null(), 0);
                }
            });
            return 0;
        }
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT && with(|p| p.hot.is_some() && p.auto.is_none()) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_HAND));
            return 1;
        }
        WM_MOUSEWHEEL => {
            with(|p| p.wheel(((wp >> 16) & 0xffff) as i16 as i32));
            return 0;
        }
        WM_MBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|p| p.mdown(x, y));
            return 0;
        }
        WM_MBUTTONUP => {
            with(|p| p.mup());
            return 0;
        }
        WM_RBUTTONDOWN => {
            with(|p| p.stop_auto());
            return 0;
        }
        WM_SETCURSOR if with(|p| p.auto.is_some()) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_SIZENS));
            return 1;
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|p| {
                p.stop_auto();
                if p.sb_down(x, y) {
                    return;
                }
                p.press = p.row_at(x, y);
                if p.player.is_none() {
                    return;
                }
                let wide = |t: RECT| RECT { left: t.left - p.px(8), right: t.right + p.px(8), ..t };
                if inside(&p.play_btn(), x, y) {
                    p.toggle();
                } else if inside(&p.mute_btn(), x, y) {
                    p.mute();
                } else if let Some(b) = [(Bar::Seek, p.track()), (Bar::Volume, p.vol_track())].into_iter().find(|(_, t)| inside(&wide(*t), x, y)).map(|b| b.0) {
                    p.drag = Some(b);
                    SetCapture(h);
                    if b == Bar::Seek { p.seek(x) } else { p.volume(x) }
                }
            });
            return 0;
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(l);
            with(|p| match p.drag {
                Some(Bar::Seek) => p.seek(x),
                Some(Bar::Volume) => p.volume(x),
                Some(Bar::Thumb) => p.drag_thumb(y),
                None => {
                    let hot = p.row_at(x, y);
                    if hot != p.hot {
                        p.hot = hot;
                        InvalidateRect(p.hwnd, null(), 0);
                    }
                    if hot.is_some() && !p.tracking {
                        p.tracking = true;
                        let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: p.hwnd, dwHoverTime: 0 };
                        TrackMouseEvent(&mut t);
                    }
                }
            });
            return 0;
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            if m == WM_LBUTTONUP {
                let (x, y) = xy(l);
                // a click on the row that was pressed opens it, outside `with`: it may start a search, which re-enters the pane
                if let Some(path) = with(|p| p.press.take().filter(|&i| p.row_at(x, y) == Some(i)).map(|i| p.rows[i].path.clone())).flatten() {
                    super::open_item(&path);
                    return 0;
                }
            }
            if with(|p| p.drag.take().is_some()) == Some(true) {
                if m == WM_LBUTTONUP {
                    ReleaseCapture();
                }
                with(|p| unsafe { InvalidateRect(p.hwnd, null(), 0) }); // the thumb narrows again
            }
            if m == WM_CAPTURECHANGED {
                with(|p| {
                    if p.auto.take().is_some() {
                        KillTimer(p.hwnd, T_AUTO); // something else took the pointer
                        InvalidateRect(p.hwnd, null(), 0);
                    }
                });
            }
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

/// The video's window: the media engine draws into it; a click pauses or plays.
unsafe extern "system" fn video_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(h, &mut ps);
            EndPaint(h, &ps);
            return 0;
        }
        WM_LBUTTONDOWN => {
            with(|p| p.toggle());
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_told_from_binary_and_decoded() {
        assert!(looks_text(b"fn main() {}\n"));
        assert!(looks_text(&[0xff, 0xfe, b'h', 0, b'i', 0]));
        assert!(!looks_text(b"MZ\x90\0\x03\0"));
        assert_eq!(String::from_utf16_lossy(&decode(&[0xff, 0xfe, b'h', 0, b'i', 0], false)), "hi");
        assert_eq!(String::from_utf16_lossy(&decode("\u{feff}é".as_bytes(), false)), "é");
        assert_eq!(String::from_utf16_lossy(&decode(&[b'a', 0xe9], false)), "aé"); // not UTF-8: Latin-1
        assert_eq!(String::from_utf16_lossy(&decode(&"ab€".as_bytes()[..4], true)), "ab"); // cut inside the €
        assert_eq!(clock(75.0), "1:15");
        assert_eq!(clock(3725.0), "1:02:05");
    }
}
