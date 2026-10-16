//! `--native`: the search window drawn with Direct2D + DirectWrite instead of a WebView, for tens of MB of RAM
//! instead of a browser engine. Same look (app.css is the spec), same backend (service pipe or in-process index),
//! same shell helpers.

#[path = "native_gfx.rs"]
mod gfx;
#[path = "native_side.rs"]
mod side;
#[path = "native_preview.rs"]
mod preview;
#[path = "native_themes.rs"]
mod themes;
#[path = "native_tabs.rs"]
mod tabs;
#[path = "native_drop.rs"]
mod dnd;
#[path = "native_save.rs"]
pub mod save;
#[path = "native_apps.rs"]
mod apps;
#[path = "native_icons.rs"]
mod icons;
#[path = "native_help.rs"]
mod help;
use tabs::*;

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
/// The shortcut arrows View > Icons… offers (`Ui::link_arrow`): Windows 11's, a dark one, Windows 7's, a bare arrow, a round
/// badge, a box, a chain, a folded corner, macOS's, Windows 95's.
const ARROWS: [&[u8]; 10] = [
    include_bytes!("../icons/link-windows.png"),
    include_bytes!("../icons/link-dark.png"),
    include_bytes!("../icons/link-classic.png"),
    include_bytes!("../icons/link-arrow.png"),
    include_bytes!("../icons/link-badge.png"),
    include_bytes!("../icons/link-box.png"),
    include_bytes!("../icons/link-chain.png"),
    include_bytes!("../icons/link-corner.png"),
    include_bytes!("../icons/link-mac.png"),
    include_bytes!("../icons/link-win95.png"),
];
/// … and how big, in % of Windows' own size (`Ui::link_size`).
const ARROW_SIZES: std::ops::RangeInclusive<u8> = 50..=200;
const WM_RESULT: u32 = WM_APP + 1;
const WM_ICON: u32 = WM_APP + 2;
const WM_INSTALLED: u32 = WM_APP + 3;
const WM_THUMB: u32 = WM_APP + 4;
const WM_PREVIEW: u32 = WM_APP + 5;
const WM_KIND: u32 = WM_APP + 20;
const WM_TAB_ICON: u32 = WM_APP + 6; // lParam: a boxed (icon source, its PNG)
const WM_RENAME: u32 = WM_APP + 30; // to the list: rename the focused item in place (Explorer's menu asked, or a new item showed up)
const WM_RENAME_END: u32 = WM_APP + 31; // … the field (wParam) lost the focus: keep what was typed
const T_TYPE: usize = 1;
const T_POLL: usize = 2;
const T_FLASH: usize = 3;
const T_THUMB: usize = 4;
const T_PREVIEW: usize = 5;
const T_UNCLOAK: usize = 8; // the window shown even if its first frame never came
const T_VIEWS: usize = 9; // the folders' views written, a second after the last change (a Ctrl+wheel zoom is many)
const T_RENAME: usize = 7; // a second, slow click on a selected item's name: rename it, unless a double-click follows
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
#[derive(Clone, Copy, PartialEq)]
pub(super) struct Theme {
    bg: u32,
    card: u32,
    text: u32,
    muted: u32,
    accent: u32,
    line: u32,
    hl: u32,
    row: u32,
    sel: u32,
    dark: bool,
}
#[allow(clippy::too_many_arguments)]
const fn th(bg: u32, card: u32, text: u32, muted: u32, accent: u32, line: u32, hl: u32, row: u32, sel: u32, dark: bool) -> Theme {
    Theme { bg, card, text, muted, accent, line, hl, row, sel, dark }
}
/// The themes the picker offers, dark ones then light ones: (settings key, name, palette). Each dark one is a
/// near-black tinted toward its accent, chrome (`card`) a step lighter; selection and hover are low tints of the
/// accent, so the content stands out rather than the chrome. The light ones are white or a faint tint of theirs.
/// Their accents come from the app icon's presets.
const THEMES: [(&str, &str, Theme); 16] = [
    ("dark", "Midnight", th(0x0f1216, 0x181c22, 0xe6e8eb, 0x8b93a0, 0x7aa2ff, 0x262b33, 0x1f2a44, 0x1c2129, 0x243257, true)),
    ("graphite", "Graphite", th(0x121315, 0x1b1c1f, 0xe7e8ea, 0x8e9299, 0xb4bccb, 0x2b2d31, 0x2a2c31, 0x202124, 0x373a41, true)),
    ("navy", "Navy", th(0x0a0f20, 0x111831, 0xe4ebfa, 0x8a97bd, 0x5b95ff, 0x1f2a4d, 0x182550, 0x141d3b, 0x1f3575, true)),
    ("ocean", "Ocean", th(0x071518, 0x0d2127, 0xdcf3f5, 0x7fa6ab, 0x3fd9e0, 0x18353d, 0x10343c, 0x102a30, 0x114a54, true)),
    ("forest", "Forest", th(0x09130f, 0x0f1f18, 0xdcf3e6, 0x7fa592, 0x3fe08a, 0x1b3328, 0x133325, 0x12291f, 0x164a31, true)),
    ("sunset", "Sunset", th(0x160b08, 0x21110c, 0xfbe6da, 0xb08f80, 0xff9a4a, 0x3a1f16, 0x3d1e12, 0x2b1610, 0x5c2a14, true)),
    ("rose", "Rose", th(0x140a11, 0x20101b, 0xfbe0ee, 0xb08aa0, 0xff7ab8, 0x3a1d31, 0x3c1631, 0x2a1323, 0x5c1d47, true)),
    ("amethyst", "Amethyst", th(0x0e0b1b, 0x17122b, 0xece4fa, 0x9a8fbd, 0xb07bff, 0x2b2148, 0x2b1e52, 0x1f1840, 0x3c2878, true)),
    ("light", "Daylight", th(0xffffff, 0xffffff, 0x1a1d21, 0x6b7280, 0x317dcf, 0xd6dae1, 0xe9f1fd, 0xf3f5f8, 0xd7e6fb, false)),
    ("fog", "Fog", th(0xf4f5f7, 0xfbfbfc, 0x1b1d21, 0x6a707a, 0x4f5868, 0xdcdfe4, 0xe7e9ed, 0xeceef1, 0xd8dce3, false)),
    ("mint", "Mint", th(0xf3faf6, 0xffffff, 0x14231c, 0x5f7a6c, 0x14996a, 0xd3e8dc, 0xdff4ea, 0xe9f5ef, 0xc6ecda, false)),
    ("lagoon", "Lagoon", th(0xf2f9fa, 0xffffff, 0x0f2226, 0x5b7a80, 0x138a96, 0xcfe5e8, 0xdcf1f3, 0xe8f4f5, 0xc4e8ec, false)),
    ("paper", "Paper", th(0xf8f5ef, 0xfffdf9, 0x2a251e, 0x7d7366, 0xb8640e, 0xe6dfd3, 0xf4e8d4, 0xf1ece3, 0xefd9b8, false)),
    ("sunrise", "Sunrise", th(0xfff6f1, 0xffffff, 0x2b1810, 0x8a6a5c, 0xe0541c, 0xf0dbd0, 0xfde7da, 0xfaefe8, 0xfad2bd, false)),
    ("blossom", "Blossom", th(0xfdf4f8, 0xffffff, 0x2a1020, 0x8a6478, 0xd6337f, 0xf0d9e4, 0xfbe0ee, 0xf8edf2, 0xf6cde0, false)),
    ("lavender", "Lavender", th(0xf7f5fd, 0xffffff, 0x1d1630, 0x6e6688, 0x7a4ae6, 0xe2ddf2, 0xece4fa, 0xf1eef9, 0xdccff7, false)),
];
impl Theme {
    fn by_name(name: &str) -> Theme {
        THEMES.iter().find(|t| t.0 == name).map_or(THEMES[0].2, |t| t.2) // "default" and unknown: Midnight
    }
    /// `self` blended `k` (0–1) of the way to `o`, colour by colour.
    fn mix(&self, o: &Theme, k: f64) -> Theme {
        let m = |a: u32, b: u32| {
            let ch = |s: u32| {
                let (x, y) = (((a >> s) & 0xff) as f64, ((b >> s) & 0xff) as f64);
                ((x + (y - x) * k).round() as u32) << s
            };
            ch(16) | ch(8) | ch(0)
        };
        th(m(self.bg, o.bg), m(self.card, o.card), m(self.text, o.text), m(self.muted, o.muted), m(self.accent, o.accent), m(self.line, o.line), m(self.hl, o.hl), m(self.row, o.row), m(self.sel, o.sel), if k < 0.5 { self.dark } else { o.dark })
    }
}
static THEME: std::sync::RwLock<Theme> = std::sync::RwLock::new(THEMES[0].2);
static THEME_NAME: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());
/// A theme change under way: from, to, since. Every draw reads `theme()`, so the whole window cross-fades.
static FADE: std::sync::RwLock<Option<(Theme, Theme, Instant)>> = std::sync::RwLock::new(None);
const FADE_S: f64 = 0.22;
pub(super) fn theme() -> Theme {
    if let Some((a, b, t0)) = *FADE.read().unwrap() {
        let k = (t0.elapsed().as_secs_f64() / FADE_S).min(1.0);
        return a.mix(&b, k * k * (3.0 - 2.0 * k));
    }
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
/// Switch to `name`, cross-fading from the colours on screen now.
fn fade_to(name: &str) {
    let from = theme();
    set_theme(name);
    *FADE.write().unwrap() = Some((from, theme_now(), Instant::now()));
}
fn theme_now() -> Theme {
    *THEME.read().unwrap()
}
/// The theme picker's choice, shown everywhere (not kept yet).
fn theme_preview(name: &str) {
    fade_to(name);
    with(|u| u.repaint_theme());
}
/// … and kept.
fn theme_apply(name: &str) {
    if theme_name() != name {
        theme_preview(name);
    }
    with(|u| u.save_settings());
}
/// A frame of the theme cross-fade: true while it runs (every window is repainted meanwhile).
fn fading() -> bool {
    let mut f = FADE.write().unwrap();
    if f.is_some_and(|(_, _, t0)| t0.elapsed().as_secs_f64() >= FADE_S) {
        *f = None;
        return true; // one last frame, at the end colours
    }
    f.is_some()
}
/// Syncs the OS-drawn title bar (caption background/text, dark-mode caption-button glyphs) to the
/// active theme — DWM doesn't pick this up on its own when the theme changes after the window exists.
unsafe fn apply_titlebar(main: HWND) {
    let dark: BOOL = theme().dark as i32;
    DwmSetWindowAttribute(main, 20, &dark as *const _ as *const c_void, 4); // DWMWA_USE_IMMERSIVE_DARK_MODE
    let caption = gdi(theme().bg);
    DwmSetWindowAttribute(main, 35, &caption as *const _ as *const c_void, 4); // DWMWA_CAPTION_COLOR: title bar = the tab strip
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
fn col_defs() -> [(&'static str, &'static str); 5] {
    [(t("col.name"), "name"), (t("col.path"), "path"), (t("col.size"), "size"), (t("col.type"), "type"), (t("col.modified"), "mtime")]
}
/// Columns' default widths: Name's share of what Name and Path have together, then Size, Type, Modified (CSS px).
const COLS_DEFAULT: (f64, [i32; 3]) = (0.5, [85, 150, 150]);
/// The "cols" setting ("share,size,type,modified"); the defaults if it's missing or garbled.
fn parse_cols(s: &str) -> (f64, [i32; 3]) {
    let v: Vec<f64> = s.split(',').filter_map(|x| x.parse().ok()).collect();
    match v[..] {
        [f, a, b, c] if (0.05..=0.95).contains(&f) && [a, b, c].iter().all(|&x| (20.0..2000.0).contains(&x)) => (f, [a as i32, b as i32, c as i32]),
        _ => COLS_DEFAULT,
    }
}
/// Column widths `w` with the edge after column `k` moved `d` px: column k grows as k+1 shrinks, neither below `min`.
fn col_resize(mut w: [i32; 5], k: usize, d: i32, min: i32) -> [i32; 5] {
    let d = d.clamp((min - w[k]).min(0), (w[k + 1] - min).max(0));
    w[k] += d;
    w[k + 1] -= d;
    w
}
/// The classic File/Edit/View/Search/Tools/Help strip above the tab row, like a conventional desktop app's.
fn menu_bar_labels() -> [&'static str; 7] {
    [t("menubar.file"), t("menubar.edit"), t("menubar.view"), t("menubar.tabs"), t("menubar.search"), t("menubar.tools"), t("menubar.help")]
}
/// The grid sizes Explorer's View submenu names (extra large, large, medium, small icons); 0 is Details, the list.
const VIEWS: [(&str, i32); 5] = [("menu.icons_xl", MAX_SIZE), ("menu.icons_l", 160), ("menu.icons_m", 96), ("menu.icons_s", 48), ("menu.details", 0)];

/// A popup menu's entry: no label is a separator; `tag` is what picking it returns; `sub`, a submenu. `icon` is
/// w×h premultiplied BGRA.
#[derive(Clone, Default)]
struct MItem {
    label: String,
    keys: String,
    tag: usize,
    on: bool,
    off: bool,
    bold: bool,
    icon: Option<(i32, i32, Vec<u8>)>,
    sub: Vec<MItem>,
}

const EXTRA: usize = 0x8000; // tags of Mosaic's own entries in Explorer's menu: EXTRA + their act's index
const RENAME: usize = usize::MAX; // Explorer's Rename, which the list does itself

fn entry(label: &str, on: bool, a: Act, acts: &mut Vec<Act>) -> MItem {
    acts.push(a);
    MItem { label: label.into(), on, tag: EXTRA + acts.len() - 1, ..Default::default() }
}

/// What Mosaic adds to Explorer's menu on items (after its Open group): what only Mosaic does with them.
fn item_extras(dir: bool, acts: &mut Vec<Act>) -> Vec<MItem> {
    let mut v = Vec::new();
    if dir {
        v.push(entry(t("menu.open_here"), false, Act::OpenHere, acts));
        v.push(entry(t("menu.open_tab"), false, Act::OpenTab, acts));
    }
    v.push(entry(t("menu.open_folder"), false, Act::Reveal, acts));
    v.push(entry(t("menu.search_in"), false, Act::SearchIn, acts));
    v
}

/// What Mosaic adds to Explorer's menu on empty space (first, as Explorer has them): View ▸, Sort by ▸, Refresh.
fn bg_extras(u: &Ui, acts: &mut Vec<Act>) -> Vec<MItem> {
    // the View size nearest the tiles' own (Ctrl+wheel sizes them in between)
    let near = VIEWS.iter().filter(|v| v.1 > 0).min_by_key(|v| (v.1 - u.size).abs()).map_or(0, |v| v.1);
    let view: Vec<MItem> = VIEWS.iter().map(|&(k, s)| entry(t(k), if u.grid { s == near } else { s == 0 }, Act::View(s), acts)).collect();
    let mut sort: Vec<MItem> = col_defs().into_iter().map(|(l, k)| entry(l, u.args.sort == k, Act::Sort(k), acts)).collect();
    sort.push(MItem::default());
    sort.push(entry(t("menu.ascending"), u.args.dir != "desc", Act::Desc(false), acts));
    sort.push(entry(t("menu.descending"), u.args.dir == "desc", Act::Desc(true), acts));
    // Paste is Explorer's view's, not the folder's menu's: here too, greyed with no files on the clipboard
    let mut paste = entry(t("menu.paste"), false, Act::Paste, acts);
    paste.keys = "Ctrl+V".into();
    paste.off = unsafe { windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable(15) } == 0; // CF_HDROP
    vec![
        MItem { label: t("menubar.view").into(), sub: view, ..Default::default() },
        MItem { label: t("menu.sort_by").into(), sub: sort, ..Default::default() },
        entry(t("menu.refresh"), false, Act::Refresh, acts),
        MItem::default(),
        paste,
    ]
}

/// Explorer's menu as entries: an item's tag is its command id, Rename's is [`RENAME`].
fn shell_entries(v: Vec<shell::MenuItem>) -> Vec<MItem> {
    tidy(
        v.into_iter()
            .map(|s| MItem {
                tag: if s.verb.eq_ignore_ascii_case("rename") { RENAME } else { s.id as usize },
                icon: (s.w > 0 && s.h > 0 && s.px.len() == (s.w * s.h * 4) as usize).then_some((s.w, s.h, s.px)),
                sub: shell_entries(s.sub),
                label: s.label,
                keys: s.keys,
                on: s.on,
                off: s.off,
                bold: s.bold,
            })
            .collect(),
    )
}

/// Mosaic's entries into Explorer's: first on empty space, after the Open group on items, set apart by lines.
fn merge(mut shell: Vec<MItem>, extras: Vec<MItem>, bg: bool) -> Vec<MItem> {
    let at = if bg { 0 } else { shell.iter().position(|i| i.label.is_empty()).map_or(shell.len(), |k| k + 1) };
    shell.splice(at..at, std::iter::once(MItem::default()).chain(extras).chain([MItem::default()]));
    tidy(shell)
}

/// No separator first, last, or twice in a row; no entry twice (PowerToys adds some both ways).
fn tidy(v: Vec<MItem>) -> Vec<MItem> {
    let mut out: Vec<MItem> = Vec::new();
    let mut seen = HashSet::new();
    for it in v {
        if !it.label.is_empty() && !seen.insert(it.label.clone()) {
            continue;
        }
        if !(it.label.is_empty() && out.last().is_none_or(|l| l.label.is_empty())) {
            out.push(it);
        }
    }
    if out.last().is_some_and(|l| l.label.is_empty()) {
        out.pop();
    }
    out
}

/// view, items per row and the Names/type mode survive restarts (the web UI kept them in localStorage).
/// JSON now (was a flat key=value text file); `load_settings` migrates the old file on first read.
/// Folder -> icon kind, kept between runs so a folder is only ever looked into once. Each answer is stamped with
/// the folder's modified time and recomputed when that changes. A `kvlog` store: a save appends the new answers
/// (the whole file, MBs, was rewritten every few minutes while browsing).
fn kinds_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("foldericons.bin")
}

fn load_kinds() -> HashMap<String, (u8, u64)> {
    let kinds = crate::kvlog::load(&kinds_path());
    if !kinds.is_empty() {
        return kinds;
    }
    // the text file of earlier versions (`kind<TAB>mtime<TAB>path` per line), replaced at the first save
    let old = kinds_path().with_extension("txt");
    std::fs::read_to_string(old).unwrap_or_default().lines().filter_map(|l| { let mut f = l.splitn(3, '\t'); let (k, m) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?); Some((f.next()?.to_string(), (k, m))) }).collect()
}

fn mtime_secs(p: &str) -> u64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

fn settings_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Mosaic").join("native.json")
}
/// Each folder's own view and sort, as last set there. In `views.txt`
/// beside the settings, the 1000 set last; read once, written only after a change.
/// A folder's own view: grid, tile size, sorted by, descending; and when it was set.
#[derive(Clone, PartialEq, Debug)]
struct FolderView {
    grid: bool,
    size: i32,
    sort: String,
    desc: bool,
    n: u64, // when set: seconds since 1970, to weigh against Explorer's own
}
type Views = HashMap<String, FolderView>; // by lowercased path
static FOLDER_VIEWS: std::sync::Mutex<Option<(Views, bool)>> = std::sync::Mutex::new(None); // … and changed since written
const MAX_VIEWS: usize = 1000;

fn views_path() -> std::path::PathBuf {
    settings_path().with_file_name("views.txt")
}
fn parse_views(s: &str) -> Views {
    s.lines()
        .filter_map(|l| {
            let mut it = l.splitn(6, '\t');
            let (v, size, sort, dir) = (it.next()?, it.next()?.parse().ok()?, it.next()?, it.next()?);
            let (n, p) = (it.next()?.parse().ok()?, it.next()?);
            Some((p.to_string(), FolderView { grid: v == "grid", size, sort: sort.into(), desc: dir == "desc", n }))
        })
        .collect()
}
fn folder_views<R>(f: impl FnOnce(&mut Views, &mut bool) -> R) -> R {
    let mut g = FOLDER_VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let (m, dirty) = g.get_or_insert_with(|| (parse_views(&std::fs::read_to_string(views_path()).unwrap_or_default()), false));
    f(m, dirty)
}
/// `dir`'s view set to `v`; written a second later.
fn set_folder_view(main: HWND, dir: &str, kind: &'static str, v: FolderView) {
    folder_views(|m, dirty| {
        m.insert(dir.trim_end_matches('\\').to_lowercase(), FolderView { n: now_secs(), ..v.clone() });
        if m.len() > MAX_VIEWS {
            // ponytail: O(n) scan per overflow; fine at 1000
            if let Some(old) = m.iter().min_by_key(|v| v.1.n).map(|v| v.0.clone()) {
                m.remove(&old);
            }
        }
        *dirty = true;
    });
    let mut q = BAGS_DUE.lock().unwrap_or_else(|e| e.into_inner());
    q.retain(|b| !b.0.eq_ignore_ascii_case(dir));
    q.push((dir.to_string(), kind.into(), v));
    unsafe { SetTimer(main, T_VIEWS, 1000, None) };
}
fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
fn places() -> &'static crate::shellbags::Places {
    static PLACES: std::sync::OnceLock<crate::shellbags::Places> = std::sync::OnceLock::new();
    PLACES.get_or_init(crate::shellbags::places)
}
/// Explorer's view of `dir`, if it has one, as a folder's view.
fn bag_view(dir: &str) -> Option<FolderView> {
    let at = places();
    let b = crate::shellbags::read(crate::shellbags::BASE, dir, at)?;
    let (sort, desc) = b.sort.unwrap_or(("name", false));
    Some(FolderView { grid: b.grid, size: if b.grid { crate::shellbags::tile_of(b.icon) } else { 0 }, sort: sort.into(), desc, n: b.when })
}
/// The folders whose view Explorer is to be told, with their folder type: at the next write.
static BAGS_DUE: std::sync::Mutex<Vec<(String, &'static str, FolderView)>> = std::sync::Mutex::new(Vec::new());
/// Explorer told each view set since: into the bags it keeps; a folder it never kept, by a helper (the shell parses its path).
fn write_bags() {
    let at = places();
    for (dir, kind, v) in std::mem::take(&mut *BAGS_DUE.lock().unwrap_or_else(|e| e.into_inner())) {
        let set = crate::shellbags::Set { grid: v.grid, icon: if v.grid { crate::shellbags::icon_of(v.size) } else { 16 }, sort: v.sort, desc: v.desc, kind: kind.into() };
        if !crate::shellbags::update(crate::shellbags::BASE, &dir, at, &set) {
            if let Ok(exe) = std::env::current_exe() {
                use std::os::windows::process::CommandExt;
                let (g, i, d) = (if set.grid { "grid" } else { "list" }, set.icon.to_string(), if set.desc { "desc" } else { "asc" });
                let _ = std::process::Command::new(exe).args(["--bag-write", g, &i, &set.sort, d, kind, &dir]).creation_flags(0x0800_0000).spawn();
            }
        }
    }
}
fn write_views() {
    folder_views(|m, dirty| {
        if std::mem::take(dirty) {
            let mut v: Vec<_> = m.iter().collect();
            v.sort_by_key(|e| e.1.n);
            let s: String = v
                .iter()
                .map(|(p, f)| format!("{}\t{}\t{}\t{}\t{}\t{p}\n", if f.grid { "grid" } else { "list" }, f.size, f.sort, if f.desc { "desc" } else { "asc" }, f.n))
                .collect();
            let _ = std::fs::write(views_path(), s);
        }
    });
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

/// F11: the window over its whole screen, no frame; again: back as it was.
unsafe fn fullscreen(h: HWND) {
    let style = GetWindowLongW(h, GWL_STYLE) as u32;
    if let Some(wp) = FULL.take() {
        SetWindowLongW(h, GWL_STYLE, (style | WS_OVERLAPPEDWINDOW) as i32);
        SetWindowPlacement(h, &wp);
        SetWindowPos(h, null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
        return;
    }
    let mut wp: WINDOWPLACEMENT = zeroed();
    wp.length = size_of::<WINDOWPLACEMENT>() as u32;
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    if GetWindowPlacement(h, &mut wp) == 0 || GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
        return;
    }
    FULL.set(Some(wp));
    SetWindowLongW(h, GWL_STYLE, (style & !WS_OVERLAPPEDWINDOW) as i32);
    let m = mi.rcMonitor;
    SetWindowPos(h, HWND_TOP, m.left, m.top, m.right - m.left, m.bottom - m.top, SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
}
thread_local!(static FULL: std::cell::Cell<Option<WINDOWPLACEMENT>> = const { std::cell::Cell::new(None) });
// The window is on screen but cloaked until its first frame is drawn: Windows would show it white until then.
thread_local!(static CLOAKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

unsafe fn cloak(h: HWND, on: bool) {
    let v: i32 = on as i32;
    DwmSetWindowAttribute(h, 13, &v as *const _ as *const c_void, 4); // DWMWA_CLOAK
    CLOAKED.set(on);
    if on {
        SetTimer(h, T_UNCLOAK, 500, None);
    } else {
        KillTimer(h, T_UNCLOAK);
        save::on_screen();
        if std::env::args().any(|a| a == "--file-windows") && apps::hwnd().is_none() {
            apps::open(h); // over the window, once that is drawn
        }
        if std::env::args().any(|a| a == "--icons") && icons::hwnd().is_none() {
            icons::open(h);
        }
        // a search switch's help on its own (`--switch-help=N`, N from 0, Timestamps last), for a look at it
        if let Some(n) = std::env::args().find_map(|a| a.strip_prefix("--switch-help=").and_then(|n| n.parse::<usize>().ok())) {
            const SWITCHES: [Hit; 7] = [Hit::Content, Hit::Tags, Hit::MatchCase, Hit::WholeWord, Hit::Regex, Hit::Diacritics, Hit::Timestamps];
            let k = SWITCHES[n.min(6)];
            if let Some(r) = with(|u| u.hits.iter().find(|x| x.1 == k).map(|x| x.0)).flatten() {
                help::open(h, k, r);
            }
        }
    }
}

/// The window's place for the settings: "left,top,right,bottom,maximized" of its normal (restored) size. While
/// full screen, the place it had before.
unsafe fn place_of(h: HWND) -> String {
    let mut wp: WINDOWPLACEMENT = zeroed();
    wp.length = size_of::<WINDOWPLACEMENT>() as u32;
    if let Some(f) = FULL.get() {
        wp = f;
    } else if GetWindowPlacement(h, &mut wp) == 0 {
        return String::new();
    }
    let r = wp.rcNormalPosition;
    format!("{},{},{},{},{}", r.left, r.top, r.right, r.bottom, (wp.showCmd == SW_SHOWMAXIMIZED as u32) as u8)
}

/// [`place_of`]'s text back into a placement.
fn placement(s: &str) -> Option<WINDOWPLACEMENT> {
    let v: Vec<i32> = s.split(',').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [left, top, right, bottom, max] = v[..] else { return None };
    if right - left < 200 || bottom - top < 150 {
        return None;
    }
    let mut wp: WINDOWPLACEMENT = unsafe { zeroed() };
    wp.length = size_of::<WINDOWPLACEMENT>() as u32;
    wp.showCmd = if max != 0 { SW_SHOWMAXIMIZED } else { SW_SHOWNORMAL } as u32;
    wp.rcNormalPosition = RECT { left, top, right, bottom };
    Some(wp)
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
    hidden: bool, // drawn faded, like Explorer
    link: bool,   // .lnk/.url: Windows' shortcut arrow goes over the icon
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
    Sort,   // grid: sort dropdown
    Dir,    // grid: reverse order
    View(usize), // List | Grid
    Preview,     // status bar: the preview pane on / off (Alt+P)
    Smooth,      // status bar: smooth scrolling on / off (off: the wheel moves in steps)
    Browse(u8),  // status bar [Browse|Subfolders]: 1 / 2 lists a folder's contents / everything below it here; neither opens folders in Explorer (Alt+Up goes up)
    Side,        // status bar: the places panel at the left on / off (Alt+B)
    Fold,        // status bar, at its right end: › folds all its buttons away, ‹ brings them back
    MatchCase,   // toolbar: case-sensitive search (Alt+M)
    WholeWord,   // toolbar: whole-word search (Alt+W)
    Regex,       // toolbar: regex search (Alt+E)
    Diacritics,  // toolbar: accent-insensitive search (Alt+D)
    Timestamps,  // toolbar: index created/accessed times for dc:/da: and their sort columns (Alt+T)
    Split,       // the gap between the list and the preview pane: drag to resize the pane
    Col(usize, usize), // a pane's column header: (pane, column)
    ColEdge(usize, usize), // … the gap after a column: drag to resize it, double-click for the default widths
    Search,
    Lang,  // no button of its own now: names the language picker `drop_menu` opens from Tools
    Menu(usize), // the File/Edit/View/Search strip above the tab row
    Back,        // toolbar: previous folder / search (Alt+Left, mouse back button)
    Fwd,         // toolbar: next one (Alt+Right, mouse forward button)
    Tab(usize),  // the tabs, along the menu strip
    TabClose(usize), // … the × on one
    NewTab,      // … the + after them
    Pane(usize),      // a split pane's caption: click to focus it, drag it elsewhere or onto the tab strip
    PaneClose(usize), // … the × on it
    Gutter(usize),    // the gap between two split panes (the split's id): drag to resize
    Group(u64),       // a tab group's label in the strip: click to fold it, drag it to move the group
    TabList,          // ▾ after the tabs: every tab in a menu
    Scroll(bool),     // the strip's ‹ › when its tabs scroll (true: on to the right)
    TreePane(usize, usize), // vertical tabs: a split tab's pane under it (tab, pane)
    SaveName,   // Save window (native_save.rs): the name box
    SaveType,   // … the type dropdown
    SaveOk,     // … Save
    SaveCancel, // … Cancel
    SaveWin,    // … the Windows dialog instead
}

/// How a tab's room is shared: one pane, or two parts side by side (`vert`) or stacked. `shown` is the share `a`
/// has on screen, springing (at `vel`) toward `ratio`.
#[derive(Clone)]
enum Node {
    Leaf(usize),
    Split { id: usize, vert: bool, ratio: f64, shown: f64, vel: f64, a: Box<Node>, b: Box<Node> },
}

impl Node {
    fn is(&self, leaf: usize) -> bool {
        matches!(self, Node::Leaf(l) if *l == leaf)
    }
    fn leaves(&self) -> Vec<usize> {
        match self {
            Node::Leaf(l) => vec![*l],
            Node::Split { a, b, .. } => [a.leaves(), b.leaves()].concat(),
        }
    }
    /// `me` and `new` side by side (side 0 left, 1 right) or stacked (2 top, 3 bottom), `new` on `side`, opening
    /// from nothing.
    fn split_off(me: Node, side: u8, new: Node, id: usize) -> Node {
        let (a, b, shown) = if side.is_multiple_of(2) { (new, me, 0.0) } else { (me, new, 1.0) };
        Node::Split { id, vert: side < 2, ratio: 0.5, shown, vel: 0.0, a: Box::new(a), b: Box::new(b) }
    }
    /// Put `sub` on `side` of pane `target`.
    fn insert(&mut self, target: usize, side: u8, sub: &mut Option<Node>, id: usize) -> bool {
        match self {
            Node::Leaf(l) if *l == target => {
                let Some(new) = sub.take() else { return false };
                *self = Node::split_off(Node::Leaf(target), side, new, id);
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.insert(target, side, sub, id) || b.insert(target, side, sub, id),
        }
    }
    /// Put `sub` (a pane, or a whole tab's) where it was dropped: beside a pane, or along an edge of the whole room.
    fn dock(&mut self, sub: Node, to: Dock, id: usize) {
        match to {
            Dock::Pane(t, side) => {
                self.insert(t, side, &mut Some(sub), id);
            }
            Dock::Root(side) => {
                let old = std::mem::replace(self, Node::Leaf(0));
                *self = Node::split_off(old, side, sub, id);
            }
            Dock::Strip(_) => {}
        }
    }
    /// Every split straight to where it's going: the layout once the springs have settled.
    fn settle(&mut self) {
        if let Node::Split { ratio, shown, vel, a, b, .. } = self {
            (*shown, *vel) = (*ratio, 0.0);
            a.settle();
            b.settle();
        }
    }
    /// Take pane `leaf` out, its sibling getting the room. False when it isn't here, or is all there is.
    fn detach(&mut self, leaf: usize) -> bool {
        let rest = match self {
            Node::Split { a, b, .. } if a.is(leaf) => std::mem::replace(&mut **b, Node::Leaf(0)),
            Node::Split { a, b, .. } if b.is(leaf) => std::mem::replace(&mut **a, Node::Leaf(0)),
            Node::Split { a, b, .. } => return a.detach(leaf) || b.detach(leaf),
            Node::Leaf(_) => return false,
        };
        *self = rest;
        true
    }
    /// Start pane `leaf` shrinking away (its sibling growing over it). False when it is all there is.
    fn collapse(&mut self, leaf: usize) -> bool {
        match self {
            Node::Split { a, ratio, .. } if a.is(leaf) => *ratio = 0.0,
            Node::Split { b, ratio, .. } if b.is(leaf) => *ratio = 1.0,
            Node::Split { a, b, .. } => return a.collapse(leaf) || b.collapse(leaf),
            Node::Leaf(_) => return false,
        }
        true
    }
    /// Whether pane `leaf`'s split has finished moving (a closing pane: it's gone from the screen); None if it has none.
    fn settled(&self, leaf: usize) -> Option<bool> {
        match self {
            Node::Split { a, b, ratio, shown, vel, .. } if a.is(leaf) || b.is(leaf) => Some(shown == ratio && *vel == 0.0),
            Node::Split { a, b, .. } => a.settled(leaf).or_else(|| b.settled(leaf)),
            Node::Leaf(_) => None,
        }
    }
    /// Any split still springing.
    fn moving(&self) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { ratio, shown, vel, a, b, .. } => shown != ratio || *vel != 0.0 || a.moving() || b.moving(),
        }
    }
    fn set_ratio(&mut self, sid: usize, r: f64) {
        if let Node::Split { id, ratio, shown, vel, a, b, .. } = self {
            if *id == sid {
                (*ratio, *shown, *vel) = (r, r, 0.0);
            } else {
                a.set_ratio(sid, r);
                b.set_ratio(sid, r);
            }
        }
    }
    /// One frame of every split's spring; true while any still moves.
    fn animate(&mut self, dt: f64) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { ratio, shown, vel, a, b, .. } => spring(shown, vel, *ratio, dt, 22.0, 0.0005) | a.animate(dt) | b.animate(dt),
        }
    }
    /// Each pane's box in `r`, and each split's (id, box, the gap between its parts, side by side). Panes in
    /// `fold.0` are folded to a bar `fold.1` thick.
    fn place(&self, r: RECT, gap: i32, fold: (&[usize], i32), leaves: &mut Vec<(usize, RECT)>, gutters: &mut Vec<(usize, RECT, RECT, bool)>) {
        match self {
            Node::Leaf(id) => leaves.push((*id, r)),
            Node::Split { id, vert, shown, a, b, .. } => {
                let len = (if *vert { r.right - r.left } else { r.bottom - r.top } - gap).max(0);
                let folded = |n: &Node| matches!(n, Node::Leaf(l) if fold.0.contains(l));
                let k = match (folded(a), folded(b)) {
                    (true, false) => fold.1.min(len),
                    (false, true) => (len - fold.1).max(0),
                    _ => (len as f64 * shown.clamp(0.0, 1.0)).round() as i32,
                };
                let (ra, g, rb) = if *vert {
                    (RECT { right: r.left + k, ..r }, RECT { left: r.left + k, right: r.left + k + gap, ..r }, RECT { left: r.left + k + gap, ..r })
                } else {
                    (RECT { bottom: r.top + k, ..r }, RECT { top: r.top + k, bottom: r.top + k + gap, ..r }, RECT { top: r.top + k + gap, ..r })
                };
                gutters.push((*id, r, g, *vert));
                a.place(ra, gap, fold, leaves, gutters);
                b.place(rb, gap, fold, leaves, gutters);
            }
        }
    }
}

/// A spring pulling `x` (moving at `v`) to `to`, critically damped: it picks up speed and settles, never past `to`
/// (no bounce); retargeted mid-way, it bends toward the new spot keeping its speed. `w` is its speed. One frame of
/// `dt` seconds; false once it has come to rest (within `snap`) on `to`.
fn spring(x: &mut f64, v: &mut f64, to: f64, dt: f64, w: f64, snap: f64) -> bool {
    if *x == to && *v == 0.0 {
        return false;
    }
    let n = (dt / 0.002).ceil().max(1.0);
    let h = dt / n;
    for _ in 0..n as usize {
        *v += (w * w * (to - *x) - 2.0 * w * *v) * h;
        *x += *v * h;
    }
    if (to - *x).abs() < snap && v.abs() < snap * w {
        (*x, *v) = (to, 0.0);
    }
    true
}

/// A tab: its panes, the one focused, where its button is drawn (`x`, springing at `v` toward its slot `to`), and
/// when it opened, while it's still unfolding into its slot.
struct Tab {
    root: Node,
    focus: usize,
    x: f64,
    v: f64,
    to: f64,
    born: Option<Instant>,
    uid: u64,       // stays the tab's as tabs move around
    used: u64,      // when it was last shown (`Ui::tick_n`): the Ctrl+Tab order, and which tabs sleep first
    o: TabOpts,     // its title, colour, pin… (native_tabs.rs)
}
impl Tab {
    fn new(pane: usize) -> Tab {
        Tab { root: Node::Leaf(pane), focus: pane, x: f64::NAN, v: 0.0, to: 0.0, born: None, uid: new_uid(), used: 0, o: TabOpts::default() }
    }
}
/// A closed tab folding away where it was: its slot, how it looked, and when it closed.
struct Ghost {
    r: RECT,
    face: Face,
    at: Instant,
}
/// How long a tab takes to unfold as it opens, and to fold away as it closes.
const TAB_IN: f64 = 0.22;
const TAB_OUT: f64 = 0.18;
/// Ease-out cubic of `t` in 0..1: quick to start, gentle to land.
fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// A tab or a pane caption being dragged: where it was grabbed, whether it has moved enough to count as a drag,
/// whether the tab on screen was lifted out (the window showing another one meanwhile), and where it would land.
struct Drag {
    grab: Grab,
    x0: i32,
    y0: i32,
    on: bool,
    lifted: bool,
    to: Option<Dock>,
}
#[derive(Clone, Copy, PartialEq)]
enum Grab {
    Tab(usize),
    Pane(usize),
    Group(u64),
}
/// Where a dragged tab or pane lands: on a side of a pane (0 left, 1 right, 2 top, 3 bottom), of the whole tab, or
/// in the tab strip at an index.
#[derive(Clone, Copy, PartialEq)]
enum Dock {
    Pane(usize, u8),
    Root(u8),
    Strip(usize),
}

/// The landing zone shown while dragging: a translucent accent box springing (`r` at `v`) to `to`, fading in `a`.
/// Drawn in a window of its own ([`Zone`]) over the main one, so moving it repaints nothing beneath.
struct Ov {
    r: [f64; 4],
    v: [f64; 4],
    to: [f64; 4],
    a: f64,
    on: bool,
    label: String,
}

/// The landing zone's window: layered, click-through, above the main window and below the chip. Its picture is
/// drawn into a premultiplied bitmap (`bmp` in `dc`, `cap` big, grown as needed) and handed to the compositor.
struct Zone {
    hwnd: HWND,
    cv: Canvas,
    dc: HDC,
    bmp: HBITMAP,
    cap: (i32, i32),
}

/// The lists Mosaic makes itself rather than reading a folder: (search, name's key, Explorer's own place for it).
const LISTS: [(&str, &str, &str); 6] = [
    ("home:", "side.home", "shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}"),
    ("recent:", "side.recent_files", "shell:recent"),
    ("recent-dirs:", "side.recent_folders", "shell:::{22877a6d-37a1-461a-91b0-dbda5aaebc99}"),
    ("pc:", "side.this_pc", "shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"),
    ("net:", "side.network", "shell:::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}"),
    ("linux:", "side.linux", "shell:::{B2B4A4D1-2754-4140-A2EB-9A76D9D7CDC6}"),
];
fn list_of(q: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    LISTS.iter().find(|l| l.0 == q.trim())
}

/// What a tab is called: the folder it lists (and any words narrowing it), All, Home, or its search.
fn place_name(q: &str) -> String {
    let q = q.trim();
    match q {
        "" => return t("side.all").into(),
        _ => {}
    }
    if let Some(l) = list_of(q) {
        return t(l.1).into();
    }
    let Some(d) = listed_dir(q) else { return q.into() };
    // a share's top (`\\wsl.localhost\Ubuntu`) has no file name to Rust: its last part, as for any folder
    let name = std::path::Path::new(&d).file_name().map_or_else(|| d.trim_end_matches('\\').rsplit('\\').next().unwrap_or(&d).to_string(), |n| n.to_string_lossy().into_owned());
    match q.splitn(3, '"').nth(2).map(str::trim).filter(|r| !r.is_empty()) {
        Some(rest) => format!("{name} › {rest}"),
        None => name,
    }
}

enum Press {
    Item { i: usize, defer: bool, x: i32, y: i32, again: bool }, // `again`: on the name of the only selected item
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
    // on the selection
    Open,
    OpenHere, // a folder, listed in Mosaic
    OpenTab,  // … in a new tab
    Reveal,
    Props,
    Copy,
    Cut,
    Path,
    Rename,
    Del,
    DelNow, // skip the Recycle Bin
    SearchIn,
    // on the folder on screen (`Ui::here`), or the view: no selection needed
    Paste,
    NewFolder,
    HereProps,
    Refresh,
    SelectAll,
    Undo,
    View(i32), // grid tile size, 0: the list
    Sort(&'static str),
    Desc(bool),
}

struct Job {
    seq: u64,
    who: usize, // the pane asking
    args: SearchArgs,
    h: SearchHandle,
}
/// Searches are numbered across all panes, so a result finds the pane that asked for it.
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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
    fx: Option<Instant>, // a live refresh's animation: when it started
    slide: HashMap<String, (f64, f64)>, // item → where it was on screen before the refresh, from where it is now
    born: HashSet<String>, // items the refresh brought in: outlined for a moment
    scale: f64,
    gfx: Gfx,
    cm: Canvas, // main window
    tab_png: HashMap<String, Option<Vec<u8>>>, // tabs' places' shell icons by source (None: asked, or none)
    tab_bmp: HashMap<String, (Bitmap, u32)>,    // … made on `cm` (its `gen`)
    cl: Canvas, // list
    edit_font: HFONT,
    naming: Option<usize>,                          // the tab whose title is being typed over (see `TA::Rename`)
    name_slot: std::cell::Cell<Option<(RECT, u32)>>, // where its title was last drawn, and on what
    backend: SearchBackend,
    jobs: mpsc::Sender<Job>,
    args: SearchArgs,  // what the controls say
    shown: SearchArgs, // what the rows on screen came from (pages are fetched with these)
    seq: u64,
    fresh: bool, // the pending result is a new search: reset scroll and selection when it lands
    typing: bool,
    terms: Vec<Vec<u16>>,
    matches: usize,
    version: Option<u64>, // the listing's version the rows came from (`SearchResult::stamp`)
    objects: usize,
    changes: u64, // the index's change counter when the rows were fetched
    scanning: bool,
    error: Option<String>,
    offered: bool,
    err_polls: u32, // consecutive polls that found the service unreachable
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
    fast: Option<Instant>, // since files changed here: the index is asked often for a moment, the lists keep up with it
    hits: Vec<(RECT, Hit)>,
    head_h: i32,
    menu_h: i32, // the File/Edit/View/Search strip above the tab row
    search_box: RECT,
    lh: RECT,
    status_top: i32, // the status bar along the bottom
    save_top: i32,   // a Save/Open window's bar under it (the window's height when none)
    status_bot: i32, // where the status bar ends (under the search bar in a Save/Open window)
    sys: HashMap<String, i32>,
    himl: isize,
    isz: i32,
    thousands: Vec<u16>,
    wheel_lines: f64,
    smooth: bool, // glide on wheel / scrollbar paging, else jump
    folded: bool, // the status bar's buttons folded away behind its ‹
    folder_color: bool, // folder names in the accent colour (off: plain text, like files)
    folder_icons: u8,   // View > Icons…: a tile of a folder of one kind of file shows 0 that kind's folder, 1 what's inside, 2 both
    link_arrow: u8,     // … the arrow on a shortcut's tile: an index into `ARROWS`
    link_size: u8,      // … how big, in % of Windows' own size (`ARROW_SIZES`)
    hist: std::cell::RefCell<(Vec<String>, usize)>, // folders / searches visited (browse_to) and where we are in them
    tabs: Vec<Tab>,
    ghosts: Vec<Ghost>, // tabs just closed, folding away
    plus: (f64, f64, f64), // the + after the tabs: where it's drawn, its speed, where it's going
    cur: usize, // … the one shown
    last: usize, // the pane focused in the tab shown before (a lifted tab gives way to it)
    // split panes: the focused one's state is in the fields here (see `View`), the others' wait in `panes`
    id: usize,                  // the pane whose state the fields hold
    panes: Vec<Option<View>>,   // by pane id: None for the focused pane (and closed ones)
    guest: bool,                // another pane is swapped in for a moment (`with_pane`)
    relayout: bool,             // … and asked for a layout, done once it's swapped back
    cap: RECT,                  // this pane's caption (split tabs only)
    org: POINT,                 // where its list window is, in the main window
    areas: Vec<(usize, RECT)>,  // the panes on screen: caption to bottom, in the main window
    gutters: Vec<(usize, RECT, RECT, bool)>, // the splits on screen: (id, box, gap, side by side)
    content: RECT,              // the room the panes share
    splits: usize,              // split ids handed out
    dying: Vec<usize>,          // panes closing: shrinking away, then dropped
    drag: Option<Drag>,
    hole: Option<usize>,        // a pane dragged over the tab strip: the slot opened for it
    ov: Option<Ov>,
    zone: Option<Zone>,
    chip: Option<(HWND, Canvas, String, bool)>, // what's being dragged (a split tab?), following the pointer
    gut: Option<usize>,         // dragging a split's gap
    gut_to: Option<(i32, i32)>, // … to where the pointer last was, applied on the next frame
    gut_lit: (usize, f64),      // the gap whose grip shows, and how lit it is
    tick: Instant,              // the last frame of the window-wide animations
    browse: u8, // 0: open folders in Explorer; 1: list what's directly inside (child:); 2: list everything below (descendant:)
    timestamps: bool, // index created/accessed times for dc:/da: and their sort columns (Alt+T)
    rtl: bool, // Arabic UI: columns and icon/text order mirror right-to-left (see `col_positions`)
    colw: (f64, [i32; 3]), // the list's columns, every pane's: Name's share of Name + Path, then Size, Type, Modified (CSS px)
    col_drag: Option<(usize, usize, i32, [i32; 5])>, // dragging the edge after column k: (pane, k, where it was pressed, the widths then)
    col_gen: u32, // bumped as `colw` changes: the rows' laid-out text is stale
    grid: bool,
    size: i32,  // grid: tile size (CSS px), the zoom level
    def_view: (bool, i32), // (grid, size) of a folder with none of its own, and of a search
    windows: bool, // [Windows|List|Grid]: each folder its own view and sort, as Explorer's; else List or Grid everywhere
    per: usize, // grid: tiles per row, as many as fit
    zoom_acc: i32, // Ctrl+wheel delta not yet a whole notch (precision touchpads)
    step_acc: i32, // likewise, for wheel steps with smooth scrolling off
    live_at: Instant,      // when the last live refresh finished (while one runs: when it started)
    live_cost: Duration,   // how long it took, search included: the next one waits 8x that (a busy disk changes the index constantly)
    poll_y: f64,           // scroll position at the previous poll: moving = scrolling, no refresh
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
    texts_key: (i32, bool, i32, u32),
    sys_pics: HashMap<i32, Option<Pic>>,
    own: HashMap<String, Option<Pic>>,
    pending: HashSet<String>,
    thumbs: HashMap<String, (Option<Pic>, i32)>, // path → thumbnail and the tile size it was made for
    thumb_order: VecDeque<String>,
    thumb_bytes: usize,
    thumb_pending: HashSet<String>,
    link_ov: HashMap<i32, Option<Pic>>, // the shortcut arrow, made at each size it's drawn at
    kinds: HashMap<String, (u8, u64)>,      // folder path -> what its files all are (see `crate::folder_kind`); 0 = mixed
    kind_pending: HashSet<String>,
    special: HashSet<String>,        // Desktop, Downloads, Documents, Pictures, Music, Videos: they keep their own shell icon
    kind_checked: HashSet<String>,   // folders whose cached kind was compared with their modified time this run
    kinds_changed: Vec<String>,      // folders in `kinds` changed (or dropped) since the last save
    kinds_saved: std::time::Instant,
    kind_dirs: Vec<Option<String>>,  // the Windows folder whose icon each kind wears (Pictures, Videos, Music, Documents)
    jumbo_png: HashMap<String, Vec<u8>>,            // type key → the shell's 256 px icon
    jumbo: HashMap<(String, i32), Option<Pic>>,     // (type key, px) → decoded at tile size
    busy: Option<Instant>, // its search hasn't answered yet: since when
    want_y: Option<f64>,  // where a restored pane scrolls to once its first result lands
    // tabs beyond the basics (native_tabs.rs)
    tp: TabPrefs,               // how tabs look and behave, saved with the settings
    groups: Vec<Group>,
    closed: Vec<(TabSpec, usize)>, // closed tabs, newest last, and where they were
    tick_n: u64,                // bumped on every tab switch: `Tab::used`
    slot_rs: Vec<RECT>,         // each tab's slot (with any hole), by tab index
    hole_r: RECT,               // the hole a pane dragged over the strip opened
    strip_r: RECT,              // the strip's whole area
    scroll: (f64, i32),         // scrolling tabs: how far along, and how far it can go
    card: Option<(HWND, Canvas, Panel)>, // the card shown over the window: a tab pointed at, the switcher, a note
    card_for: Option<usize>,    // … the tab it waits to show for
    saved: String,              // the session as last written (without scroll positions)
    polls: u32,
    secondary: bool,            // a window opened from another (a tab dragged out): never writes the session
    merged: bool,               // … emptied into another window: nothing to save on the way out
    cmp: HashMap<String, u8>,   // compare mode: file → 1 only here, 2 differs (from the other pane's folder)
}

/// What each pane of a split has its own of. The focused pane's live in `Ui`'s fields of the same names, where all
/// the list code reads them; the others' wait in `Ui::panes`, swapped in for a moment to paint, scroll or take a
/// search result (`Ui::with_pane`).
macro_rules! view {
    ($($f:ident: $t:ty,)*) => {
        struct View { $($f: $t,)* }
        impl Ui {
            fn swap_view(&mut self, v: &mut View) {
                $(std::mem::swap(&mut self.$f, &mut v.$f);)*
            }
        }
    };
}
view! {
    id: usize,
    list: HWND,
    cl: Canvas,
    args: SearchArgs,
    shown: SearchArgs,
    seq: u64,
    fresh: bool,
    terms: Vec<Vec<u16>>,
    matches: usize,
    version: Option<u64>,
    objects: usize,
    changes: u64,
    pages: HashMap<usize, Vec<Row>>,
    order: VecDeque<usize>,
    y: f64,
    target: f64,
    frame: Instant,
    glide: (f64, Instant),
    sel: Sel,
    focus: Option<usize>,
    anchor: Option<usize>,
    hover: Option<usize>,
    tracking: bool,
    press: Option<Press>,
    auto: Option<Auto>,
    fx: Option<Instant>,
    slide: HashMap<String, (f64, f64)>,
    born: HashSet<String>,
    lh: RECT,
    cap: RECT,
    org: POINT,
    hist: std::cell::RefCell<(Vec<String>, usize)>,
    grid: bool,
    size: i32,
    per: usize,
    zoom_acc: i32,
    step_acc: i32,
    live_at: Instant,
    live_cost: Duration,
    poll_y: f64,
    meta_gen: usize,
    tip_item: Option<usize>,
    sb_hot: bool,
    sb_drag: Option<i32>,
    pics_gen: u32,
    texts: HashMap<usize, Vec<Text>>,
    texts_key: (i32, bool, i32, u32),
    sys_pics: HashMap<i32, Option<Pic>>,
    own: HashMap<String, Option<Pic>>,
    pending: HashSet<String>,
    thumbs: HashMap<String, (Option<Pic>, i32)>,
    thumb_order: VecDeque<String>,
    thumb_bytes: usize,
    thumb_pending: HashSet<String>,
    link_ov: HashMap<i32, Option<Pic>>,
    jumbo: HashMap<(String, i32), Option<Pic>>,
    busy: Option<Instant>,
    want_y: Option<f64>,
}

thread_local!(static UI: RefCell<Option<Ui>> = const { RefCell::new(None) });

/// `f` on the pane whose list window is `h` (see `Ui::on_list`).
fn withp<R>(h: HWND, f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    with(|u| u.on_list(h, f)).flatten()
}

/// The tooltip watches list `list` itself: grid tiles show their path on hover (`Ui::set_tip`).
unsafe fn add_tip(tip: HWND, list: HWND, add: bool) {
    let mut ti: windows_sys::Win32::UI::Controls::TTTOOLINFOW = zeroed();
    ti.cbSize = size_of::<windows_sys::Win32::UI::Controls::TTTOOLINFOW>() as u32;
    ti.uFlags = 0x10; // TTF_SUBCLASS
    ti.hwnd = list;
    ti.uId = 1;
    let mut empty = [0u16; 1];
    ti.lpszText = empty.as_mut_ptr();
    SendMessageW(tip, WM_USER + if add { 50 } else { 51 }, 0, &ti as *const _ as isize); // TTM_ADDTOOLW / TTM_DELTOOLW
}

/// Run `f` on the UI state. `None` when it is already borrowed further up this thread's stack (a nested window
/// message): callers then fall back to default handling or re-post. Modal things (menus, message boxes, drags)
/// run outside `with`, so their message loops can reach the state.
fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

// ---- window identity: the two UIs get their own name, icon and taskbar button ---------------------------------
macro_rules! icon_set {
    ($n:literal) => {
        icon_set!("../icons/", $n)
    };
    ($dir:literal, $n:literal) => {
        [
            (16, &include_bytes!(concat!($dir, $n, "-16.png"))[..]),
            (20, include_bytes!(concat!($dir, $n, "-20.png"))),
            (24, include_bytes!(concat!($dir, $n, "-24.png"))),
            (32, include_bytes!(concat!($dir, $n, "-32.png"))),
            (40, include_bytes!(concat!($dir, $n, "-40.png"))),
            (48, include_bytes!(concat!($dir, $n, "-48.png"))),
            (64, include_bytes!(concat!($dir, $n, "-64.png"))),
            (256, include_bytes!(concat!($dir, $n, "-256.png"))),
        ]
    };
}

/// Separate taskbar group for this process (both UIs are the same exe). Call before any window exists.
pub fn app_id(id: &str) {
    unsafe { windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(wz(id).as_ptr()) };
}

/// The installer's icon presets (icons/presets-src/*.svg, rendered by tools/gen-presets.ps1), picked per process type.
macro_rules! presets {
    ($($n:literal),*) => {
        fn preset_set(name: &str) -> Option<[(i32, &'static [u8]); 8]> {
            match name {
                $($n => Some(icon_set!("../icons/presets/", $n)),)*
                _ => None,
            }
        }
    };
}
presets!(
    "crimson",
    "crimson-bis",
    "gold",
    "gold-bis",
    "graphite",
    "graphite-bis",
    "green",
    "green-bis",
    "light",
    "light-bis",
    "mint",
    "mint-bis",
    "navy",
    "navy-bis",
    "ocean",
    "ocean-bis",
    "purple",
    "purple-bis",
    "rose",
    "rose-bis",
    "sunset",
    "sunset-bis"
);

/// Title-bar and taskbar icons for `hwnd`: the exe's own icon (what the installer stamped), at each size for the window's DPI.
pub fn set_icons(hwnd: HWND) {
    unsafe {
        let (inst, dpi) = (GetModuleHandleW(null()), GetDpiForWindow(hwnd));
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let px = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(metric, dpi);
            let h = LoadImageW(inst, 32512 as *const u16, IMAGE_ICON, px, px, 0);
            if !h.is_null() {
                SendMessageW(hwnd, WM_SETICON, which as usize, h as isize);
            }
        }
    }
}

/// `mosaic.exe`: this exe with its own name and icon. Started without arguments it opens the window. A dev build
/// (cargo's `mosaic-wrapper.exe`) makes or refreshes it beside itself on start.
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
        // our manifest (common controls v6, asInvoker, long paths, segment heap) in place of any the linker added;
        // version and icons are replaced too
        let h = windows_sys::Win32::System::LibraryLoader::BeginUpdateResourceW(wz(&tmp.to_string_lossy()).as_ptr(), 1);
        if h.is_null() {
            return Err(err());
        }
        let put = |ty: u16, id: u16, data: &[u8]| UpdateResourceW(h, ty as _, id as _, 0, data.as_ptr() as _, data.len() as u32);
        put(24, 1, include_bytes!("../mosaic.manifest")); // RT_MANIFEST #1
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

/// `--dist <dir> [role=preset ...]`: the named, iconed exes an installer ships, with the icon chosen for each process
/// type (roles main / index / content / thumbs; anything else or unnamed = its default icon). The choices are kept in
/// `<dir>\icons.txt` for the service's `mosaic-index.exe`, which is made later, at service install.
/// The preset each process type wears unless another is chosen (the installer preselects the same ones).
fn default_set(role: &str) -> [(i32, &'static [u8]); 8] {
    let name = match role {
        "main" => "light-bis",
        "index" => "sunset-bis",
        "content" => "purple-bis",
        _ => "green-bis",
    };
    preset_set(name).expect("built-in preset")
}

pub fn dist(dir: &std::path::Path, choice: &[(String, String)]) -> std::io::Result<()> {
    let me = std::env::current_exe()?;
    std::fs::create_dir_all(dir)?;
    let pick = |role: &str, default: [(i32, &'static [u8]); 8]| choice.iter().find(|(r, _)| r == role).and_then(|(_, n)| preset_set(n)).unwrap_or(default);
    // helpers last: an exe older than its helpers is never "refreshed" at run time (a read-only Program Files can't)
    stamp(&me, &dir.join("mosaic.exe"), "Mosaic", pick("main", default_set("main")))?;
    stamp(&me, &dir.join("mosaic-content.exe"), "Mosaic Content Index", pick("content", default_set("content")))?;
    stamp(&me, &dir.join("mosaic-thumbs.exe"), "Mosaic Thumbnails", pick("thumbs", default_set("thumbs")))?;
    stamp(&me, &dir.join("mosaic-dialogs.exe"), "Mosaic Save Window", pick("thumbs", default_set("thumbs")))?;
    std::fs::write(dir.join("icons.txt"), choice.iter().map(|(r, n)| format!("{r}={n}
")).collect::<String>())
}

/// The service exe's icons: the preset chosen for `index` in `<dir>\icons.txt`, else the default.
pub fn index_icons(dir: &std::path::Path) -> [(i32, &'static [u8]); 8] {
    let txt = std::fs::read_to_string(dir.join("icons.txt")).unwrap_or_default();
    txt.lines().find_map(|l| l.strip_prefix("index=")).and_then(|n| preset_set(n.trim())).unwrap_or(default_set("index"))
}

/// `mosaic-content.exe`: the `--extract` IFilter helper, stamped with its own name and icon so a
/// burst of short-lived extraction subprocesses reads as "Mosaic Content Index" in Task Manager
/// instead of flickering under whatever launched it.
pub fn content_exe() -> std::io::Result<std::path::PathBuf> {
    stamped_helper("mosaic-content.exe", "Mosaic Content Index", default_set("content"))
}

/// `mosaic-dialogs.exe`: the `--dialogs` watcher that puts Mosaic in place of Firefox's Save window (native_save.rs).
pub fn dialogs_exe() -> std::io::Result<std::path::PathBuf> {
    stamped_helper("mosaic-dialogs.exe", "Mosaic Save Window", default_set("thumbs"))
}

/// `mosaic-thumbs.exe`: the `--shell-helper` thumbnail/icon workers (up to 4, one per worker
/// thread), stamped so they read as "Mosaic Thumbnails" instead of duplicating the GUI's name.
pub fn thumbs_exe() -> std::io::Result<std::path::PathBuf> {
    stamped_helper("mosaic-thumbs.exe", "Mosaic Thumbnails", default_set("thumbs"))
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
    // started from inside a folder, the window (and its helpers) would hold it open: it couldn't be moved or deleted
    if let Some(d) = std::env::current_exe().ok().as_deref().and_then(std::path::Path::parent) {
        let _ = std::env::set_current_dir(d);
    }
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        dark_menus();
        std::thread::spawn(shell::menu_warm); // the right-click menu's process, so the first one is quick too
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
        set_icons(main);
        let edit = CreateWindowExW(0, wz("EDIT").as_ptr(), null(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, main, null_mut(), inst, null());
        SendMessageW(edit, 0x1501, 1, wz("Search file and folder names…").as_ptr() as isize); // EM_SETCUEBANNER
        SetWindowTheme(edit, wz("DarkMode_CFD").as_ptr(), null()); // dark caret/selection/context menu
        let list = CreateWindowExW(0, cl.as_ptr(), null(), WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN, 0, 0, 10, 10, main, null_mut(), inst, null()); // the rename field sits on it

        // the grid's jumbo type icons come from SHGetImageList, which needs COM on this thread; OLE takes drops
        windows_sys::Win32::System::Ole::OleInitialize(null_mut());
        dnd::register(main);
        dnd::register(list);
        let tip = CreateWindowExW(WS_EX_TOPMOST, wz("tooltips_class32").as_ptr(), null(), 0x02 | 0x01, 0, 0, 0, 0, main, null_mut(), inst, null()); // TTS_NOPREFIX | TTS_ALWAYSTIP
        SetWindowTheme(tip, wz("DarkMode_Explorer").as_ptr(), null());
        add_tip(tip, list, true);
        SendMessageW(tip, WM_USER + 24, 0, 900); // TTM_SETMAXTIPWIDTH: allow the path/details line break

        // tool 2: the toolbar/status-bar buttons, in `main`'s own client coordinates (`Ui::set_toolbar_tip`
        // moves this tool's rect to whichever hit is hot, as it changes on WM_MOUSEMOVE)
        let mut ti2: windows_sys::Win32::UI::Controls::TTTOOLINFOW = zeroed();
        ti2.cbSize = size_of::<windows_sys::Win32::UI::Controls::TTTOOLINFOW>() as u32;
        ti2.uFlags = 0x10; // TTF_SUBCLASS
        ti2.hwnd = main;
        ti2.uId = 2;
        let mut empty = [0u16; 1];
        ti2.lpszText = empty.as_mut_ptr();
        SendMessageW(tip, WM_USER + 50, 0, &ti2 as *const _ as isize); // TTM_ADDTOOLW

        let st = load_settings();
        let get = |k: &str| st.get(k).cloned().unwrap_or_default();
        save::load(&st);
        save::begin(); // `--save`: this window answers a Save dialog
        set_theme(&get("theme"));
        apply_titlebar(main);
        if !get("lang").is_empty() {
            crate::i18n::set_lang(&get("lang"));
        }
        crate::datestore::set_enabled(get("timestamps") == "1");
        let scale = GetDpiForWindow(main) as f64 / 96.0;
        let pane = preview::create(main, scale);
        let side = side::create(main, scale, &get("sidebar"));
        // language is fixed for the process's lifetime (set from settings above, no in-app switcher), so RTL fonts
        // can be built once here rather than needing a rebuild-on-switch path.
        let rtl = crate::i18n::rtl();
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
            meta: String::new(), // names only (the Names / type dropdown is gone)
            tags: get("tags") == "1",
            content: get("content") == "1",
            match_case: get("match_case") == "1",
            whole_word: get("whole_word") == "1",
            regex: get("regex") == "1",
            diacritics: get("diacritics") == "1",
            hidden: get("hidden") == "1",
            system: get("system") == "1",
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
            fx: None,
            slide: HashMap::new(),
            born: HashSet::new(),
            scale,
            cm: Canvas::new(main, &gfx),
            tab_png: HashMap::new(),
            tab_bmp: HashMap::new(),
            cl: Canvas::new(list, &gfx),
            gfx,
            edit_font: edit_font(scale),
            naming: None,
            name_slot: std::cell::Cell::new(None),
            backend,
            jobs: worker(main),
            shown: args.clone(),
            args,
            seq: 0,
            fresh: true,
            typing: false,
            terms: Vec::new(),
            matches: 0,
            version: None,
            objects: 0,
            changes: 0,
            scanning: false,
            error: None,
            offered: false,
            err_polls: 0,
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
            fast: None,
            hits: Vec::new(),
            head_h: 0,
            menu_h: 0,
            search_box: zeroed(),
            lh: zeroed(),
            status_top: 0,
            save_top: 0,
            status_bot: 0,
            sys: HashMap::new(),
            himl: 0,
            isz: 16,
            thousands: thousands(),
            wheel_lines: lines.max(1) as f64,
            smooth: get("smooth") != "0",
            folded: get("folded") == "1",
            folder_color: get("folder_color") == "1",
            folder_icons: get("folder_icons").parse().map_or(2, |v: u8| v.min(2)),
            link_arrow: get("link_arrow").parse().map_or(0, |v: u8| v.min(ARROWS.len() as u8 - 1)),
            link_size: get("link_size").parse().map_or(100, |v: u8| v.clamp(*ARROW_SIZES.start(), *ARROW_SIZES.end())),
            hist: Default::default(),
            tabs: vec![Tab::new(0)],
            ghosts: Vec::new(),
            plus: (f64::NAN, 0.0, 0.0),
            cur: 0,
            last: 0,
            id: 0,
            panes: vec![None],
            guest: false,
            relayout: false,
            cap: zeroed(),
            org: POINT { x: 0, y: 0 },
            areas: Vec::new(),
            gutters: Vec::new(),
            content: zeroed(),
            splits: 0,
            dying: Vec::new(),
            drag: None,
            hole: None,
            ov: None,
            zone: None,
            chip: None,
            gut: None,
            gut_to: None,
            gut_lit: (usize::MAX, 0.0),
            tick: Instant::now(),
            browse: get("browse").parse().unwrap_or(0).min(2),
            timestamps: get("timestamps") == "1",
            rtl,
            colw: parse_cols(&get("cols")),
            col_drag: None,
            col_gen: 0,
            grid: get("view") == "grid",
            size: get("size").parse().unwrap_or(128).clamp(MIN_SIZE, MAX_SIZE),
            def_view: (get("view") == "grid", get("size").parse().unwrap_or(128).clamp(MIN_SIZE, MAX_SIZE)),
            windows: get("windows_view") != "0",
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
            texts_key: (0, false, 0, 0),
            sys_pics: HashMap::new(),
            own: HashMap::new(),
            pending: HashSet::new(),
            thumbs: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_bytes: 0,
            thumb_pending: HashSet::new(),
            link_ov: HashMap::new(),
            kinds: load_kinds(),
            kind_pending: HashSet::new(),
            special: side::special_dirs(),
            live_at: Instant::now(),
            live_cost: Duration::ZERO,
            poll_y: 0.0,
            kind_checked: HashSet::new(),
            kinds_changed: Vec::new(),
            kinds_saved: std::time::Instant::now(),
            kind_dirs: (0..5).map(|k| if k == 0 { None } else { side::kind_dir(k) }).collect(),
            jumbo_png: HashMap::new(),
            jumbo: HashMap::new(),
            busy: None,
            want_y: None,
            tp: serde_json::from_str(&get("tabs")).unwrap_or_default(),
            groups: Vec::new(),
            closed: Vec::new(),
            tick_n: 0,
            slot_rs: Vec::new(),
            hole_r: zeroed(),
            strip_r: zeroed(),
            scroll: (0.0, 0),
            card: None,
            card_for: None,
            saved: String::new(),
            polls: 0,
            secondary: false,
            merged: false,
            cmp: HashMap::new(),
        };
        std::thread::spawn(|| {
            crate::home_query(); // warm Home's folder list so the first click on it is instant
        });
        ui.sys_icon(&None, true); // loads the system image list
        if ui.himl != 0 {
            let (mut cx, mut cy) = (16, 16);
            ImageList_GetIconSize(ui.himl as _, &mut cx, &mut cy);
            ui.isz = cy;
        }
        SendMessageW(edit, WM_SETFONT, ui.edit_font as usize, 0);
        save::create(main, ui.edit_font);
        ui.run(false);
        UI.with(|u| *u.borrow_mut() = Some(ui));
        with(|u| u.restore());
        // `--file-windows` / `--icons`: Tools > File windows… or View > Icons… on its own, its window gone with it
        // (the session left alone)
        let file_windows = std::env::args().any(|a| a == "--file-windows" || a == "--icons");
        if file_windows {
            with(|u| u.secondary = true);
        }
        with(|u| u.layout());
        SetTimer(main, T_POLL, 500, None);
        // where and how big it was when last closed (not a window torn off another: that one opens at the cursor)
        // (a Save/Open window keeps its own)
        let wp = if save::active() { st.get("save_place") } else { st.get("place").filter(|_| with(|u| !u.secondary) == Some(true)) }.and_then(|s| placement(s));
        cloak(main, true);
        match wp {
            Some(wp) => {
                SetWindowPlacement(main, &wp); // shows it, maximized or not; moved onto a screen if its own is gone
            }
            None if save::active() => save::show(main),
            None => {
                ShowWindow(main, SW_SHOW);
            }
        }
        SetFocus(edit);
        save::shown();
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
            if let Some((lists, pane, main)) = with(|u| {
                u.step();
                (u.visible_lists(), [u.pane, u.side], u.main)
            }) {
                // one frame: all painted together (a divider drag moves them all, a theme change recolours them all)
                for list in lists {
                    UpdateWindow(list);
                }
                for w in pane.into_iter().chain(themes::hwnd()) {
                    UpdateWindow(w);
                }
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
    // a shortcut to a folder opens that folder here (with Browse on); any other shortcut is run by the shell
    let low = p.to_ascii_lowercase();
    let linked = if low.ends_with(".lnk") { shell::link_target(p) } else if low.ends_with(".url") { shell::url_target(p) } else { None };
    let p = match &linked {
        Some(t) if std::fs::metadata(t).is_ok_and(|m| m.is_dir()) => t.as_str(),
        _ => p,
    };
    if save::active() {
        // a Save/Open/folder window: a folder opens here, a file may be the answer
        if let Some(main) = with(|u| u.main) {
            unsafe { save::open(main, p) };
        }
        return;
    }
    if with(|u| u.browse > 0).unwrap_or(false) && (std::fs::metadata(p).is_ok_and(|m| m.is_dir()) || crate::direct::server(p)) {
        with(|u| u.go_dir(p));
    } else {
        crate::recent::note(p, std::fs::metadata(p).is_ok_and(|m| m.is_dir()));
        side::sync();
        shell::open(p);
    }
}

unsafe fn dispatch(msg: &MSG) {
    if themes::hwnd().or(apps::hwnd()).or(icons::hwnd()).or(help::hwnd()).is_some_and(|h| h == msg.hwnd || IsChild(h, msg.hwnd) != 0) {
        // the theme or file windows dialog: the window behind waits, its shortcuts with it
        TranslateMessage(msg);
        DispatchMessageW(msg);
        return;
    }
    if msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN {
        let a = with(|u| {
            let in_list = u.on_list(msg.hwnd, |_| ()).is_some();
            (msg.hwnd == u.main || msg.hwnd == u.edit || in_list).then(|| u.tab_key(msg.wParam as u16, in_list)).flatten()
        });
        if let Some(Some(a)) = a {
            return tab_act(a);
        }
    }
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
        // … and the split pane under it, focused or not
        if under != msg.hwnd && with(|u| u.on_list(under, |_| ()).is_some()) == Some(true) {
            SendMessageW(under, msg.message, msg.wParam, msg.lParam);
            return;
        }
    }
    if save::is_name(msg.hwnd) {
        // the name box: Enter answers, Esc cancels; for a Save, nothing a file name can't have (Open and folder
        // take paths and "a" "b")
        let main = GetParent(msg.hwnd);
        match msg.message {
            WM_KEYDOWN if msg.wParam == VK_RETURN as usize => return save::commit(main),
            WM_KEYDOWN if msg.wParam == VK_ESCAPE as usize => return save::cancel(main),
            WM_CHAR if save::kind_of() == Some(save::Kind::Save) && char::from_u32(msg.wParam as u32).is_some_and(|c| BAD_NAME.contains(c)) => return bad_name_tip(msg.hwnd),
            _ => {}
        }
    }
    if msg.message == WM_KEYDOWN && msg.wParam == VK_ESCAPE as usize && with(|u| u.drag.as_ref().is_some_and(|d| d.on)) == Some(true) {
        with(|u| u.drag_end(false));
        ReleaseCapture();
        return;
    }
    if msg.message == WM_KEYDOWN && matches!(msg.wParam as u16, VK_RETURN | VK_ESCAPE) && RENAMING.with(|r| r.borrow().as_ref().is_some_and(|r| r.0 == msg.hwnd)) {
        return end_rename(msg.wParam as u16 == VK_RETURN);
    }
    if msg.message == WM_SYSKEYDOWN && msg.wParam == VK_RETURN as usize {
        // Alt+Enter: Properties, of the selection or else the folder on screen
        return act(if with(|u| u.sel.count() > 0) == Some(true) { Act::Props } else { Act::HereProps });
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
    if msg.message == WM_KEYDOWN && msg.wParam == VK_BACK as usize && key_down(VK_CONTROL) && is_edit(msg.hwnd) {
        // the WM_CHAR it would make (0x7F) is dropped below: the search box, menus' find fields, any edit box
        delete_word(msg.hwnd);
        let mut m: MSG = zeroed();
        PeekMessageW(&mut m, msg.hwnd, WM_CHAR, WM_CHAR, PM_REMOVE);
        return;
    }
    DispatchMessageW(msg);
}

/// A Windows EDIT control (the search box, a rename or find field).
unsafe fn is_edit(h: HWND) -> bool {
    let mut b = [0u16; 8];
    let n = GetClassNameW(h, b.as_mut_ptr(), 8);
    String::from_utf16_lossy(&b[..n.max(0) as usize]).eq_ignore_ascii_case("edit")
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
    if target == "all:" {
        // every file, as with an empty search box
        if with(|u| u.browse_all()).is_some() {
            return;
        }
    }
    if let Some(l) = list_of(target) {
        if with(|u| (u.browse > 0).then(|| u.browse_list(target))) == Some(Some(())) {
            return;
        }
        return shell::open_special(l.2);
    }
    if with(|u| (mosaic && u.browse > 0).then(|| u.go_dir(target))) == Some(Some(())) {
        return;
    }
    if target.starts_with("shell:") {
        shell::open_special(target);
    } else {
        crate::recent::note(target, std::path::Path::new(target).is_dir());
        side::sync();
        shell::open(target);
    }
}

/// A middle-click on a place: a folder (or Home) in a new tab behind this one, as a browser's middle-click does;
/// anything else as a click.
fn go_place_tab(target: &str, mosaic: bool) {
    let q = match target {
        _ if list_of(target).is_some() => target.to_string(),
        t if std::path::Path::new(t).is_dir() => match with(|u| u.browse_q(t)) {
            Some(q) => q,
            None => return,
        },
        _ => return go_place(target, mosaic),
    };
    with(|u| u.new_tab_bg(&q, u.cur + 1));
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
        while let Ok(j) = rx.recv() {
            // each pane's newest only
            let mut todo = vec![j];
            while let Ok(n) = rx.try_recv() {
                todo.retain(|t| t.who != n.who);
                todo.push(n);
            }
            for j in todo {
                let r = find(&j.args, 0, PG, &j.h);
                let done: Box<Done> = Box::new((j.seq, j.args, r));
                unsafe { PostMessageW(hw as HWND, WM_RESULT, 0, Box::into_raw(done) as isize) };
            }
        }
    });
    tx
}

// ---- helpers -------------------------------------------------------------------------------------------------
/// The search box is a real EDIT control (caret, IME, undo, clipboard for free), so it keeps a GDI font.
unsafe fn edit_font(s: f64) -> HFONT {
    gdi_font(12.0 * s, 400)
}
/// Segoe UI, `px` tall, for an edit box.
pub(super) unsafe fn gdi_font(px: f64, weight: i32) -> HFONT {
    let mut lf: LOGFONTW = zeroed();
    lf.lfHeight = -(px.round() as i32);
    lf.lfWeight = weight;
    lf.lfQuality = 5 as _; // CLEARTYPE_QUALITY
    for (d, c) in lf.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
        *d = c;
    }
    CreateFontIndirectW(&lf)
}

/// Undocumented but long-stable uxtheme calls (what Notepad++ and others use) that make the search box's own
/// right-click menu dark. Our menus are drawn by us (see `popup`); Explorer's full one (`shell::menu_main`) uses it.
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
    if (dw, dh) == (sw, sh) {
        // no resize (GIF frames, small icons): only premultiply and swap to BGRA
        let mut out = vec![0u8; sw * sh * 4];
        for (o, p) in out.chunks_exact_mut(4).zip(px.chunks_exact(4)) {
            let a = p[3] as u32;
            o.copy_from_slice(&[(p[2] as u32 * a / 255) as u8, (p[1] as u32 * a / 255) as u8, (p[0] as u32 * a / 255) as u8, p[3]]);
        }
        return (out, sw as i32, sh as i32);
    }
    // area-weighted (every source pixel counts by the fraction of it inside the destination pixel), alpha
    // premultiplied, done as two separable passes in f32: no integer-rounded footprints, so no staircasing
    let axis = |n: usize, d: usize| -> Vec<Vec<(usize, f32)>> {
        (0..d)
            .map(|i| {
                let (a, b) = (i as f32 * n as f32 / d as f32, (i + 1) as f32 * n as f32 / d as f32);
                (a.floor() as usize..(b.ceil() as usize).min(n)).map(|k| (k, ((k + 1) as f32).min(b) - (k as f32).max(a))).filter(|w| w.1 > 0.0).collect()
            })
            .collect()
    };
    let (wx, wy) = (axis(sw, dw), axis(sh, dh));
    let mut mid = vec![0f32; dw * sh * 4]; // horizontal pass: dw x sh, premultiplied RGBA
    for y in 0..sh {
        for (x, taps) in wx.iter().enumerate() {
            let (mut acc, mut wsum) = ([0f32; 4], 0f32);
            for &(k, w) in taps {
                let p = &px[(y * sw + k) * 4..][..4];
                let a = p[3] as f32 / 255.0;
                acc[0] += p[0] as f32 * a * w;
                acc[1] += p[1] as f32 * a * w;
                acc[2] += p[2] as f32 * a * w;
                acc[3] += a * w * 255.0;
                wsum += w;
            }
            for c in 0..4 {
                mid[(y * dw + x) * 4 + c] = acc[c] / wsum;
            }
        }
    }
    let mut out = vec![0u8; dw * dh * 4];
    for (y, taps) in wy.iter().enumerate() {
        for x in 0..dw {
            let (mut acc, mut wsum) = ([0f32; 4], 0f32);
            for &(k, w) in taps {
                for c in 0..4 {
                    acc[c] += mid[(k * dw + x) * 4 + c] * w;
                }
                wsum += w;
            }
            let o = (y * dw + x) * 4;
            out[o] = (acc[2] / wsum + 0.5) as u8; // B
            out[o + 1] = (acc[1] / wsum + 0.5) as u8; // G
            out[o + 2] = (acc[0] / wsum + 0.5) as u8; // R
            out[o + 3] = (acc[3] / wsum + 0.5) as u8;
        }
    }
    (out, dw as i32, dh as i32)
}

/// Premultiplied BGRA pixels, width, height.
type Px = (Vec<u8>, i32, i32);

/// A thumbnail key for View > Icons… Both: this, then the folder's path.
const BOTH: &str = "\0both:";

/// The file directly in `dir` of `kind` (see [`crate::kind_of`]) that Windows previews on the folder: the last
/// modified, the first by name among those modified at once.
fn newest_of_kind(dir: &str, kind: u8) -> Option<std::path::PathBuf> {
    let (_, _, name) = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.metadata().ok().filter(|m| m.is_file()).map(|m| (m.modified().ok(), e.file_name().to_string_lossy().into_owned())))
        .filter(|(_, n)| crate::kind_of(n) == kind)
        .map(|(t, n)| (t, std::cmp::Reverse(n.to_lowercase()), n))
        .max()?;
    Some(std::path::Path::new(dir).join(name))
}

/// View > Icons… Both: the kind's Windows folder (Videos purple, Pictures blue...) holding a preview of a file
/// inside, laid out as Windows lays out a plain folder holding pictures (measured at 256 px): the front panel moved
/// down 66/256 with its tab shape, the preview cropped to fill the folder's width less 7/224 a side, its top just
/// under the back's top edge. The kind's symbol is kept, shrunk onto the shorter front. Colours are read off `folder`
/// itself, so whatever Windows' icons look like. `folder` is premultiplied BGRA; `inner` the preview's PNG, `icon`
/// when it is the file's icon (no preview). None when `folder` doesn't look like a folder.
fn fold_in(folder: &Px, inner: &[u8], icon: bool) -> Option<Px> {
    let (src, w, h) = (&folder.0, folder.1 as usize, folder.2 as usize);
    if w < 16 || h < 16 {
        return None;
    }
    let at = |x: usize, y: usize| -> [f32; 4] {
        let i = (y.min(h - 1) * w + x.min(w - 1)) * 4;
        [src[i] as f32, src[i + 1] as f32, src[i + 2] as f32, src[i + 3] as f32]
    };
    let un = |c: [f32; 4]| if c[3] > 0.0 { [c[0] * 255.0 / c[3], c[1] * 255.0 / c[3], c[2] * 255.0 / c[3], 255.0] } else { c };
    let dist = |a: [f32; 4], b: [f32; 4]| (0..3).map(|k| (a[k] - b[k]).abs()).sum::<f32>();
    // the folder: its sides across its body, its bottom down its middle
    let row = h * 6 / 10;
    let left = (0..w).find(|&x| at(x, row)[3] > 128.0)?;
    let right = (0..w).rev().find(|&x| at(x, row)[3] > 128.0)? + 1;
    let bottom = (0..h).rev().find(|&y| at(w / 2, y)[3] > 128.0)? + 1;
    // each column's front edge: the first step in colour down from the folder's top (the tab's dip included, below
    // the rest); the outline's half-covered columns and the back's rounded corner take their neighbour's
    let edge_at = |x: usize| {
        let top = (0..bottom).find(|&y| at(x, y)[3] > 200.0)?;
        (top + 1..bottom).find(|&y| at(x, y)[3] > 200.0 && dist(at(x, y), at(x, y - 1)) > 40.0)
    };
    let mid = edge_at(w * 7 / 10)?;
    let found: Vec<Option<usize>> = (0..w).map(|x| edge_at(x).filter(|&e| e + h / 128 >= mid && e < mid + h / 8)).collect();
    let mut last = found.iter().flatten().next().copied()?;
    let edge: Vec<usize> = found.iter().map(|e| { last = e.unwrap_or(last); last }).collect();
    let d = h * 66 / 256; // how far down Windows moves the front
    let band = (h * 8 / 256).max(1); // the front's top edge (clear of the symbol): moved as it is
    let rim = bottom - (h / 90).max(1); // the folder's bottom rim: kept as it is
    if mid + d + 3 * band >= rim {
        return None;
    }
    let mut out = src.clone();
    let put = |out: &mut [u8], x: usize, y: usize, c: [f32; 4], a: f32| {
        // premultiplied `c` over the pixel, at coverage `a`
        let i = (y * w + x) * 4;
        let k = 1.0 - c[3] / 255.0 * a;
        for j in 0..4 {
            out[i + j] = (c[j] * a + out[i + j] as f32 * k).round().clamp(0.0, 255.0) as u8;
        }
    };
    // the back, where the front was: each column the back's colour just above its edge, inside the outline (the
    // back is narrower than the front: its last column's colour on out, as Windows draws it)
    let mut last = [0f32; 4];
    let back: Vec<[f32; 4]> = (0..w)
        .map(|x| {
            let c = at(x, edge[x].saturating_sub(band / 2 + 1));
            if c[3] > 250.0 || last[3] == 0.0 {
                last = un(c);
            }
            last
        })
        .collect();
    let first = back.iter().position(|c| c[3] > 0.0).unwrap_or(0);
    let back = |x: usize| back[x.max(first)];
    for x in left..right {
        let c = back(x);
        for y in edge[x].saturating_sub(3)..(edge[x] + d + band).min(rim) {
            let i = (y * w + x) * 4;
            let a = src[i + 3] as f32 / 255.0;
            for j in 0..4 {
                out[i + j] = (c[j] * a).round() as u8;
            }
        }
    }
    // the preview, cropped to fill its card (centred), down under the front; one with see-through parts (an icon,
    // a logo) whole on a white card instead, in the card's open part
    let inset = ((right - left) * 7 / 224).max(1);
    let (x0, y0) = (left + inset, mid + (h / 64).max(1));
    let (cw, ch) = (right - left - 2 * inset, edge[left..right].iter().max()? + d + band - y0);
    // (as Windows: a picture with see-through parts, or an icon, on a pale card, the back's colour lightened; an icon
    // at half the folder's size, centred, its top 4/256 under the card's)
    let pic = png_pixels(inner, if icon { (h / 2) as i32 } else { i32::MAX })?;
    let pale = icon || pic.0.chunks_exact(4).filter(|p| p[3] < 250).count() * 50 > pic.0.len() / 4; // over 2% see-through
    // a picture fills the box Windows crops to: the card down to 13/256 above the folder's bottom (more than shows)
    let bh = (bottom - h * 13 / 256).saturating_sub(y0).max(ch) as f32;
    let fill = (cw as f32 / pic.1 as f32).max(bh / pic.2 as f32);
    let pic = if !icon && fill < 1.0 { png_pixels(inner, (pic.1.max(pic.2) as f32 * fill).ceil() as i32)? } else { pic };
    let (pw, ph) = (pic.1 as usize, pic.2 as usize);
    let k = if icon { 1.0 } else { (cw as f32 / pw as f32).max(bh / ph as f32) };
    let (ox, oy) = if icon {
        (((pw as f32 - cw as f32) / 2.0).floor(), -((h * 4 / 256) as f32))
    } else {
        ((pw as f32 * k - cw as f32) / 2.0, (ph as f32 * k - bh) / 2.0)
    };
    let sample = |sx: f32, sy: f32| -> [f32; 4] {
        // bilinear, premultiplied
        if sx < 0.0 || sy < 0.0 || sx >= pw as f32 || sy >= ph as f32 {
            return [0.0; 4]; // beside an icon
        }
        let (sx, sy) = ((sx - 0.5).clamp(0.0, (pw - 1) as f32), (sy - 0.5).clamp(0.0, (ph - 1) as f32));
        let (ax, ay) = (sx as usize, sy as usize);
        let (bx, by, tx, ty) = ((ax + 1).min(pw - 1), (ay + 1).min(ph - 1), sx.fract(), sy.fract());
        let p = |x: usize, y: usize, j: usize| pic.0[(y * pw + x) * 4 + j] as f32;
        std::array::from_fn(|j| (p(ax, ay, j) * (1.0 - tx) + p(bx, ay, j) * tx) * (1.0 - ty) + (p(ax, by, j) * (1.0 - tx) + p(bx, by, j) * tx) * ty)
    };
    let r = (h as f32 * 3.0 / 256.0).max(1.0);
    for y in 0..ch.min(h - y0) {
        for x in 0..cw {
            let (dx, dy) = ((r - x as f32 - 0.5).max(x as f32 + 0.5 - (cw as f32 - r)).max(0.0), (r - y as f32 - 0.5).max(0.0));
            let a = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0); // rounded top corners
            if pale {
                let b = back(x0 + x);
                put(&mut out, x0 + x, y0 + y, std::array::from_fn(|j| b[j] * 0.45 + 255.0 * 0.55), a);
            }
            put(&mut out, x0 + x, y0 + y, sample((x as f32 + 0.5 + ox) / k, (y as f32 + 0.5 + oy) / k), a);
        }
    }
    // the shadow the front casts on the preview
    let sh = (h * 12 / 256).max(2);
    for x in x0..x0 + cw {
        let low = edge[x] + d;
        for y in low.saturating_sub(sh)..low {
            let t = 1.0 - (low - y) as f32 / (sh + 1) as f32;
            put(&mut out, x, y, [0.0, 0.0, 0.0, 255.0], 0.08 * t * t);
        }
    }
    // the front: its top edge moved down as it is; below it the icon's own shading (each column's top colour down to
    // its bottom corners'), with the symbol drawn smaller on it; the rim the icon's own
    let (yb, xl, xr) = (rim - 1 - h / 40, left + w * 3 / 100, right - 1 - w * 3 / 100);
    let (bl, br) = (un(at(xl, yb)), un(at(xr, yb)));
    let shade = |x: usize, y: f32, top: f32| -> [f32; 4] {
        // the front's shading at (x, y), its top `top` (the original's or the moved one)
        let (u, v) = ((x - left) as f32 / (right - left) as f32, ((y - top) / (yb as f32 - top)).clamp(0.0, 1.0));
        let t = un(at(x, edge[x] + band));
        std::array::from_fn(|j| t[j] * (1.0 - v) + (bl[j] * (1.0 - u) + br[j] * u) * v)
    };
    let (to, tn) = ((mid + band) as f32, (mid + d + band) as f32); // the body's top, before and after
    let f = (rim as f32 - tn) / (rim as f32 - to); // how much smaller the symbol gets
    let cx = (left + right) as f32 / 2.0;
    for x in left..right {
        let low = edge[x] + d;
        // the front's top edge, moved: each pixel as much front as it was (the curve's soft pixels, mixed with the
        // back, are unmixed: the front's own colour laid over the preview at that much)
        let (b, solid) = (back(x), un(at(x, edge[x] + band)));
        for y in edge[x].saturating_sub(3)..edge[x] + band {
            let (ny, p) = (y + d, at(x, y));
            if ny >= rim {
                break;
            }
            let i = (ny * w + x) * 4;
            if p[3] < 250.0 {
                // the outline's edge (as see-through there as here), or the front's corner beyond the back
                if p[3] >= src[i + 3] as f32 {
                    out[i..i + 4].copy_from_slice(&src[(y * w + x) * 4..][..4]);
                } else {
                    put(&mut out, x, ny, p, 1.0);
                }
                continue;
            }
            let cov = if y > edge[x] { 1.0 } else { (dist(p, b) / dist(solid, b).max(1.0)).clamp(0.0, 1.0) };
            if cov < 0.1 {
                continue;
            }
            let c: [f32; 4] = if cov > 0.25 { std::array::from_fn(|j| ((p[j] - b[j] * (1.0 - cov)) / cov).clamp(0.0, 255.0)) } else { solid };
            put(&mut out, x, ny, [c[0], c[1], c[2], 255.0], cov * src[i + 3] as f32 / 255.0);
        }
        for y in low + band..bottom {
            let i = (y * w + x) * 4;
            if y >= rim {
                out[i..i + 4].copy_from_slice(&src[i..i + 4]);
                continue;
            }
            let mut c = shade(x, y as f32, (low + band) as f32);
            // the symbol: what the original front holds over its shading, sampled 3x3 over the shrunk footprint
            let mut sym = [0f32; 3];
            let mut n = 0.0;
            for sy in 0..3 {
                for sx in 0..3 {
                    let px = cx + (x as f32 + (sx as f32 + 0.5) / 3.0 - cx) / f;
                    let py = to + (y as f32 + (sy as f32 + 0.5) / 3.0 - tn) / f;
                    if px < xl as f32 || px > xr as f32 || py >= yb as f32 { // the front's own edge shading left out
                        continue;
                    }
                    let (qx, qy) = (px as usize, py as usize);
                    let p = at(qx, qy);
                    if p[3] < 250.0 || qy < edge[qx] + band {
                        continue;
                    }
                    let g = shade(qx, py, (edge[qx] + band) as f32);
                    for j in 0..3 {
                        sym[j] += un(p)[j] - g[j];
                    }
                    n += 1.0;
                }
            }
            if n > 0.0 {
                let m = sym.iter().map(|s| (s / n).abs()).sum::<f32>();
                let keep = ((m - 24.0) / 24.0).clamp(0.0, 1.0); // the shading's own small drift left out
                for j in 0..3 {
                    c[j] += sym[j] / n * keep;
                }
            }
            put(&mut out, x, y, c, src[i + 3] as f32 / 255.0);
        }
    }
    Some((out, folder.1, folder.2))
}

/// A dialog's shape: rounded as Windows 11 rounds a window, by its own region as a menu is (the compositor's
/// rounding isn't always applied to a popup). Call once it has its size.
unsafe fn outline(h: HWND) {
    let mut c: RECT = zeroed();
    GetClientRect(h, &mut c);
    let d = 2 * dialog_radius(GetDpiForWindow(h) as f64 / 96.0);
    SetWindowRgn(h, CreateRoundRectRgn(0, 0, c.right + 1, c.bottom + 1, d, d), 0);
}

fn dialog_radius(scale: f64) -> i32 {
    (8.0 * scale).round() as i32
}

/// A dialog's outline, along its rounded edge (as a menu's): drawn last, over everything.
fn frame(cv: &Canvas, h: HWND, scale: f64) {
    let mut c: RECT = unsafe { zeroed() };
    unsafe { GetClientRect(h, &mut c) };
    cv.rrect(&c, dialog_radius(scale) as f32, None, Some(mix(theme().line, theme().muted, 0.35)));
}

/// Whether `w` is `h` or a window owned by it (its own Open window, a message box...).
unsafe fn owns(h: HWND, mut w: HWND) -> bool {
    while !w.is_null() {
        if w == h {
            return true;
        }
        w = GetWindow(w, GW_OWNER);
    }
    false
}

/// Premultiplied BGRA made at most `size` px, as [`pixels`] makes a PNG's.
fn shrink((px, w, h): Px, size: i32) -> Px {
    let rgba: Vec<u8> = px
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as u32;
            let un = |c: u8| if a == 0 { 0 } else { (c as u32 * 255 / a).min(255) as u8 };
            [un(p[2]), un(p[1]), un(p[0]), p[3]]
        })
        .collect();
    pixels(&rgba, w as usize, h as usize, size.max(1) as usize)
}

/// A shell helper's PNG as premultiplied BGRA of at most `size` px.
fn png_pixels(png: &[u8], size: i32) -> Option<Px> {
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

/// Left/right edges of the Name, Path, Size, Type and Modified columns in a row `width` wide, packed left-to-right
/// with `pad` on each side and `gap` between columns: Name takes `share` of what the `fixed`-width Size, Type and
/// Modified leave, Path the rest. Short of room, the name comes first: Path gives way, down to `path_min` and then
/// out, then Type, Size and Modified drop out in that order, before Name goes under `name_min`. A column out is an
/// empty box where it would be. In RTL, the whole row mirrors (column 0 ends up flush against the right edge) while
/// keeping each column's own width and reading-order index — callers that already treat `out[k].0` as "column k's
/// near edge" don't need a separate RTL branch.
#[allow(clippy::too_many_arguments)]
fn fit_cols(width: i32, pad: i32, gap: i32, share: f64, fixed: [i32; 3], name_min: i32, path_min: i32, rtl: bool) -> [(i32, i32); 5] {
    let mut w = [0, 0, fixed[0], fixed[1], fixed[2]];
    let mut on = [true; 5];
    for out in [1, 3, 2, 4, usize::MAX] {
        let n = on.iter().filter(|&&o| o).count() as i32;
        let fixed_w: i32 = (2..5).filter(|&k| on[k]).map(|k| w[k]).sum();
        let room = (width - 2 * pad - (n - 1) * gap - fixed_w).max(0);
        (w[0], w[1]) = if on[1] {
            let name = ((room as f64 * share).round() as i32).max(name_min.min(room));
            (name, room - name)
        } else {
            (room, 0)
        };
        let short = if out == 1 { w[1] < path_min } else { w[0] < name_min };
        if !short || out == usize::MAX {
            break;
        }
        on[out] = false;
    }
    let mut x = pad;
    let mut cols = [(0, 0); 5];
    for k in 0..5 {
        if on[k] {
            cols[k] = (x, x + w[k]);
            x += w[k] + gap;
        } else {
            cols[k] = (x, x);
        }
    }
    if rtl {
        let right = (x - gap).max(pad); // the row's far edge (last column's own right edge)
        cols = cols.map(|(a, b)| (pad + right - b, pad + right - a));
    }
    cols
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
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#""[^"]*"|\S+"#).unwrap());
    RE.find_iter(q)
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
thread_local!(static WATCH: RefCell<Option<(String, isize)>> = const { RefCell::new(None) });

/// The focused pane lists a folder outside the index (a USB stick, an exFAT drive): `true` when its files changed
/// since last asked, as the index tells of the folders it sees. One change handle, for that folder only; local
/// drives only (a share's could block).
fn watch(q: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification, GetDriveTypeW, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let want = listed_dir(q).filter(|d| d.as_bytes().get(1) == Some(&b':') && crate::direct::outside(d) && matches!(unsafe { GetDriveTypeW(wz(&d[..3]).as_ptr()) }, 2 | 3));
    WATCH.with(|w| unsafe {
        let mut w = w.borrow_mut();
        if w.as_ref().map(|x| &x.0) != want.as_ref() {
            if let Some((_, h)) = w.take().filter(|x| x.1 != 0) {
                FindCloseChangeNotification(h as _);
            }
            *w = want.map(|d| {
                let h = FindFirstChangeNotificationW(wz(&d).as_ptr(), 0, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE);
                (d, if h == INVALID_HANDLE_VALUE { 0 } else { h as isize }) // 0: not asked again until the folder changes
            });
            return false;
        }
        match *w {
            Some((_, h)) if h != 0 && WaitForSingleObject(h as _, 0) == 0 => FindNextChangeNotification(h as _) != 0,
            _ => false,
        }
    })
}

/// The folder a query lists: `child:"C:\dir"` / `descendant:"C:\dir"`, maybe followed by more words.
fn listed_dir(q: &str) -> Option<String> {
    let q = q.trim();
    let (d, _) = ["child:\"", "descendant:\""].iter().find_map(|p| q.strip_prefix(p))?.split_once('"')?;
    Some(if d.ends_with(':') { format!("{d}\\") } else { d.to_string() })
}
/// A tab's × button: a square of side `s` at its right end.
fn tab_x(r: &RECT, s: i32) -> RECT {
    let top = r.top + (r.bottom - r.top - s) / 2;
    RECT { left: r.right - s - (r.bottom - r.top - s) / 2, top, right: r.right - (r.bottom - r.top - s) / 2, bottom: top + s }
}
/// A split tab's mark: a small window outline cut in two.
fn split_glyph(cv: &Canvas, x: i32, y: i32, w: i32, h: i32, c: u32, scale: f64) {
    let t = (scale.round() as i32).max(1);
    cv.rrect(&RECT { left: x, top: y, right: x + w, bottom: y + h }, (2.0 * scale) as f32, None, Some(c));
    cv.fill(&RECT { left: x + w / 2 - t / 2, top: y, right: x + w / 2 - t / 2 + t, bottom: y + h }, c, 1.0);
}
/// The chip that follows the pointer while a tab or pane is dragged: never takes a click.
unsafe extern "system" fn chip_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_NCHITTEST => -1, // HTTRANSPARENT
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            with(|u| u.paint_chip());
            EndPaint(h, &ps);
            0
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}
/// The landing zone's window ([`Zone`]): never takes a click; its picture comes from `UpdateLayeredWindow`.
unsafe extern "system" fn zone_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_NCHITTEST => -1, // HTTRANSPARENT
        _ => DefWindowProcW(h, m, wp, lp),
    }
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
    /// Always the shell's 256 px image (its jumbo icon / cached thumbnail): box-filtered down to the tile it is
    /// sharp, whereas asking for 64/128 made the shell stretch a 48 px icon.
    fn tsize(&self) -> i32 {
        256
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
        let q = self.browse_q(dir);
        self.visit(&q);
        unsafe { SetWindowTextW(self.edit, wz(&q).as_ptr()) };
    }
    fn browse_q(&self, dir: &str) -> String {
        let deep = self.browse == 2 && !crate::direct::outside(dir); // a share is read a folder at a time
        format!("{}:\"{}\"", if deep { "descendant" } else { "child" }, dir.trim_end_matches('\\'))
    }

    // -- panes
    /// `f` with pane `id`'s state in the fields (the focused pane's put aside meanwhile).
    fn with_pane<R>(&mut self, id: usize, f: impl FnOnce(&mut Ui) -> R) -> R {
        if id == self.id {
            return f(self);
        }
        let mut v = self.panes[id].take().expect("a pane swapped in twice");
        self.swap_view(&mut v);
        self.guest = true;
        let r = f(self);
        self.guest = false;
        self.swap_view(&mut v);
        self.panes[id] = Some(v);
        if std::mem::take(&mut self.relayout) {
            self.layout();
        }
        r
    }
    /// `f` on the pane whose list window is `h`; None if it is none of them.
    fn on_list<R>(&mut self, h: HWND, f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
        if h == self.list {
            return Some(f(self));
        }
        if self.guest {
            return None;
        }
        let id = self.panes.iter().position(|v| v.as_ref().is_some_and(|v| v.list == h))?;
        Some(self.with_pane(id, f))
    }
    fn view(&self, id: usize) -> Option<&View> {
        self.panes.get(id).and_then(Option::as_ref)
    }
    fn list_of(&self, id: usize) -> HWND {
        if id == self.id { self.list } else { self.view(id).map_or(null_mut(), |v| v.list) }
    }
    fn pane_name(&self, id: usize) -> String {
        place_name(if id == self.id { &self.args.q } else { self.view(id).map_or("", |v| &v.args.q) })
    }
    /// The panes on screen: the shown tab's.
    fn visible(&self) -> Vec<usize> {
        self.tabs[self.cur].root.leaves()
    }
    fn visible_lists(&self) -> Vec<HWND> {
        self.visible().into_iter().map(|id| self.list_of(id)).collect()
    }
    fn each_visible(&mut self, mut f: impl FnMut(&mut Ui)) {
        for id in self.visible() {
            self.with_pane(id, &mut f);
        }
    }
    fn invalidate_all(&self) {
        for l in self.visible_lists() {
            unsafe { InvalidateRect(l, null(), 0) };
        }
        unsafe { InvalidateRect(self.main, null(), 0) };
    }
    /// A pane listing `q`, with the focused one's options, its own window (hidden until laid out) and no history.
    fn new_pane(&mut self, q: &str) -> usize {
        let id = self.new_pane_idle(q);
        self.with_pane(id, |u| u.run(false));
        id
    }
    /// … not searching yet.
    fn new_pane_idle(&mut self, q: &str) -> usize {
        let list = unsafe { CreateWindowExW(0, wz("MosaicList").as_ptr(), null(), WS_CHILD | WS_CLIPCHILDREN, 0, 0, 10, 10, self.main, null_mut(), GetModuleHandleW(null()), null()) };
        unsafe { add_tip(self.tip, list, true) };
        let id = self.panes.len();
        let args = SearchArgs { q: q.into(), cat: String::new(), ..self.args.clone() };
        let now = Instant::now();
        let r: RECT = unsafe { zeroed() };
        self.panes.push(Some(View {
            id,
            list,
            cl: Canvas::new(list, &self.gfx),
            shown: args.clone(),
            args,
            seq: 0,
            fresh: true,
            terms: Vec::new(),
            matches: 0,
            version: None,
            objects: 0,
            changes: 0,
            pages: HashMap::new(),
            order: VecDeque::new(),
            y: 0.0,
            target: 0.0,
            frame: now,
            glide: (0.0, now),
            sel: Sel::default(),
            focus: None,
            anchor: None,
            hover: None,
            tracking: false,
            press: None,
            auto: None,
            fx: None,
            slide: HashMap::new(),
            born: HashSet::new(),
            lh: r,
            cap: r,
            org: POINT { x: 0, y: 0 },
            hist: std::cell::RefCell::new((vec![q.into()], 0)),
            grid: self.grid,
            size: self.size,
            per: 1,
            zoom_acc: 0,
            step_acc: 0,
            live_at: now,
            live_cost: Duration::ZERO,
            poll_y: 0.0,
            meta_gen: 0,
            tip_item: None,
            sb_hot: false,
            sb_drag: None,
            pics_gen: 0,
            texts: HashMap::new(),
            texts_key: (0, false, 0, 0),
            sys_pics: HashMap::new(),
            own: HashMap::new(),
            pending: HashSet::new(),
            thumbs: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_bytes: 0,
            thumb_pending: HashSet::new(),
            link_ov: HashMap::new(),
            jumbo: HashMap::new(),
            busy: None,
            want_y: None,
        }));
        dnd::register(list);
        id
    }
    /// Drop a pane that isn't focused.
    fn drop_pane(&mut self, id: usize) {
        if let Some(v) = self.panes.get_mut(id).and_then(Option::take) {
            unsafe {
                add_tip(self.tip, v.list, false);
                dnd::revoke(v.list);
                DestroyWindow(v.list);
            }
        }
    }
    /// Every pane's list canvas made anew for `g` (a new scale or text direction).
    fn new_canvases(&mut self, g: &Gfx) {
        for id in 0..self.panes.len() {
            if id == self.id || self.panes[id].is_some() {
                self.with_pane(id, |u| {
                    u.cl = Canvas::new(u.list, g);
                    u.pics_gen = u32::MAX;
                });
            }
        }
    }
    /// A pane put away (its tab not shown): its pictures let go of, the GPU memory with them.
    fn hibernate(&mut self) {
        self.cl = Canvas::new(self.list, &self.gfx);
        self.pics_gen = u32::MAX; // the new canvas's pictures start from scratch
        self.forget_pics();
        self.hover = None;
    }
    /// Focus pane `id`: its state into the fields, its search into the box (which doesn't run it again).
    fn activate(&mut self, id: usize) {
        if id == self.id || self.guest || self.view(id).is_none() {
            return;
        }
        self.typed();
        let mut v = self.panes[id].take().unwrap();
        self.swap_view(&mut v);
        let old = v.id;
        self.panes[old] = Some(v);
        if let Some(t) = self.tabs.get_mut(self.cur).filter(|t| t.root.leaves().contains(&id)) {
            t.focus = id;
        }
        unsafe { SetWindowTextW(self.edit, wz(&self.args.q).as_ptr()) };
        self.layout();
        self.invalidate_all();
        self.want_preview();
    }
    /// The pane whose list is `h` focused, as a click on it does. False for a closing one.
    fn activate_list(&mut self, h: HWND) -> bool {
        let Some(id) = (0..self.panes.len()).find(|&id| self.list_of(id) == h) else { return false };
        if self.dying.contains(&id) {
            return false;
        }
        self.activate(id);
        true
    }
    /// The search box's text, typed since the last search, searched for now (a pane switch must not carry it over).
    fn typed(&mut self) {
        if !self.typing {
            return;
        }
        unsafe { KillTimer(self.main, T_TYPE) };
        self.typing = false;
        let n = unsafe { GetWindowTextLengthW(self.edit) };
        let mut b = vec![0u16; n as usize + 1];
        unsafe { GetWindowTextW(self.edit, b.as_mut_ptr(), n + 1) };
        let q = String::from_utf16_lossy(&b[..n as usize]);
        if q != self.args.q {
            let old = listed_dir(&self.args.q);
            self.args.q = q;
            self.run(false);
            let new = listed_dir(&self.args.q);
            self.navsync(old, new);
        }
    }
    /// Close pane `id` (the tab, when it's the only one): it shrinks away as its neighbour takes the room.
    fn close_pane(&mut self, id: usize) {
        if self.dying.contains(&id) {
            return;
        }
        if !self.tabs[self.cur].root.collapse(id) {
            return self.close_tab(self.cur);
        }
        self.dying.push(id);
        if self.id == id {
            let had = unsafe { GetFocus() } == self.list;
            let next = self.visible().into_iter().find(|p| !self.dying.contains(p)).unwrap_or(id);
            self.activate(next);
            if had {
                unsafe { SetFocus(self.list) };
            }
        }
        self.layout(); // its caption and headers go at once
    }
    /// Closing panes done with (or no longer on screen): out of their tab, and gone.
    fn bury(&mut self, all: bool) {
        for id in std::mem::take(&mut self.dying) {
            if !all && self.tabs[self.cur].root.settled(id) == Some(false) {
                self.dying.push(id);
                continue;
            }
            if let Some(t) = self.tabs.iter_mut().find(|t| t.root.leaves().contains(&id)) {
                t.root.detach(id);
            }
            self.drop_pane(id);
            self.layout();
        }
    }
    /// Split the focused pane: the same place again on `side`, focused (Ctrl+\, Ctrl+Shift+\ below).
    fn split_here(&mut self, side: u8) {
        let id = self.new_pane(&self.args.q.clone());
        let sid = self.next_split();
        let (me, root) = (self.id, &mut self.tabs[self.cur].root);
        root.insert(me, side, &mut Some(Node::Leaf(id)), sid);
        self.activate(id);
        unsafe { SetFocus(self.list) };
    }
    fn next_split(&mut self) -> usize {
        self.splits += 1;
        self.splits
    }

    // -- tabs
    fn tab_of(&self, pane: usize) -> Option<usize> {
        self.tabs.iter().position(|t| t.root.leaves().contains(&pane))
    }
    /// Show tab `k`: its panes come back as they were, the ones it hides let go of their pictures.
    fn switch_tab(&mut self, k: usize) {
        if k == self.cur || k >= self.tabs.len() {
            return;
        }
        self.wake(k);
        self.bury(true);
        let had = unsafe { GetFocus() } == self.list;
        let old = self.visible();
        self.last = self.id;
        let was = self.cur;
        self.cur = k;
        let f = self.tabs[k].focus;
        self.activate(f);
        for p in old {
            if self.view(p).is_some() {
                self.with_pane(p, |u| u.hibernate());
            }
        }
        self.layout();
        if had {
            unsafe { SetFocus(self.list) };
        }
        self.invalidate_all();
        self.shown_tab(was);
    }
    /// A new tab at `at` showing `q`, with no history behind it.
    fn new_tab(&mut self, q: &str, at: usize) {
        let at = at.min(self.tabs.len());
        let id = self.new_pane(q);
        self.tabs.insert(at, Tab { born: (!self.tp.still).then(Instant::now), ..Tab::new(id) });
        if at <= self.cur {
            self.cur += 1;
        }
        self.switch_tab(at);
    }
    /// A folder in a new tab, right after this one (as a browser opens links).
    fn open_tab(&mut self, dir: &str) {
        let q = self.browse_q(dir);
        self.new_tab(&q, self.cur + 1);
    }
    /// Close tab `k` and its panes; the last one closes the window, as Explorer's does.
    fn close_tab(&mut self, k: usize) {
        self.drop_tab(k, true);
    }

    // -- dragging tabs and panes
    /// A press in the main window that may become a drag: a tab, a pane's caption, a split's gap. False if it's none.
    fn press_main(&mut self, x: i32, y: i32) -> bool {
        let Some(&(_, hit)) = self.hits.iter().find(|(r, _)| inside(r, x, y)) else { return false };
        let grab = match hit {
            Hit::Tab(k) => Grab::Tab(k),
            Hit::Group(g) => Grab::Group(g),
            Hit::Pane(id) => {
                if self.dying.contains(&id) {
                    return true;
                }
                if self.tabs[self.cur].o.folded.contains(&id) {
                    // a folded pane's caption: unfolds it
                    self.tabs[self.cur].o.folded.retain(|&p| p != id);
                    self.layout();
                    self.invalidate_all();
                    return true;
                }
                self.activate(id);
                unsafe { SetFocus(self.list) };
                Grab::Pane(id)
            }
            Hit::Gutter(sid) => {
                self.gut = Some(sid);
                self.gut_to = Some((x, y));
                unsafe { SetCapture(self.main) };
                return true;
            }
            Hit::ColEdge(id, k) => {
                let mut w = [0; 5];
                for &(r, h) in &self.hits {
                    if let Hit::Col(p, j) = h {
                        if p == id {
                            w[j] = r.right - r.left;
                        }
                    }
                }
                self.col_drag = Some((id, k, x, w));
                unsafe { SetCapture(self.main) };
                return true;
            }
            _ => return false,
        };
        self.drag = Some(Drag { grab, x0: x, y0: y, on: false, lifted: false, to: None });
        unsafe { SetCapture(self.main) };
        true
    }
    /// The split whose gap is being dragged or pointed at: whether its panes sit side by side.
    fn gutter_vert(&self) -> Option<bool> {
        let sid = self.gut.or(match self.hot {
            Some(Hit::Gutter(s)) => Some(s),
            _ => None,
        })?;
        self.gutters.iter().find(|g| g.0 == sid).map(|g| g.3)
    }
    fn grab_name(&self, g: Grab) -> String {
        match g {
            Grab::Tab(k) => self.tab_name(k),
            Grab::Pane(id) => self.pane_name(id),
            Grab::Group(g) => self.groups.iter().find(|x| x.id == g).map_or(String::new(), |x| x.name.clone()),
        }
    }
    /// Tab slot `i` in the strip.
    fn slot(&self, i: usize) -> RECT {
        if self.hole == Some(i) {
            return self.hole_r;
        }
        self.slot_rs.get(i).copied().unwrap_or(self.hole_r)
    }
    /// Where a drop at (x, y) would land: the tab strip, an edge of the whole tab's room, or a side of a pane. Near
    /// where two sides of a pane meet, the one it pointed at before (`was`) holds until the other is clearly nearer:
    /// no flicker along the diagonal.
    fn dock_at(&self, x: i32, y: i32, g: Grab, was: Option<Dock>) -> Option<Dock> {
        if self.on_strip(x, y) {
            return Some(Dock::Strip(match g {
                Grab::Tab(k) => self.strip_index(x, y, |j| j != k),
                Grab::Pane(_) => self.strip_index(x, y, |_| true),
                Grab::Group(gid) => self.strip_index(x, y, |j| self.tabs[j].o.group != Some(gid)),
            }));
        }
        if matches!(g, Grab::Tab(k) if k == self.cur) || matches!(g, Grab::Group(_)) {
            return None; // the tab on screen into itself, a group anywhere but the strip: nowhere
        }
        let c = self.content;
        if !inside(&c, x, y) {
            return None;
        }
        // a pane in hand has already left: aim at the room as it'd be without it, so its own spot is a target too
        let areas = match g {
            Grab::Pane(p) => {
                let mut root = self.tabs[self.cur].root.clone();
                if !root.detach(p) {
                    return None;
                }
                root.settle();
                let (mut leaves, mut gutters) = (Vec::new(), Vec::new());
                root.place(c, self.px(8), (&[], 0), &mut leaves, &mut gutters);
                leaves
            }
            Grab::Tab(_) | Grab::Group(_) => self.areas.clone(),
        };
        // the room's own edges, once it's split: a part across the whole tab
        let band = self.px(22);
        if areas.len() > 1 {
            let edges = [x - c.left, c.right - x, y - c.top, c.bottom - y];
            if let Some(side) = (0..4).filter(|&s| edges[s] < band).min_by_key(|&s| edges[s]) {
                return Some(Dock::Root(side as u8));
            }
        }
        let &(id, r) = areas.iter().find(|(_, r)| inside(r, x, y))?;
        if self.dying.contains(&id) {
            return None;
        }
        let (fx, fy) = ((x - r.left) as f64 / (r.right - r.left).max(1) as f64, (y - r.top) as f64 / (r.bottom - r.top).max(1) as f64);
        let d = [fx, 1.0 - fx, fy, 1.0 - fy];
        let side = (0..4).min_by(|&a, &b| d[a].total_cmp(&d[b])).unwrap_or(1);
        let side = match was {
            Some(Dock::Pane(w, s)) if w == id && d[s as usize] < d[side] + 0.06 => s as usize,
            _ => side,
        };
        Some(Dock::Pane(id, side as u8))
    }
    /// The box a landing shows in: the half of a pane or of the whole room it would take, or a tab slot.
    fn dock_rect(&self, d: Dock) -> RECT {
        let half = |r: RECT, side: u8| {
            let g = self.px(4);
            let (mx, my) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            match side {
                0 => RECT { right: mx - g, ..r },
                1 => RECT { left: mx + g, ..r },
                2 => RECT { bottom: my - g, ..r },
                _ => RECT { top: my + g, ..r },
            }
        };
        match d {
            Dock::Strip(i) => self.slot(i),
            Dock::Root(side) => half(self.content, side),
            Dock::Pane(id, side) => half(self.areas.iter().find(|a| a.0 == id).map_or(self.content, |a| a.1), side),
        }
    }
    /// Point the landing zone at `r` (springing there, or growing in where there was none), or fade it out.
    fn aim(&mut self, r: Option<RECT>, label: String) {
        let Some(r) = r else {
            if let Some(o) = &mut self.ov {
                o.on = false;
            }
            return;
        };
        let to = [r.left as f64, r.top as f64, r.right as f64, r.bottom as f64];
        match &mut self.ov {
            Some(o) if o.a > 0.0 => (o.to, o.on, o.label) = (to, true, label),
            _ => {
                // grows in from a little inside where it lands
                let (cx, cy, k) = ((to[0] + to[2]) / 2.0, (to[1] + to[3]) / 2.0, 0.94);
                let r0 = [cx + (to[0] - cx) * k, cy + (to[1] - cy) * k, cx + (to[2] - cx) * k, cy + (to[3] - cy) * k];
                self.ov = Some(Ov { r: r0, v: [0.0; 4], to, a: 0.0, on: true, label });
            }
        }
    }
    fn drag_move(&mut self, x: i32, y: i32) {
        let Some(mut d) = self.drag.take() else { return };
        if !d.on {
            let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
            if (x - d.x0).abs() <= dx && (y - d.y0).abs() <= dy {
                self.drag = Some(d);
                return;
            }
            d.on = true;
            if let Grab::Tab(k) = d.grab {
                self.wake(k); // asleep: its panes back, to dock them
                if k == self.cur && self.tabs.len() > 1 {
                    // the tab on screen lifted out: the window shows the one before it meanwhile
                    let to = self.tab_of(self.last).filter(|&j| j != k).unwrap_or(if k > 0 { k - 1 } else { 1 });
                    self.switch_tab(to);
                    d.lifted = true;
                }
            }
            self.chip_open(d.grab);
            unsafe { InvalidateRect(self.main, null(), 0) }; // the tab in hand leaves the strip
        }
        self.chip_move();
        // a pane goes nowhere till it's clearly been pulled away: a slip on its caption leaves it put
        let near = matches!(d.grab, Grab::Pane(_)) && (x - d.x0).pow(2) + (y - d.y0).pow(2) < self.px(40).pow(2);
        d.to = if near { None } else { self.dock_at(x, y, d.grab, d.to) };
        let (mut hole, mut moved) = (None, false);
        let zone = match (d.grab, d.to) {
            (Grab::Tab(k), Some(Dock::Strip(j))) => {
                // reordered as it goes: the others slide aside
                if j != k {
                    moved = true;
                    let t = self.tabs.remove(k);
                    self.tabs.insert(j, t);
                    self.cur = match self.cur {
                        c if c == k => j,
                        c if k < c && j >= c => c - 1,
                        c if k > c && j <= c => c + 1,
                        c => c,
                    };
                    d.grab = Grab::Tab(j);
                }
                Some(self.slot(j))
            }
            (Grab::Tab(k), None) => Some(self.slot(k)), // back home
            (Grab::Group(gid), Some(Dock::Strip(j))) => {
                // the group moves along as it goes
                let before: Vec<u64> = self.tabs.iter().map(|t| t.uid).collect();
                self.move_group(gid, j);
                moved = self.tabs.iter().map(|t| t.uid).collect::<Vec<_>>() != before;
                None
            }
            (Grab::Group(_), _) => None,
            (Grab::Pane(_), Some(Dock::Strip(i))) => {
                hole = Some(i);
                Some(self.slot(i))
            }
            (g, Some(to)) => self.landing(g, to).or_else(|| Some(self.dock_rect(to))),
            (_, None) => None,
        };
        let label = self.grab_name(d.grab);
        self.drag = Some(d);
        if hole != self.hole || moved {
            self.hole = hole;
            self.layout(); // the tabs' slots
        }
        let zone = match (zone, hole) {
            (Some(_), Some(i)) => Some(self.slot(i)), // the slot as laid out with the hole
            (z, _) => z,
        };
        self.aim(zone, label); // its own window: nothing else repaints as it moves
    }
    /// The drag ends: dropped (`commit`) or let go of (Esc, capture lost).
    fn drag_end(&mut self, commit: bool) {
        let Some(d) = self.drag.take() else { return };
        self.chip_close();
        self.hole = None;
        self.aim(None, String::new());
        let to = if commit { d.to } else { None };
        // a tab dropped outside the window: into the Mosaic window there, else a window of its own
        if let (true, true, None, Grab::Tab(k)) = (commit, d.on, to, d.grab) {
            let mut pt = POINT { x: 0, y: 0 };
            let mut wr: RECT = unsafe { zeroed() };
            unsafe {
                GetCursorPos(&mut pt);
                GetWindowRect(self.main, &mut wr);
            }
            if !inside(&wr, pt.x, pt.y) {
                let json = self.json_of(&[k]);
                let gone = match window_at(self.main, pt) {
                    Some(h) => unsafe { send_tabs(h, &json, false) },
                    None => self.tabs.len() > 1 && spawn_window(&json, Some(pt)),
                };
                if gone {
                    if self.tabs.len() == 1 {
                        self.merged = true; // its one tab went on: nothing left here to keep
                        unsafe { PostMessageW(self.main, WM_CLOSE, 0, 0) };
                    } else {
                        self.drop_tab(k, false);
                    }
                    self.layout();
                    self.invalidate_all();
                    return;
                }
            }
        }
        match (d.grab, d.on, to) {
            (Grab::Group(g), false, _) if commit => {
                if let Some(x) = self.groups.iter_mut().find(|x| x.id == g) {
                    x.folded = !x.folded;
                }
            }
            (Grab::Group(_), ..) => self.order_tabs(),
            (Grab::Tab(k), false, _) if commit => self.switch_tab(k),
            (Grab::Tab(k), true, Some(to @ (Dock::Pane(..) | Dock::Root(_)))) if k != self.cur => {
                self.wake(k);
                let t = self.tabs.remove(k);
                if k < self.cur {
                    self.cur -= 1;
                }
                self.dock(t.root, to);
                self.activate(t.focus);
            }
            (Grab::Tab(k), true, _) => {
                if d.lifted {
                    self.switch_tab(k);
                }
                self.regroup(k);
                self.order_tabs();
            }
            (Grab::Pane(p), true, Some(Dock::Strip(i))) => {
                // out to a tab of its own, where the strip opened for it
                if self.tabs[self.cur].root.detach(p) {
                    if self.id == p {
                        let next = self.visible()[0];
                        self.activate(next);
                    }
                    let mut t = Tab::new(p);
                    // dropped between two tabs of a group: one of it
                    t.o.group = self.tabs.get(i).and_then(|x| x.o.group).filter(|&g| i > 0 && self.tabs[i - 1].o.group == Some(g));
                    self.tabs.insert(i, t);
                    if i <= self.cur {
                        self.cur += 1;
                    }
                    self.with_pane(p, |u| u.hibernate());
                }
            }
            (Grab::Pane(p), true, Some(to)) => {
                if self.tabs[self.cur].root.detach(p) {
                    self.dock(Node::Leaf(p), to);
                    self.activate(p);
                }
            }
            _ => {}
        }
        self.layout();
        self.invalidate_all();
    }
    /// Put `sub` (a pane, or a whole tab's) where it was dropped in the tab shown.
    fn dock(&mut self, sub: Node, to: Dock) {
        let sid = self.next_split();
        self.tabs[self.cur].root.dock(sub, to, sid);
    }
    /// Where `g` would end up dropped at `to` in the tab shown, exactly as the drop will lay it out: a pane moved
    /// within the tab once it's out of its old place (its sibling taking that room), not as a third pane beside the
    /// two there now; another tab as the box all its panes take.
    fn landing(&self, g: Grab, to: Dock) -> Option<RECT> {
        let mut root = self.tabs[self.cur].root.clone();
        let sub = match g {
            Grab::Pane(p) => {
                if !root.detach(p) {
                    return None;
                }
                Node::Leaf(p)
            }
            Grab::Tab(k) => self.tabs[k].root.clone(),
            Grab::Group(_) => return None,
        };
        let ids = sub.leaves();
        root.dock(sub, to, usize::MAX);
        root.settle();
        let (mut leaves, mut gutters) = (Vec::new(), Vec::new());
        root.place(self.content, self.px(8), (&[], 0), &mut leaves, &mut gutters);
        leaves.into_iter().filter(|l| ids.contains(&l.0)).map(|l| l.1).reduce(|a, b| RECT {
            left: a.left.min(b.left),
            top: a.top.min(b.top),
            right: a.right.max(b.right),
            bottom: a.bottom.max(b.bottom),
        })
    }
    /// The dragged thing's chip, following the pointer: a little window of its own, over everything.
    fn chip_open(&mut self, grab: Grab) {
        let label = self.grab_name(grab);
        let multi = matches!(grab, Grab::Tab(k) if self.tabs[k].root.leaves().len() > 1);
        static CLASS: std::sync::Once = std::sync::Once::new();
        let class = wz("MosaicChip");
        unsafe {
            CLASS.call_once(|| {
                let mut wc: WNDCLASSEXW = zeroed();
                wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
                wc.style = CS_DROPSHADOW;
                wc.hInstance = GetModuleHandleW(null());
                wc.lpfnWndProc = Some(chip_proc);
                wc.lpszClassName = class.as_ptr();
                RegisterClassExW(&wc);
            });
            let g = &self.gfx;
            let wd = g.width(&label, &g.f.reg).min(self.px(260)) + self.px(if multi { 46 } else { 28 });
            let ht = g.f.lh + self.px(14);
            let h = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT, class.as_ptr(), null(), WS_POPUP, 0, 0, wd, ht, self.main, null_mut(), GetModuleHandleW(null()), null());
            if h.is_null() {
                return;
            }
            let dark: BOOL = 1;
            DwmSetWindowAttribute(h, 20, &dark as *const _ as *const c_void, 4);
            let d = self.px(12);
            SetWindowRgn(h, CreateRoundRectRgn(0, 0, wd + 1, ht + 1, d, d), 0);
            self.chip = Some((h, Canvas::new(h, g), label, multi));
            self.chip_move();
            ShowWindow(h, SW_SHOWNA);
        }
    }
    fn chip_move(&self) {
        let Some((h, ..)) = &self.chip else { return };
        let mut p = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut p);
            SetWindowPos(*h, null_mut(), p.x + self.px(14), p.y + self.px(12), 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
    fn chip_close(&mut self) {
        if let Some((h, ..)) = self.chip.take() {
            unsafe { DestroyWindow(h) };
        }
    }
    /// The landing zone's window, made when a zone first shows.
    fn zone_open(&self) -> Option<Zone> {
        static CLASS: std::sync::Once = std::sync::Once::new();
        let class = wz("MosaicZone");
        unsafe {
            CLASS.call_once(|| {
                let mut wc: WNDCLASSEXW = zeroed();
                wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
                wc.hInstance = GetModuleHandleW(null());
                wc.lpfnWndProc = Some(zone_proc);
                wc.lpszClassName = class.as_ptr();
                RegisterClassExW(&wc);
            });
            let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            let hwnd = CreateWindowExW(ex, class.as_ptr(), null(), WS_POPUP, 0, 0, 1, 1, self.main, null_mut(), GetModuleHandleW(null()), null());
            let dc = CreateCompatibleDC(null_mut());
            if hwnd.is_null() || dc.is_null() {
                DestroyWindow(hwnd);
                return None;
            }
            Some(Zone { hwnd, cv: Canvas::new(hwnd, &self.gfx), dc, bmp: null_mut(), cap: (0, 0) })
        }
    }
    /// Show the landing zone as it is now: its window moved and drawn anew (nothing under it repaints), or closed
    /// once the zone is gone.
    fn show_zone(&mut self) {
        let Some(o) = &self.ov else {
            if let Some(z) = self.zone.take() {
                unsafe {
                    DestroyWindow(z.hwnd);
                    DeleteDC(z.dc);
                    DeleteObject(z.bmp);
                }
            }
            return;
        };
        let r = RECT { left: o.r[0].round() as i32, top: o.r[1].round() as i32, right: o.r[2].round() as i32, bottom: o.r[3].round() as i32 };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let Some(mut z) = self.zone.take().or_else(|| self.zone_open()) else { return };
        unsafe {
            if w <= 0 || h <= 0 {
                ShowWindow(z.hwnd, SW_HIDE);
                self.zone = Some(z);
                return;
            }
            if w > z.cap.0 || h > z.cap.1 {
                // a 32-bit top-down DIB: what the Direct2D target draws into and the compositor reads from
                let cap = (w.max(z.cap.0), h.max(z.cap.1));
                let mut bi: BITMAPINFO = zeroed();
                bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                (bi.bmiHeader.biWidth, bi.bmiHeader.biHeight) = (cap.0, -cap.1);
                (bi.bmiHeader.biPlanes, bi.bmiHeader.biBitCount) = (1, 32);
                let mut bits = null_mut();
                let bmp = CreateDIBSection(z.dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
                if bmp.is_null() {
                    self.zone = Some(z);
                    return;
                }
                SelectObject(z.dc, bmp);
                DeleteObject(z.bmp);
                (z.bmp, z.cap) = (bmp, cap);
            }
            if z.cv.begin_dc(z.dc, w, h) {
                self.paint_ov(&z.cv, r.left, r.top);
                z.cv.end_dc();
            }
            let mut at = POINT { x: r.left, y: r.top };
            ClientToScreen(self.main, &mut at);
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(z.hwnd, null_mut(), &at, &SIZE { cx: w, cy: h }, z.dc, &POINT { x: 0, y: 0 }, 0, &blend, ULW_ALPHA);
            if IsWindowVisible(z.hwnd) == 0 {
                ShowWindow(z.hwnd, SW_SHOWNA);
            }
        }
        self.zone = Some(z);
    }
    fn paint_chip(&mut self) {
        let (s, rad, lh) = (self.scale, self.pf(6.0), self.gfx.f.lh);
        let px = |v: i32| (v as f64 * s).round() as i32;
        let Some((h, cv, label, multi)) = &mut self.chip else { return };
        if !cv.begin() {
            return;
        }
        let mut c: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(*h, &mut c) };
        cv.clear(theme().card);
        cv.rrect(&c, rad, Some(theme().hl), Some(theme().accent));
        let mut x = px(14);
        if *multi {
            split_glyph(cv, x, (c.bottom - px(10)) / 2, px(14), px(10), theme().accent, s);
            x += px(20);
        }
        if let Some(t) = self.gfx.layout(&w(label), &self.gfx.f.reg, (c.right - x - px(10)) as f32, lh as f32) {
            cv.text(&t, x, (c.bottom - lh) / 2, theme().text);
        }
        cv.end();
    }
    /// The landing zone, drawn on `cv` (its own window's, at (ox, oy) in the main window).
    fn paint_ov(&self, cv: &Canvas, ox: i32, oy: i32) {
        let Some(o) = &self.ov else { return };
        let a = o.a as f32;
        if a <= 0.0 {
            return;
        }
        let r = RECT { left: o.r[0].round() as i32 - ox, top: o.r[1].round() as i32 - oy, right: o.r[2].round() as i32 - ox, bottom: o.r[3].round() as i32 - oy };
        let rad = ((r.bottom - r.top) as f32 / 4.0).min(self.pf(10.0)).max(1.0);
        cv.rrect_a(&r, rad, Some(theme().accent), None, 0.16 * a);
        cv.rrect_a(&r, rad, None, Some(theme().accent), 0.9 * a);
        cv.rrect_a(&RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 }, rad - 1.0, None, Some(theme().accent), 0.5 * a);
        let (g, lh) = (&self.gfx, self.gfx.f.lh);
        let room = r.right - r.left - self.px(20);
        if room > self.px(24) && r.bottom - r.top > lh {
            let tw = g.width(&o.label, &g.f.reg).min(room);
            if let Some(t) = g.layout(&w(&o.label), &g.f.reg, room as f32, lh as f32) {
                cv.text_a(&t, (r.left + r.right - tw) / 2, (r.top + r.bottom - lh) / 2, theme().accent, a);
            }
        }
    }
    /// The window-wide animations' frame: splits opening and closing, tabs sliding, the landing zone, a gap's
    /// grip. True while any moves.
    fn step_ui(&mut self) -> bool {
        let now = Instant::now();
        let dt = (now - self.tick).as_secs_f64().min(1.0 / 30.0);
        self.tick = now;
        let mut moved = false;
        // a split's gap dragged: resized to where the pointer was, once a frame
        if let (Some(sid), Some((x, y))) = (self.gut, self.gut_to.take()) {
            if let Some(&(_, r, g, vert)) = self.gutters.iter().find(|x| x.0 == sid) {
                let gw = if vert { g.right - g.left } else { g.bottom - g.top };
                let (pos, len) = if vert { (x - r.left, r.right - r.left - gw) } else { (y - r.top, r.bottom - r.top - gw) };
                let min = (self.px(140) as f64 / len.max(1) as f64).min(0.5);
                let ratio = ((pos - gw / 2) as f64 / len.max(1) as f64).clamp(min, 1.0 - min);
                self.tabs[self.cur].root.set_ratio(sid, ratio);
                self.layout();
            }
        }
        if self.tabs[self.cur].root.animate(dt) {
            self.layout();
            moved = true;
        }
        if !self.dying.is_empty() {
            self.bury(false);
            moved = true;
        }
        let dragged = match &self.drag {
            Some(Drag { grab: Grab::Tab(k), on: true, .. }) => Some(*k),
            _ => None,
        };
        let mut slid = false;
        for (k, t) in self.tabs.iter_mut().enumerate() {
            if t.x.is_nan() || dragged == Some(k) {
                (t.x, t.v) = (t.to, 0.0);
            }
            slid |= spring(&mut t.x, &mut t.v, t.to, dt, 28.0, 0.3);
        }
        if self.plus.0.is_finite() {
            slid |= spring(&mut self.plus.0, &mut self.plus.1, self.plus.2, dt, 28.0, 0.3);
        }
        // tabs opening and closing: unfold and fold away, then settle
        for t in &mut self.tabs {
            if t.born.is_some_and(|b| b.elapsed().as_secs_f64() >= TAB_IN) {
                t.born = None;
                slid = true;
            }
        }
        self.ghosts.retain(|g| g.at.elapsed().as_secs_f64() < TAB_OUT);
        if slid || !self.ghosts.is_empty() || self.tabs.iter().any(|t| t.born.is_some()) || self.spinning() {
            unsafe { InvalidateRect(self.main, null(), 0) };
            moved = true;
        }
        if let Some(o) = &mut self.ov {
            let mut m = false;
            for i in 0..4 {
                m |= spring(&mut o.r[i], &mut o.v[i], o.to[i], dt, 36.0, 0.25);
            }
            let want = if o.on { 1.0 } else { 0.0 };
            if o.a != want {
                o.a = if want > o.a { (o.a + dt / 0.08).min(1.0) } else { (o.a - dt / 0.15).max(0.0) };
                m = true;
            }
            if !o.on && o.a == 0.0 {
                self.ov = None;
                m = true;
            }
            if m {
                self.show_zone();
                moved = true;
            }
        }
        // a gap's grip: lit while the pointer is on the gap or drags it
        let hot = self.gut.or(match self.hot {
            Some(Hit::Gutter(s)) => Some(s),
            _ => None,
        });
        let (id, a) = self.gut_lit;
        let want = if hot.is_some() { 1.0 } else { 0.0 };
        let (id, a) = match hot {
            Some(h) if h != id => (h, 0.0),
            _ => (id, a),
        };
        if a != want {
            let a = if want > a { (a + dt / 0.2).min(1.0) } else { (a - dt / 0.3).max(0.0) };
            self.gut_lit = (id, a);
            unsafe { InvalidateRect(self.main, null(), 0) };
            moved = true;
        } else {
            self.gut_lit = (id, a);
        }
        moved
    }
    /// Whether `step_ui` has anything to do.
    fn ui_moving(&self) -> bool {
        let hot = self.gut.is_some() || matches!(self.hot, Some(Hit::Gutter(_)));
        self.gut_to.is_some()
            || !self.dying.is_empty()
            || self.ov.as_ref().is_some_and(|o| o.r != o.to || o.a != if o.on { 1.0 } else { 0.0 } || o.v != [0.0; 4])
            || self.tabs.iter().any(|t| t.x != t.to || t.v != 0.0 || t.born.is_some())
            || !self.ghosts.is_empty()
            || self.spinning()
            || self.plus.0.is_finite() && (self.plus.0 != self.plus.2 || self.plus.1 != 0.0)
            || self.gut_lit.1 != if hot { 1.0 } else { 0.0 }
            || self.tabs[self.cur].root.moving()
    }

    /// All: an empty search box.
    fn browse_all(&self) {
        self.visit("");
        unsafe { SetWindowTextW(self.edit, wz("").as_ptr()) };
    }

    /// One of Mosaic's own lists (see [`LISTS`]): Home, Recent, This PC… The search box gets its word.
    fn browse_list(&self, q: &str) {
        self.visit(q);
        unsafe { SetWindowTextW(self.edit, wz(q).as_ptr()) };
    }

    /// Remember `q` as the newest place (dropping any "forward" ones), after the one we are leaving.
    fn visit(&self, q: &str) {
        if let Some(d) = listed_dir(q) {
            crate::recent::note(&d, true);
            side::sync();
        }
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
        if let Some(d) = self.here() {
            return d;
        }
        if let Some(parent) = sel.first().and_then(|p| std::path::Path::new(p).parent()) {
            return parent.to_string_lossy().into_owned();
        }
        std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into())
    }

    /// The folder on screen: what a Browse listing (`child:"…"` / `descendant:"…"`, maybe narrowed by more words)
    /// shows. Drive roots get their `\`.
    fn here(&self) -> Option<String> {
        listed_dir(&self.args.q)
    }

    /// Search inside `dir` (and below it), the box ready for the words to look for. Back returns to before.
    fn search_in(&self, dir: &str) {
        let q = format!("descendant:\"{}\" ", dir.trim_end_matches('\\'));
        self.visit(q.trim_end());
        unsafe {
            SetWindowTextW(self.edit, wz(&q).as_ptr());
            SetFocus(self.edit);
            SendMessageW(self.edit, 0x00B1, q.encode_utf16().count(), q.encode_utf16().count() as isize); // EM_SETSEL: caret at the end
        }
    }

    fn select_all(&mut self) {
        if self.matches > 0 {
            let mut s = Sel::default();
            s.add(0, self.matches - 1);
            self.set_sel(s, self.focus, Some(Some(0)));
        }
    }

    /// Where item `i`'s name is drawn (as `paint` puts it), which its rename field covers.
    fn name_box(&self, i: usize) -> RECT {
        let (x, top) = self.spot(i);
        let (x, top) = (x as i32, top.round() as i32);
        let lh = self.gfx.f.lh;
        if self.grid {
            let left = x + self.px(G + 5);
            let top = top + self.px(G + 5) + self.tile() + self.px(6);
            RECT { left, top, right: left + self.tile(), bottom: top + 2 * lh }
        } else {
            let c = self.cols(self.cw())[0];
            let top = top + self.px(2) + (self.rh() - self.px(4) - lh) / 2;
            let h = lh;
            let icon = self.px(26); // the name starts beside the icon, at the column's near edge
            let (left, right) = if self.rtl { (c.0, c.1 - icon) } else { (c.0 + icon, c.1) };
            RECT { left, top, right, bottom: top + h }
        }
    }

    /// Alt+Up while browsing a folder: its parent. False when the search box isn't showing a folder listing.
    fn browse_up(&self) -> bool {
        let Some(dir) = self.args.q.trim().strip_prefix("child:\"").or_else(|| self.args.q.trim().strip_prefix("descendant:\"")).and_then(|d| d.strip_suffix('"')) else { return false };
        let Some((parent, _)) = dir.rsplit_once('\\').filter(|(p, _)| !p.is_empty()) else { return false }; // already at a drive's top
        self.browse_to(parent);
        true
    }

    fn run(&mut self, keep: bool) {
        self.seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.fresh |= !keep;
        if keep {
            self.live_at = Instant::now(); // `done` turns it into the refresh's cost, then its finish
        } else {
            self.busy = Some(Instant::now());
        }
        self.terms = terms(&self.args.q);
        if !keep && self.windows {
            let (g, s) = self.view_for(&self.args.q);
            self.shape(g, s);
            if let Some((by, desc)) = self.sort_for(&self.args.q) {
                (self.args.sort, self.args.dir) = (by, if desc { "desc" } else { "asc" }.into());
            }
        }
        if self.tabs.get(self.cur).is_some_and(|t| t.focus == self.id) {
            side::current(&self.args.q); // the place marked at once, not at the next poll
        }
        let mut args = self.args.clone();
        args.q = self.scoped(&args.q);
        let _ = self.jobs.send(Job { seq: self.seq, who: self.id, args, h: self.backend.handle() });
    }
    /// Files changed here (moved, deleted…): for two seconds the index is asked every 50 ms (the service hears of
    /// a change the moment it lands) and the panes on screen list again each time it changed, without the pause
    /// kept between live refreshes. A status call is a pipe round trip: nothing next to the refresh itself.
    fn relist_soon(&mut self) {
        self.fast = Some(Instant::now());
        unsafe { SetTimer(self.main, T_POLL, 50, None) };
    }
    /// The pane a search result is for: the one whose newest search it is.
    fn pane_by_seq(&self, seq: u64) -> Option<usize> {
        if seq == self.seq {
            return Some(self.id);
        }
        self.panes.iter().position(|v| v.as_ref().is_some_and(|v| v.seq == seq))
    }

    fn done(&mut self, (seq, args, r): Done) {
        if seq != self.seq {
            return;
        }
        self.busy = None;
        let fresh = std::mem::take(&mut self.fresh);
        if !fresh {
            (self.live_cost, self.live_at) = (self.live_at.elapsed(), Instant::now());
        }
        // a live refresh of a folder nothing changed in: the very rows on screen, so they stay as they are
        if !fresh && r.stamp.is_some() && r.stamp == self.version && r.total == self.matches && r.error.is_none() && args == self.shown {
            return;
        }
        self.version = r.stamp;
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
            save::picked(self); // a folder picker's box: the folder now on screen
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
        if let Some(y) = self.want_y.take() {
            // a restored tab: where it was scrolled to
            (self.y, self.target, self.glide.0) = (y, y, y);
            self.clamp();
        }
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
        // a renamed or new item: selected once it's listed (the new one then renamed, as in Explorer)
        if let Some((p, rename, until)) = LISTED.with(|l| l.borrow().clone()) {
            if Instant::now() > until {
                LISTED.set(None);
            } else if let Some(k) = find(&self.shown, 0, self.matches.min(5000), &self.backend.handle()).hits.iter().position(|h| h.path.eq_ignore_ascii_case(&p)) {
                LISTED.set(None);
                self.set_sel(Sel::one(k), Some(k), Some(Some(k)));
                self.show_row(k);
                if rename {
                    unsafe { PostMessageW(self.list, WM_RENAME, 0, 0) };
                }
            }
        }
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
    /// Compare mode: a bar at the item's left, green when only this side has it, orange when the two differ.
    fn cmp_mark(&self, path: &str, bx: &RECT) {
        let Some(&k) = self.cmp.get(&path.to_lowercase()) else { return };
        let c = if k == 1 { 0x3CB371 } else { 0xF2994A };
        let r = RECT { left: bx.left, top: bx.top + self.px(3), right: bx.left + self.px(3), bottom: bx.bottom - self.px(3) };
        self.cl.rrect(&r, 1.5, Some(c), None);
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
        if self.fast.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
            self.fast = None;
            unsafe { SetTimer(self.main, T_POLL, 500, None) };
        }
        let st = self.backend.status();
        // a busy service refuses a connection now and then (every pipe instance taken while it scans); only an
        // unreachable one for ten polls in a row (5 s) is worth offering to install
        self.err_polls = if st.state == "error" { self.err_polls + 1 } else { 0 };
        if self.err_polls >= 10 && !self.offered {
            self.offered = true;
            return true;
        }
        let (count, changes) = (st.count, st.changes);
        save::poll();
        if watch(&self.args.q) {
            self.run(true); // a folder the index doesn't see (a USB stick…) changed
        }
        if !self.tabs[self.cur].o.muted {
            self.each_visible(|u| u.poll_view(count, changes));
        }
        self.tab_poll(count, changes);
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
    /// A pane's share of `poll`: its search again when the index changed under it.
    fn poll_view(&mut self, count: usize, changes: u64) {
        // Metadata: results fill in as the metadata index reads files
        let meta = self.args.tags && crate::metadata::generation() != self.meta_gen;
        // Re-ranking millions of entries costs a core for about a second, and a busy disk changes the index all
        // the time: never while scrolling, and no more often than 8x the cost of the last refresh (2 s at least).
        let scrolling = self.y != self.poll_y;
        self.poll_y = self.y;
        let gap = if self.fast.is_some() { self.live_cost } else { (self.live_cost * 8).max(Duration::from_secs(2)) };
        let due = self.live_at.elapsed() >= gap;
        if (count != self.objects || changes != self.changes || meta) && (self.guest || !self.typing) && self.press.is_none() && !scrolling && due {
            self.meta_gen = crate::metadata::generation();
            (self.objects, self.changes) = (count, changes); // `done` skips a result that's the same as before
            self.run(true);
        }
    }

    fn row(&mut self, h: crate::application::index::Hit) -> Row {
        let ext = if h.is_dir { None } else { ext_of(&h.name) };
        // Home lists Quick access folders: each gets its own shell icon (Desktop, Downloads... as in Explorer)
        let own = matches!(ext.as_deref(), Some("exe" | "lnk" | "ico" | "msi" | "scr")) || (h.is_dir && (list_of(&self.args.q).is_some() || self.special.contains(&h.path.to_lowercase())));
        let (dir, name) = match h.path.rsplit_once('\\') {
            Some((_, "")) => ("", h.name.as_str()), // a drive (This PC): "Local Disk (C:)"
            Some(p) => p,
            None => ("", h.path.as_str()),
        };
        Row {
            name: w(name),
            dir: w(dir),
            // folders from the index carry their total; other folder rows (drives, Home places) have none
            size_s: if h.is_dir && h.size < 0 { Vec::new() } else { w(&fmt_size(h.size.max(0))) },
            ty: w(&type_cache(&ext, h.is_dir)),
            date: fmt_date(h.modified),
            hidden: h.attributes & 0x2 != 0,
            link: matches!(ext.as_deref(), Some("lnk" | "url")),
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
            self.forget_pics();
        }
        ok
    }
    fn forget_pics(&mut self) {
        self.texts.clear();
        self.sys_pics.clear();
        self.own.clear();
        self.thumbs.clear();
        self.thumb_order.clear();
        self.thumb_bytes = 0;
        self.jumbo.clear();
        self.link_ov.clear();
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
        let (p, hw, lw, size) = (path.to_string(), self.main as isize, self.list as usize, self.isz);
        shell::shell_spawn(Box::new(move |dropped| {
            // decoded here, on the helper's thread: the UI thread only uploads the pixels
            let px = if dropped { None } else { shell::thumbnail(&p, size, true).and_then(|b| png_pixels(&b, size)) };
            let b: Box<(String, Option<Px>, bool)> = Box::new((p, px, dropped));
            unsafe { PostMessageW(hw as HWND, WM_ICON, lw, Box::into_raw(b) as isize) };
        }));
    }

    fn got_icon(&mut self, (p, px, dropped): (String, Option<Px>, bool)) {
        self.pending.remove(&p);
        if dropped || unsafe { IsWindowVisible(self.list) } == 0 {
            return; // a pane put away meanwhile: asked again when it's back
        }
        let pic = px.and_then(|(px, w, h)| self.upload(&px, w, h));
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

    /// The Windows folder (Videos, Pictures...) matching what a folder holds, when it holds one kind of file.
    fn kind_icon(&self, path: &str, is_dir: bool) -> Option<String> {
        // Home keeps each folder's own shell icon (Documents stays grey whatever it holds)
        if !is_dir || list_of(&self.args.q).is_some() || self.special.contains(&path.to_lowercase()) {
            return None;
        }
        self.kinds.get(path).and_then(|&(k, _)| self.kind_dirs.get(k as usize)).cloned().flatten()
    }

    /// The path whose shell icon a row wears: its own, or the Windows folder matching what the folder holds (a
    /// tile shows what's inside instead, or both, as View > Icons… says; a row is too small to tell).
    fn icon_key(&self, path: &str, is_dir: bool) -> String {
        if self.both(path, is_dir).is_some() {
            return format!("{BOTH}{path}");
        }
        let kind = if self.grid { self.folder_icons == 0 } else { self.folder_icons != 1 };
        kind.then(|| self.kind_icon(path, is_dir)).flatten().unwrap_or_else(|| path.to_string())
    }

    /// Both, on a tile: the kind's Windows folder and its kind, for [`fold_in`].
    fn both(&self, path: &str, is_dir: bool) -> Option<(String, u8)> {
        if !self.grid || self.folder_icons != 2 {
            return None;
        }
        Some((self.kind_icon(path, is_dir)?, self.kinds.get(path)?.0))
    }

    /// View > Icons… changed: every pane's pictures made again.
    fn icons_changed(&mut self) {
        self.forget_pics();
        for v in self.panes.iter_mut().flatten() {
            v.pics_gen = u32::MAX; // forgotten when next shown (`canvas_ready`)
        }
        self.save_settings();
        self.invalidate_all();
    }

    /// The shortcut arrow's look or size changed: only the arrows are made again; `save` once the choice is final.
    fn arrow_changed(&mut self, save: bool) {
        self.link_ov.clear();
        for v in self.panes.iter_mut().flatten() {
            v.link_ov.clear();
        }
        if save {
            self.save_settings();
        }
        self.invalidate_all();
    }

    /// The shortcut arrow over the icon at (x, y, w, h), in its bottom-left corner, at 100 % as big as Windows draws its own:
    /// 100 px on a 256 icon, 40 on 96, 20 on 48, 13 on 32, 8 on 16 (imageres.dll's overlay measured), made at that size.
    fn link_badge(&mut self, x: i32, y: i32, w: i32, h: i32) -> Option<(Bitmap, RECT)> {
        let n = w.max(h);
        let s = ((n as f64 * 0.39 + 1.5) * self.link_size as f64 / 100.0).round().clamp(4.0, n.max(4) as f64) as i32;
        if !self.link_ov.contains_key(&s) {
            let pic = png_pixels(ARROWS[self.link_arrow as usize], s).and_then(|(px, w, h)| self.upload(&px, w, h));
            self.link_ov.insert(s, pic);
        }
        let (bmp, _, _) = self.link_ov.get(&s).cloned().flatten()?;
        Some((bmp, RECT { left: x, top: y + h - s, right: x + s, bottom: y + h }))
    }

    fn save_kinds(&mut self) {
        if !self.kinds_changed.is_empty() {
            self.kinds_saved = std::time::Instant::now();
            let changed = std::mem::take(&mut self.kinds_changed);
            // the first save writes it whole (there's no snapshot yet), so the old text file can go
            if crate::kvlog::save(&kinds_path(), &self.kinds, &changed).is_ok() {
                let _ = std::fs::remove_file(kinds_path().with_extension("txt"));
            }
        }
    }

    fn got_kind(&mut self, (p, k, m): (String, u8, u64)) {
        self.kind_pending.remove(&p);
        if self.kinds.len() > 200_000 {
            self.kinds_changed.extend(self.kinds.drain().map(|(p, _)| p));
        }
        if self.kinds.insert(p.clone(), (k, m)) != Some((k, m)) {
            self.kinds_changed.push(p);
        }
        // saved while browsing at most every 5 minutes; the rest lands on exit
        if self.kind_pending.is_empty() && self.kinds_saved.elapsed() > std::time::Duration::from_secs(300) {
            self.save_kinds();
        }
        if k != 0 {
            self.each_visible(|u| {
                u.want_thumbs();
                unsafe { InvalidateRect(u.list, null(), 0) };
            });
        }
    }

    /// Ask the shell helpers for thumbnails of the tiles on screen and one screen below.
    fn want_thumbs(&mut self) {
        if !self.grid || self.matches == 0 {
            return;
        }
        let (h, rh) = (self.list_size().1 as f64, self.rh() as f64);
        let r0 = (self.y / rh).floor() as usize;
        let r1 = ((self.y + 2.0 * h) / rh).floor() as usize;
        let (tile, ts, hw, lw) = (self.tile(), self.tsize(), self.main as isize, self.list as usize);
        // folders only in Home (`own`): their shell icon (Documents, Downloads...), not a preview of their contents
        let mut seen = HashSet::new();
        // (key, path, icon only, a shortcut shown as what it opens, Both's kind folder and kind)
        let want: Vec<(String, String, bool, bool, Option<(String, u8)>)> = (r0 * self.per..((r1 + 1) * self.per).min(self.matches))
            .filter_map(|i| self.get(i))
            .filter_map(|r| {
                let key = self.icon_key(&r.path, r.is_dir);
                // a plain folder: what's inside (View > Icons…), else the type's jumbo folder
                let shown = r.tkey != "svg" && (!r.is_dir || r.own || key != r.path || self.folder_icons != 0);
                let icon_only = r.is_dir && (r.own || key != r.path);
                let follow = r.link && r.tkey == "lnk";
                // fetched: the kind's Windows folder (Type), else the item itself
                let get = if key.starts_with(BOTH) { r.path.clone() } else { key.clone() };
                shown.then(|| (key, get, icon_only, follow, self.both(&r.path, r.is_dir)))
            })
            .filter(|(key, ..)| !self.thumb_pending.contains(key) && self.thumbs.get(key).is_none_or(|t| t.1 != tile) && seen.insert(key.clone()))
            .collect();
        // shell_spawn runs the newest job first: queue bottom-up so the top-left tile comes first
        for (key, p, icon_only, follow, both) in want.into_iter().rev() {
            self.thumb_pending.insert(key.clone());
            shell::shell_spawn(Box::new(move |dropped| {
                // decoded and resampled here, on the helper's thread: the UI thread only uploads the pixels
                let px = if dropped {
                    None
                } else if let Some((dir, kind)) = both {
                    // the kind's folder holding a preview of its first such file (that folder alone when there's none);
                    // drawn at full size, then made tile-sized as a whole, so its edges come out smooth
                    let icon = shell::thumbnail(&dir, ts, true).and_then(|b| png_pixels(&b, ts));
                    let inner = newest_of_kind(&p, kind).and_then(|f| {
                        let f = f.to_string_lossy();
                        // no preview (Windows answers with the icon itself): drawn as Windows draws an icon there
                        let b = shell::thumbnail(&f, ts, false)?;
                        let icon = shell::thumbnail(&f, ts, true).is_some_and(|i| i == b);
                        Some((b, icon))
                    });
                    icon.map(|i| shrink(inner.and_then(|(b, ic)| fold_in(&i, &b, ic)).unwrap_or(i), tile))
                } else if follow {
                    // what it opens when that has a picture of its own (a photo, a video...); else, as Explorer, the
                    // shortcut's own icon: a custom one (a web app's) or one kept for a program since moved or removed
                    let pic = shell::link_target(&p).and_then(|t| {
                        let b = shell::thumbnail(&t, ts, false)?;
                        (shell::thumbnail(&t, ts, true).as_ref() != Some(&b)).then_some(b)
                    });
                    pic.or_else(|| shell::thumbnail(&p, ts, true)).and_then(|b| png_pixels(&b, tile))
                } else {
                    shell::thumbnail(&p, ts, icon_only).and_then(|b| png_pixels(&b, tile))
                };
                let p = key;
                let b: Box<(String, Option<Px>, bool, i32)> = Box::new((p, px, dropped, tile));
                unsafe { PostMessageW(hw as HWND, WM_THUMB, lw, Box::into_raw(b) as isize) };
            }));
        }
    }

    fn got_thumb(&mut self, (p, px, dropped, tile): (String, Option<Px>, bool, i32)) {
        self.thumb_pending.remove(&p);
        if dropped || unsafe { IsWindowVisible(self.list) } == 0 {
            return;
        }
        let pic = px.and_then(|(px, w, h)| self.upload(&px, w, h));
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
            Hit::Sort => t("tip.sort").into(),
            Hit::Preview => t("tip.preview").into(),
            Hit::Smooth => t("tip.smooth").into(),
            Hit::Fold => t(if self.folded { "tip.unfold_bar" } else { "tip.fold_bar" }).into(),
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
            Hit::View(2) => t("tip.windows_view").into(),
            Hit::View(_) => t("tip.list_view").into(),
            Hit::Dir => t("tip.dir").into(),
            Hit::Tag => t("tip.tag").into(),
            Hit::Col(_, k) => col_defs().get(k).map_or(String::new(), |m| t("tip.col").replace("{col}", m.0)),
            Hit::Pane(id) => self.pane_name(id),
            Hit::PaneClose(_) => "Ctrl+W".into(),
            Hit::Gutter(_) | Hit::ColEdge(..) => String::new(),
            Hit::Split => t("tip.split").into(),
            Hit::Search | Hit::SaveName | Hit::SaveType | Hit::SaveOk | Hit::SaveCancel | Hit::SaveWin => String::new(),
            Hit::Menu(i) => menu_bar_labels().get(i).copied().unwrap_or_default().into(),
            // in full: the tab may cut it short (its card says more)
            Hit::Tab(k) => if self.tp.cards { String::new() } else { self.tab_name(k) },
            Hit::TabClose(_) => format!("{} (Ctrl+W)", t("tab.close")),
            Hit::NewTab => format!("{} (Ctrl+T)", t("tab.new")),
            Hit::Group(_) => String::new(),
            Hit::TabList => t("tab.recent").into(),
            Hit::Scroll(_) => String::new(),
            Hit::TreePane(_, p) => self.qname(&self.pane_q(p)),
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
        self.sync_scroll();
    }
    fn animating(&mut self) -> bool {
        if self.split_x.is_some() || self.grip_a != self.grip_want() || self.ui_moving() || FADE.read().unwrap().is_some() || themes::moving() {
            return true;
        }
        let mut any = false;
        self.each_visible(|u| any |= u.view_moving());
        any
    }
    fn view_moving(&self) -> bool {
        self.auto.is_some() || self.band_edge() != 0.0 || (self.target - self.y).abs() > 0.5 || self.fx.is_some()
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
        let w = (((c.right - (x - off) - self.px(preview::GAP)) as f64 / self.scale).round() as i32).max(240);
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
    fn step(&mut self) {
        let dt = (Instant::now() - self.tick).as_secs_f64().min(1.0 / 30.0);
        self.drag_split();
        let want = self.grip_want();
        if self.grip_a != want {
            // the divider's grip fades in (0.25 s) and out (0.35 s)
            let step = dt / if want > self.grip_a { 0.25 } else { 0.35 };
            self.grip_a = if want > self.grip_a { (self.grip_a + step).min(want) } else { (self.grip_a - step).max(want) };
        }
        // drawn by the pane: fully lit for the whole drag, else eased (smoothstep)
        let a = self.grip_a;
        preview::grip(if self.split.is_some() { 1.0 } else { (a * a * (3.0 - 2.0 * a)) as f32 });
        if fading() {
            self.repaint_theme();
        }
        themes::step();
        self.step_ui();
        self.each_visible(|u| u.step_view());
    }
    /// A pane's frame: its scrolling, and a live refresh's slides and outlines.
    fn step_view(&mut self) {
        if let Some(t) = self.fx {
            if t.elapsed().as_secs_f64() > if self.born.is_empty() { SLIDE } else { GLOW } {
                (self.fx, self.slide, self.born) = (None, HashMap::new(), HashSet::new());
            }
            unsafe { InvalidateRect(self.list, null(), 0) };
        }
        let now = Instant::now();
        let dt = (now - self.frame).as_secs_f64().min(0.05);
        self.frame = now;
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
            // steps: the system's lines per notch in the list (one tile row in the grid), landing on row edges
            self.step_acc += delta;
            let n = self.step_acc / 120;
            self.step_acc -= n * 120;
            if n != 0 {
                let rh = self.rh() as f64;
                let rows = if self.grid { 1.0 } else { self.wheel_lines };
                self.glide_to(((self.y / rh).round() - n as f64 * rows) * rh);
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
        if x < 0 || x >= self.cw() {
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
        self.linked();
        save::picked(self);
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
        if !self.show_pv || self.guest {
            return; // the focused pane's selection only
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
                return Some(Some(if ctrl { Act::Reveal } else { Act::Open })); // Ctrl+Enter: where it is, as in Everything
            }
            VK_F2 if !in_search && self.sel.count() > 0 => return Some(Some(Act::Rename)),
            VK_F5 => return Some(Some(Act::Refresh)),
            VK_F11 => unsafe { fullscreen(self.main) },
            VK_BACK if !in_search => {
                self.browse_up(); // as in Explorer
            }
            // Ctrl+F / Ctrl+L / F3: to the search box, its text selected
            0x46 | 0x4C if ctrl => unsafe {
                SetFocus(self.edit);
                SendMessageW(self.edit, 0x00B1, 0, -1); // EM_SETSEL: all
            },
            VK_F3 => unsafe {
                SetFocus(self.edit);
                SendMessageW(self.edit, 0x00B1, 0, -1);
            },
            0x56 if ctrl && !in_search => return Some(Some(Act::Paste)),
            0x4E if ctrl && shift => return Some(Some(Act::NewFolder)),
            0x43 if ctrl && shift && self.sel.count() > 0 && !in_search => return Some(Some(Act::Path)),
            VK_ESCAPE => {
                if self.auto.take().is_some() {
                    unsafe { ReleaseCapture() };
                } else {
                    self.set_sel(Sel::default(), None, Some(None));
                }
            }
            VK_DELETE if !in_search => return Some(Some(if shift { Act::DelNow } else { Act::Del })),
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
            0x41 if ctrl && !in_search => self.select_all(),
            // as in Explorer: Ctrl+D deletes, Ctrl+Z undoes (in the search box, its own text's undo), Ctrl+Shift+1…6
            // pick a view (1–4 icon sizes, 5 List and 6 Details: the list)
            0x44 if ctrl && !in_search => return Some(Some(Act::Del)),
            0x5A if ctrl && !in_search => return Some(Some(Act::Undo)),
            0x31..=0x36 if ctrl && shift => return Some(Some(Act::View(VIEWS[(vk as usize - 0x31).min(4)].1))),
            // tabs: `tab_key` (native_tabs.rs), ahead of this
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
                if !r.size_s.is_empty() {
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
                        bytes += r.size;
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
    /// Name, Path, Size, Type, Modified column boxes, left-to-right reading order even in
    /// RTL (see `fit_cols`) — callers index by column, not by screen side. A column with no width is out.
    fn cols(&self, width: i32) -> [(i32, i32); 5] {
        let (f, [s, t, m]) = self.colw;
        fit_cols(width, self.px(16), self.px(10), f, [self.px(s), self.px(t), self.px(m)], self.px(180), self.px(60), self.rtl)
    }
    /// The edge after column `k` dragged to `x`: every pane's columns take the new widths.
    fn drag_col(&mut self, x: i32) {
        let Some((_, k, x0, w0)) = self.col_drag else { return };
        let d = if self.rtl { x0 - x } else { x - x0 };
        let w = col_resize(w0, k, d, self.px(40));
        let css = |v: i32| (v as f64 / self.scale).round() as i32;
        let f = if w[0] > 0 && w[1] > 0 { w[0] as f64 / (w[0] + w[1]) as f64 } else { self.colw.0 };
        // a column that's out keeps the width it comes back with
        let keep = |k: usize, old: i32| if w0[k] > 0 { css(w[k]) } else { old };
        let [s, t, m] = self.colw.1;
        let colw = (f.clamp(0.05, 0.95), [keep(2, s), keep(3, t), keep(4, m)]);
        if colw != self.colw {
            self.colw = colw;
            self.col_gen += 1;
            self.layout();
            self.invalidate_all();
        }
    }

    fn layout(&mut self) {
        self.layout_at(self.first_visible());
    }

    /// How many tiles of the chosen size fit across the list.
    fn fit_per(&self) -> usize {
        (self.cw() / (self.tile() + self.px(2 * G + 10))).max(1) as usize
    }

    /// Where the lists end: at the status bar, or at a Save/Open window's bar when the status bar is up top.
    fn list_bottom(&self) -> i32 {
        if self.status_bot <= self.head_h { self.save_top } else { self.status_top }
    }

    fn layout_at(&mut self, first: usize) {
        if self.guest {
            self.relayout = true; // once the focused pane is back in the fields
            return;
        }
        unsafe {
            let mut c: RECT = zeroed();
            GetClientRect(self.main, &mut c);
            let (wd, ht) = (c.right, c.bottom);
            let (g, lh) = (&self.gfx, self.gfx.f.lh);
            let scale = self.scale;
            let px = move |v: i32| (v as f64 * scale).round() as i32;
            let mut hits = Vec::new();
            // header, as Everything's toolbar: back / forward, then the search box across the window and the filter
            // dropdown at its right, both compact (4px padding + 1px border, twice)
            let box_h = lh + px(10);
            let tab_h = lh + px(12);
            // the classic File/Edit/View/Search strip, flush against the top of the header
            let mut mx = px(8);
            for (i, label) in menu_bar_labels().iter().enumerate() {
                let w = g.width(label, &g.f.reg) + px(20);
                hits.push((RECT { left: mx, top: px(6), right: mx + w, bottom: lh + px(16) }, Hit::Menu(i)));
                mx += w;
            }
            // the tabs (native_tabs.rs): along the rest of that strip, standing on the toolbar; or above the status
            // bar; or down the left side
            let pos = self.tp.pos;
            self.menu_h = lh + px(20);
            if pos == 0 {
                let area = RECT { left: mx + px(20), top: px(6), right: wd - px(16), bottom: ht };
                let sh = self.place_strip(area, &mut hits);
                self.menu_h = self.menu_h.max(px(6) + sh);
            }
            self.head_h = self.menu_h + tab_h + px(12) + 1;
            let g = &self.gfx;
            let side = px(10);
            let mut x = side;
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
            self.save_top = ht - self.save_height(box_h);
            if self.save_top < ht {
                // a Save/Open window: right under the search bar, the header growing by it
                self.status_top = self.head_h - 1;
                self.head_h += bar_h;
            } else {
                self.status_top = self.save_top - bar_h;
            }
            self.status_bot = self.status_top + bar_h;
            let ctl_h = lh + px(4);
            let y = self.status_top + 1 + (bar_h - 1 - ctl_h) / 2;
            let mut x = wd - px(10);
            let mut push = |width: i32, h: Hit, gap: i32| {
                let r = RECT { left: x - width, top: y, right: x, bottom: y + ctl_h };
                hits.push((r, h));
                x = r.left - gap;
            };
            let seg = |s: &str| g.width(s, &g.f.reg) + px(20);
            push(ctl_h, Hit::Fold, px(6));
            if !self.folded {
                push(seg(t("toolbar.grid")), Hit::View(1), -1); // the segments share their borders
                push(seg(t("toolbar.list")), Hit::View(0), -1);
                push(seg("Windows"), Hit::View(2), px(10));
                push(seg(t("toolbar.preview")), Hit::Preview, px(6));
                push(seg(t("toolbar.smooth")), Hit::Smooth, px(6));
                push(seg(t("toolbar.subfolders")), Hit::Browse(2), -1); // like List | Grid, but either can be switched off again
                push(seg(t("toolbar.browse")), Hit::Browse(1), px(6));
                push(seg(t("toolbar.sidebar")), Hit::Side, px(10));
                if self.grid {
                    push(g.width("▲", &g.f.reg) + px(20), Hit::Dir, px(6));
                    push(select(&col_defs()), Hit::Sort, px(6));
                }
                if let Some(f) = filters().iter().find(|f| !f.1.is_empty() && f.1 == self.args.cat) {
                    push(g.width(f.0, &g.f.reg) + px(16), Hit::Tag, 0);
                }
            }
            // column header (8px below the header; list view only), list, status bar; the preview pane at the right
            let lh_y = self.head_h + px(8);
            // the pane: as dragged (the list keeps 300 px), else 40% of the window. The list never gives up its
            // own minimum width for the pane's sake — on a narrow window the pane shrinks, then hides, first.
            let min_list = px(220);
            // the side panels: flush against the window's edges, from the toolbar down to the status bar, a hairline
            // between them and the lists, which run right up to it (no strip of background between)
            let (top, mut bottom) = (self.head_h, self.list_bottom());
            self.place_save(wd, box_h, tm.tmHeight, &mut hits);
            let mut left = 0;
            match pos {
                1 => {
                    let area = RECT { left: px(8), top: 0, right: wd - px(8), bottom: ht };
                    let sh = self.strip_height(area);
                    let area = RECT { top: bottom - sh - px(4), ..area };
                    self.place_strip(area, &mut hits);
                    bottom = area.top - px(4);
                }
                2 => {
                    left = px(210);
                    self.place_strip(RECT { left: 0, top, right: left, bottom }, &mut hits);
                }
                _ => {}
            }
            // the places panel, left of the list: it gives way first on a narrow window
            let side_w = if self.show_side && wd >= px(760) { px(side::width()) } else { 0 };
            MoveWindow(self.side, left, top, side_w.max(1), (bottom - top).max(1), 1);
            let left = left + side_w;
            ShowWindow(self.side, if side_w > 0 { SW_SHOWNA } else { SW_HIDE });
            let max_pane = (wd - min_list - left - px(preview::GAP)).max(0);
            let auto = ((wd as f64 * 0.4) as i32).clamp(px(240), px(960)).min(wd / 2).min(max_pane);
            let show_pane = self.show_pv && max_pane >= px(160);
            let pane_w = if !show_pane { 0 } else if self.pv_w > 0 { px(self.pv_w).min(max_pane).max(px(160)) } else { auto };
            let right = if show_pane { wd - pane_w - px(preview::GAP) } else { wd };
            // the pane's window takes in the divider: it draws its grip, which so moves with it
            MoveWindow(self.pane, right, top, pane_w.max(1) + px(preview::GAP), (bottom - top).max(1), 1);
            ShowWindow(self.pane, if show_pane { SW_SHOWNA } else { SW_HIDE });
            if show_pane {
                let gap = RECT { left: right, top, right: right + px(preview::GAP), bottom };
                let (mid, half) = ((gap.top + gap.bottom) / 2, (gap.bottom - gap.top) / 8);
                self.grip = RECT { top: mid - half, bottom: mid + half, ..gap };
                hits.push((gap, Hit::Split));
            }
            // the panes share the room: each its caption (once split), column header and list
            self.content = RECT { left, top: lh_y, right, bottom };
            let (mut leaves, mut gutters) = (Vec::new(), Vec::new());
            let o = &self.tabs[self.cur].o;
            let cap = lh + px(14);
            match o.zoom {
                // zoomed: the one pane takes the room, the others wait
                Some(z) => Node::Leaf(z).place(self.content, px(8), (&[], 0), &mut leaves, &mut gutters),
                None => self.tabs[self.cur].root.place(self.content, px(8), (&o.folded, cap), &mut leaves, &mut gutters),
            }
            for &(sid, _, g, _) in &gutters {
                hits.push((g, Hit::Gutter(sid)));
            }
            let multi = leaves.len() > 1 && (self.tp.captions || !self.tabs[self.cur].o.folded.is_empty());
            for &(id, r) in &leaves {
                let first = (id == self.id).then_some(first);
                self.with_pane(id, |u| u.place_pane(r, multi, first, &mut hits));
            }
            for id in 0..self.panes.len() {
                let on = leaves.iter().any(|l| l.0 == id);
                let l = self.list_of(id);
                if !l.is_null() && (IsWindowVisible(l) != 0) != on {
                    ShowWindow(l, if on { SW_SHOWNA } else { SW_HIDE });
                }
            }
            (self.areas, self.gutters, self.hits) = (leaves, gutters, hits);
            InvalidateRect(self.main, null(), 0);
        }
    }

    /// Lay this pane out in `r` (main window coordinates): its caption when the tab is split, column header, list.
    fn place_pane(&mut self, r: RECT, multi: bool, first: Option<usize>, hits: &mut Vec<(RECT, Hit)>) {
        let px = |v: i32| (v as f64 * self.scale).round() as i32;
        let first = first.unwrap_or_else(|| self.first_visible());
        let closing = self.dying.contains(&self.id); // shrinking away: nothing on it answers any more
        let mut top = r.top;
        self.cap = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if multi {
            self.cap = RECT { left: r.left + px(6), top, right: r.right - px(4), bottom: top + self.gfx.f.lh + px(10) };
            top = self.cap.bottom + px(4);
            if !closing {
                let c = self.cap;
                hits.push((tab_x(&c, c.bottom - c.top - px(6)), Hit::PaneClose(self.id)));
                hits.push((c, Hit::Pane(self.id)));
            }
        }
        self.lh = RECT { left: r.left, top, right: r.right - px(SB), bottom: top + if self.grid { 0 } else { px(30) } };
        if !self.grid && !closing {
            let cols = self.cols(self.lh.right - self.lh.left);
            // the gaps between columns: drag to resize (first, so they win over the headers they overlap)
            let on = |k: usize| cols[k].1 > cols[k].0;
            for k in (0..cols.len() - 1).filter(|&k| on(k) && on(k + 1)) {
                let x = self.lh.left + if self.rtl { (cols[k].0 + cols[k + 1].1) / 2 } else { (cols[k].1 + cols[k + 1].0) / 2 };
                hits.push((RECT { left: x - px(4), top: self.lh.top + px(4), right: x + px(4), bottom: self.lh.bottom - px(4) }, Hit::ColEdge(self.id, k)));
            }
            for (k, (a, b)) in cols.iter().enumerate().filter(|&(k, _)| on(k)) {
                hits.push((RECT { left: self.lh.left + a, top: self.lh.top, right: self.lh.left + b, bottom: self.lh.bottom }, Hit::Col(self.id, k)));
            }
        }
        let top = self.lh.bottom;
        self.org = POINT { x: r.left, y: top };
        unsafe { MoveWindow(self.list, r.left, top, (r.right - r.left).max(1), (r.bottom - top).max(1), 1) };
        // tiles keep their size: the window's width decides how many fit in a row. Keep the first item in view.
        let per = self.fit_per();
        if per != self.per {
            self.per = per;
            self.texts.clear();
            self.y = (first / self.ncols()) as f64 * self.rh() as f64;
            self.target = self.y;
        }
        self.clamp();
        unsafe { InvalidateRect(self.list, null(), 0) };
    }

    fn save_settings(&self) {
        let p = settings_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        // the main window's place and a Save/Open window's, each written only by its own kind of window
        let keep = |k: &str| load_settings().remove(k).unwrap_or_default();
        let here = || unsafe { place_of(self.main) };
        let (place, save_place) = if save::active() {
            (keep("place"), here())
        } else if self.secondary {
            (keep("place"), keep("save_place"))
        } else {
            (here(), keep("save_place"))
        };
        let m: HashMap<&str, String> = HashMap::from([
            ("view", (if self.def_view.0 { "grid" } else { "list" }).to_string()),
            ("size", self.def_view.1.to_string()),
            ("windows_view", (self.windows as u8).to_string()),
            ("preview", (self.show_pv as u8).to_string()),
            ("pvw", self.pv_w.to_string()),
            ("cols", { let (f, [s, t, m]) = self.colw; format!("{f:.4},{s},{t},{m}") }),
            ("smooth", (self.smooth as u8).to_string()),
            ("folded", (self.folded as u8).to_string()),
            ("folder_color", (self.folder_color as u8).to_string()),
            ("folder_icons", self.folder_icons.to_string()),
            ("link_arrow", self.link_arrow.to_string()),
            ("link_size", self.link_size.to_string()),
            ("browse", self.browse.to_string()),
            ("side", (self.show_side as u8).to_string()),
            ("tags", (self.args.tags as u8).to_string()),
            ("content", (self.args.content as u8).to_string()),
            ("match_case", (self.args.match_case as u8).to_string()),
            ("whole_word", (self.args.whole_word as u8).to_string()),
            ("regex", (self.args.regex as u8).to_string()),
            ("diacritics", (self.args.diacritics as u8).to_string()),
            ("hidden", (self.args.hidden as u8).to_string()),
            ("system", (self.args.system as u8).to_string()),
            ("timestamps", (self.timestamps as u8).to_string()),
            ("theme", theme_name()),
            ("lang", crate::i18n::lang()),
            ("tabs", serde_json::to_string(&self.tp).unwrap_or_default()),
            ("sidebar", side::prefs_json()),
            ("save_dialog", (save::on() as u8).to_string()),
            ("dialog_apps", save::rules_json()),
            ("save_dir", save::last_dir()),
            ("place", place),
            ("save_place", save_place),
        ]);
        let new = serde_json::to_string(&m).unwrap_or_default();
        if std::fs::read_to_string(&p).ok().as_deref() != Some(new.as_str()) {
            let _ = std::fs::write(p, new);
        }
    }

    /// UI language, live: text updates immediately (`t()` reads the active table on every call), and
    /// Arabic's right-to-left layout needs the DirectWrite formats rebuilt, same as a DPI change.
    fn set_lang(&mut self, code: &str) {
        crate::i18n::set_lang(code);
        self.rtl = crate::i18n::is_rtl(code);
        if let Ok(g) = Gfx::new(self.scale, self.rtl) {
            self.cm = Canvas::new(self.main, &g);
            self.new_canvases(&g);
            self.gfx = g;
        }
        self.save_settings();
        side::relabel();
        self.layout();
        self.invalidate();
    }

    /// Every window repainted in the colours of the moment: `theme()` is a global read every draw call touches, so
    /// no rebuild is needed, but the panels, the search box and the title bar are windows of their own.
    fn repaint_theme(&self) {
        unsafe {
            apply_titlebar(self.main);
            InvalidateRect(self.edit, null(), 0);
        }
        preview::invalidate();
        side::invalidate();
        themes::invalidate();
        self.invalidate_all();
    }

    /// Index of the first item on screen.
    fn first_visible(&self) -> usize {
        (self.y / self.rh() as f64) as usize * self.ncols()
    }

    /// The user's switch of view or tile size: kept as the folder's own (Windows), else as List or Grid's.
    fn reshape(&mut self, grid: bool, size: i32) {
        if grid == self.grid && size == self.size {
            return;
        }
        self.shape(grid, size);
        match listed_dir(&self.args.q).filter(|_| self.windows) {
            Some(_) => self.remember(),
            None => {
                self.def_view = (grid, size);
                self.save_settings();
            }
        }
    }
    /// The folder listed keeps its view and sort as they are now.
    fn remember(&self) {
        if let Some(d) = listed_dir(&self.args.q).filter(|_| self.windows) {
            set_folder_view(self.main, &d, self.kind_of(&d), FolderView { grid: self.grid, size: self.size, sort: self.args.sort.clone(), desc: self.args.dir == "desc", n: 0 });
        }
    }
    /// The user's sort: kept as the folder's own, and listed again.
    fn resort(&mut self) {
        self.remember();
        self.run(false);
    }
    /// The sort a listing opens in: its folder's own, else by name as Explorer's. A search keeps the one it has.
    fn sort_for(&self, q: &str) -> Option<(String, bool)> {
        Some(self.own_view(&listed_dir(q)?).map(|v| (v.sort, v.desc)).unwrap_or_else(|| ("name".into(), false)))
    }
    /// A folder's own view: Mosaic's or Explorer's, whichever was set last.
    fn own_view(&self, d: &str) -> Option<FolderView> {
        let mine = folder_views(|m, _| m.get(&d.trim_end_matches('\\').to_lowercase()).cloned());
        let theirs = bag_view(d).map(|v| FolderView { size: if v.grid { v.size } else { mine.as_ref().map_or(self.def_view.1, |m| m.size) }, ..v });
        match (mine, theirs) {
            // the same view, as Mosaic wrote it there: Mosaic's exact tile size
            (Some(a), Some(b)) if a.grid == b.grid && a.sort == b.sort && a.desc == b.desc && (!a.grid || crate::shellbags::icon_of(a.size) == crate::shellbags::icon_of(b.size)) => Some(a),
            (a, b) => [a, b].into_iter().flatten().max_by_key(|v| v.n),
        }
    }
    /// The folder type Explorer files `d` under: of the kind folder it is in, else generic.
    fn kind_of(&self, d: &str) -> &'static str {
        let k = d.trim_end_matches('\\').to_lowercase();
        let i = self.kind_dirs.iter().position(|p| {
            p.as_ref().is_some_and(|p| {
                let p = p.trim_end_matches('\\').to_lowercase();
                k == p || k.starts_with(&format!("{p}\\"))
            })
        });
        ["Generic", "Pictures", "Videos", "Music", "Documents"][i.unwrap_or(0)]
    }
    /// The view a listing opens in: its folder's own, else large icons in Pictures and Videos, else the default.
    fn view_for(&self, q: &str) -> (bool, i32) {
        let Some(d) = listed_dir(q) else { return self.def_view };
        let k = d.trim_end_matches('\\').to_lowercase();
        if let Some(v) = self.own_view(&d) {
            return (v.grid, v.size);
        }
        let under = |p: &Option<String>| {
            p.as_ref().is_some_and(|p| {
                let p = p.trim_end_matches('\\').to_lowercase();
                k == p || k.starts_with(&format!("{p}\\"))
            })
        };
        if self.kind_dirs.iter().skip(1).take(2).any(under) {
            return (true, 96); // Explorer's large icons, as it opens these
        }
        self.def_view
    }
    /// Switch view or tile size, keeping the first visible item in view.
    fn shape(&mut self, grid: bool, size: i32) {
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
        self.invalidate();
    }

    /// Ctrl+wheel / Ctrl +/−: bigger or smaller icons, `ZOOM` a step. Like Explorer, zooming in from the list
    /// opens the grid at its smallest tiles, and zooming out past those goes back to the list.
    fn zoom(&mut self, bigger: bool) {
        let (grid, size) = zoom_step(self.grid, self.size, bigger);
        self.reshape(grid, size);
    }

    /// A toolbar click. Returns the target when it opens something modal (a menu, the Contents page).
    /// A search switch (Content, Metadata, Match case, Whole word, Regex, Diacritics, Timestamps) on or off.
    fn toggled(&self, h: Hit) -> bool {
        match h {
            Hit::Tags => self.args.tags,
            Hit::Content => self.args.content,
            Hit::MatchCase => self.args.match_case,
            Hit::WholeWord => self.args.whole_word,
            Hit::Regex => self.args.regex,
            Hit::Diacritics => self.args.diacritics,
            _ => self.timestamps,
        }
    }

    /// Turns a search switch (see [`Self::toggled`]) over, saves, and searches again.
    fn flip(&mut self, h: Hit) {
        match h {
            Hit::Tags => self.args.tags = !self.args.tags,
            Hit::Content => self.args.content = !self.args.content,
            Hit::MatchCase => self.args.match_case = !self.args.match_case,
            Hit::WholeWord => self.args.whole_word = !self.args.whole_word,
            Hit::Regex => self.args.regex = !self.args.regex,
            Hit::Diacritics => self.args.diacritics = !self.args.diacritics,
            _ => {
                self.timestamps = !self.timestamps;
                crate::datestore::set_enabled(self.timestamps);
            }
        }
        self.save_settings();
        self.run(false);
        self.invalidate();
    }

    fn click_main(&mut self, x: i32, y: i32) -> Option<Hit> {
        let &(_, hit) = self.hits.iter().find(|(r, _)| inside(r, x, y))?;
        match hit {
            Hit::Filter | Hit::Sort | Hit::Preview | Hit::Side | Hit::Lang | Hit::Menu(_) => return Some(hit),
            Hit::SaveType | Hit::SaveOk | Hit::SaveCancel | Hit::SaveWin => return Some(hit),
            Hit::SaveName => save::focus(),
            Hit::Back | Hit::Fwd => self.go_hist(hit == Hit::Back),
            Hit::Tab(k) => self.switch_tab(k),
            Hit::TabClose(k) => self.drop_tab(k, true),
            Hit::Pane(id) => self.activate(id),
            Hit::PaneClose(id) => self.close_pane(id),
            Hit::Gutter(_) | Hit::ColEdge(..) => {}
            Hit::NewTab => self.new_tab("home:", self.cur + 1),
            Hit::TabList => return Some(hit),
            Hit::Group(g) => {
                if let Some(x) = self.groups.iter_mut().find(|x| x.id == g) {
                    x.folded = !x.folded;
                }
                self.layout();
            }
            Hit::Scroll(right) => {
                let d = self.px(200);
                self.scroll_strip(if right { d } else { -d });
            }
            Hit::TreePane(k, p) => {
                if k != self.cur {
                    self.switch_tab(k);
                }
                self.activate(p);
            }
            Hit::Tags | Hit::Content | Hit::MatchCase | Hit::WholeWord | Hit::Regex | Hit::Diacritics | Hit::Timestamps => self.flip(hit),
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
                self.resort();
            }
            Hit::Smooth => {
                self.smooth = !self.smooth;
                self.save_settings();
            }
            Hit::Fold => {
                self.folded = !self.folded;
                self.save_settings();
                self.layout();
            }
            Hit::View(2) => {
                if !self.windows {
                    self.windows = true;
                    self.save_settings();
                    self.run(false); // this folder's own view and sort
                }
            }
            Hit::View(k) => {
                self.windows = false;
                self.shape(k == 1, self.size);
                self.def_view = (self.grid, self.size);
                self.save_settings();
            }
            Hit::Browse(k) => self.set_browse(if self.browse == k { 0 } else { k }),
            Hit::Col(id, k) => {
                self.activate(id);
                let s = col_defs()[k].1;
                let desc = if self.args.sort == s { self.args.dir != "desc" } else { s == "size" || s == "mtime" };
                self.args.sort = s.into();
                self.args.dir = if desc { "desc" } else { "asc" }.into();
                self.resort();
            }
        }
        self.invalidate();
        None
    }

    // -- painting
    fn paint_main(&mut self) {
        self.tab_icons();
        if !self.cm.begin() {
            return;
        }
        let (g, cv, lh) = (&self.gfx, &self.cm, self.gfx.f.lh);
        let px = |v: i32| (v as f64 * self.scale).round() as i32;
        let rad = |v: f64| (v * self.scale) as f32;
        let mut c: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.main, &mut c) };
        cv.clear(theme().bg);
        // header: the tab strip on the window's own colour (as the title bar), the toolbar under it a card
        cv.fill(&RECT { top: self.menu_h, bottom: self.head_h - 1, ..c }, theme().card, 1.0);
        cv.fill(&RECT { top: self.head_h - 1, bottom: self.head_h, ..c }, theme().line, 1.0);
        // status bar
        cv.fill(&RECT { top: self.status_top, bottom: self.status_bot, ..c }, theme().card, 1.0);
        cv.fill(&RECT { top: self.status_top, bottom: self.status_top + 1, ..c }, theme().line, 1.0);
        cv.fill(&RECT { top: self.menu_h, bottom: self.menu_h + 1, ..c }, theme().line, 1.0);
        self.paint_save(c.right);
        let hot = |h: Hit| self.hot == Some(h);
        for &(r, h) in &self.hits {
            if let Hit::Menu(i) = h {
                if hot(h) {
                    cv.rrect(&r, rad(4.0), Some(theme().hl), None);
                }
                put_mid(g, cv, menu_bar_labels()[i], &g.f.reg, &r, r.top + (r.bottom - r.top - lh) / 2, theme().text);
            }
        }
        // search box: a plain edit box, 1px border, accent when focused (a Windows edit, as in Everything)
        let sb = self.search_box;
        let focused = unsafe { GetFocus() } == self.edit;
        cv.rrect(&sb, rad(4.0), Some(theme().bg), Some(if focused { theme().accent } else { theme().line }));
        // filter dropdown, status bar filter and view controls
        for &(r, h) in &self.hits {
            let ty = r.top + (r.bottom - r.top - lh) / 2;
            let tc = if hot(h) { theme().text } else { theme().muted };
            match h {
                Hit::Filter | Hit::Sort => {
                    let label = match h {
                        Hit::Filter => filters().iter().find(|m| m.1 == self.args.cat).map_or(t("filter.everything"), |m| m.0),
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
                Hit::Fold => {
                    // › folds the buttons away to the right, ‹ brings them back
                    if hot(h) {
                        cv.rrect(&r, rad(4.0), Some(theme().hl), None);
                    }
                    let (cx, cy, d) = ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0, rad(3.5));
                    let s = if self.folded { -1.0 } else { 1.0 };
                    cv.lines(&[(cx - s * d / 2.0, cy - d), (cx + s * d / 2.0, cy), (cx - s * d / 2.0, cy + d)], tc, rad(1.4));
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
        if self.tp.pos == 1 {
            let y = self.strip_r.top - self.px(4);
            cv.fill(&RECT { top: y, bottom: y + 1, ..c }, theme().line, 1.0);
        }
        self.paint_strip();
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
        // Windows | List | Grid: the "off" segments first, so the accent border of the "on" one wins the shared edge
        let segs: Vec<(RECT, usize)> = self.hits.iter().filter_map(|&(r, h)| if let Hit::View(k) = h { Some((r, k)) } else { None }).collect();
        for pass in [false, true] {
            for &(r, k) in &segs {
                let on = if k == 2 { self.windows } else { !self.windows && self.grid == (k == 1) };
                if on != pass {
                    continue;
                }
                let radii = match k {
                    2 => [rad(4.0), 0.0, 0.0, rad(4.0)],
                    1 => [0.0, rad(4.0), rad(4.0), 0.0],
                    _ => [0.0; 4],
                };
                cv.corners(&r, radii, if on { theme().hl } else { theme().card }, if on { theme().accent } else { theme().line });
                let ty = r.top + (r.bottom - r.top - lh) / 2;
                let label = match k {
                    2 => "Windows",
                    1 => t("toolbar.grid"),
                    _ => t("toolbar.list"),
                };
                put_mid(g, cv, label, &g.f.reg, &r, ty, if on { theme().accent } else if hot(Hit::View(k)) { theme().text } else { theme().muted });
            }
        }
        // status text: left of the status bar's controls, cut with "…" where they begin
        let stop = self.hits.iter().filter(|x| x.0.top >= self.status_top && x.0.top < self.status_bot).map(|x| x.0.left).min().unwrap_or(c.right);
        if let Some(t) = g.layout(&w(&self.status()), &g.f.reg, (stop - px(20)) as f32, lh as f32) {
            cv.text(&t, px(10), self.status_top + 1 + (self.status_bot - self.status_top - 1 - lh) / 2, theme().muted);
        }
        // each pane's caption and column header
        for (id, _) in self.areas.clone() {
            self.with_pane(id, |u| u.paint_head());
        }
        // between split panes: a hairline down (or across) the middle of the gap
        for &(_, _, g, vert) in &self.gutters {
            let line = if vert {
                let x = (g.left + g.right) / 2;
                RECT { left: x, right: x + 1, ..g }
            } else {
                let y = (g.top + g.bottom) / 2;
                RECT { top: y, bottom: y + 1, ..g }
            };
            self.cm.fill(&line, theme().line, 1.0);
        }
        // a split's gap: its grip, lit while the pointer is on it
        let (gid, a) = self.gut_lit;
        if a > 0.0 {
            if let Some(&(_, _, g, vert)) = self.gutters.iter().find(|x| x.0 == gid) {
                let a = (a * a * (3.0 - 2.0 * a)) as f32;
                let (mx, my) = ((g.left + g.right) / 2, (g.top + g.bottom) / 2);
                let (len, th) = ((if vert { g.bottom - g.top } else { g.right - g.left }) / 8, self.px(4));
                let len = len.clamp(self.px(24), self.px(90));
                let r = if vert { RECT { left: mx - th / 2, right: mx + th / 2, top: my - len / 2, bottom: my + len / 2 } } else { RECT { left: mx - len / 2, right: mx + len / 2, top: my - th / 2, bottom: my + th / 2 } };
                let c = if self.gut.is_some() { theme().accent } else { theme().muted };
                self.cm.rrect_a(&r, th as f32 / 2.0, Some(c), None, a);
            }
        }
        self.cm.end();
    }

    /// This pane's caption (when its tab is split: its place, and × to close it) and column header (list view),
    /// on the main window.
    fn paint_head(&self) {
        let (g, cv, lh) = (&self.gfx, &self.cm, self.gfx.f.lh);
        let px = |v: i32| (v as f64 * self.scale).round() as i32;
        let rad = |v: f64| (v * self.scale) as f32;
        let hot = |h: Hit| self.hot == Some(h);
        let c = self.cap;
        if c.right > c.left {
            // the focused pane's caption stands out (a card, its name in full colour, an accent dot); the others are
            // quiet until pointed at
            let on = !self.guest;
            let near = matches!(self.hot, Some(Hit::Pane(p) | Hit::PaneClose(p)) if p == self.id);
            if on || near {
                cv.rrect(&c, rad(6.0), Some(if on { theme().card } else { theme().hl }), on.then_some(theme().line));
            }
            let my = (c.top + c.bottom) as f32 / 2.0;
            let tc = self.tab_of(self.id).and_then(|k| self.tab_color(k));
            cv.circle((c.left + px(12)) as f32, my, rad(3.0), tc.unwrap_or(if on { theme().accent } else { theme().line }), 1.0);
            let x = tab_x(&c, c.bottom - c.top - px(6));
            let ty = c.top + (c.bottom - c.top - lh) / 2;
            if let Some(t) = g.layout(&w(&place_name(&self.args.q)), &g.f.reg, (x.left - c.left - px(26)) as f32, lh as f32) {
                cv.text(&t, c.left + px(22), ty, if on { theme().text } else { theme().muted });
            }
            if on || near {
                if hot(Hit::PaneClose(self.id)) {
                    cv.rrect(&x, rad(3.0), Some(theme().line), None);
                }
                put_mid(g, cv, "×", &g.f.reg, &x, ty, if near { theme().text } else { theme().muted });
            }
        }
        if self.grid {
            return;
        }
        cv.rrect(&RECT { left: self.lh.left + px(6), ..self.lh }, rad(8.0), Some(theme().card), Some(theme().line));
        let base = |p: f64| {
            let (size, lh) = (p * self.scale, (p * 1.4 * self.scale).round());
            (lh - size * 2724.0 / 2048.0) / 2.0 + size * 2210.0 / 2048.0
        };
        for &(r, h) in &self.hits {
            let Hit::Col(id, k) = h else { continue };
            if id != self.id {
                continue;
            }
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
        // a column gap pointed at or dragged: a thin bar shows where it grabs; at rest, a faint tick
        for &(r, h) in &self.hits {
            let Hit::ColEdge(id, k) = h else { continue };
            if id != self.id {
                continue;
            }
            let on = match self.col_drag {
                Some((p, j, ..)) => p == id && j == k,
                None => hot(h),
            };
            let x = (r.left + r.right) / 2;
            let bar = if on { RECT { left: x - px(1), right: x + px(1), ..r } } else { RECT { left: x, right: x + 1, top: r.top + px(4), bottom: r.bottom - px(4) } };
            cv.rrect(&bar, rad(1.0), Some(if on { theme().accent } else { theme().line }), None);
        }
    }

    /// Look into the folders on screen for what they hold (see `folder_kind`). Run once painting has stopped for a
    /// moment, never per frame: dragging the scroll bar over millions of rows would otherwise start a check, several
    /// full scans in the service, for every folder that flashes past.
    fn want_kinds(&mut self) {
        let (_, h) = self.list_size();
        let rh = self.rh() as f64;
        let r0 = (self.y / rh).floor() as usize;
        let r1 = ((self.y + h as f64) / rh).floor() as usize;
        let (i0, i1) = (r0 * self.ncols(), ((r1 + 1) * self.ncols()).min(self.matches));
        if i0 >= i1 {
            return;
        }
        // a folder holding only one kind of file (all pictures, all videos...) wears that Windows folder's icon
        let todo: Vec<(String, Option<(u8, u64)>)> = (i0..i1)
            .filter_map(|i| self.get(i))
            .filter(|r| r.is_dir && list_of(&self.args.q).is_none() && !self.kind_checked.contains(&r.path))
            .map(|r| (r.path.clone(), self.kinds.get(&r.path).copied()))
            .collect();
        if !todo.is_empty() {
            for (p, _) in &todo {
                self.kind_checked.insert(p.clone());
                self.kind_pending.insert(p.clone());
            }
            let (h, hw) = (self.backend.handle(), self.main as isize);
            std::thread::spawn(move || {
                for (p, cached) in todo {
                    // the cached answer stands while the folder's modified time (entries added or removed) is unchanged
                    let m = mtime_secs(&p);
                    let k = match cached {
                        Some((k, cm)) if cm == m => k,
                        _ => crate::folder_kind(&h, &p),
                    };
                    let b: Box<(String, u8, u64)> = Box::new((p, k, m));
                    unsafe { PostMessageW(hw as HWND, WM_KIND, 0, Box::into_raw(b) as isize) };
                }
            });
        }
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
        let want: Vec<String> = (i0..i1)
            .filter_map(|i| self.get(i))
            .filter_map(|r| {
                let key = self.icon_key(&r.path, r.is_dir);
                ((r.own || key != r.path) && !self.own.contains_key(&key)).then_some(key)
            })
            .collect();
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
        let key = (cw, self.grid, self.tile(), self.col_gen);
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
        // the item being renamed looks unselected, its name in the field, as in Explorer
        let renaming = RENAMING.with(|r| r.borrow().as_ref().map(|r| r.1.clone()));
        for i in i0..i1.max(i0) {
            let texts = self.item_texts(i);
            let Some((is_dir, sys, own, path, tkey, hidden, link)) = self.get(i).map(|r| (r.is_dir, r.sys, r.own, r.path.clone(), r.tkey.clone(), r.hidden, r.link)) else { continue };
            let arrow = link;
            let fade = if hidden { 0.5 } else { 1.0 };
            let color = if is_dir && self.folder_color { theme().accent } else { theme().text };
            let (sel, hover) = match renaming.as_deref() == Some(path.as_str()) {
                true => (false, false),
                false => (self.sel.has(i), self.hover == Some(i)),
            };
            let (dx, dy) = self.lag(&path); // sliding to its new place after a live refresh
            let (dx, dy) = (dx.round() as i32, dy.round() as i32);
            if self.grid {
                let (a, b) = self.cell_x(i % self.per);
                let (a, b) = (a + dx, b + dx);
                let top = ((i / self.per) as f64 * rh as f64 - self.y).round() as i32 + dy;
                let g = self.px(G);
                let bx = RECT { left: a + g, top: top + g, right: b - g, bottom: top + rh - g };
                if sel {
                    self.cl.rrect(&bx, rad(8.0), Some(theme().sel), Some(theme().accent));
                } else if hover {
                    self.cl.rrect(&bx, rad(8.0), Some(theme().row), None);
                }
                self.glow(&path, &bx, rad(8.0));
                self.cmp_mark(&path, &bx);
                let tile = self.tile();
                let pv = RECT { left: bx.left + self.px(5), top: bx.top + self.px(5), right: bx.left + self.px(5) + tile, bottom: bx.top + self.px(5) + tile };
                match self.thumbs.get(&self.icon_key(&path, is_dir)).and_then(|t| t.0.clone()) {
                    // the thumbnail: contained in the preview box, never enlarged past its own size
                    Some((bmp, w, h)) => {
                        let k = (tile as f64 / w as f64).min(tile as f64 / h as f64).min(self.scale);
                        let (dw, dh) = ((w as f64 * k).round() as i32, (h as f64 * k).round() as i32);
                        let (x, y) = (pv.left + (tile - dw) / 2, pv.top + (tile - dh) / 2);
                        self.cl.bitmap(&bmp, &RECT { left: x, top: y, right: x + dw, bottom: y + dh }, fade);
                        if let Some((ov, r)) = arrow.then(|| self.link_badge(x, y, dw, dh)).flatten() {
                            self.cl.bitmap(&ov, &r, fade);
                        }
                    }
                    // until then, the type's jumbo icon filling the box (.pv img.ph)
                    None => {
                        if let Some((bmp, _, _)) = self.jumbo_pic(&tkey, tile.min(256)) {
                            self.cl.bitmap(&bmp, &pv, fade);
                        }
                    }
                }
                if let Some(t) = texts.as_ref().and_then(|t| t.first()) {
                    self.cl.text_a(t, pv.left, pv.bottom + self.px(6), color, fade);
                }
            } else {
                let top = (i as f64 * rh as f64 - self.y).round() as i32 + dy;
                let bx = RECT { left: self.px(6), top: top + self.px(2), right: cw, bottom: top + rh - self.px(2) };
                if sel {
                    self.cl.rrect(&bx, rad(6.0), Some(theme().sel), None);
                } else if hover {
                    self.cl.rrect(&bx, rad(6.0), Some(theme().row), None);
                }
                self.glow(&path, &bx, rad(6.0));
                self.cmp_mark(&path, &bx);
                let cols = self.cols(cw);
                let ty = bx.top + (bx.bottom - bx.top - lh) / 2;
                // icon: the file's own (exe/lnk) or its type's, centered in an 18px box
                let key = self.icon_key(&path, is_dir);
                let pic = if own || key != path { self.own.get(&key).cloned().flatten() } else { None }.or_else(|| self.sys_pic(sys));
                // icon sits at the near edge of the Name column in reading order: the column's left in LTR,
                // its right in RTL (the column itself is already mirrored there by `cols()`/`col_positions`).
                let icon_x = if self.rtl { cols[0].1 - self.px(18) } else { cols[0].0 };
                if let Some((bmp, w, h)) = pic {
                    let bs = self.px(18);
                    let (x, y) = (icon_x + (bs - w) / 2, bx.top + (bx.bottom - bx.top - h) / 2);
                    self.cl.bitmap(&bmp, &RECT { left: x, top: y, right: x + w, bottom: y + h }, fade);
                    if let Some((ov, r)) = arrow.then(|| self.link_badge(x, y, w, h)).flatten() {
                        self.cl.bitmap(&ov, &r, fade);
                    }
                }
                if let Some(t) = &texts {
                    let name_x = if self.rtl { cols[0].0 } else { cols[0].0 + self.px(26) };
                    let x = [name_x, cols[1].0, cols[2].0, cols[3].0, cols[4].0];
                    for (k, t) in t.iter().enumerate().filter(|&(k, _)| k == 0 || cols[k].1 > cols[k].0) {
                        self.cl.text_a(t, x[k], ty, if k == 0 { color } else { theme().muted }, fade);
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
        if self.matches > 0 {
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
                let again = !ctrl && !shift && self.sel.count() == 1 && self.sel.has(i) && inside(&self.name_box(i), x, y);
                if ctrl || shift {
                    self.click(i, ctrl, shift);
                } else if !self.sel.has(i) {
                    self.set_sel(Sel::one(i), Some(i), Some(Some(i)));
                } else {
                    defer = true; // part of a multi-selection: keep it for dragging, collapse on a plain click
                }
                self.press = Some(Press::Item { i, defer, x, y, again });
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
            // as in Explorer: a click on the selected item's name renames it, once a double-click is ruled out
            Some(Press::Item { again: true, .. }) => unsafe {
                SetTimer(self.main, T_RENAME, GetDoubleClickTime(), None);
            },
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
/// One open menu: the first, or a submenu opened from the entry `from` of the one before it.
struct Level {
    hwnd: HWND,
    rect: RECT, // on screen
    items: Vec<MItem>,
    rows: Vec<RECT>,
    hover: Option<usize>,
    from: usize,
    canvas: Canvas,
    icons: Option<Vec<Option<Bitmap>>>, // made on the first paint, on its canvas
    gutter: i32,                        // room at the left for checkmarks and icons
    list: Option<(RECT, i32)>,          // a searchable menu's: the area its rows scroll in (under the search box), and their height
    first: usize,                       // … and the first row shown there
}
struct Menu {
    levels: Vec<Level>,
    done: Option<Option<usize>>,
    aim: Option<(usize, usize)>, // the entry the mouse came to: after a pause its submenu opens, or others close
    gfx: Gfx,
    scale: f64,
    owner: HWND,
    find: Option<(Field, Vec<MItem>)>, // a searchable menu's search box, and all its entries
}

/// A one-line text box drawn by hand (a searchable menu's): its text, the caret, and where a selection started
/// (byte offsets); edited with Windows' keys.
#[derive(Default)]
struct Field {
    q: String,
    caret: usize,
    anchor: usize,
}

/// What a key did to a [`Field`] that reaches outside it.
#[derive(PartialEq, Debug)]
enum Clip {
    Copy(String),
    Paste,
}

impl Field {
    fn sel(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }
    fn cut(&mut self) -> String {
        let (a, b) = self.sel();
        let s = self.q[a..b].to_string();
        self.q.replace_range(a..b, "");
        (self.caret, self.anchor) = (a, a);
        s
    }
    fn insert(&mut self, s: &str) {
        self.cut();
        self.q.insert_str(self.caret, s);
        self.caret += s.len();
        self.anchor = self.caret;
    }
    /// The caret's place one character, or one word (Ctrl), to the left of `i`: a word's start, as Windows'.
    fn back(&self, i: usize, word: bool) -> usize {
        let mut it = self.q[..i].char_indices().rev().peekable();
        if !word {
            return it.next().map_or(0, |c| c.0);
        }
        while it.next_if(|c| c.1.is_whitespace()).is_some() {}
        let alnum = it.peek().is_some_and(|c| c.1.is_alphanumeric());
        let mut at = it.peek().map_or(0, |c| c.0);
        while let Some(c) = it.next_if(|c| !c.1.is_whitespace() && (c.1.is_alphanumeric() == alnum)) {
            at = c.0;
        }
        at
    }
    /// … to the right: the next word's start.
    fn fwd(&self, i: usize, word: bool) -> usize {
        let mut it = self.q[i..].char_indices().map(|(k, c)| (k + i, c)).peekable();
        if !word {
            return it.nth(1).map_or(self.q.len(), |c| c.0);
        }
        let alnum = it.peek().is_some_and(|c| c.1.is_alphanumeric());
        while it.next_if(|c| !c.1.is_whitespace() && (c.1.is_alphanumeric() == alnum)).is_some() {}
        while it.next_if(|c| c.1.is_whitespace()).is_some() {}
        it.peek().map_or(self.q.len(), |c| c.0)
    }
    /// Key `vk` with Ctrl and Shift as held: true when it was the field's (the caller then redraws, and copies or
    /// pastes what `clip` says).
    fn key(&mut self, vk: u16, ctrl: bool, shift: bool, clip: &mut Option<Clip>) -> bool {
        let to = |f: &mut Field, at: usize| {
            f.caret = at;
            if !shift {
                f.anchor = at;
            }
        };
        let (a, b) = self.sel();
        match vk {
            VK_LEFT if !shift && !ctrl && a != b => to(self, a),
            VK_RIGHT if !shift && !ctrl && a != b => to(self, b),
            VK_LEFT => to(self, self.back(self.caret, ctrl)),
            VK_RIGHT => to(self, self.fwd(self.caret, ctrl)),
            VK_HOME => to(self, 0),
            VK_END => to(self, self.q.len()),
            VK_BACK | VK_DELETE if a != b => {
                let s = self.cut();
                if vk == VK_DELETE && shift {
                    *clip = Some(Clip::Copy(s)); // Shift+Delete: cut
                }
            }
            VK_BACK => {
                self.anchor = self.back(self.caret, ctrl);
                self.cut();
            }
            VK_DELETE => {
                self.anchor = self.fwd(self.caret, ctrl);
                self.cut();
            }
            0x41 if ctrl => (self.anchor, self.caret) = (0, self.q.len()), // Ctrl+A
            0x43 if ctrl && a != b => *clip = Some(Clip::Copy(self.q[a..b].to_string())), // Ctrl+C
            VK_INSERT if ctrl && a != b => *clip = Some(Clip::Copy(self.q[a..b].to_string())),
            0x58 if ctrl && a != b => *clip = Some(Clip::Copy(self.cut())), // Ctrl+X
            0x56 if ctrl => *clip = Some(Clip::Paste), // Ctrl+V
            VK_INSERT if shift => *clip = Some(Clip::Paste),
            0x41 | 0x43 | 0x58 | VK_INSERT if ctrl || shift => {}
            _ => return false,
        }
        true
    }
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

/// [`cascade`] of plain entries (label, shortcut, checked); returns the chosen one's index.
#[allow(clippy::too_many_arguments)]
unsafe fn popup(owner: HWND, x: i32, y: i32, above: i32, min_w: i32, checks: bool, items: Vec<(String, String, bool)>, find: bool) -> Option<usize> {
    let items = items.into_iter().enumerate().map(|(k, (label, keys, on))| MItem { label, keys, on, tag: k, ..Default::default() }).collect();
    cascade(owner, x, y, above, min_w, checks, items, find)
}

/// Show a menu at screen point (x, y), at least `min_w` wide, and run it until it is closed. Returns the tag of
/// the chosen entry. Where it doesn't fit below, it ends at `above` instead (the top of the button that opened it).
/// Keys and clicks elsewhere are the menu's while it is open, like a Win32 menu; a submenu opens after a pause
/// over its entry, on a click, or with →. With `find`, typing narrows it to the entries whose label or shortcut
/// text has what was typed, in a search box above a scrolling list (no submenus then).
#[allow(clippy::too_many_arguments)]
unsafe fn cascade(owner: HWND, x: i32, y: i32, above: i32, min_w: i32, checks: bool, items: Vec<MItem>, find: bool) -> Option<usize> {
    let scale = GetDpiForWindow(owner) as f64 / 96.0;
    let gfx = Gfx::new(scale, crate::i18n::rtl()).ok()?;
    let hover = items.iter().position(|i| i.on);
    let find = find.then(|| (Field::default(), items.clone()));
    let mut m = Menu { levels: Vec::new(), done: None, aim: None, gfx, scale, owner, find };
    let h0 = open_level(&mut m, items, (x, y, above), min_w, checks, None);
    if h0.is_null() {
        return None;
    }
    m.levels[0].hover = hover;
    follow(&mut m.levels[0]);
    POPUP.with(|p| *p.borrow_mut() = Some(m));
    ShowWindow(h0, SW_SHOWNOACTIVATE);
    UpdateWindow(h0);
    SetCapture(h0); // every mouse message comes here, at a screen point: handled below, for whichever level is there
    let root = GetAncestor(owner, GA_ROOT);
    let mut msg: MSG = zeroed();
    while POPUP.with(|m| m.borrow().as_ref().is_some_and(|m| m.done.is_none())) {
        if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
            PostQuitMessage(0);
            break;
        }
        match msg.message {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                TranslateMessage(&msg); // the typed character, for the search box
                menu_key(msg.wParam as u16);
                continue;
            }
            WM_CHAR => {
                menu_char(msg.wParam as u32);
                continue;
            }
            WM_SYSCHAR | WM_KEYUP | WM_SYSKEYUP => continue,
            WM_MOUSEMOVE => {
                menu_move(msg.pt);
                continue;
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                menu_up(msg.pt);
                continue;
            }
            WM_MOUSEWHEEL if menu_at(msg.pt).is_some() => {
                // a notch is 3 rows; a touchpad's smaller steps a row each
                let d = (msg.wParam >> 16) as u16 as i16 as isize;
                menu_scroll(if d.abs() >= 120 { -d / 40 } else { -d.signum() });
                continue;
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN | WM_MOUSEWHEEL => {
                if menu_at(msg.pt).is_none() {
                    finish(None); // a click outside closes the menu and goes nowhere else
                }
                continue;
            }
            WM_TIMER if msg.hwnd == h0 && msg.wParam == 1 => {
                KillTimer(h0, 1);
                settle();
                continue;
            }
            _ => {}
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
        if GetCapture() != h0 || GetForegroundWindow() != root {
            finish(None);
        }
    }
    ReleaseCapture();
    let m = POPUP.with(|m| m.borrow_mut().take())?;
    for l in &m.levels {
        DestroyWindow(l.hwnd);
    }
    m.done.flatten()
}

/// Lay out `items` and make their (hidden) window: at (x, y, above) as [`cascade`] says, or for a submenu beside
/// its entry (`beside`: that row on screen, and its index), on the left where the right has no room.
unsafe fn open_level(m: &mut Menu, items: Vec<MItem>, (x, y, above): (i32, i32, i32), min_w: i32, checks: bool, beside: Option<(RECT, usize)>) -> HWND {
    let (g, s) = (&m.gfx, m.scale);
    let px = |v: i32| (v as f64 * s).round() as i32;
    let lh = g.f.lh;
    let label_w = items.iter().map(|i| g.width(&i.label, if i.bold { &g.f.semi } else { &g.f.reg })).max().unwrap_or(0);
    let key_w = items.iter().map(|i| g.width(&i.keys, &g.f.reg)).max().unwrap_or(0);
    let arrow = if items.iter().any(|i| !i.sub.is_empty()) { px(18) } else { 0 };
    // padding inside the 1px border; rows as tall as Windows' own menus' (22 px at 100%, 9 for a line)
    let (pad, top) = (px(4) + 1, px(2) + 1);
    let gutter = if checks || items.iter().any(|i| i.on || i.icon.is_some()) { px(24) } else { 0 };
    let wd = min_w.max(px(210)).max(2 * pad + px(20) + gutter + label_w + arrow + if key_w > 0 { px(28) + key_w } else { 0 });
    let rh = px(22).max(lh + px(2));
    // a searchable menu: its search box, then 14 rows at a time (`place` lays them out)
    let head = if beside.is_none() && m.find.is_some() { px(34) } else { 0 };
    let mut rows = Vec::new();
    let mut yy = top + head;
    for it in &items {
        let h = if it.label.is_empty() { px(9) } else { rh };
        rows.push(RECT { left: pad, top: yy, right: wd - pad, bottom: yy + h });
        yy += h;
    }
    // ponytail: only a searchable menu scrolls; any other taller than the screen is cut off (none of Explorer's are)
    let list = (head > 0).then(|| (RECT { left: pad, top: top + head, right: wd - pad, bottom: top + head + 14 * rh }, rh));
    let ht = list.map_or(yy, |l| l.0.bottom) + top;
    let at = beside.map_or(POINT { x, y }, |(r, _)| POINT { x: r.right, y: r.top });
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST), &mut mi);
    let work = mi.rcWork;
    let (x, y) = match beside {
        None => {
            let x = x.min(work.right - wd - px(4)).max(work.left + px(4));
            let y = if y + ht > work.bottom - px(4) { above - ht } else { y };
            (x, y.min(work.bottom - ht - px(4)).max(work.top + px(4)))
        }
        Some((r, _)) => {
            let x = if r.right + pad + wd <= work.right { r.right + pad } else { r.left - pad - wd };
            (x.max(work.left), (r.top - top).min(work.bottom - ht).max(work.top))
        }
    };

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
    let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, class.as_ptr(), null(), WS_POPUP, x, y, wd, ht, m.owner, null_mut(), GetModuleHandleW(null()), null());
    if hwnd.is_null() {
        return hwnd;
    }
    let dark: BOOL = 1;
    DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const c_void, 4); // dark
    // rounded by its own shape, not the compositor's corner preference: that one isn't always applied to popups
    // (the outline is painted along the edge, see `paint_menu`)
    let d = 2 * menu_radius(s);
    SetWindowRgn(hwnd, CreateRoundRectRgn(0, 0, wd + 1, ht + 1, d, d), 0);
    let canvas = Canvas::new(hwnd, &m.gfx);
    let rect = RECT { left: x, top: y, right: x + wd, bottom: y + ht };
    m.levels.push(Level { hwnd, rect, items, rows, hover: None, from: beside.map_or(0, |b| b.1), canvas, icons: None, gutter, list, first: 0 });
    hwnd
}

/// A searchable level's rows: those from `first` that fit its list area; the others get no room (so they are
/// neither drawn nor hit).
fn place(l: &mut Level) {
    let Some((a, rh)) = l.list else { return };
    let cap = ((a.bottom - a.top) / rh) as usize;
    l.first = l.first.min(l.items.len().saturating_sub(cap));
    let first = l.first;
    l.rows = (0..l.items.len())
        .map(|i| match i.checked_sub(first).filter(|&k| k < cap) {
            Some(k) => RECT { top: a.top + k as i32 * rh, bottom: a.top + (k as i32 + 1) * rh, ..a },
            None => RECT { left: 0, top: 0, right: 0, bottom: 0 },
        })
        .collect();
}

/// Scroll a searchable level so its lit entry is in view.
fn follow(l: &mut Level) {
    let Some((a, rh)) = l.list else { return };
    let cap = ((a.bottom - a.top) / rh) as usize;
    if let Some(h) = l.hover {
        l.first = l.first.min(h).max((h + 1).saturating_sub(cap));
    }
    place(l);
}

/// The wheel over a searchable menu: `d` rows on.
unsafe fn menu_scroll(d: isize) {
    let h = POPUP.with(|p| {
        let mut g = p.borrow_mut();
        let l = g.as_mut()?.levels.first_mut().filter(|l| l.list.is_some())?;
        l.first = l.first.saturating_add_signed(d);
        place(l);
        Some(l.hwnd)
    });
    if let Some(h) = h {
        InvalidateRect(h, null(), 0);
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        POPUP.with(|p| {
            if let Some(m) = p.borrow_mut().as_mut() {
                m.aim = None;
            }
        });
        menu_move(pt); // light what is under the mouse now
    }
}

/// A typed character: into a searchable menu's search box.
unsafe fn menu_char(c: u32) {
    let Some(c) = char::from_u32(c).filter(|c| !c.is_control()) else { return };
    search(|f| f.insert(c.encode_utf8(&mut [0; 4])));
}

/// Change a searchable menu's search text, and show only the entries that match it, the first one lit.
unsafe fn search(edit: impl FnOnce(&mut Field)) {
    let h = POPUP.with(|p| {
        let mut g = p.borrow_mut();
        let m = g.as_mut()?;
        let (f, all) = m.find.as_mut()?;
        let was = f.q.clone();
        edit(f);
        let l = &mut m.levels[0];
        if f.q == was {
            return Some(l.hwnd); // the caret moved, or a selection: the same list
        }
        let q = f.q.to_lowercase();
        l.items = all.iter().filter(|i| i.label.to_lowercase().contains(&q) || i.keys.to_lowercase().contains(&q)).cloned().collect();
        l.icons = None;
        l.first = 0;
        l.hover = (!l.items.is_empty()).then_some(0);
        m.aim = None;
        place(l);
        Some(l.hwnd)
    });
    if let Some(h) = h {
        InvalidateRect(h, null(), 0);
    }
}

/// The deepest open level at screen point `pt`, and the entry there (not a separator).
fn menu_at(pt: POINT) -> Option<(usize, Option<usize>)> {
    POPUP.with(|p| {
        let g = p.borrow();
        let m = g.as_ref()?;
        let k = m.levels.iter().rposition(|l| inside(&l.rect, pt.x, pt.y))?;
        let l = &m.levels[k];
        let (x, y) = (pt.x - l.rect.left, pt.y - l.rect.top);
        Some((k, l.rows.iter().position(|r| inside(r, x, y)).filter(|&i| !l.items[i].label.is_empty())))
    })
}

unsafe fn menu_move(pt: POINT) {
    // over another menu-bar button: close this menu, the bar opens that one (see `menu_bar_popup`)
    if let Some(i) = BAR.with(|b| b.borrow().iter().find(|(r, _)| inside(r, pt.x, pt.y)).map(|b| b.1)) {
        SWITCH.with(|s| s.set(Some(i)));
        finish(None);
        return;
    }
    let Some((k, Some(i))) = menu_at(pt) else { return };
    let moved = POPUP.with(|p| {
        let mut g = p.borrow_mut();
        let m = g.as_mut().filter(|m| m.aim != Some((k, i)))?;
        m.aim = Some((k, i));
        m.levels[k].hover = Some(i);
        Some((m.levels[k].hwnd, m.levels[0].hwnd))
    });
    if let Some((h, h0)) = moved {
        InvalidateRect(h, null(), 0);
        SetTimer(h0, 1, 250, None); // then `settle`
    }
}

unsafe fn menu_up(pt: POINT) {
    let Some((k, Some(i))) = menu_at(pt) else { return };
    let open = POPUP.with(|p| {
        let mut g = p.borrow_mut();
        let m = g.as_mut()?;
        let it = &m.levels[k].items[i];
        let (sub, off, tag) = (!it.sub.is_empty(), it.off, it.tag);
        if sub {
            m.aim = Some((k, i));
        } else if !off {
            m.done = Some(Some(tag));
        }
        Some(sub)
    });
    if open == Some(true) {
        settle(); // a click opens a submenu straight away
    }
}

/// Where the mouse came to rest: close the submenus that aren't that entry's, and open its own.
unsafe fn settle() {
    let mut open = None;
    let gone: Vec<HWND> = POPUP.with(|p| {
        let mut g = p.borrow_mut();
        let Some(m) = g.as_mut() else { return Vec::new() };
        let Some((k, i)) = m.aim.filter(|a| a.0 < m.levels.len()) else { return Vec::new() };
        let keep = if m.levels.get(k + 1).is_some_and(|l| l.from == i) { k + 2 } else { k + 1 };
        let gone = m.levels.drain(keep..).map(|l| l.hwnd).collect();
        let it = &m.levels[k].items[i];
        if keep == k + 1 && !it.sub.is_empty() && !it.off {
            open = Some((k, i));
        }
        gone
    });
    for h in gone {
        DestroyWindow(h);
    }
    if let Some((k, i)) = open {
        open_sub(k, i, false);
    }
}

/// The submenu of entry `i` of level `k` (the deepest one), its first entry lit when opened from the keyboard.
unsafe fn open_sub(k: usize, i: usize, keys: bool) {
    let h = POPUP.with(|p| {
        let mut g = p.borrow_mut();
        let m = g.as_mut()?;
        let l = &m.levels[k];
        let r = l.rows[i];
        let row = RECT { left: l.rect.left + r.left, top: l.rect.top + r.top, right: l.rect.left + r.right, bottom: l.rect.top + r.bottom };
        let items = l.items[i].sub.clone();
        let h = open_level(m, items, (0, 0, 0), 0, false, Some((row, i)));
        if keys && !h.is_null() {
            let l = m.levels.last_mut()?;
            l.hover = l.items.iter().position(|it| !it.label.is_empty());
        }
        Some(h).filter(|h| !h.is_null())
    });
    if let Some(h) = h {
        ShowWindow(h, SW_SHOWNOACTIVATE);
    }
}

unsafe fn menu_key(vk: u16) {
    enum Then {
        Stay,
        Open(usize, usize),
        Close,
    }
    // a searchable menu: Space types, Backspace erases, Esc clears what was typed before it closes the menu
    // … and a text box's keys: the caret, selecting, Ctrl+Backspace a word, Ctrl+A/C/X/V
    match (POPUP.with(|p| p.borrow().as_ref().and_then(|m| m.find.as_ref().map(|f| f.0.q.is_empty()))), vk) {
        (Some(_), VK_SPACE) => return,
        (Some(false), VK_ESCAPE) => return search(|f| *f = Field::default()),
        (Some(_), _) => {
            let (mut clip, mut mine) = (None, false);
            search(|f| mine = f.key(vk, key_down(VK_CONTROL), key_down(VK_SHIFT), &mut clip));
            match clip {
                Some(Clip::Copy(s)) => _ = clip_text(GetActiveWindow(), &s),
                Some(Clip::Paste) => {
                    let s = clip_read().unwrap_or_default();
                    let line = s.lines().next().unwrap_or("").to_string();
                    search(|f| f.insert(&line));
                }
                None => {}
            }
            if mine {
                return;
            }
        }
        _ => {}
    }
    let then = POPUP.with(|p| {
        let mut g = p.borrow_mut();
        let Some(m) = g.as_mut() else { return Then::Stay };
        m.aim = None; // the keys lead now, not where the mouse last was
        let depth = m.levels.len();
        let l = &mut m.levels[depth - 1];
        let n = l.items.len() as isize;
        let items = &l.items;
        let step = |from: isize, d: isize| -> Option<usize> { (1..=n).map(|k| (from + d * k).rem_euclid(n) as usize).find(|&i| !items[i].label.is_empty()) };
        match vk {
            VK_DOWN => l.hover = step(l.hover.map_or(-1, |h| h as isize), 1),
            VK_UP => l.hover = step(l.hover.map_or(n, |h| h as isize), -1),
            VK_HOME => l.hover = step(-1, 1),
            VK_END => l.hover = step(n, -1),
            VK_RIGHT | VK_RETURN | VK_SPACE => {
                if let Some(it) = l.hover.map(|i| &l.items[i]) {
                    if !it.sub.is_empty() && !it.off {
                        return Then::Open(depth - 1, l.hover.unwrap_or(0));
                    }
                    if vk != VK_RIGHT && it.sub.is_empty() && !it.off {
                        m.done = Some(Some(it.tag));
                    }
                }
            }
            VK_LEFT | VK_ESCAPE if depth > 1 => return Then::Close,
            VK_ESCAPE | VK_MENU | VK_F10 | VK_TAB => m.done = Some(None),
            _ => {}
        }
        follow(&mut m.levels[depth - 1]);
        Then::Stay
    });
    match then {
        Then::Open(k, i) => open_sub(k, i, true),
        Then::Close => {
            if let Some(h) = POPUP.with(|p| p.borrow_mut().as_mut().and_then(|m| m.levels.pop()).map(|l| l.hwnd)) {
                DestroyWindow(h);
            }
        }
        Then::Stay => {}
    }
    for h in POPUP.with(|p| p.borrow().as_ref().map_or(Vec::new(), |m| m.levels.iter().map(|l| l.hwnd).collect::<Vec<_>>())) {
        InvalidateRect(h, null(), 0);
    }
}

unsafe extern "system" fn menu_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            let painted = POPUP.with(|p| {
                let Ok(mut g) = p.try_borrow_mut() else { return false };
                if let Some(m) = g.as_mut() {
                    paint_menu(m, h);
                }
                true
            });
            EndPaint(h, &ps);
            if !painted {
                InvalidateRect(h, null(), 0); // asked mid-change: again once that's done
            }
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

/// The corner radius of a menu, at `scale`.
fn menu_radius(scale: f64) -> i32 {
    (6.0 * scale).round() as i32
}

/// `a` moved a fraction `t` of the way to `b` (0xRRGGBB).
fn mix(a: u32, b: u32, t: f32) -> u32 {
    (0..3).map(|k| k * 8).fold(0, |c, sh| {
        let (x, y) = ((a >> sh & 0xff) as f32, (b >> sh & 0xff) as f32);
        c | ((x + (y - x) * t).round() as u32) << sh
    })
}

fn paint_menu(m: &mut Menu, h: HWND) {
    let (g, s) = (&m.gfx, m.scale);
    let Some(l) = m.levels.iter_mut().find(|l| l.hwnd == h) else { return };
    let px = |v: i32| (v as f64 * s).round() as i32;
    if l.icons.is_none() {
        let (items, canvas) = (&l.items, &mut l.canvas);
        l.icons = Some(items.iter().map(|it| it.icon.as_ref().and_then(|(w, h, p)| canvas.make_bitmap(p, *w, *h))).collect());
    }
    if !l.canvas.begin() {
        return;
    }
    let cv = &l.canvas;
    cv.clear(theme().card);
    let edge = RECT { left: 0, top: 0, right: l.rect.right - l.rect.left, bottom: l.rect.bottom - l.rect.top };
    cv.rrect(&edge, menu_radius(s) as f32, None, Some(mix(theme().line, theme().muted, 0.35)));
    // a searchable menu's search box (only the language list's: hence its placeholder), with a caret after the text
    if let (Some((a, _)), Some((f, _))) = (l.list, m.find.as_ref()) {
        let b = RECT { left: a.left, top: a.top - px(32), right: a.right, bottom: a.top - px(6) };
        cv.rrect(&b, (4.0 * s) as f32, Some(theme().bg), Some(mix(theme().line, theme().accent, 0.5)));
        let (x, ty) = (b.left + px(8), b.top + (b.bottom - b.top - g.f.lh) / 2);
        let at = |i: usize| x + g.width(&f.q[..i], &g.f.reg);
        let (sa, sb) = f.sel();
        if sa != sb {
            cv.fill(&RECT { left: at(sa), top: ty, right: at(sb), bottom: ty + g.f.lh }, theme().accent, 0.35);
        }
        if f.q.is_empty() {
            put(g, cv, t("lang.search"), &g.f.reg, x + px(2), ty, theme().muted);
        } else {
            put(g, cv, &f.q, &g.f.reg, x, ty, theme().text);
        }
        let w = at(f.caret) - x;
        cv.fill(&RECT { left: x + w, top: ty, right: x + w + 1.max(px(1)), bottom: ty + g.f.lh }, theme().accent, 1.0);
    }
    let sz = px(16);
    for (k, (it, r)) in l.items.iter().zip(&l.rows).enumerate() {
        if r.right == r.left {
            continue; // scrolled out of a searchable menu's list
        }
        if it.label.is_empty() {
            cv.fill(&RECT { left: r.left + px(6), top: r.top + px(4), right: r.right - px(6), bottom: r.top + px(4) + 1 }, theme().line, 1.0);
            continue;
        }
        let on = l.hover == Some(k);
        if on {
            cv.rrect(r, (3.0 * s) as f32, Some(theme().hl), Some(mix(theme().hl, theme().accent, 0.35)));
        }
        let ty = r.top + (r.bottom - r.top - g.f.lh) / 2;
        if let Some(Some(b)) = l.icons.as_ref().and_then(|v| v.get(k)) {
            let top = r.top + (r.bottom - r.top - sz) / 2;
            cv.bitmap(b, &RECT { left: r.left + px(6), top, right: r.left + px(6) + sz, bottom: top + sz }, if it.off { 0.4 } else { 1.0 });
        } else if it.on {
            put(g, cv, "✓", &g.f.reg, r.left + px(8), ty, theme().accent); // in its own column: never over a shortcut
        }
        let color = if it.off { theme().muted } else { theme().text };
        put(g, cv, &it.label, if it.bold { &g.f.semi } else { &g.f.reg }, r.left + px(10) + l.gutter, ty, color);
        let mut right = r.right - px(10);
        if !it.sub.is_empty() {
            put(g, cv, "›", &g.f.reg, right - g.width("›", &g.f.reg), ty, if it.off { theme().muted } else { theme().text });
            right -= px(18);
        }
        if !it.keys.is_empty() {
            put(g, cv, &it.keys, &g.f.reg, right - g.width(&it.keys, &g.f.reg), ty, theme().muted);
        }
    }
    l.canvas.end();
}

// ---- actions (outside `with`: they show dialogs, menus, or run a drag loop) ------------------------------------
fn plural(n: usize) -> String {
    t_plural("item", n as i64)
}

unsafe fn confirm(owner: HWND, text: &str) -> bool {
    MessageBoxW(owner, wz(text).as_ptr(), wz("Mosaic").as_ptr(), MB_OKCANCEL | MB_ICONQUESTION) == IDOK
}

thread_local! {
    /// The rename field over an item, and the path it renames.
    static RENAMING: RefCell<Option<(HWND, String)>> = const { RefCell::new(None) };
    /// An item to select once the list shows it (a rename's new name, a new folder), whether to rename it then, and
    /// until when to keep looking.
    static LISTED: RefCell<Option<(String, bool, Instant)>> = const { RefCell::new(None) };
    /// What Ctrl+Z takes back, newest last.
    static UNDO: RefCell<Vec<Undo>> = const { RefCell::new(Vec::new()) };
    /// The text field open now (one at a time).
    static FIELD: std::cell::Cell<Fld> = const { std::cell::Cell::new(Fld { e: null_mut(), look: Look::Box, pad: 0, padv: 0, min_w: 0, th: 0, file: false }) };
}

/// What Windows refuses in a file's name.
const BAD_NAME: &str = "\\/:*?\"<>|";

/// How a text field looks.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Look {
    /// A box of its own: padded, in a rounded accent border, at least 150 px wide.
    Box,
    /// Right over a name drawn in `r` (a list row's): the same font, the text where it was, a thin square border
    /// hugging it, as Explorer's.
    Name,
    /// The same for a name drawn on up to two centred lines (a grid tile's): it wraps, and grows down as it's typed into.
    Wrap,
    /// No box at all: the text where it was drawn, in this font, on this background (a tab's title).
    Bare(HFONT, u32),
}

#[derive(Clone, Copy)]
struct Fld {
    e: HWND,
    look: Look,
    pad: i32,  // across: the frame each side of the text
    padv: i32, // down: above and below it
    min_w: i32,
    th: i32, // a line's height
    file: bool,
}

/// A text field over `r` (`parent`'s coordinates) for naming anything, widening as it's typed into (see [`Look`]).
/// `text` starts with its first `sel` UTF-16 units selected (-1: all), `hint` shows while it's empty, and `file`
/// refuses what a file name can't have. Ctrl+A selects all, Ctrl+Backspace deletes a word.
pub(super) unsafe fn field(parent: HWND, r: RECT, text: &str, sel: isize, hint: &str, file: bool, look: Look) -> HWND {
    let Some((font, scale)) = with(|u| (u.edit_font, u.scale)) else { return null_mut() };
    let font = if let Look::Bare(f, _) = look { f } else { font };
    let p = |v: f64| (v * scale).round() as i32;
    let dc = GetDC(parent);
    let old = SelectObject(dc, font);
    let mut tm: TEXTMETRICW = zeroed();
    GetTextMetricsW(dc, &mut tm);
    SelectObject(dc, old);
    ReleaseDC(parent, dc);
    let th = tm.tmHeight;
    let (rw, rh) = (r.right - r.left, r.bottom - r.top);
    let fld = |pad, padv, min_w| Fld { e: null_mut(), look, pad, padv, min_w, th, file };
    let (style, f, x, y, h) = match look {
        Look::Box => {
            let (pad, h) = (p(8.0), (th + p(10.0)).max(rh));
            (ES_AUTOHSCROLL, fld(pad, 0, p(150.0)), r.left - pad, (r.top + r.bottom - h) / 2, h)
        }
        Look::Name => {
            let (pad, padv) = (p(4.0), p(2.0));
            (ES_AUTOHSCROLL, fld(pad, padv, p(28.0)), r.left - pad, r.top - padv, rh + 2 * padv)
        }
        Look::Wrap => {
            let (pad, padv) = (p(4.0), p(2.0));
            (ES_MULTILINE | ES_CENTER | ES_AUTOVSCROLL, fld(pad, padv, rw + 2 * pad), r.left - pad, r.top - padv, th + 2 * padv)
        }
        Look::Bare(..) => (ES_AUTOHSCROLL, fld(0, 0, rw), r.left, r.top, rh),
    };
    FIELD.set(f); // before the window: its first WM_NCCALCSIZE reads it
    let e = CreateWindowExW(0, wz("EDIT").as_ptr(), null(), WS_CHILD | style as u32, x.max(0), y, f.min_w, h, parent, null_mut(), GetModuleHandleW(null()), null());
    if e.is_null() {
        return e;
    }
    FIELD.set(Fld { e, ..f });
    windows_sys::Win32::UI::Shell::SetWindowSubclass(e, Some(field_proc), 1, 0);
    SetWindowTheme(e, wz("DarkMode_CFD").as_ptr(), null());
    SendMessageW(e, WM_SETFONT, font as usize, 0);
    if look != Look::Box {
        SendMessageW(e, 0x00D3, 3, 0); // EM_SETMARGINS, none: the text lands exactly where it was drawn
    }
    SetWindowTextW(e, wz(text).as_ptr());
    let hint = wz(hint);
    SendMessageW(e, 0x1501, 1, hint.as_ptr() as isize); // EM_SETCUEBANNER, shown while focused too
    SendMessageW(e, 0x00B1, 0, sel); // EM_SETSEL
    fit(e, true);
    ShowWindow(e, SW_SHOW);
    SetFocus(e);
    e
}

/// The background field `e` is drawn on (its parent's WM_CTLCOLOREDIT asks).
unsafe fn field_bg(e: HWND) -> u32 {
    match FIELD.get() {
        Fld { e: f, look: Look::Bare(_, bg), .. } if f == e => bg,
        _ => theme().bg,
    }
}

/// Widens (or narrows back) field `e` to its text, within its parent, and rounds a box's corners. A wrapping one keeps
/// its width and grows down instead, up to five lines.
unsafe fn fit(e: HWND, force: bool) {
    let Fld { pad, padv, min_w, th, look, .. } = FIELD.get();
    let (mut wr, mut pr): (RECT, RECT) = (zeroed(), zeroed());
    GetWindowRect(e, &mut wr);
    MapWindowPoints(null_mut(), GetParent(e), &mut wr as *mut RECT as *mut POINT, 2);
    GetClientRect(GetParent(e), &mut pr);
    let (w, h) = match look {
        Look::Bare(..) => (min_w, wr.bottom - wr.top),
        Look::Wrap => (min_w, SendMessageW(e, 0x00BA, 0, 0).clamp(1, 5) as i32 * th + 2 * padv), // EM_GETLINECOUNT
        _ => {
            let n = GetWindowTextLengthW(e);
            let mut b = vec![0u16; n as usize + 1];
            GetWindowTextW(e, b.as_mut_ptr(), n + 1);
            let dc = GetDC(e);
            let old = SelectObject(dc, SendMessageW(e, WM_GETFONT, 0, 0) as HGDIOBJ);
            let mut sz: SIZE = zeroed();
            GetTextExtentPoint32W(dc, b.as_ptr(), n, &mut sz);
            SelectObject(dc, old);
            ReleaseDC(e, dc);
            let room = if look == Look::Box { 4 * pad } else { 2 * pad + th / 3 }; // a name's box hugs it, the caret aside
            ((sz.cx + room).min(pr.right - wr.left - 2).max(min_w), wr.bottom - wr.top)
        }
    };
    if !force && w == wr.right - wr.left && h == wr.bottom - wr.top {
        return;
    }
    SetWindowPos(e, null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    if look == Look::Box {
        SetWindowRgn(e, CreateRoundRectRgn(0, 0, w + 1, h + 1, pad, pad), 1);
    }
    // scrolled while it was narrower: back to the start, then to the caret
    let (mut a, mut z) = (0u32, 0u32);
    SendMessageW(e, 0x00B0, &mut a as *mut u32 as usize, &mut z as *mut u32 as isize); // EM_GETSEL
    SendMessageW(e, 0x00B1, 0, 0);
    SendMessageW(e, 0x00B1, a as usize, z as isize);
}

/// Ctrl+Backspace in an edit box: the word before the caret (and the spaces after it), or the selection. A plain
/// EDIT control would type a box character instead.
unsafe fn delete_word(h: HWND) {
    let (mut a, mut z) = (0u32, 0u32);
    SendMessageW(h, 0x00B0, &mut a as *mut u32 as usize, &mut z as *mut u32 as isize);
    if a == z {
        let n = GetWindowTextLengthW(h);
        let mut b = vec![0u16; n as usize + 1];
        GetWindowTextW(h, b.as_mut_ptr(), n + 1);
        let s = String::from_utf16_lossy(&b[..a as usize]);
        let t = s.trim_end();
        let w = t.trim_end_matches(char::is_alphanumeric);
        let k = if w.len() < t.len() { w.len() } else { t.len() - t.chars().last().map_or(0, char::len_utf8) }; // a lone mark: just it
        SendMessageW(h, 0x00B1, s[..k].encode_utf16().count(), a as isize);
    }
    SendMessageW(h, 0x00C2, 1, wz("").as_ptr() as isize); // EM_REPLACESEL, undoable
}

unsafe extern "system" fn field_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM, _: usize, _: usize) -> LRESULT {
    let Fld { pad, padv, th, file, look, .. } = FIELD.get();
    match m {
        WM_NCCALCSIZE => {
            // the padding is the frame, so the text is centred and the caret never touches the border
            let r = &mut *(l as *mut RECT);
            let v = if look == Look::Wrap { padv } else { ((r.bottom - r.top - th) / 2).max(0) };
            *r = RECT { left: r.left + pad, top: r.top + v, right: (r.right - pad).max(r.left + pad), bottom: (r.bottom - v).max(r.top + v) };
            return 0;
        }
        WM_NCPAINT => {
            let dc = GetWindowDC(h);
            let (mut wr, mut cr) = (zeroed::<RECT>(), zeroed::<RECT>());
            GetWindowRect(h, &mut wr);
            GetClientRect(h, &mut cr);
            let mut o = POINT { x: 0, y: 0 };
            ClientToScreen(h, &mut o);
            let (ox, oy) = (o.x - wr.left, o.y - wr.top);
            ExcludeClipRect(dc, ox, oy, ox + cr.right, oy + cr.bottom);
            let (w, hh) = (wr.right - wr.left, wr.bottom - wr.top);
            let bg = CreateSolidBrush(gdi(field_bg(h)));
            FillRect(dc, &RECT { left: 0, top: 0, right: w, bottom: hh }, bg);
            DeleteObject(bg);
            if !matches!(look, Look::Bare(..)) {
                let (wd, rad) = if look == Look::Box { ((pad / 8).max(1), pad) } else { (1, 0) }; // a name's: square
                let pen = CreatePen(PS_INSIDEFRAME, wd, gdi(theme().accent));
                let (op, ob) = (SelectObject(dc, pen), SelectObject(dc, GetStockObject(NULL_BRUSH)));
                RoundRect(dc, 0, 0, w, hh, rad, rad);
                SelectObject(dc, op);
                SelectObject(dc, ob);
                DeleteObject(pen);
            }
            ReleaseDC(h, dc);
            return 0;
        }
        WM_CHAR if wp == 1 => {
            SendMessageW(h, 0x00B1, 0, -1); // Ctrl+A
            return 0;
        }
        WM_CHAR if wp == 0x7F => {
            delete_word(h);
            return 0;
        }
        WM_CHAR if look == Look::Wrap && matches!(wp, 9 | 10 | 13) => return 0, // a name is one line, however it wraps
        WM_CHAR if file && char::from_u32(wp as u32).is_some_and(|c| BAD_NAME.contains(c)) => {
            bad_name_tip(h);
            return 0;
        }
        WM_NCDESTROY => {
            windows_sys::Win32::UI::Shell::RemoveWindowSubclass(h, Some(field_proc), 1);
        }
        _ => {}
    }
    let r = windows_sys::Win32::UI::Shell::DefSubclassProc(h, m, wp, l);
    if file && m == WM_PASTE {
        // pasted: what a name can't have goes
        let n = GetWindowTextLengthW(h);
        let mut b = vec![0u16; n as usize + 1];
        GetWindowTextW(h, b.as_mut_ptr(), n + 1);
        let s = String::from_utf16_lossy(&b[..n as usize]);
        if s.contains(|c| BAD_NAME.contains(c) || c < ' ') {
            let clean: String = s.chars().filter(|&c| !BAD_NAME.contains(c) && c >= ' ').collect();
            SetWindowTextW(h, wz(&clean).as_ptr());
            SendMessageW(h, 0x00B1, usize::MAX >> 1, (usize::MAX >> 1) as isize);
            bad_name_tip(h);
        }
    }
    if matches!(m, WM_CHAR | WM_KEYDOWN | WM_PASTE | WM_CUT | WM_CLEAR | WM_UNDO) {
        fit(h, false);
    }
    r
}

/// The characters a file name can't have, in a tip under field `h`.
unsafe fn bad_name_tip(h: HWND) {
    use windows_sys::Win32::UI::Controls::EDITBALLOONTIP;
    let text = wz(&BAD_NAME.chars().map(String::from).collect::<Vec<_>>().join("  "));
    let title = wz("");
    let tip = EDITBALLOONTIP { cbStruct: size_of::<EDITBALLOONTIP>() as u32, pszTitle: title.as_ptr(), pszText: text.as_ptr(), ttiIcon: 0 };
    SendMessageW(h, 0x1503, 0, &tip as *const _ as isize); // EM_SHOWBALLOONTIP
}

/// A file operation done here, as Ctrl+Z reverses it. ponytail: no redo (Ctrl+Y); drag-and-drop moves aren't recorded.
enum Undo {
    Renamed(String, String),      // (now, was)
    Recycled(Vec<String>),        // where they were
    Made(Vec<String>),            // a new folder, a paste's copies: back to the Recycle Bin
    Moved(Vec<(String, String)>), // a paste's moves: (now, was)
}

fn did(u: Undo) {
    UNDO.with(|s| {
        let mut s = s.borrow_mut();
        s.push(u);
        if s.len() > 100 {
            s.remove(0);
        }
    });
}

/// Ctrl+Z: the newest operation reversed, as far as the files still allow (nothing put back over a newer file).
unsafe fn undo() {
    use crate::infrastructure::fileops;
    let gone = |p: &String| !std::path::Path::new(p).exists();
    let ok = match UNDO.with(|s| s.borrow_mut().pop()) {
        None => return with(|u| u.flash(t("status.nothing_undo").into())).unwrap_or(()),
        Some(Undo::Renamed(now, was)) => {
            preview::release(std::slice::from_ref(&now));
            !gone(&now) && (gone(&was) || now.to_lowercase() == was.to_lowercase()) && std::fs::rename(&now, &was).is_ok()
        }
        Some(Undo::Recycled(paths)) => fileops::restore(&paths) > 0,
        Some(Undo::Made(paths)) => {
            let there: Vec<String> = paths.into_iter().filter(|p| !gone(p)).collect();
            preview::release(&there);
            !there.is_empty() && fileops::delete_many(&there, true).is_ok()
        }
        Some(Undo::Moved(pairs)) => {
            let back: Vec<(String, String)> = pairs.into_iter().filter(|(now, was)| !gone(now) && gone(was)).collect();
            preview::release(&back.iter().map(|b| b.0.clone()).collect::<Vec<_>>());
            !back.is_empty() && fileops::move_many(&back).is_ok()
        }
    };
    with(|u| {
        u.flash(t(if ok { "status.undone" } else { "status.nothing_undo" }).into());
        u.relist_soon();
    });
}

/// Ctrl+V into `dir`: what the paste will make, noted for Ctrl+Z (checked again then, as the paste runs on its own
/// and may be cancelled). Names already there are left out: the shell asks about those, or makes "- Copy"s.
fn note_paste(dir: &str) {
    let Some((files, cut)) = shell::clipboard_get() else { return };
    if cut {
        preview::release(&files);
    }
    let dest = std::path::Path::new(dir);
    let pairs: Vec<(String, String)> = files.into_iter().filter_map(|f| {
        let to = dest.join(std::path::Path::new(&f).file_name()?);
        (!to.exists()).then(|| (to.to_string_lossy().into_owned(), f))
    }).collect();
    if pairs.is_empty() {
        return;
    }
    did(if cut { Undo::Moved(pairs) } else { Undo::Made(pairs.into_iter().map(|p| p.0).collect()) });
}

/// Explorer's menu at (x, y) for `paths` (with `bg`, the folder `paths[0]`'s empty space; none: only `extras`),
/// drawn as Mosaic's menus are, then what was picked: by Explorer, or by Mosaic for its own entries (`acts`).
unsafe fn explorer_menu(paths: Vec<String>, bg: bool, extras: Vec<MItem>, acts: Vec<Act>, x: i32, y: i32) {
    let Some(main) = with(|u| u.main) else { return };
    let shell = if paths.is_empty() { None } else { shell::menu_items(&paths, bg) };
    let had = shell.is_some();
    let pick = cascade(main, x, y, y, 0, false, merge(shell_entries(shell.unwrap_or_default()), extras, bg), false);
    if had {
        shell::menu_pick(pick.filter(|&k| k < EXTRA).map(|k| k as u32), x, y); // runs it, or lets the menu go
    }
    match pick {
        Some(RENAME) => start_rename(),
        Some(k) if k >= EXTRA => {
            if let Some(&a) = acts.get(k - EXTRA) {
                act(a);
            }
        }
        _ => {}
    }
}

/// New folder in `dir`: the first free "New folder", "New folder (2)", …, renamed in place once the list shows it,
/// as in Explorer.
unsafe fn new_folder(dir: &str) {
    let stem = t("new.folder");
    let made = (1..1000).find_map(|k| {
        let p = std::path::Path::new(dir).join(if k == 1 { stem.to_string() } else { format!("{stem} ({k})") });
        match std::fs::create_dir(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
            r => Some(r.map(|_| p)),
        }
    });
    match made {
        Some(Ok(p)) => {
            did(Undo::Made(vec![p.to_string_lossy().into_owned()]));
            LISTED.set(Some((p.to_string_lossy().into_owned(), true, Instant::now() + Duration::from_secs(10))));
            with(|u| u.relist_soon());
        }
        Some(Err(e)) => {
            with(|u| u.flash(t("status.failed").replace("{e}", &e.to_string())));
        }
        None => {}
    }
}

/// F2: a text field over the focused item's name, the name without its extension selected, as in Explorer.
/// Enter or a click elsewhere keeps it, Esc doesn't.
unsafe fn start_rename() {
    end_rename(true);
    let Some(Some((list, path, is_dir, r))) = with(|u| {
        let i = u.focus.filter(|&i| u.sel.has(i)).or_else(|| u.sel.iter().next())?;
        u.show_row(i);
        u.ensure(i / PG);
        let row = u.get(i)?;
        Some((u.list, row.path.clone(), row.is_dir, u.name_box(i)))
    }) else {
        return;
    };
    UpdateWindow(list);
    let name = std::path::Path::new(&path).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let stem = if is_dir { name.len() } else { name.rfind('.').filter(|&k| k > 0).unwrap_or(name.len()) };
    let look = if with(|u| u.grid) == Some(true) { Look::Wrap } else { Look::Name };
    let edit = field(list, r, &name, name[..stem].encode_utf16().count() as isize, "", true, look);
    if !edit.is_null() {
        RENAMING.set(Some((edit, path)));
        InvalidateRect(list, null(), 0); // its selection off
    }
}

unsafe fn end_rename(keep: bool) {
    let Some((edit, path)) = RENAMING.take() else { return };
    let n = GetWindowTextLengthW(edit);
    let mut b = vec![0u16; n as usize + 1];
    GetWindowTextW(edit, b.as_mut_ptr(), n + 1);
    let name = String::from_utf16_lossy(&b[..n as usize]).trim().to_string();
    if GetFocus() == edit {
        if let Some(list) = with(|u| u.list) {
            SetFocus(list);
        }
    }
    let list = GetParent(edit);
    DestroyWindow(edit);
    InvalidateRect(list, null(), 0); // its selection back
    let from = std::path::Path::new(&path);
    if !keep || name.is_empty() || from.file_name().is_some_and(|n| n.to_string_lossy() == name) {
        return;
    }
    let to = from.with_file_name(&name);
    preview::release(std::slice::from_ref(&path));
    let msg = match std::fs::rename(from, &to) {
        Ok(()) => {
            did(Undo::Renamed(to.to_string_lossy().into_owned(), path.clone()));
            LISTED.set(Some((to.to_string_lossy().into_owned(), false, Instant::now() + Duration::from_secs(10))));
            t("status.renamed").replace("{name}", &name)
        }
        Err(e) => t("status.failed").replace("{e}", &e.to_string()),
    };
    with(|u| {
        u.flash(msg);
        u.relist_soon();
    });
}

unsafe fn act(a: Act) {
    // the folder on screen, or the view: no selection needed
    match a {
        Act::Refresh => return with(|u| u.run(true)).unwrap_or(()),
        Act::SelectAll => return with(|u| u.select_all()).unwrap_or(()),
        Act::Undo => return undo(),
        Act::View(size) => return with(|u| u.reshape(size > 0, if size > 0 { size } else { u.size })).unwrap_or(()),
        Act::Sort(_) | Act::Desc(_) => {
            with(|u| {
                match a {
                    Act::Sort(k) => u.args.sort = k.into(),
                    _ => u.args.dir = if matches!(a, Act::Desc(true)) { "desc" } else { "asc" }.into(),
                }
                u.resort();
                u.invalidate();
            });
            return;
        }
        Act::Paste | Act::NewFolder | Act::HereProps => {
            let Some(Some(dir)) = with(|u| u.here()) else {
                with(|u| u.flash(t("status.no_folder").into()));
                return;
            };
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p); // where a paste's questions show
            match a {
                Act::Paste => {
                    note_paste(&dir);
                    shell::menu_verb(&dir, "paste", p.x, p.y)
                }
                Act::HereProps => shell::properties(dir),
                _ => new_folder(&dir),
            }
            return;
        }
        _ => {}
    }
    let Some(Some((paths, n, main))) = with(|u| (u.sel.count() > 0).then(|| (u.sel_paths(), u.sel.count(), u.main))) else { return };
    if paths.is_empty() {
        return;
    }
    let msg = match a {
        Act::OpenHere => return with(|u| u.go_dir(&paths[0])).unwrap_or(()),
        Act::OpenTab => return with(|u| u.open_tab(&paths[0])).unwrap_or(()),
        Act::Rename => return start_rename(),
        Act::SearchIn => {
            let dir = if std::fs::metadata(&paths[0]).is_ok_and(|m| m.is_dir()) { paths[0].clone() } else { std::path::Path::new(&paths[0]).parent().map_or(String::new(), |d| d.to_string_lossy().into_owned()) };
            with(|u| u.search_in(&dir));
            return;
        }
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
        Act::Del | Act::DelNow => {
            let recycle = matches!(a, Act::Del);
            if !confirm(main, &t(if recycle { "confirm.recycle" } else { "confirm.delete_now" }).replace("{n}", &plural(n))) {
                return;
            }
            preview::release(&paths);
            let _ = crate::infrastructure::fileops::delete_many(&paths, recycle);
            let gone: Vec<String> = paths.iter().filter(|p| !std::path::Path::new(p).exists()).cloned().collect();
            if recycle && !gone.is_empty() {
                did(Undo::Recycled(gone.clone()));
            }
            let gone = gone.len();
            with(|u| {
                u.relist_soon();
                u.set_sel(Sel::default(), None, Some(None));
            });
            if gone == 0 {
                t("status.nothing_deleted").into()
            } else if recycle {
                t("status.moved_recycle").replace("{n}", &plural(gone))
            } else {
                t("status.deleted").replace("{n}", &plural(gone))
            }
        }
        _ => return, // the ones above, with no selection needed
    };
    with(|u| u.flash(msg));
}

/// The clipboard's text, if it holds some.
unsafe fn clip_read() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    if OpenClipboard(null_mut()) == 0 {
        return None;
    }
    let h = GetClipboardData(13); // CF_UNICODETEXT
    let p = if h.is_null() { null_mut() } else { GlobalLock(h as _) as *const u16 };
    let s = (!p.is_null()).then(|| {
        let n = (0..).take_while(|&i| *p.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    });
    if !p.is_null() {
        GlobalUnlock(h as _);
    }
    CloseClipboard();
    s
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

/// Right-click: Explorer's own menu, with submenus and every shell extension's entries. On items, theirs plus what
/// only Mosaic does; on empty space, the folder on screen's (Paste, New ▸, Open in Terminal…) after View ▸,
/// Sort by ▸ and Refresh, as in Explorer.
unsafe fn context_menu(list: HWND, l: LPARAM) {
    let (mut x, mut y) = xy(l);
    let mut acts = Vec::new();
    let Some((paths, bg, extras)) = with(|u| {
        if x == -1 && y == -1 {
            // keyboard (menu key / Shift+F10): at the focused item, or the list's corner
            let i = u.focus.filter(|_| u.sel.count() > 0).unwrap_or(0);
            let x0 = if u.grid { u.cell_x(i % u.per).0 } else { 0 };
            let y0 = if u.sel.count() > 0 { ((i / u.ncols()) as f64 * u.rh() as f64 - u.y) as i32 + u.rh() } else { u.px(20) };
            let mut p = POINT { x: x0 + u.px(40), y: y0 };
            ClientToScreen(list, &mut p);
            (x, y) = (p.x, p.y);
        } else {
            let mut p = POINT { x, y };
            ScreenToClient(list, &mut p);
            match u.item_at(p.x, p.y) {
                Some(i) if !u.sel.has(i) => u.set_sel(Sel::one(i), Some(i), Some(Some(i))),
                Some(_) => {}
                None => u.set_sel(Sel::default(), None, Some(None)), // empty space: the folder's menu, not the items'
            }
        }
        if u.sel.count() == 0 {
            return (u.here().into_iter().collect(), true, bg_extras(u, &mut acts));
        }
        let one_dir = u.sel.count() == 1 && u.sel.iter().next().and_then(|i| u.get(i)).is_some_and(|r| r.is_dir);
        (u.sel_paths(), false, item_extras(one_dir, &mut acts))
    }) else {
        return;
    };
    UpdateWindow(list);
    explorer_menu(paths, bg, extras, acts, x, y);
}

/// The filter and grid sort dropdowns: the options under the button (over it, in the status bar), the
/// current one checked.
unsafe fn drop_menu(main: HWND, hit: Hit) {
    // (label, value, a second name shown muted at the right: a language's English one, to find it by either)
    let opts: Vec<(&str, &str, &str)> = match hit {
        Hit::Filter => filters().iter().map(|o| (o.0, o.1, "")).collect(),
        Hit::Lang => crate::i18n::LANGS.iter().map(|l| (l.name, l.code, if l.english == l.name { "" } else { l.english })).collect(),
        _ => col_defs().iter().map(|o| (o.0, o.1, "")).collect(),
    };
    let cur = |u: &Ui| match hit {
        Hit::Filter => u.args.cat.clone(),
        Hit::Lang => crate::i18n::lang(),
        _ => u.args.sort.clone(),
    };
    let Some((r, cur, gap, dialog_w)) = with(|u| (u.hits.iter().find(|x| x.1 == hit).map_or(RECT { left: 0, top: 0, right: 0, bottom: 0 }, |x| x.0), cur(u), u.px(4), u.px(260))) else { return };
    // Language has too many options to read as a flyout under a small button — open it centered over the main
    // window instead, like a little dialog with a search box, rather than anchored to the button.
    let (p, top, min_w) = if hit == Hit::Lang {
        let mut wr: RECT = zeroed();
        GetWindowRect(main, &mut wr);
        let (cx, cy) = ((wr.left + wr.right) / 2, (wr.top + wr.bottom) / 2);
        let pt = POINT { x: cx - dialog_w * 3 / 4, y: cy - dialog_w };
        (pt, pt, dialog_w * 3 / 2)
    } else {
        let mut p = POINT { x: r.left, y: r.bottom + gap };
        let mut top = POINT { x: r.left, y: r.top - gap };
        ClientToScreen(main, &mut p);
        ClientToScreen(main, &mut top);
        (p, top, r.right - r.left)
    };
    let items = opts.iter().map(|(l, v, e)| (l.to_string(), e.to_string(), *v == cur)).collect();
    if let Some(k) = popup(main, p.x, p.y, top.y, min_w, true, items, hit == Hit::Lang) {
        let v = opts[k].1;
        with(|u| {
            match hit {
                Hit::Filter => {
                    u.args.cat = v.into();
                    u.layout(); // the status bar shows the filter
                }
                Hit::Lang => return u.set_lang(v),
                _ => {
                    u.args.sort = v.into();
                    u.remember();
                }
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
    if menu == 3 {
        return tabs_bar_menu(main, r, gap);
    }
    let menu = if menu > 3 { menu - 1 } else { menu }; // the ones after Tabs, as numbered before it came
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
                (format!("{}…", t("menu.theme")), String::new(), false),
                (format!("{}…", t("icons.title")), String::new(), false),
                sep(),
                (t("menu.show_hidden").into(), String::new(), u.args.hidden),
                (t("menu.folder_color").into(), String::new(), u.folder_color),
                (t("menu.show_system").into(), String::new(), u.args.system),
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
        4 => vec![(t("menu.language").into(), String::new(), false), sep(), (format!("{}…", t("apps.title")), String::new(), false)],
        _ => vec![(t("menu.about").into(), String::new(), false)],
    };
    let Some(k) = popup(main, p.x, p.y, top.y, r.right - r.left, matches!(menu, 2..=4), items, false) else { return };
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
        (1, 3) => act(Act::SelectAll),
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
        (2, 13) => {
            with(|u| {
                u.args.hidden = !u.args.hidden;
                u.save_settings();
                u.run(false);
            });
        }
        (2, 15) => {
            with(|u| {
                u.args.system = !u.args.system;
                u.save_settings();
                u.run(false);
            });
        }
        (2, 14) => {
            with(|u| {
                u.folder_color = !u.folder_color;
                u.save_settings();
                u.invalidate_all();
            });
        }
        (2, 10) => themes::open(main),
        (2, 11) => icons::open(main),
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
        (4, 2) => apps::open(main),
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

// ---- window procedures ---------------------------------------------------------------------------------------
unsafe extern "system" fn main_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            with(|u| u.paint_main());
            EndPaint(h, &ps);
            if CLOAKED.get() {
                // the panes are windows of their own: drawn now, then the whole shows at once
                RedrawWindow(h, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN);
                cloak(h, false);
            }
            return 0;
        }
        WM_SIZE => {
            with(|u| u.layout());
            return 0;
        }
        WM_DEVICECHANGE if matches!(wp, 0x8000 | 0x8004) => {
            // DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE: a drive came or went
            side::relabel();
            with(|u| {
                if matches!(u.args.q.trim(), "pc:" | "net:") {
                    u.run(true);
                }
            });
        }
        WM_CTLCOLOREDIT => {
            // Cached by color, not just created once: `theme()` changes at runtime (the Theme dropdown),
            // and a brush made for the old theme would otherwise paint every edit box the wrong shade forever.
            static BRUSH: std::sync::Mutex<Option<(u32, isize)>> = std::sync::Mutex::new(None);
            let bg = gdi(field_bg(l as HWND));
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
        WM_COMMAND if wp & 0xffff == save::ID => {
            if matches!((wp >> 16) as u32, 0x0100 | 0x0200) {
                InvalidateRect(h, null(), 0); // EN_SETFOCUS / EN_KILLFOCUS: the name box's focus ring
            }
            return 0;
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
                        u.typing = true;
                        u.typed();
                    });
                }
                T_UNCLOAK => cloak(h, false),
                T_POLL => {
                    if with(|u| u.poll()) == Some(true) {
                        offer_install(h);
                    }
                }
                T_RENAME => {
                    KillTimer(h, T_RENAME);
                    if with(|u| u.sel.count() == 1 && u.press.is_none() && GetFocus() == u.list) == Some(true) {
                        start_rename();
                    }
                }
                T_FLASH => {
                    KillTimer(h, T_FLASH);
                    InvalidateRect(h, null(), 0);
                }
                T_THUMB => {
                    KillTimer(h, T_THUMB);
                    with(|u| {
                        u.each_visible(|u| {
                            u.want_thumbs();
                            u.want_kinds();
                        })
                    });
                }
                T_VIEWS => {
                    KillTimer(h, T_VIEWS);
                    write_views();
                    write_bags();
                }
                T_CARD => {
                    with(|u| u.card_due());
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
            // for the pane whose search it is; a pane closed meanwhile: dropped
            let p = l as *mut Done;
            match with(|u| u.pane_by_seq((*p).0).map(|id| u.with_pane(id, |u| u.done(*Box::from_raw(p))))) {
                None => {
                    PostMessageW(h, m, wp, l); // state busy: try again in a moment
                }
                Some(None) => drop(Box::from_raw(p)),
                Some(Some(())) => {}
            }
            return 0;
        }
        WM_ICON => {
            let p = l as *mut (String, Option<Px>, bool);
            match with(|u| u.on_list(wp as HWND, |u| u.got_icon(*Box::from_raw(p)))) {
                None => {
                    PostMessageW(h, m, wp, l);
                }
                Some(None) => drop(Box::from_raw(p)),
                Some(Some(())) => {}
            }
            return 0;
        }
        WM_TAB_ICON => {
            let p = l as *mut (String, Option<Vec<u8>>);
            if with(|u| u.got_tab_icon(*Box::from_raw(p))).is_none() {
                PostMessageW(h, m, wp, l);
            }
            return 0;
        }
        WM_KIND => {
            let p = l as *mut (String, u8, u64);
            if with(|u| u.got_kind(*Box::from_raw(p))).is_none() {
                PostMessageW(h, m, wp, l);
            }
            return 0;
        }
        WM_THUMB => {
            let p = l as *mut (String, Option<Px>, bool, i32);
            match with(|u| u.on_list(wp as HWND, |u| u.got_thumb(*Box::from_raw(p)))) {
                None => {
                    PostMessageW(h, m, wp, l);
                }
                Some(None) => drop(Box::from_raw(p)),
                Some(Some(())) => {}
            }
            return 0;
        }
        WM_COPYDATA => {
            return copydata(h, l) as LRESULT;
        }
        WM_TABS_IN => {
            let json = *Box::from_raw(l as *mut String);
            with(|u| {
                if let Ok(s) = serde_json::from_str::<Session>(&json) {
                    u.load_session(s, false);
                }
                if wp == 1 {
                    u.secondary = false; // the window that kept the session merged in here: this one keeps it now
                }
                u.autosave(true);
                u.invalidate_all();
            });
            SetForegroundWindow(h);
            return 0;
        }
        WM_MERGE => {
            // all these tabs to the window in wParam, then close
            let Some(json) = with(|u| u.session_json(true)) else { return 0 };
            let keeper = with(|u| !u.secondary).unwrap_or(false);
            if send_tabs(wp as HWND, &json, keeper) {
                with(|u| u.merged = true);
                PostMessageW(h, WM_CLOSE, 0, 0);
            }
            return 0;
        }
        dnd::WM_DROPPED => {
            dnd::dropped(l);
            return 0;
        }
        WM_CLOSE => {
            save::closing();
            let n = with(|u| if u.merged { 0 } else { u.tabs.len() }).unwrap_or(0);
            if n > 10 {
                let text = wz(&t("tabs.close_many").replace("{n}", &n.to_string()));
                let title = wz("Mosaic");
                if MessageBoxW(h, text.as_ptr(), title.as_ptr(), MB_OKCANCEL | MB_ICONQUESTION) != IDOK {
                    return 0;
                }
            }
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
            // a tab, a pane's caption or a gap between panes: held, it may be dragged
            if with(|u| u.press_main(x, y)) == Some(true) {
                return 0;
            }
            match with(|u| u.click_main(x, y)).flatten() {
                Some(Hit::Preview) => toggle_preview(),
                Some(Hit::Side) => toggle_side(),
                Some(Hit::Menu(i)) => menu_bar_popup(h, i),
                Some(Hit::SaveType) => save::type_menu(h),
                Some(Hit::SaveOk) => save::commit(h),
                Some(Hit::SaveCancel) => save::cancel(h),
                Some(Hit::SaveWin) => save::give_back(h),
                Some(Hit::TabList) => {
                    if let Some(r) = with(|u| u.hits.iter().find(|x| x.1 == Hit::TabList).map(|x| x.0)).flatten() {
                        tab_list_menu(h, r);
                    }
                }
                Some(hit) => drop_menu(h, hit),
                None => {
                    // the margin left of the list (by the places panel, or the window's edge) starts a selection
                    // band as the list's own empty space does: no dead strip to aim past
                    let list = with(|u| u.areas.iter().filter(|(_, r)| y >= r.top && y < r.bottom).min_by_key(|(_, r)| r.left).map(|&(id, _)| u.list_of(id))).flatten();
                    if let Some(list) = list {
                        let mut r: RECT = zeroed();
                        GetWindowRect(list, &mut r);
                        let mut p = POINT { x, y };
                        ClientToScreen(h, &mut p);
                        let side = with(|u| u.side).filter(|&s| IsWindowVisible(s) != 0);
                        let from = side.map_or(i32::MIN, |s| {
                            let mut sr: RECT = zeroed();
                            GetWindowRect(s, &mut sr);
                            sr.right
                        });
                        if p.y >= r.top && p.y < r.bottom && p.x < r.left && p.x >= from {
                            let (lx, ly) = (p.x - r.left, p.y - r.top);
                            SendMessageW(list, WM_LBUTTONDOWN, wp, ((ly as u16 as u32) << 16 | lx as u16 as u32) as isize);
                        }
                    }
                }
            }
            return 0;
        }
        WM_MBUTTONUP => {
            // middle-click on a tab (or a pane's caption) closes it, as in a browser
            let (x, y) = xy(l);
            with(|u| match u.hits.iter().find(|(r, _)| inside(r, x, y)).map(|h| h.1) {
                Some(Hit::Tab(k) | Hit::TabClose(k)) if !u.tabs[k].o.pinned => u.close_tab(k),
                Some(Hit::Pane(id) | Hit::PaneClose(id)) => u.close_pane(id),
                _ => {}
            });
            return 0;
        }
        WM_RBUTTONUP => {
            let (x, y) = xy(l);
            // a search switch: what it does, with examples (Timestamps: a date filter to pick)
            let switch = with(|u| u.hits.iter().find(|(r, _)| inside(r, x, y)).copied()).flatten().filter(|(_, k)| {
                matches!(k, Hit::Tags | Hit::Content | Hit::MatchCase | Hit::WholeWord | Hit::Regex | Hit::Diacritics | Hit::Timestamps)
            });
            if let Some((r, k)) = switch {
                help::open(h, k, r);
                return 0;
            }
            if main_menu(h, x, y) {
                return 0;
            }
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = xy(l);
            // a tab: renamed; a pane's caption: zoomed; a gap: halved; the strip's empty space: a new tab
            let hit = with(|u| u.hits.iter().find(|(r, _)| inside(r, x, y)).map(|h| h.1).or_else(|| u.on_strip(x, y).then_some(Hit::NewTab))).flatten();
            let ta = match hit {
                Some(Hit::Tab(k)) => Some(TA::Rename(k)),
                Some(Hit::Pane(p)) => Some(TA::Zoom(p)),
                Some(Hit::Gutter(sid)) => Some(TA::Half(sid)),
                Some(Hit::NewTab) => None,
                _ => None,
            };
            if let Some(a) = ta {
                with(|u| u.drag_end(false));
                ReleaseCapture();
                tab_act(a);
                return 0;
            }
            if hit == Some(Hit::NewTab) && with(|u| !u.hits.iter().any(|(r, _)| inside(r, x, y))) == Some(true) {
                with(|u| u.new_tab("home:", u.tabs.len()));
                return 0;
            }
            let tag = with(|u| {
                if u.hits.iter().any(|(r, t)| matches!(t, Hit::ColEdge(..)) && inside(r, x, y)) {
                    // a column gap: back to the default widths
                    u.colw = COLS_DEFAULT;
                    u.col_gen += 1;
                    u.layout();
                    u.invalidate_all();
                    u.save_settings();
                    return true;
                }
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
            let held = with(|u| {
                if u.drag.is_some() {
                    u.drag_move(x, y);
                } else if u.gut.is_some() {
                    u.gut_to = Some((x, y)); // resized on the next frame (`step_ui`)
                } else if u.col_drag.is_some() {
                    u.drag_col(x);
                } else {
                    return false;
                }
                true
            });
            if held == Some(true) {
                return 0;
            }
            with(|u| {
                let hot = u.hits.iter().find(|(r, _)| inside(r, x, y)).map(|h| h.1).filter(|h| *h != Hit::Search);
                if hot != u.hot {
                    u.hot = hot;
                    InvalidateRect(h, null(), 0);
                    u.card_hover();
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
                u.card_close();
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
        WM_LBUTTONUP | WM_CAPTURECHANGED if with(|u| u.drag.is_some() || u.gut.is_some() || u.col_drag.is_some()) == Some(true) => {
            with(|u| {
                if u.drag.is_some() {
                    u.drag_end(m == WM_LBUTTONUP);
                } else if u.col_drag.take().is_some() {
                    u.save_settings();
                } else {
                    u.gut = None;
                    u.gut_to = None;
                }
                u.invalidate_all();
            });
            if m == WM_LBUTTONUP {
                ReleaseCapture();
            }
            return 0;
        }
        // a click on this window while a dialog (Theme, Icons, File windows) holds it disabled: the dialog closes, as a
        // menu does, and the click goes no further (no beep)
        WM_SETCURSOR if (l & 0xffff) as u16 == HTERROR as u16
            && matches!((l >> 16) as u32 & 0xffff, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN)
            && themes::hwnd().or(apps::hwnd()).or(icons::hwnd()).or(help::hwnd()).is_some() =>
        {
            themes::dismiss();
            apps::dismiss();
            icons::dismiss();
            help::dismiss();
            return 1;
        }
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT && with(|u| {
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            ScreenToClient(h, &mut p);
            u.split.is_some() || inside(&u.grip, p.x, p.y) && u.hot == Some(Hit::Split) || u.col_drag.is_some() || matches!(u.hot, Some(Hit::ColEdge(..)))
        }) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE));
            return 1;
        }
        // a gap between panes: the cursor says which way it moves
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT && with(|u| u.gutter_vert().is_some()) == Some(true) => {
            let vert = with(|u| u.gutter_vert()).flatten() == Some(true);
            SetCursor(LoadCursorW(null_mut(), if vert { IDC_SIZEWE } else { IDC_SIZENS }));
            return 1;
        }
        // the name box's edge around its text field: a text box too, not a button
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT && with(|u| u.hot == Some(Hit::SaveName)) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_IBEAM));
            return 1;
        }
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT => {
            if with(|u| u.hot.is_some_and(|h| !matches!(h, Hit::Filter | Hit::Sort | Hit::Tag | Hit::Menu(_) | Hit::Tab(_) | Hit::Pane(_) | Hit::Gutter(_)))) == Some(true) {
                SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                return 1;
            }
        }
        WM_MOUSEWHEEL => {
            // over the strip: scrolled, when its tabs scroll; else the next or previous tab
            let mut p = POINT { x: (l & 0xffff) as i16 as i32, y: ((l >> 16) & 0xffff) as i16 as i32 };
            ScreenToClient(h, &mut p);
            let delta = ((wp >> 16) & 0xffff) as i16 as i32;
            if with(|u| u.on_strip(p.x, p.y)) == Some(true) {
                with(|u| {
                    if u.scroll.1 > 0 {
                        u.scroll_strip(-delta * u.px(60) / 120);
                    } else {
                        let n = u.tabs.len();
                        u.switch_tab((u.cur + if delta < 0 { 1 } else { n - 1 }) % n);
                    }
                });
                return 0;
            }
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
                    u.new_canvases(&g);
                    u.gfx = g;
                }
                DeleteObject(u.edit_font);
                u.edit_font = edit_font(u.scale);
                SendMessageW(u.edit, WM_SETFONT, u.edit_font as usize, 1);
            });
            SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            set_icons(h);
            with(|u| u.layout());
            preview::rescale((wp & 0xffff) as f64 / 96.0);
            side::rescale((wp & 0xffff) as f64 / 96.0);
            return 0;
        }
        WM_DESTROY => {
            with(|u| {
                u.save_settings(); // the window's place
                u.save_kinds();
                u.autosave(true);
            });
            crate::recent::save();
            write_views();
            PostQuitMessage(0);
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

unsafe extern "system" fn list_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    // a press (or a Ctrl+wheel zoom) in a pane makes it the focused one; one closing takes none
    let press = matches!(m, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_MBUTTONDOWN | WM_RBUTTONDOWN) || m == WM_MOUSEWHEEL && wp & 0x8 != 0;
    if press && with(|u| u.activate_list(h)) == Some(false) {
        return 0;
    }
    match m {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            withp(h, |u| u.paint_list());
            EndPaint(h, &ps);
            return 0;
        }
        WM_SIZE => {
            withp(h, |u| u.clamp());
            InvalidateRect(h, null(), 0);
            return 0;
        }
        WM_MOUSEWHEEL => {
            end_rename(true); // the field would stay put while its item scrolls away
            let delta = ((wp >> 16) & 0xffff) as i16 as i32;
            withp(h, |u| {
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
            withp(h, |u| u.ldown(x, y));
            return 0;
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = xy(l);
            if let Some(main) = with(|u| u.main) {
                KillTimer(main, T_RENAME); // a double-click opens, never renames
            }
            if withp(h, |u| u.on_sb(x)) == Some(true) {
                withp(h, |u| u.ldown(x, y)); // a second press on the scrollbar pages again
                return 0;
            }
            if let Some(Some((p, dir))) = withp(h, |u| u.item_at(x, y).and_then(|i| u.get(i)).map(|r| (r.path.clone(), r.is_dir))) {
                // a folder with Ctrl: in a new tab; with Shift: in a pane beside this one
                match (dir, key_down(VK_CONTROL), key_down(VK_SHIFT)) {
                    (true, true, _) => {
                        with(|u| u.open_tab(&p));
                    }
                    (true, _, true) => {
                        with(|u| u.open_beside(&p, 1));
                    }
                    _ => open_item(&p),
                }
            }
            return 0;
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(l);
            let r = withp(h, |u| u.mmove(x, y));
            // paint now: queued, the list's paint waits behind the main window's (the status bar), which a drag
            // invalidates again on every move, so the band trailed the pointer
            UpdateWindow(h);
            if let Some(Some(paths)) = r {
                if !paths.is_empty() {
                    // the first item's thumbnail at the grid's tile size (in the list too), from a shell worker (its
                    // helper already running); not long waited for
                    let (size, ask) = withp(h, |u| (u.tile(), u.tsize())).unwrap_or((96, 256));
                    let (tx, rx) = std::sync::mpsc::channel();
                    let first = paths[0].clone();
                    shell::shell_spawn(Box::new(move |skip| {
                        let _ = tx.send(if skip { None } else { shell::thumbnail(&first, ask, false) });
                    }));
                    let image = rx.recv_timeout(Duration::from_millis(400)).ok().flatten().and_then(|png| png_pixels(&png, size));
                    // a playing preview would hold the file: the move would wait on it ("File In Use")
                    let had = preview::release(&paths);
                    if shell::drag_out(&paths.iter().map(String::as_str).collect::<Vec<_>>(), h as isize, image) {
                        withp(h, |u| u.relist_soon()); // dropped (moved by Explorer, maybe): shown as soon as it lands
                    } else if had {
                        withp(h, |u| {
                            u.pv_key.clear(); // cancelled: shown again
                            u.want_preview();
                        });
                    }
                }
            }
            return 0;
        }
        WM_LBUTTONUP => {
            withp(h, |u| u.lup());
            return 0;
        }
        WM_MOUSELEAVE => {
            withp(h, |u| {
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
            withp(h, |u| {
                // on a folder: it opens in a new tab, as a browser's middle-click does; elsewhere, autoscroll
                let dir = u.item_at(x, y).and_then(|i| u.get(i)).filter(|r| r.is_dir).map(|r| r.path.clone());
                if let (None, Some(d)) = (&u.auto, dir) {
                    // in the background, as a browser's middle-click does
                    let q = u.browse_q(&d);
                    return u.new_tab_bg(&q, u.cur + 1);
                }
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
            withp(h, |u| {
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
            shell::menu_warm(); // its menu is asked for on the button's release
            withp(h, |u| {
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
        WM_SETCURSOR if withp(h, |u| u.auto.is_some()) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_SIZENS));
            return 1;
        }
        WM_CHAR if wp >= 32 => {
            // typing while the list has focus goes to the search box, at the end of what's there. With one item
            // selected it looks inside that folder (or the selected file's): the folder goes into the box first.
            let Some((edit, dir)) = withp(h, |u| {
                let one = (u.sel.count() == 1).then(|| u.sel.iter().next().and_then(|i| u.get(i))).flatten();
                let dir = one.and_then(|r| if r.is_dir { Some(r.path.clone()) } else { std::path::Path::new(&r.path).parent().map(|d| d.to_string_lossy().into_owned()) });
                (u.edit, dir)
            }) else {
                return 0;
            };
            match dir {
                Some(d) => {
                    withp(h, |u| u.search_in(&d));
                }
                None => {
                    SetFocus(edit);
                    SendMessageW(edit, 0x00B1, GetWindowTextLengthW(edit) as usize, GetWindowTextLengthW(edit) as isize); // EM_SETSEL: caret at the end
                }
            }
            SendMessageW(edit, WM_CHAR, wp, l);
            return 0;
        }
        WM_COMMAND if (wp >> 16) as u32 == 0x0200 => {
            // EN_KILLFOCUS from the rename field: keep the name, once the focus has settled
            PostMessageW(h, WM_RENAME_END, l as usize, 0);
            return 0;
        }
        WM_RENAME_END => {
            if RENAMING.with(|r| r.borrow().as_ref().is_some_and(|r| r.0 as usize == wp)) && GetFocus() as usize != wp {
                end_rename(true);
            }
            return 0;
        }
        WM_RENAME => {
            start_rename();
            return 0;
        }
        WM_CTLCOLOREDIT => return main_proc(GetParent(h), m, wp, l), // the rename field: the search box's colours
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_views_read_back_as_written() {
        let m = parse_views("grid\t160\tmtime\tdesc\t3\tc:\\users\\me\\pictures\nlist\t128\tname\tasc\t4\td:\\a\tb\ngrid\t160\t3\tc:\\old\n");
        let v = |grid, size, sort: &str, desc, n| Some(FolderView { grid, size, sort: sort.into(), desc, n });
        assert_eq!(m.get("c:\\users\\me\\pictures").cloned(), v(true, 160, "mtime", true, 3));
        assert_eq!(m.get("d:\\a\tb").cloned(), v(false, 128, "name", false, 4)); // a tab in a name: the path is the rest
        assert_eq!(m.len(), 2); // a line of the old kind, dropped
    }

    #[test]
    fn a_search_box_edits_as_windows_does() {
        use super::{Clip, Field};
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_BACK, VK_END, VK_HOME, VK_LEFT, VK_RIGHT};
        let mut f = Field::default();
        let mut c = None;
        f.insert("français (Canada)");
        // Ctrl+Backspace: a word at a time, a run of punctuation counting as one
        for want in ["français (Canada", "français (", "français ", ""] {
            f.key(VK_BACK, true, false, &mut c);
            assert_eq!(f.q, want);
        }
        f.insert("ab cd ef");
        f.key(VK_LEFT, true, false, &mut c);
        assert_eq!(f.caret, 6);
        f.key(VK_LEFT, true, true, &mut c); // Ctrl+Shift+Left: "cd " selected
        assert_eq!(f.sel(), (3, 6));
        f.key(0x58, true, false, &mut c); // Ctrl+X
        assert_eq!((f.q.as_str(), c.take()), ("ab ef", Some(Clip::Copy("cd ".into()))));
        f.key(VK_HOME, false, false, &mut c);
        f.key(VK_RIGHT, true, false, &mut c);
        assert_eq!(f.caret, 3);
        f.key(VK_END, false, true, &mut c);
        f.insert("é");
        assert_eq!((f.q.as_str(), f.caret), ("ab é", 5));
        f.key(VK_LEFT, false, false, &mut c);
        assert_eq!(f.caret, 3); // a character back, not a byte
        assert!(!f.key(0x7B, false, false, &mut c)); // F12: not the box's
    }

    #[test]
    fn the_folder_preview_is_the_newest_file_of_its_kind() {
        let d = std::env::temp_dir().join(format!("mosaic-newest-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let t = std::time::SystemTime::now();
        let at = |n: &str, secs: u64| {
            let f = std::fs::File::create(d.join(n)).unwrap();
            f.set_modified(t - std::time::Duration::from_secs(secs)).unwrap();
        };
        at("a.mp4", 300);
        at("z.mp4", 10); // newest video, last by name
        at("m.txt", 1); // newer, but not a video
        let kind = crate::kind_of("x.mp4");
        let pick = || newest_of_kind(d.to_str().unwrap(), kind).map(|p| p.file_name().unwrap().to_string_lossy().into_owned());
        assert_eq!(pick().as_deref(), Some("z.mp4"));
        at("b.mp4", 10);
        at("z.mp4", 10); // a tie: the first by name
        assert_eq!(pick().as_deref(), Some("b.mp4"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn both_puts_the_preview_in_the_kind_folder() {
        // a 64 px folder: dark back from row 10, light front from row 20 (BGRA, opaque)
        let (back, front) = ([90u8, 60, 120, 255], [230u8, 180, 250, 255]);
        let mut px = vec![0u8; 64 * 64 * 4];
        for y in 10..54 {
            for x in 4..60 {
                px[(y * 64 + x) * 4..][..4].copy_from_slice(if y < 20 { &back } else { &front });
            }
        }
        // a red picture
        let mut png = Vec::new();
        let mut e = png::Encoder::new(&mut png, 8, 8);
        e.set_color(png::ColorType::Rgba);
        e.write_header().unwrap().write_image_data(&[255, 0, 0, 255].repeat(64)).unwrap();
        let (out, ..) = fold_in(&(px, 64, 64), &png, false).unwrap();
        let at = |x: usize, y: usize| out[(y * 64 + x) * 4..][..4].to_vec();
        // Windows' layout: the front down 66/256 (16 px), the picture from just under the back's edge, 1 px in
        assert_eq!(at(32, 25), [0, 0, 255, 255]); // the picture, over the back
        assert_eq!(at(32, 50), front); // the front, over the picture's bottom
        assert_eq!(at(4, 30), back); // where the front was, beside the picture: the back now
        assert_eq!(at(2, 30)[3], 0); // outside the folder: still nothing
    }

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
    fn moving_a_pane_of_two_below_the_other_takes_the_whole_bottom_half() {
        // 1 | 2, and 2 dragged onto 1's bottom: the preview (`Ui::landing`) lays it out with 2 out of its old place
        let mut root = Node::split_off(Node::Leaf(1), 1, Node::Leaf(2), 7);
        assert!(root.detach(2));
        root.dock(Node::Leaf(2), Dock::Pane(1, 3), 8);
        root.settle();
        let (mut leaves, mut gutters) = (Vec::new(), Vec::new());
        root.place(RECT { left: 0, top: 0, right: 1000, bottom: 610 }, 10, (&[], 0), &mut leaves, &mut gutters);
        let r = leaves.iter().find(|l| l.0 == 2).unwrap().1;
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 310, 1000, 610));
    }

    #[test]
    fn springs_settle_on_their_target_without_bouncing() {
        let (mut x, mut v) = (0.0, 0.0);
        let mut path = vec![];
        while spring(&mut x, &mut v, 1.0, 1.0 / 60.0, 22.0, 0.0005) {
            path.push(x);
        }
        assert_eq!((x, v), (1.0, 0.0));
        assert!(path.windows(2).all(|w| w[0] <= w[1] && w[1] <= 1.0), "only ever forward, never past");
        assert!(path.len() < 60, "settles within a second, took {} frames", path.len());
    }

    #[test]
    fn panes_split_move_and_close() {
        let r = RECT { left: 0, top: 0, right: 1010, bottom: 500 };
        let mut t = Node::Leaf(0);
        assert!(t.insert(0, 1, &mut Some(Node::Leaf(1)), 10)); // 1 right of 0
        assert!(t.insert(1, 3, &mut Some(Node::Leaf(2)), 11)); // 2 below 1
        assert_eq!(t.leaves(), [0, 1, 2]);
        while t.animate(1.0 / 60.0) {}
        let (mut leaves, mut gutters) = (vec![], vec![]);
        t.place(r, 10, (&[], 0), &mut leaves, &mut gutters);
        let l0 = leaves[0].1;
        assert_eq!((leaves[0].0, l0.left, l0.top, l0.right, l0.bottom), (0, 0, 0, 500, 500));
        assert_eq!(leaves[1].1.left, 510);
        assert_eq!((leaves[2].1.top, leaves[2].1.bottom), (255, 500)); // (500 - 10 gap) / 2 + 10
        assert_eq!(gutters.iter().map(|g| (g.0, g.3)).collect::<Vec<_>>(), [(10, true), (11, false)]);
        // closing 1: it shrinks away, then leaves 2 in its place
        assert!(t.collapse(1));
        assert_eq!(t.settled(1), Some(false));
        while t.animate(1.0 / 60.0) {}
        assert_eq!(t.settled(1), Some(true));
        assert!(t.detach(1));
        assert_eq!(t.leaves(), [0, 2]);
        assert!(!t.detach(7));
        assert!(t.detach(2));
        assert!(!t.detach(0)); // the last one stays
        assert_eq!(t.leaves(), [0]);
    }

    #[test]
    fn the_folder_on_screen_comes_from_a_folder_listing_query() {
        assert_eq!(listed_dir(r#"child:"C:\Users\ann\Docs""#).as_deref(), Some(r"C:\Users\ann\Docs"));
        assert_eq!(listed_dir(r#" descendant:"D:\Photos" beach "#).as_deref(), Some(r"D:\Photos")); // narrowed by a word
        assert_eq!(listed_dir(r#"child:"E:""#).as_deref(), Some(r"E:\")); // a drive's top
        assert_eq!(listed_dir("holiday photos"), None);
        assert_eq!(listed_dir(r#"child:"C:\unclosed"#), None);
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
    fn mix_moves_each_channel_toward_the_other_color() {
        assert_eq!(mix(0x000000, 0xff8040, 0.5), 0x804020);
        assert_eq!(mix(0x123456, 0xabcdef, 0.0), 0x123456);
    }

    #[test]
    fn mosaic_entries_go_after_explorers_open_group_with_tidy_lines() {
        let e = |l: &str| MItem { label: l.into(), ..Default::default() };
        let names = |v: Vec<MItem>| v.iter().map(|i| if i.label.is_empty() { "-".to_string() } else { i.label.clone() }).collect::<Vec<_>>().join(" ");
        let shell = vec![e("Open"), e(""), e(""), e("Copy"), e(""), e("Copy")];
        assert_eq!(names(merge(shell.clone(), vec![e("Mine")], false)), "Open - Mine - Copy");
        assert_eq!(names(merge(shell, vec![e("View")], true)), "View - Open - Copy");
        assert_eq!(names(merge(Vec::new(), vec![e("View")], true)), "View");
    }

    #[test]
    fn pixels_premultiply_and_shrink() {
        // 2×2: opaque red, transparent, opaque white, half-transparent white → one pixel, averaged, premultiplied
        let px = [255, 0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 128];
        let (out, w, h) = pixels(&px, 2, 2, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, [96, 96, 160, 160]); // B, G: (0+0+255+128)/4; R, A: (255+0+255+128)/4
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
    fn fit_cols_packs_flex_then_fixed_left_to_right() {
        // width 1000, pad 10, gap 10, fixed 150/100/100: content = 1000-20-40 = 940; Name and Path split 940-350
        let v = fit_cols(1000, 10, 10, 0.5, [150, 100, 100], 100, 50, false);
        assert_eq!(v, [(10, 305), (315, 610), (620, 770), (780, 880), (890, 990)]);
        // columns are contiguous, gapped by exactly `gap`, and stay inside [pad, width-pad]
        for w in v.windows(2) {
            assert_eq!(w[1].0 - w[0].1, 10);
        }
    }

    #[test]
    fn fit_cols_rtl_mirrors_the_row_keeping_each_columns_width_and_index() {
        for width in [1000, 400] {
            let ltr = fit_cols(width, 10, 10, 0.5, [85, 150, 150], 180, 60, false);
            let rtl = fit_cols(width, 10, 10, 0.5, [85, 150, 150], 180, 60, true);
            // same widths, same reading-order (column 0 first), but the row is flush against the right edge
            for (&(a, b), &(c, d)) in ltr.iter().zip(&rtl) {
                assert_eq!(b - a, d - c, "column width preserved under mirroring");
            }
            assert_eq!(rtl[0].1, width - 10); // column 0 (reading order) is flush against the row's right edge
        }
    }

    #[test]
    fn fit_cols_gives_way_to_the_name_when_narrow() {
        let fit = |width| fit_cols(width, 10, 10, 0.5, [85, 150, 150], 180, 60, false);
        let w = |v: [(i32, i32); 5]| v.map(|(a, b)| b - a);
        // Path shrinks first, then goes; Type, then Size go; Modified stays while Name has its minimum
        assert_eq!(w(fit(1000)), [278, 277, 85, 150, 150]);
        assert_eq!(w(fit(685)), [180, 60, 85, 150, 150]);
        assert_eq!(w(fit(684)), [249, 0, 85, 150, 150]);
        assert_eq!(w(fit(400)), [220, 0, 0, 0, 150]);
        assert_eq!(fit(400)[4], (240, 390));
        // nothing left but the name, which takes the whole row
        assert_eq!(w(fit(150)), [130, 0, 0, 0, 0]);
        // never overlapping, whatever the width
        for width in (0..1200).step_by(7) {
            let v = fit(width);
            for k in 0..4 {
                assert!(v[k].1 <= v[k + 1].0, "{width}: {v:?}");
            }
        }
    }

    #[test]
    fn column_resizing_trades_width_between_neighbours_and_keeps_a_minimum() {
        let w = [300, 300, 85, 150, 150];
        assert_eq!(col_resize(w, 0, 50, 40), [350, 250, 85, 150, 150]);
        assert_eq!(col_resize(w, 2, 500, 40), [300, 300, 195, 40, 150]); // Type stops at the minimum
        assert_eq!(col_resize(w, 2, -500, 40), [300, 300, 40, 195, 150]);
        // uneven shares: the flexible pair splits the room by it, the last one taking the rounding
        let v = fit_cols(1000, 10, 10, 0.3, [150, 100, 0], 100, 50, false);
        assert_eq!((v[0].1 - v[0].0, v[1].1 - v[1].0), (207, 483));
        assert_eq!(parse_cols("0.3,90,140,160"), (0.3, [90, 140, 160]));
        assert_eq!(parse_cols("junk"), COLS_DEFAULT);
    }
}
