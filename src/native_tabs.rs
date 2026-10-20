//! Tabs beyond the basics (see `Tab` and `Ui::tabs`): how each one looks and behaves (`TabOpts`), how the strip
//! looks (`TabPrefs`), groups, sleeping tabs, sessions and workspaces, the strip's layouts (top, bottom, left;
//! shrinking, scrolling or wrapping), tools for split tabs, the hover card and the Ctrl+Tab switcher, rebindable
//! keys, and tabs moving between windows.

use super::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const T_CARD: usize = 6;
pub(super) const WM_MERGE: u32 = WM_APP + 40; // to another window: send your tabs to the window in wParam, then close
pub(super) const WM_TABS_IN: u32 = WM_APP + 42; // to this window: tabs another one sent (lParam: a boxed JSON session)
const DATA_TABS: usize = 0x4D54; // WM_COPYDATA: some tabs
const DATA_ALL: usize = 0x4D55; // … a whole window that kept the session: the receiver keeps it from now on

/// The colours a tab or a group can wear (0xRRGGBB), with their names' keys.
pub(super) const PALETTE: [(u32, &str); 12] = [
    (0xE5484D, "color.red"),
    (0xF2994A, "color.orange"),
    (0xF5C542, "color.yellow"),
    (0x8BC34A, "color.lime"),
    (0x3CB371, "color.green"),
    (0x26A69A, "color.teal"),
    (0x29B6F6, "color.sky"),
    (0x4C8DF6, "color.blue"),
    (0x6E6AF0, "color.indigo"),
    (0xA66CF0, "color.purple"),
    (0xEC6CB9, "color.pink"),
    (0x9AA0A6, "color.grey"),
];
const EMOJI: [&str; 20] = ["📁", "📂", "⭐", "❤️", "🔥", "💼", "🏠", "🎵", "🎬", "📷", "📄", "💻", "🎮", "📦", "🧪", "🛠️", "💡", "🌐", "🔒", "✅"];

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
fn now_secs() -> u64 {
    now_ms() / 1000
}
/// An id no other tab or group has, in this run or any other (they're saved with the session).
pub(super) fn new_uid() -> u64 {
    static N: std::sync::LazyLock<AtomicU64> = std::sync::LazyLock::new(|| AtomicU64::new(now_ms() << 12));
    N.fetch_add(1, Ordering::Relaxed)
}

/// What a tab has of its own beyond its panes. The ones not saved are about this run only.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct TabOpts {
    pub(super) title: Option<String>, // shown instead of its place's name
    pub(super) color: Option<u32>,
    pub(super) icon: Option<String>, // an emoji
    pub(super) pinned: bool,         // first, icon only, no ×
    pub(super) locked: bool,         // folders opened from it open in a new tab
    pub(super) muted: bool,          // no live refresh
    pub(super) live: bool,           // refreshed even when not shown
    pub(super) scope: Option<String>, // searches typed in it look only in this folder
    pub(super) group: Option<u64>,
    pub(super) sync: bool,    // its panes scroll together
    pub(super) link: bool,    // a folder selected in one pane is listed in the next
    pub(super) compare: bool, // its two panes' folders compared: what's only in one, what differs
    pub(super) navsync: bool, // a pane going into a subfolder (or up) takes the others along
    #[serde(skip)]
    pub(super) zoom: Option<usize>, // the one pane shown, the others waiting
    #[serde(skip)]
    pub(super) folded: Vec<usize>, // panes folded to their caption
    #[serde(skip)]
    pub(super) asleep: Option<(NodeSpec, usize)>, // put to sleep: its panes as they were, and the focused one's index
    #[serde(skip)]
    pub(super) dot: bool, // its folder changed since it was last shown
    #[serde(skip)]
    pub(super) seen: u64, // when it was last shown (Unix seconds)
}

/// How the tab strip looks and behaves, kept with the settings ("tabs").
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct TabPrefs {
    pub(super) fused: u8,       // a split tab shows: 1 its layout, 2 every pane's name, 4 a pane count, 8 an underline
    pub(super) color_style: u8, // 0 a stripe, 1 a tint, 2 a dot
    pub(super) width: u8,       // 0 compact, 1 normal, 2 wide
    pub(super) fit: u8,         // too many to fit: 0 shrink, 1 scroll, 2 more rows
    pub(super) pos: u8,         // 0 top, 1 bottom, 2 left
    pub(super) close: u8,       // the ×: 0 on hover, 1 always, 2 never
    pub(super) icons: bool,
    pub(super) path: bool, // the full path instead of the folder's name
    pub(super) font: u8,   // 0 small, 1 normal, 2 large
    pub(super) still: bool, // no sliding or folding
    pub(super) count: bool, // items listed, after the name
    pub(super) busy: bool,  // a spinner while a tab searches
    pub(super) dots: bool,  // a dot on a tab whose folder changed
    pub(super) drive_color: bool,
    pub(super) rules: Vec<(String, u32)>, // folder (or folder name) → colour
    pub(super) mru: bool,                 // Ctrl+Tab in most-recently-used order, with a switcher
    pub(super) captions: bool,            // split panes' captions
    pub(super) sounds: bool,
    pub(super) start: u8,             // at launch: 0 the tabs as they were, 1 Desktop, 2 Home, 3 All, 4 `startup`
    pub(super) startup: Vec<String>,  // the folders `start` 4 opens
    pub(super) sleep: usize,          // at most this many tabs awake (0: no limit)
    pub(super) keys: HashMap<String, String>, // shortcuts changed from `KEYS`
    pub(super) cards: bool,           // a card about a tab pointed at
}
impl Default for TabPrefs {
    fn default() -> Self {
        TabPrefs {
            fused: 1 | 2,
            color_style: 0,
            width: 1,
            fit: 0,
            pos: 0,
            close: 0,
            icons: true,
            path: false,
            font: 1,
            still: false,
            count: false,
            busy: true,
            dots: true,
            drive_color: false,
            rules: Vec::new(),
            mru: false,
            captions: true,
            sounds: false,
            start: 3,
            startup: Vec::new(),
            sleep: 0,
            keys: HashMap::new(),
            cards: true,
        }
    }
}

/// A pane as saved: what it lists, its history, its view, where it was scrolled to (size 0: the defaults).
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct PaneSpec {
    pub(super) q: String,
    pub(super) hist: Vec<String>,
    pub(super) at: usize,
    pub(super) grid: bool,
    pub(super) size: i32,
    pub(super) sort: String,
    pub(super) dir: String,
    pub(super) cat: String,
    pub(super) y: f64,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(super) enum NodeSpec {
    Leaf(PaneSpec),
    Split { vert: bool, ratio: f64, a: Box<NodeSpec>, b: Box<NodeSpec> },
}
impl Default for NodeSpec {
    fn default() -> Self {
        NodeSpec::Leaf(PaneSpec::default())
    }
}
impl NodeSpec {
    pub(super) fn leaves(&self) -> Vec<&PaneSpec> {
        match self {
            NodeSpec::Leaf(p) => vec![p],
            NodeSpec::Split { a, b, .. } => [a.leaves(), b.leaves()].concat(),
        }
    }
    /// Its shape as a `Node` of panes numbered from `n` on: to draw it.
    fn shape(&self, n: &mut usize) -> Node {
        match self {
            NodeSpec::Leaf(_) => {
                *n += 1;
                Node::Leaf(*n - 1)
            }
            NodeSpec::Split { vert, ratio, a, b } => {
                let (a, b) = (a.shape(n), b.shape(n));
                Node::Split { id: 0, vert: *vert, ratio: *ratio, shown: *ratio, vel: 0.0, a: Box::new(a), b: Box::new(b) }
            }
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct TabSpec {
    pub(super) root: NodeSpec,
    pub(super) focus: usize, // index among its panes
    pub(super) o: TabOpts,
}
impl TabSpec {
    pub(super) fn folder(q: &str) -> TabSpec {
        TabSpec { root: NodeSpec::Leaf(PaneSpec { q: q.into(), ..Default::default() }), ..Default::default() }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Group {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) color: u32,
    pub(super) folded: bool,
}
/// A window's tabs, as saved, sent to another window, or kept as a workspace.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Session {
    pub(super) tabs: Vec<TabSpec>,
    pub(super) cur: usize,
    pub(super) groups: Vec<Group>,
}

impl Tab {
    /// A tab that wakes (its panes made) when it's first shown.
    pub(super) fn asleep(s: TabSpec) -> Tab {
        let mut t = Tab::new(usize::MAX);
        t.o = TabOpts { zoom: None, folded: Vec::new(), dot: false, asleep: Some((s.root, s.focus)), ..s.o };
        t
    }
}

fn app_dir() -> std::path::PathBuf {
    settings_path().parent().map(|p| p.to_path_buf()).unwrap_or_default()
}
fn session_path() -> std::path::PathBuf {
    app_dir().join("session.json")
}
fn workspaces_path() -> std::path::PathBuf {
    app_dir().join("workspaces.json")
}
fn load_workspaces() -> Vec<(String, Session)> {
    std::fs::read_to_string(workspaces_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_workspaces(v: &[(String, Session)]) {
    let _ = std::fs::create_dir_all(app_dir());
    let _ = std::fs::write(workspaces_path(), serde_json::to_string(v).unwrap_or_default());
}
thread_local! {
    /// The session as last written, scroll positions and all: the same again isn't written.
    static WRITTEN: RefCell<String> = const { RefCell::new(String::new()) };
}

/// A tab's colour from its drive (C: blue, D: green…).
pub(super) fn drive_color(dir: &str) -> Option<u32> {
    let d = dir.chars().next()?.to_ascii_uppercase();
    if !d.is_ascii_alphabetic() || dir.get(1..2) != Some(":") {
        return None;
    }
    Some(match d {
        'C' => 0x4C8DF6,
        'D' => 0x3CB371,
        'E' => 0xF2994A,
        'F' => 0xA66CF0,
        'G' => 0x26A69A,
        _ => PALETTE[d as usize % PALETTE.len()].0,
    })
}
/// A tab's colour from the rules: a path is a folder and all below it, a plain name any folder of that name and
/// all below it. The longest that matches wins.
pub(super) fn rule_color(rules: &[(String, u32)], dir: &str) -> Option<u32> {
    let d = dir.to_lowercase();
    let hit = |p: &str| {
        let p = p.trim().trim_end_matches('\\').to_lowercase();
        if p.is_empty() {
            false
        } else if p.contains('\\') || p.contains(':') {
            d == p || d.starts_with(&format!("{p}\\"))
        } else {
            d.split('\\').any(|c| c == p)
        }
    };
    rules.iter().filter(|(p, _)| hit(p)).max_by_key(|(p, _)| p.len()).map(|r| r.1)
}
/// Where a tab's place takes its icon from, as the sidebar's: Explorer's own place, or the folder. A search: none.
fn icon_src(q: &str) -> Option<String> {
    match q.trim() {
        "" => Some("shell:::{031E4825-7B94-4dc3-B131-E946B44C8DD5}".into()),
        "home:" => Some("shell:::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}".into()),
        q => list_of(q).map(|l| l.2.to_string()).or_else(|| listed_dir(q)),
    }
}
fn kind_icon(q: &str) -> &'static str {
    match q.trim() {
        "" => "🗂️",
        "home:" => "🏠",
        "recent:" | "recent-dirs:" => "🕘",
        "pc:" => "💻",
        "net:" => "🌐",
        "linux:" => "🐧",
        q if listed_dir(q).is_some() => "📁",
        _ => "🔍",
    }
}
/// A colour's swatch for a menu: a round dot, premultiplied BGRA.
fn swatch(c: u32) -> (i32, i32, Vec<u8>) {
    let n = 32usize;
    let mut v = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
            let a = (13.0 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            let i = (y * n + x) * 4;
            let ch = |s: u32| (((c >> s) & 0xff) as f32 * a).round() as u8;
            v[i..i + 4].copy_from_slice(&[ch(0), ch(8), ch(16), (255.0 * a).round() as u8]);
        }
    }
    (32, 32, v)
}

/// A shortcut's name as the bindings write it ("Ctrl+Shift+T").
pub(super) fn combo_of(ctrl: bool, shift: bool, alt: bool, vk: u16) -> String {
    let mut s = String::new();
    for (on, m) in [(ctrl, "Ctrl+"), (shift, "Shift+"), (alt, "Alt+")] {
        if on {
            s += m;
        }
    }
    s + &match vk {
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        VK_TAB => "Tab".into(),
        VK_PRIOR => "PgUp".into(),
        VK_NEXT => "PgDn".into(),
        VK_LEFT => "Left".into(),
        VK_RIGHT => "Right".into(),
        VK_UP => "Up".into(),
        VK_DOWN => "Down".into(),
        VK_RETURN => "Enter".into(),
        VK_DELETE => "Del".into(),
        VK_INSERT => "Ins".into(),
        VK_HOME => "Home".into(),
        VK_END => "End".into(),
        VK_SPACE => "Space".into(),
        VK_BACK => "Backspace".into(),
        0xDC => "\\".into(),
        0xBC => ",".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xBA => ";".into(),
        0xDB => "[".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        0xC0 => "`".into(),
        0xBB => "=".into(),
        0xBD => "-".into(),
        _ => format!("#{vk}"),
    }
}
fn combo(vk: u16) -> String {
    combo_of(key_down(VK_CONTROL), key_down(VK_SHIFT), key_down(VK_MENU), vk)
}
/// The commands with a shortcut that can be changed: (id, default, label key).
const KEYS: [(&str, &str, &str); 18] = [
    ("new_tab", "Ctrl+T", "tab.new"),
    ("close_tab", "Ctrl+Shift+W", "tab.close"),
    ("close_pane", "Ctrl+W", "layout.close_pane"),
    ("reopen", "Ctrl+Shift+T", "tab.reopen"),
    ("next", "Ctrl+Tab", "keys.next"),
    ("prev", "Ctrl+Shift+Tab", "keys.prev"),
    ("move_left", "Ctrl+Shift+PgUp", "keys.move_left"),
    ("move_right", "Ctrl+Shift+PgDn", "keys.move_right"),
    ("duplicate", "Ctrl+Shift+D", "tab.duplicate"),
    ("unfuse", "Ctrl+Shift+U", "tab.unfuse"),
    ("rename", "Ctrl+F2", "tab.rename"),
    ("search", "Ctrl+Shift+A", "tabs.search"),
    ("split_right", "Ctrl+\\", "tab.split_right"),
    ("split_below", "Ctrl+Shift+\\", "tab.split_below"),
    ("zoom", "Ctrl+Shift+M", "layout.zoom"),
    ("copy_other", "Shift+F5", "layout.copy_other"),
    ("move_other", "Shift+F6", "layout.move_other"),
    ("result_tab", "Ctrl+Shift+Enter", "act.open_parent_tab"),
];

/// The names a split tab shows: each pane's; past 4, the first 3 and "+N" for the rest.
fn seg_names(panes: &[(usize, String)]) -> Vec<String> {
    let mut v: Vec<String> = panes.iter().map(|p| p.1.clone()).collect();
    if v.len() > 4 {
        let more = format!("+{}", v.len() - 3);
        v.truncate(3);
        v.push(more);
    }
    v
}
/// Widths for pieces wanting `want` in `room`: those that fit whole keep their width, the rest share what's left.
fn fair(want: &[i32], room: i32) -> Vec<i32> {
    let mut order: Vec<usize> = (0..want.len()).collect();
    order.sort_by_key(|&i| want[i]);
    let (mut out, mut left) = (vec![0; want.len()], room);
    for (n, &i) in order.iter().enumerate() {
        out[i] = want[i].min(left / (want.len() - n) as i32);
        left -= out[i];
    }
    out
}
/// `ws` grown to fill `room`: what's to spare shared evenly.
fn spread(mut ws: Vec<i32>, room: i32) -> Vec<i32> {
    let (n, spare) = (ws.len() as i32, room - ws.iter().sum::<i32>());
    if n > 0 && spare > 0 {
        for (j, w) in ws.iter_mut().enumerate() {
            *w += spare / n + ((j as i32) < spare % n) as i32;
        }
    }
    ws
}

/// How a tab is drawn: worked out from it and the prefs, also kept by a closing tab while it folds away.
pub(super) struct Face {
    pub(super) name: String,
    pub(super) split: bool, // its panes' names side by side ([`seg_names`]) instead of `name`
    pub(super) panes: Vec<(usize, String)>, // in `root`: each pane and its place's name
    pub(super) root: Node,
    pub(super) color: Option<u32>,
    pub(super) icon: Option<String>,
    pub(super) pic: Option<String>, // its place's shell icon (a source in `tab_bmp`), over `icon` once made
    pub(super) pinned: bool,
    pub(super) asleep: bool,
    pub(super) dot: bool,
    pub(super) busy: bool,
    pub(super) count: Option<usize>,
    pub(super) marks: String, // locked, muted, live
    pub(super) naming: bool,  // its title is being typed over: where it goes is noted, the field shows it
}

/// What the card over the window shows: a tab pointed at, the Ctrl+Tab switcher (tabs, the one picked), a note.
pub(super) enum Panel {
    Tab(usize),
    Mru(Vec<usize>, usize),
    Note(String),
}
enum CardRow {
    Title(String, Option<String>),
    Text(String),
    Shape(Node, Vec<(usize, String)>, u32),
    Pick(String, Option<String>, bool),
}

/// The strip laid out: tabs, group labels, a hole for a pane dragged in, panes listed under a split tab (left
/// strip); tabs hidden in a folded group (at their label); the +, the ▾ and ‹ › when the tabs don't fit; where
/// tabs show (the rest scrolled away); how tall it is; how far it scrolls.
enum Item {
    Tab(usize),
    Label(u64),
    Hole,
    Tree(usize, usize),
}
struct Flow {
    items: Vec<(Item, RECT)>,
    hidden: Vec<(usize, RECT)>,
    plus: RECT,
    list: Option<RECT>,
    arrows: Option<(RECT, RECT)>,
    clip: RECT,
    height: i32,
    max_scroll: i32,
}

/// Something to do to tabs: picked from a menu, or a key. Run by [`tab_act`], outside `with` (some ask first).
#[derive(Clone)]
pub(super) enum TA {
    Key(&'static str),
    Goto(usize),
    FocusN(usize),
    FocusDir(u8),
    MoveDir(u8),
    NewRight(usize),
    Duplicate(usize),
    Close(usize),
    CloseMany(usize, u8), // others, right, left
    Reopen(usize),
    Flag(usize, fn(&mut TabOpts) -> &mut bool),
    Scope(usize),
    Sleep(usize),
    Rename(usize),
    Color(usize, Option<u32>),
    ColorCustom(usize),
    ColorRule(usize),
    Icon(usize, Option<String>),
    IconCustom(usize),
    NewWindow(usize),
    ToWindow(usize, isize),
    SplitWith(usize, u8),
    Unfuse(usize),
    Fuse(usize, bool),
    CopyPath(usize),
    Explorer(usize),
    Terminal(usize),
    Favorite(usize),
    Props(usize),
    GroupNew(usize),
    GroupJoin(usize, u64),
    GroupLeave(usize),
    GroupFold(u64),
    GroupRename(u64),
    GroupColor(u64, u32),
    GroupClose(u64),
    GroupWindow(u64),
    GroupTab(u64),
    Ungroup(u64),
    Swap(usize),
    Rotate(Option<usize>),
    Equalize,
    Half(usize),
    Zoom(usize),
    Fold(usize),
    Preset(u8),
    CopyOther(bool),
    PaneTab(usize),
    ClosePane(usize),
    Search,
    WsSave,
    WsOpen(usize, bool),
    WsExport(usize),
    WsDelete(usize),
    WsImport,
    Merge,
    Window(isize),
    StartupSet,
    StartupClear,
    Pu(fn(&mut TabPrefs) -> &mut u8, u8), // a choice among a few
    Pb(fn(&mut TabPrefs) -> &mut bool),   // on / off
    Fused(u8),                            // a bit of `fused`
    SleepMax(usize),
    RuleDel(usize),
    Rebind(&'static str),
    KeysReset,
}

/// Menu entries being made, and what each does.
#[derive(Default)]
pub(super) struct Mb {
    acts: Vec<TA>,
}
impl Mb {
    fn it(&mut self, label: &str, keys: &str, on: bool, a: TA) -> MItem {
        self.acts.push(a);
        MItem { label: label.into(), keys: keys.into(), on, tag: self.acts.len() - 1, ..Default::default() }
    }
}
fn sub(label: &str, items: Vec<MItem>) -> MItem {
    MItem { label: label.into(), off: items.is_empty(), sub: items, ..Default::default() }
}
fn sep() -> MItem {
    MItem::default()
}
fn off(mut i: MItem, off: bool) -> MItem {
    i.off = off;
    i
}

/// Every Mosaic window but `me`, with its title.
pub(super) fn windows(me: HWND) -> Vec<(HWND, String)> {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let v = &mut *(l as *mut Vec<HWND>);
        let mut c = [0u16; 32];
        let n = GetClassNameW(h, c.as_mut_ptr(), 32);
        if String::from_utf16_lossy(&c[..n.max(0) as usize]) == "MosaicNative" {
            v.push(h);
        }
        1
    }
    let mut v: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(each), &mut v as *mut _ as LPARAM) };
    v.into_iter()
        .filter(|&h| h != me)
        .map(|h| {
            let mut b = [0u16; 256];
            let n = unsafe { GetWindowTextW(h, b.as_mut_ptr(), 256) };
            (h, String::from_utf16_lossy(&b[..n.max(0) as usize]))
        })
        .collect()
}
/// The Mosaic window (not `me`) at screen point `p`.
pub(super) fn window_at(me: HWND, p: POINT) -> Option<HWND> {
    let h = unsafe { GetAncestor(WindowFromPoint(p), GA_ROOT) };
    (!h.is_null() && h != me && windows(me).iter().any(|w| w.0 == h)).then_some(h)
}
/// Tabs to another Mosaic window (`all`: the whole of this one, which kept the session).
pub(super) unsafe fn send_tabs(to: HWND, json: &str, all: bool) -> bool {
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    let cd = COPYDATASTRUCT { dwData: if all { DATA_ALL } else { DATA_TABS }, cbData: json.len() as u32, lpData: json.as_ptr() as *mut c_void };
    let mut r = 0usize;
    SendMessageTimeoutW(to, WM_COPYDATA, 0, &cd as *const _ as LPARAM, SMTO_ABORTIFHUNG, 3000, &mut r) != 0
}
/// WM_COPYDATA: tabs from another window, taken in once the window is free to (`WM_TABS_IN`).
pub(super) unsafe fn copydata(h: HWND, l: LPARAM) -> bool {
    let cd = &*(l as *const windows_sys::Win32::System::DataExchange::COPYDATASTRUCT);
    if cd.dwData != DATA_TABS && cd.dwData != DATA_ALL || cd.lpData.is_null() {
        return false;
    }
    let s = String::from_utf8_lossy(std::slice::from_raw_parts(cd.lpData as *const u8, cd.cbData as usize)).into_owned();
    PostMessageW(h, WM_TABS_IN, (cd.dwData == DATA_ALL) as usize, Box::into_raw(Box::new(s)) as LPARAM);
    true
}
/// A new window with these tabs (a session as JSON), at `at` on screen.
pub(super) fn spawn_window(json: &str, at: Option<POINT>) -> bool {
    let mut args = vec!["--tabs-json".to_string(), json.to_string()];
    if let Some(p) = at {
        args.extend(["--at".to_string(), format!("{},{}", p.x, p.y)]);
    }
    if super::open(args) {
        return true; // a thread in this process (no file to pass it through)
    }
    let f = std::env::temp_dir().join(format!("mosaic-tabs-{}.json", new_uid()));
    let Ok(exe) = std::env::current_exe() else { return false };
    if std::fs::write(&f, json).is_err() {
        return false;
    }
    let mut c = std::process::Command::new(exe);
    c.arg("--native").arg("--tabs").arg(&f);
    if let Some(p) = at {
        c.arg("--at").arg(format!("{},{}", p.x, p.y));
    }
    c.spawn().is_ok()
}

/// `$e` over pane `$p`'s state, wherever it is: in the fields (the focused pane) or waiting in `panes`.
macro_rules! of_pane {
    ($u:expr, $p:expr, |$v:ident| $e:expr) => {
        if $p == $u.id {
            let $v = &*$u;
            Some($e)
        } else {
            $u.view($p).map(|$v| $e)
        }
    };
}

fn zero_y(n: &mut NodeSpec) {
    match n {
        NodeSpec::Leaf(p) => p.y = 0.0,
        NodeSpec::Split { a, b, .. } => {
            zero_y(a);
            zero_y(b);
        }
    }
}
/// Every split of `n` through `f` (side by side?, ratio).
fn each_split(n: &mut Node, f: &mut impl FnMut(&mut bool, &mut f64)) {
    if let Node::Split { vert, ratio, a, b, .. } = n {
        f(vert, ratio);
        each_split(a, f);
        each_split(b, f);
    }
}
/// Panes `x` and `y` trade places.
fn swap_leaves(n: &mut Node, x: usize, y: usize) {
    match n {
        Node::Leaf(l) if *l == x => *l = y,
        Node::Leaf(l) if *l == y => *l = x,
        Node::Leaf(_) => {}
        Node::Split { a, b, .. } => {
            swap_leaves(a, x, y);
            swap_leaves(b, x, y);
        }
    }
}
fn parent_dir(p: &str) -> Option<String> {
    std::path::Path::new(p).parent().map(|d| d.to_string_lossy().into_owned()).filter(|d| !d.is_empty())
}
fn is_dir(p: &str) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_dir())
}
fn grow(r: RECT, d: i32) -> RECT {
    RECT { left: r.left - d, top: r.top - d, right: r.right + d, bottom: r.bottom + d }
}
fn cut(r: RECT, c: RECT) -> RECT {
    RECT { left: r.left.max(c.left), top: r.top.max(c.top), right: r.right.min(c.right), bottom: r.bottom.min(c.bottom) }
}

impl Ui {
    // -- what a tab holds, as saved
    pub(super) fn pane_q(&self, p: usize) -> String {
        of_pane!(self, p, |v| v.args.q.clone()).unwrap_or_default()
    }
    fn pane_busy(&self, p: usize) -> bool {
        of_pane!(self, p, |v| v.busy.is_some_and(|b| b.elapsed() > Duration::from_millis(150))) == Some(true)
    }
    pub(super) fn pane_dir(&self, p: usize) -> Option<String> {
        listed_dir(&self.pane_q(p))
    }
    fn pane_spec(&self, p: usize) -> PaneSpec {
        of_pane!(self, p, |v| {
            let h = v.hist.borrow();
            let skip = h.0.len().saturating_sub(50);
            PaneSpec { q: v.args.q.clone(), hist: h.0[skip..].to_vec(), at: h.1.saturating_sub(skip), grid: v.grid, size: v.size, sort: v.args.sort.clone(), dir: v.args.dir.clone(), cat: v.args.cat.clone(), y: v.y }
        })
        .unwrap_or_default()
    }
    fn node_spec(&self, n: &Node) -> NodeSpec {
        match n {
            Node::Leaf(p) => NodeSpec::Leaf(self.pane_spec(*p)),
            Node::Split { vert, ratio, a, b, .. } => NodeSpec::Split { vert: *vert, ratio: *ratio, a: Box::new(self.node_spec(a)), b: Box::new(self.node_spec(b)) },
        }
    }
    pub(super) fn tab_spec(&self, k: usize) -> TabSpec {
        let t = &self.tabs[k];
        match &t.o.asleep {
            Some((root, focus)) => TabSpec { root: root.clone(), focus: *focus, o: t.o.clone() },
            None => TabSpec { root: self.node_spec(&t.root), focus: t.root.leaves().iter().position(|&p| p == t.focus).unwrap_or(0), o: t.o.clone() },
        }
    }
    fn session_of(&self, ks: &[usize]) -> Session {
        let groups = self.groups.iter().filter(|g| ks.iter().any(|&k| self.tabs[k].o.group == Some(g.id))).cloned().collect();
        Session { tabs: ks.iter().map(|&k| self.tab_spec(k)).collect(), cur: ks.iter().position(|&k| k == self.cur).unwrap_or(0), groups }
    }
    pub(super) fn session_json(&self, scroll: bool) -> String {
        let mut s = self.session_of(&(0..self.tabs.len()).collect::<Vec<_>>());
        if !scroll {
            s.tabs.iter_mut().for_each(|t| zero_y(&mut t.root));
        }
        serde_json::to_string(&s).unwrap_or_default()
    }
    pub(super) fn json_of(&self, ks: &[usize]) -> String {
        serde_json::to_string(&self.session_of(ks)).unwrap_or_default()
    }
    /// A pane as saved, made again: its history, view and sort, scrolled where it was once its listing lands.
    fn spawn(&mut self, s: &PaneSpec) -> usize {
        let id = self.new_pane_idle(&s.q);
        self.with_pane(id, |u| {
            if !s.hist.is_empty() {
                *u.hist.borrow_mut() = (s.hist.clone(), s.at.min(s.hist.len() - 1));
            }
            if s.size > 0 {
                (u.grid, u.size) = (s.grid, s.size.clamp(MIN_SIZE, MAX_SIZE));
            }
            if !s.sort.is_empty() {
                (u.args.sort, u.args.dir) = (s.sort.clone(), s.dir.clone());
            }
            u.args.cat = s.cat.clone();
            u.want_y = (s.y > 0.0).then_some(s.y);
            u.run(false);
        });
        id
    }
    fn build(&mut self, s: &NodeSpec) -> Node {
        match s {
            NodeSpec::Leaf(p) => Node::Leaf(self.spawn(p)),
            NodeSpec::Split { vert, ratio, a, b } => {
                let (a, b) = (self.build(a), self.build(b));
                let r = ratio.clamp(0.05, 0.95);
                Node::Split { id: self.next_split(), vert: *vert, ratio: r, shown: r, vel: 0.0, a: Box::new(a), b: Box::new(b) }
            }
        }
    }
    /// A sleeping tab's panes made again (it's about to show).
    pub(super) fn wake(&mut self, k: usize) {
        let Some((root, focus)) = self.tabs.get_mut(k).and_then(|t| t.o.asleep.take()) else { return };
        let r = self.build(&root);
        let leaves = r.leaves();
        let t = &mut self.tabs[k];
        t.focus = leaves.get(focus).copied().unwrap_or(leaves[0]);
        t.root = r;
    }
    /// Tab `k`'s panes let go of, kept as a spec till it's shown again.
    fn sleep_tab(&mut self, k: usize) {
        if k == self.cur || self.tabs[k].o.asleep.is_some() {
            return;
        }
        let s = self.tab_spec(k);
        let t = &mut self.tabs[k];
        t.o.asleep = Some((s.root, s.focus));
        t.focus = usize::MAX;
        let old = std::mem::replace(&mut t.root, Node::Leaf(usize::MAX));
        for p in old.leaves() {
            self.drop_pane(p);
        }
    }
    /// At most `tp.sleep` tabs awake: the ones used longest ago sleep (never the one shown, a pinned or a live one).
    fn auto_sleep(&mut self) {
        while self.tp.sleep > 0 && self.tabs.iter().filter(|t| t.o.asleep.is_none()).count() > self.tp.sleep {
            let Some(k) = (0..self.tabs.len()).filter(|&k| k != self.cur && self.tabs[k].o.asleep.is_none() && !self.tabs[k].o.pinned && !self.tabs[k].o.live).min_by_key(|&k| self.tabs[k].used) else { break };
            self.sleep_tab(k);
        }
    }
    /// Tab `cur` has just been shown (the one before it was `was`).
    pub(super) fn shown_tab(&mut self, was: usize) {
        self.tick_n += 1;
        let (n, now) = (self.tick_n, now_secs());
        if let Some(t) = self.tabs.get_mut(was) {
            t.o.seen = now;
        }
        let t = &mut self.tabs[self.cur];
        (t.used, t.o.dot, t.o.seen) = (n, false, now);
        if let Some(g) = t.o.group.and_then(|g| self.groups.iter_mut().find(|x| x.id == g)) {
            g.folded = false;
        }
        self.auto_sleep();
        self.title();
        self.card_close();
    }
    /// The window's title: the tab shown's name (the taskbar and Alt+Tab show it).
    pub(super) fn title(&self) {
        super::side::current(&self.focus_q(self.cur));
        let s = super::save::title().unwrap_or_else(|| format!("{} — Mosaic", self.tab_name(self.cur)));
        let mut b = [0u16; 512];
        let n = unsafe { GetWindowTextW(self.main, b.as_mut_ptr(), 512) };
        if String::from_utf16_lossy(&b[..n.max(0) as usize]) != s {
            unsafe { SetWindowTextW(self.main, wz(&s).as_ptr()) };
        }
    }

    // -- sessions
    /// At start: the tabs sent from another window (`--tabs`), else the last session's, else the startup folders.
    pub(super) fn restore(&mut self) {
        let args = super::args();
        let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
        if let Some((x, y)) = arg("--at").and_then(|s| {
            let (a, b) = s.split_once(',')?;
            Some((a.parse::<i32>().ok()?, b.parse::<i32>().ok()?))
        }) {
            unsafe { SetWindowPos(self.main, null_mut(), x - self.px(200), y - self.px(16), 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
        }
        let s: Option<Session> = if let Some(d) = super::save::start_dir() {
            self.secondary = true; // a Save window: its own folder, not the session's
            Some(Session { tabs: vec![TabSpec::folder(&self.browse_q(&d))], ..Default::default() })
        } else if let Some(d) = arg("--open") {
            self.secondary = true; // a folder opened from the desktop or elsewhere, in place of Explorer
            Some(Session { tabs: vec![TabSpec::folder(&self.browse_q(&d))], ..Default::default() })
        } else if let Some(j) = arg("--tabs-json") {
            self.secondary = true;
            serde_json::from_str(&j).ok()
        } else if let Some(f) = arg("--tabs") {
            self.secondary = true;
            let s = std::fs::read_to_string(&f).ok().and_then(|s| serde_json::from_str(&s).ok());
            let _ = std::fs::remove_file(&f);
            s
        } else {
            let one = |q: String| Some(Session { tabs: vec![TabSpec::folder(&q)], ..Default::default() });
            match self.tp.start {
                0 => std::fs::read_to_string(session_path()).ok().and_then(|s| serde_json::from_str(&s).ok()),
                1 => super::side::desktop().and_then(|d| one(self.browse_q(&d))),
                2 => one("home:".into()),
                4 if !self.tp.startup.is_empty() => Some(Session { tabs: self.tp.startup.iter().map(|d| TabSpec::folder(&self.browse_q(d))).collect(), ..Default::default() }),
                _ => one(String::new()),
            }
        };
        if let Some(s) = s.filter(|s| !s.tabs.is_empty()) {
            self.load_session(s, true);
        }
        self.saved = self.session_json(false);
        WRITTEN.set(self.session_json(true));
        self.title();
    }
    /// `s`'s tabs after these (sleeping till shown), its current one shown; `replace`: instead of these.
    pub(super) fn load_session(&mut self, s: Session, replace: bool) {
        if s.tabs.is_empty() {
            return;
        }
        let base = self.tabs.len();
        for g in s.groups {
            if !self.groups.iter().any(|x| x.id == g.id) {
                self.groups.push(g);
            }
        }
        let n = s.tabs.len();
        self.tabs.extend(s.tabs.into_iter().map(Tab::asleep));
        self.switch_tab(base + s.cur.min(n - 1));
        if replace {
            let old: Vec<Tab> = self.tabs.drain(..base).collect();
            self.cur -= base;
            for t in old {
                for p in t.root.leaves() {
                    self.drop_pane(p);
                }
            }
        }
        self.order_tabs();
        self.layout();
    }
    /// The session to disk when its tabs changed (not for scrolling alone), or `force` (closing: with where each
    /// pane was scrolled to). The same text twice is never written.
    pub(super) fn autosave(&mut self, force: bool) {
        if self.secondary || self.merged || self.tp.start != 0 {
            return;
        }
        let now = self.session_json(false);
        if now == self.saved && !force {
            return;
        }
        self.saved = now;
        let full = self.session_json(true);
        if WRITTEN.with(|w| *w.borrow() == full) {
            return;
        }
        let _ = std::fs::create_dir_all(app_dir());
        if std::fs::write(session_path(), &full).is_ok() {
            WRITTEN.set(full);
        }
    }

    // -- opening, closing, moving tabs
    /// Close tab `k` (`remember`: Reopen can bring it back); the last one closes the window, as Explorer's does.
    pub(super) fn drop_tab(&mut self, k: usize, remember: bool) {
        if self.tabs.len() == 1 {
            unsafe { PostMessageW(self.main, WM_CLOSE, 0, 0) };
            return;
        }
        if k == self.cur {
            self.switch_tab(if k + 1 < self.tabs.len() { k + 1 } else { k - 1 }); // the one to its right, else its left
        }
        if remember {
            self.closed.push((self.tab_spec(k), k));
            if self.closed.len() > 25 {
                self.closed.remove(0);
            }
        }
        // it folds away where it is while the ones after it slide over its place
        let r = self.drawn(k);
        if r.right > r.left && !self.tp.still {
            self.ghosts.push(Ghost { r, face: self.face(k), at: Instant::now() });
        }
        let t = self.tabs.remove(k);
        if k < self.cur {
            self.cur -= 1;
        }
        for p in t.root.leaves() {
            self.drop_pane(p);
        }
        self.order_tabs();
        self.layout();
        self.invalidate();
    }
    /// A closed tab back where it was (`i`: which, newest when None).
    pub(super) fn reopen(&mut self, i: Option<usize>) {
        let i = i.unwrap_or(self.closed.len().wrapping_sub(1));
        if i >= self.closed.len() {
            return;
        }
        let (mut s, at) = self.closed.remove(i);
        if s.o.group.is_some_and(|g| !self.groups.iter().any(|x| x.id == g)) {
            s.o.group = None;
        }
        self.insert_tab(Tab::asleep(s), at, true);
    }
    /// `t` in the strip at `at`, shown (`show`) or left waiting.
    fn insert_tab(&mut self, mut t: Tab, at: usize, show: bool) {
        let at = at.min(self.tabs.len());
        t.born = (!self.tp.still).then(Instant::now);
        self.tabs.insert(at, t);
        if at <= self.cur {
            self.cur += 1;
        }
        if show {
            self.switch_tab(at);
        }
        self.order_tabs();
        self.layout();
    }
    /// `q` in a new tab at `at` that waits behind this one, as a browser's middle-click opens a link.
    pub(super) fn new_tab_bg(&mut self, q: &str, at: usize) {
        let mut s = TabSpec::folder(q);
        s.o.group = self.tabs[self.cur].o.group.filter(|_| at == self.cur + 1);
        self.insert_tab(Tab::asleep(s), at, false);
    }
    fn duplicate(&mut self, k: usize) {
        let mut s = self.tab_spec(k);
        s.o.pinned = false;
        self.insert_tab(Tab::asleep(s), k + 1, true);
    }
    fn move_tab(&mut self, k: usize, d: isize) {
        let j = k as isize + d;
        if j < 0 || j >= self.tabs.len() as isize {
            return;
        }
        let uid = self.tabs[self.cur].uid;
        self.tabs.swap(k, j as usize);
        self.cur = self.tabs.iter().position(|t| t.uid == uid).unwrap_or(0);
        self.regroup(j as usize);
        self.order_tabs();
        self.layout();
    }
    /// Pinned tabs first, each group's tabs together where its first one is; groups with no tab left go.
    pub(super) fn order_tabs(&mut self) {
        let uid = self.tabs.get(self.cur).map(|t| t.uid);
        let mut v = std::mem::take(&mut self.tabs);
        v.sort_by_key(|t| !t.o.pinned);
        let mut out: Vec<Tab> = Vec::with_capacity(v.len());
        for t in v {
            match t.o.group.and_then(|g| out.iter().rposition(|x| x.o.group == Some(g))) {
                Some(i) => out.insert(i + 1, t),
                None => out.push(t),
            }
        }
        self.tabs = out;
        self.cur = self.tabs.iter().position(|t| Some(t.uid) == uid).unwrap_or(0);
        let tabs = &self.tabs;
        self.groups.retain(|g| tabs.iter().any(|t| t.o.group == Some(g.id)));
    }
    /// Tab `k` was dropped where it is: in the group around it, or out of one it left.
    pub(super) fn regroup(&mut self, k: usize) {
        let g = |i: Option<usize>| i.and_then(|i| self.tabs.get(i)).and_then(|t| t.o.group);
        let (l, r, mine) = (g(k.checked_sub(1)), g(Some(k + 1)), self.tabs[k].o.group);
        self.tabs[k].o.group = if mine.is_some() && (l == mine || r == mine) { mine } else if l.is_some() && l == r { l } else { None };
    }
    /// Group `g`'s tabs moved to before the `j`th of the others (never into another group).
    pub(super) fn move_group(&mut self, g: u64, j: usize) {
        let uid = self.tabs[self.cur].uid;
        let (mine, mut rest): (Vec<Tab>, Vec<Tab>) = std::mem::take(&mut self.tabs).into_iter().partition(|t| t.o.group == Some(g));
        let mut j = j.min(rest.len());
        while j > 0 && j < rest.len() && rest[j].o.group.is_some() && rest[j].o.group == rest[j - 1].o.group {
            j += 1;
        }
        rest.splice(j..j, mine);
        self.tabs = rest;
        self.cur = self.tabs.iter().position(|t| t.uid == uid).unwrap_or(0);
    }
    /// A folder opened from this tab: here, or in a new tab when it's locked.
    pub(super) fn go_dir(&mut self, d: &str) {
        if self.tabs[self.cur].o.locked {
            self.open_tab(d);
        } else {
            self.browse_to(d);
        }
    }
    /// `dir` in a new pane on `side` of the focused one (0 left, 1 right, 2 top, 3 bottom), focused.
    pub(super) fn open_beside(&mut self, dir: &str, side: u8) {
        let q = self.browse_q(dir);
        let id = self.new_pane(&q);
        let sid = self.next_split();
        let me = self.id;
        self.tabs[self.cur].root.insert(me, side, &mut Some(Node::Leaf(id)), sid);
        self.activate(id);
        unsafe { SetFocus(self.list) };
    }
    /// Pane `p` (any, shown or not) taken to folder `d`, into its history.
    pub(super) fn take_to(&mut self, p: usize, d: &str) {
        self.with_pane(p, |u| {
            let q = u.browse_q(d);
            if q != u.args.q {
                u.visit(&q);
                u.args.q = q;
                u.run(false);
                if !u.guest {
                    unsafe { SetWindowTextW(u.edit, wz(&u.args.q).as_ptr()) };
                }
            }
        });
        self.invalidate_all();
    }

    // -- how a tab looks
    pub(super) fn tab_name(&self, k: usize) -> String {
        match &self.tabs[k].o.title {
            Some(s) => s.clone(),
            None => self.qname(&self.focus_q(k)),
        }
    }
    /// What tab `k`'s focused pane lists.
    /// Tab `k`'s place's shell icon source: none when its icon is the user's own, or icons are off.
    fn tab_pic(&self, k: usize) -> Option<String> {
        (self.tabs[k].o.icon.is_none() && self.tp.icons).then(|| icon_src(&self.focus_q(k))).flatten()
    }
    /// Make the tabs' icons for this canvas, asking the shell helper for those not had yet.
    pub(super) fn tab_icons(&mut self) {
        if self.tab_png.len() > 300 {
            (self.tab_png, self.tab_bmp) = (HashMap::new(), HashMap::new());
        }
        let (gen, s) = (self.cm.gen, self.px(16));
        for k in 0..self.tabs.len() {
            let Some(src) = self.tab_pic(k) else { continue };
            if self.tab_bmp.get(&src).is_some_and(|b| b.1 == gen) {
                continue;
            }
            match self.tab_png.get(&src) {
                Some(Some(png)) => {
                    if let Some(bm) = png_pixels(png, s).and_then(|(px, w, h)| self.cm.make_bitmap(&px, w, h)) {
                        self.tab_bmp.insert(src, (bm, gen));
                    }
                }
                Some(None) => {}
                None => {
                    let (hw, size, key) = (self.main as isize, self.px(48), src.clone()); // 3x, box-filtered down: sharper
                    self.tab_png.insert(src, None);
                    shell::shell_spawn(Box::new(move |dropped| {
                        let png = if dropped { None } else { shell::thumbnail(&key, size, true) };
                        let b: Box<(String, Option<Vec<u8>>)> = Box::new((key, png));
                        unsafe { PostMessageW(hw as HWND, WM_TAB_ICON, 0, Box::into_raw(b) as isize) };
                    }));
                }
            }
        }
    }
    pub(super) fn got_tab_icon(&mut self, (src, png): (String, Option<Vec<u8>>)) {
        if png.is_some() {
            self.tab_bmp.remove(&src);
            self.tab_png.insert(src, png);
            unsafe { InvalidateRect(self.main, null(), 0) };
        }
    }
    pub(super) fn focus_q(&self, k: usize) -> String {
        let t = &self.tabs[k];
        match &t.o.asleep {
            Some((r, f)) => {
                let l = r.leaves();
                l.get(*f).or(l.first()).map_or(String::new(), |p| p.q.clone())
            }
            None => self.pane_q(t.focus),
        }
    }
    pub(super) fn qname(&self, q: &str) -> String {
        match listed_dir(q).filter(|_| self.tp.path) {
            Some(d) => d,
            None => place_name(q),
        }
    }
    /// Its own colour, else its group's, a rule's, its drive's.
    pub(super) fn tab_color(&self, k: usize) -> Option<u32> {
        let t = self.tabs.get(k)?;
        let dir = || listed_dir(&self.focus_q(k)).unwrap_or_default();
        t.o.color
            .or_else(|| t.o.group.and_then(|g| self.groups.iter().find(|x| x.id == g)).map(|g| g.color))
            .or_else(|| rule_color(&self.tp.rules, &dir()))
            .or_else(|| self.tp.drive_color.then(|| drive_color(&dir())).flatten())
    }
    /// How many names tab `k` shows side by side: each pane's, when split tabs show them (at most 4, see [`seg_names`]).
    fn names_shown(&self, k: usize) -> usize {
        let t = &self.tabs[k];
        if self.tp.fused & 2 == 0 || t.o.title.is_some() || t.o.pinned || self.tp.pos == 2 {
            return 1;
        }
        let n = match &t.o.asleep {
            Some((r, _)) => r.leaves().len(),
            None => t.root.leaves().len(),
        };
        n.clamp(1, 4)
    }
    pub(super) fn face(&self, k: usize) -> Face {
        let t = &self.tabs[k];
        let (root, panes): (Node, Vec<(usize, String)>) = match &t.o.asleep {
            Some((r, _)) => (r.shape(&mut 0), r.leaves().iter().enumerate().map(|(i, p)| (i, self.qname(&p.q))).collect()),
            None => (t.root.clone(), t.root.leaves().into_iter().map(|p| (p, self.qname(&self.pane_q(p)))).collect()),
        };
        let awake = t.o.asleep.is_none();
        let marks: String = [(t.o.locked, "🔒"), (t.o.muted, "🔕"), (t.o.live, "⚡"), (t.o.scope.is_some(), "◎")].iter().filter(|m| m.0).map(|m| m.1).collect();
        let split = self.names_shown(k) > 1;
        let name = if panes.len() > 1 && self.tp.fused & 2 != 0 && t.o.title.is_none() { panes.iter().map(|p| p.1.as_str()).collect::<Vec<_>>().join(" | ") } else { self.tab_name(k) };
        Face {
            name,
            split,
            panes,
            root,
            color: self.tab_color(k),
            icon: t.o.icon.clone().or_else(|| self.tp.icons.then(|| kind_icon(&self.focus_q(k)).to_string())),
            pic: self.tab_pic(k),
            pinned: t.o.pinned,
            asleep: !awake,
            dot: self.tp.dots && t.o.dot,
            busy: self.tp.busy && awake && t.root.leaves().iter().any(|&p| self.pane_busy(p)),
            count: (self.tp.count && awake).then(|| of_pane!(self, t.focus, |v| v.matches).unwrap_or(0)),
            marks,
            naming: self.naming == Some(k),
        }
    }
    /// Any tab's spinner turning.
    pub(super) fn spinning(&self) -> bool {
        self.tp.busy && self.tabs.iter().any(|t| t.o.asleep.is_none() && t.root.leaves().iter().any(|&p| self.pane_busy(p)))
    }
    /// Where tab `k` is drawn now (its slot, as far as it has slid there).
    pub(super) fn drawn(&self, k: usize) -> RECT {
        let Some(&r) = self.slot_rs.get(k) else { return unsafe { zeroed() } };
        let t = &self.tabs[k];
        let d = if t.x.is_finite() { (t.x - t.to).round() as i32 } else { 0 };
        if self.tp.pos == 2 {
            RECT { top: r.top + d, bottom: r.bottom + d, ..r }
        } else {
            RECT { left: r.left + d, right: r.right + d, ..r }
        }
    }
}

// -- the strip
impl Ui {
    fn tab_font(&self) -> (&Format, &Format, i32) {
        let f = &self.gfx.f.tabs[self.tp.font.min(2) as usize];
        (&f.0, &f.1, f.2)
    }
    /// A tab row's height.
    pub(super) fn tab_h(&self) -> i32 {
        self.tab_font().2 + self.px(14)
    }
    /// What each tab's button is, in strip order: its group's label first, then it (or nothing: folded away).
    fn strip_items(&self) -> Vec<Item> {
        let mut v = Vec::new();
        for k in 0..self.tabs.len() {
            if self.hole == Some(k) {
                v.push(Item::Hole);
            }
            let g = self.tabs[k].o.group;
            if let Some(g) = g.filter(|&g| k == 0 || self.tabs[k - 1].o.group != Some(g)) {
                v.push(Item::Label(g));
            }
            v.push(Item::Tab(k));
        }
        if self.hole.is_some_and(|h| h >= self.tabs.len()) {
            v.push(Item::Hole);
        }
        v
    }
    fn group(&self, g: u64) -> Option<&Group> {
        self.groups.iter().find(|x| x.id == g)
    }
    fn folded_away(&self, k: usize) -> bool {
        k != self.cur && self.tabs[k].o.group.and_then(|g| self.group(g)).is_some_and(|g| g.folded)
    }
    fn label_text(&self, g: &Group) -> String {
        let name = if g.name.is_empty() { "•".to_string() } else { g.name.clone() };
        if g.folded {
            format!("{name} {}", self.tabs.iter().filter(|t| t.o.group == Some(g.id)).count())
        } else {
            name
        }
    }
    /// The strip laid out in `area` (its top-left, and how much room: the height only bounds a left strip).
    fn flow(&self, area: RECT) -> Flow {
        let px = |v: i32| self.px(v);
        let (fr, _, _) = self.tab_font();
        let rh = self.tab_h();
        let gap = px(2);
        let items = self.strip_items();
        let maxw = px([140, 220, 300][self.tp.width.min(2) as usize]);
        let label_w = |g: u64| self.group(g).map_or(px(30), |g| self.gfx.width(&self.label_text(g), fr) + px(24));
        let mut f = Flow { items: Vec::new(), hidden: Vec::new(), plus: area, list: None, arrows: None, clip: area, height: rh, max_scroll: 0 };
        if self.tp.pos == 2 {
            // down the left side: a row each, a split tab's panes under it
            let (x0, x1) = (area.left + px(6), area.right - px(6));
            let mut y = area.top + px(6);
            let mut label_at: HashMap<u64, RECT> = HashMap::new();
            for it in items {
                let r = RECT { left: x0, top: y, right: x1, bottom: y + rh };
                match it {
                    Item::Tab(k) if self.folded_away(k) => {
                        let lr = self.tabs[k].o.group.and_then(|g| label_at.get(&g).copied()).unwrap_or(r);
                        f.hidden.push((k, lr));
                        continue;
                    }
                    Item::Tab(k) => {
                        let indent = if self.tabs[k].o.group.is_some() { px(10) } else { 0 };
                        f.items.push((Item::Tab(k), RECT { left: x0 + indent, ..r }));
                        y += rh + gap;
                        let t = &self.tabs[k];
                        let leaves = t.root.leaves();
                        if t.o.asleep.is_none() && leaves.len() > 1 {
                            for p in leaves {
                                let rr = RECT { left: x0 + indent + px(22), top: y, right: x1, bottom: y + rh - px(6) };
                                f.items.push((Item::Tree(k, p), rr));
                                y += rh - px(6);
                            }
                            y += gap;
                        }
                        continue;
                    }
                    Item::Label(g) => {
                        label_at.insert(g, r);
                    }
                    Item::Hole | Item::Tree(..) => {}
                }
                f.items.push((it, r));
                y += rh + gap;
            }
            f.plus = RECT { left: x0, top: y, right: x0 + px(28), bottom: y + px(28) };
            y += px(28) + px(6);
            f.height = y - area.top;
            f.max_scroll = (f.height - (area.bottom - area.top)).max(0);
            return f;
        }
        // along the top or bottom
        let pinw = px(40);
        let width_of = |it: &Item, tw: i32| match it {
            Item::Tab(k) if self.tabs[*k].o.pinned => pinw,
            Item::Tab(_) => tw, // a split tab too: its panes' names share the width
            Item::Hole => tw,
            Item::Label(g) => label_w(*g),
            Item::Tree(..) => 0,
        };
        let shown = |it: &Item| !matches!(it, Item::Tab(k) if self.folded_away(*k));
        // along the top, the current tab's bottom corners flare out (see `Canvas::tab`): room for the first one's
        let x0 = area.left + if self.tp.pos == 0 { px(8) } else { 0 };
        let total = |tw: i32| items.iter().filter(|i| shown(i)).map(|i| width_of(i, tw) + gap).sum::<i32>() + px(6 + 28) + x0 - area.left;
        let room = area.right - area.left;
        let mut tw = maxw;
        let mut scroll = false;
        if total(tw) > room {
            match self.tp.fit {
                0 => {
                    let fixed = total(0); // labels, pinned tabs, gaps and the +
                    let n: f64 = items
                        .iter()
                        .map(|i| match i {
                            Item::Hole => 1.0,
                            Item::Tab(k) if shown(i) && !self.tabs[*k].o.pinned => 1.0,
                            _ => 0.0,
                        })
                        .sum();
                    tw = (((room - fixed) as f64 / n.max(1.0)) as i32).min(maxw);
                    if tw < px(56) {
                        tw = px(56).max(maxw / 2);
                        scroll = true;
                    }
                }
                1 => scroll = true,
                _ => {}
            }
        }
        let ctl = px(26);
        let right = if scroll { area.right - 4 * ctl } else { area.right };
        let (mut x, mut y) = (x0, area.top);
        let mut label_at: HashMap<u64, RECT> = HashMap::new();
        for it in items {
            if let Item::Tab(k) = it {
                if self.folded_away(k) {
                    let lr = self.tabs[k].o.group.and_then(|g| label_at.get(&g).copied()).unwrap_or(RECT { left: x, top: y, right: x, bottom: y + rh });
                    f.hidden.push((k, lr));
                    continue;
                }
            }
            let wd = width_of(&it, tw);
            if self.tp.fit == 2 && x > x0 && x + wd > right - px(34) {
                (x, y) = (x0, y + rh + gap);
            }
            let r = RECT { left: x, top: y, right: x + wd, bottom: y + rh };
            if let Item::Label(g) = it {
                label_at.insert(g, r);
            }
            f.items.push((it, r));
            x += wd + gap;
        }
        f.height = y + rh - area.top;
        if scroll {
            f.clip = RECT { right, top: area.top, bottom: area.top + rh, ..area };
            f.max_scroll = (x - right).max(0);
            let cy = area.top + (rh - ctl) / 2;
            let b = |i: i32| RECT { left: right + i * ctl, top: cy, right: right + (i + 1) * ctl, bottom: cy + ctl };
            f.arrows = Some((b(0), b(1)));
            f.list = Some(b(2));
            f.plus = b(3);
        } else {
            f.clip = RECT { bottom: area.top + f.height, ..area };
            let ns = px(28);
            let ny = y + (rh - ns) / 2;
            f.plus = RECT { left: x + px(4), top: ny, right: x + px(4) + ns, bottom: ny + ns };
        }
        f
    }
    /// How tall the strip is in `area`.
    pub(super) fn strip_height(&self, area: RECT) -> i32 {
        self.flow(area).height
    }
    /// The strip in `area`: each tab's slot, its springs' targets, and what can be clicked.
    pub(super) fn place_strip(&mut self, area: RECT, hits: &mut Vec<(RECT, Hit)>) -> i32 {
        let f = self.flow(area);
        let vert = self.tp.pos == 2;
        let off = (self.scroll.0.round() as i32).clamp(0, f.max_scroll);
        self.scroll = (off as f64, f.max_scroll);
        let mv = |r: RECT| if vert { RECT { top: r.top - off, bottom: r.bottom - off, ..r } } else { RECT { left: r.left - off, right: r.right - off, ..r } };
        let resized = (self.strip_r.left, self.strip_r.top, self.strip_r.right, self.strip_r.bottom) != (f.clip.left, f.clip.top, f.clip.right, f.clip.bottom);
        self.strip_r = f.clip;
        let old = std::mem::replace(&mut self.slot_rs, vec![unsafe { zeroed() }; self.tabs.len()]);
        let clip = f.clip;
        let scale = self.scale;
        let px = move |v: i32| (v as f64 * scale).round() as i32;
        let main = |r: &RECT| if vert { r.top } else { r.left } as f64;
        let cross = |r: &RECT| if vert { r.left } else { r.top };
        let set = |u: &mut Ui, k: usize, r: RECT| {
            u.slot_rs[k] = r;
            let t = &mut u.tabs[k];
            t.to = main(&r);
            if resized || u.tp.still || t.x.is_nan() || old.get(k).is_none_or(|o| cross(o) != cross(&r)) {
                (t.x, t.v) = (t.to, 0.0);
            }
        };
        for (it, r) in f.items {
            let r = mv(r);
            let vis = cut(r, clip);
            let seen = vis.right > vis.left && vis.bottom > vis.top;
            match it {
                Item::Tab(k) => {
                    set(self, k, r);
                    if seen {
                        let t = &self.tabs[k];
                        let narrow = r.right - r.left < px(80) && k != self.cur;
                        if self.tp.close != 2 && !t.o.pinned && !narrow {
                            hits.push((cut(tab_x(&RECT { bottom: r.bottom - if vert { 0 } else { px(4) }, ..r }, px(20)), clip), Hit::TabClose(k)));
                        }
                        hits.push((vis, Hit::Tab(k)));
                    }
                }
                Item::Label(g) if seen => hits.push((vis, Hit::Group(g))),
                Item::Hole => self.hole_r = r,
                Item::Tree(k, p) if seen => hits.push((vis, Hit::TreePane(k, p))),
                _ => {}
            }
        }
        for (k, r) in f.hidden {
            set(self, k, mv(r));
        }
        let plus = if f.arrows.is_some() { f.plus } else { mv(f.plus) };
        self.plus.2 = main(&plus);
        if resized || self.tp.still || self.plus.0.is_nan() {
            (self.plus.0, self.plus.1) = (self.plus.2, 0.0);
        }
        if f.arrows.is_some() || cut(plus, clip).right > cut(plus, clip).left {
            hits.push((plus, Hit::NewTab));
        }
        if let Some((a, b)) = f.arrows {
            hits.push((a, Hit::Scroll(false)));
            hits.push((b, Hit::Scroll(true)));
        }
        if let Some(l) = f.list {
            hits.push((l, Hit::TabList));
        }
        f.height
    }
    /// Where in the strip a drop at (x, y) goes: how many of the tabs `count` picks are before it.
    pub(super) fn strip_index(&self, x: i32, y: i32, count: impl Fn(usize) -> bool) -> usize {
        let vert = self.tp.pos == 2;
        let key = |r: &RECT| if vert { (0, (r.top + r.bottom) / 2) } else { (r.top, (r.left + r.right) / 2) };
        let pt = if vert {
            (0, y)
        } else {
            let row = self.slot_rs.iter().map(|r| r.top).filter(|&t| t <= y).max().or_else(|| self.slot_rs.iter().map(|r| r.top).min()).unwrap_or(0);
            (row, x)
        };
        self.slot_rs.iter().enumerate().filter(|&(k, r)| count(k) && key(r) < pt).count()
    }
    /// Over the strip (a little around it too).
    pub(super) fn on_strip(&self, x: i32, y: i32) -> bool {
        inside(&grow(self.strip_r, self.px(6)), x, y)
    }
    /// Scroll the strip by `d` (pixels).
    pub(super) fn scroll_strip(&mut self, d: i32) {
        let to = (self.scroll.0 + d as f64).clamp(0.0, self.scroll.1 as f64);
        if to != self.scroll.0 {
            self.scroll.0 = to;
            for t in &mut self.tabs {
                t.x = f64::NAN;
            }
            self.layout();
        }
    }
    /// The strip scrolled so tab `k` shows.
    fn reveal_tab(&mut self, k: usize) {
        let (Some(&r), c) = (self.slot_rs.get(k), self.strip_r) else { return };
        let vert = self.tp.pos == 2;
        let (a, b, ca, cb) = if vert { (r.top, r.bottom, c.top, c.bottom) } else { (r.left, r.right, c.left, c.right) };
        if a < ca {
            self.scroll_strip(a - ca);
        } else if b > cb {
            self.scroll_strip(b - cb);
        }
    }

    // -- painting the strip
    pub(super) fn paint_strip(&self) {
        let cv = &self.cm;
        let px = |v: i32| self.px(v);
        let rad = |v: f64| (v * self.scale) as f32;
        let (fr, _, flh) = self.tab_font();
        let vert = self.tp.pos == 2;
        let c = self.strip_r;
        if vert {
            let panel = RECT { left: 0, top: self.head_h, right: c.right, bottom: self.list_bottom() };
            cv.fill(&panel, theme().card, 1.0);
            cv.fill(&RECT { left: panel.right - 1, ..panel }, theme().line, 1.0);
        }
        // the strip's row (its column, down the side), + and arrows included
        let (far, g2) = (i32::MAX / 2, px(2));
        let lane = if vert { RECT { left: c.left - g2, right: c.right + g2, top: -far, bottom: far } } else { RECT { top: c.top - g2, bottom: c.bottom + g2, left: -far, right: far } };
        if !cv.shows(&lane) {
            return;
        }
        let hot = |h: Hit| self.hot == Some(h);
        let dragged = match &self.drag {
            Some(Drag { grab: Grab::Tab(j), on: true, .. }) => Some(*j),
            _ => None,
        };
        cv.clip(&grow(c, px(2)));
        // groups: a pill with the name, a line along their tabs
        for &(r, h) in &self.hits {
            if let Hit::Group(g) = h {
                let Some(gr) = self.group(g) else { continue };
                let fill = mix(theme().bg, gr.color, if hot(h) { 0.5 } else { 0.35 });
                let pill = RECT { top: r.top + px(4), bottom: r.bottom - px(if vert { 2 } else { 6 }), ..r };
                cv.rrect(&pill, rad(8.0), Some(fill), None);
                let name = self.label_text(gr);
                if let Some(t) = self.gfx.layout(&w(&name), fr, (pill.right - pill.left - px(12)).max(1) as f32, flh as f32) {
                    cv.text(&t, pill.left + px(8), (pill.top + pill.bottom - flh) / 2, theme().text);
                }
                for k in (0..self.tabs.len()).filter(|&k| self.tabs[k].o.group == Some(g) && !self.folded_away(k)) {
                    let d = self.drawn(k);
                    let line = if vert { RECT { left: d.left - px(5), right: d.left - px(3), ..d } } else { RECT { top: d.bottom - px(5), bottom: d.bottom - px(3), ..d } };
                    cv.fill(&line, gr.color, 0.9);
                }
            }
        }
        // closing tabs fold away under the ones sliding over their place
        for gh in &self.ghosts {
            let a = 1.0 - ease_out(gh.at.elapsed().as_secs_f64() / TAB_OUT);
            self.draw_face(gh.r, a, &gh.face, false, false, false, None);
        }
        let lit = |j: usize| matches!(self.hot, Some(Hit::Tab(i) | Hit::TabClose(i)) if i == j);
        let order: Vec<usize> = (0..self.tabs.len()).filter(|&k| k != self.cur).chain([self.cur]).collect();
        self.seg_rs.borrow_mut().clear();
        for k in order {
            if dragged == Some(k) || self.folded_away(k) || self.slot_rs.get(k).is_none() {
                continue;
            }
            let r = self.drawn(k);
            let a = self.tabs[k].born.map_or(1.0, |b| ease_out(b.elapsed().as_secs_f64() / TAB_IN));
            let on = k == self.cur;
            let near = on || lit(k) || self.naming == Some(k);
            let next = k + 1 < self.tabs.len() && !self.folded_away(k + 1);
            let sep = !vert && !near && next && k + 1 != self.cur && !lit(k + 1);
            let close = match self.tp.close {
                2 => None,
                _ if self.tabs[k].o.pinned => None,
                1 => Some(hot(Hit::TabClose(k))),
                _ => near.then_some(hot(Hit::TabClose(k))),
            };
            self.seg_of.set(Some(k));
            self.draw_face(r, a, &self.face(k), on, near, sep, close);
            self.seg_of.set(None);
        }
        // a split tab's panes, under it (left strip)
        for &(r, h) in &self.hits {
            let Hit::TreePane(k, p) = h else { continue };
            let on = k == self.cur && p == self.id;
            if on || hot(h) {
                cv.rrect(&r, rad(5.0), Some(if on { theme().hl } else { theme().bg }), None);
            }
            cv.circle((r.left + px(6)) as f32, (r.top + r.bottom) as f32 / 2.0, rad(2.5), if on { theme().accent } else { theme().line }, 1.0);
            if let Some(t) = self.gfx.layout(&w(&self.qname(&self.pane_q(p))), fr, (r.right - r.left - px(18)).max(1) as f32, flh as f32) {
                cv.text(&t, r.left + px(14), (r.top + r.bottom - flh) / 2, if on { theme().text } else { theme().muted });
            }
        }
        cv.unclip();
        // the +, the ‹ › and ▾
        for &(r, h) in &self.hits {
            match h {
                Hit::NewTab => {
                    let d = if self.scroll.1 > 0 && !vert { 0 } else { (self.plus.0 - self.plus.2).round() as i32 };
                    let r = if vert { RECT { top: r.top + d, bottom: r.bottom + d, ..r } } else { RECT { left: r.left + d, right: r.right + d, ..r } };
                    if hot(h) {
                        cv.rrect(&r, rad(6.0), Some(theme().hl), None);
                    }
                    let (cx, cy, half, th) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2, px(6), ((1.5 * self.scale).round() as i32).max(1));
                    let col = if hot(h) { theme().text } else { theme().muted };
                    cv.fill(&RECT { left: cx - half, top: cy - th / 2, right: cx + half, bottom: cy - th / 2 + th }, col, 1.0);
                    cv.fill(&RECT { left: cx - th / 2, top: cy - half, right: cx - th / 2 + th, bottom: cy + half }, col, 1.0);
                }
                Hit::Scroll(right) => {
                    let on = if right { self.scroll.0 < self.scroll.1 as f64 } else { self.scroll.0 > 0.0 };
                    if hot(h) && on {
                        cv.rrect(&r, rad(6.0), Some(theme().hl), None);
                    }
                    let col = if on { theme().text } else { theme().line };
                    let (cx, cy, s) = ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0, rad(3.5));
                    let d = if right { s / 2.0 } else { -s / 2.0 };
                    cv.lines(&[(cx - d, cy - s), (cx + d, cy), (cx - d, cy + s)], col, rad(1.4));
                }
                Hit::Update => {
                    // a green download arrow, lighter pointed at
                    let (cx, cy, k) = ((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0, self.scale as f32);
                    let c = if hot(h) { mix(crate::update::GREEN, 0xffffff, 0.3) } else { crate::update::GREEN };
                    cv.lines(&[(cx, cy - 6.0 * k), (cx, cy + 3.5 * k)], c, 2.0 * k);
                    cv.lines(&[(cx - 4.5 * k, cy - 1.0 * k), (cx, cy + 3.5 * k), (cx + 4.5 * k, cy - 1.0 * k)], c, 2.0 * k);
                    cv.lines(&[(cx - 6.0 * k, cy + 7.0 * k), (cx + 6.0 * k, cy + 7.0 * k)], c, 2.0 * k);
                }
                Hit::TabList => {
                    if hot(h) {
                        cv.rrect(&r, rad(6.0), Some(theme().hl), None);
                    }
                    cv.chevron((r.left + r.right) as f32 / 2.0, (r.top + r.bottom) as f32 / 2.0, rad(3.5), true, theme().text, rad(1.4));
                }
                _ => {}
            }
        }
    }
    /// A tab's button in `r`, `a` of the way unfolded (0: a sliver, faded out; 1: all there). `on`: the one shown,
    /// `near`: it or pointed at, `sep`: a hairline after it, `close`: its × shows (and whether it's pointed at).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_face(&self, r: RECT, a: f64, fc: &Face, on: bool, near: bool, sep: bool, close: Option<bool>) {
        let cv = &self.cm;
        let (fr, fs, flh) = self.tab_font();
        let px = |v: i32| self.px(v);
        let vert = self.tp.pos == 2;
        let bottom = self.tp.pos == 1;
        let full = r;
        let r = RECT { right: r.left + ((r.right - r.left) as f64 * a).round() as i32, ..r };
        let wd = r.right - r.left;
        if wd < px(2) {
            return;
        }
        let rad = |v: f64| ((v * self.scale) as f32).min(wd as f32 / 2.0);
        let fade = |c: u32| mix(theme().bg, c, a as f32);
        let color = fc.color;
        let tint = |base: u32| match color.filter(|_| self.tp.color_style == 1) {
            Some(c) => mix(base, c, if on { 0.22 } else { 0.18 }),
            None => base,
        };
        let multi = fc.panes.len() > 1;
        let body = if vert || bottom { RECT { left: r.left, top: r.top + px(1), right: r.right, bottom: r.bottom - px(1) } } else { RECT { left: r.left + px(2), top: r.top + px(1), right: r.right - px(2), bottom: r.bottom - px(5) } };
        if on && !vert && !bottom {
            // over the strip's bottom line: the tab opens onto the toolbar
            cv.tab(&RECT { bottom: r.bottom + 1, ..r }, rad(8.0), rad(8.0), fade(tint(theme().card)), Some(fade(theme().line)));
        } else if on {
            cv.rrect(&body, rad(7.0), Some(fade(tint(theme().hl))), Some(fade(theme().line)));
        } else if near {
            cv.rrect(&body, rad(7.0), Some(fade(tint(theme().hl))), None);
        } else if color.is_some() && self.tp.color_style == 1 {
            cv.rrect(&body, rad(7.0), Some(fade(tint(theme().bg))), None);
        } else if sep && a >= 1.0 {
            let (x, my, half) = (r.right + px(1), (r.top + r.bottom - px(4)) / 2, (r.bottom - r.top) / 4);
            cv.fill(&RECT { left: x, top: my - half, right: x + 1, bottom: my + half }, theme().line, 1.0);
        }
        if let Some(c) = color.filter(|_| self.tp.color_style == 0) {
            let s = if vert {
                RECT { left: body.left + px(2), top: body.top + px(6), right: body.left + px(5), bottom: body.bottom - px(6) }
            } else if bottom {
                RECT { left: body.left + px(8), top: body.bottom - px(3), right: body.right - px(8), bottom: body.bottom }
            } else {
                RECT { left: r.left + px(8), top: r.top + px(1), right: r.right - px(8), bottom: r.top + px(4) }
            };
            cv.rrect_a(&s, rad(1.5), Some(c), None, a as f32);
        }
        if multi && self.tp.fused & 8 != 0 {
            let u = RECT { left: body.left + px(10), top: body.bottom - px(3), right: body.right - px(10), bottom: body.bottom - px(1) };
            cv.fill(&u, theme().accent, (0.8 * a) as f32);
        }
        let ty = body.top + (body.bottom - body.top - flh) / 2 + px(if on && !vert && !bottom { 2 } else { 0 });
        let cy = (body.top + body.bottom) / 2;
        let x_r = tab_x(&RECT { left: full.left, top: body.top, right: full.right, bottom: body.bottom }, px(20));
        let mut tx = r.left + px(if fc.pinned { 0 } else { 12 });
        cv.clip(&RECT { left: r.left, top: r.top, right: r.right - px(4).min(wd / 4), bottom: r.bottom });
        let ta = (a * if fc.asleep { 0.55 } else { 1.0 }) as f32;
        if let Some(c) = color.filter(|_| self.tp.color_style == 2) {
            cv.circle((tx + px(3)) as f32, cy as f32, (3.5 * self.scale) as f32, c, a as f32);
            tx += px(11);
        }
        if fc.busy {
            // a spinner where the icon is
            let t = Instant::now().duration_since(*SPIN_T).as_secs_f32() * 6.0;
            let (sx, s) = ((tx + px(8)) as f32, (5.5 * self.scale) as f32);
            let pts: Vec<(f32, f32)> = (0..=10).map(|i| {
                let ang = t + i as f32 * 0.47;
                (sx + s * ang.cos(), cy as f32 + s * ang.sin())
            }).collect();
            cv.lines(&pts, theme().accent, (1.6 * self.scale) as f32);
            tx += px(20);
        } else if let Some((bm, _)) = fc.pic.as_ref().and_then(|s| self.tab_bmp.get(s)).filter(|b| b.1 == cv.gen) {
            let s = px(16);
            if fc.pinned {
                tx = r.left + (full.right - full.left - s) / 2;
            }
            cv.bitmap(bm, &RECT { left: tx, top: cy - s / 2, right: tx + s, bottom: cy - s / 2 + s }, ta);
            tx += s + px(6);
        } else if let Some(ic) = &fc.icon {
            let iw = self.gfx.width(ic, fr);
            if fc.pinned {
                tx = r.left + (full.right - full.left - iw) / 2;
            }
            if let Some(t) = self.gfx.layout(&w(ic), fr, (iw + px(4)) as f32, flh as f32) {
                cv.text_a(&t, tx, ty, theme().text, ta);
            }
            tx += iw + px(6);
        }
        if multi && self.tp.fused & 1 != 0 && !fc.pinned {
            mini(cv, RECT { left: tx, top: cy - px(5), right: tx + px(14), bottom: cy + px(5) }, &fc.root, fade(if on { theme().accent } else { theme().muted }));
            tx += px(20);
        }
        if !fc.pinned {
            let mut name = fc.name.clone();
            if let Some(n) = fc.count {
                name += &format!(" ({n})");
            }
            if !fc.marks.is_empty() {
                name += " ";
                name += &fc.marks;
            }
            let end = if close.is_some() { x_r.left - px(2) } else { full.right - px(8) };
            let badge = if multi && self.tp.fused & 4 != 0 { px(18) } else { 0 };
            let font = if on { fs } else { fr };
            if fc.naming {
                // typed over: no title under the field, which takes its place and background
                let fill = if on && !vert && !bottom { tint(theme().card) } else { tint(theme().hl) };
                self.name_slot.set(Some((RECT { left: tx, top: ty, right: end, bottom: ty + flh }, fill)));
            } else if fc.split && !vert {
                // each pane's name, side by side: short ones whole, long ones sharing what's left, cut with …
                let mut segs = seg_names(&fc.panes);
                if let Some(l) = segs.last_mut() {
                    l.push_str(&name[fc.name.len()..]); // the count and marks, after the last
                }
                let gap = px(13);
                // up to where the × goes, shown or not: the names don't shift as it comes and goes on hover
                let room = (x_r.left - px(2) - tx - badge - gap * (segs.len() as i32 - 1)).max(1);
                let want: Vec<i32> = segs.iter().map(|s| self.gfx.width(s, font) + 1).collect();
                let ws = spread(fair(&want, room), room);
                let mut x = tx;
                for (i, (s, sw)) in segs.iter().zip(ws).enumerate() {
                    if i > 0 {
                        let dx = x - gap / 2 - 1;
                        cv.fill(&RECT { left: dx, top: cy - px(6), right: dx + 1, bottom: cy + px(6) }, theme().line, a as f32);
                    }
                    // pointed at: this name's pane is the one a middle-click closes (not "+N", which is several)
                    let lit = self.seg_of.get().is_some_and(|k| self.seg_hot == Some((k, i))) && !(i == 3 && fc.panes.len() > 4);
                    if lit {
                        let b = RECT { left: x - px(5), top: cy - flh / 2 - px(3), right: x + sw + px(5), bottom: cy + flh / 2 + px(3) };
                        cv.rrect_a(&b, rad(5.0), Some(theme().accent), None, (0.22 * a) as f32);
                    }
                    // centred in its share of the tab, so a short name leaves no gap at the end
                    let tw = want[i].min(sw);
                    if let Some(tl) = self.gfx.layout(&w(s), font, tw.max(1) as f32, flh as f32) {
                        cv.text_a(&tl, x + (sw - tw) / 2, ty, if near || lit { theme().text } else { theme().muted }, ta);
                    }
                    if let Some(k) = self.seg_of.get() {
                        self.seg_rs.borrow_mut().push((k, RECT { left: x - gap / 2, top: r.top, right: x + sw + gap / 2, bottom: r.bottom }, i));
                    }
                    x += sw + gap;
                }
                if badge > 0 {
                    let bx = x - gap + px(5);
                    let b = RECT { left: bx, top: cy - px(7), right: bx + px(14), bottom: cy + px(7) };
                    cv.rrect_a(&b, rad(7.0), Some(theme().line), None, a as f32);
                    put_mid(&self.gfx, cv, &fc.panes.len().to_string(), &self.gfx.f.small, &b, cy - self.gfx.f.lh / 2 + px(1), theme().text);
                }
            } else if let Some(tl) = self.gfx.layout(&w(&name), font, (end - tx - badge).max(1) as f32, flh as f32) {
                let tw = gfx::metrics(&tl).width.ceil() as i32;
                cv.text_a(&tl, tx, ty, if near { theme().text } else { theme().muted }, ta);
                if badge > 0 {
                    let b = RECT { left: tx + tw + px(5), top: cy - px(7), right: tx + tw + px(5) + px(14), bottom: cy + px(7) };
                    cv.rrect_a(&b, rad(7.0), Some(theme().line), None, a as f32);
                    put_mid(&self.gfx, cv, &fc.panes.len().to_string(), &self.gfx.f.small, &b, cy - self.gfx.f.lh / 2 + px(1), theme().text);
                }
            }
        }
        cv.unclip();
        if fc.dot {
            cv.circle((full.right - px(9)) as f32, (body.top + px(6)) as f32, rad(3.0), theme().accent, a as f32);
        }
        if let Some(hot) = close.filter(|_| a >= 1.0) {
            let (cx, cy) = ((x_r.left + x_r.right) as f32 / 2.0, (x_r.top + x_r.bottom) as f32 / 2.0);
            if hot {
                cv.circle(cx, cy, rad(9.0), if on { theme().hl } else { theme().line }, 1.0);
            }
            cv.cross(cx, cy, rad(3.5), if hot { theme().text } else { theme().muted }, rad(1.3));
        }
    }
}
/// When the spinners started turning: they all turn together.
static SPIN_T: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// A split tab's layout, small: its outline and the lines between its panes.
fn mini(cv: &Canvas, r: RECT, root: &Node, c: u32) {
    let (mut leaves, mut gutters) = (Vec::new(), Vec::new());
    root.place(r, 0, (&[], 0), &mut leaves, &mut gutters);
    cv.rrect(&r, 1.5, None, Some(c));
    for (_, _, g, vert) in gutters {
        let l = if vert { RECT { right: g.left + 1, ..g } } else { RECT { bottom: g.top + 1, ..g } };
        cv.fill(&l, c, 1.0);
    }
}

// -- the card over the window: a tab pointed at, the Ctrl+Tab switcher, a note
impl Ui {
    fn card_rows(&self, p: &Panel) -> Vec<CardRow> {
        match p {
            Panel::Tab(k) => {
                let Some(tb) = self.tabs.get(*k) else { return Vec::new() };
                let f = self.face(*k);
                let mut v = vec![CardRow::Title(self.tab_name(*k), f.icon.clone())];
                let q = self.focus_q(*k);
                v.push(CardRow::Text(listed_dir(&q).unwrap_or(q)));
                if f.panes.len() > 1 {
                    v.push(CardRow::Shape(f.root.clone(), f.panes.clone(), f.color.unwrap_or(theme().accent)));
                }
                if let Some(g) = tb.o.group.and_then(|g| self.group(g)) {
                    v.push(CardRow::Text(format!("{} {}", t("tab.group"), g.name)));
                }
                let flags: Vec<&str> = [(tb.o.pinned, "tab.pin"), (tb.o.locked, "tab.lock"), (tb.o.muted, "tab.mute"), (tb.o.live, "tab.live"), (tb.o.scope.is_some(), "tab.scope"), (tb.o.asleep.is_some(), "tab.sleep")].iter().filter(|f| f.0).map(|f| t(f.1)).collect();
                if !flags.is_empty() {
                    v.push(CardRow::Text(flags.join(" · ")));
                }
                v
            }
            Panel::Mru(order, pick) => order.iter().enumerate().filter(|(_, &k)| k < self.tabs.len()).map(|(i, &k)| CardRow::Pick(self.tab_name(k), self.face(k).icon, i == *pick)).collect(),
            Panel::Note(s) => vec![CardRow::Text(s.clone())],
        }
    }
    fn row_h(&self, r: &CardRow) -> i32 {
        let lh = self.gfx.f.lh;
        match r {
            CardRow::Title(..) => lh + self.px(8),
            CardRow::Text(_) => lh + self.px(4),
            CardRow::Shape(..) => self.px(78),
            CardRow::Pick(..) => lh + self.px(12),
        }
    }
    /// The card shown (made, or redrawn with `p`), under the tab it's about or over the middle of the window.
    pub(super) fn card_open(&mut self, p: Panel) {
        static CLASS: std::sync::Once = std::sync::Once::new();
        let class = wz("MosaicCard");
        let rows = self.card_rows(&p);
        if rows.is_empty() {
            return self.card_close();
        }
        let wd = self.px(if matches!(p, Panel::Mru(..)) { 380 } else { 320 });
        let ht = rows.iter().map(|r| self.row_h(r)).sum::<i32>() + self.px(20);
        let mut wr: RECT = unsafe { zeroed() };
        unsafe { GetWindowRect(self.main, &mut wr) };
        let at = match &p {
            Panel::Tab(k) => {
                let r = self.drawn(*k);
                let mut pt = if self.tp.pos == 2 { POINT { x: r.right + self.px(8), y: r.top } } else if self.tp.pos == 1 { POINT { x: r.left, y: r.top - self.px(8) - ht } } else { POINT { x: r.left, y: r.bottom + self.px(6) } };
                unsafe { ClientToScreen(self.main, &mut pt) };
                pt
            }
            _ => POINT { x: (wr.left + wr.right - wd) / 2, y: (wr.top + wr.bottom - ht) / 2 },
        };
        unsafe {
            CLASS.call_once(|| {
                let mut wc: WNDCLASSEXW = zeroed();
                wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
                wc.style = CS_DROPSHADOW;
                wc.hInstance = GetModuleHandleW(null());
                wc.lpfnWndProc = Some(card_proc);
                wc.lpszClassName = class.as_ptr();
                RegisterClassExW(&wc);
            });
            let h = match &self.card {
                Some((h, ..)) => *h,
                None => {
                    let h = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT, class.as_ptr(), null(), WS_POPUP, 0, 0, 1, 1, self.main, null_mut(), GetModuleHandleW(null()), null());
                    if h.is_null() {
                        return;
                    }
                    let dark: BOOL = 1;
                    DwmSetWindowAttribute(h, 20, &dark as *const _ as *const c_void, 4);
                    self.card = Some((h, Canvas::new(h, &self.gfx), Panel::Note(String::new())));
                    h
                }
            };
            let d = self.px(16);
            SetWindowRgn(h, CreateRoundRectRgn(0, 0, wd + 1, ht + 1, d, d), 0);
            SetWindowPos(h, HWND_TOPMOST, at.x, at.y, wd, ht, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            if let Some(c) = &mut self.card {
                c.2 = p;
            }
            InvalidateRect(h, null(), 0);
        }
    }
    pub(super) fn card_close(&mut self) {
        self.card_for = None;
        unsafe { KillTimer(self.main, T_CARD) };
        if let Some((h, ..)) = self.card.take() {
            unsafe { DestroyWindow(h) };
        }
    }
    /// The pointer came to (or left) a tab: its card shows after a moment.
    pub(super) fn card_hover(&mut self) {
        let k = match self.hot {
            Some(Hit::Tab(k)) if self.tp.cards && self.drag.is_none() => Some(k),
            _ => None,
        };
        if k == self.card_for {
            return;
        }
        if matches!(self.card, Some((_, _, Panel::Tab(_)))) || k.is_none() {
            self.card_close();
        }
        self.card_for = k;
        if k.is_some() {
            unsafe { SetTimer(self.main, T_CARD, 550, None) };
        }
    }
    /// T_CARD: the card for the tab still pointed at.
    pub(super) fn card_due(&mut self) {
        unsafe { KillTimer(self.main, T_CARD) };
        if let Some(k) = self.card_for.filter(|&k| self.hot == Some(Hit::Tab(k))) {
            self.card_open(Panel::Tab(k));
            self.card_for = Some(k);
        }
    }
    fn paint_card(&mut self) {
        let rows = match &self.card {
            Some((_, _, p)) => self.card_rows(p),
            None => return,
        };
        let sc = self.scale;
        let px = move |v: i32| (v as f64 * sc).round() as i32;
        let rad = |v: f64| (v * self.scale) as f32;
        let (s, lh) = (self.scale, self.gfx.f.lh);
        let heights: Vec<i32> = rows.iter().map(|r| self.row_h(r)).collect();
        let Some((h, mut cv, panel)) = self.card.take() else { return };
        if !cv.begin() {
            self.card = Some((h, cv, panel));
            return;
        }
        let g = &self.gfx;
        let mut c: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(h, &mut c) };
        cv.clear(theme().card);
        cv.rrect(&RECT { right: c.right - 1, bottom: c.bottom - 1, ..c }, rad(8.0), None, Some(theme().line));
        let mut y = px(10);
        let x0 = px(14);
        let room = (c.right - x0 - px(14)) as f32;
        for (row, rh) in rows.iter().zip(heights) {
            match row {
                CardRow::Title(name, icon) | CardRow::Pick(name, icon, _) => {
                    let picked = matches!(row, CardRow::Pick(_, _, true));
                    if picked {
                        cv.rrect(&RECT { left: px(6), top: y, right: c.right - px(6), bottom: y + rh }, rad(6.0), Some(theme().hl), Some(theme().accent));
                    }
                    let mut x = x0;
                    let ty = y + (rh - lh) / 2;
                    if let Some(i) = icon {
                        x += put(g, &cv, i, &g.f.reg, x, ty, theme().text) + px(6);
                    }
                    let bold = matches!(row, CardRow::Title(..)) || picked;
                    if let Some(t) = g.layout(&w(name), if bold { &g.f.semi } else { &g.f.reg }, room - (x - x0) as f32, lh as f32) {
                        cv.text(&t, x, ty, theme().text);
                    }
                }
                CardRow::Text(s) => {
                    if let Some(t) = g.layout(&w(s), &g.f.reg, room, lh as f32) {
                        cv.text(&t, x0, y + (rh - lh) / 2, theme().muted);
                    }
                }
                CardRow::Shape(root, panes, col) => {
                    let r = RECT { left: x0, top: y + px(4), right: c.right - px(14), bottom: y + rh - px(4) };
                    let (mut leaves, mut gutters) = (Vec::new(), Vec::new());
                    root.place(r, (3.0 * s).round() as i32, (&[], 0), &mut leaves, &mut gutters);
                    for (p, pr) in leaves {
                        cv.rrect(&pr, rad(4.0), Some(mix(theme().bg, *col, 0.18)), Some(mix(theme().line, *col, 0.5)));
                        let name = panes.iter().find(|x| x.0 == p).map_or("", |x| x.1.as_str());
                        if let Some(t) = g.layout(&w(name), &g.f.small, (pr.right - pr.left - px(8)).max(1) as f32, lh as f32) {
                            cv.text(&t, pr.left + px(4), (pr.top + pr.bottom - lh) / 2, theme().text);
                        }
                    }
                }
            }
            y += rh;
        }
        cv.end();
        self.card = Some((h, cv, panel));
    }
}

unsafe extern "system" fn card_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(h, &mut ps);
            with(|u| u.paint_card());
            EndPaint(h, &ps);
            0
        }
        WM_NCHITTEST => -1, // HTTRANSPARENT
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(h, m, wp, l),
    }
}

/// Ctrl+Tab with the switcher: the tabs in the order they were used, Tab / Shift+Tab to pick one, shown once Ctrl
/// is let go of (Esc: none).
pub(super) unsafe fn mru_switch(back: bool) {
    let Some(order) = with(|u| {
        let mut v: Vec<usize> = (0..u.tabs.len()).collect();
        v.sort_by_key(|&k| std::cmp::Reverse(u.tabs[k].used));
        v
    }) else { return };
    if order.len() < 2 {
        return;
    }
    let n = order.len();
    let mut pick = if back { n - 1 } else { 1 };
    with(|u| u.card_open(Panel::Mru(order.clone(), pick)));
    let mut msg: MSG = zeroed();
    let mut go = true;
    loop {
        if !key_down(VK_CONTROL) {
            break;
        }
        if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
            PostQuitMessage(0);
            go = false;
            break;
        }
        match msg.message {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = msg.wParam as u16;
                let back = vk == VK_LEFT || vk == VK_UP || vk == VK_TAB && key_down(VK_SHIFT);
                match vk {
                    VK_ESCAPE => {
                        go = false;
                        break;
                    }
                    VK_RETURN => break,
                    VK_TAB | VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN => pick = if back { (pick + n - 1) % n } else { (pick + 1) % n },
                    _ => {}
                }
                with(|u| u.card_open(Panel::Mru(order.clone(), pick)));
                continue;
            }
            WM_KEYUP | WM_SYSKEYUP if msg.wParam as u16 == VK_CONTROL => break,
            WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR => continue,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                go = false;
                break;
            }
            _ => {}
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    with(|u| {
        u.card_close();
        if go {
            u.switch_tab(order[pick]);
            u.reveal_tab(u.cur);
        }
    });
}

/// A line of text typed over `r` (main window coordinates) in place, starting as `init` (`hint` while empty): Enter
/// or a click elsewhere keeps it, Esc doesn't.
pub(super) unsafe fn ask(main: HWND, r: RECT, init: &str, hint: &str, look: Look) -> Option<String> {
    let e = field(main, r, init, -1, hint, false, look);
    if e.is_null() {
        return None;
    }
    let mut msg: MSG = zeroed();
    let mut keep = true;
    let mut redo: Option<MSG> = None;
    loop {
        if GetFocus() != e {
            break;
        }
        if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
            PostQuitMessage(0);
            keep = false;
            break;
        }
        match msg.message {
            WM_KEYDOWN if msg.wParam as u16 == VK_RETURN => break,
            WM_KEYDOWN if msg.wParam as u16 == VK_ESCAPE => {
                keep = false;
                break;
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN if msg.hwnd != e => {
                redo = Some(msg);
                break;
            }
            _ => {}
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let n = GetWindowTextLengthW(e);
    let mut b = vec![0u16; n as usize + 1];
    GetWindowTextW(e, b.as_mut_ptr(), n + 1);
    DestroyWindow(e);
    if let Some(m) = redo {
        PostMessageW(m.hwnd, m.message, m.wParam, m.lParam);
    }
    keep.then(|| String::from_utf16_lossy(&b[..n as usize]).trim().to_string())
}

/// Windows' colour picker, starting on `init` (0xRRGGBB).
unsafe fn pick_color(owner: HWND, init: Option<u32>) -> Option<u32> {
    use windows_sys::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
    let swap = |c: u32| ((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff);
    static mut CUSTOM: [u32; 16] = [0xffffff; 16];
    let mut cc: CHOOSECOLORW = zeroed();
    cc.lStructSize = size_of::<CHOOSECOLORW>() as u32;
    cc.hwndOwner = owner;
    cc.rgbResult = swap(init.unwrap_or(0x4C8DF6));
    cc.lpCustColors = std::ptr::addr_of_mut!(CUSTOM) as *mut u32;
    cc.Flags = CC_RGBINIT | CC_FULLOPEN;
    (ChooseColorW(&mut cc) != 0).then(|| swap(cc.rgbResult))
}

/// A JSON file to save to (`save`) or open.
unsafe fn json_file(owner: HWND, save: bool, name: &str) -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, GetSaveFileNameW, OFN_FILEMUSTEXIST, OFN_OVERWRITEPROMPT, OPENFILENAMEW};
    let mut buf = [0u16; 1024];
    for (i, c) in name.encode_utf16().take(200).enumerate() {
        buf[i] = c;
    }
    let filter: Vec<u16> = "JSON\0*.json\0\0".encode_utf16().collect();
    let ext = wz("json");
    let mut o: OPENFILENAMEW = zeroed();
    o.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    o.hwndOwner = owner;
    o.lpstrFilter = filter.as_ptr();
    o.lpstrFile = buf.as_mut_ptr();
    o.nMaxFile = buf.len() as u32;
    o.lpstrDefExt = ext.as_ptr();
    o.Flags = if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST };
    let ok = if save { GetSaveFileNameW(&mut o) } else { GetOpenFileNameW(&mut o) };
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..n]))
}

/// The next shortcut pressed (Backspace: back to the default, as ""), for `label`; Esc or a click: none.
unsafe fn capture(label: &str) -> Option<String> {
    with(|u| u.card_open(Panel::Note(t("tabs.press").replace("{name}", label))));
    let mut msg: MSG = zeroed();
    let mut got = None;
    loop {
        if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
            PostQuitMessage(0);
            break;
        }
        match msg.message {
            WM_KEYDOWN | WM_SYSKEYDOWN => match msg.wParam as u16 {
                VK_ESCAPE => break,
                VK_BACK => {
                    got = Some(String::new());
                    break;
                }
                VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL | VK_LMENU | VK_RMENU => continue,
                vk => {
                    got = Some(combo(vk));
                    break;
                }
            },
            WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR => continue,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => break,
            _ => {}
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    with(|u| u.card_close());
    got
}

// -- keys
impl Ui {
    fn binding(&self, id: &str) -> String {
        match self.tp.keys.get(id) {
            Some(b) => b.clone(),
            None => KEYS.iter().find(|k| k.0 == id).map_or(String::new(), |k| k.1.to_string()),
        }
    }
    /// What key `vk` does to tabs, pressed in a list (`in_list`) or the search box; None: nothing (the key is the
    /// list's or the box's).
    pub(super) fn tab_key(&self, vk: u16, in_list: bool) -> Option<TA> {
        let c = combo(vk);
        let split = self.visible().len() > 1;
        for (id, ..) in KEYS {
            if self.binding(id) != c {
                continue;
            }
            if matches!(id, "copy_other" | "move_other") && !(in_list && split) || id == "result_tab" && !in_list {
                continue;
            }
            return Some(TA::Key(id));
        }
        let (ctrl, shift, alt) = (key_down(VK_CONTROL), key_down(VK_SHIFT), key_down(VK_MENU));
        let n = self.tabs.len();
        match vk {
            VK_PRIOR | VK_NEXT if ctrl && !shift && !alt => Some(TA::Goto((self.cur + if vk == VK_NEXT { 1 } else { n - 1 }) % n)),
            0x31..=0x39 if ctrl && !shift && !alt => Some(TA::Goto(if vk == 0x39 { n - 1 } else { (vk as usize - 0x31).min(n - 1) })),
            0x31..=0x39 if alt && !ctrl && !shift && split => Some(TA::FocusN(vk as usize - 0x31)),
            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN if ctrl && alt && !shift && split => Some(TA::FocusDir(dir_of(vk))),
            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN if ctrl && shift && !alt && split && in_list => Some(TA::MoveDir(dir_of(vk))),
            _ => None,
        }
    }
    /// The pane next to the focused one toward `d` (0 left, 1 right, 2 up, 3 down).
    fn pane_toward(&self, d: u8) -> Option<usize> {
        let &(_, me) = self.areas.iter().find(|a| a.0 == self.id)?;
        let mid = |r: &RECT| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let (mx, my) = mid(&me);
        self.areas
            .iter()
            .filter(|(p, _)| *p != self.id)
            .filter(|(_, r)| match d {
                0 => r.right <= me.left + 2,
                1 => r.left >= me.right - 2,
                2 => r.bottom <= me.top + 2,
                _ => r.top >= me.bottom - 2,
            })
            .min_by_key(|(_, r)| {
                let (x, y) = mid(r);
                (x - mx).pow(2) + (y - my).pow(2)
            })
            .map(|a| a.0)
    }
}
fn dir_of(vk: u16) -> u8 {
    match vk {
        VK_LEFT => 0,
        VK_RIGHT => 1,
        VK_UP => 2,
        _ => 3,
    }
}

// -- doing it
/// Run `a`. Outside `with`: some ask first (a menu, a dialog, a name typed).
pub(super) unsafe fn tab_act(a: TA) {
    let Some((main, cur, id)) = with(|u| (u.main, u.cur, u.id)) else { return };
    let dir_of_tab = |k: usize| with(|u| (k < u.tabs.len()).then(|| listed_dir(&u.focus_q(k))).flatten()).flatten();
    let save = |u: &mut Ui| {
        u.save_settings();
        u.layout();
        u.invalidate_all();
    };
    match a {
        TA::Key(k) => match k {
            "new_tab" => tab_act(TA::NewRight(cur)),
            "close_tab" => tab_act(TA::Close(cur)),
            "close_pane" => {
                with(|u| u.close_pane(u.id));
            }
            "reopen" => {
                with(|u| u.reopen(None));
            }
            "next" | "prev" => {
                if with(|u| u.tp.mru).unwrap_or(false) {
                    return mru_switch(k == "prev");
                }
                with(|u| {
                    let n = u.tabs.len();
                    u.switch_tab((u.cur + if k == "next" { 1 } else { n - 1 }) % n);
                    u.reveal_tab(u.cur);
                });
            }
            "move_left" | "move_right" => {
                with(|u| u.move_tab(u.cur, if k == "move_left" { -1 } else { 1 }));
            }
            "duplicate" => tab_act(TA::Duplicate(cur)),
            "unfuse" => tab_act(TA::Unfuse(cur)),
            "rename" => tab_act(TA::Rename(cur)),
            "search" => tab_act(TA::Search),
            "split_right" | "split_below" => {
                with(|u| u.split_here(if k == "split_right" { 1 } else { 3 }));
            }
            "zoom" => tab_act(TA::Zoom(id)),
            "copy_other" | "move_other" => tab_act(TA::CopyOther(k == "copy_other")),
            "result_tab" => {
                with(|u| {
                    let Some(p) = u.sel_paths().into_iter().next() else { return };
                    if let Some(d) = parent_dir(&p) {
                        u.open_tab(&d);
                    }
                });
            }
            _ => {}
        },
        TA::Goto(k) => {
            with(|u| {
                u.switch_tab(k);
                u.reveal_tab(k);
            });
        }
        TA::FocusN(i) => {
            with(|u| {
                if let Some(&p) = u.visible().get(i) {
                    u.activate(p);
                    SetFocus(u.list);
                }
            });
        }
        TA::FocusDir(d) => {
            with(|u| {
                if let Some(p) = u.pane_toward(d) {
                    u.activate(p);
                    SetFocus(u.list);
                }
            });
        }
        TA::MoveDir(d) => {
            with(|u| {
                if let Some(p) = u.pane_toward(d) {
                    let me = u.id;
                    swap_leaves(&mut u.tabs[u.cur].root, me, p);
                    u.layout();
                    u.invalidate_all();
                }
            });
        }
        TA::NewRight(k) => {
            with(|u| {
                let at = u.tabs[k].o.group.and_then(|g| u.tabs.iter().rposition(|t| t.o.group == Some(g))).map_or(k + 1, |i| i + 1).max(k + 1);
                let g = u.tabs[k].o.group;
                u.new_tab("home:", at);
                u.tabs[u.cur].o.group = g;
                u.order_tabs();
                u.layout();
            });
        }
        TA::Duplicate(k) => {
            with(|u| u.duplicate(k));
        }
        TA::Close(k) => {
            with(|u| u.drop_tab(k, true));
        }
        TA::CloseMany(k, which) => {
            let Some(ks) = with(|u| (0..u.tabs.len()).filter(|&j| j != k && !u.tabs[j].o.pinned && (which == 0 || which == 1 && j > k || which == 2 && j < k)).collect::<Vec<_>>()) else { return };
            if ks.len() > 10 && !confirm(main, &t("tabs.close_many").replace("{n}", &ks.len().to_string())) {
                return;
            }
            with(|u| {
                let keep = u.tabs[k].uid;
                u.switch_tab(k);
                for &j in ks.iter().rev() {
                    u.drop_tab(j, true);
                }
                if let Some(i) = u.tabs.iter().position(|t| t.uid == keep) {
                    u.switch_tab(i);
                }
            });
        }
        TA::Reopen(i) => {
            with(|u| u.reopen(Some(i)));
        }
        TA::Flag(k, f) => {
            with(|u| {
                let v = f(&mut u.tabs[k].o);
                *v = !*v;
                u.order_tabs();
                u.layout();
                u.invalidate_all();
            });
        }
        TA::Scope(k) => {
            with(|u| {
                let d = listed_dir(&u.focus_q(k));
                let o = &mut u.tabs[k].o;
                o.scope = if o.scope.is_some() { None } else { d };
                u.invalidate();
            });
        }
        TA::Sleep(k) => {
            with(|u| {
                u.sleep_tab(k);
                u.invalidate();
            });
        }
        TA::Rename(k) => {
            // typed right on the tab, in its own font: the title just becomes editable
            with(|u| {
                u.naming = Some(k);
                u.name_slot.set(None);
                u.invalidate();
            });
            UpdateWindow(main);
            let Some((r, name, look, font)) = with(|u| {
                let name = u.tabs[k].o.title.clone().unwrap_or_else(|| u.tab_name(k));
                match u.name_slot.get().filter(|(r, _)| r.right - r.left > u.px(24)) {
                    Some((r, fill)) => {
                        let px = [10.5, 12.0, 14.0][u.tp.font.min(2) as usize] * u.scale;
                        let font = gdi_font(px, if k == u.cur { 600 } else { 400 });
                        (r, name, Look::Bare(font, fill), font)
                    }
                    None => {
                        // pinned, or not on screen: a box of its own over the search box
                        let r = RECT { left: u.search_box.left, top: u.search_box.top, right: u.search_box.left + u.px(240), bottom: u.search_box.bottom };
                        (r, name, Look::Box, null_mut())
                    }
                }
            }) else { return };
            let got = ask(main, r, &name, &with(|u| u.tab_name(k)).unwrap_or_default(), look);
            if !font.is_null() {
                DeleteObject(font);
            }
            with(|u| {
                u.naming = None;
                u.invalidate();
            });
            if let Some(s) = got {
                with(|u| {
                    if let Some(t) = u.tabs.get_mut(k) {
                        t.o.title = (!s.is_empty()).then_some(s);
                    }
                    u.layout();
                    u.title();
                });
            }
        }
        TA::Color(k, c) => {
            with(|u| {
                u.tabs[k].o.color = c;
                u.invalidate();
            });
        }
        TA::ColorCustom(k) => {
            let init = with(|u| u.tab_color(k)).flatten();
            if let Some(c) = pick_color(main, init) {
                tab_act(TA::Color(k, Some(c)));
            }
        }
        TA::ColorRule(k) => {
            let Some(d) = dir_of_tab(k) else { return };
            let init = with(|u| u.tab_color(k)).flatten();
            if let Some(c) = pick_color(main, init) {
                with(|u| {
                    u.tp.rules.retain(|r| !r.0.eq_ignore_ascii_case(&d));
                    u.tp.rules.push((d, c));
                    save(u);
                });
            }
        }
        TA::Icon(k, s) => {
            with(|u| {
                u.tabs[k].o.icon = s;
                u.layout();
            });
        }
        TA::IconCustom(k) => {
            let Some(r) = with(|u| u.drawn(k)) else { return };
            if let Some(s) = ask(main, r, "", t("tab.icon"), Look::Box) {
                tab_act(TA::Icon(k, (!s.is_empty()).then_some(s)));
            }
        }
        TA::NewWindow(k) => {
            let Some(json) = with(|u| u.json_of(&[k])) else { return };
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            if spawn_window(&json, Some(p)) {
                with(|u| u.drop_tab(k, false));
            }
        }
        TA::ToWindow(k, h) => {
            let Some(json) = with(|u| u.json_of(&[k])) else { return };
            if send_tabs(h as HWND, &json, false) {
                with(|u| u.drop_tab(k, false));
                SetForegroundWindow(h as HWND);
            }
        }
        TA::SplitWith(k, side) => {
            with(|u| {
                u.switch_tab(k);
                u.split_here(side);
            });
        }
        TA::Unfuse(k) => {
            with(|u| u.unfuse(k));
        }
        TA::Fuse(k, next) => {
            with(|u| u.fuse(k, next));
        }
        TA::CopyPath(k) => {
            if let Some(d) = dir_of_tab(k) {
                let ok = clip_text(main, &d);
                with(|u| u.flash(t(if ok { "status.path_copied" } else { "status.clipboard_busy" }).into()));
            }
        }
        TA::Explorer(k) => {
            if let Some(d) = dir_of_tab(k) {
                shell::open(&d);
            }
        }
        TA::Terminal(k) => {
            if let Some(d) = dir_of_tab(k) {
                shell::wt(&d);
            }
        }
        TA::Favorite(k) => {
            if let Some(d) = dir_of_tab(k) {
                shell::item_verb(&d, "pintohome");
            }
        }
        TA::Props(k) => {
            if let Some(d) = dir_of_tab(k) {
                shell::properties(d);
            }
        }
        TA::GroupNew(k) => {
            let Some(g) = with(|u| {
                let id = new_uid();
                let color = PALETTE[u.groups.len() % PALETTE.len()].0;
                u.groups.push(Group { id, name: String::new(), color, folded: false });
                u.tabs[k].o.group = Some(id);
                u.order_tabs();
                u.layout();
                id
            }) else { return };
            tab_act(TA::GroupRename(g));
        }
        TA::GroupJoin(k, g) => {
            with(|u| {
                u.tabs[k].o.group = Some(g);
                u.order_tabs();
                u.layout();
            });
        }
        TA::GroupLeave(k) => {
            with(|u| {
                u.tabs[k].o.group = None;
                u.order_tabs();
                u.layout();
            });
        }
        TA::GroupFold(g) => {
            with(|u| {
                if let Some(x) = u.groups.iter_mut().find(|x| x.id == g) {
                    x.folded = !x.folded;
                }
                u.layout();
            });
        }
        TA::GroupRename(g) => {
            let Some((r, name)) = with(|u| {
                let r = u.hits.iter().find(|h| h.1 == Hit::Group(g)).map_or(u.search_box, |h| h.0);
                (RECT { right: r.left + u.px(180), ..r }, u.group(g).map_or(String::new(), |x| x.name.clone()))
            }) else { return };
            if let Some(s) = ask(main, r, &name, t("tab.group_new"), Look::Box) {
                with(|u| {
                    if let Some(x) = u.groups.iter_mut().find(|x| x.id == g) {
                        x.name = s;
                    }
                    u.layout();
                });
            }
        }
        TA::GroupColor(g, c) => {
            with(|u| {
                if let Some(x) = u.groups.iter_mut().find(|x| x.id == g) {
                    x.color = c;
                }
                u.invalidate();
            });
        }
        TA::GroupClose(g) => {
            let Some(n) = with(|u| u.tabs.iter().filter(|t| t.o.group == Some(g)).count()) else { return };
            if n > 10 && !confirm(main, &t("tabs.close_many").replace("{n}", &n.to_string())) {
                return;
            }
            with(|u| {
                while let Some(k) = u.tabs.iter().rposition(|t| t.o.group == Some(g)) {
                    if u.tabs.len() == 1 {
                        u.drop_tab(k, true);
                        break;
                    }
                    u.drop_tab(k, true);
                }
            });
        }
        TA::GroupWindow(g) => {
            let Some((ks, json)) = with(|u| {
                let ks: Vec<usize> = (0..u.tabs.len()).filter(|&k| u.tabs[k].o.group == Some(g)).collect();
                let json = u.json_of(&ks);
                (ks, json)
            }) else { return };
            let mut p = POINT { x: 0, y: 0 };
            GetCursorPos(&mut p);
            if spawn_window(&json, Some(p)) {
                with(|u| {
                    for &k in ks.iter().rev() {
                        u.drop_tab(k, false);
                    }
                });
            }
        }
        TA::GroupTab(g) => {
            with(|u| {
                let at = u.tabs.iter().rposition(|t| t.o.group == Some(g)).map_or(u.tabs.len(), |i| i + 1);
                u.new_tab("home:", at);
                u.tabs[u.cur].o.group = Some(g);
                u.order_tabs();
                u.layout();
            });
        }
        TA::Ungroup(g) => {
            with(|u| {
                for t in &mut u.tabs {
                    if t.o.group == Some(g) {
                        t.o.group = None;
                    }
                }
                u.order_tabs();
                u.layout();
            });
        }
        TA::Swap(sid) => {
            with(|u| {
                if let Some(Node::Split { ratio, shown, a, b, .. }) = split_by(&mut u.tabs[u.cur].root, sid) {
                    std::mem::swap(a, b);
                    (*ratio, *shown) = (1.0 - *ratio, 1.0 - *shown);
                }
                u.layout();
                u.invalidate_all();
            });
        }
        TA::Rotate(sid) => {
            with(|u| {
                match sid {
                    Some(s) => {
                        if let Some(Node::Split { vert, .. }) = split_by(&mut u.tabs[u.cur].root, s) {
                            *vert = !*vert;
                        }
                    }
                    None => each_split(&mut u.tabs[u.cur].root, &mut |v, _| *v = !*v),
                }
                u.layout();
                u.invalidate_all();
            });
        }
        TA::Equalize => {
            with(|u| {
                each_split(&mut u.tabs[u.cur].root, &mut |_, r| *r = 0.5);
                u.layout();
            });
        }
        TA::Half(sid) => {
            with(|u| {
                if let Some(Node::Split { ratio, .. }) = split_by(&mut u.tabs[u.cur].root, sid) {
                    *ratio = 0.5;
                }
                u.layout();
            });
        }
        TA::Zoom(p) => {
            with(|u| {
                let o = &mut u.tabs[u.cur].o;
                o.zoom = if o.zoom.is_some() { None } else { Some(p) };
                u.activate(p);
                u.layout();
                u.invalidate_all();
            });
        }
        TA::Fold(p) => {
            with(|u| {
                let f = &mut u.tabs[u.cur].o.folded;
                match f.iter().position(|&x| x == p) {
                    Some(i) => {
                        f.remove(i);
                    }
                    None => f.push(p),
                }
                u.layout();
                u.invalidate_all();
            });
        }
        TA::Preset(n) => {
            with(|u| u.preset(n));
        }
        TA::CopyOther(copy) => copy_other(copy),
        TA::PaneTab(p) => {
            with(|u| u.pane_tab(p));
        }
        TA::ClosePane(p) => {
            with(|u| u.close_pane(p));
        }
        TA::Search => {
            let Some((mut mb, items)) = with(|u| {
                let mut mb = Mb::default();
                let items = u.tab_list(&mut mb);
                (mb, items)
            }) else { return };
            let mut wr: RECT = zeroed();
            GetWindowRect(main, &mut wr);
            let Some(px) = with(|u| u.px(420)) else { return };
            let (x, y) = ((wr.left + wr.right - px) / 2, wr.top + px / 4);
            run_menu(main, &mut mb, items, x, y, y, px, true);
        }
        TA::WsSave => {
            let Some(r) = with(|u| RECT { right: u.search_box.left + u.px(260), ..u.search_box }) else { return };
            let Some(name) = ask(main, r, "", t("ws.name"), Look::Box).filter(|s| !s.is_empty()) else { return };
            let Some(s) = with(|u| u.session_of(&(0..u.tabs.len()).collect::<Vec<_>>())) else { return };
            let mut v = load_workspaces();
            v.retain(|w| w.0 != name);
            v.push((name, s));
            save_workspaces(&v);
        }
        TA::WsOpen(i, add) => {
            if let Some((_, s)) = load_workspaces().into_iter().nth(i) {
                with(|u| u.load_session(s, !add));
            }
        }
        TA::WsExport(i) => {
            let v = load_workspaces();
            let Some(ws) = v.get(i) else { return };
            if let Some(f) = json_file(main, true, &ws.0) {
                let _ = std::fs::write(f, serde_json::to_string_pretty(&ws.1).unwrap_or_default());
            }
        }
        TA::WsDelete(i) => {
            let mut v = load_workspaces();
            if i < v.len() {
                v.remove(i);
                save_workspaces(&v);
            }
        }
        TA::WsImport => {
            let Some(f) = json_file(main, false, "") else { return };
            let Some(s) = std::fs::read_to_string(&f).ok().and_then(|s| serde_json::from_str::<Session>(&s).ok()) else { return };
            let name = std::path::Path::new(&f).file_stem().map_or("?".into(), |n| n.to_string_lossy().into_owned());
            let mut v = load_workspaces();
            v.retain(|w| w.0 != name);
            v.push((name, s));
            save_workspaces(&v);
        }
        TA::Merge => {
            for (h, _) in windows(main) {
                PostMessageW(h, WM_MERGE, main as usize, 0);
            }
        }
        TA::Window(h) => {
            SetForegroundWindow(h as HWND);
        }
        TA::StartupSet => {
            with(|u| {
                u.tp.startup = (0..u.tabs.len()).filter_map(|k| listed_dir(&u.focus_q(k))).collect();
                u.tp.start = 4;
                save(u);
            });
        }
        TA::StartupClear => {
            with(|u| {
                u.tp.startup.clear();
                if u.tp.start == 4 {
                    u.tp.start = 3;
                }
                save(u);
            });
        }
        TA::Pu(f, v) => {
            with(|u| {
                *f(&mut u.tp) = v;
                for t in &mut u.tabs {
                    t.x = f64::NAN;
                }
                u.scroll.0 = 0.0;
                save(u);
            });
        }
        TA::Pb(f) => {
            with(|u| {
                let b = f(&mut u.tp);
                *b = !*b;
                save(u);
            });
        }
        TA::Fused(bit) => {
            with(|u| {
                u.tp.fused ^= bit;
                save(u);
            });
        }
        TA::SleepMax(n) => {
            with(|u| {
                u.tp.sleep = n;
                u.auto_sleep();
                save(u);
            });
        }
        TA::RuleDel(i) => {
            with(|u| {
                if i < u.tp.rules.len() {
                    u.tp.rules.remove(i);
                }
                save(u);
            });
        }
        TA::Rebind(k) => {
            let label = KEYS.iter().find(|x| x.0 == k).map_or("", |x| t(x.2));
            if let Some(c) = capture(label) {
                with(|u| {
                    if c.is_empty() {
                        u.tp.keys.remove(k);
                    } else {
                        u.tp.keys.insert(k.into(), c);
                    }
                    u.save_settings();
                });
            }
        }
        TA::KeysReset => {
            with(|u| {
                u.tp.keys.clear();
                u.save_settings();
            });
        }
    }
    with(|u| {
        u.autosave(false);
        u.title();
    });
}

fn split_by(n: &mut Node, sid: usize) -> Option<&mut Node> {
    match n {
        Node::Split { id, .. } if *id == sid => Some(n),
        Node::Split { a, b, .. } => match split_by(a, sid) {
            Some(x) => Some(x),
            None => split_by(b, sid),
        },
        Node::Leaf(_) => None,
    }
}

/// Shift+F5 / F6: the selection copied (moved) into the next pane's folder, the way a drop does it.
unsafe fn copy_other(copy: bool) {
    use crate::infrastructure::fileops;
    let Some(Some((paths, dir, main))) = with(|u| {
        let v = u.visible();
        let me = v.iter().position(|&p| p == u.id)?;
        let other = v[(me + 1) % v.len()];
        let dir = u.pane_dir(other)?;
        Some((u.sel_paths(), dir, u.main))
    }) else {
        with(|u| u.flash(t("status.no_folder").into()));
        return;
    };
    if paths.is_empty() {
        return;
    }
    let to = |p: &String| std::path::Path::new(&dir).join(std::path::Path::new(p).file_name().unwrap_or_default()).to_string_lossy().into_owned();
    let pairs: Vec<(String, String)> = paths.iter().filter(|p| !std::path::Path::new(&to(p)).exists()).map(|p| (to(p), p.clone())).collect();
    if !copy {
        preview::release(&paths);
    }
    match fileops::transfer(&paths, &dir, copy, main as isize) {
        Ok(()) => {
            if !pairs.is_empty() {
                did(if copy { Undo::Made(pairs.into_iter().map(|p| p.0).collect()) } else { Undo::Moved(pairs) });
            }
            with(|u| {
                u.relist_soon();
                u.flash(t(if copy { "status.copied" } else { "status.cut" }).replace("{n}", &plural(paths.len())));
            });
        }
        Err(e) if e == "cancelled" => {}
        Err(e) => {
            with(|u| u.flash(t("status.failed").replace("{e}", &e)));
        }
    }
}

// -- split tabs
impl Ui {
    /// Tab `k`'s panes each in a tab of its own, after it.
    fn unfuse(&mut self, k: usize) {
        self.wake(k);
        let leaves = self.tabs[k].root.leaves();
        if leaves.len() < 2 {
            return;
        }
        let keep = self.tabs[k].focus;
        let mut at = k + 1;
        for p in leaves.into_iter().filter(|&p| p != keep) {
            self.tabs[k].root.detach(p);
            let mut t = Tab::new(p);
            t.o.group = self.tabs[k].o.group;
            self.tabs.insert(at, t);
            if at <= self.cur {
                self.cur += 1;
            }
            at += 1;
            if self.view(p).is_some() {
                self.with_pane(p, |u| u.hibernate());
            }
        }
        self.tabs[k].o.zoom = None;
        self.tabs[k].o.folded.clear();
        self.layout();
        self.invalidate_all();
    }
    /// Tab `k` and the one after (`next`) or before it as one, side by side.
    fn fuse(&mut self, k: usize, next: bool) {
        let j = if next { k + 1 } else { k.wrapping_sub(1) };
        if j >= self.tabs.len() {
            return;
        }
        self.switch_tab(k);
        self.wake(j);
        let t = self.tabs.remove(j);
        if j < self.cur {
            self.cur -= 1;
        }
        self.dock(t.root, Dock::Root(if next { 1 } else { 0 }));
        self.layout();
        self.invalidate_all();
    }
    /// Pane `p` out to a tab of its own, after this one.
    fn pane_tab(&mut self, p: usize) {
        if self.visible().len() < 2 || !self.tabs[self.cur].root.detach(p) {
            return;
        }
        if self.id == p {
            let next = self.visible()[0];
            self.activate(next);
        }
        self.tabs.insert(self.cur + 1, Tab::new(p));
        self.with_pane(p, |u| u.hibernate());
        let o = &mut self.tabs[self.cur].o;
        o.zoom = None;
        o.folded.retain(|&x| x != p);
        self.layout();
        self.invalidate_all();
    }
    /// The panes of the tab shown arranged: 0 two columns, 1 two rows, 2 a 2×2 grid, 3 one left and two right.
    /// Panes it lacks are made (listing what the focused one does); ones it has too many of go to tabs of their own.
    fn preset(&mut self, n: u8) {
        let want = [2, 2, 4, 3][n.min(3) as usize];
        let mut ps = self.visible();
        if let Some(i) = ps.iter().position(|&p| p == self.id) {
            ps.swap(0, i);
        }
        while ps.len() > want {
            let p = ps.pop().unwrap();
            self.tabs[self.cur].root.detach(p);
            self.tabs.insert(self.cur + 1, Tab::new(p));
            self.with_pane(p, |u| u.hibernate());
        }
        let q = self.args.q.clone();
        while ps.len() < want {
            ps.push(self.new_pane(&q));
        }
        let sp = |a: Node, b: Node, vert: bool, u: &mut Ui| Node::Split { id: u.next_split(), vert, ratio: 0.5, shown: 0.5, vel: 0.0, a: Box::new(a), b: Box::new(b) };
        let l = |i: usize| Node::Leaf(ps[i]);
        let root = match n {
            0 => sp(l(0), l(1), true, self),
            1 => sp(l(0), l(1), false, self),
            2 => {
                let top = sp(l(0), l(1), true, self);
                let bottom = sp(l(2), l(3), true, self);
                sp(top, bottom, false, self)
            }
            _ => {
                let right = sp(l(1), l(2), false, self);
                sp(l(0), right, true, self)
            }
        };
        let o = &mut self.tabs[self.cur];
        o.root = root;
        o.o.zoom = None;
        o.o.folded.clear();
        self.layout();
        self.invalidate_all();
    }
}

// -- menus
impl Ui {
    fn tab_list(&self, mb: &mut Mb) -> Vec<MItem> {
        (0..self.tabs.len())
            .map(|k| {
                let f = self.face(k);
                let mut i = mb.it(&format!("{}{}", f.icon.map_or(String::new(), |i| format!("{i} ")), self.tab_name(k)), &listed_dir(&self.focus_q(k)).unwrap_or_default(), k == self.cur, TA::Goto(k));
                i.bold = k == self.cur;
                i
            })
            .collect()
    }
    fn closed_items(&self, mb: &mut Mb) -> Vec<MItem> {
        self.closed.iter().enumerate().rev().take(15).map(|(i, (s, _))| {
            let q = s.root.leaves().get(s.focus).or(s.root.leaves().first()).map_or(String::new(), |p| p.q.clone());
            mb.it(&s.o.title.clone().unwrap_or_else(|| self.qname(&q)), "", false, TA::Reopen(i))
        }).collect()
    }
    fn color_items(&self, mb: &mut Mb, cur: Option<u32>, f: impl Fn(Option<u32>) -> TA) -> Vec<MItem> {
        let mut v = vec![mb.it(t("tab.no_color"), "", cur.is_none(), f(None))];
        for (c, key) in PALETTE {
            let mut i = mb.it(t(key), "", cur == Some(c), f(Some(c)));
            i.icon = Some(swatch(c));
            v.push(i);
        }
        v
    }
    pub(super) fn tab_menu(&self, k: usize, mb: &mut Mb) -> Vec<MItem> {
        let tb = &self.tabs[k];
        let o = &tb.o;
        let n = self.tabs.len();
        let multi = tb.o.asleep.as_ref().map_or(tb.root.leaves().len(), |a| a.0.leaves().len()) > 1;
        let has_dir = listed_dir(&self.focus_q(k)).is_some();
        let key = |id: &str| self.binding(id);
        let mut colors = self.color_items(mb, o.color, |c| TA::Color(k, c));
        colors.push(sep());
        colors.push(mb.it(&format!("{}…", t("tab.custom")), "", false, TA::ColorCustom(k)));
        colors.push(off(mb.it(&format!("{}…", t("tab.color_rule")), "", false, TA::ColorRule(k)), !has_dir));
        let mut icons = vec![mb.it(t("tab.no_color"), "", o.icon.is_none(), TA::Icon(k, None))];
        for e in EMOJI {
            icons.push(mb.it(e, "", o.icon.as_deref() == Some(e), TA::Icon(k, Some(e.into()))));
        }
        icons.push(sep());
        icons.push(mb.it(&format!("{}…", t("tab.custom")), "", false, TA::IconCustom(k)));
        let mut groups = vec![mb.it(t("tab.group_new"), "", false, TA::GroupNew(k))];
        if !self.groups.is_empty() {
            groups.push(sep());
        }
        for g in &self.groups {
            let mut i = mb.it(if g.name.is_empty() { "•" } else { &g.name }, "", o.group == Some(g.id), TA::GroupJoin(k, g.id));
            i.icon = Some(swatch(g.color));
            groups.push(i);
        }
        if o.group.is_some() {
            groups.push(sep());
            groups.push(mb.it(t("tab.group_leave"), "", false, TA::GroupLeave(k)));
        }
        let wins: Vec<MItem> = windows(self.main).into_iter().map(|(h, title)| mb.it(&title, "", false, TA::ToWindow(k, h as isize))).collect();
        vec![
            mb.it(t("tab.new_right"), &key("new_tab"), false, TA::NewRight(k)),
            mb.it(t("tab.duplicate"), &key("duplicate"), false, TA::Duplicate(k)),
            off(mb.it(t("tab.reopen"), &key("reopen"), false, TA::Reopen(self.closed.len().wrapping_sub(1))), self.closed.is_empty()),
            sub(t("tab.recent"), self.closed_items(mb)),
            sep(),
            mb.it(t("tab.pin"), "", o.pinned, TA::Flag(k, |o| &mut o.pinned)),
            mb.it(t("tab.lock"), "", o.locked, TA::Flag(k, |o| &mut o.locked)),
            mb.it(t("tab.mute"), "", o.muted, TA::Flag(k, |o| &mut o.muted)),
            mb.it(t("tab.live"), "", o.live, TA::Flag(k, |o| &mut o.live)),
            off(mb.it(t("tab.scope"), "", o.scope.is_some(), TA::Scope(k)), !has_dir && o.scope.is_none()),
            sep(),
            mb.it(&format!("{}…", t("tab.rename")), &key("rename"), false, TA::Rename(k)),
            sub(t("tab.color"), colors),
            sub(t("tab.icon"), icons),
            sub(t("tab.group"), groups),
            sep(),
            mb.it(t("tab.split_right"), &key("split_right"), false, TA::SplitWith(k, 1)),
            mb.it(t("tab.split_below"), &key("split_below"), false, TA::SplitWith(k, 3)),
            off(mb.it(t("tab.unfuse"), &key("unfuse"), false, TA::Unfuse(k)), !multi),
            off(mb.it(t("tab.fuse_next"), "", false, TA::Fuse(k, true)), k + 1 >= n),
            off(mb.it(t("tab.fuse_prev"), "", false, TA::Fuse(k, false)), k == 0),
            sep(),
            off(mb.it(t("tab.copy_path"), "", false, TA::CopyPath(k)), !has_dir),
            off(mb.it(t("tab.explorer"), "", false, TA::Explorer(k)), !has_dir),
            off(mb.it(t("tab.terminal"), "", false, TA::Terminal(k)), !has_dir),
            off(mb.it(t("tab.favorite"), "", false, TA::Favorite(k)), !has_dir),
            off(mb.it(t("tab.properties"), "", false, TA::Props(k)), !has_dir),
            sep(),
            off(mb.it(t("tab.sleep"), "", o.asleep.is_some(), TA::Sleep(k)), k == self.cur || o.asleep.is_some()),
            off(mb.it(t("tab.new_window"), "", false, TA::NewWindow(k)), n < 2),
            off(sub(t("tab.to_window"), wins), n < 2),
            sep(),
            mb.it(t("tab.close"), &key("close_tab"), false, TA::Close(k)),
            off(mb.it(t("tab.close_others"), "", false, TA::CloseMany(k, 0)), n < 2),
            off(mb.it(t("tab.close_right"), "", false, TA::CloseMany(k, 1)), k + 1 >= n),
            off(mb.it(t("tab.close_left"), "", false, TA::CloseMany(k, 2)), k == 0),
        ]
    }
    pub(super) fn group_menu(&self, g: u64, mb: &mut Mb) -> Vec<MItem> {
        let gr = self.group(g);
        let folded = gr.is_some_and(|x| x.folded);
        let colors = PALETTE.iter().map(|&(c, key)| {
            let mut i = mb.it(t(key), "", gr.is_some_and(|x| x.color == c), TA::GroupColor(g, c));
            i.icon = Some(swatch(c));
            i
        }).collect();
        vec![
            mb.it(&format!("{}…", t("group.rename")), "", false, TA::GroupRename(g)),
            sub(t("group.color"), colors),
            mb.it(t(if folded { "group.unfold" } else { "group.fold" }), "", false, TA::GroupFold(g)),
            sep(),
            mb.it(t("group.new_tab"), "", false, TA::GroupTab(g)),
            mb.it(t("group.window"), "", false, TA::GroupWindow(g)),
            sep(),
            mb.it(t("group.ungroup"), "", false, TA::Ungroup(g)),
            mb.it(t("group.close"), "", false, TA::GroupClose(g)),
        ]
    }
    /// The tab shown's split tools: on a gap (`sid`), a caption (`pane`), or from the menu bar.
    pub(super) fn layout_menu(&self, sid: Option<usize>, pane: Option<usize>, mb: &mut Mb) -> Vec<MItem> {
        let o = &self.tabs[self.cur].o;
        let split = self.visible().len() > 1;
        let presets = vec![
            mb.it(t("layout.cols"), "", false, TA::Preset(0)),
            mb.it(t("layout.rows"), "", false, TA::Preset(1)),
            mb.it(t("layout.grid"), "", false, TA::Preset(2)),
            mb.it(t("layout.one_two"), "", false, TA::Preset(3)),
        ];
        let mut v = Vec::new();
        if let Some(p) = pane {
            v.push(mb.it(t("layout.zoom"), &self.binding("zoom"), o.zoom == Some(p), TA::Zoom(p)));
            v.push(off(mb.it(t("layout.fold"), "", o.folded.contains(&p), TA::Fold(p)), !split));
            v.push(off(mb.it(t("layout.own_tab"), "", false, TA::PaneTab(p)), !split));
            v.push(mb.it(t("layout.close_pane"), &self.binding("close_pane"), false, TA::ClosePane(p)));
            v.push(sep());
        }
        if let Some(s) = sid {
            v.push(mb.it(t("layout.swap"), "", false, TA::Swap(s)));
            v.push(mb.it(t("layout.rotate"), "", false, TA::Rotate(Some(s))));
            v.push(mb.it(t("layout.half"), "", false, TA::Half(s)));
        }
        v.push(off(mb.it(t("layout.rotate_all"), "", false, TA::Rotate(None)), !split));
        v.push(off(mb.it(t("layout.equalize"), "", false, TA::Equalize), !split));
        v.push(sub(t("layout.presets"), presets));
        v.push(sep());
        let k = self.cur;
        v.push(off(mb.it(t("layout.sync"), "", o.sync, TA::Flag(k, |o| &mut o.sync)), !split));
        v.push(off(mb.it(t("layout.navsync"), "", o.navsync, TA::Flag(k, |o| &mut o.navsync)), !split));
        v.push(off(mb.it(t("layout.link"), "", o.link, TA::Flag(k, |o| &mut o.link)), !split));
        v.push(off(mb.it(t("layout.compare"), "", o.compare, TA::Flag(k, |o| &mut o.compare)), self.visible().len() != 2));
        v.push(sep());
        v.push(off(mb.it(t("layout.copy_other"), &self.binding("copy_other"), false, TA::CopyOther(true)), !split));
        v.push(off(mb.it(t("layout.move_other"), &self.binding("move_other"), false, TA::CopyOther(false)), !split));
        v
    }
    fn appearance_menu(&self, mb: &mut Mb) -> Vec<MItem> {
        let p = &self.tp;
        macro_rules! choice {
            ($f:ident, [$($v:expr => $k:expr),*]) => {
                vec![$(mb.it(t($k), "", p.$f == $v, TA::Pu(|p| &mut p.$f, $v))),*]
            };
        }
        macro_rules! check {
            ($f:ident, $k:expr) => {
                mb.it(t($k), "", p.$f, TA::Pb(|p| &mut p.$f))
            };
        }
        let fused = [(1u8, "ap.f_layout"), (2, "ap.f_names"), (4, "ap.f_count"), (8, "ap.f_line")].iter().map(|&(b, k)| mb.it(t(k), "", p.fused & b != 0, TA::Fused(b))).collect();
        let rules: Vec<MItem> = p.rules.iter().enumerate().map(|(i, (d, c))| {
            let mut it = mb.it(&format!("{d}  ✕"), "", false, TA::RuleDel(i));
            it.icon = Some(swatch(*c));
            it
        }).collect();
        let mut anim = mb.it(t("ap.animations"), "", !p.still, TA::Pb(|p| &mut p.still));
        anim.on = !p.still;
        vec![
            sub(t("ap.position"), choice!(pos, [0 => "ap.top", 1 => "ap.bottom", 2 => "ap.left"])),
            sub(t("ap.width"), choice!(width, [0 => "ap.compact", 1 => "ap.normal", 2 => "ap.wide"])),
            sub(t("ap.fit"), choice!(fit, [0 => "ap.shrink", 1 => "ap.scroll", 2 => "ap.rows"])),
            sub(t("ap.close"), choice!(close, [0 => "ap.hover", 1 => "ap.always", 2 => "tabs.never"])),
            sub(t("ap.font"), choice!(font, [0 => "ap.small", 1 => "ap.normal", 2 => "ap.large"])),
            sub(t("ap.color_style"), choice!(color_style, [0 => "ap.stripe", 1 => "ap.tint", 2 => "ap.dot"])),
            sub(t("ap.fused"), fused),
            sep(),
            check!(icons, "ap.icons"),
            check!(path, "ap.path"),
            check!(count, "ap.count"),
            check!(busy, "ap.busy"),
            check!(dots, "ap.dots"),
            check!(captions, "ap.captions"),
            check!(cards, "ap.cards"),
            check!(drive_color, "ap.drive"),
            sub(t("ap.rules"), rules),
            sep(),
            anim,
            check!(sounds, "ap.sounds"),
        ]
    }
    /// Everything about tabs: the menu bar's Tabs, and a right-click on the strip's empty part.
    pub(super) fn tabs_menu(&self, mb: &mut Mb) -> Vec<MItem> {
        let p = &self.tp;
        let ws = load_workspaces();
        let mut wsi = vec![mb.it(&format!("{}…", t("ws.save")), "", false, TA::WsSave)];
        if !ws.is_empty() {
            wsi.push(sep());
        }
        for (i, (name, s)) in ws.iter().enumerate() {
            let items = vec![
                mb.it(t("ws.open"), "", false, TA::WsOpen(i, false)),
                mb.it(t("ws.add"), "", false, TA::WsOpen(i, true)),
                mb.it(&format!("{}…", t("ws.export")), "", false, TA::WsExport(i)),
                mb.it(t("ws.delete"), "", false, TA::WsDelete(i)),
            ];
            wsi.push(sub(&format!("{name} ({})", s.tabs.len()), items));
        }
        wsi.push(sep());
        wsi.push(mb.it(&format!("{}…", t("ws.import")), "", false, TA::WsImport));
        let others = windows(self.main);
        let mut wins: Vec<MItem> = others.iter().map(|(h, title)| mb.it(title, "", false, TA::Window(*h as isize))).collect();
        if !wins.is_empty() {
            wins.push(sep());
        }
        wins.push(off(mb.it(t("tabs.merge"), "", false, TA::Merge), others.is_empty()));
        let startup = vec![
            mb.it(t("st.restore"), "", p.start == 0, TA::Pu(|p| &mut p.start, 0)),
            mb.it(t("side.desktop"), "", p.start == 1, TA::Pu(|p| &mut p.start, 1)),
            mb.it(t("side.home"), "", p.start == 2, TA::Pu(|p| &mut p.start, 2)),
            mb.it(t("side.all"), "", p.start == 3, TA::Pu(|p| &mut p.start, 3)),
            sep(),
            mb.it(t("st.use"), "", p.start == 4 && !p.startup.is_empty(), TA::StartupSet),
            off(mb.it(t("st.clear"), "", false, TA::StartupClear), p.startup.is_empty()),
        ];
        let sleep = [0usize, 3, 5, 10, 20].iter().map(|&n| mb.it(&if n == 0 { t("tabs.never").to_string() } else { t("tabs.keep").replace("{n}", &n.to_string()) }, "", p.sleep == n, TA::SleepMax(n))).collect();
        let mut keys: Vec<MItem> = KEYS.iter().map(|&(id, _, label)| mb.it(&format!("{}…", t(label)), &self.binding(id), false, TA::Rebind(id))).collect();
        keys.push(sep());
        keys.push(mb.it(t("tabs.keys_reset"), "", false, TA::KeysReset));
        let groups: Vec<MItem> = self.groups.iter().map(|g| {
            let mut i = sub(if g.name.is_empty() { "•" } else { &g.name }, self.group_menu(g.id, mb));
            i.icon = Some(swatch(g.color));
            i
        }).collect();
        vec![
            mb.it(t("tab.new"), &self.binding("new_tab"), false, TA::NewRight(self.cur)),
            off(mb.it(t("tab.reopen"), &self.binding("reopen"), false, TA::Reopen(self.closed.len().wrapping_sub(1))), self.closed.is_empty()),
            sub(t("tab.recent"), self.closed_items(mb)),
            mb.it(&format!("{}…", t("tabs.search")), &self.binding("search"), false, TA::Search),
            sep(),
            sub(t("tabs.this"), self.tab_menu(self.cur, mb)),
            sub(t("tabs.layout"), self.layout_menu(None, Some(self.id), mb)),
            sub(t("tabs.groups"), groups),
            sep(),
            sub(t("tabs.workspaces"), wsi),
            sub(t("tabs.windows"), wins),
            sub(t("tabs.startup"), startup),
            sep(),
            sub(t("tabs.appearance"), self.appearance_menu(mb)),
            mb.it(t("tabs.mru"), "", p.mru, TA::Pb(|p| &mut p.mru)),
            sub(t("tabs.sleep"), sleep),
            sub(t("tabs.keys"), keys),
        ]
    }
}

/// `items` (built with `mb`) at screen (x, y), and what's picked done.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn run_menu(main: HWND, mb: &mut Mb, items: Vec<MItem>, x: i32, y: i32, above: i32, min_w: i32, find: bool) {
    if let Some(tag) = cascade(main, x, y, above, min_w, true, items, find) {
        if let Some(a) = mb.acts.get(tag).cloned() {
            tab_act(a);
        }
    }
}

/// A right-click on the main window at (x, y) (client): a tab's menu, a group's, a gap's, a caption's, the strip's.
/// False if it was none of those.
pub(super) unsafe fn main_menu(main: HWND, x: i32, y: i32) -> bool {
    let Some(Some((mb, items))) = with(|u| {
        let hit = u.hits.iter().find(|(r, _)| inside(r, x, y)).map(|h| h.1);
        let mut mb = Mb::default();
        let items = match hit {
            Some(Hit::Tab(k) | Hit::TabClose(k)) => u.tab_menu(k, &mut mb),
            Some(Hit::Group(g)) => u.group_menu(g, &mut mb),
            Some(Hit::Gutter(s)) => u.layout_menu(Some(s), None, &mut mb),
            Some(Hit::Pane(p) | Hit::PaneClose(p)) => {
                u.activate(p);
                u.layout_menu(None, Some(p), &mut mb)
            }
            Some(Hit::TreePane(k, p)) => {
                u.switch_tab(k);
                u.activate(p);
                u.layout_menu(None, Some(p), &mut mb)
            }
            Some(Hit::NewTab | Hit::TabList | Hit::Scroll(_)) => u.tabs_menu(&mut mb),
            None if u.on_strip(x, y) => u.tabs_menu(&mut mb),
            _ => return None,
        };
        Some((mb, items))
    }) else {
        return false;
    };
    let mut mb = mb;
    let mut p = POINT { x, y };
    ClientToScreen(main, &mut p);
    with(|u| u.card_close());
    run_menu(main, &mut mb, items, p.x, p.y, p.y, 0, false);
    true
}

/// The menu bar's Tabs menu, under its button `r` (client).
pub(super) unsafe fn tabs_bar_menu(main: HWND, r: RECT, gap: i32) {
    let Some((mut mb, items)) = with(|u| {
        let mut mb = Mb::default();
        let items = u.tabs_menu(&mut mb);
        (mb, items)
    }) else { return };
    let mut p = POINT { x: r.left, y: r.bottom + gap };
    let mut top = POINT { x: r.left, y: r.top - gap };
    ClientToScreen(main, &mut p);
    ClientToScreen(main, &mut top);
    run_menu(main, &mut mb, items, p.x, p.y, top.y, r.right - r.left, false);
}

/// ▾: every tab, to pick one; typing narrows them.
pub(super) unsafe fn tab_list_menu(main: HWND, r: RECT) {
    let Some((mut mb, items)) = with(|u| {
        let mut mb = Mb::default();
        let items = u.tab_list(&mut mb);
        (mb, items)
    }) else { return };
    let mut p = POINT { x: r.right, y: r.bottom };
    ClientToScreen(main, &mut p);
    let w = with(|u| u.px(320)).unwrap_or(320);
    run_menu(main, &mut mb, items, p.x - w, p.y, p.y, w, true);
}

// -- every poll, and panes that follow each other
impl Ui {
    /// The tabs' share of `poll` (twice a second): live tabs refreshed, dots on tabs whose folder changed, the
    /// compared folders, the session kept.
    pub(super) fn tab_poll(&mut self, count: usize, changes: u64) {
        self.polls = self.polls.wrapping_add(1);
        for k in 0..self.tabs.len() {
            if k != self.cur && self.tabs[k].o.live && self.tabs[k].o.asleep.is_none() {
                for p in self.tabs[k].root.leaves() {
                    if self.view(p).is_some() {
                        self.with_pane(p, |u| u.poll_view(count, changes));
                    }
                }
            }
        }
        if self.polls % 4 == 0 && self.tp.dots {
            self.dots();
        }
        if self.polls % 4 == 1 {
            self.compare();
        }
        if self.polls % 10 == 0 {
            self.autosave(false);
        }
        self.title();
    }
    /// A dot on each tab not shown whose folder changed since it was last shown.
    fn dots(&mut self) {
        let mut any = false;
        for k in 0..self.tabs.len() {
            if k == self.cur || self.tabs[k].o.dot {
                continue;
            }
            let Some(d) = listed_dir(&self.focus_q(k)) else { continue };
            let m = mtime_secs(&d);
            let t = &mut self.tabs[k];
            if t.o.seen == 0 {
                t.o.seen = now_secs();
            } else if m > t.o.seen {
                t.o.dot = true;
                any = true;
            }
        }
        if any {
            self.invalidate();
        }
    }
    /// Compare mode: what's only in each of the two panes' folders, and what differs (size or time).
    fn compare(&mut self) {
        let v = self.visible();
        let on = self.tabs[self.cur].o.compare && v.len() == 2;
        let dirs: Option<Vec<String>> = if on { v.iter().map(|&p| self.pane_dir(p)).collect() } else { None };
        let Some(dirs) = dirs else {
            if !self.cmp.is_empty() {
                self.cmp.clear();
                self.invalidate_all();
            }
            return;
        };
        // ponytail: a folder's listing read every 2 s while comparing; a file changed in place shows once its size or time does
        let list = |d: &str| -> HashMap<String, (bool, u64, u64)> {
            std::fs::read_dir(d).into_iter().flatten().flatten().filter_map(|e| {
                let m = e.metadata().ok()?;
                let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                Some((e.file_name().to_string_lossy().to_lowercase(), (m.is_dir(), if m.is_dir() { 0 } else { m.len() }, if m.is_dir() { 0 } else { t })))
            }).collect()
        };
        let (a, b) = (list(&dirs[0]), list(&dirs[1]));
        let mut cmp = HashMap::new();
        for (mine, other, d) in [(&a, &b, &dirs[0]), (&b, &a, &dirs[1])] {
            for (name, info) in mine {
                let path = format!("{}\\{name}", d.trim_end_matches('\\')).to_lowercase();
                match other.get(name) {
                    None => {
                        cmp.insert(path, 1u8);
                    }
                    Some(o) if o != info => {
                        cmp.insert(path, 2u8);
                    }
                    _ => {}
                }
            }
        }
        if cmp != self.cmp {
            self.cmp = cmp;
            self.invalidate_all();
        }
    }
    /// The focused pane scrolled: the others in its tab along with it (synced scrolling).
    pub(super) fn sync_scroll(&mut self) {
        if self.guest || !self.tabs[self.cur].o.sync {
            return;
        }
        let (y, target, me) = (self.y, self.target, self.id);
        for p in self.visible().into_iter().filter(|&p| p != me) {
            self.with_pane(p, |u| {
                (u.y, u.target, u.glide.0) = (y, target, y);
                u.clamp();
                unsafe { InvalidateRect(u.list, null(), 0) };
            });
        }
    }
    /// Linked panes: a single folder selected in the focused one is listed in the next.
    pub(super) fn linked(&mut self) {
        if self.guest || !self.tabs[self.cur].o.link || self.sel.count() != 1 {
            return;
        }
        let Some(r) = self.sel.iter().next().and_then(|i| self.get(i)).filter(|r| r.is_dir) else { return };
        let d = r.path.clone();
        let v = self.visible();
        let Some(me) = v.iter().position(|&p| p == self.id) else { return };
        if v.len() > 1 {
            self.take_to(v[(me + 1) % v.len()], &d);
        }
    }
    /// The focused pane went from folder `old` to `new`: the others in its tab go the same way (into the same
    /// subfolder, or up), where they have it.
    pub(super) fn navsync(&mut self, old: Option<String>, new: Option<String>) {
        if self.guest || !self.tabs[self.cur].o.navsync {
            return;
        }
        let (Some(old), Some(new)) = (old, new) else { return };
        let (ol, nl) = (old.trim_end_matches('\\').to_lowercase(), new.trim_end_matches('\\').to_lowercase());
        let me = self.id;
        for p in self.visible().into_iter().filter(|&p| p != me) {
            let Some(d) = self.pane_dir(p) else { continue };
            let d = d.trim_end_matches('\\').to_string();
            let to = if let Some(rest) = nl.strip_prefix(&format!("{ol}\\")) {
                let tail = &new.trim_end_matches('\\')[new.trim_end_matches('\\').len() - rest.len()..];
                Some(format!("{d}\\{tail}"))
            } else if ol.starts_with(&format!("{nl}\\")) {
                let up = ol[nl.len() + 1..].split('\\').count();
                let mut x = std::path::PathBuf::from(&d);
                (0..up).all(|_| x.pop()).then(|| x.to_string_lossy().into_owned())
            } else {
                None
            };
            if let Some(to) = to.filter(|t| is_dir(t)) {
                self.take_to(p, &to);
            }
        }
    }
    /// What a search from pane `p` looks in when its tab is kept to one folder.
    pub(super) fn scoped(&self, q: &str) -> String {
        let Some(s) = self.tab_of(self.id).and_then(|k| self.tabs[k].o.scope.clone()) else { return q.into() };
        if q.contains("child:") || q.contains("descendant:") || list_of(q).is_some() {
            return q.into();
        }
        format!("descendant:\"{}\" {q}", s.trim_end_matches('\\'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tab_names_share_the_room() {
        assert_eq!(fair(&[30, 200], 150), vec![30, 120]); // the short one whole, the long one cut
        assert_eq!(fair(&[30, 40], 150), vec![30, 40]);
        assert_eq!(fair(&[200, 200], 100), vec![50, 50]);
        assert_eq!(spread(vec![30, 40], 151), vec![71, 80]); // short names share what's to spare
        assert_eq!(spread(vec![30, 120], 150), vec![30, 120]);
        let p = |n: usize| (0..n).map(|i| (i, format!("d{i}"))).collect::<Vec<_>>();
        assert_eq!(seg_names(&p(4)), ["d0", "d1", "d2", "d3"]);
        assert_eq!(seg_names(&p(6)), ["d0", "d1", "d2", "+3"]);
    }

    #[test]
    fn colours_from_drive_and_rules() {
        assert_eq!(drive_color(r"C:\Users"), Some(0x4C8DF6));
        assert_eq!(drive_color(r"d:\"), Some(0x3CB371));
        assert_eq!(drive_color(r"\\server\share"), None);
        assert_eq!(drive_color(""), None);
        let rules = vec![("Projects".to_string(), 1), (r"C:\Work".to_string(), 2), (r"C:\Work\Old\".to_string(), 3)];
        assert_eq!(rule_color(&rules, r"D:\Projects\app"), Some(1));
        assert_eq!(rule_color(&rules, r"C:\work"), Some(2));
        assert_eq!(rule_color(&rules, r"C:\Work\old\x"), Some(3)); // the longest wins
        assert_eq!(rule_color(&rules, r"C:\Workshop"), None); // a folder, not a prefix of a name
        assert_eq!(rule_color(&rules, r"C:\MyProjects"), None);
    }

    #[test]
    fn shortcut_names() {
        assert_eq!(combo_of(true, true, false, b'T' as u16), "Ctrl+Shift+T");
        assert_eq!(combo_of(true, false, false, VK_TAB), "Ctrl+Tab");
        assert_eq!(combo_of(false, true, false, 0x74), "Shift+F5");
        assert_eq!(combo_of(true, false, false, 0xDC), "Ctrl+\\");
        assert_eq!(combo_of(true, true, false, VK_RETURN), "Ctrl+Shift+Enter");
    }

    #[test]
    fn default_keys_unique_and_labelled() {
        let en = include_str!("lang/en.txt");
        for (i, k) in KEYS.iter().enumerate() {
            assert!(KEYS[..i].iter().all(|o| o.0 != k.0 && o.1 != k.1), "{} clashes", k.0);
            assert!(en.lines().any(|l| l.starts_with(&format!("{}=", k.2))), "no label {}", k.2);
        }
        for (_, key) in PALETTE {
            assert!(en.lines().any(|l| l.starts_with(&format!("{key}="))), "no label {key}");
        }
    }

    #[test]
    fn session_round_trip() {
        let mut t = TabSpec::folder(r"C:\a");
        t.root = NodeSpec::Split { vert: true, ratio: 0.3, a: Box::new(t.root), b: Box::new(TabSpec::folder(r"D:\b").root) };
        t.o.title = Some("x".into());
        t.o.zoom = Some(1); // not saved
        let s = Session { tabs: vec![t, TabSpec::folder("home:")], cur: 1, groups: vec![Group { id: 7, name: "g".into(), color: 1, folded: true }] };
        let back: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.cur, 1);
        assert!(back.tabs[0].root == s.tabs[0].root);
        assert_eq!(back.tabs[0].root.leaves().iter().map(|p| p.q.as_str()).collect::<Vec<_>>(), [r"C:\a", r"D:\b"]);
        assert_eq!(back.tabs[0].o.title.as_deref(), Some("x"));
        assert_eq!(back.tabs[0].o.zoom, None);
        assert!(back.groups[0].folded);
        // older or partial files still load
        let old: Session = serde_json::from_str(r#"{"tabs":[{}]}"#).unwrap();
        assert_eq!(old.tabs.len(), 1);
        let p: TabPrefs = serde_json::from_str("{}").unwrap();
        assert!(p.start == 3 && p.icons);
    }
}
