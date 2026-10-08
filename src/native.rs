//! `--native`: the search window drawn with Direct2D + DirectWrite instead of a WebView, for tens of MB of RAM
//! instead of a browser engine. Same look (app.css is the spec), same backend (service pipe or in-process index),
//! same shell helpers.

#[path = "native_gfx.rs"]
mod gfx;
#[path = "native_side.rs"]
mod side;
#[path = "native_preview.rs"]
mod preview;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gfx::{Bitmap, Canvas, Format, Gfx, Text};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Globalization::{GetDateFormatEx, GetLocaleInfoEx, GetTimeFormatEx};
use windows_sys::Win32::Graphics::Dwm::{DwmFlush, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Controls::{ImageList_GetIcon, ImageList_GetIconSize, SetWindowTheme, WM_MOUSELEAVE};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::application::SearchResult;
use crate::backend::{SearchBackend, SearchHandle};
use crate::{ext_of, find, shell, type_cache, SearchArgs};

const PG: usize = 100; // rows per backend page
const KEEP: usize = 60; // pages kept in memory
const WM_RESULT: u32 = WM_APP + 1;
const WM_ICON: u32 = WM_APP + 2;
const WM_INSTALLED: u32 = WM_APP + 3;
const WM_THUMB: u32 = WM_APP + 4;
const WM_PREVIEW: u32 = WM_APP + 5;
const T_TYPE: usize = 1;
const T_POLL: usize = 2;
const T_FLASH: usize = 3;
const T_THUMB: usize = 4;
const T_PREVIEW: usize = 5;
const G: i32 = 6; // grid: gutter between a cell's edge and its clickable box
const SB: i32 = 12; // width kept free for the scrollbar at the list's right edge
const GLIDE: f64 = 0.11; // seconds a wheel notch takes to scroll
const SLIDE: f64 = 0.25; // seconds items take to slide to their new place after a live refresh
const GLOW: f64 = 1.8; // seconds a file that just appeared stays outlined
const MIN_SIZE: i32 = 32; // grid: smallest tile (CSS px)
const MAX_SIZE: i32 = 256; // and biggest: the shell's largest thumbnail
const ZOOM: f64 = 1.05; // tile size change per wheel notch or Ctrl +/−: smallest to biggest in ~43 steps
const THUMB_BYTES: usize = 24 << 20; // decoded thumbnails kept (GPU memory)

/// A palette of 0xRRGGBB colors. `Canvas`/`Gfx` take raw u32s per draw call, so swapping the
/// active theme needs no rendering-layer changes — every draw site reads through `theme()`.
#[derive(Clone, Copy)]
pub(super) struct Theme {
    bg: u32,
    card: u32,
    text: u32,
    muted: u32,
    accent: u32,
    green: u32, // temporary: the fps counter
    line: u32,
    hl: u32,
    row: u32,
    sel: u32,
}
impl Theme {
    // Dark: neutral graphite with one muted steel-blue accent (selection and hover are low-contrast tints of
    // it), so the content stands out rather than the chrome. Light: white, with the icon's blue as accent.
    const fn dark() -> Theme {
        Theme { bg: 0x0f1216, card: 0x181c22, text: 0xe6e8eb, muted: 0x8b93a0, accent: 0x7aa2ff, green: 0x3fb950, line: 0x262b33, hl: 0x1f2a44, row: 0x1c2129, sel: 0x243257 }
    }
    const fn light() -> Theme {
        // pure white, kept flat/uniform — separation comes from `line` borders, not grey fills
        Theme { bg: 0xffffff, card: 0xffffff, text: 0x1a1d21, muted: 0x6b7280, accent: 0x317dcf, green: 0x1a7f37, line: 0xd6dae1, hl: 0xe9f1fd, row: 0xf3f5f8, sel: 0xd7e6fb }
    }
    fn by_name(name: &str) -> Theme {
        match name {
            "light" => Theme::light(),
            _ => Theme::dark(), // "dark" and "default" both start identical to the original palette
        }
    }
}
static THEME: std::sync::RwLock<Theme> = std::sync::RwLock::new(Theme::dark());
static THEME_NAME: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());
pub(super) fn theme() -> Theme {
    *THEME.read().unwrap()
}
pub(super) fn theme_name() -> String {
    let n = THEME_NAME.read().unwrap();
    if n.is_empty() { "default".into() } else { n.clone() }
}
pub(super) fn set_theme(name: &str) {
    *THEME.write().unwrap() = Theme::by_name(name);
    *THEME_NAME.write().unwrap() = name.to_string();
}
/// Syncs the OS-drawn title bar (caption background/text, dark-mode caption-button glyphs) to the
/// active theme — DWM doesn't pick this up on its own when the theme changes after the window exists.
unsafe fn apply_titlebar(main: HWND) {
    let dark: BOOL = (theme_name() != "light") as i32;
    DwmSetWindowAttribute(main, 20, &dark as *const _ as *const c_void, 4); // DWMWA_USE_IMMERSIVE_DARK_MODE
    let caption = gdi(theme().card);
    DwmSetWindowAttribute(main, 35, &caption as *const _ as *const c_void, 4); // DWMWA_CAPTION_COLOR: title bar = header
    let text = gdi(theme().text);
    DwmSetWindowAttribute(main, 36, &text as *const _ as *const c_void, 4); // DWMWA_TEXT_COLOR
}
/// 0xRRGGBB as a GDI COLORREF
const fn gdi(c: u32) -> u32 {
    ((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff)
}

use crate::i18n::{t, t_plural};

/// The filter dropdown right of the search box, named and ordered like Everything's. A fn (not a
/// const) since the labels are resolved through `t()` against the active language at call time.
fn filters() -> [(&'static str, &'static str); 9] {
    [
        (t("filter.everything"), ""),
        (t("filter.audio"), "audio"),
        (t("filter.code"), "code"),
        (t("filter.compressed"), "archives"),
        (t("filter.document"), "documents"),
        (t("filter.executable"), "executables"),
        (t("filter.folder"), "folders"),
        (t("filter.image"), "images"),
        (t("filter.video"), "video"),
    ]
}
fn metas() -> [(&'static str, &'static str); 3] {
    [(t("meta.names"), ""), (t("meta.names_type"), "both"), (t("meta.type_only"), "only")]
}
fn col_defs() -> [(&'static str, &'static str); 6] {
    [
        (t("col.name"), "name"),
        (t("col.path"), "path"),
        (t("col.size"), "size"),
        (t("col.type"), "type"),
        (t("col.modified"), "mtime"),
        (t("col.attributes"), "attributes"),
    ]
}
// each language's own name for itself, not translated (a UI in Russian shouldn't call French "Французский")
fn lang_opts() -> [(&'static str, &'static str); 17] {
    [
        ("English", "en"),
        ("Español", "es"),
        ("العربية", "ar"),
        ("Português", "pt"),
        ("Français", "fr"),
        ("Kiswahili", "sw"),
        ("Hausa", "ha"),
        ("Yorùbá", "yo"),
        ("Igbo", "ig"),
        ("አማርኛ", "am"),
        ("isiZulu", "zu"),
        ("Soomaali", "so"),
        ("Wolof", "wo"),
        ("Afaan Oromoo", "om"),
        ("हिन्दी", "hi"),
        ("中文", "zh"),
        ("Русский", "ru"),
    ]
}
fn theme_opts() -> [(&'static str, &'static str); 2] {
    [(t("theme.dark"), "dark"), (t("theme.light"), "light")]
}
/// The classic File/Edit/View/Search/Tools/Help strip above the tab row, like a conventional desktop app's.
fn menu_bar_labels() -> [&'static str; 6] {
    [t("menubar.file"), t("menubar.edit"), t("menubar.view"), t("menubar.search"), t("menubar.tools"), t("menubar.help")]
}
fn menu() -> [(&'static str, &'static str, Option<Act>); 10] {
    [
        (t("menu.open"), "Enter", Some(Act::Open)),
        (t("menu.open_folder"), "", Some(Act::Reveal)),
        ("", "", None),
        (t("menu.copy"), "Ctrl+C", Some(Act::Copy)),
        (t("menu.cut"), "Ctrl+X", Some(Act::Cut)),
        (t("menu.copy_path"), "", Some(Act::Path)),
        ("", "", None),
        (t("menu.delete"), "Del", Some(Act::Del)),
        ("", "", None),
        (t("menu.properties"), "", Some(Act::Props)),
    ]
}

/// view, items per row and the Names/type mode survive restarts (the web UI kept them in localStorage).
/// JSON now (was a flat key=value text file); `load_settings` migrates the old file on first read.
fn settings_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("native.json")
}
fn legacy_settings_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("native.txt")
}
fn load_settings() -> HashMap<String, String> {
    if let Ok(s) = std::fs::read_to_string(settings_path()) {
        if let Ok(m) = serde_json::from_str(&s) {
            return m;
        }
    }
    std::fs::read_to_string(legacy_settings_path()).unwrap_or_default().lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
fn wz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
fn lc(c: u16) -> u16 {
    if c < 128 {
        (c as u8).to_ascii_lowercase() as u16
    } else {
        char::from_u32(c as u32).and_then(|ch| ch.to_lowercase().next()).filter(|l| (*l as u32) < 0x10000).map_or(c, |l| l as u16)
    }
}

// ---- selection: sorted, disjoint, non-adjacent inclusive ranges (Ctrl+A on 2M files is one entry) -------------
#[derive(Default, Clone, PartialEq)]
struct Sel(Vec<(usize, usize)>);
impl Sel {
    fn one(i: usize) -> Self {
        Sel(vec![(i, i)])
    }
    fn has(&self, i: usize) -> bool {
        let k = self.0.partition_point(|r| r.1 < i);
        k < self.0.len() && self.0[k].0 <= i
    }
    fn count(&self) -> usize {
        self.0.iter().map(|r| r.1 - r.0 + 1).sum()
    }
    fn add(&mut self, mut a: usize, mut b: usize) {
        let lo = self.0.partition_point(|r| r.1 + 1 < a);
        let hi = self.0.partition_point(|r| r.0 <= b + 1);
        if lo < hi {
            a = a.min(self.0[lo].0);
            b = b.max(self.0[hi - 1].1);
        }
        self.0.splice(lo..hi, [(a, b)]);
    }
    fn remove(&mut self, a: usize, b: usize) {
        let lo = self.0.partition_point(|r| r.1 < a);
        let hi = self.0.partition_point(|r| r.0 <= b);
        if lo >= hi {
            return;
        }
        let (first, last) = (self.0[lo].0, self.0[hi - 1].1);
        let mut keep = Vec::new();
        if first < a {
            keep.push((first, a - 1));
        }
        if last > b {
            keep.push((b + 1, last));
        }
        self.0.splice(lo..hi, keep);
    }
    fn toggle(&mut self, i: usize) {
        if self.has(i) {
            self.remove(i, i)
        } else {
            self.add(i, i)
        }
    }
    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().flat_map(|&(a, b)| a..=b)
    }
}

// ---- state --------------------------------------------------------------------------------------------------
struct Row {
    path: String,
    name: Vec<u16>,
    dir: Vec<u16>,
    size_s: Vec<u16>,
    ty: Vec<u16>,
    date: Vec<u16>,
    attrs_s: Vec<u16>,
    size: i64,
    is_dir: bool,
    sys: i32,     // system image list index of the type's icon
    own: bool,    // exe/lnk/ico/msi/scr: has its own icon, fetched by a shell helper
    tkey: String, // type key (extension, or "\0dir"): the grid's placeholder icon
}

/// A picture uploaded to the list's render target, with its size in pixels.
type Pic = (Bitmap, i32, i32);

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Tags,    // right of the search box: search inside files' metadata
    Content, // … and inside their text
    Filter, // the filter dropdown right of the search box
    Tag,    // status bar: the active filter's name; double-click turns it off
    Meta,   // Names / Names + type / Type only dropdown
    Sort,   // grid: sort dropdown
    Dir,    // grid: reverse order
    View(usize), // List | Grid
    Preview,     // status bar: the preview pane on / off (Alt+P)
    Smooth,      // status bar: smooth scrolling on / off (off: the wheel moves in steps)
    Browse(u8),  // status bar [Browse|Subfolders]: 1 / 2 lists a folder's contents / everything below it here; neither opens folders in Explorer (Alt+Up goes up)
    Side,        // status bar: the places panel at the left on / off (Alt+B)
    MatchCase,   // toolbar: case-sensitive search (Alt+M)
    WholeWord,   // toolbar: whole-word search (Alt+W)
    Regex,       // toolbar: regex search (Alt+E)
    Diacritics,  // toolbar: accent-insensitive search (Alt+D)
    Timestamps,  // toolbar: index created/accessed times for dc:/da: and their sort columns (Alt+T)
    Split,       // the gap between the list and the preview pane: drag to resize the pane
    Col(usize),
    Search,
    Lang,  // no button of its own now: names the language picker `drop_menu` opens from Tools
    Menu(usize), // the File/Edit/View/Search strip above the tab row
    Back,        // toolbar: previous folder / search (Alt+Left, mouse back button)
    Fwd,         // toolbar: next one (Alt+Right, mouse forward button)
}

enum Press {
    Item { i: usize, defer: bool, x: i32, y: i32 },
    Band { x: i32, y: f64, cx: i32, cy: i32, xor: bool, base: Sel, shown: bool },
}

struct Auto {
    x: i32,
    y: i32,
    cy: i32,
    t0: Instant,
    moved: bool,
}

#[derive(Clone, Copy)]
enum Act {
    Open,
    Reveal,
    Props,
    Copy,
    Cut,
    Path,
    Del,
}

struct Job {
    seq: u64,
    args: SearchArgs,
    h: SearchHandle,
}
type Done = (u64, SearchArgs, SearchResult);

struct Ui {
    main: HWND,
    edit: HWND,
    list: HWND,
    pane: HWND,      // the preview pane right of the list
    show_pv: bool,   // … shown
    side: HWND,      // the places panel left of the list
    show_side: bool, // … shown
    pv_key: String,  // what it was last asked to show (a path, or the note)
    pv_w: i32,       // … its width (CSS px) as dragged; 0 = 40% of the window
    split: Option<i32>, // dragging the divider left of the pane: the pointer's offset from its left edge
    split_x: Option<i32>, // … where the pointer moved to, applied on the next frame
    grip: RECT,         // the divider's grip: the part that can be dragged
    grip_a: f64,        // … how lit it is (fades)
    fps: (u32, Instant, u32), // temporary: animation frames since `.1`, and the last count per second
    fx: Option<Instant>, // a live refresh's animation: when it started
    slide: HashMap<String, (f64, f64)>, // item → where it was on screen before the refresh, from where it is now
    born: HashSet<String>, // items the refresh brought in: outlined for a moment
    scale: f64,
    gfx: Gfx,
    cm: Canvas, // main window
    cl: Canvas, // list
    edit_font: HFONT,
    backend: SearchBackend,
    jobs: mpsc::Sender<Job>,
    args: SearchArgs,  // what the controls say
    shown: SearchArgs, // what the rows on screen came from (pages are fetched with these)
    seq: u64,
    fresh: bool, // the pending result is a new search: reset scroll and selection when it lands
    typing: bool,
    terms: Vec<Vec<u16>>,
    matches: usize,
    objects: usize,
    changes: u64, // the index's change counter when the rows were fetched
    scanning: bool,
    error: Option<String>,
    offered: bool,
    pages: HashMap<usize, Vec<Row>>,
    order: VecDeque<usize>,
    y: f64,
    target: f64,
    frame: Instant,
    glide: (f64, Instant), // where the current smooth scroll started, and when
    sel: Sel,
    focus: Option<usize>,
    anchor: Option<usize>,
    hover: Option<usize>,
    tracking: bool,
    hot: Option<Hit>,
    press: Option<Press>,
    auto: Option<Auto>,
    flash: Option<(String, Instant)>,
    hits: Vec<(RECT, Hit)>,
    head_h: i32,
    menu_h: i32, // the File/Edit/View/Search strip above the tab row
    tab: RECT, // "Files"
    search_box: RECT,
    lh: RECT,
    status_top: i32, // the status bar along the bottom
    sys: HashMap<String, i32>,
    himl: isize,
    isz: i32,
    thousands: Vec<u16>,
    wheel_lines: f64,
    smooth: bool, // glide on wheel / scrollbar paging, else jump
    hist: std::cell::RefCell<(Vec<String>, usize)>, // folders / searches visited (browse_to) and where we are in them
    browse: u8, // 0: open folders in Explorer; 1: list what's directly inside (child:); 2: list everything below (descendant:)
    timestamps: bool, // index created/accessed times for dc:/da: and their sort columns (Alt+T)
    rtl: bool, // Arabic UI: columns and icon/text order mirror right-to-left (see `col_positions`)
    grid: bool,
    size: i32,  // grid: tile size (CSS px), the zoom level
    per: usize, // grid: tiles per row, as many as fit
    zoom_acc: i32, // Ctrl+wheel delta not yet a whole notch (precision touchpads)
    step_acc: i32, // likewise, for wheel steps with smooth scrolling off
    meta_gen: usize, // the metadata index's generation the results were last run against
    was_building: bool, // last poll()'s content/metadata-progress state, so poll() can repaint once more right as it finishes
    content_done: usize, // last poll()'s IndexStatus.content_done/content_total — see poll()'s comment
    content_total: usize,
    tip: HWND,
    tip_item: Option<usize>,
    tip_hit: Option<Hit>, // last hit shown by tool 2, the toolbar/status-bar tooltip (see `set_toolbar_tip`)
    sb_hot: bool,          // pointer over the scrollbar: drawn wide
    sb_drag: Option<i32>,  // dragging the thumb: where it was grabbed
    // things drawn with the list's render target; all dropped when it is recreated (`pics_gen`)
    pics_gen: u32,
    texts: HashMap<usize, Vec<Text>>, // laid-out text of the items on screen
    texts_key: (i32, bool, i32),
    sys_pics: HashMap<i32, Option<Pic>>,
    own: HashMap<String, Option<Pic>>,
    pending: HashSet<String>,
    thumbs: HashMap<String, (Option<Pic>, i32)>, // path → thumbnail and the tile size it was made for
    thumb_order: VecDeque<String>,
    thumb_bytes: usize,
    thumb_pending: HashSet<String>,
    jumbo_png: HashMap<String, Vec<u8>>,            // type key → the shell's 256 px icon
    jumbo: HashMap<(String, i32), Option<Pic>>,     // (type key, px) → decoded at tile size
}

thread_local!(static UI: RefCell<Option<Ui>> = const { RefCell::new(None) });

/// Run `f` on the UI state. `None` when it is already borrowed further up this thread's stack (a nested window
/// message): callers then fall back to default handling or re-post. Modal things (menus, message boxes, drags)
/// run outside `with`, so their message loops can reach the state.
fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

// ---- window identity: the two UIs get their own name, icon and taskbar button ---------------------------------
macro_rules! icon_set {
    ($n:literal) => {
        [
            (16, &include_bytes!(concat!("../icons/", $n, "-16.png"))[..]),
            (20, include_bytes!(concat!("../icons/", $n, "-20.png"))),
            (24, include_bytes!(concat!("../icons/", $n, "-24.png"))),
            (32, include_bytes!(concat!("../icons/", $n, "-32.png"))),
            (40, include_bytes!(concat!("../icons/", $n, "-40.png"))),
            (48, include_bytes!(concat!("../icons/", $n, "-48.png"))),
            (64, include_bytes!(concat!("../icons/", $n, "-64.png"))),
            (256, include_bytes!(concat!("../icons/", $n, "-256.png"))),
        ]
    };
}

/// Separate taskbar group for this process (both UIs are the same exe). Call before any window exists.
pub fn app_id(id: &str) {
    unsafe { windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(wz(id).as_ptr()) };
}

/// Title-bar and taskbar icons for `hwnd`, from the drawn PNG closest to each size at the window's DPI.
pub fn set_icons(hwnd: HWND, native: bool) {
    let set: [(i32, &[u8]); 8] = if native { icon_set!("native") } else { icon_set!("web") };
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let px = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(metric, dpi);
            let (_, png) = set.iter().find(|(s, _)| *s >= px).unwrap_or(&set[7]);
            let h = CreateIconFromResourceEx(png.as_ptr(), png.len() as u32, 1, 0x0003_0000, px, px, LR_DEFAULTCOLOR);
            if !h.is_null() {
                SendMessageW(hwnd, WM_SETICON, which as usize, h as isize);
            }
        }
    }
}

/// `mosaic.exe`: this exe with its own name and icon, so Task Manager and Explorer tell the two UIs apart.
/// Started without arguments it opens the native window. Made (or refreshed after a rebuild) by the web UI's start.
pub fn native_exe() -> std::io::Result<std::path::PathBuf> {
    stamped_helper("mosaic.exe", "Mosaic", icon_set!("native"))
}

/// Copy `src` to `out` with `name` as its description and its own icons (via a temp file, so a failed update
/// never leaves a half-written exe).
pub fn stamp(src: &std::path::Path, out: &std::path::Path, name: &str, set: [(i32, &[u8]); 8]) -> std::io::Result<()> {
    use windows_sys::Win32::System::LibraryLoader::*;
    let me = src;
    let tmp = out.with_extension("exe.tmp");
    std::fs::copy(me, &tmp)?;
    let err = || std::io::Error::last_os_error();
    unsafe {
        // the manifest is kept (common controls v6, asInvoker, long paths); version and icons are replaced
        let lib = LoadLibraryExW(wz(&me.to_string_lossy()).as_ptr(), null_mut(), LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE);
        let res = FindResourceW(lib, 1 as _, 24 as _); // RT_MANIFEST #1
        let manifest = if res.is_null() { Vec::new() } else { std::slice::from_raw_parts(LockResource(LoadResource(lib, res)) as *const u8, SizeofResource(lib, res) as usize).to_vec() };
        FreeLibrary(lib);
        let h = windows_sys::Win32::System::LibraryLoader::BeginUpdateResourceW(wz(&tmp.to_string_lossy()).as_ptr(), 1);
        if h.is_null() {
            return Err(err());
        }
        let put = |ty: u16, id: u16, data: &[u8]| UpdateResourceW(h, ty as _, id as _, 0, data.as_ptr() as _, data.len() as u32);
        if !manifest.is_empty() {
            put(24, 1, &manifest);
        }
        put(16, 1, &version_info(name)); // RT_VERSION
        let mut group = vec![0u8, 0, 1, 0, set.len() as u8, 0]; // GRPICONDIR: reserved, type 1 (icon), count
        for (k, (s, png)) in set.iter().enumerate() {
            put(3, k as u16 + 1, png); // RT_ICON (PNG-compressed, Vista+)
            group.extend_from_slice(&[(*s % 256) as u8, (*s % 256) as u8, 0, 0, 1, 0, 32, 0]);
            group.extend_from_slice(&(png.len() as u32).to_le_bytes());
            group.extend_from_slice(&(k as u16 + 1).to_le_bytes());
        }
        put(14, 32512, &group); // RT_GROUP_ICON: the id the window class loads
        if EndUpdateResourceW(h, 0) == 0 {
            return Err(err());
        }
    }
    std::fs::rename(&tmp, out) // fails while `out` is running: the caller retries or refreshes later
}

pub fn index_icons() -> [(i32, &'static [u8]); 8] {
    icon_set!("index")
}

/// `mosaic-content.exe`: the `--extract` IFilter helper, stamped with its own name and icon so a
/// burst of short-lived extraction subprocesses reads as "Mosaic Content Index" in Task Manager
/// instead of flickering under whatever launched it.
pub fn content_exe() -> std::io::Result<std::path::PathBuf> {
    stamped_helper("mosaic-content.exe", "Mosaic Content Index", icon_set!("content"))
}

/// `mosaic-thumbs.exe`: the `--shell-helper` thumbnail/icon workers (up to 4, one per worker
/// thread), stamped so they read as "Mosaic Thumbnails" instead of duplicating the GUI's name.
pub fn thumbs_exe() -> std::io::Result<std::path::PathBuf> {
    stamped_helper("mosaic-thumbs.exe", "Mosaic Thumbnails", icon_set!("thumbs"))
}

fn stamped_helper(file_name: &str, name: &str, set: [(i32, &[u8]); 8]) -> std::io::Result<std::path::PathBuf> {
    let me = std::env::current_exe()?;
    let out = me.with_file_name(file_name);
    let newer = |a: &std::path::Path, b: &std::path::Path| -> bool {
        let t = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        t(a).zip(t(b)).is_none_or(|(a, b)| a > b)
    };
    if !out.exists() || newer(&me, &out) {
        stamp(&me, &out, name, set)?;
    }
    Ok(out)
}

/// A VS_VERSIONINFO resource: Task Manager and Explorer show `FileDescription`.
fn version_info(name: &str) -> Vec<u8> {
    fn node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 2];
        b.extend_from_slice(&value_len.to_le_bytes());
        b.extend_from_slice(&(text as u16).to_le_bytes());
        b.extend(wz(key).iter().flat_map(|c| c.to_le_bytes()));
        let pad = |b: &mut Vec<u8>| b.resize(b.len().next_multiple_of(4), 0);
        pad(&mut b);
        b.extend_from_slice(value);
        if !children.is_empty() {
            pad(&mut b);
            b.extend_from_slice(children);
        }
        let n = b.len() as u16;
        b[..2].copy_from_slice(&n.to_le_bytes());
        pad(&mut b);
        b
    }
    let s = |k: &str, v: &str| {
        let v = wz(v);
        node(k, &v.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<_>>(), v.len() as u16, true, &[])
    };
    let v = env!("CARGO_PKG_VERSION");
    let strings: Vec<u8> = [s("CompanyName", "mosaic"), s("FileDescription", name), s("FileVersion", v), s("ProductName", name), s("ProductVersion", v)].concat();
    let table = node("000004b0", &[], 0, true, &strings);
    let sfi = node("StringFileInfo", &[], 0, true, &table);
    let vfi = node("VarFileInfo", &[], 0, true, &node("Translation", &[0, 0, 0xb0, 0x04], 4, false, &[]));
    let (maj, min, pat) = (env!("CARGO_PKG_VERSION_MAJOR").parse::<u32>().unwrap(), env!("CARGO_PKG_VERSION_MINOR").parse::<u32>().unwrap(), env!("CARGO_PKG_VERSION_PATCH").parse::<u32>().unwrap());
    let fixed: Vec<u8> = [0xFEEF04BD, 0x0001_0000, maj << 16 | min, pat << 16, maj << 16 | min, pat << 16, 0x3f, 0, 0x40004, 1, 0, 0, 0].iter().flat_map(|x: &u32| x.to_le_bytes()).collect();
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[sfi, vfi].concat())
}

// ---- entry ---------------------------------------------------------------------------------------------------
pub fn run() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        dark_menus();
        let inst = GetModuleHandleW(null());
        let cursor = LoadCursorW(null_mut(), IDC_ARROW);
        let icon = LoadIconW(inst, 32512 as *const u16); // the exe's icon (tauri-build embeds it under 32512)
        let (cm, cl) = (wz("MosaicNative"), wz("MosaicList"));
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = inst;
        wc.hCursor = cursor;
        wc.hIcon = icon;
        wc.lpfnWndProc = Some(main_proc);
        wc.lpszClassName = cm.as_ptr();
        wc.style = CS_DBLCLKS; // main: double-clicking the status bar's filter turns it off, as in Everything
        RegisterClassExW(&wc);
        wc.hIcon = null_mut();
        wc.lpfnWndProc = Some(list_proc);
        wc.lpszClassName = cl.as_ptr();
        RegisterClassExW(&wc);

        let s = GetDpiForSystem() as f64 / 96.0;
        let main = CreateWindowExW(0, cm.as_ptr(), wz("Mosaic").as_ptr(), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, CW_USEDEFAULT, CW_USEDEFAULT, (1400.0 * s) as i32, (900.0 * s) as i32, null_mut(), null_mut(), inst, null());
        set_icons(main, true);
        let edit = CreateWindowExW(0, wz("EDIT").as_ptr(), null(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, main, null_mut(), inst, null());
        SendMessageW(edit, 0x1501, 1, wz("Search file and folder names…").as_ptr() as isize); // EM_SETCUEBANNER
        SetWindowTheme(edit, wz("DarkMode_CFD").as_ptr(), null()); // dark caret/selection/context menu
        let list = CreateWindowExW(0, cl.as_ptr(), null(), WS_CHILD | WS_VISIBLE, 0, 0, 10, 10, main, null_mut(), inst, null());

        // the grid's jumbo type icons come from SHGetImageList, which needs COM on this thread
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        let tip = CreateWindowExW(WS_EX_TOPMOST, wz("tooltips_class32").as_ptr(), null(), 0x02 | 0x01, 0, 0, 0, 0, main, null_mut(), inst, null()); // TTS_NOPREFIX | TTS_ALWAYSTIP
        SetWindowTheme(tip, wz("DarkMode_Explorer").as_ptr(), null());
        let mut ti: windows_sys::Win32::UI::Controls::TTTOOLINFOW = zeroed();
        ti.cbSize = size_of::<windows_sys::Win32::UI::Controls::TTTOOLINFOW>() as u32;
        ti.uFlags = 0x10; // TTF_SUBCLASS: the tooltip watches the list's mouse messages itself
        ti.hwnd = list;
        ti.uId = 1;
        let mut empty = [0u16; 1];
        ti.lpszText = empty.as_mut_ptr();
        SendMessageW(tip, WM_USER + 50, 0, &ti as *const _ as isize); // TTM_ADDTOOLW
        SendMessageW(tip, WM_USER + 24, 0, 900); // TTM_SETMAXTIPWIDTH: allow the path/details line break

        // tool 2: the toolbar/status-bar buttons, in `main`'s own client coordinates (`Ui::set_toolbar_tip`
        // moves this tool's rect to whichever hit is hot, as it changes on WM_MOUSEMOVE)
        let mut ti2 = ti;
        ti2.hwnd = main;
        ti2.uId = 2;
        SendMessageW(tip, WM_USER + 50, 0, &ti2 as *const _ as isize); // TTM_ADDTOOLW

        let st = load_settings();
        let get = |k: &str| st.get(k).cloned().unwrap_or_default();
        set_theme(&get("theme"));
        apply_titlebar(main);
        if !get("lang").is_empty() {
            crate::i18n::set_lang(&get("lang"));
        }
        crate::datestore::set_enabled(get("timestamps") == "1");
        let scale = GetDpiForWindow(main) as f64 / 96.0;
        let pane = preview::create(main, scale);
        let side = side::create(main, scale);
        // language is fixed for the process's lifetime (set from settings above, no in-app switcher), so RTL fonts
        // can be built once here rather than needing a rebuild-on-switch path.
        let rtl = crate::i18n::lang() == "ar";
        let gfx = match Gfx::new(scale, rtl) {
            Ok(g) => g,
            Err(e) => {
                MessageBoxW(main, wz(&format!("Direct2D is not available: {e}")).as_ptr(), wz("Mosaic").as_ptr(), MB_ICONERROR);
                return;
            }
        };
        let backend = SearchBackend::detect();
        let args = SearchArgs {
            q: String::new(),
            cat: String::new(),
            sort: "name".into(),
            dir: "asc".into(),
            meta: get("meta"),
            offset: 0,
            limit: 0,
            tags: get("tags") == "1",
            content: get("content") == "1",
            match_case: get("match_case") == "1",
            whole_word: get("whole_word") == "1",
            regex: get("regex") == "1",
            diacritics: get("diacritics") == "1",
        };
        let mut lines: u32 = 3;
        SystemParametersInfoW(SPI_GETWHEELSCROLLLINES, 0, &mut lines as *mut _ as *mut c_void, 0);
        let mut ui = Ui {
            main,
            edit,
            list,
            pane,
            show_pv: get("preview") == "1",
            side,
            show_side: get("side") != "0", // on unless switched off
            pv_key: String::new(),
            pv_w: get("pvw").parse().unwrap_or(0),
            split: None,
            split_x: None,
            grip: zeroed(),
            grip_a: 0.0,
            fps: (0, Instant::now(), 0),
            fx: None,
            slide: HashMap::new(),
            born: HashSet::new(),
            scale,
            cm: Canvas::new(main, &gfx),
            cl: Canvas::new(list, &gfx),
            gfx,
            edit_font: edit_font(scale),
            backend,
            jobs: worker(main),
            shown: args.clone(),
            args,
            seq: 0,
            fresh: true,
            typing: false,
            terms: Vec::new(),
            matches: 0,
            objects: 0,
            changes: 0,
            scanning: false,
            error: None,
            offered: false,
            pages: HashMap::new(),
            order: VecDeque::new(),
            y: 0.0,
            target: 0.0,
            frame: Instant::now(),
            glide: (0.0, Instant::now()),
            sel: Sel::default(),
            focus: None,
            anchor: None,
            hover: None,
            tracking: false,
            hot: None,
            press: None,
            auto: None,
            flash: None,
            hits: Vec::new(),
            head_h: 0,
            menu_h: 0,
            tab: zeroed(),
            search_box: zeroed(),
            lh: zeroed(),
            status_top: 0,
            sys: HashMap::new(),
            himl: 0,
            isz: 16,
            thousands: thousands(),
            wheel_lines: lines.max(1) as f64,
            smooth: get("smooth") != "0",
            hist: Default::default(),
            browse: get("browse").parse().unwrap_or(0).min(2),
            timestamps: get("timestamps") == "1",
            rtl,
            grid: get("view") == "grid",
            size: get("size").parse().unwrap_or(128).clamp(MIN_SIZE, MAX_SIZE),
            per: 1, // set by layout
            zoom_acc: 0,
            step_acc: 0,
            meta_gen: 0,
            was_building: false,
            content_done: 0,
            content_total: 0,
            tip,
            tip_item: None,
            tip_hit: None,
            sb_hot: false,
            sb_drag: None,
            pics_gen: 0,
            texts: HashMap::new(),
            texts_key: (0, false, 0),
            sys_pics: HashMap::new(),
            own: HashMap::new(),
            pending: HashSet::new(),
            thumbs: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_bytes: 0,
            thumb_pending: HashSet::new(),
            jumbo_png: HashMap::new(),
            jumbo: HashMap::new(),
        };
        ui.sys_icon(&None, true); // loads the system image list
        if ui.himl != 0 {
            let (mut cx, mut cy) = (16, 16);
            ImageList_GetIconSize(ui.himl as _, &mut cx, &mut cy);
            ui.isz = cy;
        }
        SendMessageW(edit, WM_SETFONT, ui.edit_font as usize, 0);
        ui.run(false);
        UI.with(|u| *u.borrow_mut() = Some(ui));
        with(|u| u.layout());
        SetTimer(main, T_POLL, 500, None);
        ShowWindow(main, SW_SHOW);
        SetFocus(edit);
        pump();
    }
}

/// Messages, plus one animation step per displayed frame (DwmFlush waits for the next vsync) while something moves:
/// smooth wheel scrolling, middle-click autoscroll, or a selection band pushing past an edge. 60–144 Hz, whatever
/// the screen runs.
unsafe fn pump() {
    let mut msg: MSG = zeroed();
    loop {
        if with(|u| u.animating()).unwrap_or(false) {
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    return;
                }
                dispatch(&msg);
            }
            if let Some((list, pane, main)) = with(|u| {
                u.step();
                (u.list, u.pane, u.main)
            }) {
                // one frame: all three painted together (a divider drag moves them all)
                UpdateWindow(list);
                UpdateWindow(pane);
                UpdateWindow(main);
            }
            if DwmFlush() < 0 {
                std::thread::sleep(Duration::from_millis(7));
            }
        } else {
            if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
                return;
            }
            dispatch(&msg);
        }
    }
}

/// Opens `p` like double-click / Enter: with Browse on, a folder is listed in Mosaic; anything else (and every
/// folder with Browse off) goes to the shell, i.e. Explorer for a folder.
fn open_item(p: &str) {
    if with(|u| u.browse > 0).unwrap_or(false) && std::fs::metadata(p).is_ok_and(|m| m.is_dir()) {
        with(|u| u.browse_to(p));
    } else {
        shell::open(p);
    }
}

unsafe fn dispatch(msg: &MSG) {
    if msg.message == WM_SYSKEYDOWN && msg.wParam == VK_UP as usize && with(|u| u.browse_up()) == Some(true) {
        return; // Alt+Up: one folder up, as in Explorer
    }
    if msg.message == WM_SYSKEYDOWN && (msg.wParam == VK_LEFT as usize || msg.wParam == VK_RIGHT as usize) {
        let back = msg.wParam == VK_LEFT as usize;
        return with(|u| u.go_hist(back)).unwrap_or(());
    }
    if msg.message == WM_SYSKEYDOWN && msg.wParam == b'B' as usize {
        return toggle_side(); // Alt+B
    }
    if msg.message == WM_SYSKEYDOWN && msg.wParam == b'P' as usize {
        return toggle_preview(); // Alt+P, as in Explorer
    }
    if msg.message == WM_SYSKEYDOWN {
        let toggled: Option<fn(&mut Ui) -> &mut bool> = match msg.wParam as u8 as char {
            'M' => Some(|u| &mut u.args.match_case),
            'W' => Some(|u| &mut u.args.whole_word),
            'E' => Some(|u| &mut u.args.regex),
            'D' => Some(|u| &mut u.args.diacritics),
            _ => None,
        };
        if let Some(field) = toggled {
            with(|u| {
                let f = field(u);
                *f = !*f;
                u.save_settings();
                u.run(false);
            });
            return;
        }
        if msg.wParam as u8 as char == 'T' {
            with(|u| {
                u.timestamps = !u.timestamps;
                crate::datestore::set_enabled(u.timestamps);
                u.save_settings();
                u.run(false);
            });
            return;
        }
    }
    if msg.message == WM_MOUSEWHEEL {
        // the wheel goes to the focused window: scroll the preview's text when the pointer is over it
        let pt = POINT { x: (msg.lParam & 0xffff) as i16 as i32, y: ((msg.lParam >> 16) & 0xffff) as i16 as i32 };
        let under = WindowFromPoint(pt);
        if with(|u| u.pane).is_some_and(|p| under == p) {
            SendMessageW(under, msg.message, msg.wParam, msg.lParam);
            return;
        }
    }
    if msg.message == WM_KEYDOWN {
        let r = with(|u| (msg.hwnd == u.edit || msg.hwnd == u.list).then(|| u.key(msg.wParam as u16, msg.hwnd == u.edit)).flatten()).flatten();
        match r {
            Some(Some(a)) => return act(a),
            Some(None) => return,
            None => {}
        }
    }
    TranslateMessage(msg);
    DispatchMessageW(msg);
}

/// The preview pane on / off (status bar button, Alt+P). Off stops playback and lets go of the file.
unsafe fn toggle_side() {
    with(|u| {
        u.show_side = !u.show_side;
        u.save_settings();
        u.layout();
    });
}

/// A click in the places panel. Real folders are listed here when Browse is on (like a folder in the results);
/// everything else, and every folder with Browse off, opens in Explorer.
fn go_place(target: &str, mosaic: bool) {
    if let Some(spec) = target.strip_prefix("term:") {
        let dir = with(|u| u.term_dir()).unwrap_or_default();
        return shell::terminal(spec.starts_with("ps"), spec.ends_with("admin"), &dir);
    }
    if with(|u| (mosaic && u.browse > 0).then(|| u.browse_to(target))) == Some(Some(())) {
        return;
    }
    if target.starts_with("shell:") {
        shell::open_special(target);
    } else {
        shell::open(target);
    }
}

unsafe fn toggle_preview() {
    let on = with(|u| {
        u.show_pv = !u.show_pv;
        u.pv_key.clear();
        u.save_settings();
        u.layout();
        u.want_preview();
        u.show_pv
    });
    if on == Some(false) {
        preview::clear();
    }
}

/// Searches run here, newest first: typing never waits on a search, and superseded ones are skipped.
fn worker(main: HWND) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    let hw = main as isize;
    std::thread::spawn(move || {
        while let Ok(mut j) = rx.recv() {
            while let Ok(n) = rx.try_recv() {
                j = n;
            }
            let r = find(&j.args, 0, PG, &j.h);
            let done: Box<Done> = Box::new((j.seq, j.args, r));
            unsafe { PostMessageW(hw as HWND, WM_RESULT, 0, Box::into_raw(done) as isize) };
        }
    });
    tx
}

// ---- helpers -------------------------------------------------------------------------------------------------
/// The search box is a real EDIT control (caret, IME, undo, clipboard for free), so it keeps a GDI font.
unsafe fn edit_font(s: f64) -> HFONT {
    let mut lf: LOGFONTW = zeroed();
    lf.lfHeight = -((12.0 * s).round() as i32);
    lf.lfWeight = 400;
    lf.lfQuality = 5 as _; // CLEARTYPE_QUALITY
    for (d, c) in lf.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
        *d = c;
    }
    CreateFontIndirectW(&lf)
}

/// Undocumented but long-stable uxtheme calls (what Notepad++ and others use) that make the search box's own
/// right-click menu dark. Our menus are drawn by us (see `popup`).
unsafe fn dark_menus() {
    let ux = LoadLibraryW(wz("uxtheme.dll").as_ptr());
    if ux.is_null() {
        return;
    }
    if let Some(f) = GetProcAddress(ux, 135 as *const u8) {
        let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
        set_mode(2); // SetPreferredAppMode(ForceDark)
    }
    if let Some(f) = GetProcAddress(ux, 136 as *const u8) {
        let flush: unsafe extern "system" fn() = std::mem::transmute(f);
        flush(); // FlushMenuThemes
    }
}

/// RGBA (straight alpha) → premultiplied BGRA, box-filtered down to at most `size` px on the longer side.
fn pixels(px: &[u8], sw: usize, sh: usize, size: usize) -> (Vec<u8>, i32, i32) {
    let m = sw.max(sh).max(1);
    let (dw, dh) = if m > size { ((sw * size / m).max(1), (sh * size / m).max(1)) } else { (sw, sh) };
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        for x in 0..dw {
            let (x0, x1, y0, y1) = (x * sw / dw, ((x + 1) * sw / dw).max(x * sw / dw + 1), y * sh / dh, ((y + 1) * sh / dh).max(y * sh / dh + 1));
            let mut acc = [0u32; 4];
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let p = &px[(yy * sw + xx) * 4..][..4];
                    let a = p[3] as u32;
                    acc[0] += p[2] as u32 * a / 255; // B
                    acc[1] += p[1] as u32 * a / 255; // G
                    acc[2] += p[0] as u32 * a / 255; // R
                    acc[3] += a;
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            for c in 0..4 {
                out[(y * dw + x) * 4 + c] = (acc[c] / n) as u8;
            }
        }
    }
    (out, dw as i32, dh as i32)
}

/// A shell helper's PNG as premultiplied BGRA of at most `size` px.
fn png_pixels(png: &[u8], size: i32) -> Option<(Vec<u8>, i32, i32)> {
    let mut r = png::Decoder::new(png).read_info().ok()?;
    let mut px = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut px).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    Some(pixels(&px, info.width as usize, info.height as usize, size.max(1) as usize))
}

unsafe fn thousands() -> Vec<u16> {
    let mut b = [0u16; 8];
    let n = GetLocaleInfoEx(null(), 0x0F, b.as_mut_ptr(), 8); // LOCALE_STHOUSAND
    if n > 1 {
        b[..n as usize - 1].to_vec()
    } else {
        w(",")
    }
}

/// Left/right edges for a row of `flex` equal-share flexible columns followed by `fixed`-width ones,
/// packed left-to-right with `pad` on each side and `gap` between columns. In RTL, the whole row mirrors
/// (column 0 ends up flush against the right edge) while keeping each column's own width and reading-order
/// index — callers that already treat `out[k].0` as "column k's near edge" don't need a separate RTL branch.
fn col_positions(width: i32, pad: i32, gap: i32, flex: usize, fixed: &[i32], rtl: bool) -> Vec<(i32, i32)> {
    let n = flex + fixed.len();
    if n == 0 {
        return Vec::new();
    }
    let content = width - 2 * pad - (n as i32 - 1) * gap;
    let fr = ((content - fixed.iter().sum::<i32>()) / flex.max(1) as i32).max(0);
    let widths = (0..flex).map(|_| fr).chain(fixed.iter().copied());
    let mut x = pad;
    let out: Vec<(i32, i32)> = widths
        .map(|wd| {
            let r = (x, x + wd);
            x += wd + gap;
            r
        })
        .collect();
    if !rtl {
        return out;
    }
    let right = (x - gap).max(pad); // the row's far edge (last column's own right edge)
    out.iter().map(|&(a, b)| (pad + right - b, pad + right - a)).collect()
}

/// Explorer-style single-letter attribute codes: Read-only, Hidden, System, Archive.
fn fmt_attrs(attrs: u32) -> String {
    [(0x1, 'R'), (0x2, 'H'), (0x4, 'S'), (0x20, 'A')].iter().filter(|&&(bit, _)| attrs & bit != 0).map(|&(_, c)| c).collect()
}

fn fmt_size(n: i64) -> String {
    let mut v = n as f64;
    for u in ["B", "KB", "MB", "GB", "TB"] {
        if v < 1024.0 || u == "TB" {
            return match u {
                "B" => format!("{n} B"),
                "KB" => format!("{v:.0} KB"),
                _ => format!("{v:.1} {u}"),
            };
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// The user's short date + time, like Explorer and Everything show it.
fn fmt_date(ms: i64) -> Vec<u16> {
    use chrono::{Datelike, Timelike};
    let Some(t) = chrono::DateTime::from_timestamp_millis(ms).filter(|_| ms > 0) else { return Vec::new() };
    let t = t.with_timezone(&chrono::Local);
    let st = SYSTEMTIME { wYear: t.year() as u16, wMonth: t.month() as u16, wDayOfWeek: t.weekday().num_days_from_sunday() as u16, wDay: t.day() as u16, wHour: t.hour() as u16, wMinute: t.minute() as u16, wSecond: 0, wMilliseconds: 0 };
    let mut out = Vec::with_capacity(20);
    unsafe {
        let mut b = [0u16; 64];
        let n = GetDateFormatEx(null(), 1, &st, null(), b.as_mut_ptr(), 64, null()); // DATE_SHORTDATE
        out.extend_from_slice(&b[..(n.max(1) - 1) as usize]);
        out.push(b' ' as u16);
        let n = GetTimeFormatEx(null(), 2, &st, null(), b.as_mut_ptr(), 64); // TIME_NOSECONDS
        out.extend_from_slice(&b[..(n.max(1) - 1) as usize]);
    }
    out
}

/// The typed words to bold in names and paths (the web UI's S.terms).
fn terms(q: &str) -> Vec<Vec<u16>> {
    let re = regex::Regex::new(r#""[^"]*"|\S+"#).unwrap();
    re.find_iter(q)
        .map(|m| m.as_str())
        .filter(|t| !["ext:", "size:", "dm:", "date:"].iter().any(|p| t.to_lowercase().starts_with(p)))
        .map(|t| t.trim_matches('"'))
        .filter(|t| !t.is_empty())
        .map(|t| t.encode_utf16().map(lc).collect())
        .collect()
}

/// `s` split into (start, end, matched) runs.
fn runs(s: &[u16], terms: &[Vec<u16>]) -> Vec<(usize, usize, bool)> {
    let (mut out, mut i, mut start) = (Vec::new(), 0, 0);
    while i < s.len() {
        if let Some(t) = terms.iter().find(|t| i + t.len() <= s.len() && t.iter().zip(&s[i..]).all(|(a, b)| *a == lc(*b))) {
            if start < i {
                out.push((start, i, false));
            }
            out.push((i, i + t.len(), true));
            i += t.len();
            start = i;
        } else {
            i += 1;
        }
    }
    if start < s.len() {
        out.push((start, s.len(), false));
    }
    out
}

/// Draw `s` with its top-left at (x, y); returns its width.
fn put(g: &Gfx, cv: &Canvas, s: &str, f: &Format, x: i32, y: i32, c: u32) -> i32 {
    let Some(t) = g.layout(&w(s), f, 1e4, 1e4) else { return 0 };
    cv.text(&t, x, y, c);
    gfx::metrics(&t).widthIncludingTrailingWhitespace.ceil() as i32
}
/// Draw `s` centered in `r` horizontally, at `y`.
fn put_mid(g: &Gfx, cv: &Canvas, s: &str, f: &Format, r: &RECT, y: i32, c: u32) {
    let x = r.left + (r.right - r.left - g.width(s, f)) / 2;
    put(g, cv, s, f, x, y, c);
}

/// The view (grid?, tile size) one zoom step from `(grid, size)`.
fn zoom_step(grid: bool, size: i32, bigger: bool) -> (bool, i32) {
    match (grid, bigger) {
        (false, true) => (true, MIN_SIZE),
        (false, false) => (false, size),
        (true, false) if size <= MIN_SIZE => (false, size),
        (true, false) => (true, ((size as f64 / ZOOM).round() as i32).min(size - 1).max(MIN_SIZE)),
        (true, true) => (true, ((size as f64 * ZOOM).round() as i32).max(size + 1).min(MAX_SIZE)),
    }
}

fn xy(l: LPARAM) -> (i32, i32) {
    ((l & 0xffff) as i16 as i32, ((l >> 16) & 0xffff) as i16 as i32)
}
fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}
fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

// ---- the UI --------------------------------------------------------------------------------------------------
impl Ui {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }
    fn pf(&self, v: f64) -> f32 {
        (v * self.scale) as f32
    }
    fn rh(&self) -> i32 {
        if self.grid {
            self.tile() + self.px(2 * G + 50)
        } else {
            self.px(28)
        }
    }
    fn ncols(&self) -> usize {
        if self.grid {
            self.per
        } else {
            1
        }
    }
    /// Width of the list's content: everything left of the scrollbar.
    fn cw(&self) -> i32 {
        (self.list_size().0 - self.px(SB)).max(1)
    }
    /// Grid preview size: the zoom level, fixed whatever the window's width (unless the window is narrower).
    fn tile(&self) -> i32 {
        self.px(self.size).min(self.cw() - self.px(2 * G + 10)).max(self.px(16))
    }
    /// Thumbnail size asked of the shell (it pre-renders these): the smallest that covers the tile.
    fn tsize(&self) -> i32 {
        match self.tile() {
            t if t <= 64 => 64,
            t if t <= 128 => 128,
            _ => 256,
        }
    }
    /// x range of grid column `c` in whole pixels: a tile-sized cell centered in its share of the row, so spare
    /// width goes into the gaps, never into the tiles.
    fn cell_x(&self, c: usize) -> (i32, i32) {
        let slot = self.cw() as f64 / self.per.max(1) as f64;
        let w = self.tile() + self.px(2 * G + 10);
        let a = (c as f64 * slot + (slot - w as f64) / 2.0).round() as i32;
        (a, a + w)
    }
    fn list_size(&self) -> (i32, i32) {
        let mut r: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.list, &mut r) };
        (r.right, r.bottom)
    }

    // -- searching
    /// Lists what's directly inside `dir` (the search box gets `child:"dir"`, which then runs like any typed query).
    fn browse_to(&self, dir: &str) {
        let q = format!("{}:\"{}\"", if self.browse == 2 { "descendant" } else { "child" }, dir.trim_end_matches('\\'));
        self.visit(&q);
        unsafe { SetWindowTextW(self.edit, wz(&q).as_ptr()) };
    }

    /// Remember `q` as the newest place (dropping any "forward" ones), after the one we are leaving.
    fn visit(&self, q: &str) {
        let mut h = self.hist.borrow_mut();
        let (v, i) = &mut *h;
        if v.is_empty() {
            v.push(self.args.q.clone());
        } else if v[*i] != self.args.q {
            v.truncate(*i + 1);
            v.push(self.args.q.clone());
            *i += 1;
        }
        v.truncate(*i + 1);
        v.push(q.to_string());
        *i = v.len() - 1;
    }

    fn can_go(&self, back: bool) -> bool {
        let h = self.hist.borrow();
        let (v, i) = &*h;
        if back {
            *i > 0 || v.get(*i).is_some_and(|q| *q != self.args.q)
        } else {
            *i + 1 < v.len()
        }
    }

    /// Back / forward through the places visited. A search typed since the last move is kept as a place too.
    fn go_hist(&mut self, back: bool) {
        if !self.can_go(back) {
            return;
        }
        let q = {
            let mut h = self.hist.borrow_mut();
            let (v, i) = &mut *h;
            if back && v[*i] != self.args.q {
                v.truncate(*i + 1);
                v.push(self.args.q.clone());
                *i += 1;
            }
            *i = if back { *i - 1 } else { *i + 1 };
            v[*i].clone()
        };
        unsafe { SetWindowTextW(self.edit, wz(&q).as_ptr()) };
    }

    /// Browse off / contents / subfolders; a folder listing on screen is redone under the new mode.
    fn set_browse(&mut self, mode: u8) {
        self.browse = mode;
        self.save_settings();
        let q = self.args.q.trim().to_string();
        if let Some(d) = q.strip_prefix("child:\"").or_else(|| q.strip_prefix("descendant:\"")).and_then(|d| d.strip_suffix('"')) {
            if mode > 0 {
                self.browse_to(d);
            }
        }
        self.invalidate();
    }

    /// Where a terminal opens: the folder selected, else the one being browsed, else beside the selected file, else home.
    fn term_dir(&mut self) -> String {
        let sel = self.sel_paths();
        let is_dir = |p: &str| std::fs::metadata(p).is_ok_and(|m| m.is_dir());
        if let Some(d) = sel.iter().find(|p| is_dir(p)).filter(|_| sel.len() == 1) {
            return d.clone();
        }
        let q = self.args.q.trim();
        if let Some(d) = ["child:\"", "descendant:\""].iter().find_map(|p| q.strip_prefix(p)).and_then(|d| d.strip_suffix('"')) {
            return if d.ends_with(':') { format!("{d}\\") } else { d.to_string() };
        }
        if let Some(parent) = sel.first().and_then(|p| std::path::Path::new(p).parent()) {
            return parent.to_string_lossy().into_owned();
        }
        std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into())
    }

    /// Alt+Up while browsing a folder: its parent. False when the search box isn't showing a folder listing.
    fn browse_up(&self) -> bool {
        let Some(dir) = self.args.q.trim().strip_prefix("child:\"").or_else(|| self.args.q.trim().strip_prefix("descendant:\"")).and_then(|d| d.strip_suffix('"')) else { return false };
        let Some((parent, _)) = dir.rsplit_once('\\').filter(|(p, _)| !p.is_empty()) else { return false }; // already at a drive's top
        self.browse_to(parent);
        true
    }

    fn run(&mut self, keep: bool) {
        self.seq += 1;
        self.fresh |= !keep;
        self.terms = terms(&self.args.q);
        let _ = self.jobs.send(Job { seq: self.seq, args: self.args.clone(), h: self.backend.handle() });
    }

    fn done(&mut self, (seq, args, r): Done) {
        if seq != self.seq {
            return;
        }
        let fresh = std::mem::take(&mut self.fresh);
        // a live refresh keeps the selection on the same files: note where they were (up to 1000 of them)
        let at = |u: &Self, i: Option<usize>| i.and_then(|i| u.get(i).map(|r| (i, r.path.clone())));
        let kept: Vec<(usize, String)> = if fresh || self.sel.count() > 1000 { Vec::new() } else { self.sel.iter().filter_map(|i| at(self, Some(i))).collect() };
        let (focus, anchor) = (at(self, self.focus), at(self, self.anchor));
        // ...and the view on the same rows: the focused item if it's on screen, else the top one
        let (n, rh, h) = (self.ncols(), self.rh() as f64, self.list_size().1 as f64);
        let seen = |i: usize| (i / n) as f64 * rh + rh > self.y && ((i / n) as f64 * rh) < self.y + h;
        let pin = if fresh { None } else { focus.clone().filter(|(i, _)| seen(*i)).or_else(|| at(self, Some((self.y / rh) as usize * n))) };
        // ...and where every loaded item was drawn (mid-slide: where it is now), so the ones that still move glide
        let before: HashMap<String, (f64, f64)> = if fresh {
            HashMap::new()
        } else {
            self.pages.iter().flat_map(|(p, rows)| rows.iter().enumerate().map(move |(k, r)| (p * PG + k, r))).map(|(i, r)| {
                let (s, l) = (self.spot(i), self.lag(&r.path));
                (r.path.clone(), (s.0 + l.0, s.1 + l.1))
            }).collect()
        };
        let had: HashSet<usize> = self.pages.keys().copied().collect();
        let st = self.backend.status();
        self.objects = st.count;
        self.changes = st.changes;
        self.scanning = st.state == "indexing";
        self.error = r.error.clone().or((st.state == "error").then_some(st.message));
        self.matches = r.total;
        self.shown = args;
        self.pages.clear();
        self.order.clear();
        self.texts.clear();
        let rows = r.hits.into_iter().map(|h| self.row(h)).collect();
        self.put_page(0, rows);
        if fresh {
            self.y = 0.0;
            self.target = 0.0;
            self.sel = Sel::default();
            self.focus = None;
            self.anchor = None;
        } else {
            if !kept.is_empty() || focus.is_some() {
                let mut sel = Sel::default();
                for (i, p) in &kept {
                    if let Some(k) = self.relocate(*i, p) {
                        sel.add(k, k);
                    }
                }
                self.sel = sel;
                self.focus = focus.and_then(|(i, p)| self.relocate(i, &p));
                self.anchor = anchor.and_then(|(i, p)| self.relocate(i, &p));
            }
            // files come and go all the time: scroll along with the pinned row so nothing on screen jumps
            if let Some(d) = pin.and_then(|(i, p)| Some((i, self.relocate(i, &p)?))).map(|(i, k)| ((k / n) as f64 - (i / n) as f64) * rh) {
                self.y += d;
                self.target += d;
                self.glide.0 += d;
            }
        }
        self.clamp();
        // what's on screen now: slide what moved from where it was, outline what's new
        (self.fx, self.slide, self.born) = (None, HashMap::new(), HashSet::new());
        if !fresh && !self.scanning {
            self.prepare();
            let h = self.list_size().1 as f64;
            for i in self.on_screen() {
                let Some(p) = self.get(i).map(|r| r.path.clone()) else { continue };
                let now = self.spot(i);
                match before.get(&p) {
                    Some(&(x, y)) if (x, y) != now && (y - now.1).abs() < 2.0 * h => {
                        self.slide.insert(p, (x - now.0, y - now.1));
                    }
                    None if had.contains(&(i / PG)) => {
                        self.born.insert(p);
                    }
                    _ => {}
                }
            }
            if self.born.len() > 50 {
                self.born.clear(); // a folder unpacked or deleted: too many to point at
            }
            if !self.slide.is_empty() || !self.born.is_empty() {
                self.fx = Some(Instant::now());
            }
        }
        self.invalidate();
        self.want_preview();
    }

    /// Items on screen, by index.
    fn on_screen(&self) -> std::ops::Range<usize> {
        let (h, rh) = (self.list_size().1 as f64, self.rh() as f64);
        let (r0, r1) = ((self.y / rh).floor() as usize, ((self.y + h) / rh).floor() as usize);
        r0 * self.ncols()..((r1 + 1) * self.ncols()).min(self.matches)
    }
    /// Where item `i`'s box is drawn in the list: its left and top.
    fn spot(&self, i: usize) -> (f64, f64) {
        let n = self.ncols();
        (if self.grid { self.cell_x(i % n).0 as f64 } else { 0.0 }, (i / n) as f64 * self.rh() as f64 - self.y)
    }
    /// The outline around a file a live refresh brought in: closes in on the item as it fades in, holds, fades out.
    fn glow(&self, path: &str, bx: &RECT, rad: f32) {
        let Some(t) = self.fx.filter(|_| self.born.contains(path)).map(|t| t.elapsed().as_secs_f64()) else { return };
        let a = ((t / 0.2).min(1.0) * (1.0 - ((t - GLOW + 0.7) / 0.7).clamp(0.0, 1.0))) as f32;
        let g = ((1.0 - (t / 0.35).min(1.0)).powi(2) * 6.0 * self.scale).round() as i32;
        let r = RECT { left: (bx.left - g).max(0), top: bx.top - g, right: bx.right + g, bottom: bx.bottom + g };
        self.cl.rrect_a(&r, rad + g as f32, Some(theme().accent), None, 0.10 * a);
        self.cl.rrect_a(&r, rad + g as f32, None, Some(theme().accent), a);
        self.cl.rrect_a(&RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 }, rad + g as f32 - 1.0, None, Some(theme().accent), a);
    }
    /// How far from its place an item is still drawn while it slides there after a live refresh.
    fn lag(&self, path: &str) -> (f64, f64) {
        let (Some(t), Some(&(x, y))) = (self.fx, self.slide.get(path)) else { return (0.0, 0.0) };
        let f = (1.0 - (t.elapsed().as_secs_f64() / SLIDE).min(1.0)).powi(3); // ease out
        (x * f, y * f)
    }

    /// Where the file that was item `i` is now: a live refresh moves items a few places when files come and go
    /// before them. `None` when it's gone (or moved further than a page).
    fn relocate(&mut self, i: usize, path: &str) -> Option<usize> {
        let p = i / PG;
        for q in [p, p.saturating_sub(1), p + 1] {
            self.ensure(q);
            if let Some(k) = self.pages.get(&q).and_then(|rows| rows.iter().position(|r| r.path == path)) {
                return Some(q * PG + k);
            }
        }
        None
    }

    /// Twice a second: re-run the search when the index changed (live updates, indexing progress). `true` = offer
    /// the service install.
    fn poll(&mut self) -> bool {
        self.count_fps();
        let st = self.backend.status();
        if st.state == "error" && !self.offered {
            self.offered = true;
            return true;
        }
        // Metadata: results fill in as the metadata index reads files
        let meta = self.args.tags && crate::metadata::generation() != self.meta_gen;
        if (st.count != self.objects || st.changes != self.changes || meta) && !self.typing && self.press.is_none() {
            self.meta_gen = crate::metadata::generation();
            self.run(true);
        }
        // Cached off `st` (not re-read in `status()`'s text builder below): when a background
        // service does the actual indexing, this is the only way that progress reaches the GUI at
        // all, over the IPC status response — re-querying it per paint would mean a pipe round trip
        // on every repaint instead of once per poll tick.
        (self.content_done, self.content_total) = (st.content_done, st.content_total);
        // Neither bulk content-indexing nor the metadata reader bumps `st.count`/`st.changes`, so the
        // status bar's "building index" / ETA text above never gets a repaint to show it without this —
        // one extra, cheap `invalidate()` while either is running, and once more right after it finishes.
        let building = crate::content_progress(self.content_done, self.content_total).is_some() || (self.args.tags && crate::metadata_progress().is_some());
        if building || self.was_building {
            self.invalidate();
        }
        self.was_building = building;
        false
    }

    fn row(&mut self, h: crate::application::index::Hit) -> Row {
        let ext = if h.is_dir { None } else { ext_of(&h.name) };
        let own = matches!(ext.as_deref(), Some("exe" | "lnk" | "ico" | "msi" | "scr"));
        let (dir, name) = h.path.rsplit_once('\\').unwrap_or(("", &h.path));
        Row {
            name: w(name),
            dir: w(dir),
            size_s: if h.is_dir { Vec::new() } else { w(&fmt_size(h.size.max(0))) },
            ty: w(&type_cache(&ext, h.is_dir)),
            date: fmt_date(h.modified),
            attrs_s: w(&fmt_attrs(h.attributes)),
            sys: self.sys_icon(&ext, h.is_dir),
            own,
            tkey: if h.is_dir { "\0dir".into() } else { ext.clone().unwrap_or_default() },
            size: h.size.max(0),
            is_dir: h.is_dir,
            path: h.path,
        }
    }

    fn put_page(&mut self, p: usize, rows: Vec<Row>) {
        self.pages.insert(p, rows);
        self.order.retain(|&k| k != p);
        self.order.push_back(p);
        while self.order.len() > KEEP {
            let k = self.order.pop_front().unwrap();
            self.pages.remove(&k);
        }
    }

    /// Load page `p` now if missing: a page costs the service a few ms, so the rows for any scroll position are
    /// there in the frame that shows them (no blank rows, no placeholder flash).
    fn ensure(&mut self, p: usize) {
        if self.pages.contains_key(&p) || p * PG >= self.matches {
            return;
        }
        let r = find(&self.shown, p * PG, PG, &self.backend.handle());
        if !r.hits.is_empty() {
            let rows = r.hits.into_iter().map(|h| self.row(h)).collect();
            self.put_page(p, rows);
        }
    }

    fn get(&self, i: usize) -> Option<&Row> {
        self.pages.get(&(i / PG)).and_then(|p| p.get(i % PG))
    }

    fn sys_icon(&mut self, ext: &Option<String>, dir: bool) -> i32 {
        let key = if dir { "\0dir".to_string() } else { ext.clone().unwrap_or_default() };
        if let Some(&i) = self.sys.get(&key) {
            return i;
        }
        let name = if dir { wz("folder") } else { wz(&format!("x.{}", ext.as_deref().unwrap_or("dat"))) };
        let mut info: SHFILEINFOW = unsafe { zeroed() };
        // SYSICONINDEX | SMALLICON | USEFILEATTRIBUTES: registry only, never the disk
        let himl = unsafe { SHGetFileInfoW(name.as_ptr(), if dir { 0x10 } else { 0x80 }, &mut info, size_of::<SHFILEINFOW>() as u32, 0x4000 | 0x1 | 0x10) };
        if himl != 0 && self.himl == 0 {
            self.himl = himl as isize;
        }
        self.sys.insert(key, info.iIcon);
        info.iIcon
    }

    // -- pictures (bitmaps of the list's render target)
    /// Make sure the list's render target exists; if it was recreated, forget everything made with the old one.
    fn canvas_ready(&mut self) -> bool {
        let ok = self.cl.ensure();
        if self.cl.gen != self.pics_gen {
            self.pics_gen = self.cl.gen;
            self.texts.clear();
            self.sys_pics.clear();
            self.own.clear();
            self.thumbs.clear();
            self.thumb_order.clear();
            self.thumb_bytes = 0;
            self.jumbo.clear();
        }
        ok
    }
    fn upload(&mut self, px: &[u8], w: i32, h: i32) -> Option<Pic> {
        if !self.canvas_ready() {
            return None;
        }
        self.cl.make_bitmap(px, w, h).map(|b| (b, w, h))
    }

    /// The type's small icon from the system image list.
    fn sys_pic(&mut self, idx: i32) -> Option<Pic> {
        if let Some(p) = self.sys_pics.get(&idx) {
            return p.clone();
        }
        let pic = unsafe {
            let h = ImageList_GetIcon(self.himl as _, idx, 1); // ILD_TRANSPARENT
            let rgba = if h.is_null() { None } else { crate::infrastructure::icons::icon_rgba(h) };
            if !h.is_null() {
                DestroyIcon(h);
            }
            rgba.map(|(px, w, h)| pixels(&px, w as usize, h as usize, self.isz as usize))
        }
        .and_then(|(px, w, h)| self.upload(&px, w, h));
        self.sys_pics.insert(idx, pic.clone());
        pic
    }

    /// Ask a shell helper process for an exe/lnk's own icon (in-process shell parsing crashed on some extensions).
    fn want_icon(&mut self, path: &str) {
        if self.own.contains_key(path) || !self.pending.insert(path.to_string()) {
            return;
        }
        if self.own.len() > 3000 {
            self.own.clear();
        }
        let (p, hw, size) = (path.to_string(), self.main as isize, self.isz);
        shell::shell_spawn(Box::new(move |dropped| {
            let png = if dropped { None } else { shell::thumbnail(&p, size, true) };
            let b: Box<(String, Option<Vec<u8>>, bool)> = Box::new((p, png, dropped));
            unsafe { PostMessageW(hw as HWND, WM_ICON, 0, Box::into_raw(b) as isize) };
        }));
    }

    fn got_icon(&mut self, (p, png, dropped): (String, Option<Vec<u8>>, bool)) {
        self.pending.remove(&p);
        if dropped {
            return;
        }
        let pic = png.and_then(|b| png_pixels(&b, self.isz)).and_then(|(px, w, h)| self.upload(&px, w, h));
        self.own.insert(p, pic);
        unsafe { InvalidateRect(self.list, null(), 0) };
    }

    fn jumbo_clear(&mut self) {
        self.jumbo.clear();
    }

    /// The type's 256 px shell icon at `px`: a tile's picture until (or instead of) its thumbnail.
    fn jumbo_pic(&mut self, key: &str, px: i32) -> Option<Pic> {
        let k = (key.to_string(), px);
        if let Some(p) = self.jumbo.get(&k) {
            return p.clone();
        }
        if !self.jumbo_png.contains_key(key) {
            let dir = key == "\0dir";
            let png = crate::infrastructure::icons::jumbo_png((!dir && !key.is_empty()).then_some(key), dir).unwrap_or_default();
            self.jumbo_png.insert(key.to_string(), png);
        }
        let pic = png_pixels(&self.jumbo_png[key], px).and_then(|(b, w, h)| self.upload(&b, w, h));
        self.jumbo.insert(k, pic.clone());
        pic
    }

    /// Ask the shell helpers for thumbnails of the tiles on screen and one screen below.
    fn want_thumbs(&mut self) {
        if !self.grid || self.matches == 0 {
            return;
        }
        let (h, rh) = (self.list_size().1 as f64, self.rh() as f64);
        let r0 = (self.y / rh).floor() as usize;
        let r1 = ((self.y + 2.0 * h) / rh).floor() as usize;
        let (tile, ts, hw) = (self.tile(), self.tsize(), self.main as isize);
        let want: Vec<String> = (r0 * self.per..((r1 + 1) * self.per).min(self.matches))
            .filter_map(|i| self.get(i))
            .filter(|r| !r.is_dir && r.tkey != "svg" && !self.thumb_pending.contains(&r.path) && self.thumbs.get(&r.path).is_none_or(|t| t.1 != tile))
            .map(|r| r.path.clone())
            .collect();
        // shell_spawn runs the newest job first: queue bottom-up so the top-left tile comes first
        for p in want.into_iter().rev() {
            self.thumb_pending.insert(p.clone());
            shell::shell_spawn(Box::new(move |dropped| {
                let png = if dropped { None } else { shell::thumbnail(&p, ts, false) };
                let b: Box<(String, Option<Vec<u8>>, bool, i32)> = Box::new((p, png, dropped, tile));
                unsafe { PostMessageW(hw as HWND, WM_THUMB, 0, Box::into_raw(b) as isize) };
            }));
        }
    }

    fn got_thumb(&mut self, (p, png, dropped, tile): (String, Option<Vec<u8>>, bool, i32)) {
        self.thumb_pending.remove(&p);
        if dropped {
            return;
        }
        let pic = png.and_then(|b| png_pixels(&b, tile)).and_then(|(px, w, h)| self.upload(&px, w, h));
        let bytes = |p: &Option<Pic>| p.as_ref().map_or(0, |p| (p.1 * p.2 * 4) as usize);
        self.thumb_bytes += bytes(&pic);
        if let Some(old) = self.thumbs.insert(p.clone(), (pic, tile)) {
            self.thumb_bytes -= bytes(&old.0);
            self.thumb_order.retain(|k| *k != p); // ponytail: O(n) per re-decode, only after a tile size change
        }
        self.thumb_order.push_back(p);
        while self.thumb_bytes > THUMB_BYTES || self.thumb_order.len() > 20000 {
            let Some(k) = self.thumb_order.pop_front() else { break };
            if let Some(t) = self.thumbs.remove(&k) {
                self.thumb_bytes -= bytes(&t.0);
            }
        }
        unsafe { InvalidateRect(self.list, null(), 0) };
    }

    /// Grid tiles show the full path and details on hover (names are cut to two lines there).
    fn set_tip(&mut self) {
        let item = if self.grid && self.press.is_none() { self.hover } else { None };
        if item == self.tip_item {
            return;
        }
        self.tip_item = item;
        let mut ti: windows_sys::Win32::UI::Controls::TTTOOLINFOW = unsafe { zeroed() };
        ti.cbSize = size_of::<windows_sys::Win32::UI::Controls::TTTOOLINFOW>() as u32;
        ti.hwnd = self.list;
        ti.uId = 1;
        let mut text = vec![0u16];
        if let Some(i) = item {
            let (a, b) = self.cell_x(i % self.per);
            let top = ((i / self.per) as f64 * self.rh() as f64 - self.y).round() as i32;
            ti.rect = RECT { left: a, top, right: b, bottom: top + self.rh() };
            if let Some(r) = self.get(i) {
                let details: Vec<String> = [&r.ty, &r.size_s, &r.date].into_iter().filter(|s| !s.is_empty()).map(|s| String::from_utf16_lossy(s)).collect();
                text = wz(&format!("{}\n{}", r.path, details.join(" · ")));
            }
        }
        ti.lpszText = text.as_mut_ptr();
        unsafe {
            SendMessageW(self.tip, WM_USER + 52, 0, &ti as *const _ as isize); // TTM_NEWTOOLRECTW
            SendMessageW(self.tip, WM_USER + 57, 0, &ti as *const _ as isize); // TTM_UPDATETIPTEXTW
        }
    }

    /// A toolbar/status-bar button's hover tooltip: a fuller explanation of what it does, not just its
    /// visible label — translated via the `tip.*` i18n keys.
    fn tip_text(&self, h: Hit) -> String {
        match h {
            Hit::Lang => t("tip.lang").into(),
            Hit::Filter => t("tip.filter").into(),
            Hit::Meta => t("tip.meta").into(),
            Hit::Sort => t("tip.sort").into(),
            Hit::Preview => t("tip.preview").into(),
            Hit::Smooth => t("tip.smooth").into(),
            Hit::Browse(_) => t("tip.browse").into(),
            Hit::Back => t("tip.back").into(),
            Hit::Fwd => t("tip.forward").into(),
            Hit::Side => t("tip.sidebar").into(),
            Hit::Tags => t("tip.tags").into(),
            Hit::Content => t("tip.content").into(),
            Hit::MatchCase => t("tip.match_case").into(),
            Hit::WholeWord => t("tip.whole_word").into(),
            Hit::Regex => t("tip.regex").into(),
            Hit::Diacritics => t("tip.diacritics").into(),
            Hit::Timestamps => t("tip.timestamps").into(),
            Hit::View(1) => t("tip.grid_view").into(),
            Hit::View(_) => t("tip.list_view").into(),
            Hit::Dir => t("tip.dir").into(),
            Hit::Tag => t("tip.tag").into(),
            Hit::Col(k) => col_defs().get(k).map_or(String::new(), |m| t("tip.col").replace("{col}", m.0)),
            Hit::Split => t("tip.split").into(),
            Hit::Search => String::new(),
            Hit::Menu(i) => menu_bar_labels().get(i).copied().unwrap_or_default().into(),
        }
    }

    /// Toolbar/status-bar hover tooltip (tool 2): follows `self.hot`, set on every `WM_MOUSEMOVE` over `main`.
    fn set_toolbar_tip(&mut self) {
        if self.hot == self.tip_hit {
            return;
        }
        self.tip_hit = self.hot;
        let mut ti: windows_sys::Win32::UI::Controls::TTTOOLINFOW = unsafe { zeroed() };
        ti.cbSize = size_of::<windows_sys::Win32::UI::Controls::TTTOOLINFOW>() as u32;
        ti.hwnd = self.main;
        ti.uId = 2;
        let mut text = vec![0u16];
        if let Some(h) = self.hot {
            if let Some(&(r, _)) = self.hits.iter().find(|x| x.1 == h) {
                ti.rect = r;
                let s = self.tip_text(h);
                if !s.is_empty() {
                    text = wz(&s);
                }
            }
        }
        ti.lpszText = text.as_mut_ptr();
        unsafe {
            SendMessageW(self.tip, WM_USER + 52, 0, &ti as *const _ as isize); // TTM_NEWTOOLRECTW
            SendMessageW(self.tip, WM_USER + 57, 0, &ti as *const _ as isize); // TTM_UPDATETIPTEXTW
        }
    }

    // -- scrolling
    fn content_h(&self) -> f64 {
        self.matches.div_ceil(self.ncols()) as f64 * self.rh() as f64
    }
    fn clamp(&mut self) {
        let max = (self.content_h() - self.list_size().1 as f64).max(0.0);
        self.y = self.y.clamp(0.0, max);
        self.target = self.target.clamp(0.0, max);
    }
    fn scrolled(&mut self) {
        self.clamp();
        self.band_update();
        unsafe { InvalidateRect(self.list, null(), 0) };
    }
    fn animating(&self) -> bool {
        self.auto.is_some() || self.band_edge() != 0.0 || (self.target - self.y).abs() > 0.5 || self.fx.is_some() || self.split_x.is_some() || self.grip_a != self.grip_want()
    }
    /// The divider's grip is lit while the pointer is anywhere along the gap, or it's being dragged.
    fn grip_want(&self) -> f64 {
        if self.split.is_some() || self.hot == Some(Hit::Split) { 1.0 } else { 0.0 }
    }
    /// Dragging the divider: resize to where the pointer last was, once per frame. Done on every mouse move, the
    /// moves queued up behind the repaints and the divider trailed the pointer.
    fn drag_split(&mut self) {
        let (Some(off), Some(x)) = (self.split, self.split_x.take()) else { return };
        let mut c: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.main, &mut c) };
        let w = (((c.right - self.px(10) - (x - off) - self.px(8)) as f64 / self.scale).round() as i32).max(240);
        if w != self.pv_w {
            self.pv_w = w;
            self.layout();
        }
    }
    /// Band-select auto-scroll speed (px per 60 Hz frame) while the pointer is past the list's top or bottom edge.
    fn band_edge(&self) -> f64 {
        let Some(Press::Band { cy, shown: true, .. }) = &self.press else { return 0.0 };
        let h = self.list_size().1;
        let d = if *cy < 0 { *cy } else if *cy > h { cy - h } else { 0 };
        if d == 0 {
            return 0.0;
        }
        d.signum() as f64 * (d.abs() as f64 / 3.0 + 2.0).min(30.0) * self.scale
    }
    /// Temporary fps counter: animation frames per second, recounted every half second. Called from each frame
    /// (a timer waits behind them, so the count froze while scrolling) and from the poll (for idle: 0).
    fn count_fps(&mut self) {
        let t = self.fps.1.elapsed().as_secs_f64();
        if t < 0.5 {
            return;
        }
        let fps = (self.fps.0 as f64 / t).round() as u32;
        self.fps.0 = 0;
        self.fps.1 = Instant::now();
        if fps != self.fps.2 {
            self.fps.2 = fps;
            unsafe { InvalidateRect(self.main, &RECT { left: 0, top: self.status_top, right: 1 << 15, bottom: 1 << 15 }, 0) };
        }
    }
    fn step(&mut self) {
        self.count_fps();
        if let Some(t) = self.fx {
            if t.elapsed().as_secs_f64() > if self.born.is_empty() { SLIDE } else { GLOW } {
                (self.fx, self.slide, self.born) = (None, HashMap::new(), HashSet::new());
            }
            unsafe { InvalidateRect(self.list, null(), 0) };
        }
        let now = Instant::now();
        let dt = (now - self.frame).as_secs_f64().min(0.05);
        self.frame = now;
        self.fps.0 += 1;
        self.drag_split();
        let want = self.grip_want();
        if self.grip_a != want {
            // the divider's grip fades in (0.25 s) and out (0.35 s)
            let step = dt.min(1.0 / 30.0) / if want > self.grip_a { 0.25 } else { 0.35 };
            self.grip_a = if want > self.grip_a { (self.grip_a + step).min(want) } else { (self.grip_a - step).max(want) };
        }
        // drawn by the pane: fully lit for the whole drag, else eased (smoothstep)
        let a = self.grip_a;
        preview::grip(if self.split.is_some() { 1.0 } else { (a * a * (3.0 - 2.0 * a)) as f32 });
        let before = self.y;
        if let Some(a) = &self.auto {
            let d = (a.cy - a.y) as f64 / self.scale;
            let e = if d.abs() < 12.0 { 0.0 } else { d - 12.0 * d.signum() };
            self.y += (e * e.abs() * 0.05 + e * 4.0) * self.scale * dt;
            self.target = self.y;
        } else if self.band_edge() != 0.0 {
            self.y += self.band_edge() * dt * 60.0;
            self.target = self.y;
        } else {
            // a fixed-length glide that eases out and stops, like the browser's (an exponential ease has a long tail)
            let t = ((now - self.glide.1).as_secs_f64() / GLIDE).min(1.0);
            let (from, to) = (self.glide.0, self.target);
            self.y = from + (to - from) * (1.0 - (1.0 - t) * (1.0 - t));
        }
        self.clamp();
        if self.y != before {
            self.scrolled();
        } else if self.auto.is_none() {
            self.target = self.y; // pinned at an end: stop animating
        }
    }
    fn wheel(&mut self, delta: i32) {
        self.auto = None;
        if !self.smooth {
            // steps: one row (a tile row in the grid) per notch, landing on row edges
            self.step_acc += delta;
            let n = self.step_acc / 120;
            self.step_acc -= n * 120;
            if n != 0 {
                let rh = self.rh() as f64;
                self.glide_to(((self.y / rh).round() - n as f64) * rh);
            }
            return;
        }
        let line = if self.grid { self.px(40) } else { self.rh() } as f64; // a tile row per line would jump a whole screen
        self.glide_to(self.target - delta as f64 / 120.0 * self.wheel_lines * line);
    }
    /// Scroll smoothly to `t`, starting from wherever the view is now (or jump there, with smooth scrolling off).
    fn glide_to(&mut self, t: f64) {
        self.target = t;
        self.clamp();
        if !self.smooth {
            self.y = self.target;
            self.scrolled();
            return;
        }
        self.glide = (self.y, Instant::now());
        self.frame = Instant::now();
    }
    fn show_row(&mut self, n: usize) {
        let (h, rh) = (self.list_size().1 as f64, self.rh() as f64);
        let top = (n / self.ncols()) as f64 * rh;
        if top < self.y {
            self.y = top;
        } else if top + rh > self.y + h {
            self.y = top + rh - h;
        }
        self.target = self.y;
        self.scrolled();
    }

    // -- scrollbar (drawn by us: thin, widens under the pointer, like Edge's)
    /// (top, length) of the thumb, or None when everything fits.
    fn sb_thumb(&self) -> Option<(i32, i32)> {
        let h = self.list_size().1 as f64;
        let total = self.content_h();
        if total <= h + 0.5 {
            return None;
        }
        let len = (h * h / total).max(self.px(24) as f64).min(h);
        Some(((self.y / (total - h) * (h - len)).round() as i32, len.round() as i32))
    }
    fn on_sb(&self, x: i32) -> bool {
        x >= self.list_size().0 - self.px(SB)
    }
    /// A press on the scrollbar: grab the thumb, or page towards the pointer. False if not on the scrollbar.
    fn sb_down(&mut self, x: i32, y: i32) -> bool {
        if !self.on_sb(x) {
            return false;
        }
        if let Some((top, len)) = self.sb_thumb() {
            if y >= top && y < top + len {
                self.sb_drag = Some(y - top);
            } else {
                let h = self.list_size().1 as f64;
                self.glide_to(self.target + if y < top { -h } else { h });
            }
            unsafe { SetCapture(self.list) };
        }
        self.invalidate();
        true
    }
    fn sb_move(&mut self, y: i32) {
        let (Some(grab), Some((_, len))) = (self.sb_drag, self.sb_thumb()) else { return };
        let h = self.list_size().1 as f64;
        let t = ((y - grab) as f64 / (h - len as f64).max(1.0)).clamp(0.0, 1.0);
        self.y = t * (self.content_h() - h);
        self.target = self.y;
        self.scrolled();
    }

    // -- selection
    /// The item whose clickable box is at (x, y); the gaps around boxes start a selection band instead.
    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        if x >= self.cw() {
            return None;
        }
        let rh = self.rh() as f64;
        let pad = self.px(if self.grid { G } else { 2 }) as f64;
        let d = y as f64 + self.y;
        let r = (d / rh).floor();
        if d < 0.0 || d < r * rh + pad || d >= (r + 1.0) * rh - pad {
            return None;
        }
        let mut i = r as usize * self.ncols();
        if self.grid {
            let c = (0..self.per).find(|&c| {
                let (a, b) = self.cell_x(c);
                x >= a + self.px(G) && x < b - self.px(G)
            })?;
            i += c;
        }
        (i < self.matches).then_some(i)
    }
    fn set_sel(&mut self, s: Sel, focus: Option<usize>, anchor: Option<Option<usize>>) {
        self.sel = s;
        self.focus = focus;
        if let Some(a) = anchor {
            self.anchor = a;
        }
        self.invalidate();
        self.want_preview();
    }

    /// What the preview pane should show: the one selected file, else a note.
    fn pv_target(&self) -> preview::Target {
        use preview::Target::*;
        match self.sel.count() {
            0 => Note("Select a file to preview"),
            1 => match self.sel.iter().next().and_then(|i| self.get(i)) {
                None => Note(""),
                Some(r) => File {
                    path: r.path.clone(),
                    name: String::from_utf16_lossy(&r.name),
                    detail: if r.is_dir {
                        format!("{} · {}", String::from_utf16_lossy(&r.date), String::from_utf16_lossy(&r.dir))
                    } else {
                        format!("{} · {} · {}", String::from_utf16_lossy(&r.size_s), String::from_utf16_lossy(&r.date), String::from_utf16_lossy(&r.dir))
                    },
                },
            },
            _ => Note("Select one file to preview"),
        }
    }

    /// The selection changed: show it in the pane once it settles (arrowing through a list opens nothing on the way).
    fn want_preview(&mut self) {
        if !self.show_pv {
            return;
        }
        let key = match self.pv_target() {
            preview::Target::Note(m) => m.to_string(),
            preview::Target::File { path, .. } => path,
        };
        if key != self.pv_key {
            self.pv_key = key;
            unsafe { SetTimer(self.main, T_PREVIEW, 120, None) };
        }
    }
    fn click(&mut self, i: usize, ctrl: bool, shift: bool) {
        if let (true, Some(a)) = (shift, self.anchor) {
            let mut s = if ctrl { self.sel.clone() } else { Sel::default() };
            s.add(a.min(i), a.max(i));
            self.set_sel(s, Some(i), None);
        } else if ctrl {
            let mut s = self.sel.clone();
            s.toggle(i);
            self.set_sel(s, Some(i), Some(Some(i)));
        } else {
            self.set_sel(Sel::one(i), Some(i), Some(Some(i)));
        }
    }
    fn band_update(&mut self) {
        let w = self.cw();
        let (rh, pad, total, sy) = (self.rh() as f64, self.px(2) as f64, self.matches, self.y);
        let Some(Press::Band { x, y, cx, cy, xor, base, shown: true }) = &self.press else { return };
        let ey = *cy as f64 + sy;
        let (y0, y1, x0, x1) = (y.min(ey), y.max(ey), (*x).min(*cx), (*x).max(*cx));
        let mut res = base.clone();
        let mut first = None;
        let mut take = |a: usize, b: usize, res: &mut Sel| {
            if *xor {
                (a..=b).for_each(|i| res.toggle(i));
            } else {
                res.add(a, b);
            }
            first = Some(first.map_or(a, |f: usize| f.min(a)));
        };
        if self.grid && total > 0 {
            // only the clickable box of a tile counts, not its gutter
            let (g, rows) = (self.px(G) as f64, total.div_ceil(self.per));
            let (ra, rb) = ((y0 / rh).floor().max(0.0) as usize, ((y1 / rh).floor() as usize).min(rows - 1));
            let cs: Vec<usize> = (0..self.per)
                .filter(|&c| {
                    let (a, b) = self.cell_x(c);
                    x1 >= a + g as i32 && x0 < b - g as i32
                })
                .collect();
            if let (Some(&ca), Some(&cb)) = (cs.first(), cs.last()) {
                for r in ra..=rb {
                    if y1 >= r as f64 * rh + g && y0 <= (r + 1) as f64 * rh - g {
                        let (a, b) = (r * self.per + ca, (r * self.per + cb).min(total - 1));
                        if a <= b {
                            take(a, b, &mut res);
                        }
                    }
                }
            }
        } else if total > 0 && x1 >= 0 && x0 <= w {
            let hit = |r: f64| y1 >= r * rh + pad && y0 <= (r + 1.0) * rh - pad;
            let (ra, rb) = ((y0 / rh).floor().max(0.0), (y1 / rh).floor().min(total as f64 - 1.0));
            let a = if hit(ra) { ra } else { ra + 1.0 };
            let b = if hit(rb) { rb } else { rb - 1.0 };
            if a <= b {
                take(a as usize, b as usize, &mut res);
            }
        }
        let (f, an) = (first.or(self.focus), first.or(self.anchor));
        if (&res, f, an) != (&self.sel, self.focus, self.anchor) {
            self.set_sel(res, f, Some(an));
        }
    }
    fn move_to(&mut self, n: isize, extend: bool) {
        if self.matches == 0 {
            return;
        }
        let n = n.clamp(0, self.matches as isize - 1) as usize;
        match (extend, self.anchor) {
            (true, Some(a)) => self.set_sel(
                {
                    let mut s = Sel::default();
                    s.add(a.min(n), a.max(n));
                    s
                },
                Some(n),
                None,
            ),
            _ => self.set_sel(Sel::one(n), Some(n), Some(Some(n))),
        }
        self.show_row(n);
    }

    /// Keys for the list, also while typing in the search box (arrows, Enter…). `None` = not ours.
    fn key(&mut self, vk: u16, in_search: bool) -> Option<Option<Act>> {
        let (ctrl, shift) = (key_down(VK_CONTROL), key_down(VK_SHIFT));
        let i = self.focus.unwrap_or(0) as isize;
        let none = self.focus.is_none();
        let cols = self.ncols() as isize;
        let page = (self.list_size().1 / self.rh()).max(1) as isize * cols;
        let side = !in_search || ctrl; // in the search box, ←/→ move the caret
        let text_sel = in_search && {
            let (mut a, mut b) = (0u32, 0u32);
            unsafe { SendMessageW(self.edit, 0x00B0, &mut a as *mut _ as usize, &mut b as *mut _ as isize) }; // EM_GETSEL
            a != b
        };
        match vk {
            VK_DOWN => self.move_to(if none { 0 } else { i + cols }, shift),
            VK_UP => self.move_to(if none { 0 } else { i - cols }, shift),
            VK_RIGHT if self.grid && side => self.move_to(i + 1, shift),
            VK_LEFT if self.grid && side => self.move_to(i - 1, shift),
            VK_NEXT => self.move_to(i + page, shift),
            VK_PRIOR => self.move_to(i - page, shift),
            VK_HOME if !in_search => self.move_to(0, shift),
            VK_END if !in_search => self.move_to(self.matches as isize - 1, shift),
            VK_RETURN => {
                if self.sel.count() == 0 && self.matches > 0 {
                    self.set_sel(Sel::one(i as usize), Some(i as usize), Some(Some(i as usize)));
                }
                return Some(Some(Act::Open));
            }
            VK_ESCAPE => {
                if self.auto.take().is_some() {
                    unsafe { ReleaseCapture() };
                } else {
                    self.set_sel(Sel::default(), None, Some(None));
                }
            }
            VK_DELETE if !in_search => return Some(Some(Act::Del)),
            VK_BACK if in_search && ctrl => unsafe {
                // stock EDIT control has no word-delete of its own: Ctrl+Backspace otherwise just inserts a raw DEL char
                let (mut a, mut b) = (0u32, 0u32);
                SendMessageW(self.edit, 0x00B0, &mut a as *mut _ as usize, &mut b as *mut _ as isize); // EM_GETSEL
                let (mut start, end) = (a.min(b), a.max(b));
                if start == end {
                    let len = SendMessageW(self.edit, WM_GETTEXTLENGTH, 0, 0) as usize;
                    let mut buf = vec![0u16; len + 1];
                    SendMessageW(self.edit, WM_GETTEXT, buf.len(), buf.as_mut_ptr() as isize);
                    let text = &buf[..len];
                    let ws = |c: u16| matches!(c, 0x20 | 0x09 | 0x0A | 0x0D);
                    let mut i = start as usize;
                    while i > 0 && ws(text[i - 1]) {
                        i -= 1;
                    }
                    while i > 0 && !ws(text[i - 1]) {
                        i -= 1;
                    }
                    start = i as u32;
                }
                SendMessageW(self.edit, 0x00B1, start as usize, end as isize); // EM_SETSEL
                let repl: Vec<u16> = std::iter::once(0).collect();
                SendMessageW(self.edit, 0x00C2, 1, repl.as_ptr() as isize); // EM_REPLACESEL
            },
            0xBB | 0x6B if ctrl => self.zoom(true),  // Ctrl + (= key or numpad)
            0xBD | 0x6D if ctrl => self.zoom(false), // Ctrl −
            0x41 if ctrl && !in_search && self.matches > 0 => {
                let mut s = Sel::default();
                s.add(0, self.matches - 1);
                self.set_sel(s, self.focus, Some(Some(0)));
            }
            0x43 | 0x58 if ctrl && self.sel.count() > 0 && !text_sel => return Some(Some(if vk == 0x43 { Act::Copy } else { Act::Cut })),
            _ => return None,
        }
        Some(None)
    }

    /// Paths of the selection: from loaded rows when possible, else from the engine by index ranges.
    fn sel_paths(&mut self) -> Vec<String> {
        if self.sel.count() <= 500 {
            if let Some(v) = self.sel.iter().map(|i| self.get(i).map(|r| r.path.clone())).collect::<Option<Vec<_>>>() {
                return v;
            }
        }
        let mut out: Vec<String> = Vec::new();
        for &(a, b) in &self.sel.0.clone() {
            let n = (b + 1 - a).min(20000usize.saturating_sub(out.len()));
            if n == 0 {
                break;
            }
            out.extend(find(&self.shown, a, n, &self.backend.handle()).hits.into_iter().map(|h| h.path));
        }
        out
    }

    fn flash(&mut self, s: String) {
        self.flash = Some((s, Instant::now() + Duration::from_millis(3500)));
        unsafe {
            SetTimer(self.main, T_FLASH, 3600, None);
            InvalidateRect(self.main, null(), 0);
        }
    }
    fn invalidate(&self) {
        unsafe {
            InvalidateRect(self.list, null(), 0);
            InvalidateRect(self.main, null(), 0);
        }
    }

    fn num(&self, n: usize) -> String {
        let s = n.to_string();
        let sep = String::from_utf16_lossy(&self.thousands);
        let mut out = String::new();
        for (k, c) in s.chars().enumerate() {
            if k > 0 && (s.len() - k).is_multiple_of(3) {
                out += &sep;
            }
            out.push(c);
        }
        out
    }

    fn status(&self) -> String {
        if let Some((m, until)) = &self.flash {
            if Instant::now() < *until {
                return m.clone();
            }
        }
        // Everything's formats: "$o items", "$o items | Size: $s | Date Modified: $m | Path: $p", "$o items | $x items selected | $z"
        let items = |n: usize| crate::i18n::t_count("item", n as i64, &self.num(n));
        let mut t = items(self.matches);
        if self.scanning {
            t += &crate::i18n::t("status.indexing").replace("{items}", &items(self.objects));
        }
        let eta_suffix = |eta: Duration| {
            let s = eta.as_secs();
            match s {
                0..=119 => crate::i18n::t("status.eta_secs").replace("{s}", &s.to_string()),
                120..=5399 => crate::i18n::t("status.eta_min").replace("{m}", &(s / 60).to_string()),
                _ => crate::i18n::t("status.eta_hm").replace("{h}", &(s / 3600).to_string()).replace("{m}", &((s % 3600) / 60).to_string()),
            }
        };
        if let Some((done, all, eta)) = crate::metadata_progress().filter(|_| self.args.tags) {
            t += &crate::i18n::t("status.reading_metadata").replace("{done}", &self.num(done)).replace("{all}", &self.num(all));
            if let Some(eta) = eta {
                t += &eta_suffix(eta);
            }
        }
        if let Some((done, all, eta)) = crate::content_progress(self.content_done, self.content_total) {
            t += &crate::i18n::t("status.building_index").replace("{done}", &self.num(done)).replace("{all}", &self.num(all));
            if let Some(eta) = eta {
                t += &eta_suffix(eta);
            }
        }
        if let Some(e) = &self.error {
            t += &crate::i18n::t("status.error").replace("{e}", e);
        }
        let n = self.sel.count();
        if n == 1 {
            if let Some(r) = self.sel.iter().next().and_then(|i| self.get(i)) {
                if !r.is_dir {
                    t += &crate::i18n::t("status.size").replace("{size}", &String::from_utf16_lossy(&r.size_s));
                }
                t += &crate::i18n::t("status.date_path")
                    .replace("{date}", &String::from_utf16_lossy(&r.date))
                    .replace("{path}", &String::from_utf16_lossy(&r.dir));
            }
        } else if n > 1 {
            let (mut bytes, mut seen) = (0i64, 0usize);
            for (&p, rows) in &self.pages {
                for (k, r) in rows.iter().enumerate() {
                    if self.sel.has(p * PG + k) {
                        seen += 1;
                        if !r.is_dir {
                            bytes += r.size;
                        }
                    }
                }
            }
            t += &crate::i18n::t("status.selected").replace("{items}", &items(n));
            if bytes > 0 || seen < n {
                t += &format!(" | {}{}", fmt_size(bytes), if seen < n { "+" } else { "" });
            }
        }
        t
    }

    // -- layout (app.css in pixels: every number below is a CSS length times the DPI scale)
    /// Name, Path, Size, Type, Modified, Attributes column boxes, left-to-right reading order even in
    /// RTL (see `col_positions`) — callers index by column, not by screen side.
    fn cols(&self, width: i32) -> [(i32, i32); 6] {
        let (pad, gap) = (self.px(10), self.px(10));
        let fixed = [self.px(85), self.px(150), self.px(150), self.px(60)]; // Size, Type, Modified, Attributes
        let v = col_positions(width, pad, gap, 2, &fixed, self.rtl);
        [v[0], v[1], v[2], v[3], v[4], v[5]]
    }

    fn layout(&mut self) {
        self.layout_at(self.first_visible());
    }

    /// How many tiles of the chosen size fit across the list.
    fn fit_per(&self) -> usize {
        (self.cw() / (self.tile() + self.px(2 * G + 10))).max(1) as usize
    }

    fn layout_at(&mut self, first: usize) {
        unsafe {
            let mut c: RECT = zeroed();
            GetClientRect(self.main, &mut c);
            let (wd, ht) = (c.right, c.bottom);
            let (g, lh) = (&self.gfx, self.gfx.f.lh);
            let px = |v: i32| (v as f64 * self.scale).round() as i32;
            let mut hits = Vec::new();
            // header, as Everything's toolbar: logo, tabs, then the search box across the window and the filter
            // dropdown at its right, both compact (4px padding + 1px border, twice)
            let box_h = lh + px(10);
            let tab_h = lh + px(12);
            self.menu_h = lh + px(8);
            self.head_h = self.menu_h + tab_h + px(12) + 1;
            // the classic File/Edit/View/Search strip, flush against the top of the header
            let mut mx = px(10);
            for (i, label) in menu_bar_labels().iter().enumerate() {
                let w = g.width(label, &g.f.reg) + px(20);
                hits.push((RECT { left: mx, top: 0, right: mx + w, bottom: self.menu_h }, Hit::Menu(i)));
                mx += w;
            }
            let side = px(10);
            let mut x = side + g.width("Mosaic", &g.f.logo) + px(20);
            let ty = self.menu_h + (self.head_h - 1 - self.menu_h - tab_h) / 2;
            self.tab = RECT { left: x, top: ty, right: x + g.width(t("toolbar.files"), &g.f.semi) + px(24), bottom: ty + tab_h };
            x = self.tab.right + px(4);
            let select = |opts: &[(&str, &str)]| opts.iter().map(|o| g.width(o.0, &g.f.reg)).max().unwrap_or(0) + px(8) + px(26) + 2; // widest option + padding + arrow
            let top = self.menu_h + (self.head_h - 1 - self.menu_h - box_h) / 2;
            for h in [Hit::Back, Hit::Fwd] {
                hits.push((RECT { left: x, top, right: x + box_h + px(6), bottom: top + box_h }, h));
                x += box_h + px(8);
            }
            let filter = RECT { left: wd - side - select(&filters()).max(px(150)), top, right: wd - side, bottom: top + box_h };
            // Content | Metadata | Match case | Whole word | Regex | Diacritics | Timestamps toggles, packed
            // right to left between the search box and the filter. The search box keeps a minimum width no
            // matter how narrow the window gets: toggles that would eat into it are dropped (not drawn, not
            // hit-testable) rather than left to crush the box down to nothing.
            let seg = |s: &str| g.width(s, &g.f.reg) + px(20);
            let search_left = x - px(4) + px(16);
            let min_search = px(120);
            let toggles = [
                (Hit::Content, t("toolbar.content")),
                (Hit::Tags, t("toolbar.metadata")),
                (Hit::MatchCase, t("toolbar.match_case")),
                (Hit::WholeWord, t("toolbar.whole_word")),
                (Hit::Regex, t("toolbar.regex")),
                (Hit::Diacritics, t("toolbar.diacritics")),
                (Hit::Timestamps, t("toolbar.timestamps")),
            ];
            let mut right_edge = filter.left - px(6);
            for (hit, label) in toggles {
                let r = RECT { left: right_edge - seg(label), top, right: right_edge, bottom: top + box_h };
                if r.left < search_left + min_search + px(6) {
                    break;
                }
                hits.push((r, hit));
                right_edge = r.left - px(6);
            }
            self.search_box = RECT { left: search_left, top, right: right_edge.max(search_left + min_search), bottom: top + box_h };
            hits.push((self.search_box, Hit::Search));
            hits.push((filter, Hit::Filter));
            let sb = self.search_box;
            let mut tm: TEXTMETRICW = zeroed();
            let dc = GetDC(self.edit);
            let old = SelectObject(dc, self.edit_font);
            GetTextMetricsW(dc, &mut tm);
            SelectObject(dc, old);
            ReleaseDC(self.edit, dc);
            MoveWindow(self.edit, sb.left + px(8), sb.top + (box_h - tm.tmHeight) / 2, (sb.right - sb.left - px(16)).max(1), tm.tmHeight, 1);
            // status bar along the bottom (text on the left). At its right, like Explorer's and Everything's:
            // the active filter, then the view controls. Right to left: [List|Grid], 10, (grid: ▲, sort), names, 10, filter
            let bar_h = lh + px(12);
            self.status_top = ht - bar_h;
            let ctl_h = lh + px(4);
            let y = self.status_top + 1 + (bar_h - 1 - ctl_h) / 2;
            let mut x = wd - px(10);
            let mut push = |width: i32, h: Hit, gap: i32| {
                let r = RECT { left: x - width, top: y, right: x, bottom: y + ctl_h };
                hits.push((r, h));
                x = r.left - gap;
            };
            let seg = |s: &str| g.width(s, &g.f.reg) + px(20);
            push(seg(t("toolbar.grid")), Hit::View(1), -1); // the two segments share a border
            push(seg(t("toolbar.list")), Hit::View(0), px(10));
            push(seg(t("toolbar.preview")), Hit::Preview, px(6));
            push(seg(t("toolbar.smooth")), Hit::Smooth, px(6));
            push(seg(t("toolbar.subfolders")), Hit::Browse(2), -1); // like List | Grid, but either can be switched off again
            push(seg(t("toolbar.browse")), Hit::Browse(1), px(6));
            push(seg(t("toolbar.sidebar")), Hit::Side, px(10));
            if self.grid {
                push(g.width("▲", &g.f.reg) + px(20), Hit::Dir, px(6));
                push(select(&col_defs()), Hit::Sort, px(6));
            }
            push(select(&metas()), Hit::Meta, px(10));
            if let Some(f) = filters().iter().find(|f| !f.1.is_empty() && f.1 == self.args.cat) {
                push(g.width(f.0, &g.f.reg) + px(16), Hit::Tag, 0);
            }
            // column header (8px below the header; list view only), list, status bar; the preview pane at the right
            let lh_y = self.head_h + px(8);
            // the pane: as dragged (the list keeps 300 px), else 40% of the window. The list never gives up its
            // own minimum width for the pane's sake — on a narrow window the pane shrinks, then hides, first.
            let min_list = px(220);
            // the places panel, left of the list: it gives way first on a narrow window
            let side_w = if self.show_side && wd >= px(760) { px(side::WIDTH) } else { 0 };
            let left = px(10) + if side_w > 0 { side_w + px(8) } else { 0 };
            MoveWindow(self.side, px(10), lh_y, side_w.max(1), (self.status_top - px(4) - lh_y).max(1), 1);
            ShowWindow(self.side, if side_w > 0 { SW_SHOWNA } else { SW_HIDE });
            let max_pane = (wd - px(28) - min_list - (left - px(10))).max(0);
            let auto = ((wd as f64 * 0.4) as i32).clamp(px(240), px(960)).min(wd / 2).min(max_pane);
            let show_pane = self.show_pv && max_pane >= px(160);
            let pane_w = if !show_pane { 0 } else if self.pv_w > 0 { px(self.pv_w).min(max_pane).max(px(160)) } else { auto };
            let right = wd - px(10) - if show_pane { pane_w + px(8) } else { 0 };
            // the pane's window takes in the gap: it draws the divider's grip, which so moves with it
            MoveWindow(self.pane, right, lh_y, pane_w.max(1) + px(preview::GAP), (self.status_top - px(4) - lh_y).max(1), 1);
            ShowWindow(self.pane, if show_pane { SW_SHOWNA } else { SW_HIDE });
            if show_pane {
                let gap = RECT { left: right, top: lh_y, right: right + px(8), bottom: self.status_top - px(4) };
                let (mid, half) = ((gap.top + gap.bottom) / 2, (gap.bottom - gap.top) / 8);
                self.grip = RECT { top: mid - half, bottom: mid + half, ..gap };
                hits.push((gap, Hit::Split));
            }
            self.lh = RECT { left, top: lh_y, right: right - px(SB), bottom: lh_y + if self.grid { 0 } else { px(30) } };
            if !self.grid {
                let cols = self.cols(self.lh.right - self.lh.left);
                for (k, (a, b)) in cols.iter().enumerate() {
                    hits.push((RECT { left: self.lh.left + a, top: self.lh.top, right: self.lh.left + b, bottom: self.lh.bottom }, Hit::Col(k)));
                }
            }
            self.hits = hits;
            let top = self.lh.bottom;
            MoveWindow(self.list, left, top, (right - left).max(1), (self.status_top - px(4) - top).max(1), 1);
            // tiles keep their size: the window's width decides how many fit in a row. Keep the first item in view.
            let per = self.fit_per();
            if per != self.per {
                self.per = per;
                self.texts.clear();
                self.y = (first / self.ncols()) as f64 * self.rh() as f64;
                self.target = self.y;
            }
            self.clamp();
            InvalidateRect(self.main, null(), 0);
            InvalidateRect(self.list, null(), 0);
        }
    }

    fn save_settings(&self) {
        let p = settings_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let m: HashMap<&str, String> = HashMap::from([
            ("view", (if self.grid { "grid" } else { "list" }).to_string()),
            ("size", self.size.to_string()),
            ("meta", self.args.meta.clone()),
            ("preview", (self.show_pv as u8).to_string()),
            ("pvw", self.pv_w.to_string()),
            ("smooth", (self.smooth as u8).to_string()),
            ("browse", self.browse.to_string()),
            ("side", (self.show_side as u8).to_string()),
            ("tags", (self.args.tags as u8).to_string()),
            ("content", (self.args.content as u8).to_string()),
            ("match_case", (self.args.match_case as u8).to_string()),
            ("whole_word", (self.args.whole_word as u8).to_string()),
            ("regex", (self.args.regex as u8).to_string()),
            ("diacritics", (self.args.diacritics as u8).to_string()),
            ("timestamps", (self.timestamps as u8).to_string()),
            ("theme", theme_name()),
            ("lang", crate::i18n::lang()),
        ]);
        let _ = std::fs::write(p, serde_json::to_string(&m).unwrap_or_default());
    }

    /// UI language, live: text updates immediately (`t()` reads the active table on every call), and
    /// Arabic's right-to-left layout needs the DirectWrite formats rebuilt, same as a DPI change.
    fn set_lang(&mut self, code: &str) {
        crate::i18n::set_lang(code);
        self.rtl = code == "ar";
        if let Ok(g) = Gfx::new(self.scale, self.rtl) {
            self.cm = Canvas::new(self.main, &g);
            self.cl = Canvas::new(self.list, &g);
            self.gfx = g;
        }
        self.save_settings();
        side::relabel();
        self.layout();
        self.invalidate();
    }

    /// Color theme, live: `theme()` is a global read every draw call touches, so no rebuild is needed. The
    /// preview pane and the search edit box are their own windows though, so `invalidate` (main + list only)
    /// leaves them stale until something else repaints them — force them right away too.
    fn set_theme_choice(&mut self, name: &str) {
        set_theme(name);
        unsafe {
            apply_titlebar(self.main);
            InvalidateRect(self.edit, null(), 0);
        }
        preview::invalidate();
        side::invalidate();
        self.save_settings();
        self.invalidate();
    }

    /// Index of the first item on screen.
    fn first_visible(&self) -> usize {
        (self.y / self.rh() as f64) as usize * self.ncols()
    }

    /// Switch view or tile size, keeping the first visible item in view.
    fn reshape(&mut self, grid: bool, size: i32) {
        if grid == self.grid && size == self.size {
            return;
        }
        let first = self.first_visible();
        self.grid = grid;
        self.size = size;
        self.texts.clear();
        self.jumbo_clear(); // decoded at the old tile size
        self.layout_at(first);
        self.y = (first / self.ncols()) as f64 * self.rh() as f64;
        self.target = self.y;
        self.scrolled();
        self.set_tip();
        self.save_settings();
        self.invalidate();
    }

    /// Ctrl+wheel / Ctrl +/−: bigger or smaller icons, `ZOOM` a step. Like Explorer, zooming in from the list
    /// opens the grid at its smallest tiles, and zooming out past those goes back to the list.
    fn zoom(&mut self, bigger: bool) {
        let (grid, size) = zoom_step(self.grid, self.size, bigger);
        self.reshape(grid, size);
    }

    /// A toolbar click. Returns the target when it opens something modal (a menu, the Contents page).
    fn click_main(&mut self, x: i32, y: i32) -> Option<Hit> {
        let &(_, hit) = self.hits.iter().find(|(r, _)| inside(r, x, y))?;
        match hit {
            Hit::Filter | Hit::Meta | Hit::Sort | Hit::Preview | Hit::Side | Hit::Lang | Hit::Menu(_) => return Some(hit),
            Hit::Back | Hit::Fwd => self.go_hist(hit == Hit::Back),
            Hit::Tags | Hit::Content => {
                if hit == Hit::Tags {
                    self.args.tags = !self.args.tags;
                } else {
                    self.args.content = !self.args.content;
                }
                self.save_settings();
                self.run(false);
            }
            Hit::MatchCase | Hit::WholeWord | Hit::Regex | Hit::Diacritics => {
                match hit {
                    Hit::MatchCase => self.args.match_case = !self.args.match_case,
                    Hit::WholeWord => self.args.whole_word = !self.args.whole_word,
                    Hit::Regex => self.args.regex = !self.args.regex,
                    _ => self.args.diacritics = !self.args.diacritics,
                }
                self.save_settings();
                self.run(false);
            }
            Hit::Search => unsafe {
                SetFocus(self.edit);
            },
            Hit::Tag => return None, // double-click only
            Hit::Split => {
                if !inside(&self.grip, x, y) {
                    return None; // only the grip drags
                }
                self.split = Some(x - self.grip.left);
                self.grip_a = 1.0; // fully lit for the whole drag, even if it was still fading in
                preview::grip(1.0);
                unsafe { SetCapture(self.main) };
                preview::resizing(true);
                return None;
            }
            Hit::Dir => {
                self.args.dir = if self.args.dir == "desc" { "asc" } else { "desc" }.into();
                self.run(false);
            }
            Hit::Smooth => {
                self.smooth = !self.smooth;
                self.save_settings();
            }
            Hit::Timestamps => {
                self.timestamps = !self.timestamps;
                crate::datestore::set_enabled(self.timestamps);
                self.save_settings();
                self.run(false);
            }
            Hit::View(k) => self.reshape(k == 1, self.size),
            Hit::Browse(k) => self.set_browse(if self.browse == k { 0 } else { k }),
            Hit::Col(k) => {
                let s = col_defs()[k].1;
                let desc = if self.args.sort == s { self.args.dir != "desc" } else { s == "size" || s == "mtime" };
                self.args.sort = s.into();
                self.args.dir = if desc { "desc" } else { "asc" }.into();
                self.run(false);
            }
        }
        self.invalidate();
        None
    }

    // -- painting
    fn paint_main(&mut self) {
        if !self.cm.begin() {
            return;
        }
        let (g, cv, lh) = (&self.gfx, &self.cm, self.gfx.f.lh);
        let px = |v: i32| (v as f64 * self.scale).round() as i32;
        let rad = |v: f64| (v * self.scale) as f32;
        let mut c: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.main, &mut c) };
        cv.clear(theme().bg);
        // header
        cv.fill(&RECT { bottom: self.head_h - 1, ..c }, theme().card, 1.0);
        cv.fill(&RECT { top: self.head_h - 1, bottom: self.head_h, ..c }, theme().line, 1.0);
        // status bar
        cv.fill(&RECT { top: self.status_top, ..c }, theme().card, 1.0);
        cv.fill(&RECT { top: self.status_top, bottom: self.status_top + 1, ..c }, theme().line, 1.0);
        cv.fill(&RECT { top: self.menu_h, bottom: self.menu_h + 1, ..c }, theme().line, 1.0);
        let side = px(10);
        let logo_lh = (20.0 * 1.4 * self.scale).round() as i32;
        put(g, cv, "Mosaic", &g.f.logo, side, self.menu_h + (self.head_h - 1 - self.menu_h - logo_lh) / 2, theme().accent);
        let hot = |h: Hit| self.hot == Some(h);
        for &(r, h) in &self.hits {
            if let Hit::Menu(i) = h {
                if hot(h) {
                    cv.rrect(&r, rad(4.0), Some(theme().hl), None);
                }
                put_mid(g, cv, menu_bar_labels()[i], &g.f.reg, &r, r.top + (r.bottom - r.top - lh) / 2, theme().text);
            }
        }
        let files = self.tab;
        cv.rrect(&files, rad(8.0), Some(theme().hl), None);
        put(g, cv, t("toolbar.files"), &g.f.semi, files.left + px(12), files.top + px(6), theme().accent);
        // search box: a plain edit box, 1px border, accent when focused (a Windows edit, as in Everything)
        let sb = self.search_box;
        let focused = unsafe { GetFocus() } == self.edit;
        cv.rrect(&sb, rad(4.0), Some(theme().bg), Some(if focused { theme().accent } else { theme().line }));
        // filter dropdown, status bar filter and view controls
        for &(r, h) in &self.hits {
            let ty = r.top + (r.bottom - r.top - lh) / 2;
            let tc = if hot(h) { theme().text } else { theme().muted };
            match h {
                Hit::Filter | Hit::Meta | Hit::Sort => {
                    let label = match h {
                        Hit::Filter => filters().iter().find(|m| m.1 == self.args.cat).map_or(t("filter.everything"), |m| m.0),
                        Hit::Meta => metas().iter().find(|m| m.1 == self.args.meta).map_or(t("meta.names"), |m| m.0),
                        _ => col_defs().iter().find(|m| m.1 == self.args.sort).map_or(t("col.name"), |m| m.0),
                    };
                    cv.rrect(&r, rad(4.0), Some(theme().bg), Some(if hot(h) { theme().muted } else { theme().line }));
                    put(g, cv, label, &g.f.reg, r.left + px(9), ty, theme().text);
                    cv.chevron((r.right - px(13)) as f32, (r.top + r.bottom) as f32 / 2.0, rad(3.5), true, theme().text, rad(1.25));
                }
                Hit::Tag => {
                    if hot(h) {
                        cv.rrect(&r, rad(4.0), Some(theme().hl), None);
                    }
                    let name = filters().iter().find(|m| m.1 == self.args.cat).map_or("", |m| m.0);
                    put_mid(g, cv, name, &g.f.reg, &r, ty, theme().accent);
                }
                Hit::Back | Hit::Fwd => {
                    let on = self.can_go(h == Hit::Back);
                    if hot(h) && on {
                        cv.rrect(&r, rad(6.0), Some(theme().hl), None);
                    }
                    let c = if !on { theme().line } else if hot(h) { theme().accent } else { theme().text };
                    cv.arrow((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0, rad(8.0), h == Hit::Fwd, c, rad(2.0));
                }
                Hit::Dir => {
                    cv.rrect(&r, rad(4.0), Some(theme().card), Some(theme().line));
                    put_mid(g, cv, if self.args.dir == "desc" { "▼" } else { "▲" }, &g.f.reg, &r, ty, tc);
                }
                Hit::Preview | Hit::Smooth | Hit::Side | Hit::Tags | Hit::Content | Hit::MatchCase | Hit::WholeWord | Hit::Regex | Hit::Diacritics | Hit::Timestamps => {
                    let (on, label) = match h {
                        Hit::Preview => (self.show_pv, t("toolbar.preview")),
                        Hit::Smooth => (self.smooth, t("toolbar.smooth")),
                        Hit::Side => (self.show_side, t("toolbar.sidebar")),
                        Hit::Tags => (self.args.tags, t("toolbar.metadata")),
                        Hit::MatchCase => (self.args.match_case, t("toolbar.match_case")),
                        Hit::WholeWord => (self.args.whole_word, t("toolbar.whole_word")),
                        Hit::Regex => (self.args.regex, t("toolbar.regex")),
                        Hit::Diacritics => (self.args.diacritics, t("toolbar.diacritics")),
                        Hit::Timestamps => (self.timestamps, t("toolbar.timestamps")),
                        _ => (self.args.content, t("toolbar.content")),
                    };
                    cv.rrect(&r, rad(4.0), Some(if on { theme().hl } else { theme().card }), Some(if on { theme().accent } else { theme().line }));
                    put_mid(g, cv, label, &g.f.reg, &r, ty, if on { theme().accent } else { tc });
                }
                _ => {}
            }
        }
        // Browse | Subfolders: neither, one, never both
        for pass in [false, true] {
            for &(r, h) in &self.hits {
                let Hit::Browse(k) = h else { continue };
                let on = self.browse == k;
                if on != pass {
                    continue;
                }
                let radii = if k == 1 { [rad(4.0), 0.0, 0.0, rad(4.0)] } else { [0.0, rad(4.0), rad(4.0), 0.0] };
                cv.corners(&r, radii, if on { theme().hl } else { theme().card }, if on { theme().accent } else { theme().line });
                let ty = r.top + (r.bottom - r.top - lh) / 2;
                put_mid(g, cv, if k == 1 { t("toolbar.browse") } else { t("toolbar.subfolders") }, &g.f.reg, &r, ty, if on { theme().accent } else if hot(h) { theme().text } else { theme().muted });
            }
        }
        // List | Grid: the "off" segment first, so the accent border of the "on" one wins the shared edge
        let segs: Vec<(RECT, usize)> = self.hits.iter().filter_map(|&(r, h)| if let Hit::View(k) = h { Some((r, k)) } else { None }).collect();
        for pass in [false, true] {
            for &(r, k) in &segs {
                let on = self.grid == (k == 1);
                if on != pass {
                    continue;
                }
                let radii = if k == 0 { [rad(4.0), 0.0, 0.0, rad(4.0)] } else { [0.0, rad(4.0), rad(4.0), 0.0] };
                cv.corners(&r, radii, if on { theme().hl } else { theme().card }, if on { theme().accent } else { theme().line });
                let ty = r.top + (r.bottom - r.top - lh) / 2;
                put_mid(g, cv, if k == 1 { t("toolbar.grid") } else { t("toolbar.list") }, &g.f.reg, &r, ty, if on { theme().accent } else if hot(Hit::View(k)) { theme().text } else { theme().muted });
            }
        }
        // status text: left of the status bar's controls, cut with "…" where they begin
        let stop = self.hits.iter().filter(|x| x.0.top >= self.status_top).map(|x| x.0.left).min().unwrap_or(c.right);
        // temporary fps counter, left of the controls
        let fps = format!("{} fps", self.fps.2);
        let stop = stop - px(10) - g.width("000 fps", &g.f.reg);
        put(g, cv, &fps, &g.f.reg, stop, self.status_top + 1 + (c.bottom - self.status_top - 1 - lh) / 2, theme().green);
        if let Some(t) = g.layout(&w(&self.status()), &g.f.reg, (stop - px(20)) as f32, lh as f32) {
            cv.text(&t, px(10), self.status_top + 1 + (c.bottom - self.status_top - 1 - lh) / 2, theme().muted);
        }
        // column header
        if !self.grid {
            cv.rrect(&self.lh, rad(8.0), Some(theme().card), Some(theme().line));
            let base = |p: f64| {
                let (size, lh) = (p * self.scale, (p * 1.4 * self.scale).round());
                (lh - size * 2724.0 / 2048.0) / 2.0 + size * 2210.0 / 2048.0
            };
            for &(r, h) in &self.hits {
                let Hit::Col(k) = h else { continue };
                let ty = r.top + (r.bottom - r.top - lh) / 2;
                let cd = col_defs();
                let label = cd[k].0;
                let sorted = self.args.sort == cd[k].1;
                let arrow = if self.args.dir == "desc" { " ▼" } else { " ▲" };
                let aw = if sorted { g.width(arrow, &g.f.small) } else { 0 };
                // Size is the one right-aligned column; a trailing-edge alignment, so it flips sides with RTL
                // (trailing = right in LTR, left in RTL) exactly like the row body's `DWRITE_TEXT_ALIGNMENT_TRAILING`.
                let right_align = k == 2;
                let at_leading_edge = right_align == self.rtl;
                let x = if at_leading_edge { r.left } else { r.right - g.width(label, &g.f.reg) - aw };
                let x = x + put(g, cv, label, &g.f.reg, x, ty, if hot(h) { theme().text } else { theme().muted });
                if sorted {
                    put(g, cv, arrow, &g.f.small, x, ty + (base(12.0) - base(9.0)).round() as i32, theme().accent);
                }
            }
        }
        self.cm.end();
    }

    /// Fill in rows before drawing: pages and per-file icons for what is on screen.
    fn prepare(&mut self) {
        let (_, h) = self.list_size();
        let rh = self.rh() as f64;
        let r0 = (self.y / rh).floor() as usize;
        let r1 = ((self.y + h as f64) / rh).floor() as usize;
        let (i0, i1) = (r0 * self.ncols(), ((r1 + 1) * self.ncols()).min(self.matches));
        if i0 >= i1 {
            return;
        }
        for p in i0 / PG..=(i1 - 1) / PG {
            self.ensure(p);
        }
        if self.grid {
            return; // tiles show thumbnails (they include exe/lnk icons)
        }
        let want: Vec<String> = (i0..i1).filter_map(|i| self.get(i)).filter(|r| r.own && !self.own.contains_key(&r.path)).map(|r| r.path.clone()).collect();
        for p in want {
            self.want_icon(&p);
        }
    }

    /// Laid-out text of item `i`: [name, path, size, type, date] in the list, [label] in the grid.
    fn item_texts(&mut self, i: usize) -> Option<Vec<Text>> {
        if let Some(t) = self.texts.get(&i) {
            return Some(t.clone());
        }
        let row = self.get(i)?;
        let (g, lh) = (&self.gfx, self.gfx.f.lh as f32);
        let accent = self.cl.accent.clone();
        let hl = |t: &Text, s: &[u16]| {
            if let Some(b) = &accent {
                for (a, e, m) in runs(s, &self.terms) {
                    if m {
                        gfx::emphasize(t, a, e, b);
                    }
                }
            }
        };
        let out: Vec<Text> = if self.grid {
            let t = g.layout(&row.name, &g.f.label, self.tile() as f32, 2.0 * lh)?;
            hl(&t, &row.name);
            vec![t]
        } else {
            let cols = self.cols(self.cw());
            let width = |k: usize| (cols[k].1 - cols[k].0) as f32;
            let name = g.layout(&row.name, &g.f.reg, width(0) - self.pf(26.0), lh)?; // beside the 18px icon + 8px gap
            hl(&name, &row.name);
            let dir = g.layout(&row.dir, &g.f.reg, width(1), lh)?;
            hl(&dir, &row.dir);
            let size = g.layout(&row.size_s, &g.f.reg, width(2), lh)?;
            unsafe {
                let _ = size.SetTextAlignment(windows61::Win32::Graphics::DirectWrite::DWRITE_TEXT_ALIGNMENT_TRAILING);
            }
            vec![
                name,
                dir,
                size,
                g.layout(&row.ty, &g.f.reg, width(3), lh)?,
                g.layout(&row.date, &g.f.reg, width(4), lh)?,
                g.layout(&row.attrs_s, &g.f.reg, width(5), lh)?,
            ]
        };
        self.texts.insert(i, out.clone());
        Some(out)
    }

    fn paint_list(&mut self) {
        self.prepare();
        if !self.canvas_ready() || !self.cl.begin() {
            return;
        }
        let (wd, ht) = self.list_size();
        let cw = self.cw();
        let key = (cw, self.grid, self.tile());
        if key != self.texts_key {
            self.texts_key = key;
            self.texts.clear();
        }
        let (rh, lh, s) = (self.rh(), self.gfx.f.lh, self.scale);
        let rad = move |v: f64| (v * s) as f32;
        self.cl.clear(theme().bg);
        let r0 = (self.y / rh as f64).floor() as usize;
        let r1 = ((self.y + ht as f64) / rh as f64).floor() as usize;
        let (i0, i1) = (r0 * self.ncols(), ((r1 + 1) * self.ncols()).min(self.matches));
        for i in i0..i1.max(i0) {
            let texts = self.item_texts(i);
            let Some((is_dir, sys, own, path, tkey)) = self.get(i).map(|r| (r.is_dir, r.sys, r.own, r.path.clone(), r.tkey.clone())) else { continue };
            let color = if is_dir { theme().accent } else { theme().text };
            let (dx, dy) = self.lag(&path); // sliding to its new place after a live refresh
            let (dx, dy) = (dx.round() as i32, dy.round() as i32);
            if self.grid {
                let (a, b) = self.cell_x(i % self.per);
                let (a, b) = (a + dx, b + dx);
                let top = ((i / self.per) as f64 * rh as f64 - self.y).round() as i32 + dy;
                let g = self.px(G);
                let bx = RECT { left: a + g, top: top + g, right: b - g, bottom: top + rh - g };
                if self.sel.has(i) {
                    self.cl.rrect(&bx, rad(8.0), Some(theme().sel), Some(theme().accent));
                } else if self.hover == Some(i) {
                    self.cl.rrect(&bx, rad(8.0), Some(theme().row), None);
                }
                self.glow(&path, &bx, rad(8.0));
                let tile = self.tile();
                let pv = RECT { left: bx.left + self.px(5), top: bx.top + self.px(5), right: bx.left + self.px(5) + tile, bottom: bx.top + self.px(5) + tile };
                match self.thumbs.get(&path).and_then(|t| t.0.clone()) {
                    // the thumbnail: contained in the preview box, never enlarged past its own size
                    Some((bmp, w, h)) => {
                        let k = (tile as f64 / w as f64).min(tile as f64 / h as f64).min(self.scale);
                        let (dw, dh) = ((w as f64 * k).round() as i32, (h as f64 * k).round() as i32);
                        let (x, y) = (pv.left + (tile - dw) / 2, pv.top + (tile - dh) / 2);
                        self.cl.bitmap(&bmp, &RECT { left: x, top: y, right: x + dw, bottom: y + dh }, 1.0);
                    }
                    // until then, the type's jumbo icon filling the box (.pv img.ph)
                    None => {
                        if let Some((bmp, _, _)) = self.jumbo_pic(&tkey, tile.min(256)) {
                            self.cl.bitmap(&bmp, &pv, 1.0);
                        }
                    }
                }
                if let Some(t) = texts.as_ref().and_then(|t| t.first()) {
                    self.cl.text(t, pv.left, pv.bottom + self.px(6), color);
                }
            } else {
                let top = (i as f64 * rh as f64 - self.y).round() as i32 + dy;
                let bx = RECT { left: 0, top: top + self.px(2), right: cw, bottom: top + rh - self.px(2) };
                if self.sel.has(i) {
                    self.cl.rrect(&bx, rad(6.0), Some(theme().sel), None);
                } else if self.hover == Some(i) {
                    self.cl.rrect(&bx, rad(6.0), Some(theme().row), None);
                }
                self.glow(&path, &bx, rad(6.0));
                let cols = self.cols(cw);
                let ty = bx.top + (bx.bottom - bx.top - lh) / 2;
                // icon: the file's own (exe/lnk) or its type's, centered in an 18px box
                let pic = if own { self.own.get(&path).cloned().flatten() } else { None }.or_else(|| self.sys_pic(sys));
                // icon sits at the near edge of the Name column in reading order: the column's left in LTR,
                // its right in RTL (the column itself is already mirrored there by `cols()`/`col_positions`).
                let icon_x = if self.rtl { cols[0].1 - self.px(18) } else { cols[0].0 };
                if let Some((bmp, w, h)) = pic {
                    let bs = self.px(18);
                    let (x, y) = (icon_x + (bs - w) / 2, bx.top + (bx.bottom - bx.top - h) / 2);
                    self.cl.bitmap(&bmp, &RECT { left: x, top: y, right: x + w, bottom: y + h }, 1.0);
                }
                if let Some(t) = &texts {
                    let name_x = if self.rtl { cols[0].0 } else { cols[0].0 + self.px(26) };
                    let x = [name_x, cols[1].0, cols[2].0, cols[3].0, cols[4].0, cols[5].0];
                    for (k, t) in t.iter().enumerate() {
                        self.cl.text(t, x[k], ty, if k == 0 { color } else { theme().muted });
                    }
                }
            }
        }
        self.texts.retain(|&k, _| k >= i0 && k < i1);
        // selection band
        if let Some(Press::Band { x, y, cx, cy, shown: true, .. }) = &self.press {
            let by = (*y - self.y).round() as i32;
            let r = RECT { left: (*x).min(*cx), top: by.min(*cy), right: (*x).max(*cx), bottom: by.max(*cy) };
            self.cl.rrect_a(&r, rad(2.0), Some(theme().accent), None, 0.18);
            self.cl.rrect(&r, rad(2.0), None, Some(theme().accent));
        }
        // middle-click autoscroll anchor
        if let Some(a) = &self.auto {
            let (x, y) = (a.x as f32, a.y as f32);
            self.cl.circle(x, y, rad(13.0), theme().card, 0.95);
            self.cl.circle_line(x, y, rad(13.0), theme().muted, 1.0);
            self.cl.chevron(x, y - rad(5.0), rad(4.0), false, theme().text, rad(1.5));
            self.cl.chevron(x, y + rad(5.0), rad(4.0), true, theme().text, rad(1.5));
        }
        // scrollbar
        if let Some((top, len)) = self.sb_thumb() {
            let wide = self.sb_hot || self.sb_drag.is_some();
            if wide {
                self.cl.fill(&RECT { left: wd - self.px(SB), top: 0, right: wd, bottom: ht }, theme().text, 0.04);
            }
            let bw = self.px(if wide { 8 } else { 4 });
            let x1 = wd - self.px(2);
            let r = RECT { left: x1 - bw, top: top + self.px(2), right: x1, bottom: top + len - self.px(2) };
            let a = if self.sb_drag.is_some() { 0.9 } else if wide { 0.7 } else { 0.45 };
            self.cl.rrect_a(&r, bw as f32 / 2.0, Some(theme().muted), None, a);
        }
        self.cl.end();
        if self.grid && self.matches > 0 {
            unsafe { SetTimer(self.main, T_THUMB, 80, None) }; // thumbnails for tiles that stay on screen for a moment
        }
    }

    // -- list mouse
    fn ldown(&mut self, x: i32, y: i32) {
        if self.auto.take().is_some() {
            unsafe { ReleaseCapture() };
            self.invalidate();
            return;
        }
        if self.sb_down(x, y) {
            return;
        }
        let (ctrl, shift) = (key_down(VK_CONTROL), key_down(VK_SHIFT));
        unsafe {
            SetFocus(self.list);
            SetCapture(self.list);
        }
        self.hover = None;
        self.set_tip();
        match self.item_at(x, y) {
            Some(i) => {
                let mut defer = false;
                if ctrl || shift {
                    self.click(i, ctrl, shift);
                } else if !self.sel.has(i) {
                    self.set_sel(Sel::one(i), Some(i), Some(Some(i)));
                } else {
                    defer = true; // part of a multi-selection: keep it for dragging, collapse on a plain click
                }
                self.press = Some(Press::Item { i, defer, x, y });
            }
            None => {
                let base = if ctrl || shift { self.sel.clone() } else { Sel::default() };
                self.press = Some(Press::Band { x, y: y as f64 + self.y, cx: x, cy: y, xor: ctrl, base, shown: false });
            }
        }
        unsafe { InvalidateRect(self.main, null(), 0) };
    }

    /// Returns paths to drag out when a press on a selected item moves far enough.
    fn mmove(&mut self, x: i32, y: i32) -> Option<Vec<String>> {
        if !self.tracking {
            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.list, dwHoverTime: 0 };
            unsafe { TrackMouseEvent(&mut t) };
            self.tracking = true;
        }
        if self.sb_drag.is_some() {
            self.sb_move(y);
            return None;
        }
        let sb_hot = self.press.is_none() && self.auto.is_none() && self.on_sb(x);
        if sb_hot != self.sb_hot {
            self.sb_hot = sb_hot;
            unsafe { InvalidateRect(self.list, null(), 0) };
        }
        if let Some(a) = &mut self.auto {
            a.cy = y;
            if (y - a.y).abs() > 12 {
                a.moved = true;
            }
            return None;
        }
        let hov = if self.press.is_none() { self.item_at(x, y) } else { None };
        if hov != self.hover {
            self.hover = hov;
            self.set_tip();
            unsafe { InvalidateRect(self.list, null(), 0) };
        }
        let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
        match &mut self.press {
            Some(Press::Item { x: px, y: py, .. }) if (x - *px).abs() > dx || (y - *py).abs() > dy => {
                self.press = None;
                unsafe { ReleaseCapture() };
                Some(self.sel_paths())
            }
            Some(Press::Band { x: bx, y: by, cx, cy, shown, .. }) => {
                *cx = x;
                *cy = y;
                if !*shown && ((x - *bx).pow(2) as f64 + (y as f64 + self.y - *by).powi(2)).sqrt() >= 5.0 {
                    *shown = true;
                    self.frame = Instant::now();
                }
                self.band_update();
                unsafe { InvalidateRect(self.list, null(), 0) };
                None
            }
            _ => None,
        }
    }

    fn lup(&mut self) {
        if self.sb_drag.take().is_some() {
            unsafe {
                ReleaseCapture();
                InvalidateRect(self.list, null(), 0);
            }
            return;
        }
        match self.press.take() {
            Some(Press::Item { i, defer: true, .. }) => self.set_sel(Sel::one(i), Some(i), Some(Some(i))),
            Some(Press::Band { shown: false, base, .. }) if base.0.is_empty() && !key_down(VK_CONTROL) && !key_down(VK_SHIFT) => {
                self.set_sel(Sel::default(), None, Some(None)) // click on empty space clears
            }
            _ => {}
        }
        unsafe {
            ReleaseCapture();
            InvalidateRect(self.list, null(), 0);
        }
    }
}

// ---- popup menus: drawn like the web page's (.menu), rounded and shadowed by the compositor --------------------
struct Menu {
    items: Vec<(String, String, bool)>, // label ("" = separator), shortcut, checked
    rows: Vec<RECT>,
    hover: Option<usize>,
    done: Option<Option<usize>>,
    canvas: Canvas,
    gfx: Gfx,
    scale: f64,
    gutter: i32, // room at the left for the checkmarks
}
thread_local!(static POPUP: RefCell<Option<Menu>> = const { RefCell::new(None) });
// While a menu-bar menu is open, the other menu-bar buttons (screen rects): hovering one switches to its menu
thread_local!(static BAR: RefCell<Vec<(RECT, usize)>> = const { RefCell::new(Vec::new()) });
thread_local!(static SWITCH: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) });

fn finish(v: Option<usize>) {
    POPUP.with(|m| {
        if let Some(m) = m.borrow_mut().as_mut() {
            m.done.get_or_insert(v);
        }
    });
}

/// Show a menu at screen point (x, y), at least `min_w` wide, and run it until it is closed. Returns the index of
/// the chosen item. Where it doesn't fit below, it ends at `above` instead (the top of the button that opened it).
/// Keys and clicks elsewhere are the menu's while it is open, like a Win32 menu.
unsafe fn popup(owner: HWND, x: i32, y: i32, above: i32, min_w: i32, checks: bool, items: Vec<(String, String, bool)>) -> Option<usize> {
    let scale = GetDpiForWindow(owner) as f64 / 96.0;
    let gfx = Gfx::new(scale, crate::i18n::lang() == "ar").ok()?;
    let px = |v: i32| (v as f64 * scale).round() as i32;
    let lh = gfx.f.lh;
    let label_w = items.iter().map(|i| gfx.width(&i.0, &gfx.f.reg)).max().unwrap_or(0);
    let key_w = items.iter().map(|i| gfx.width(&i.1, &gfx.f.reg)).max().unwrap_or(0);
    let pad = px(4) + 1; // padding inside the 1px border
    let gutter = if checks { px(22) } else { 0 };
    let wd = min_w.max(px(210)).max(2 * pad + px(20) + gutter + label_w + if key_w > 0 { px(28) + key_w } else { 0 });
    let mut rows = Vec::new();
    let mut yy = pad;
    for it in &items {
        let h = if it.0.is_empty() { px(9) } else { lh + px(12) };
        rows.push(RECT { left: pad, top: yy, right: wd - pad, bottom: yy + h });
        yy += h;
    }
    let ht = yy + pad;
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST), &mut mi);
    let work = mi.rcWork;
    let x = x.min(work.right - wd - px(4)).max(work.left + px(4));
    let y = if y + ht > work.bottom - px(4) { above - ht } else { y };
    let y = y.min(work.bottom - ht - px(4)).max(work.top + px(4));

    static CLASS: std::sync::Once = std::sync::Once::new();
    let class = wz("MosaicMenu");
    CLASS.call_once(|| {
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_DROPSHADOW;
        wc.hInstance = GetModuleHandleW(null());
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.lpfnWndProc = Some(menu_proc);
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
    });
    let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, class.as_ptr(), null(), WS_POPUP, x, y, wd, ht, owner, null_mut(), GetModuleHandleW(null()), null());
    if hwnd.is_null() {
        return None;
    }
    let (dark, round, border): (BOOL, u32, u32) = (1, 2, gdi(theme().line));
    DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const c_void, 4); // dark
    DwmSetWindowAttribute(hwnd, 33, &round as *const _ as *const c_void, 4); // DWMWA_WINDOW_CORNER_PREFERENCE: round
    DwmSetWindowAttribute(hwnd, 34, &border as *const _ as *const c_void, 4); // DWMWA_BORDER_COLOR: --line
    let hover = items.iter().position(|i| i.2);
    let canvas = Canvas::new(hwnd, &gfx);
    POPUP.with(|m| *m.borrow_mut() = Some(Menu { items, rows, hover, done: None, canvas, gfx, scale, gutter }));
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    UpdateWindow(hwnd);
    SetCapture(hwnd);
    let root = GetAncestor(owner, GA_ROOT);
    let mut msg: MSG = zeroed();
    while POPUP.with(|m| m.borrow().as_ref().is_some_and(|m| m.done.is_none())) {
        if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
            PostQuitMessage(0);
            break;
        }
        match msg.message {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                menu_key(hwnd, msg.wParam as u16);
                continue;
            }
            WM_CHAR | WM_SYSCHAR | WM_KEYUP | WM_SYSKEYUP => continue,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN | WM_MOUSEWHEEL => {
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                if msg.hwnd != hwnd || !inside(&r, msg.pt.x, msg.pt.y) {
                    finish(None); // a click outside closes the menu and goes nowhere else
                    continue;
                }
            }
            _ => {}
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
        if GetCapture() != hwnd || GetForegroundWindow() != root {
            finish(None);
        }
    }
    ReleaseCapture();
    DestroyWindow(hwnd);
    POPUP.with(|m| m.borrow_mut().take()).and_then(|m| m.done.flatten())
}

unsafe fn menu_key(hwnd: HWND, vk: u16) {
    POPUP.with(|m| {
        let mut g = m.borrow_mut();
        let Some(m) = g.as_mut() else { return };
        let n = m.items.len() as isize;
        let step = |from: isize, d: isize| -> Option<usize> {
            (1..=n).map(|k| (from + d * k).rem_euclid(n) as usize).find(|&i| !m.items[i].0.is_empty())
        };
        match vk {
            VK_DOWN => m.hover = step(m.hover.map_or(-1, |h| h as isize), 1),
            VK_UP => m.hover = step(m.hover.map_or(n, |h| h as isize), -1),
            VK_HOME => m.hover = step(-1, 1),
            VK_END => m.hover = step(n, -1),
            VK_RETURN | VK_SPACE => {
                m.done = Some(m.hover);
            }
            VK_ESCAPE | VK_MENU | VK_F10 | VK_TAB => m.done = Some(None),
            _ => {}
        }
    });
    InvalidateRect(hwnd, null(), 0);
}

unsafe extern "system" fn menu_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            POPUP.with(|p| {
                if let Some(m) = p.borrow_mut().as_mut() {
                    paint_menu(m);
                }
            });
            EndPaint(h, &ps);
            return 0;
        }
        WM_MOUSEMOVE => {
            // over another menu-bar button: close this menu, the bar opens that one (see `menu_bar_popup`)
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            if let Some(i) = BAR.with(|b| b.borrow().iter().find(|(r, _)| inside(r, pt.x, pt.y)).map(|b| b.1)) {
                SWITCH.with(|s| s.set(Some(i)));
                finish(None);
                return 0;
            }
            let (x, y) = xy(l);
            let changed = POPUP.with(|p| {
                let mut g = p.borrow_mut();
                let m = g.as_mut()?;
                let hov = m.rows.iter().position(|r| inside(r, x, y)).filter(|&i| !m.items[i].0.is_empty());
                let changed = hov.is_some() && hov != m.hover;
                if hov.is_some() {
                    m.hover = hov;
                }
                Some(changed)
            });
            if changed == Some(true) {
                InvalidateRect(h, null(), 0);
            }
            return 0;
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            let (x, y) = xy(l);
            let pick = POPUP.with(|p| p.borrow().as_ref().and_then(|m| m.rows.iter().position(|r| inside(r, x, y)).filter(|&i| !m.items[i].0.is_empty())));
            if let Some(i) = pick {
                finish(Some(i));
            }
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

fn paint_menu(m: &mut Menu) {
    if !m.canvas.begin() {
        return;
    }
    let (g, cv, s) = (&m.gfx, &m.canvas, m.scale);
    let px = |v: i32| (v as f64 * s).round() as i32;
    cv.clear(theme().card);
    for (k, ((label, keys, checked), r)) in m.items.iter().zip(&m.rows).enumerate() {
        if label.is_empty() {
            cv.fill(&RECT { left: r.left + px(2), top: r.top + px(4), right: r.right - px(2), bottom: r.top + px(4) + 1 }, theme().line, 1.0);
            continue;
        }
        let on = m.hover == Some(k);
        if on {
            cv.rrect(r, (6.0 * s) as f32, Some(theme().hl), None);
        }
        put(g, cv, label, &g.f.reg, r.left + px(10) + m.gutter, r.top + px(6), if on || *checked { theme().accent } else { theme().text });
        if !keys.is_empty() {
            put(g, cv, keys, &g.f.reg, r.right - px(10) - g.width(keys, &g.f.reg), r.top + px(6), theme().muted);
        }
        if *checked {
            put(g, cv, "✓", &g.f.reg, r.left + px(8), r.top + px(6), theme().accent); // in its own column: never over a shortcut
        }
    }
    m.canvas.end();
}

// ---- actions (outside `with`: they show dialogs, menus, or run a drag loop) ------------------------------------
fn plural(n: usize) -> String {
    t_plural("item", n as i64)
}

unsafe fn confirm(owner: HWND, text: &str) -> bool {
    MessageBoxW(owner, wz(text).as_ptr(), wz("Mosaic").as_ptr(), MB_OKCANCEL | MB_ICONQUESTION) == IDOK
}

unsafe fn act(a: Act) {
    let Some(Some((paths, n, main))) = with(|u| (u.sel.count() > 0).then(|| (u.sel_paths(), u.sel.count(), u.main))) else { return };
    if paths.is_empty() {
        return;
    }
    let msg = match a {
        Act::Open => {
            if n > 5 && !confirm(main, &t("confirm.open_items").replace("{n}", &n.to_string())) {
                return;
            }
            paths.iter().take(20).for_each(|p| open_item(p));
            return;
        }
        Act::Reveal => return shell::reveal(&paths[0]),
        Act::Props => return shell::properties(paths[0].clone()),
        Act::Copy | Act::Cut => {
            let cut = matches!(a, Act::Cut);
            match shell::clipboard_files(&paths, cut) {
                Ok(()) if cut => t("status.cut").replace("{n}", &plural(paths.len())),
                Ok(()) => t("status.copied").replace("{n}", &plural(paths.len())),
                Err(e) => t("status.failed").replace("{e}", &e),
            }
        }
        Act::Path => {
            if clip_text(main, &paths.join("\r\n")) {
                t("status.path_copied").into()
            } else {
                t("status.clipboard_busy").into()
            }
        }
        Act::Del => {
            if !confirm(main, &t("confirm.recycle").replace("{n}", &plural(n))) {
                return;
            }
            let _ = crate::infrastructure::fileops::recycle_many(&paths);
            let gone = paths.iter().filter(|p| !std::path::Path::new(p).exists()).count();
            std::thread::sleep(Duration::from_millis(400)); // the USN journal needs a moment to report it
            with(|u| {
                u.run(true);
                u.set_sel(Sel::default(), None, Some(None));
            });
            if gone > 0 {
                t("status.moved_recycle").replace("{n}", &plural(gone))
            } else {
                t("status.nothing_deleted").into()
            }
        }
    };
    with(|u| u.flash(msg));
}

unsafe fn clip_text(owner: HWND, s: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    let bytes: Vec<u8> = s.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
    if OpenClipboard(owner) == 0 {
        return false;
    }
    EmptyClipboard();
    let h = shell::global(&bytes);
    let ok = !SetClipboardData(13, h as _).is_null(); // CF_UNICODETEXT
    if !ok {
        GlobalFree(h as _);
    }
    CloseClipboard();
    ok
}

unsafe fn context_menu(list: HWND, l: LPARAM) {
    let (mut x, mut y) = xy(l);
    let show = with(|u| {
        if x == -1 && y == -1 {
            // keyboard (menu key / Shift+F10): at the focused item
            let i = u.focus.unwrap_or(0);
            let x0 = if u.grid { u.cell_x(i % u.per).0 } else { 0 };
            let mut p = POINT { x: x0 + u.px(40), y: ((i / u.ncols()) as f64 * u.rh() as f64 - u.y) as i32 + u.rh() };
            ClientToScreen(list, &mut p);
            (x, y) = (p.x, p.y);
        } else {
            let mut p = POINT { x, y };
            ScreenToClient(list, &mut p);
            if let Some(i) = u.item_at(p.x, p.y) {
                if !u.sel.has(i) {
                    u.set_sel(Sel::one(i), Some(i), Some(Some(i)));
                }
            }
        }
        u.sel.count() > 0
    });
    if show != Some(true) {
        return;
    }
    UpdateWindow(list);
    let m = menu();
    let items = m.iter().map(|(l, k, _)| (l.to_string(), k.to_string(), false)).collect();
    if let Some(Some(a)) = popup(GetAncestor(list, GA_ROOT), x, y, y, 0, false, items).map(|k| m[k].2) {
        act(a);
    }
}

/// The filter, Names/type and grid sort dropdowns: the options under the button (over it, in the status bar), the
/// current one checked.
unsafe fn drop_menu(main: HWND, hit: Hit) {
    let opts: Vec<(&str, &str)> = match hit {
        Hit::Filter => filters().to_vec(),
        Hit::Meta => metas().to_vec(),
        Hit::Lang => lang_opts().to_vec(),
        _ => col_defs().to_vec(),
    };
    let cur = |u: &Ui| match hit {
        Hit::Filter => u.args.cat.clone(),
        Hit::Meta => u.args.meta.clone(),
        Hit::Lang => crate::i18n::lang(),
        _ => u.args.sort.clone(),
    };
    let Some((r, cur, gap, dialog_w)) = with(|u| (u.hits.iter().find(|x| x.1 == hit).map_or(RECT { left: 0, top: 0, right: 0, bottom: 0 }, |x| x.0), cur(u), u.px(4), u.px(260))) else { return };
    // Language has too many options (17) to read as a flyout under a small button — open it centered
    // over the main window instead, like a little dialog, rather than anchored to the button.
    let (p, top, min_w) = if hit == Hit::Lang {
        let mut wr: RECT = zeroed();
        GetWindowRect(main, &mut wr);
        let (cx, cy) = ((wr.left + wr.right) / 2, (wr.top + wr.bottom) / 2);
        let pt = POINT { x: cx - dialog_w / 2, y: cy - dialog_w * 3 / 4 };
        (pt, pt, dialog_w)
    } else {
        let mut p = POINT { x: r.left, y: r.bottom + gap };
        let mut top = POINT { x: r.left, y: r.top - gap };
        ClientToScreen(main, &mut p);
        ClientToScreen(main, &mut top);
        (p, top, r.right - r.left)
    };
    let items = opts.iter().map(|(l, v)| (l.to_string(), String::new(), *v == cur)).collect();
    if let Some(k) = popup(main, p.x, p.y, top.y, min_w, true, items) {
        let v = opts[k].1;
        with(|u| {
            match hit {
                Hit::Filter => {
                    u.args.cat = v.into();
                    u.layout(); // the status bar shows the filter
                }
                Hit::Meta => {
                    u.args.meta = v.into();
                    u.save_settings();
                }
                Hit::Lang => return u.set_lang(v),
                _ => u.args.sort = v.into(),
            }
            u.run(false);
            u.invalidate();
        });
    }
}

/// The File/Edit/View/Search strip: each item either runs a selection-scoped `Act` (same as the right-click
/// menu) or flips one of the toolbar toggles that already live elsewhere in the UI — this is just another
/// way to reach them, not a second copy of their behavior.
unsafe fn menu_bar_popup(main: HWND, mut menu: usize) {
    loop {
        SWITCH.with(|s| s.set(None));
        // the other buttons, in screen coordinates: pointing at one while this menu is open switches to its menu
        let others = with(|u| {
            u.hot = Some(Hit::Menu(menu)); // the open menu's button stays lit
            u.invalidate();
            u.hits
                .iter()
                .filter_map(|&(r, h)| match h {
                    Hit::Menu(i) if i != menu => Some((r, i)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        BAR.with(|b| {
            *b.borrow_mut() = others
                .into_iter()
                .map(|(mut r, i)| {
                    let mut p = POINT { x: r.left, y: r.top };
                    ClientToScreen(main, &mut p);
                    let (w, h) = (r.right - r.left, r.bottom - r.top);
                    r = RECT { left: p.x, top: p.y, right: p.x + w, bottom: p.y + h };
                    (r, i)
                })
                .collect()
        });
        menu_bar_once(main, menu);
        BAR.with(|b| b.borrow_mut().clear());
        match SWITCH.with(|s| s.take()) {
            Some(next) => menu = next,
            None => break,
        }
    }
    with(|u| {
        u.hot = None;
        u.invalidate();
    });
}

unsafe fn menu_bar_once(main: HWND, menu: usize) {
    let Some(Some((r, gap))) = with(|u| u.hits.iter().find(|x| x.1 == Hit::Menu(menu)).map(|x| (x.0, u.px(4)))) else { return };
    let mut p = POINT { x: r.left, y: r.bottom + gap };
    let mut top = POINT { x: r.left, y: r.top - gap };
    ClientToScreen(main, &mut p);
    ClientToScreen(main, &mut top);
    let sep = || (String::new(), String::new(), false);
    let items: Vec<(String, String, bool)> = match menu {
        0 => vec![
            (t("menu.open").into(), "Enter".into(), false),
            (t("menu.open_folder").into(), String::new(), false),
            sep(),
            (t("menu.copy_path").into(), String::new(), false),
            (t("menu.properties").into(), String::new(), false),
            sep(),
            (t("menu.delete").into(), "Del".into(), false),
            sep(),
            (t("menu.exit").into(), "Alt+F4".into(), false),
        ],
        1 => vec![
            (t("menu.copy").into(), "Ctrl+C".into(), false),
            (t("menu.cut").into(), "Ctrl+X".into(), false),
            sep(),
            (t("menu.select_all").into(), "Ctrl+A".into(), false),
        ],
        2 => with(|u| {
            vec![
                (t("menu.view_list").into(), String::new(), !u.grid),
                (t("menu.view_grid").into(), String::new(), u.grid),
                sep(),
                (t("toolbar.preview").into(), "Alt+P".into(), u.show_pv),
                (t("toolbar.smooth").into(), String::new(), u.smooth),
                (t("toolbar.sidebar").into(), "Alt+B".into(), u.show_side),
                sep(),
                (t("menu.zoom_in").into(), "Ctrl++".into(), false),
                (t("menu.zoom_out").into(), "Ctrl+-".into(), false),
                sep(),
                (format!("{}: {}", t("menu.theme"), theme_opts()[0].0), String::new(), theme_name() == theme_opts()[0].1),
                (format!("{}: {}", t("menu.theme"), theme_opts()[1].0), String::new(), theme_name() == theme_opts()[1].1),
            ]
        })
        .unwrap_or_default(),
        3 => with(|u| {
            vec![
                (t("toolbar.match_case").into(), "Alt+M".into(), u.args.match_case),
                (t("toolbar.whole_word").into(), "Alt+W".into(), u.args.whole_word),
                (t("toolbar.regex").into(), "Alt+E".into(), u.args.regex),
                (t("toolbar.diacritics").into(), "Alt+D".into(), u.args.diacritics),
                sep(),
                (t("toolbar.metadata").into(), String::new(), u.args.tags),
                (t("toolbar.content").into(), String::new(), u.args.content),
                sep(),
                (t("toolbar.timestamps").into(), "Alt+T".into(), u.timestamps),
            ]
        })
        .unwrap_or_default(),
        4 => vec![(t("menu.language").into(), String::new(), false), sep(), (t("menu.install_service").into(), String::new(), false)],
        _ => vec![(t("menu.about").into(), String::new(), false)],
    };
    let Some(k) = popup(main, p.x, p.y, top.y, r.right - r.left, matches!(menu, 2 | 3), items) else { return };
    match (menu, k) {
        (0, 0) => act(Act::Open),
        (0, 1) => act(Act::Reveal),
        (0, 3) => act(Act::Path),
        (0, 4) => act(Act::Props),
        (0, 6) => act(Act::Del),
        (0, 8) => {
            PostMessageW(main, WM_CLOSE, 0, 0);
        }
        (1, 0) => act(Act::Copy),
        (1, 1) => act(Act::Cut),
        (1, 3) => {
            with(|u| {
                if u.matches > 0 {
                    let mut s = Sel::default();
                    s.add(0, u.matches - 1);
                    u.set_sel(s, u.focus, Some(Some(0)));
                }
            });
        }
        (2, 0 | 1) => {
            with(|u| u.reshape(k == 1, u.size));
        }
        (2, 3) => toggle_preview(),
        (2, 4) => {
            with(|u| {
                u.smooth = !u.smooth;
                u.save_settings();
            });
        }
        (2, 5) => toggle_side(),
        (2, 7 | 8) => {
            with(|u| u.zoom(k == 7));
        }
        (2, 10 | 11) => {
            with(|u| u.set_theme_choice(theme_opts()[k - 10].1));
        }
        (3, 0..=3) => {
            with(|u| {
                match k {
                    0 => u.args.match_case = !u.args.match_case,
                    1 => u.args.whole_word = !u.args.whole_word,
                    2 => u.args.regex = !u.args.regex,
                    _ => u.args.diacritics = !u.args.diacritics,
                }
                u.save_settings();
                u.run(false);
            });
        }
        (3, 5 | 6) => {
            with(|u| {
                if k == 5 {
                    u.args.tags = !u.args.tags;
                } else {
                    u.args.content = !u.args.content;
                }
                u.save_settings();
                u.run(false);
            });
        }
        (3, 8) => {
            with(|u| {
                u.timestamps = !u.timestamps;
                crate::datestore::set_enabled(u.timestamps);
                u.save_settings();
                u.run(false);
            });
        }
        (4, 0) => drop_menu(main, Hit::Lang),
        (4, 2) => offer_install(main),
        (5, 0) => show_about(main),
        _ => {}
    }
}

/// Help > About: just the version, like any small utility's about box.
unsafe fn show_about(main: HWND) {
    let text = format!("Mosaic {}\n\nA fast native file search.", env!("CARGO_PKG_VERSION"));
    MessageBoxW(main, wz(&text).as_ptr(), wz("Mosaic").as_ptr(), MB_OK | MB_ICONINFORMATION);
}

unsafe fn offer_install(main: HWND) {
    let text = "Reading the file table needs the background index service (one admin prompt).\n\nInstall it now?";
    if MessageBoxW(main, wz(text).as_ptr(), wz("Mosaic").as_ptr(), MB_YESNO | MB_ICONINFORMATION) != IDYES {
        return;
    }
    let hw = main as isize;
    std::thread::spawn(move || {
        let ok = crate::infrastructure::elevation::run_elevated("--svc-setup").is_ok();
        std::thread::sleep(Duration::from_millis(1500));
        PostMessageW(hw as HWND, WM_INSTALLED, ok as usize, 0);
    });
}

struct Win(HWND);
impl raw_window_handle::HasWindowHandle for Win {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let h = std::num::NonZeroIsize::new(self.0 as isize).ok_or(raw_window_handle::HandleError::Unavailable)?;
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::Win32WindowHandle::new(h).into()) })
    }
}

// ---- window procedures ---------------------------------------------------------------------------------------
unsafe extern "system" fn main_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            with(|u| u.paint_main());
            EndPaint(h, &ps);
            return 0;
        }
        WM_SIZE => {
            with(|u| u.layout());
            return 0;
        }
        WM_CTLCOLOREDIT => {
            // Cached by color, not just created once: `theme()` changes at runtime (the Theme dropdown),
            // and a brush made for the old theme would otherwise paint every edit box the wrong shade forever.
            static BRUSH: std::sync::Mutex<Option<(u32, isize)>> = std::sync::Mutex::new(None);
            let bg = gdi(theme().bg);
            SetTextColor(wp as HDC, gdi(theme().text));
            SetBkColor(wp as HDC, bg);
            let mut b = BRUSH.lock().unwrap();
            if b.is_none_or(|(c, _)| c != bg) {
                if let Some((_, old)) = b.take() {
                    DeleteObject(old as HBRUSH);
                }
                *b = Some((bg, CreateSolidBrush(bg) as isize));
            }
            return b.unwrap().1;
        }
        WM_COMMAND => {
            match (wp >> 16) as u32 {
                0x0300 => {
                    // EN_CHANGE: search 60 ms after the last keystroke
                    with(|u| u.typing = true);
                    SetTimer(h, T_TYPE, 60, None);
                }
                0x0100 => {
                    InvalidateRect(h, null(), 0); // EN_SETFOCUS: the box's focus ring
                }
                0x0200 => {
                    InvalidateRect(h, null(), 0); // EN_KILLFOCUS: the box's focus ring
                }
                _ => {}
            }
            return 0;
        }
        WM_TIMER => {
            match wp {
                T_TYPE => {
                    KillTimer(h, T_TYPE);
                    with(|u| {
                        let n = GetWindowTextLengthW(u.edit);
                        let mut b = vec![0u16; n as usize + 1];
                        GetWindowTextW(u.edit, b.as_mut_ptr(), n + 1);
                        u.args.q = String::from_utf16_lossy(&b[..n as usize]);
                        u.typing = false;
                        u.run(false);
                    });
                }
                T_POLL => {
                    if with(|u| u.poll()) == Some(true) {
                        offer_install(h);
                    }
                }
                T_FLASH => {
                    KillTimer(h, T_FLASH);
                    InvalidateRect(h, null(), 0);
                }
                T_THUMB => {
                    KillTimer(h, T_THUMB);
                    with(|u| u.want_thumbs());
                }
                T_PREVIEW => {
                    KillTimer(h, T_PREVIEW);
                    // outside `with`: opening a preview handler is a cross-process call that pumps messages
                    let Some(t) = with(|u| u.show_pv.then(|| u.pv_target())).flatten() else { return 0 };
                    if let Some((p, size)) = preview::show(t) {
                        let hw = h as isize;
                        shell::shell_spawn(Box::new(move |dropped| {
                            let png = if dropped { None } else { shell::thumbnail(&p, size, false) };
                            let b: Box<(String, Option<Vec<u8>>)> = Box::new((p, png));
                            unsafe { PostMessageW(hw as HWND, WM_PREVIEW, 0, Box::into_raw(b) as isize) };
                        }));
                    }
                }
                _ => {}
            }
            return 0;
        }
        WM_RESULT => {
            let p = l as *mut Done;
            if with(|u| u.done(*Box::from_raw(p))).is_none() {
                PostMessageW(h, m, wp, l); // state busy: try again in a moment
            }
            return 0;
        }
        WM_ICON => {
            let p = l as *mut (String, Option<Vec<u8>>, bool);
            if with(|u| u.got_icon(*Box::from_raw(p))).is_none() {
                PostMessageW(h, m, wp, l);
            }
            return 0;
        }
        WM_THUMB => {
            let p = l as *mut (String, Option<Vec<u8>>, bool, i32);
            if with(|u| u.got_thumb(*Box::from_raw(p))).is_none() {
                PostMessageW(h, m, wp, l);
            }
            return 0;
        }
        WM_PREVIEW => {
            let (p, png) = *Box::from_raw(l as *mut (String, Option<Vec<u8>>));
            preview::got_pic(&p, png);
            return 0;
        }
        WM_INSTALLED => {
            with(|u| {
                u.backend = SearchBackend::detect();
                u.offered = false;
                u.run(true);
            });
            return 0;
        }
        // the mouse's back / forward buttons (and keyboards' browser keys): children pass them up
        WM_APPCOMMAND if matches!(((l >> 16) & 0x0fff) as u32, 1 | 2) => {
            let back = ((l >> 16) & 0x0fff) == 1;
            with(|u| u.go_hist(back));
            return 1;
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            match with(|u| u.click_main(x, y)).flatten() {
                Some(Hit::Preview) => toggle_preview(),
                Some(Hit::Side) => toggle_side(),
                Some(Hit::Menu(i)) => menu_bar_popup(h, i),
                Some(hit) => drop_menu(h, hit),
                None => {}
            }
            return 0;
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = xy(l);
            let tag = with(|u| {
                let on = u.hits.iter().any(|(r, t)| *t == Hit::Tag && inside(r, x, y));
                if on {
                    u.args.cat.clear();
                    u.hot = None;
                    u.layout();
                    u.run(false);
                    u.invalidate();
                }
                on
            });
            if tag != Some(true) {
                return main_proc(h, WM_LBUTTONDOWN, wp, l); // anywhere else it is a second click
            }
            return 0;
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(l);
            if with(|u| u.split.map(|_| u.split_x = Some(x)).is_some()) == Some(true) {
                return 0; // resized on the next frame (`drag_split`)
            }
            with(|u| {
                let hot = u.hits.iter().find(|(r, _)| inside(r, x, y)).map(|h| h.1).filter(|h| *h != Hit::Search);
                if hot != u.hot {
                    u.hot = hot;
                    InvalidateRect(h, null(), 0);
                }
                u.set_toolbar_tip();
                let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut t);
            });
            return 0;
        }
        WM_MOUSELEAVE => {
            with(|u| {
                u.hot = None;
                u.set_toolbar_tip();
            });
            InvalidateRect(h, null(), 0);
            return 0;
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED if with(|u| u.split.is_some()) == Some(true) => {
            with(|u| {
                u.drag_split();
                u.split = None;
                u.save_settings();
                InvalidateRect(h, null(), 0);
            });
            if m == WM_LBUTTONUP {
                ReleaseCapture();
            }
            preview::resizing(false);
            return 0;
        }
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT && with(|u| {
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            ScreenToClient(h, &mut p);
            u.split.is_some() || inside(&u.grip, p.x, p.y) && u.hot == Some(Hit::Split)
        }) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE));
            return 1;
        }
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT => {
            if with(|u| u.hot.is_some_and(|h| !matches!(h, Hit::Filter | Hit::Meta | Hit::Sort | Hit::Tag | Hit::Menu(_)))) == Some(true) {
                SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                return 1;
            }
        }
        WM_MOUSEWHEEL => {
            if let Some(list) = with(|u| u.list) {
                return SendMessageW(list, m, wp, l);
            }
        }
        WM_DPICHANGED => {
            let r = *(l as *const RECT);
            with(|u| {
                u.scale = (wp & 0xffff) as f64 / 96.0;
                if let Ok(g) = Gfx::new(u.scale, u.rtl) {
                    u.cm = Canvas::new(u.main, &g);
                    u.cl = Canvas::new(u.list, &g);
                    u.gfx = g;
                }
                DeleteObject(u.edit_font);
                u.edit_font = edit_font(u.scale);
                SendMessageW(u.edit, WM_SETFONT, u.edit_font as usize, 1);
            });
            SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            set_icons(h, true);
            with(|u| u.layout());
            preview::rescale((wp & 0xffff) as f64 / 96.0);
            side::rescale((wp & 0xffff) as f64 / 96.0);
            return 0;
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

unsafe extern "system" fn list_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            with(|u| u.paint_list());
            EndPaint(h, &ps);
            return 0;
        }
        WM_SIZE => {
            with(|u| u.clamp());
            InvalidateRect(h, null(), 0);
            return 0;
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xffff) as i16 as i32;
            with(|u| {
                if wp & 0x8 == 0 {
                    // MK_CONTROL not held
                    u.zoom_acc = 0;
                    return u.wheel(delta);
                }
                u.zoom_acc += delta;
                while u.zoom_acc.abs() >= 120 {
                    u.zoom(u.zoom_acc > 0);
                    u.zoom_acc -= 120 * u.zoom_acc.signum();
                }
            });
            return 0;
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|u| u.ldown(x, y));
            return 0;
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = xy(l);
            if with(|u| u.on_sb(x)) == Some(true) {
                with(|u| u.ldown(x, y)); // a second press on the scrollbar pages again
                return 0;
            }
            if let Some(Some(p)) = with(|u| u.item_at(x, y).and_then(|i| u.get(i)).map(|r| r.path.clone())) {
                open_item(&p);
            }
            return 0;
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(l);
            let r = with(|u| u.mmove(x, y));
            // paint now: queued, the list's paint waits behind the main window's (the status bar), which a drag
            // invalidates again on every move, so the band trailed the pointer
            UpdateWindow(h);
            if let Some(Some(paths)) = r {
                if !paths.is_empty() {
                    let icon = drag::Image::Raw(include_bytes!("../icons/drag.png").to_vec());
                    let files = paths.into_iter().map(std::path::PathBuf::from).collect();
                    let _ = drag::start_drag(&Win(h), drag::DragItem::Files(files), icon, |_, _| {}, Default::default());
                }
            }
            return 0;
        }
        WM_LBUTTONUP => {
            with(|u| u.lup());
            return 0;
        }
        WM_MOUSELEAVE => {
            with(|u| {
                u.tracking = false;
                let sb = std::mem::take(&mut u.sb_hot);
                if u.hover.take().is_some() || sb {
                    u.set_tip();
                    InvalidateRect(h, null(), 0);
                }
            });
            return 0;
        }
        WM_MBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|u| {
                if u.auto.take().is_some() {
                    ReleaseCapture();
                } else {
                    u.auto = Some(Auto { x, y, cy: y, t0: Instant::now(), moved: false });
                    u.frame = Instant::now();
                    SetCapture(h);
                }
                InvalidateRect(h, null(), 0);
            });
            return 0;
        }
        WM_MBUTTONUP => {
            with(|u| {
                // held and dragged: stop on release. A quick click keeps scrolling until the next click.
                if u.auto.as_ref().is_some_and(|a| a.moved || a.t0.elapsed() > Duration::from_millis(400)) {
                    u.auto = None;
                    ReleaseCapture();
                    InvalidateRect(h, null(), 0);
                }
            });
            return 0;
        }
        WM_RBUTTONDOWN => {
            with(|u| {
                if u.auto.take().is_some() {
                    ReleaseCapture();
                }
            });
            return 0;
        }
        WM_CONTEXTMENU => {
            context_menu(h, l);
            return 0;
        }
        WM_SETCURSOR if with(|u| u.auto.is_some()) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_SIZENS));
            return 1;
        }
        WM_CHAR if wp >= 32 => {
            // typing while the list has focus goes to the search box
            if let Some(edit) = with(|u| u.edit) {
                SetFocus(edit);
                SendMessageW(edit, WM_CHAR, wp, l);
            }
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
    fn selection_ranges_merge_split_and_toggle() {
        let mut s = Sel::default();
        s.add(5, 9);
        s.add(10, 12); // adjacent: merges
        assert_eq!(s.0, [(5, 12)]);
        s.toggle(8); // splits
        assert_eq!(s.0, [(5, 7), (9, 12)]);
        s.toggle(8); // rejoins
        assert_eq!(s.0, [(5, 12)]);
        s.add(20, 20);
        s.remove(0, 6);
        assert_eq!(s.0, [(7, 12), (20, 20)]);
        assert!(s.has(12) && !s.has(13) && s.has(20));
        assert_eq!(s.count(), 7);
        s.add(0, 100);
        assert_eq!(s.0, [(0, 100)]);
    }

    #[test]
    fn highlight_runs_are_case_insensitive() {
        let t = terms("Foo ext:txt \"bar\"");
        let s = w("xFOOybarz");
        let r: Vec<_> = runs(&s, &t).into_iter().map(|(a, b, m)| (String::from_utf16_lossy(&s[a..b]), m)).collect();
        assert_eq!(r, [("x".into(), false), ("FOO".into(), true), ("y".into(), false), ("bar".into(), true), ("z".into(), false)]);
    }

    #[test]
    fn zoom_steps_through_grid_sizes_and_into_the_list() {
        let mut v = (false, 100);
        let mut seen = vec![];
        for _ in 0..100 {
            v = zoom_step(v.0, v.1, true);
            seen.push(v.1);
        }
        assert_eq!(seen[0], MIN_SIZE, "list → smallest tiles");
        assert!(seen.windows(2).all(|w| w[1] > w[0] || w[1] == MAX_SIZE), "every step in is bigger, until the biggest");
        assert_eq!(v, (true, MAX_SIZE));
        for _ in 0..100 {
            v = zoom_step(v.0, v.1, false);
        }
        assert!(!v.0, "zooming out past the smallest tiles returns to the list");
    }

    #[test]
    fn pixels_premultiply_and_shrink() {
        // 2×2: opaque red, transparent, opaque white, half-transparent white → one pixel, averaged, premultiplied
        let px = [255, 0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 128];
        let (out, w, h) = pixels(&px, 2, 2, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, [95, 95, 159, 159]); // B, G: (0+0+255+128)/4; R, A: (255+0+255+128)/4
        assert_eq!(pixels(&px, 2, 2, 8).1, 2); // never enlarged
    }

    #[test]
    fn grid_labels_stop_at_two_lines() {
        let g = Gfx::new(1.0, false).unwrap();
        let s = w(&"holiday_photo ".repeat(20));
        let t = g.layout(&s, &g.f.label, 100.0, (2 * g.f.lh) as f32).unwrap();
        let m = gfx::metrics(&t);
        assert!(m.height <= (2 * g.f.lh) as f32 + 0.5, "{} lines, {} px", m.lineCount, m.height);
    }

    #[test]
    fn col_positions_packs_flex_then_fixed_left_to_right() {
        // width 1000, pad 10, gap 10, 2 flex + 2 fixed (150, 100): content = 1000-20-30=950;
        // flex share = (950-250)/2 = 350 each.
        let v = col_positions(1000, 10, 10, 2, &[150, 100], false);
        assert_eq!(v, [(10, 360), (370, 720), (730, 880), (890, 990)]);
        // columns are contiguous, gapped by exactly `gap`, and stay inside [pad, width-pad]
        for w in v.windows(2) {
            assert_eq!(w[1].0 - w[0].1, 10);
        }
        assert_eq!(v.first().unwrap().0, 10);
        assert_eq!(v.last().unwrap().1, 990);
    }

    #[test]
    fn col_positions_rtl_mirrors_the_row_keeping_each_columns_width_and_index() {
        let ltr = col_positions(1000, 10, 10, 2, &[150, 100], false);
        let rtl = col_positions(1000, 10, 10, 2, &[150, 100], true);
        // same widths, same reading-order (column 0 first), but the row is flush against the right edge
        for (&(a, b), &(c, d)) in ltr.iter().zip(&rtl) {
            assert_eq!(b - a, d - c, "column width preserved under mirroring");
        }
        assert!(rtl[0].0 > ltr[0].0, "column 0 shifts toward the right edge in RTL");
        assert_eq!(rtl[0].1, 990); // column 0 (reading order) is flush against the row's right edge in RTL
        assert_eq!(rtl.last().unwrap().0, 10); // the last column ends up flush against the left edge (pad)
    }

    #[test]
    fn col_positions_handles_zero_flex_columns_and_empty_input() {
        assert_eq!(col_positions(1000, 10, 10, 0, &[], false), Vec::<(i32, i32)>::new());
        // fixed-only row: still packs left-to-right after `pad`
        let v = col_positions(1000, 10, 10, 0, &[50, 60], false);
        assert_eq!(v, [(10, 60), (70, 130)]);
    }
}
