//! The places panel at the left of the list (Alt+B): Explorer's usual shortcuts — Home, Gallery, Recent, the user's
//! known folders, This PC, Network, Linux, the drives with how full they are — with Explorer's icons, scrollable.
//! Icons come from the shell helper process (like the list's own icons); a click is handed to `super::go_place`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ptr::{null, null_mut};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Profile, FOLDERID_Videos, KF_FLAG_DEFAULT};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{Bitmap, Canvas, Gfx};
use super::{mix, png_pixels, put, shell, t, theme, wz, xy};

const WM_ICON: u32 = WM_APP + 10;
/// Width (CSS px) of the panel.
pub const WIDTH: i32 = 200;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Place,
    Drive, // at the bottom, with how full it is
    Sep,
}

struct Item {
    kind: Kind,
    label: String,
    target: String,
    mosaic: bool, // a real folder: can be listed in Mosaic (else always Explorer's)
    icon: String, // what to take the icon from, when not `target`
    space: Option<(u64, u64)>, // a drive's free and total bytes
}

struct Side {
    hwnd: HWND,
    gfx: Gfx,
    cv: Canvas,
    scale: f64,
    items: Vec<Item>,
    png: HashMap<String, Vec<u8>>,
    bmp: HashMap<String, (Bitmap, u32)>, // decoded for canvas `gen`
    asked: HashSet<String>,
    y: f32,
    hot: Option<usize>,
    press: Option<usize>,
    tracking: bool,
    grab: Option<i32>, // dragging the scrollbar thumb: where it was grabbed, from its top
    cur: String,       // the place on screen, as an item's `target` (lowercased): marked
}

thread_local!(static SIDE: RefCell<Option<Side>> = const { RefCell::new(None) });

fn with<R>(f: impl FnOnce(&mut Side) -> R) -> Option<R> {
    SIDE.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn special(label: &str, guid: &str) -> Item {
    Item { space: None, kind: Kind::Place, label: label.into(), target: format!("shell:::{{{guid}}}"), mosaic: false, icon: String::new() }
}

/// The Desktop folder (wherever it was moved to, OneDrive included).
pub fn desktop() -> Option<String> {
    known(&FOLDERID_Desktop, "").map(|i| i.target)
}

/// Folders with a unique Windows icon of their own (lowercased paths), wherever they are listed.
pub fn special_dirs() -> std::collections::HashSet<String> {
    [&FOLDERID_Desktop, &FOLDERID_Downloads, &FOLDERID_Documents, &FOLDERID_Pictures, &FOLDERID_Music, &FOLDERID_Videos].into_iter().filter_map(|id| known(id, "")).map(|i| i.target.to_lowercase()).collect()
}

/// The Windows folder whose icon a folder of only pictures (1), videos (2), music (3) or documents (4) wears.
pub fn kind_dir(kind: u8) -> Option<String> {
    let id = match kind {
        1 => &FOLDERID_Pictures,
        2 => &FOLDERID_Videos,
        3 => &FOLDERID_Music,
        _ => &FOLDERID_Documents,
    };
    known(id, "").map(|i| i.target)
}

fn known(id: &windows::core::GUID, label: &str) -> Option<Item> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()).ok()?;
        let path = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        Some(Item { space: None, kind: Kind::Place, label: label.into(), target: path?, mosaic: true, icon: String::new() })
    }
}

/// Windows' Recent folder: its clock icon is the Recent lists'.
fn recent_dir() -> String {
    std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join(r"Microsoft\Windows\Recent").to_string_lossy().into_owned()
}

/// Fixed and removable drives, as "Label (C:)"; the label is the volume's, else "Local Disk".
fn drives() -> Vec<Item> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let root = format!("{}:\\", (b'A' + i as u8) as char);
            let w = wz(&root);
            if !matches!(unsafe { GetDriveTypeW(w.as_ptr()) }, 2 | 3) {
                return None; // removable, fixed: the others (network, optical) can block on a dead share or empty tray
            }
            let mut name = [0u16; 261];
            let ok = unsafe { GetVolumeInformationW(w.as_ptr(), name.as_mut_ptr(), 261, null_mut(), null_mut(), null_mut(), null_mut(), 0) };
            let n = name.iter().position(|&c| c == 0).unwrap_or(0);
            let vol = if ok != 0 && n > 0 { String::from_utf16_lossy(&name[..n]) } else { t("side.local_disk").to_string() };
            Some(Item { kind: Kind::Drive, label: format!("{vol} ({}:)", &root[..1]), space: space(&root), target: root, mosaic: true, icon: String::new() })
        })
        .collect()
}

/// A drive's free and total bytes.
fn space(root: &str) -> Option<(u64, u64)> {
    let (mut free, mut total) = (0u64, 0u64);
    (unsafe { GetDiskFreeSpaceExW(wz(root).as_ptr(), &mut free, &mut total, null_mut()) } != 0 && total > 0).then_some((free, total))
}

/// A usage bar's colour for a drive `used` full (0..1): green, through yellow and orange, to red when nearly full.
fn usage_color(used: f32) -> u32 {
    const STOPS: [(f32, u32); 4] = [(0.0, 0x2EA043), (0.65, 0xD4A72C), (0.82, 0xE8803A), (0.95, 0xE5534B)];
    let k = STOPS.iter().rposition(|s| used >= s.0).unwrap_or(0);
    match STOPS.get(k + 1) {
        Some(&(b, cb)) => mix(STOPS[k].1, cb, (used - STOPS[k].0) / (b - STOPS[k].0)),
        None => STOPS[k].1,
    }
}

fn build() -> Vec<Item> {
    let sep = || Item { space: None, kind: Kind::Sep, label: String::new(), target: String::new(), mosaic: false, icon: String::new() };
    // Home and Gallery are virtual shell folders; browse their real folders (profile, Pictures) but keep Explorer's icons
    let place = |label: &str, guid: &str, id: &windows::core::GUID| {
        let mut it = known(id, label).unwrap_or_else(|| special(label, guid));
        it.icon = format!("shell:::{{{guid}}}");
        it
    };
    let all = Item { space: None, kind: Kind::Place, label: t("side.all").into(), target: "all:".into(), mosaic: true, icon: "shell:::{031E4825-7B94-4dc3-B131-E946B44C8DD5}".into() };
    let mut v = vec![
        all,
        place(t("side.home"), "F874310E-B6B7-47DC-BC84-B9E6B38F5903", &FOLDERID_Profile),
        place(t("side.gallery"), "E88865EA-0E1C-4E20-9AA6-EDCD0212C87C", &FOLDERID_Pictures),
        Item { space: None, kind: Kind::Place, label: t("side.recent_files").into(), target: "recent:".into(), mosaic: true, icon: recent_dir() },
        Item { space: None, kind: Kind::Place, label: t("side.recent_folders").into(), target: "recent-dirs:".into(), mosaic: true, icon: recent_dir() }, // the shell has no icon for it
        sep(),
    ];
    v[1].target = "home:".into(); // Explorer's Home is the Quick access list, not the profile folder
    v.extend(
        [
            known(&FOLDERID_Desktop, t("side.desktop")),
            known(&FOLDERID_Downloads, t("side.downloads")),
            known(&FOLDERID_Documents, t("side.documents")),
            known(&FOLDERID_Pictures, t("side.pictures")),
            known(&FOLDERID_Music, t("side.music")),
            known(&FOLDERID_Videos, t("side.videos")),
        ]
        .into_iter()
        .flatten(),
    );
    v.push(sep());
    // listed in Mosaic too, by asking the file system (`crate::direct`); Explorer's icons
    let list = |label: &str, q: &str, guid: &str| Item { target: q.into(), mosaic: true, icon: format!("shell:::{{{guid}}}"), ..special(label, guid) };
    v.push(list(t("side.this_pc"), "pc:", "20D04FE0-3AEA-1069-A2D8-08002B30309D"));
    v.push(list(t("side.network"), "net:", "F02C1A0D-BE21-4350-88B0-7367FC96EF3C"));
    if std::path::Path::new(r"C:\Windows\System32\wsl.exe").exists() {
        v.push(list(t("side.linux"), "linux:", "B2B4A4D1-2754-4140-A2EB-9A76D9D7CDC6"));
    }
    // terminals: open in the selected / current folder (`go_place` resolves it)
    v.push(sep());
    let ps = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    let cmd = r"C:\Windows\System32\cmd.exe";
    for (label, spec, icon) in [("PowerShell", "ps", ps), ("PowerShell (Admin)", "ps:admin", ps), ("Command Prompt", "cmd", cmd), ("Command Prompt (Admin)", "cmd:admin", cmd)] {
        v.push(Item { space: None, kind: Kind::Place, label: label.into(), target: format!("term:{spec}"), mosaic: false, icon: icon.into() });
    }
    // the drives, last: held to the panel's bottom when there's room (see `rows`)
    let d = drives();
    if !d.is_empty() {
        v.push(sep());
        v.extend(d);
    }
    v
}

/// Make the (hidden) panel as a child of `parent`. Returns its window.
pub unsafe fn create(parent: HWND, scale: f64) -> HWND {
    static CLASS: std::sync::Once = std::sync::Once::new();
    let class = wz("MosaicSide");
    let inst = GetModuleHandleW(null());
    CLASS.call_once(|| {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = inst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.lpfnWndProc = Some(side_proc);
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
    });
    let hwnd = CreateWindowExW(0, class.as_ptr(), null(), WS_CHILD, 0, 0, 10, 10, parent, null_mut(), inst, null());
    if let Ok(gfx) = Gfx::new(scale, crate::i18n::rtl()) {
        let mut s = Side {
            hwnd,
            cv: Canvas::new(hwnd, &gfx),
            gfx,
            scale,
            items: build(),
            png: HashMap::new(),
            bmp: HashMap::new(),
            asked: HashSet::new(),
            y: 0.0,
            hot: None,
            press: None,
            tracking: false,
            grab: None,
            cur: String::new(),
        };
        s.ask_icons();
        SIDE.with(|c| *c.borrow_mut() = Some(s));
    }
    hwnd
}

/// Redraw now: a theme change doesn't reach this window through the main window's repaint.
pub fn invalidate() {
    with(|s| unsafe { InvalidateRect(s.hwnd, null(), 0) });
}

/// The focused pane shows search `q`: mark its place (a folder listed, All, Home), or none.
pub fn current(q: &str) {
    let q = q.trim();
    let cur = match q {
        "" => "all:".to_string(),
        _ if super::list_of(q).is_some() => q.to_string(),
        _ => super::listed_dir(q).map_or(String::new(), |d| d.to_lowercase()),
    };
    with(|s| {
        if s.cur != cur {
            s.cur = cur;
            unsafe { InvalidateRect(s.hwnd, null(), 0) };
        }
    });
}

/// The labels follow the UI language: rebuild the list (and keep the icons already fetched).
pub fn relabel() {
    with(|s| {
        s.items = build();
        s.hot = None;
        s.ask_icons();
        unsafe { InvalidateRect(s.hwnd, null(), 0) };
    });
}

pub fn rescale(scale: f64) {
    with(|s| {
        if let Ok(g) = Gfx::new(scale, crate::i18n::rtl()) {
            s.scale = scale;
            s.cv = Canvas::new(s.hwnd, &g);
            s.gfx = g;
            s.bmp.clear();
            s.png.clear();
            s.asked.clear(); // icons were fetched for the old size
            s.ask_icons();
        }
    });
}

impl Side {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }

    /// Ask the shell helper for every item's icon (once each).
    fn ask_icons(&mut self) {
        let (size, hw) = (self.px(48), self.hwnd as isize); // 3x, box-filtered down to 16: sharper than the shell's own 16 px scaling
        for it in self.items.iter().filter(|i| i.kind != Kind::Sep) {
            if self.png.contains_key(&it.target) || !self.asked.insert(it.target.clone()) {
                continue;
            }
            let (target, src) = (it.target.clone(), if it.icon.is_empty() { it.target.clone() } else { it.icon.clone() });
            shell::shell_spawn(Box::new(move |dropped| {
                let png = if dropped { None } else { shell::thumbnail(&src, size, true) };
                let b: Box<(String, Option<Vec<u8>>)> = Box::new((target, png));
                unsafe { PostMessageW(hw as HWND, WM_ICON, 0, Box::into_raw(b) as isize) };
            }));
        }
    }

    fn got(&mut self, (target, png): (String, Option<Vec<u8>>)) {
        if let Some(p) = png {
            self.png.insert(target.clone(), p);
            self.bmp.remove(&target);
        }
        self.asked.remove(&target);
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn row_h(&self) -> i32 {
        self.gfx.f.lh + self.px(10)
    }
    /// The small text's line height (9 px, CSS line-height 1.4).
    fn small_lh(&self) -> i32 {
        (9.0 * 1.4 * self.scale).round() as i32
    }

    /// The drives' free space now (it's read when the panel shows or the pointer comes over it, not on a timer).
    fn refresh_space(&mut self) {
        let mut changed = false;
        for it in self.items.iter_mut().filter(|i| i.kind == Kind::Drive) {
            let sp = space(&it.target);
            changed |= sp != it.space;
            it.space = sp;
        }
        if changed {
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
        }
    }

    /// (item, top, bottom) of what is shown, in content coordinates, and the content's height. The drives sit at
    /// the bottom of the panel while everything fits, at the end of the list once it scrolls.
    fn rows(&self) -> (Vec<(usize, i32, i32)>, i32) {
        let (rh, mut y) = (self.row_h(), self.px(6));
        let mut v = Vec::new();
        for (i, it) in self.items.iter().enumerate() {
            let h = match it.kind {
                Kind::Sep => self.px(9),
                Kind::Drive => rh + self.px(9) + self.small_lh(), // and its bar, and its space
                Kind::Place => rh,
            };
            v.push((i, y, y + h));
            y += h;
        }
        let total = y + self.px(6);
        let room = self.view_h() - total;
        if let Some(k) = self.items.iter().position(|i| i.kind == Kind::Drive).filter(|_| room > 0) {
            for r in v.iter_mut().filter(|r| r.0 + 1 >= k) {
                r.1 += room; // from the line above them
                r.2 += room;
            }
            return (v, total + room);
        }
        (v, total)
    }

    fn view_h(&self) -> i32 {
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut c) };
        c.bottom
    }

    fn max_scroll(&self) -> f32 {
        (self.rows().1 - self.view_h()).max(0) as f32
    }

    fn thumb(&self) -> Option<RECT> {
        let (h, total) = (self.view_h() as f32, self.rows().1 as f32);
        if total <= h {
            return None;
        }
        let len = (h * h / total).max(self.px(24) as f32);
        let top = (h - len) * self.y / self.max_scroll().max(1.0);
        let right = self.px(WIDTH) - self.px(3);
        let w = self.px(if self.grab.is_some() { 7 } else { 4 });
        Some(RECT { left: right - w, top: top as i32, right, bottom: (top + len) as i32 })
    }

    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        let _ = x;
        let y = y + self.y.round() as i32;
        self.rows().0.into_iter().find(|&(i, a, b)| self.items[i].kind != Kind::Sep && y >= a && y < b).map(|r| r.0)
    }

    fn down(&mut self, x: i32, y: i32) {
        if let Some(t) = self.thumb() {
            if x >= self.px(WIDTH) - self.px(16) {
                if y >= t.top && y < t.bottom {
                    self.grab = Some(y - t.top);
                    unsafe { SetCapture(self.hwnd) };
                } else {
                    let page = self.view_h() as f32 - self.row_h() as f32;
                    self.y = (self.y + if y < t.top { -page } else { page }).clamp(0.0, self.max_scroll());
                }
                unsafe { InvalidateRect(self.hwnd, null(), 0) };
                return;
            }
        }
        self.press = self.item_at(x, y);
    }

    fn moved(&mut self, x: i32, y: i32) {
        if let Some(grab) = self.grab {
            let (h, total) = (self.view_h() as f32, self.rows().1 as f32);
            let len = (h * h / total).max(self.px(24) as f32);
            let t = ((y - grab) as f32 / (h - len).max(1.0)).clamp(0.0, 1.0);
            self.y = t * self.max_scroll();
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
            return;
        }
        let hot = self.item_at(x, y);
        if hot != self.hot {
            self.hot = hot;
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
        }
        if !self.tracking {
            self.tracking = true;
            self.refresh_space(); // the pointer came over the panel
            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            unsafe { TrackMouseEvent(&mut t) };
        }
    }

    /// The release of a press: what to open, if the pointer is still on the item pressed.
    fn up(&mut self, x: i32, y: i32) -> Option<(String, bool)> {
        if self.grab.take().is_some() {
            unsafe {
                ReleaseCapture();
                InvalidateRect(self.hwnd, null(), 0);
            }
            return None;
        }
        let i = self.press.take().filter(|&p| self.item_at(x, y) == Some(p))?;
        let it = &self.items[i];
        Some((it.target.clone(), it.mosaic))
    }

    fn wheel(&mut self, delta: i32) {
        self.y = (self.y - delta as f32 / 120.0 * 1.5 * self.row_h() as f32).clamp(0.0, self.max_scroll());
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn paint(&mut self) {
        let (rows, _) = self.rows();
        // decode the icons on screen for this canvas (after a GPU reset, again)
        let (gen, isz) = (self.cv.gen, self.px(16));
        for &(i, a, b) in &rows {
            let tg = &self.items[i].target;
            if self.items[i].kind == Kind::Sep || (b as f32) < self.y || a as f32 > self.y + self.view_h() as f32 || self.bmp.get(tg).is_some_and(|x| x.1 == gen) {
                continue;
            }
            if let Some((px, w, h)) = self.png.get(tg).and_then(|p| png_pixels(p, isz)) {
                if let Some(bm) = self.cv.make_bitmap(&px, w, h) {
                    self.bmp.insert(tg.clone(), (bm, gen));
                }
            }
        }
        if !self.cv.begin() {
            return;
        }
        let (g, cv, s) = (&self.gfx, &self.cv, self.scale);
        let px = |v: i32| (v as f64 * s).round() as i32;
        let lh = g.f.lh;
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut c) };
        // flush against the window's left edge: a panel, a hairline along its right
        cv.clear(theme().card);
        cv.fill(&RECT { left: c.right - 1, ..c }, theme().line, 1.0);
        let off = self.y.round() as i32;
        cv.clip(&RECT { left: c.left, top: c.top, right: c.right - 1, bottom: c.bottom });
        // the last row going there: Pictures rather than Gallery, which lists the same folder
        let mark = (!self.cur.is_empty()).then(|| self.items.iter().rposition(|it| it.target.to_lowercase() == self.cur)).flatten();
        for &(i, a, b) in &rows {
            let it = &self.items[i];
            let r = RECT { left: px(6), top: a - off, right: c.right - px(6), bottom: b - off };
            if r.bottom < 0 || r.top > c.bottom {
                continue;
            }
            if it.kind == Kind::Sep {
                cv.fill(&RECT { left: r.left + px(4), top: (r.top + r.bottom) / 2, right: r.right - px(4), bottom: (r.top + r.bottom) / 2 + 1 }, theme().line, 1.0);
                continue;
            }
            let on = mark == Some(i);
            if on || self.hot == Some(i) {
                let fill = if on && self.hot != Some(i) { mix(theme().card, theme().hl, 0.85) } else { theme().hl };
                cv.rrect(&r, (6.0 * s) as f32, Some(fill), None);
            }
            if on {
                // a short accent bar at the row's left, as Explorer's navigation pane marks where you are
                let h = px(16).min(r.bottom - r.top - px(8));
                let bar = RECT { left: r.left + px(1), top: (r.top + r.bottom - h) / 2, right: r.left + px(4), bottom: (r.top + r.bottom + h) / 2 };
                cv.rrect(&bar, px(2) as f32, Some(theme().accent), None);
            }
            let drive = it.kind == Kind::Drive;
            let slh = self.small_lh();
            // a drive's name over its bar over its free space, the three centred together
            let ty = r.top + (r.bottom - r.top - lh - if drive { px(9) + slh } else { 0 }) / 2;
            let ix = r.left + px(24);
            if let Some((bm, _)) = self.bmp.get(&it.target) {
                let iy = if drive { ty + (lh - px(16)) / 2 } else { r.top + (r.bottom - r.top - px(16)) / 2 };
                cv.bitmap(bm, &RECT { left: ix, top: iy, right: ix + px(16), bottom: iy + px(16) }, 1.0);
            }
            let (tx, right) = (ix + px(16) + px(8), r.right - px(10));
            if let Some((free, total)) = it.space.filter(|_| drive) {
                let sy = ty + lh + px(9);
                put(g, cv, &t("side.free").replace("{n}", &super::fmt_size(free as i64)), &g.f.small, tx, sy, theme().muted);
                let size = super::fmt_size(total as i64);
                put(g, cv, &size, &g.f.small, right - g.width(&size, &g.f.small), sy, theme().muted);
                let used = 1.0 - free as f32 / total as f32;
                let bar = RECT { left: tx, top: ty + lh + px(4), right, bottom: ty + lh + px(8) };
                let rad = px(2) as f32;
                cv.rrect(&bar, rad, Some(theme().line), None);
                let w = ((bar.right - bar.left) as f32 * used).round() as i32;
                if w > 0 {
                    cv.rrect(&RECT { right: bar.left + w.max(px(4)), ..bar }, rad, Some(usage_color(used)), None);
                }
            }
            cv.clip(&RECT { right: r.right - px(8), ..r });
            put(g, cv, &it.label, &g.f.reg, tx, ty, theme().text);
            cv.unclip();
        }
        cv.unclip();
        if let Some(r) = self.thumb() {
            cv.rrect_a(&r, (r.right - r.left) as f32 / 2.0, Some(theme().muted), None, if self.grab.is_some() { 0.9 } else { 0.45 });
        }
        self.cv.end();
    }
}

unsafe extern "system" fn side_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(h, &mut ps);
            with(|s| s.paint());
            EndPaint(h, &ps);
            return 0;
        }
        WM_SHOWWINDOW if wp != 0 => {
            with(|s| s.refresh_space());
        }
        WM_SIZE => {
            with(|s| s.y = s.y.clamp(0.0, s.max_scroll()));
            InvalidateRect(h, null(), 0);
            return 0;
        }
        WM_ICON => {
            let p = l as *mut (String, Option<Vec<u8>>);
            if with(|s| s.got(*Box::from_raw(p))).is_none() {
                PostMessageW(h, m, wp, l); // busy: try again in a moment
            }
            return 0;
        }
        WM_MOUSEWHEEL => {
            with(|s| s.wheel(((wp >> 16) & 0xffff) as i16 as i32));
            return 0;
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|s| s.down(x, y));
            return 0;
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(l);
            with(|s| s.moved(x, y));
            return 0;
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(l);
            if let Some(Some((target, mosaic))) = with(|s| s.up(x, y)) {
                super::go_place(&target, mosaic); // outside `with`: it may open a window or search
            }
            return 0;
        }
        WM_MBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|s| s.press = s.item_at(x, y));
            return 0;
        }
        WM_MBUTTONUP => {
            let (x, y) = xy(l);
            if let Some(Some((target, mosaic))) = with(|s| s.up(x, y)) {
                super::go_place_tab(&target, mosaic);
            }
            return 0;
        }
        WM_CAPTURECHANGED => {
            with(|s| {
                if s.grab.take().is_some() {
                    InvalidateRect(s.hwnd, null(), 0);
                }
            });
            return 0;
        }
        WM_MOUSELEAVE => {
            with(|s| {
                s.tracking = false;
                if s.hot.take().is_some() {
                    InvalidateRect(s.hwnd, null(), 0);
                }
            });
            return 0;
        }
        WM_SETCURSOR if (l & 0xffff) as u32 == HTCLIENT && with(|s| s.hot.is_some()) == Some(true) => {
            SetCursor(LoadCursorW(null_mut(), IDC_HAND));
            return 1;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

#[cfg(test)]
mod tests {
    #[test]
    fn usage_goes_green_to_red() {
        use super::usage_color as c;
        assert_eq!(c(0.0), 0x2EA043);
        assert_eq!(c(0.65), 0xD4A72C);
        assert_eq!(c(1.0), 0xE5534B);
        let r = |v: u32| v >> 16;
        assert!(r(c(0.3)) > r(c(0.0)) && r(c(0.9)) > r(c(0.7))); // redder as it fills
    }
}
