//! Tools > File windows…: which programs' Save, Open and Select folder windows Mosaic answers. All of them, Custom
//! (each program seen opening a file window, or added by hand, with its own switch: on until switched off), or Off.
//! A change is saved at once: the watcher reads the settings again when they change.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, ScreenToClient, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW};
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{Bitmap, Canvas, Gfx};
use super::save;
use super::themes::ink;
use super::{mix, png_pixels, put, put_mid, shell, t, theme, w, wz, xy};

// the layout, in CSS px from the top
const WIDTH: i32 = 560;
const PAD: i32 = 24;
const SEG_TOP: i32 = 98; // All programs | Custom | Off
const HINT_TOP: i32 = 142;
const ROW: i32 = 46;
const ROWS: i32 = 6; // in view
const LIST_TOP: i32 = 168;
const LIST_H: i32 = ROWS * ROW + 8;
const ADD_TOP: i32 = LIST_TOP + LIST_H + 12;
const FOOT_TOP: i32 = ADD_TOP + 34 + 36;
const HEIGHT: i32 = FOOT_TOP + 34 + PAD;
const WM_ICON: u32 = WM_APP + 21;

#[derive(Clone, Copy, PartialEq)]
enum Hot {
    Mode(Mode),
    Row(usize),
    Remove(usize),
    Add,
    Done,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    All,
    Custom,
    Off,
}

const MODES: [Mode; 3] = [Mode::All, Mode::Custom, Mode::Off];

fn mode() -> Mode {
    if !save::on() {
        Mode::Off
    } else if save::rules().custom {
        Mode::Custom
    } else {
        Mode::All
    }
}

struct Prog {
    exe: String,  // firefox.exe: what the rules go by
    path: String, // for its icon ("" when not known)
    name: String, // Firefox
}

struct Dlg {
    hwnd: HWND,
    owner: HWND,
    gfx: Gfx,
    cv: Canvas,
    scale: f64,
    progs: Vec<Prog>,
    png: HashMap<String, Vec<u8>>,
    bmp: HashMap<String, (Bitmap, u32)>, // decoded for canvas `gen`
    asked: HashSet<String>,
    scroll: i32,
    hot: Option<Hot>,
    press: Option<Hot>,
    tracking: bool,
}

thread_local!(static DLG: RefCell<Option<Dlg>> = const { RefCell::new(None) });

fn with<R>(f: impl FnOnce(&mut Dlg) -> R) -> Option<R> {
    DLG.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Every program the watcher saw or the rules name, by name.
fn list() -> Vec<Prog> {
    let mut paths: Vec<(String, String)> = Vec::new();
    for p in save::programs() {
        let exe = save::exe_name(&p);
        if !paths.iter().any(|x| x.0 == exe) {
            paths.push((exe, p));
        }
    }
    for exe in save::rules().apps.into_keys() {
        if !paths.iter().any(|x| x.0 == exe) {
            let p = save::app_path(&exe).unwrap_or_default();
            paths.push((exe, p));
        }
    }
    let mut v: Vec<Prog> = paths.into_iter().map(|(exe, path)| Prog { name: save::program_name(if path.is_empty() { &exe } else { &path }), exe, path }).collect();
    v.sort_by_key(|p| p.name.to_lowercase());
    v
}

/// Where everything is (device px).
struct Geo {
    seg: [RECT; 3], // as MODES
    list: RECT,
    add: RECT,
    done: RECT,
}

impl Dlg {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }

    fn geo(&self) -> Geo {
        let p = |v: i32| self.px(v);
        let (l, r) = (p(PAD), p(WIDTH - PAD));
        let g = &self.gfx;
        // the three share their edges
        let edge = |i: i32| l + (r - l) * i / 3;
        let seg = |i: i32| RECT { left: edge(i) - (i > 0) as i32, top: p(SEG_TOP), right: edge(i + 1), bottom: p(SEG_TOP + 34) };
        let add_w = g.width(t("apps.add"), &g.f.reg) + p(32);
        let done_w = (g.width(t("apps.done"), &g.f.semi) + p(32)).max(p(104));
        Geo {
            seg: [seg(0), seg(1), seg(2)],
            list: RECT { left: l, top: p(LIST_TOP), right: r, bottom: p(LIST_TOP + LIST_H) },
            add: RECT { left: l, top: p(ADD_TOP), right: l + add_w, bottom: p(ADD_TOP + 34) },
            done: RECT { left: r - done_w, top: p(FOOT_TOP), right: r, bottom: p(FOOT_TOP + 34) },
        }
    }

    fn row(&self, g: &Geo, i: usize) -> RECT {
        let top = g.list.top + self.px(4) + i as i32 * self.px(ROW) - self.scroll;
        RECT { left: g.list.left + self.px(4), top, right: g.list.right - self.px(4), bottom: top + self.px(ROW) }
    }

    /// A switch at the right of `r`.
    fn switch_in(&self, r: &RECT, inset: i32) -> RECT {
        let (w, h) = (self.px(40), self.px(20));
        let right = r.right - self.px(inset);
        RECT { left: right - w, top: (r.top + r.bottom - h) / 2, right, bottom: (r.top + r.bottom + h) / 2 }
    }

    /// A row's ×, left of its switch (shown while the pointer is on the row).
    fn remove_in(&self, r: &RECT) -> RECT {
        let s = self.switch_in(r, 12);
        let k = self.px(26);
        RECT { left: s.left - self.px(12) - k, top: (r.top + r.bottom - k) / 2, right: s.left - self.px(12), bottom: (r.top + r.bottom + k) / 2 }
    }

    fn max_scroll(&self) -> i32 {
        (self.progs.len() as i32 * self.px(ROW) + self.px(8) - self.px(LIST_H)).max(0)
    }

    fn hit(&self, x: i32, y: i32) -> Option<Hot> {
        let inside = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        let g = self.geo();
        if inside(&g.done) {
            return Some(Hot::Done);
        }
        if let Some(i) = g.seg.iter().position(|r| inside(r)) {
            return Some(Hot::Mode(MODES[i]));
        }
        if mode() != Mode::Custom {
            return None; // the list waits, dimmed
        }
        if inside(&g.add) {
            return Some(Hot::Add);
        }
        if !inside(&g.list) {
            return None;
        }
        (0..self.progs.len()).find(|&i| inside(&self.row(&g, i))).map(|i| if inside(&self.remove_in(&self.row(&g, i))) { Hot::Remove(i) } else { Hot::Row(i) })
    }

    /// The shell helper draws each program's icon, once.
    fn ask_icons(&mut self) {
        let (size, hw) = (self.px(48), self.hwnd as isize); // twice the size shown, box-filtered down: sharper
        for p in self.progs.iter().filter(|p| !p.path.is_empty()) {
            if !self.asked.insert(p.exe.clone()) {
                continue;
            }
            let (exe, path) = (p.exe.clone(), p.path.clone());
            shell::shell_spawn(Box::new(move |dropped| {
                let png = if dropped { None } else { shell::thumbnail(&path, size, true) };
                let b: Box<(String, Option<Vec<u8>>)> = Box::new((exe, png));
                unsafe { PostMessageW(hw as HWND, WM_ICON, 0, Box::into_raw(b) as isize) };
            }));
        }
    }

    fn scroll_by(&mut self, d: i32) {
        self.scroll = (self.scroll + d).clamp(0, self.max_scroll());
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// A click on `h` (all but Add and Done, which open or close a window).
    fn act(&mut self, h: Hot) {
        match h {
            Hot::Mode(m) => {
                if save::on() != (m != Mode::Off) {
                    save::set_on(m != Mode::Off);
                }
                let mut r = save::rules();
                if m != Mode::Off {
                    r.custom = m == Mode::Custom;
                }
                save::set_rules(r); // saves the on/off too
            }
            Hot::Row(i) => {
                let mut r = save::rules();
                let exe = self.progs[i].exe.clone();
                let on = r.wants(&exe);
                r.apps.insert(exe, !on);
                save::set_rules(r);
            }
            Hot::Remove(i) => {
                // off the list: it comes back by itself the next time it opens a file window
                let exe = self.progs.remove(i).exe;
                let mut r = save::rules();
                r.apps.remove(&exe);
                save::set_rules(r);
                save::remove_program(&exe);
                self.hot = None;
                self.scroll_by(0);
            }
            Hot::Add | Hot::Done => {}
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// A program picked in Windows' own Open window: listed, and on.
    fn added(&mut self, path: &str) {
        let exe = save::exe_name(path);
        save::add_program(path);
        let mut r = save::rules();
        r.apps.insert(exe.clone(), true);
        save::set_rules(r);
        self.progs = list();
        if let Some(i) = self.progs.iter().position(|p| p.exe == exe) {
            // scrolled into view
            let (top, bottom) = (i as i32 * self.px(ROW), (i as i32 + 1) * self.px(ROW) + self.px(8));
            if top < self.scroll {
                self.scroll = top;
            } else if bottom > self.scroll + self.px(LIST_H) {
                self.scroll = bottom - self.px(LIST_H);
            }
        }
        self.ask_icons();
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn switch(&self, r: &RECT, on: bool, hot: bool) {
        let (cv, th) = (&self.cv, theme());
        let h = (r.bottom - r.top) as f32;
        if on {
            cv.rrect(r, h / 2.0, Some(if hot { mix(th.accent, th.text, 0.15) } else { th.accent }), None);
        } else {
            cv.rrect(r, h / 2.0, None, Some(if hot { th.text } else { th.muted }));
        }
        let cy = (r.top + r.bottom) as f32 / 2.0;
        let cx = if on { r.right as f32 - h / 2.0 } else { r.left as f32 + h / 2.0 };
        cv.circle(cx, cy, h / 2.0 - (4.0 * self.scale) as f32, if on { ink(th.accent) } else if hot { th.text } else { th.muted }, 1.0);
    }

    fn paint(&mut self) {
        // the icons in this canvas's own bitmaps (after a GPU reset, again)
        let (gen, isz) = (self.cv.gen, self.px(24));
        for p in &self.progs {
            if self.bmp.get(&p.exe).is_some_and(|b| b.1 == gen) {
                continue;
            }
            if let Some((px, w, h)) = self.png.get(&p.exe).and_then(|b| png_pixels(b, isz)) {
                if let Some(bm) = self.cv.make_bitmap(&px, w, h) {
                    self.bmp.insert(p.exe.clone(), (bm, gen));
                }
            }
        }
        if !self.cv.begin() {
            return;
        }
        let (g, cv, s, lh) = (&self.gfx, &self.cv, self.scale, self.gfx.f.lh);
        let px = |v: i32| (v as f64 * s).round() as i32;
        let rad = |v: f64| (v * s) as f32;
        let th = theme();
        let geo = self.geo();
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut c) };
        let hot = |h: Hot| self.hot == Some(h);
        let mid_y = |r: &RECT| r.top + (r.bottom - r.top - lh) / 2;
        let (l, r) = (px(PAD), px(WIDTH - PAD));
        let (on, mode, rules) = (save::on(), mode(), save::rules());
        cv.clear(th.card);
        // title, and what it is about
        if let Some(tl) = g.layout(&w(t("apps.title")), &g.f.big, (r - l) as f32, g.f.big_lh as f32) {
            cv.text(&tl, l, px(22), th.text);
        }
        if let Some(tl) = g.layout(&w(t("apps.intro")), &g.f.reg, (r - l) as f32, (2 * lh) as f32) {
            cv.text(&tl, l, px(56), th.muted);
        }
        // All programs | Custom | Off: the chosen one last, so its accent wins the shared edges
        for pass in [false, true] {
            for (i, (rc, m)) in geo.seg.iter().zip(MODES).enumerate() {
                let sel = mode == m;
                if sel != pass {
                    continue;
                }
                let (a, b) = (if i == 0 { rad(6.0) } else { 0.0 }, if i == 2 { rad(6.0) } else { 0.0 });
                cv.corners(rc, [a, b, b, a], if sel { th.hl } else { th.card }, if sel { th.accent } else { th.line });
                let label = t(match m { Mode::All => "apps.all", Mode::Custom => "tab.custom", Mode::Off => "apps.off" });
                put_mid(g, cv, label, if sel { &g.f.semi } else { &g.f.reg }, rc, mid_y(rc), if sel { th.accent } else if hot(Hot::Mode(m)) { th.text } else { th.muted });
            }
        }
        let hint = match mode { Mode::All => "apps.all_hint", Mode::Custom => "apps.custom_hint", Mode::Off => "apps.off_hint" };
        put(g, cv, t(hint), &g.f.reg, l, px(HINT_TOP), th.muted);
        // the programs
        cv.rrect(&geo.list, rad(8.0), Some(th.bg), Some(th.line));
        cv.clip(&RECT { left: geo.list.left + 1, top: geo.list.top + 1, right: geo.list.right - 1, bottom: geo.list.bottom - 1 });
        if self.progs.is_empty() {
            put_mid(g, cv, t("apps.empty"), &g.f.reg, &geo.list, mid_y(&geo.list), th.muted);
        }
        let small_lh = (9.0 * 1.4 * s).round() as i32;
        for (i, p) in self.progs.iter().enumerate() {
            let rw = self.row(&geo, i);
            if rw.bottom < geo.list.top || rw.top > geo.list.bottom {
                continue;
            }
            let over = hot(Hot::Row(i)) || hot(Hot::Remove(i));
            if over {
                cv.rrect(&rw, rad(6.0), Some(th.hl), None);
            }
            // its icon (a letter on a tile until the shell has drawn it, or when it can't)
            let ic = RECT { left: rw.left + px(10), top: (rw.top + rw.bottom - px(24)) / 2, right: rw.left + px(34), bottom: (rw.top + rw.bottom + px(24)) / 2 };
            match self.bmp.get(&p.exe) {
                Some((bm, _)) => cv.bitmap(bm, &ic, 1.0),
                None => {
                    cv.rrect(&ic, rad(5.0), Some(th.line), None);
                    let letter: String = p.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                    put_mid(g, cv, &letter, &g.f.semi, &ic, mid_y(&ic), th.text);
                }
            }
            // its name over where it lives
            let sw = self.switch_in(&rw, 12);
            let tx = ic.right + px(12);
            let room = (self.remove_in(&rw).left - px(8) - tx) as f32;
            let ty = rw.top + (rw.bottom - rw.top - lh - small_lh) / 2;
            if let Some(tl) = g.layout(&w(&p.name), &g.f.reg, room, lh as f32) {
                cv.text(&tl, tx, ty, th.text);
            }
            if let Some(tl) = g.layout(&w(if p.path.is_empty() { &p.exe } else { &p.path }), &g.f.small, room, small_lh as f32) {
                cv.text(&tl, tx, ty + lh, th.muted);
            }
            if over {
                let x = self.remove_in(&rw);
                let (cx, cy) = ((x.left + x.right) as f32 / 2.0, (x.top + x.bottom) as f32 / 2.0);
                if hot(Hot::Remove(i)) {
                    cv.circle(cx, cy, (x.right - x.left) as f32 / 2.0, mix(th.hl, th.text, 0.12), 1.0);
                }
                cv.cross(cx, cy, rad(4.0), if hot(Hot::Remove(i)) { th.text } else { th.muted }, rad(1.4));
            }
            self.switch(&sw, on && rules.wants(&p.exe), over && !hot(Hot::Remove(i)));
        }
        cv.unclip();
        // a scroll mark, when there are more than fit
        let max = self.max_scroll();
        if max > 0 {
            let track = (geo.list.bottom - geo.list.top - px(12)) as f32;
            let len = (track * px(LIST_H) as f32 / (px(LIST_H) + max) as f32).max(px(24) as f32);
            let y = geo.list.top as f32 + px(6) as f32 + (track - len) * self.scroll as f32 / max as f32;
            let x = geo.list.right - px(6);
            cv.rrect(&RECT { left: x - px(3), top: y as i32, right: x, bottom: (y + len) as i32 }, rad(1.5), Some(th.muted), None);
        }
        // Add program…
        cv.rrect(&geo.add, rad(6.0), Some(if hot(Hot::Add) { th.hl } else { th.card }), Some(th.line));
        put_mid(g, cv, t("apps.add"), &g.f.reg, &geo.add, mid_y(&geo.add), th.text);
        // switched one by one only in Custom: otherwise shown as they are, dimmed
        if mode != Mode::Custom {
            cv.fill(&RECT { left: 0, top: geo.list.top - px(4), right: c.right, bottom: geo.add.bottom + px(4) }, th.card, 0.6);
        }
        // footer: a rule, the Shift tip, Done
        let rule = geo.done.top - px(18);
        cv.fill(&RECT { left: 0, top: rule, right: c.right, bottom: rule + 1 }, th.line, 1.0);
        if let Some(tl) = g.layout(&w(t("apps.shift")), &g.f.reg, (geo.done.left - px(16) - l) as f32, (2 * lh) as f32) {
            let m = super::gfx::metrics(&tl);
            cv.text(&tl, l, (geo.done.top + geo.done.bottom - m.height.round() as i32) / 2, th.muted);
        }
        cv.rrect(&geo.done, rad(6.0), Some(th.accent), None);
        if hot(Hot::Done) {
            cv.rrect_a(&geo.done, rad(6.0), Some(ink(th.accent)), None, 0.12);
        }
        put_mid(g, cv, t("apps.done"), &g.f.semi, &geo.done, mid_y(&geo.done), ink(th.accent));
        self.cv.end();
    }
}

/// The dialog, over `owner` (which waits, disabled, until it closes).
pub fn open(owner: HWND) {
    if with(|p| unsafe { SetForegroundWindow(p.hwnd) }).is_some() {
        return;
    }
    static CLASS: std::sync::Once = std::sync::Once::new();
    let class = wz("MosaicFileWindows");
    unsafe {
        CLASS.call_once(|| {
            let mut wc: WNDCLASSEXW = std::mem::zeroed();
            wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            wc.style = CS_DROPSHADOW;
            wc.hInstance = GetModuleHandleW(null());
            wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
            wc.lpfnWndProc = Some(proc_);
            wc.lpszClassName = class.as_ptr();
            RegisterClassExW(&wc);
        });
        let scale = GetDpiForWindow(owner) as f64 / 96.0;
        let Ok(gfx) = Gfx::new(scale, crate::i18n::rtl()) else { return };
        let (w, h) = ((WIDTH as f64 * scale).round() as i32, (HEIGHT as f64 * scale).round() as i32);
        let mut o: RECT = std::mem::zeroed();
        GetWindowRect(owner, &mut o);
        let (x, y) = (o.left + (o.right - o.left - w) / 2, o.top + ((o.bottom - o.top - h) / 2).max(0));
        let hwnd = CreateWindowExW(0, class.as_ptr(), wz(t("apps.title")).as_ptr(), WS_POPUP, x, y, w, h, owner, null_mut(), GetModuleHandleW(null()), null());
        if hwnd.is_null() {
            return;
        }
        let round: u32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33, &round as *const _ as *const std::ffi::c_void, 4);
        let dark: BOOL = theme().dark as i32;
        DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const std::ffi::c_void, 4);
        let mut d = Dlg { hwnd, owner, cv: Canvas::new(hwnd, &gfx), gfx, scale, progs: list(), png: HashMap::new(), bmp: HashMap::new(), asked: HashSet::new(), scroll: 0, hot: None, press: None, tracking: false };
        d.ask_icons();
        DLG.with(|c| *c.borrow_mut() = Some(d));
        EnableWindow(owner, 0);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(hwnd);
    }
}

fn close() {
    let Some(p) = DLG.with(|c| c.borrow_mut().take()) else { return };
    unsafe {
        EnableWindow(p.owner, 1);
        SetForegroundWindow(p.owner);
        DestroyWindow(p.hwnd);
        if std::env::args().any(|a| a == "--file-windows") {
            PostMessageW(p.owner, WM_CLOSE, 0, 0);
        }
    }
}

pub fn hwnd() -> Option<HWND> {
    with(|p| p.hwnd)
}

/// Windows' own Open window, for a program file. Mosaic's own file windows are never answered by Mosaic.
unsafe fn ask_program(owner: HWND) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let filter: Vec<u16> = format!("{} (*.exe)\0*.exe\0\0", t("apps.exe")).encode_utf16().collect();
    let title = wz(t("apps.add").trim_end_matches('…'));
    let dir = wz(&std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into()));
    let mut o: OPENFILENAMEW = std::mem::zeroed();
    o.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    o.hwndOwner = owner;
    o.lpstrFilter = filter.as_ptr();
    o.lpstrFile = buf.as_mut_ptr();
    o.nMaxFile = buf.len() as u32;
    o.lpstrTitle = title.as_ptr();
    o.lpstrInitialDir = dir.as_ptr();
    o.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if GetOpenFileNameW(&mut o) == 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..n])).filter(|p| !p.is_empty())
}

unsafe extern "system" fn proc_(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(h, &mut ps);
            with(|p| p.paint());
            EndPaint(h, &ps);
            return 0;
        }
        WM_ICON => {
            let b = Box::from_raw(l as *mut (String, Option<Vec<u8>>));
            let (exe, png) = *b;
            with(|p| {
                if let Some(png) = png {
                    p.bmp.remove(&exe);
                    p.png.insert(exe, png);
                    InvalidateRect(h, null(), 0);
                }
            });
            return 0;
        }
        WM_NCHITTEST => {
            // dragged by its title
            let mut pt = POINT { x: (l & 0xffff) as i16 as i32, y: ((l >> 16) & 0xffff) as i16 as i32 };
            ScreenToClient(h, &mut pt);
            let title = with(|p| pt.y < p.px(50)).unwrap_or(false);
            return if title { HTCAPTION as LRESULT } else { HTCLIENT as LRESULT };
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(l);
            with(|p| {
                let hot = p.hit(x, y);
                if hot != p.hot {
                    p.hot = hot;
                    InvalidateRect(h, null(), 0);
                }
                if !p.tracking {
                    p.tracking = true;
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                    TrackMouseEvent(&mut t);
                }
            });
            return 0;
        }
        WM_MOUSELEAVE => {
            with(|p| {
                p.tracking = false;
                p.hot = None;
                InvalidateRect(h, null(), 0);
            });
            return 0;
        }
        WM_MOUSEWHEEL => {
            let d = ((wp >> 16) & 0xffff) as i16 as i32;
            with(|p| {
                let step = p.px(ROW);
                p.scroll_by(-d * step / 120);
            });
            return 0;
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|p| p.press = p.hit(x, y));
            return 0;
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(l);
            // Add and Done open or close a window: done outside the dialog's own state
            match with(|p| p.press.take().filter(|&b| p.hit(x, y) == Some(b))).flatten() {
                Some(Hot::Done) => close(),
                Some(Hot::Add) => {
                    if let Some(path) = ask_program(h) {
                        with(|p| p.added(&path));
                    }
                }
                Some(b) => _ = with(|p| p.act(b)),
                None => {}
            }
            return 0;
        }
        WM_KEYDOWN => {
            if matches!(wp as u16, VK_ESCAPE | VK_RETURN) {
                close();
            }
            return 0;
        }
        WM_CLOSE => {
            close();
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}
